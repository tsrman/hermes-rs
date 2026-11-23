//! MCP media/document caching (`MEDIA:` tags and embedded binary resources) and oversized
//! MCP tool-result disk spillover (`<persisted-output>`).
//!
//! Port of `tools/mcp_tool_content.py`, `gateway/platforms/base_image_sniff.py`,
//! `tools/audio_container.py`, and `tools/tool_result_storage.py`, verified against
//! `crates/hermes-runtime/tests/fixtures/mcp_remainder_cases.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use regex::Regex;
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Map, Value};

/// Default per-result spillover threshold for MCP tools (`DEFAULT_MCP_RESULT_SIZE_CHARS`).
pub const DEFAULT_MCP_RESULT_SIZE_CHARS: usize = 50_000;
/// Upper cap on `tool_budget.mcp_result_size_chars` (`DEFAULT_RESULT_SIZE_CHARS`).
pub const DEFAULT_RESULT_SIZE_CHARS: usize = 100_000;
/// Default inline preview budget for persisted spillover messages (`DEFAULT_PREVIEW_SIZE_CHARS`).
pub const DEFAULT_PREVIEW_SIZE_CHARS: usize = 1_500;
/// Cap on decoded audio/resource bytes per block (`_MCP_RESOURCE_MAX_BYTES`, 50 MiB).
pub const MCP_RESOURCE_MAX_BYTES: usize = 50 * 1024 * 1024;
/// Pre-decode base64 ceiling (`_MCP_RESOURCE_MAX_B64_CHARS`).
pub const MCP_RESOURCE_MAX_B64_CHARS: usize = MCP_RESOURCE_MAX_BYTES * 4 / 3 + 4;
/// Inbound image cache cap (`DEFAULT_INBOUND_MEDIA_MAX_BYTES`, 128 MiB).
const IMAGE_MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_RESULT_FILENAME_STEM: usize = 120;

const PERSISTED_OUTPUT_TAG: &str = "<persisted-output>";
const PERSISTED_OUTPUT_CLOSING_TAG: &str = "</persisted-output>";
const ENVELOPE_METADATA_TAG: &str = "<mcp-result-metadata>";
const ENVELOPE_METADATA_CLOSING_TAG: &str = "</mcp-result-metadata>";

static RAND_FALLBACK: AtomicU64 = AtomicU64::new(1);

