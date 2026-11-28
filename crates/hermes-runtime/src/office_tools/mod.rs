//! Pure-Rust Word (`.docx`) and PowerPoint (`.pptx`) helpers exposed to `execute_code`
//! scripts via `from hermes_tools import docx_read, docx_write, pptx_read, pptx_write`.
//!
//! Runs natively inside `hermes-runtime` on every platform (Windows, Linux GNU/musl, macOS) with
//! zero external Python packages (`python-docx`, `python-pptx`, MS Office) required.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::Path;

use serde_json::Value;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::doc_extract::MAX_DOCUMENT_BYTES;
use crate::excel_tools::write_file_atomically;

pub(super) const DEFAULT_MAX_PARAGRAPHS: usize = 5_000;
pub(super) const HARD_MAX_PARAGRAPHS: usize = 50_000;
pub(super) const DEFAULT_MAX_SLIDES: usize = 200;
pub(super) const HARD_MAX_SLIDES: usize = 1_000;

// ── Shared XML, ZIP, and inline Markdown helpers ───────────────────────────────────────────────

/// Escape XML special characters and strip illegal XML 1.0 control characters so Office apps
/// never show a "corrupted content" recovery dialog.
pub(super) fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

pub(super) fn write_zip_entries<S: AsRef<str>>(
    path: &Path,
    entries: &[(S, String)],
) -> Result<(), String> {
    write_file_atomically(path, |file| {
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, content) in entries {
            let entry_name = name.as_ref();
            writer
                .start_file(entry_name, options)
                .map_err(|e| format!("failed writing ZIP entry '{entry_name}': {e}"))?;
            writer
                .write_all(content.as_bytes())
                .map_err(|e| format!("failed writing ZIP payload '{entry_name}': {e}"))?;
        }
        writer
            .finish()
            .map_err(|e| format!("failed finalizing ZIP archive: {e}"))?;
        Ok(())
    })
}

/// Read an Office archive with size verification *before* allocating memory and a bounded `.take()`
/// cap so oversized or growing files cannot exhaust RAM.
pub(super) fn read_bounded_office_file(path: &Path, kind: &str) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|e| format!("failed opening {kind} file: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("failed inspecting {kind} file: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{kind} path is not a regular file"));
    }
    if meta.len() > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "{kind} exceeds maximum size ({} bytes)",
            MAX_DOCUMENT_BYTES
        ));
    }
    let mut data = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("failed reading {kind} file: {e}"))?;
    if data.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "{kind} exceeds maximum size ({} bytes)",
            MAX_DOCUMENT_BYTES
        ));
    }
    Ok(data)
}

