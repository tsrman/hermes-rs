//! Clipboard image & file paste helpers for `hsr-gui`, including native Windows `CF_DIB` /
//! `CF_DIBV5` → PNG conversion (used by Snipping Tool / `Win+Shift+S` / `PrtScn` / Paint /
//! browsers, which `gpui 0.2.2` does not decode on Windows) and `CF_HDROP` file paste.

use std::io::Cursor;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

use gpui::ImageFormat as GpuiImageFormat;
use image::{DynamicImage, ImageFormat};

/// Convert a Windows `CF_DIB` or `CF_DIBV5` memory block (a BMP without its 14-byte
/// `BITMAPFILEHEADER`) into a standard PNG image suitable for `read_file` and vision providers.
///
/// Handles the common Windows 32-bit GDI quirk where opaque screenshots store `alpha = 0` on
/// every pixel by normalizing an all-zero alpha channel to fully opaque (`255`).
#[cfg(any(target_os = "windows", test))]
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, String> {
    if dib.len() < 40 {
        return Err("Clipboard bitmap header is too small".into());
    }
    let header_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]);
    if header_size < 40 || (header_size as usize) > dib.len() {
        return Err(format!("Unsupported DIB header size ({header_size})"));
    }
    let bit_count = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);
    let clr_used = u32::from_le_bytes([dib[32], dib[33], dib[34], dib[35]]);

    let palette_bytes = if clr_used > 0 {
        clr_used.saturating_mul(4)
    } else if bit_count <= 8 {
        (1u32.checked_shl(u32::from(bit_count)).unwrap_or(0)).saturating_mul(4)
    } else {
        0
    };
    let extra_masks = if header_size == 40 {
        match compression {
            3 => 12u32, // BI_BITFIELDS
            6 => 16u32, // BI_ALPHABITFIELDS
            _ => 0,
        }
    } else {
        0
    };
    let pixel_offset = 14u32
        .checked_add(header_size)
        .and_then(|v| v.checked_add(palette_bytes))
        .and_then(|v| v.checked_add(extra_masks))
        .ok_or_else(|| "DIB offset overflow".to_owned())?;
    let total_size = u32::try_from(14usize.saturating_add(dib.len()))
        .map_err(|_| "Clipboard bitmap is too large".to_owned())?;

    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&total_size.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&pixel_offset.to_le_bytes());
    bmp.extend_from_slice(dib);

    encode_bmp_to_png(&bmp)
}

fn encode_bmp_to_png(bmp_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let decoded = image::load_from_memory_with_format(bmp_bytes, ImageFormat::Bmp)
        .map_err(|e| format!("Failed decoding clipboard bitmap: {e}"))?;
    let mut rgba = decoded.to_rgba8();
    if !rgba.is_empty() && rgba.pixels().all(|p| p.0[3] == 0) {
        for p in rgba.pixels_mut() {
            p.0[3] = 255;
        }
    }
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(rgba)
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| format!("Failed encoding clipboard PNG: {e}"))?;
    Ok(png)
}

/// Normalize a GPUI clipboard image into `(extension, bytes)` supported by `read_file` & vision
/// providers (`png`, `jpg`, `gif`, `webp`). BMP/other raw formats are transcoded to PNG.
pub fn normalize_gpui_image(
    format: GpuiImageFormat,
    bytes: &[u8],
) -> Result<(&'static str, Vec<u8>), String> {
    if bytes.is_empty() {
        return Err("Clipboard image is empty".into());
    }
    match format {
        GpuiImageFormat::Png => Ok(("png", bytes.to_vec())),
        GpuiImageFormat::Jpeg => Ok(("jpg", bytes.to_vec())),
        GpuiImageFormat::Gif => Ok(("gif", bytes.to_vec())),
        GpuiImageFormat::Webp => Ok(("webp", bytes.to_vec())),
        GpuiImageFormat::Bmp => Ok(("png", encode_bmp_to_png(bytes)?)),
        _ => {
            let decoded = image::load_from_memory(bytes)
                .map_err(|e| format!("Unsupported clipboard image format: {e}"))?;
            let mut png = Vec::new();
            decoded
                .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
                .map_err(|e| format!("Failed converting clipboard image to PNG: {e}"))?;
            Ok(("png", png))
        }
    }
}