/// Parse `tool_budget.mcp_result_size_chars` (default 50,000, capped at 100,000).
pub fn parse_mcp_result_size_chars(config: &serde_yaml::Value) -> usize {
    let raw = config
        .get("tool_budget")
        .and_then(|b| b.get("mcp_result_size_chars"));
    let parsed = match raw {
        Some(serde_yaml::Value::Number(n)) => n.as_i64(),
        Some(serde_yaml::Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    parsed
        .filter(|&v| v > 0)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(DEFAULT_MCP_RESULT_SIZE_CHARS)
        .min(DEFAULT_RESULT_SIZE_CHARS)
}

/// Python-compatible non-strict base64 decoder (`base64.b64decode(data)`):
/// ignores characters outside the base64 alphabet, rejects `len % 4 == 1`.
pub fn decode_base64_lenient(input: &str) -> Result<Vec<u8>, String> {
    let mut vals: Vec<u8> = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    for b in input.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            _ => continue,
        };
        if pad > 0 {
            // Data after padding is ignored in Python's binascii unless it breaks a quantum;
            // reset padding only if non-pad follows? Python's a2b_base64 stops at first valid pad.
            break;
        }
        vals.push(v);
    }
    if vals.len() % 4 == 1 {
        return Err("Invalid base64-encoded string: length mod 4 == 1".into());
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    let mut i = 0;
    while i + 4 <= vals.len() {
        let n = ((vals[i] as u32) << 18)
            | ((vals[i + 1] as u32) << 12)
            | ((vals[i + 2] as u32) << 6)
            | (vals[i + 3] as u32);
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
        i += 4;
    }
    match vals.len() - i {
        0 => {}
        2 => {
            let n = ((vals[i] as u32) << 18) | ((vals[i + 1] as u32) << 12);
            out.push((n >> 16) as u8);
        }
        3 => {
            let n = ((vals[i] as u32) << 18)
                | ((vals[i + 1] as u32) << 12)
                | ((vals[i + 2] as u32) << 6);
            out.push((n >> 16) as u8);
            out.push((n >> 8) as u8);
        }
        _ => unreachable!(),
    }
    Ok(out)
}

/// Magic-byte sniffing for inbound images (`gateway/platforms/base_image_sniff.py::_looks_like_image`).
pub fn looks_like_image(data: &[u8]) -> bool {
    if data.len() >= 4
        && (data.starts_with(b"\x89PNG\r\n\x1a\n")
            || data.starts_with(b"\xff\xd8\xff")
            || data.starts_with(b"GIF87a")
            || data.starts_with(b"GIF89a")
            || data.starts_with(b"BM")
            || (data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WEBP"))
    {
        return true;
    }
    if data.len() < 16 || &data[4..8] != b"ftyp" {
        return false;
    }
    let box_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if box_size < 16 || data.len() < box_size {
        return false;
    }
    let brand = [
        data[8].to_ascii_lowercase(),
        data[9].to_ascii_lowercase(),
        data[10].to_ascii_lowercase(),
        data[11].to_ascii_lowercase(),
    ];
    matches!(
        &brand,
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"avif" | b"avis"
    )
}

/// Magic-byte audio container sniffer (`tools/audio_container.py::sniff_audio_ext`).
pub fn sniff_audio_ext(data: &[u8], fallback_ext: &str) -> String {
    let fallback = if fallback_ext.starts_with('.') {
        fallback_ext.to_owned()
    } else {
        format!(".{fallback_ext}")
    };
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        return ".m4a".to_owned();
    }
    if data.starts_with(b"OggS") {
        return ".ogg".to_owned();
    }
    if data.starts_with(b"fLaC") {
        return ".flac".to_owned();
    }
    if data.starts_with(b"ID3") {
        return ".mp3".to_owned();
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WAVE" {
        return ".wav".to_owned();
    }
    if data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0 {
        return if (data[1] & 0xF6) == 0xF0 {
            ".aac".to_owned()
        } else {
            ".mp3".to_owned()
        };
    }
    if data.starts_with(b"\x1a\x45\xdf\xa3") {
        return ".webm".to_owned();
    }
    fallback
}

fn base_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn image_ext_for_mime(mime: &str) -> &'static str {
    match base_mime(mime).as_str() {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" | "image/x-ms-bmp" => ".bmp",
        "image/svg+xml" => ".svg",
        "image/tiff" => ".tiff",
        "image/avif" => ".avif",
        "image/heic" => ".heic",
        _ => ".png",
    }
}

fn audio_ext_for_mime(mime: &str) -> &'static str {
    match base_mime(mime).as_str() {
        "audio/wav" | "audio/x-wav" | "audio/wave" => ".wav",
        "audio/mpeg" | "audio/mp3" => ".mp3",
        "audio/ogg" => ".ogg",
        "audio/flac" | "audio/x-flac" => ".flac",
        "audio/aac" => ".aac",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => ".m4a",
        "audio/webm" => ".webm",
        _ => ".ogg",
    }
}

fn doc_ext_for_mime(mime: &str) -> &'static str {
    match base_mime(mime).as_str() {
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        "application/json" => ".json",
        "text/html" => ".html",
        "text/csv" => ".csv",
        "application/xml" | "text/xml" => ".xml",
        "application/zip" => ".zip",
        "application/octet-stream" => ".bin",
        _ => ".bin",
    }
}

/// Safe display filename from an MCP resource URI (`tools/mcp_tool_content.py::_mcp_resource_filename`).
pub fn mcp_resource_filename(uri: &str, mime_type: &str) -> String {
    let mut name = String::new();
    if !uri.is_empty() {
        let path_part = extract_uri_path(uri);
        let decoded = percent_decode(path_part);
        if let Some(last) = decoded.rsplit('/').find(|seg| !seg.is_empty()) {
            name = last.to_owned();
        }
    }
    name = name
        .chars()
        .filter(|&c| !((c as u32) <= 0x1F || (c as u32) == 0x7F))
        .collect::<String>()
        .trim()
        .to_owned();
    if name.chars().count() > 150 {
        if let Some((stem, ext)) = name.rsplit_once('.') {
            let ext_len = ext.chars().count();
            if ext_len > 0 && ext_len <= 12 {
                let keep: String = stem.chars().take(150 - ext_len - 1).collect();
                name = format!("{keep}.{ext}");
            } else {
                name = name.chars().take(150).collect();
            }
        } else {
            name = name.chars().take(150).collect();
        }
    }
    if name.is_empty() || name == "." || name == ".." {
        format!("resource{}", doc_ext_for_mime(mime_type))
    } else {
        name
    }
}