pub(super) fn value_to_string(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

pub(super) fn sanitize_hex_color(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('#');
    if trimmed.len() == 6 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(trimmed.to_ascii_uppercase())
    } else {
        None
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct InlineRun {
    pub(super) text: String,
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) code: bool,
    pub(super) underline: bool,
    pub(super) strike: bool,
    pub(super) color: Option<String>,
    pub(super) size_pt: Option<f64>,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_closing_delim(input: &str, start: usize, delim: &str, word_bounded: bool) -> Option<usize> {
    let bytes = input.as_bytes();
    let dlen = delim.len();
    let mut search = start;
    while let Some(rel) = input[search..].find(delim) {
        let abs = search + rel;
        if abs > start && bytes[abs - 1] == b'\\' {
            search = abs + dlen;
            continue;
        }
        if abs == start {
            search = abs + dlen;
            continue;
        }
        let inner = &input[start..abs];
        if inner.starts_with(char::is_whitespace) || inner.ends_with(char::is_whitespace) {
            search = abs + dlen;
            continue;
        }
        if word_bounded && abs + dlen < bytes.len() && is_word_byte(bytes[abs + dlen]) {
            search = abs + dlen;
            continue;
        }
        return Some(abs - start);
    }
    None
}

fn parse_link_at(input: &str, bracket_start: usize) -> Option<(&str, usize)> {
    let rest = &input[bracket_start + 1..];
    let close_bracket = rest.find(']')?;
    let after_bracket = bracket_start + 1 + close_bracket + 1;
    if !input[after_bracket..].starts_with('(') {
        return None;
    }
    let url_start = after_bracket + 1;
    let close_paren = input[url_start..].find(')')?;
    let url = input[url_start..url_start + close_paren].trim();
    if url.is_empty() || url.contains('\n') {
        return None;
    }
    let label = &input[bracket_start + 1..bracket_start + 1 + close_bracket];
    Some((label, url_start + close_paren + 1))
}

/// Parse inline Markdown (`**bold**`, `*italic*`, `***bold italic***`, `` `code` ``,
/// `~~strike~~`, `[label](url)`) into styled runs while preserving Windows paths verbatim.
pub(super) fn parse_inline_runs(input: &str) -> Vec<InlineRun> {
    let mut runs: Vec<InlineRun> = Vec::new();
    let mut plain = String::new();
    let flush_plain = |plain: &mut String, runs: &mut Vec<InlineRun>| {
        if !plain.is_empty() {
            runs.push(InlineRun {
                text: std::mem::take(plain),
                ..Default::default()
            });
        }
    };

    let bytes = input.as_bytes();
    let len = input.len();
    let mut i = 0;

    while i < len {
        // Escaped Markdown punctuation (never eats Windows path separators like `\T`).
        if bytes[i] == b'\\' && i + 1 < len {
            let next = bytes[i + 1];
            if matches!(next, b'*' | b'`' | b'_' | b'~' | b'[' | b']') {
                plain.push(next as char);
                i += 2;
                continue;
            }
        }

        // Inline code: `...` or ``...``
        if bytes[i] == b'`' {
            let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let delim = &input[i..i + ticks];
            if let Some(rel_end) = input[i + ticks..].find(delim) {
                let raw_code = &input[i + ticks..i + ticks + rel_end];
                let code = if ticks > 1 && raw_code.starts_with(' ') && raw_code.ends_with(' ') {
                    &raw_code[1..raw_code.len() - 1]
                } else {
                    raw_code
                };
                if !code.is_empty() {
                    flush_plain(&mut plain, &mut runs);
                    runs.push(InlineRun {
                        text: code.to_owned(),
                        code: true,
                        ..Default::default()
                    });
                }
                i += ticks + rel_end + ticks;
                continue;
            }
        }

        // Links `[label](url)` and images `![alt](url)`
        if bytes[i] == b'[' || (bytes[i] == b'!' && i + 1 < len && bytes[i + 1] == b'[') {
            let b_start = if bytes[i] == b'!' { i + 1 } else { i };
            if let Some((label, next_i)) = parse_link_at(input, b_start) {
                if !label.is_empty() {
                    flush_plain(&mut plain, &mut runs);
                    runs.push(InlineRun {
                        text: label.to_owned(),
                        underline: true,
                        color: Some("1D4ED8".to_owned()),
                        ..Default::default()
                    });
                }
                i = next_i;
                continue;
            }
        }

        // Strikethrough `~~...~~`
        if input[i..].starts_with("~~") {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "~~", false) {
                flush_plain(&mut plain, &mut runs);
                runs.push(InlineRun {
                    text: input[i + 2..i + 2 + rel_end].to_owned(),
                    strike: true,
                    ..Default::default()
                });
                i += 2 + rel_end + 2;
                continue;
            }
        }

        // Bold italic `***...***`
        if input[i..].starts_with("***") {
            if let Some(rel_end) = find_closing_delim(input, i + 3, "***", false) {
                flush_plain(&mut plain, &mut runs);
                runs.push(InlineRun {
                    text: input[i + 3..i + 3 + rel_end].to_owned(),
                    bold: true,
                    italic: true,
                    ..Default::default()
                });
                i += 3 + rel_end + 3;
                continue;
            }
        }

        // Bold `**...**`
        if input[i..].starts_with("**") {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "**", false) {
                flush_plain(&mut plain, &mut runs);
                runs.push(InlineRun {
                    text: input[i + 2..i + 2 + rel_end].to_owned(),
                    bold: true,
                    ..Default::default()
                });
                i += 2 + rel_end + 2;
                continue;
            }
        }

        // Bold `__...__` (word-boundary guarded)
        if input[i..].starts_with("__") && (i == 0 || !is_word_byte(bytes[i - 1])) {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "__", true) {
                flush_plain(&mut plain, &mut runs);
                runs.push(InlineRun {
                    text: input[i + 2..i + 2 + rel_end].to_owned(),
                    bold: true,
                    ..Default::default()
                });
                i += 2 + rel_end + 2;
                continue;
            }
        }

        // Italic `*...*`
        if bytes[i] == b'*' && !input[i..].starts_with("**") {
            if let Some(rel_end) = find_closing_delim(input, i + 1, "*", false) {
                flush_plain(&mut plain, &mut runs);
                runs.push(InlineRun {
                    text: input[i + 1..i + 1 + rel_end].to_owned(),
                    italic: true,
                    ..Default::default()
                });
                i += 1 + rel_end + 1;
                continue;
            }
        }

        // Italic `_..._` (word-boundary guarded so snake_case filenames stay intact)
        if bytes[i] == b'_'
            && !input[i..].starts_with("__")
            && (i == 0 || !is_word_byte(bytes[i - 1]))
        {
            if let Some(rel_end) = find_closing_delim(input, i + 1, "_", true) {
                flush_plain(&mut plain, &mut runs);
                runs.push(InlineRun {
                    text: input[i + 1..i + 1 + rel_end].to_owned(),
                    italic: true,
                    ..Default::default()
                });
                i += 1 + rel_end + 1;
                continue;
            }
        }

        let ch = input[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }

    flush_plain(&mut plain, &mut runs);
    runs
}

