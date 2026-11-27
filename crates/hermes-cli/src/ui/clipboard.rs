//! Cross-platform clipboard text reader and writer for interactive copy/paste.
//!
//! Uses zero extra crate dependencies:
//! - Windows: direct Win32 `OpenClipboard` / `GetClipboardData` / `SetClipboardData(CF_UNICODETEXT)`
//! - macOS: `pbpaste` / `pbcopy`
//! - Linux / Unix: `wl-paste`/`wl-copy`, `xclip`, `xsel`, WSL `powershell.exe`, `termux-clipboard-*`
//! - SSH / remote TTY fallback on write: OSC 52 (`\x1b]52;c;<base64>\x07`)

#[cfg(unix)]
use std::io::{IsTerminal, Read, Write};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(test)]
use std::cell::RefCell;

#[cfg(unix)]
const CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(1500);
#[cfg(unix)]
const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;
#[cfg(unix)]
const MAX_OSC52_BYTES: usize = 100 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardRead {
    Text(String),
    Empty,
    Unavailable(String),
}

#[cfg(test)]
thread_local! {
    static TEST_OVERRIDE: RefCell<Option<ClipboardRead>> = const { RefCell::new(None) };
    static TEST_WRITE_LOG: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestClipboardGuard {
    prev: Option<ClipboardRead>,
}

#[cfg(test)]
impl Drop for TestClipboardGuard {
    fn drop(&mut self) {
        TEST_OVERRIDE.with(|slot| *slot.borrow_mut() = self.prev.take());
    }
}

#[cfg(test)]
pub(crate) fn override_for_test(value: ClipboardRead) -> TestClipboardGuard {
    let prev = TEST_OVERRIDE.with(|slot| slot.borrow_mut().replace(value));
    TestClipboardGuard { prev }
}

#[cfg(test)]
pub(crate) struct TestWriteCaptureGuard {
    prev: Option<Vec<String>>,
}

#[cfg(test)]
impl TestWriteCaptureGuard {
    pub(crate) fn writes(&self) -> Vec<String> {
        TEST_WRITE_LOG.with(|slot| slot.borrow().clone().unwrap_or_default())
    }
}

#[cfg(test)]
impl Drop for TestWriteCaptureGuard {
    fn drop(&mut self) {
        TEST_WRITE_LOG.with(|slot| *slot.borrow_mut() = self.prev.take());
    }
}

#[cfg(test)]
pub(crate) fn capture_writes_for_test() -> TestWriteCaptureGuard {
    let prev = TEST_WRITE_LOG.with(|slot| slot.borrow_mut().replace(Vec::new()));
    TestWriteCaptureGuard { prev }
}

/// Read plain UTF-8 text from the operating system clipboard.
pub fn read_text() -> ClipboardRead {
    #[cfg(test)]
    if let Some(mocked) = TEST_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return mocked;
    }

    read_text_platform()
}

/// Write plain UTF-8 text to the operating system clipboard (with OSC 52 fallback on remote TTYs).
pub fn write_text(text: &str) -> Result<(), String> {
    #[cfg(test)]
    {
        let captured = TEST_WRITE_LOG.with(|slot| {
            if let Some(log) = slot.borrow_mut().as_mut() {
                log.push(text.to_owned());
                true
            } else {
                false
            }
        });
        if captured {
            return Ok(());
        }
    }

    write_text_platform(text)
}

