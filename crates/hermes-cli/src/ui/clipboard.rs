//! Cross-platform clipboard text reader for interactive `Ctrl+V` / `Shift+Insert` paste.
//!
//! Uses zero extra crate dependencies:
//! - Windows: direct Win32 `OpenClipboard` / `GetClipboardData(CF_UNICODETEXT)` (instant, Unicode-safe)
//! - macOS: `pbpaste`
//! - Linux / Unix: `wl-paste`, `xclip`, `xsel`, WSL `powershell.exe`, or `termux-clipboard-get`

#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::Mutex;

#[cfg(unix)]
const CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(1500);
#[cfg(unix)]
const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardRead {
    Text(String),
    Empty,
    Unavailable(String),
}

#[cfg(test)]
static TEST_OVERRIDE: Mutex<Option<ClipboardRead>> = Mutex::new(None);

#[cfg(test)]
pub(crate) struct TestClipboardGuard {
    prev: Option<ClipboardRead>,
}

#[cfg(test)]
impl Drop for TestClipboardGuard {
    fn drop(&mut self) {
        *TEST_OVERRIDE.lock().unwrap() = self.prev.take();
    }
}

#[cfg(test)]
pub(crate) fn override_for_test(value: ClipboardRead) -> TestClipboardGuard {
    let prev = TEST_OVERRIDE.lock().unwrap().replace(value);
    TestClipboardGuard { prev }
}

/// Read plain UTF-8 text from the operating system clipboard.
pub fn read_text() -> ClipboardRead {
    #[cfg(test)]
    if let Some(mocked) = TEST_OVERRIDE.lock().unwrap().clone() {
        return mocked;
    }

    read_text_platform()
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