pub(super) fn parse_explicit_runs(val: &Value) -> Result<Vec<InlineRun>, String> {
    let arr = val
        .as_array()
        .ok_or_else(|| "'runs' must be a list of run objects".to_owned())?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(s) = item.as_str() {
            out.push(InlineRun {
                text: s.to_owned(),
                ..Default::default()
            });
            continue;
        }
        let obj = item
            .as_object()
            .ok_or_else(|| "each item in 'runs' must be an object or string".to_owned())?;
        let text = obj.get("text").map(value_to_string).unwrap_or_default();
        out.push(InlineRun {
            text,
            bold: obj.get("bold").and_then(Value::as_bool).unwrap_or(false),
            italic: obj.get("italic").and_then(Value::as_bool).unwrap_or(false),
            code: obj.get("code").and_then(Value::as_bool).unwrap_or(false),
            underline: obj
                .get("underline")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            strike: obj.get("strike").and_then(Value::as_bool).unwrap_or(false),
            color: obj
                .get("color")
                .and_then(Value::as_str)
                .and_then(sanitize_hex_color),
            size_pt: obj.get("size").and_then(Value::as_f64),
        });
    }
    Ok(out)
}

// ── Markdown block parser (used for DOCX `content` and PPTX string slides) ─────────────────────

#[derive(Clone, Debug)]
pub(super) struct TableData {
    headers: Option<Vec<String>>,
    rows: Vec<Vec<String>>,
}

#[derive(Clone, Debug)]
pub(super) enum DocBlock {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph {
        runs: Vec<InlineRun>,
        align: Option<String>,
    },
    Bullet {
        level: u8,
        text: String,
    },
    Numbered {
        level: u8,
        list_id: usize,
        text: String,
    },
    Quote {
        text: String,
    },
    Code {
        code: String,
    },
    Table(TableData),
    Rule,
    PageBreak,
}