/// Read copied file paths (`CF_HDROP`) from the Windows clipboard when present (e.g. Explorer
/// `Ctrl+C` on one or more files). Returns `None` if plain text is also present or no files exist.
#[cfg(target_os = "windows")]
pub fn read_windows_clipboard_files() -> Option<Vec<PathBuf>> {
    windows_impl::read_clipboard_files()
}

/// Read an image (`PNG`, `image/png`, `JFIF`, `GIF`, `CF_DIBV5`, or `CF_DIB`) directly from the
/// Windows clipboard. Used as a fallback when GPUI's clipboard reader returns `None` (such as
/// `Win+Shift+S`, `PrtScn`, Paint, Telegram, or apps that only publish `CF_DIB` / `CF_DIBV5`).
#[cfg(target_os = "windows")]
pub fn read_windows_clipboard_image() -> Result<Option<(&'static str, Vec<u8>)>, String> {
    windows_impl::read_clipboard_image()
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use core::ffi::c_void;
    use std::path::PathBuf;
    use std::ptr::null_mut;
    use std::time::Duration;

    use super::dib_to_png;

    const CF_DIB: u32 = 8;
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP: u32 = 15;
    const CF_DIBV5: u32 = 17;
    const MAX_CLIPBOARD_IMAGE_BYTES: usize = 32 * 1024 * 1024;
    const DRAGDROP_GET_FILES_COUNT: u32 = 0xFFFF_FFFF;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(hwnd_new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalLock(hmem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(hmem: *mut c_void) -> i32;
        fn GlobalSize(hmem: *mut c_void) -> usize;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn DragQueryFileW(hdrop: *mut c_void, i_file: u32, lpsz_file: *mut u16, cch: u32) -> u32;
    }

    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }

    fn open_clipboard_retry() -> Option<ClipboardGuard> {
        for _ in 0..5 {
            if unsafe { OpenClipboard(null_mut()) } != 0 {
                return Some(ClipboardGuard);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    fn register_format(name: &str) -> u32 {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
    }

    fn read_format_bytes(format: u32) -> Option<Vec<u8>> {
        if format == 0 || unsafe { IsClipboardFormatAvailable(format) } == 0 {
            return None;
        }
        let handle = unsafe { GetClipboardData(format) };
        if handle.is_null() {
            return None;
        }
        let size = unsafe { GlobalSize(handle) };
        if size == 0 || size > MAX_CLIPBOARD_IMAGE_BYTES {
            return None;
        }
        let ptr = unsafe { GlobalLock(handle) } as *const u8;
        if ptr.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
        unsafe {
            GlobalUnlock(handle);
        }
        Some(bytes)
    }

    pub(super) fn read_clipboard_files() -> Option<Vec<PathBuf>> {
        let _guard = open_clipboard_retry()?;
        // If clipboard also has Unicode text, treat it as text paste rather than file drop.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } != 0 {
            return None;
        }
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
            return None;
        }
        let hdrop = unsafe { GetClipboardData(CF_HDROP) };
        if hdrop.is_null() {
            return None;
        }
        let count = unsafe { DragQueryFileW(hdrop, DRAGDROP_GET_FILES_COUNT, null_mut(), 0) };
        if count == 0 {
            return None;
        }
        let mut paths = Vec::with_capacity(count as usize);
        for idx in 0..count {
            let len = unsafe { DragQueryFileW(hdrop, idx, null_mut(), 0) } as usize;
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len + 1];
            let written = unsafe { DragQueryFileW(hdrop, idx, buf.as_mut_ptr(), buf.len() as u32) };
            if written > 0 {
                let s = String::from_utf16_lossy(&buf[..written as usize]);
                if !s.is_empty() {
                    paths.push(PathBuf::from(s));
                }
            }
        }
        (!paths.is_empty()).then_some(paths)
    }

    pub(super) fn read_clipboard_image() -> Result<Option<(&'static str, Vec<u8>)>, String> {
        let Some(_guard) = open_clipboard_retry() else {
            return Ok(None);
        };

        // 1. Prefer native lossless PNG / encoded formats if the source app published them.
        for (name, ext) in [
            ("PNG", "png"),
            ("image/png", "png"),
            ("JFIF", "jpg"),
            ("GIF", "gif"),
        ] {
            let fmt = register_format(name);
            if let Some(bytes) = read_format_bytes(fmt) {
                return Ok(Some((ext, bytes)));
            }
        }

        // 2. Fall back to CF_DIBV5 / CF_DIB (Windows Snipping Tool, PrtScn, Paint, etc.).
        if let Some(dib) = read_format_bytes(CF_DIBV5).or_else(|| read_format_bytes(CF_DIB)) {
            let png = dib_to_png(&dib)?;
            return Ok(Some(("png", png)));
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_32bpp_dib(width: u32, height: u32, bgra_pixels: &[[u8; 4]]) -> Vec<u8> {
        let mut dib = Vec::with_capacity(40 + bgra_pixels.len() * 4);
        dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
        dib.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
        dib.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight (bottom-up)
        dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
        dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
        dib.extend_from_slice(&((bgra_pixels.len() * 4) as u32).to_le_bytes()); // biSizeImage
        dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
        dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
        dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
        dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
        for px in bgra_pixels {
            dib.extend_from_slice(px);
        }
        dib
    }

    #[test]
    fn converts_windows_dib_with_zero_alpha_to_opaque_png() {
        // 2x1 32-bit DIB with BGR=(0, 128, 255) and Windows GDI zero alpha.
        let dib = make_32bpp_dib(2, 1, &[[0, 128, 255, 0], [255, 0, 0, 0]]);
        let png = dib_to_png(&dib).expect("dib_to_png succeeds");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        let decoded = image::load_from_memory_with_format(&png, ImageFormat::Png)
            .expect("valid png")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 128, 0, 255]);
        assert_eq!(decoded.get_pixel(1, 0).0, [0, 0, 255, 255]);
    }

    fn make_32bpp_dib_v4(width: u32, height: u32, bgra_pixels: &[[u8; 4]]) -> Vec<u8> {
        let mut dib = vec![0u8; 108 + bgra_pixels.len() * 4];
        dib[0..4].copy_from_slice(&108u32.to_le_bytes()); // bV4Size = BITMAPV4HEADER
        dib[4..8].copy_from_slice(&(width as i32).to_le_bytes());
        dib[8..12].copy_from_slice(&(height as i32).to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
        dib[14..16].copy_from_slice(&32u16.to_le_bytes()); // bitCount
        dib[16..20].copy_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
        dib[20..24].copy_from_slice(&((bgra_pixels.len() * 4) as u32).to_le_bytes());
        dib[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes()); // Red mask
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes()); // Green mask
        dib[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes()); // Blue mask
        dib[52..56].copy_from_slice(&0xFF00_0000u32.to_le_bytes()); // Alpha mask
        dib[56..60].copy_from_slice(&0x7352_4742u32.to_le_bytes()); // LCS_sRGB
        let mut off = 108;
        for px in bgra_pixels {
            dib[off..off + 4].copy_from_slice(px);
            off += 4;
        }
        dib
    }

    #[test]
    fn preserves_real_alpha_in_32bpp_dib_and_rejects_truncated_dib() {
        let dib = make_32bpp_dib_v4(2, 1, &[[0, 0, 255, 128], [0, 255, 0, 0]]);
        let png = dib_to_png(&dib).expect("dib_to_png succeeds");
        let decoded = image::load_from_memory_with_format(&png, ImageFormat::Png)
            .expect("valid png")
            .to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 128]);
        assert_eq!(decoded.get_pixel(1, 0).0, [0, 255, 0, 0]);

        assert!(dib_to_png(&[0u8; 10]).is_err());
    }

    #[test]
    fn normalizes_gpui_image_formats_to_supported_extensions() {
        let dib = make_32bpp_dib(1, 1, &[[10, 20, 30, 0]]);
        let png = dib_to_png(&dib).unwrap();
        let (ext, out) = normalize_gpui_image(GpuiImageFormat::Png, &png).unwrap();
        assert_eq!(ext, "png");
        assert_eq!(out, png);
        assert!(normalize_gpui_image(GpuiImageFormat::Png, &[]).is_err());
    }
}