fn extract_uri_path(uri: &str) -> &str {
    let without_frag = uri.split('#').next().unwrap_or(uri);
    let without_query = without_frag.split('?').next().unwrap_or(without_frag);
    if let Some((_, rest)) = without_query.split_once("://") {
        // After `scheme://`, authority extends until the first `/` (if any).
        match rest.find('/') {
            Some(idx) => &rest[idx..],
            None => "",
        }
    } else if let Some((scheme, rest)) = without_query.split_once(':') {
        if !scheme.is_empty()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        {
            rest
        } else {
            without_query
        }
    } else {
        without_query
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn random_hex12() -> String {
    let mut buf = [0u8; 6];
    let rng = SystemRandom::new();
    if rng.fill(&mut buf).is_err() {
        let seq = RAND_FALLBACK.fetch_add(1, Ordering::Relaxed);
        buf.copy_from_slice(&seq.to_be_bytes()[2..8]);
    }
    let mut out = String::with_capacity(12);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn write_private_bytes(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)
    }
}

/// Cache an MCP `image` content block into `<profile>/cache/images/img_<hex12><ext>`
/// and return `MEDIA:<path>` (empty string on non-image/invalid/no-profile).
pub fn cache_mcp_image_block(block: &Value, profile: Option<&Path>) -> String {
    let Some(data) = block["data"].as_str() else {
        return String::new();
    };
    let mime = base_mime(block["mimeType"].as_str().unwrap_or(""));
    if !mime.starts_with("image/") {
        return String::new();
    }
    let Ok(raw) = decode_base64_lenient(data) else {
        return String::new();
    };
    if raw.len() > IMAGE_MAX_BYTES || !looks_like_image(&raw) {
        return String::new();
    }
    let Some(profile) = profile else {
        return String::new();
    };
    let dir = profile.join("cache").join("images");
    if ensure_private_dir(&dir).is_err() {
        return String::new();
    }
    let ext = image_ext_for_mime(&mime);
    let path = dir.join(format!("img_{}{ext}", random_hex12()));
    if write_private_bytes(&path, &raw).is_err() {
        return String::new();
    }
    format!("MEDIA:{}", path.display())
}

/// Cache an MCP `audio` content block into `<profile>/cache/audio/audio_<hex12><ext>`
/// and return `MEDIA:<path>` (or size-cap notice when over 50 MiB, empty on decode/cache failure).
pub fn cache_mcp_audio_block(block: &Value, profile: Option<&Path>) -> String {
    let Some(data) = block["data"].as_str() else {
        return String::new();
    };
    let mime = base_mime(block["mimeType"].as_str().unwrap_or(""));
    if !mime.starts_with("audio/") {
        return String::new();
    }
    if data.len() > MCP_RESOURCE_MAX_B64_CHARS {
        return format!(
            "[MCP audio resource too large to cache: ~{} bytes]",
            data.len() * 3 / 4
        );
    }
    let Ok(raw) = decode_base64_lenient(data) else {
        return String::new();
    };
    if raw.len() > MCP_RESOURCE_MAX_BYTES {
        return format!(
            "[MCP audio resource too large to cache: {} bytes]",
            raw.len()
        );
    }
    let Some(profile) = profile else {
        return String::new();
    };
    let dir = profile.join("cache").join("audio");
    if ensure_private_dir(&dir).is_err() {
        return String::new();
    }
    let ext = sniff_audio_ext(&raw, audio_ext_for_mime(&mime));
    let path = dir.join(format!("audio_{}{ext}", random_hex12()));
    if write_private_bytes(&path, &raw).is_err() {
        return String::new();
    }
    format!("MEDIA:{}", path.display())
}

/// Cache an MCP embedded binary resource (`resource.blob`) into `<profile>/cache/documents/`.
/// When `profile` is `None`, returns the upstream unavailable marker.
pub fn cache_mcp_resource_blob(
    blob: &str,
    uri: &str,
    mime: &str,
    profile: Option<&Path>,
) -> String {
    let label = if mime.is_empty() { uri } else { mime };
    if blob.len() > MCP_RESOURCE_MAX_B64_CHARS {
        return format!(
            "[MCP embedded resource too large to cache: ~{} bytes, uri={uri}]",
            blob.len() * 3 / 4
        );
    }
    let Ok(raw) = decode_base64_lenient(blob) else {
        return format!("[MCP embedded resource could not be decoded: {label}]");
    };
    if raw.len() > MCP_RESOURCE_MAX_BYTES {
        return format!(
            "[MCP embedded resource too large to cache: {} bytes, uri={uri}]",
            raw.len()
        );
    }
    let kind = if mime.is_empty() {
        "unknown type"
    } else {
        mime
    };
    let Some(profile) = profile else {
        return format!(
            "[MCP embedded resource received ({} bytes, {kind}) but document cache unavailable in this process]",
            raw.len()
        );
    };
    let dir = profile.join("cache").join("documents");
    if ensure_private_dir(&dir).is_err() {
        return format!("[MCP embedded resource could not be cached: {label}]");
    }
    let safe_name = sanitize_document_name(&mcp_resource_filename(uri, mime));
    let path = dir.join(format!("doc_{}_{safe_name}", random_hex12()));
    if write_private_bytes(&path, &raw).is_err() {
        return format!("[MCP embedded resource could not be cached: {label}]");
    }
    format!(
        "[MCP resource saved to {} ({kind}, {} bytes) — read it with read_file or terminal tools]",
        path.display(),
        raw.len()
    )
}

fn sanitize_document_name(name: &str) -> String {
    let leaf = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("document")
        .replace('\0', "")
        .trim()
        .to_owned();
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        "document".to_owned()
    } else {
        leaf
    }
}