pub(super) fn parse_md_heading(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = trimmed[hashes..].strip_prefix(' ')?;
    let title = rest.trim().trim_end_matches('#').trim();
    if title.is_empty() {
        None
    } else {
        Some((hashes as u8, title))
    }
}

fn parse_md_ordered(trimmed: &str) -> Option<&str> {
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 4 {
        return None;
    }
    let after = &trimmed[digits..];
    let rest = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some(rest)
}

fn is_md_rule(trimmed: &str) -> bool {
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.chars().all(|c| c == '-')
            || compact.chars().all(|c| c == '*')
            || compact.chars().all(|c| c == '_'))
}

fn split_md_table_row(line: &str) -> Vec<String> {
    let mut trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix('|') {
        trimmed = rest;
    }
    if let Some(rest) = trimmed.strip_suffix('|') {
        if !rest.ends_with('\\') {
            trimmed = rest;
        }
    }
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'|') {
            cur.push('|');
            chars.next();
        } else if ch == '|' {
            cells.push(cur.trim().to_owned());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    cells.push(cur.trim().to_owned());
    cells
}

fn parse_md_table_header(header_line: &str, sep_line: &str) -> Option<Vec<String>> {
    if !header_line.contains('|') || !sep_line.contains('|') {
        return None;
    }
    let headers = split_md_table_row(header_line);
    let seps = split_md_table_row(sep_line);
    if headers.is_empty() || headers.len() != seps.len() {
        return None;
    }
    for cell in &seps {
        let c = cell.trim();
        let core = c.strip_prefix(':').unwrap_or(c);
        let core = core.strip_suffix(':').unwrap_or(core);
        if core.len() < 3 || !core.chars().all(|ch| ch == '-') {
            return None;
        }
    }
    Some(headers)
}

pub(super) fn parse_markdown_to_doc_blocks(input: &str, next_list_id: &mut usize) -> Vec<DocBlock> {
    let lines: Vec<&str> = input.lines().collect();
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut in_code = false;
    let mut code_lines: Vec<&str> = Vec::new();
    let mut active_num_list: Option<usize> = None;

    let flush_para = |para: &mut Vec<&str>, blocks: &mut Vec<DocBlock>| {
        if !para.is_empty() {
            let joined = para.join("\n");
            blocks.push(DocBlock::Paragraph {
                runs: parse_inline_runs(&joined),
                align: None,
            });
            para.clear();
        }
    };

    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let leading_spaces = raw
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { 2 } else { 1 })
            .sum::<usize>();
        let indent = (leading_spaces / 2).min(2) as u8;
        let trimmed = raw.trim_start();

        if in_code {
            if trimmed.starts_with("```") {
                blocks.push(DocBlock::Code {
                    code: code_lines.join("\n"),
                });
                code_lines.clear();
                in_code = false;
            } else {
                code_lines.push(raw);
            }
            i += 1;
            continue;
        }

        if trimmed.starts_with("```") {
            flush_para(&mut para, &mut blocks);
            active_num_list = None;
            in_code = true;
            i += 1;
            continue;
        }

        if trimmed.is_empty() {
            flush_para(&mut para, &mut blocks);
            active_num_list = None;
            i += 1;
            continue;
        }

        if let Some(headers) = lines
            .get(i + 1)
            .and_then(|sep| parse_md_table_header(raw, sep))
        {
            flush_para(&mut para, &mut blocks);
            active_num_list = None;
            let col_count = headers.len();
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() {
                let row_trimmed = lines[i].trim();
                if row_trimmed.is_empty()
                    || row_trimmed.starts_with("```")
                    || parse_md_heading(row_trimmed).is_some()
                    || !row_trimmed.contains('|')
                {
                    break;
                }
                let mut cells = split_md_table_row(lines[i]);
                if cells.is_empty() {
                    break;
                }
                cells.resize(col_count, String::new());
                rows.push(cells);
                i += 1;
            }
            blocks.push(DocBlock::Table(TableData {
                headers: Some(headers),
                rows,
            }));
            continue;
        }

        if is_md_rule(trimmed) {
            flush_para(&mut para, &mut blocks);
            active_num_list = None;
            blocks.push(DocBlock::Rule);
            i += 1;
            continue;
        }

        if let Some((level, heading)) = parse_md_heading(trimmed) {
            flush_para(&mut para, &mut blocks);
            active_num_list = None;
            blocks.push(DocBlock::Heading {
                level,
                text: heading.to_owned(),
            });
            i += 1;
            continue;
        }

        if let Some(bullet) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
            .or_else(|| trimmed.strip_prefix("• "))
        {
            flush_para(&mut para, &mut blocks);
            active_num_list = None;
            blocks.push(DocBlock::Bullet {
                level: indent,
                text: bullet.to_owned(),
            });
            i += 1;
            continue;
        }

        if let Some(body) = parse_md_ordered(trimmed) {
            flush_para(&mut para, &mut blocks);
            let list_id = match active_num_list {
                Some(id) => id,
                None => {
                    let id = *next_list_id;
                    *next_list_id += 1;
                    active_num_list = Some(id);
                    id
                }
            };
            blocks.push(DocBlock::Numbered {
                level: indent,
                list_id,
                text: body.to_owned(),
            });
            i += 1;
            continue;
        }

        if let Some(quote) = trimmed
            .strip_prefix("> ")
            .or_else(|| (trimmed == ">").then_some(""))
        {
            flush_para(&mut para, &mut blocks);
            active_num_list = None;
            blocks.push(DocBlock::Quote {
                text: quote.to_owned(),
            });
            i += 1;
            continue;
        }

        active_num_list = None;
        para.push(raw);
        i += 1;
    }

    if in_code {
        blocks.push(DocBlock::Code {
            code: code_lines.join("\n"),
        });
    } else {
        flush_para(&mut para, &mut blocks);
    }

    blocks
}