#[cfg(windows)]
fn read_text_platform() -> ClipboardRead {
    use core::ffi::c_void;
    use std::ptr::null_mut;
    use std::time::Duration;

    const CF_UNICODETEXT: u32 = 13;
    const MAX_UTF16_UNITS: usize = 4 * 1024 * 1024;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(hwnd_new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalLock(hmem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(hmem: *mut c_void) -> i32;
    }

    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }

    struct GlobalLockGuard(*mut c_void);
    impl Drop for GlobalLockGuard {
        fn drop(&mut self) {
            unsafe {
                GlobalUnlock(self.0);
            }
        }
    }

    let mut opened = false;
    for _ in 0..5 {
        if unsafe { OpenClipboard(null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !opened {
        return ClipboardRead::Unavailable("Clipboard is busy".into());
    }
    let _guard = ClipboardGuard;

    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
        return ClipboardRead::Empty;
    }

    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if handle.is_null() {
        return ClipboardRead::Empty;
    }

    let ptr = unsafe { GlobalLock(handle) } as *const u16;
    if ptr.is_null() {
        return ClipboardRead::Unavailable("Failed to lock clipboard data".into());
    }
    let _lock_guard = GlobalLockGuard(handle);

    let mut len = 0usize;
    while len < MAX_UTF16_UNITS {
        if unsafe { *ptr.add(len) } == 0 {
            break;
        }
        len += 1;
    }
    if len == 0 {
        return ClipboardRead::Empty;
    }

    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf16_lossy(slice);
    if text.is_empty() {
        ClipboardRead::Empty
    } else {
        ClipboardRead::Text(text)
    }
}

#[cfg(target_os = "macos")]
fn read_text_platform() -> ClipboardRead {
    let mut cmd = Command::new("pbpaste");
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_CTYPE").is_none() {
        cmd.env("LC_CTYPE", "UTF-8");
    }
    match run_clipboard_command(&mut cmd) {
        CmdResult::Output(text) if !text.is_empty() => ClipboardRead::Text(text),
        CmdResult::Output(_) | CmdResult::Failed => ClipboardRead::Empty,
        CmdResult::NotFound => {
            ClipboardRead::Unavailable("Clipboard unavailable (`pbpaste` not found)".into())
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read_text_platform() -> ClipboardRead {
    let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    let has_x11 = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
    let is_wsl =
        std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some();

    let mut candidates: Vec<(&str, &[&str], bool)> = Vec::new();
    if has_wayland {
        candidates.push(("wl-paste", &["--no-newline"], false));
    }
    if has_x11 {
        candidates.push(("xclip", &["-o", "-selection", "clipboard"], false));
        candidates.push(("xsel", &["--clipboard", "--output"], false));
    }
    if !has_wayland && !has_x11 {
        candidates.push(("wl-paste", &["--no-newline"], false));
        candidates.push(("xclip", &["-o", "-selection", "clipboard"], false));
        candidates.push(("xsel", &["--clipboard", "--output"], false));
    }
    if is_wsl {
        candidates.push((
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-Clipboard -Raw",
            ],
            true,
        ));
    }
    candidates.push(("termux-clipboard-get", &[], false));

    let mut saw_tool = false;
    for (program, args, strip_single_trailing_newline) in candidates {
        let mut cmd = Command::new(program);
        cmd.args(args);
        match run_clipboard_command(&mut cmd) {
            CmdResult::Output(mut text) => {
                saw_tool = true;
                if strip_single_trailing_newline {
                    if let Some(stripped) = text.strip_suffix("\r\n") {
                        text = stripped.to_owned();
                    } else if let Some(stripped) = text.strip_suffix('\n') {
                        text = stripped.to_owned();
                    }
                }
                if !text.is_empty() {
                    return ClipboardRead::Text(text);
                }
            }
            CmdResult::Failed => {
                saw_tool = true;
            }
            CmdResult::NotFound => {}
        }
    }

    if saw_tool {
        ClipboardRead::Empty
    } else {
        ClipboardRead::Unavailable(
            "Clipboard unavailable (install wl-paste or xclip, or use terminal paste)".into(),
        )
    }
}

#[cfg(not(any(unix, windows)))]
fn read_text_platform() -> ClipboardRead {
    ClipboardRead::Unavailable("Clipboard paste is not supported on this platform".into())
}

#[cfg(unix)]
enum CmdResult {
    Output(String),
    Failed,
    NotFound,
}

#[cfg(unix)]
fn run_clipboard_command(cmd: &mut Command) -> CmdResult {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return CmdResult::NotFound,
        Err(_) => return CmdResult::Failed,
    };

    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout {
            let _ = out.take(MAX_CLIPBOARD_BYTES as u64).read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    let bytes = reader.join().unwrap_or_default();
    match status {
        Some(st) if st.success() => CmdResult::Output(String::from_utf8_lossy(&bytes).into_owned()),
        _ => CmdResult::Failed,
    }
}

#[cfg(windows)]
fn write_text_platform(text: &str) -> Result<(), String> {
    use core::ffi::c_void;
    use std::ptr::null_mut;
    use std::time::Duration;

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(hwnd_new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, hmem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalFree(hmem: *mut c_void) -> *mut c_void;
        fn GlobalLock(hmem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(hmem: *mut c_void) -> i32;
    }

    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }

    let mut opened = false;
    for _ in 0..5 {
        if unsafe { OpenClipboard(null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !opened {
        return Err("Clipboard is busy".into());
    }
    let _guard = ClipboardGuard;

    if unsafe { EmptyClipboard() } == 0 {
        return Err("Failed to clear clipboard".into());
    }

    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let byte_len = utf16.len() * std::mem::size_of::<u16>();
    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
    if hmem.is_null() {
        return Err("Failed to allocate clipboard memory".into());
    }

    let dst = unsafe { GlobalLock(hmem) } as *mut u16;
    if dst.is_null() {
        unsafe {
            GlobalFree(hmem);
        }
        return Err("Failed to lock clipboard memory".into());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
        GlobalUnlock(hmem);
    }

    if unsafe { SetClipboardData(CF_UNICODETEXT, hmem) }.is_null() {
        unsafe {
            GlobalFree(hmem);
        }
        return Err("Failed to write clipboard data".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn write_text_platform(text: &str) -> Result<(), String> {
    let mut cmd = Command::new("pbcopy");
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_CTYPE").is_none() {
        cmd.env("LC_CTYPE", "UTF-8");
    }
    match run_clipboard_write_command(&mut cmd, text) {
        CmdResult::Output(_) => Ok(()),
        CmdResult::Failed | CmdResult::NotFound => {
            write_osc52_fallback(text, "Clipboard unavailable (`pbcopy` failed)".into())
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn write_text_platform(text: &str) -> Result<(), String> {
    let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    let has_x11 = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
    let is_wsl =
        std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some();

    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if has_wayland {
        candidates.push(("wl-copy", &[]));
    }
    if has_x11 {
        candidates.push(("xclip", &["-i", "-selection", "clipboard"]));
        candidates.push(("xsel", &["--clipboard", "--input"]));
    }
    if !has_wayland && !has_x11 {
        candidates.push(("wl-copy", &[]));
        candidates.push(("xclip", &["-i", "-selection", "clipboard"]));
        candidates.push(("xsel", &["--clipboard", "--input"]));
    }
    if is_wsl {
        candidates.push((
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; [System.Console]::In.ReadToEnd() | Set-Clipboard",
            ],
        ));
    }
    candidates.push(("termux-clipboard-set", &[]));

    for (program, args) in candidates {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if matches!(
            run_clipboard_write_command(&mut cmd, text),
            CmdResult::Output(_)
        ) {
            return Ok(());
        }
    }

    write_osc52_fallback(
        text,
        "Clipboard unavailable (install wl-copy or xclip)".into(),
    )
}

#[cfg(not(any(unix, windows)))]
fn write_text_platform(_text: &str) -> Result<(), String> {
    Err("Clipboard copy is not supported on this platform".into())
}

#[cfg(unix)]
fn run_clipboard_write_command(cmd: &mut Command, text: &str) -> CmdResult {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return CmdResult::NotFound,
        Err(_) => return CmdResult::Failed,
    };

    let bytes = text.as_bytes().to_vec();
    let writer = child.stdin.take().map(|mut stdin| {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
            let _ = stdin.flush();
        })
    });

    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    if let Some(handle) = writer {
        let _ = handle.join();
    }

    match status {
        Some(st) if st.success() => CmdResult::Output(String::new()),
        _ => CmdResult::Failed,
    }
}

#[cfg(unix)]
fn write_osc52_fallback(text: &str, fallback_err: String) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() || text.len() > MAX_OSC52_BYTES {
        return Err(fallback_err);
    }
    let encoded = encode_base64(text.as_bytes());
    write!(stdout, "\x1b]52;c;{encoded}\x07")
        .and_then(|_| stdout.flush())
        .map_err(|_| fallback_err)
}

#[cfg(any(unix, test))]
fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_ascii_and_utf8() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64("Привет".as_bytes()), "0J/RgNC40LLQtdGC");
    }
}