/// Unwrap an MCP JSON envelope (`{"result", "structuredContent", "_meta"}`) into pageable text
/// for disk spillover (`tools/tool_result_storage.py::_pageable_text`).
pub fn pageable_text(content: &str) -> String {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(content) else {
        return content.to_owned();
    };
    if map.is_empty() {
        return content.to_owned();
    }
    let is_mcp_envelope = map.contains_key("result")
        && map
            .keys()
            .all(|k| matches!(k.as_str(), "result" | "structuredContent" | "_meta"));
    if !is_mcp_envelope {
        return content.to_owned();
    }
    let Some(text) = map
        .get("result")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return content.to_owned();
    };
    let extras: BTreeMap<String, Value> = map
        .iter()
        .filter(|(k, _)| k.as_str() != "result")
        .map(|(k, v)| (k.clone(), sort_json(v)))
        .collect();
    if extras.is_empty() {
        return text.to_owned();
    }
    let metadata = format_py_indented_json(&Value::Object(extras.into_iter().collect()), 1, 0);
    format!("{text}\n\n{ENVELOPE_METADATA_TAG}\n{metadata}\n{ENVELOPE_METADATA_CLOSING_TAG}\n")
}

/// Truncate at the last newline within `max_chars` (when past halfway) (`generate_preview`).
pub fn generate_preview(content: &str, max_chars: usize) -> (String, bool) {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= max_chars {
        return (content.to_owned(), false);
    }
    let head = &chars[..max_chars];
    if let Some(pos) = head.iter().rposition(|&c| c == '\n') {
        if pos > max_chars / 2 {
            return (chars[..=pos].iter().collect(), true);
        }
    }
    (head.iter().collect(), true)
}

/// Single safe filename for a tool-result spillover file (`_safe_result_filename`).
pub fn safe_result_filename(tool_use_id: &str) -> String {
    static UNSAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_.-]+").unwrap());
    let raw_id = if tool_use_id.is_empty() {
        "tool_result"
    } else {
        tool_use_id
    };
    let replaced = UNSAFE.replace_all(raw_id, "_");
    let trimmed = replaced.trim_matches(|c| matches!(c, '.' | '_' | '-'));
    let changed = trimmed != raw_id;
    let stem = if trimmed.is_empty() {
        "tool_result"
    } else {
        trimmed
    };
    let final_stem = if changed || stem.chars().count() > MAX_RESULT_FILENAME_STEM {
        let digest = sha256_hex(raw_id);
        let prefix: String = stem.chars().take(MAX_RESULT_FILENAME_STEM).collect();
        let prefix = prefix.trim_matches(|c| matches!(c, '.' | '_' | '-'));
        let prefix = if prefix.is_empty() {
            "tool_result"
        } else {
            prefix
        };
        format!("{prefix}_{}", &digest[..12])
    } else {
        stem.to_owned()
    };
    format!("{final_stem}.txt")
}