pub(super) fn parse_table_value(val: &Value) -> Result<TableData, String> {
    let obj = val
        .as_object()
        .ok_or_else(|| "table must be an object with 'headers' and/or 'rows'".to_owned())?;
    let headers = match obj.get("headers") {
        Some(Value::Array(arr)) => Some(arr.iter().map(value_to_string).collect::<Vec<_>>()),
        Some(Value::Null) | None => None,
        _ => return Err("table 'headers' must be a list".to_owned()),
    };
    let rows_val = obj
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "table requires a 'rows' list".to_owned())?;
    let mut rows = Vec::with_capacity(rows_val.len());
    for row in rows_val {
        let cells = row
            .as_array()
            .ok_or_else(|| "each table row must be a list".to_owned())?;
        rows.push(cells.iter().map(value_to_string).collect());
    }
    let col_count = headers
        .as_ref()
        .map(Vec::len)
        .unwrap_or(0)
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if col_count == 0 {
        return Err("table must have at least one column".to_owned());
    }
    let headers = headers.map(|mut h| {
        h.resize(col_count, String::new());
        h
    });
    for row in &mut rows {
        row.resize(col_count, String::new());
    }
    Ok(TableData { headers, rows })
}

pub(super) fn parse_structured_doc_blocks(
    items: &[Value],
    next_list_id: &mut usize,
) -> Result<Vec<DocBlock>, String> {
    let mut blocks = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if let Some(text) = item.as_str() {
            blocks.extend(parse_markdown_to_doc_blocks(text, next_list_id));
            continue;
        }
        let obj = item
            .as_object()
            .ok_or_else(|| format!("blocks[{idx}] must be an object or string"))?;
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("paragraph")
            .to_ascii_lowercase();
        match kind.as_str() {
            "heading" | "h1" | "h2" | "h3" | "h4" => {
                let default_level = match kind.as_str() {
                    "h1" => 1,
                    "h2" => 2,
                    "h3" => 3,
                    "h4" => 4,
                    _ => 1,
                };
                let level = obj
                    .get("level")
                    .and_then(Value::as_u64)
                    .map(|l| l.clamp(1, 6) as u8)
                    .unwrap_or(default_level);
                let text = obj.get("text").map(value_to_string).unwrap_or_default();
                blocks.push(DocBlock::Heading { level, text });
            }
            "paragraph" | "p" | "text" => {
                let runs = if let Some(runs_val) = obj.get("runs") {
                    parse_explicit_runs(runs_val)?
                } else {
                    let text = obj.get("text").map(value_to_string).unwrap_or_default();
                    parse_inline_runs(&text)
                };
                let align = obj.get("align").and_then(Value::as_str).map(str::to_owned);
                blocks.push(DocBlock::Paragraph { runs, align });
            }
            "bullets" | "bullet" | "ul" => {
                if let Some(arr) = obj.get("items").and_then(Value::as_array) {
                    for entry in arr {
                        blocks.push(DocBlock::Bullet {
                            level: 0,
                            text: value_to_string(entry),
                        });
                    }
                } else if let Some(text) = obj.get("text").and_then(Value::as_str) {
                    let level = obj.get("level").and_then(Value::as_u64).unwrap_or(0).min(2) as u8;
                    blocks.push(DocBlock::Bullet {
                        level,
                        text: text.to_owned(),
                    });
                } else {
                    return Err(format!("blocks[{idx}] bullets requires 'items' or 'text'"));
                }
            }
            "numbered" | "ordered" | "ol" => {
                let list_id = *next_list_id;
                *next_list_id += 1;
                if let Some(arr) = obj.get("items").and_then(Value::as_array) {
                    for entry in arr {
                        blocks.push(DocBlock::Numbered {
                            level: 0,
                            list_id,
                            text: value_to_string(entry),
                        });
                    }
                } else if let Some(text) = obj.get("text").and_then(Value::as_str) {
                    blocks.push(DocBlock::Numbered {
                        level: 0,
                        list_id,
                        text: text.to_owned(),
                    });
                } else {
                    return Err(format!("blocks[{idx}] numbered requires 'items' or 'text'"));
                }
            }
            "list" => {
                let ordered = obj.get("ordered").and_then(Value::as_bool).unwrap_or(false);
                let arr = obj
                    .get("items")
                    .and_then(Value::as_array)
                    .ok_or_else(|| format!("blocks[{idx}] list requires 'items'"))?;
                if ordered {
                    let list_id = *next_list_id;
                    *next_list_id += 1;
                    for entry in arr {
                        blocks.push(DocBlock::Numbered {
                            level: 0,
                            list_id,
                            text: value_to_string(entry),
                        });
                    }
                } else {
                    for entry in arr {
                        blocks.push(DocBlock::Bullet {
                            level: 0,
                            text: value_to_string(entry),
                        });
                    }
                }
            }
            "table" => {
                blocks.push(DocBlock::Table(parse_table_value(item)?));
            }
            "quote" | "blockquote" => {
                let text = obj.get("text").map(value_to_string).unwrap_or_default();
                blocks.push(DocBlock::Quote { text });
            }
            "code" | "code_block" => {
                let code = obj
                    .get("code")
                    .or_else(|| obj.get("text"))
                    .map(value_to_string)
                    .unwrap_or_default();
                blocks.push(DocBlock::Code { code });
            }
            "rule" | "hr" | "divider" => blocks.push(DocBlock::Rule),
            "page_break" | "pagebreak" => blocks.push(DocBlock::PageBreak),
            other => {
                return Err(format!(
                    "blocks[{idx}] unsupported block type '{other}' (expected heading, paragraph, bullets, numbered, table, quote, code, rule, page_break)"
                ));
            }
        }
    }
    Ok(blocks)
}

mod docx;
mod pptx;
#[cfg(test)]
mod tests;

pub(crate) use docx::{docx_read, docx_write};
pub(crate) use pptx::{pptx_read, pptx_write};