/// Persist an oversized MCP tool result to `<profile>/cache/spillover/<id>.txt` and return the
/// `<persisted-output>` preview message, or inline-truncate if persistence fails.
pub fn maybe_persist_mcp_result(
    content: &str,
    tool_call_id: &str,
    profile: Option<&Path>,
    threshold_chars: usize,
    preview_size: usize,
) -> String {
    if content.chars().count() <= threshold_chars {
        return content.to_owned();
    }
    let persisted_content = pageable_text(content);
    let filename = safe_result_filename(tool_call_id);
    let (preview, has_more) = generate_preview(&persisted_content, preview_size);
    if let Some(path) =
        profile.and_then(|home| write_to_spillover(home, &persisted_content, &filename))
    {
        return build_persisted_message(
            &preview,
            has_more,
            persisted_content.chars().count(),
            &path.display().to_string(),
        );
    }
    inline_truncated(&preview, content.chars().count())
}

fn write_to_spillover(profile: &Path, content: &str, filename: &str) -> Option<PathBuf> {
    let dir = profile.join("cache").join("spillover");
    ensure_private_dir(&dir).ok()?;
    let path = dir.join(filename);
    let data = content.as_bytes();
    write_private_bytes(&path, data).ok()?;
    // Read-back verification: discard archive if truncated/corrupted on disk.
    let verified = std::fs::metadata(&path)
        .ok()
        .is_some_and(|m| m.len() as usize == data.len())
        && std::fs::read(&path).ok().is_some_and(|read| read == data);
    if !verified {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path)
}

pub fn build_persisted_message(
    preview: &str,
    has_more: bool,
    original_size: usize,
    file_path: &str,
) -> String {
    let size_kb = original_size as f64 / 1024.0;
    let size_str = if size_kb >= 1024.0 {
        format!("{:.1} MB", size_kb / 1024.0)
    } else {
        format!("{size_kb:.1} KB")
    };
    let ellipsis = if has_more { "\n..." } else { "" };
    format!(
        "{PERSISTED_OUTPUT_TAG}\n\
This tool result was too large ({} characters, {size_str}).\n\
Full output saved to: {file_path}\n\
Use the read_file tool with offset and limit to access specific sections of this output.\n\
Recovery: page through the saved file with read_file (offset/limit) or \
process it with execute_code — do NOT re-request the same data from the \
remote API; the archive is kept while this session's transcript references \
it. If it is missing, inspect the current state before retrying — the call \
that produced it may already have taken effect.\n\n\
Preview (first {} chars):\n\
{preview}{ellipsis}\n\
{PERSISTED_OUTPUT_CLOSING_TAG}",
        grouped(original_size),
        preview.chars().count(),
    )
}

pub fn inline_truncated(preview: &str, total_chars: usize) -> String {
    format!(
        "{preview}\n\n[Truncated: tool response was {} chars. Full output could not be saved to sandbox.]",
        grouped(total_chars)
    )
}

fn grouped(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn sort_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> =
                map.iter().map(|(k, v)| (k.clone(), sort_json(v))).collect();
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_json).collect()),
        other => other.clone(),
    }
}

/// Format JSON like Python `json.dumps(v, ensure_ascii=False, indent=indent, sort_keys=True)`.
fn format_py_indented_json(value: &Value, indent: usize, level: usize) -> String {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                return "{}".to_owned();
            }
            let pad = " ".repeat(indent * (level + 1));
            let close_pad = " ".repeat(indent * level);
            let mut out = String::from("{\n");
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(a, _)| *a);
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                let key = serde_json::to_string(k).unwrap_or_default();
                let val = format_py_indented_json(v, indent, level + 1);
                out.push_str(&format!("{pad}{key}: {val}"));
            }
            out.push_str(&format!("\n{close_pad}}}"));
            out
        }
        Value::Array(items) => {
            if items.is_empty() {
                return "[]".to_owned();
            }
            let pad = " ".repeat(indent * (level + 1));
            let close_pad = " ".repeat(indent * level);
            let mut out = String::from("[\n");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                let val = format_py_indented_json(item, indent, level + 1);
                out.push_str(&format!("{pad}{val}"));
            }
            out.push_str(&format!("\n{close_pad}]"));
            out
        }
        other => crate::session_search::py_json_dumps(other),
    }
}

fn sha256_hex(text: &str) -> String {
    let hash = digest(&SHA256, text.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hash.as_ref() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}
