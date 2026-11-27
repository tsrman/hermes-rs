//! Pure-Rust Word (`.docx`) and PowerPoint (`.pptx`) helpers exposed to `execute_code`
//! scripts via `from hermes_tools import docx_read, docx_write, pptx_read, pptx_write`.
//!
//! Runs natively inside `hermes-runtime` on every platform (Windows, Linux GNU/musl, macOS) with
//! zero external Python packages (`python-docx`, `python-pptx`, MS Office) required.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use serde_json::{json, Value};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::doc_extract::{
    collect_docx_paragraph, find_descendants, parse_xml, slide_number, XmlNode, ZipArchive,
    MAX_DOCUMENT_BYTES,
};
use crate::excel_tools::resolve_workspace_path;

const DEFAULT_MAX_PARAGRAPHS: usize = 5_000;
const HARD_MAX_PARAGRAPHS: usize = 50_000;
const DEFAULT_MAX_SLIDES: usize = 200;
const HARD_MAX_SLIDES: usize = 1_000;

// ── Shared XML, ZIP, and inline Markdown helpers ───────────────────────────────────────────────

/// Escape XML special characters and strip illegal XML 1.0 control characters so Office apps
/// never show a "corrupted content" recovery dialog.
fn xml_escape(input: &str) -> String {
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

fn write_zip_entries<S: AsRef<str>>(path: &Path, entries: &[(S, String)]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("failed creating parent directory: {e}"))?;
    }
    let file = fs::File::create(path).map_err(|e| format!("failed creating output file: {e}"))?;
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
}

fn value_to_string(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn sanitize_hex_color(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('#');
    if trimmed.len() == 6 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(trimmed.to_ascii_uppercase())
    } else {
        None
    }
}

#[derive(Clone, Debug, Default)]
struct InlineRun {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,
    underline: bool,
    strike: bool,
    color: Option<String>,
    size_pt: Option<f64>,
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
fn parse_inline_runs(input: &str) -> Vec<InlineRun> {
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

fn parse_explicit_runs(val: &Value) -> Result<Vec<InlineRun>, String> {
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
struct TableData {
    headers: Option<Vec<String>>,
    rows: Vec<Vec<String>>,
}

#[derive(Clone, Debug)]
enum DocBlock {
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

fn parse_md_heading(trimmed: &str) -> Option<(u8, &str)> {
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

fn parse_markdown_to_doc_blocks(input: &str, next_list_id: &mut usize) -> Vec<DocBlock> {
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

fn parse_table_value(val: &Value) -> Result<TableData, String> {
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

fn parse_structured_doc_blocks(
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

// ── DOCX Reader & Writer ───────────────────────────────────────────────────────────────────────

/// Read a Word `.docx` document inside the workspace into structured paragraphs and tables.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.docx` path inside the workspace
/// - `max_paragraphs`: optional paragraph cap (default 5000, max 50000)
pub(crate) fn docx_read(workspace_root: &Path, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "docx_read requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, path_str, true) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };
    let max_paragraphs = args["max_paragraphs"]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(DEFAULT_MAX_PARAGRAPHS)
        .clamp(1, HARD_MAX_PARAGRAPHS);

    let data = match fs::read(&resolved) {
        Ok(b) => b,
        Err(err) => return json!({"error": format!("failed reading docx file: {err}")}),
    };
    if data.len() as u64 > MAX_DOCUMENT_BYTES {
        return json!({"error": format!("docx exceeds maximum size ({} bytes)", MAX_DOCUMENT_BYTES)});
    }
    let zip = match ZipArchive::from_bytes(&data, "DOCX") {
        Ok(z) => z,
        Err(err) => return json!({"error": err}),
    };
    let doc_bytes = match zip.read_entry("word/document.xml") {
        Ok(b) => b,
        Err(err) => return json!({"error": err}),
    };
    let root = match parse_xml(&doc_bytes, "word/document.xml") {
        Ok(r) => r,
        Err(err) => return json!({"error": err}),
    };

    let mut bodies = Vec::new();
    find_descendants(&root, "body", &mut bodies);
    let container = bodies.first().copied().unwrap_or(&root);

    let mut paragraphs = Vec::new();
    let mut tables = Vec::new();
    let mut truncated = false;

    for child in container.children() {
        match child.local_name() {
            "p" => {
                let mut buf = String::new();
                collect_docx_paragraph(child, &mut buf);
                if !buf.trim().is_empty() {
                    if paragraphs.len() < max_paragraphs {
                        paragraphs.push(buf);
                    } else {
                        truncated = true;
                    }
                }
            }
            "tbl" => {
                let mut tbl_rows = Vec::new();
                for tr in child.children().iter().filter(|n| n.local_name() == "tr") {
                    let mut row_cells = Vec::new();
                    for tc in tr.children().iter().filter(|n| n.local_name() == "tc") {
                        let mut cell_parts = Vec::new();
                        for p in tc.children().iter().filter(|n| n.local_name() == "p") {
                            let mut buf = String::new();
                            collect_docx_paragraph(p, &mut buf);
                            if !buf.is_empty() {
                                cell_parts.push(buf);
                            }
                        }
                        row_cells.push(cell_parts.join("\n"));
                    }
                    if !row_cells.is_empty() {
                        tbl_rows.push(row_cells);
                    }
                }
                if !tbl_rows.is_empty() {
                    tables.push(tbl_rows);
                }
            }
            _ => {}
        }
    }

    json!({
        "path": path_str,
        "paragraphs": paragraphs,
        "tables": tables,
        "paragraph_count": paragraphs.len(),
        "table_count": tables.len(),
        "truncated": truncated,
    })
}

fn docx_push_run(out: &mut String, run: &InlineRun, force_bold: bool) {
    if run.text.is_empty() {
        return;
    }
    let is_bold = force_bold || run.bold;
    out.push_str("<w:r>");
    let has_props = is_bold
        || run.italic
        || run.code
        || run.underline
        || run.strike
        || run.color.is_some()
        || run.size_pt.is_some();
    if has_props {
        out.push_str("<w:rPr>");
        if run.code {
            out.push_str(
                r#"<w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas" w:eastAsia="Consolas"/>"#,
            );
        }
        if is_bold {
            out.push_str("<w:b/><w:bCs/>");
        }
        if run.italic {
            out.push_str("<w:i/><w:iCs/>");
        }
        if run.strike {
            out.push_str("<w:strike/>");
        }
        if let Some(color) = &run.color {
            let _ = write!(out, r#"<w:color w:val="{color}"/>"#);
        } else if run.code {
            out.push_str(r#"<w:color w:val="1E3A8A"/>"#);
        }
        if let Some(pt) = run.size_pt {
            let half_pts = (pt * 2.0).round().clamp(12.0, 144.0) as u32;
            let _ = write!(
                out,
                r#"<w:sz w:val="{half_pts}"/><w:szCs w:val="{half_pts}"/>"#
            );
        } else if run.code {
            out.push_str(r#"<w:sz w:val="20"/><w:szCs w:val="20"/>"#);
        }
        if run.underline {
            out.push_str(r#"<w:u w:val="single"/>"#);
        }
        if run.code {
            out.push_str(r#"<w:shd w:val="clear" w:color="auto" w:fill="EEF2F6"/>"#);
        }
        out.push_str("</w:rPr>");
    }

    let lines: Vec<&str> = run.text.split('\n').collect();
    for (idx, part) in lines.iter().enumerate() {
        if idx > 0 {
            out.push_str("<w:br/>");
        }
        if !part.is_empty() {
            let escaped = xml_escape(part);
            let _ = write!(out, r#"<w:t xml:space="preserve">{escaped}</w:t>"#);
        }
    }
    out.push_str("</w:r>");
}

fn docx_push_runs(out: &mut String, runs: &[InlineRun], force_bold: bool) {
    for run in runs {
        docx_push_run(out, run, force_bold);
    }
}

fn docx_jc_val(align: Option<&str>) -> Option<&'static str> {
    match align.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("center") => Some("center"),
        Some("right") => Some("right"),
        Some("justify" | "both") => Some("both"),
        Some("left") => Some("left"),
        _ => None,
    }
}

fn docx_render_table(out: &mut String, tbl: &TableData) {
    let col_count = tbl
        .headers
        .as_ref()
        .map(Vec::len)
        .unwrap_or_else(|| tbl.rows.first().map(Vec::len).unwrap_or(1))
        .max(1);
    let total_width = 9026usize; // A4 usable width (11906 - 2*1440 dxa)
    let col_width = (total_width / col_count).max(600);

    out.push_str("<w:tbl><w:tblPr>");
    let _ = write!(out, r#"<w:tblW w:w="{total_width}" w:type="dxa"/>"#);
    out.push_str(
        r#"<w:tblBorders><w:top w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:left w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:bottom w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:right w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:insideH w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:insideV w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/></w:tblBorders>"#,
    );
    out.push_str(
        r#"<w:tblCellMar><w:top w:w="80" w:type="dxa"/><w:left w:w="120" w:type="dxa"/><w:bottom w:w="80" w:type="dxa"/><w:right w:w="120" w:type="dxa"/></w:tblCellMar>"#,
    );
    out.push_str("</w:tblPr><w:tblGrid>");
    for _ in 0..col_count {
        let _ = write!(out, r#"<w:gridCol w:w="{col_width}"/>"#);
    }
    out.push_str("</w:tblGrid>");

    if let Some(headers) = &tbl.headers {
        out.push_str("<w:tr><w:trPr><w:cantSplit/><w:tblHeader/></w:trPr>");
        for cell in headers {
            out.push_str("<w:tc><w:tcPr>");
            let _ = write!(out, r#"<w:tcW w:w="{col_width}" w:type="dxa"/>"#);
            out.push_str(
                r#"<w:shd w:val="clear" w:color="auto" w:fill="E8EEF5"/><w:vAlign w:val="center"/></w:tcPr>"#,
            );
            out.push_str(r#"<w:p><w:pPr><w:spacing w:before="40" w:after="40"/></w:pPr>"#);
            let runs = parse_inline_runs(cell);
            docx_push_runs(out, &runs, true);
            out.push_str("</w:p></w:tc>");
        }
        out.push_str("</w:tr>");
    }

    for (r_idx, row) in tbl.rows.iter().enumerate() {
        out.push_str("<w:tr><w:trPr><w:cantSplit/></w:trPr>");
        let fill = if r_idx % 2 == 1 { Some("F8FAFC") } else { None };
        for cell in row {
            out.push_str("<w:tc><w:tcPr>");
            let _ = write!(out, r#"<w:tcW w:w="{col_width}" w:type="dxa"/>"#);
            if let Some(bg) = fill {
                let _ = write!(
                    out,
                    r#"<w:shd w:val="clear" w:color="auto" w:fill="{bg}"/>"#
                );
            }
            out.push_str(r#"<w:vAlign w:val="center"/></w:tcPr>"#);
            out.push_str(r#"<w:p><w:pPr><w:spacing w:before="40" w:after="40"/></w:pPr>"#);
            let runs = parse_inline_runs(cell);
            docx_push_runs(out, &runs, false);
            out.push_str("</w:p></w:tc>");
        }
        out.push_str("</w:tr>");
    }
    out.push_str("</w:tbl><w:p><w:pPr><w:spacing w:after=\"120\"/></w:pPr></w:p>");
}

fn docx_numbering_xml(max_num_list_id: usize) -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="0">
    <w:multiLevelType w:val="hybridMultilevel"/>
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl>
    <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="◦"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr></w:lvl>
    <w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="▪"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="2160" w:hanging="360"/></w:pPr></w:lvl>
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="1">
    <w:multiLevelType w:val="hybridMultilevel"/>
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl>
    <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr></w:lvl>
    <w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="lowerRoman"/><w:lvlText w:val="%3."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="2160" w:hanging="360"/></w:pPr></w:lvl>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
"#,
    );
    for id in 2..=max_num_list_id.max(2) {
        let _ = writeln!(
            xml,
            r#"  <w:num w:numId="{id}"><w:abstractNumId w:val="1"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="1"/></w:lvlOverride></w:num>"#
        );
    }
    xml.push_str("</w:numbering>\n");
    xml
}

fn docx_styles_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:docDefaults>
    <w:rPrDefault>
      <w:rPr>
        <w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Calibri" w:eastAsia="Calibri"/>
        <w:color w:val="1F2933"/>
        <w:sz w:val="22"/>
        <w:szCs w:val="22"/>
      </w:rPr>
    </w:rPrDefault>
    <w:pPrDefault>
      <w:pPr>
        <w:spacing w:after="120" w:line="276" w:lineRule="auto"/>
      </w:pPr>
    </w:pPrDefault>
  </w:docDefaults>
  <w:style w:type="paragraph" w:default="1" w:styleId="Normal">
    <w:name w:val="Normal"/>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Title">
    <w:name w:val="Title"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:keepNext/>
      <w:spacing w:before="0" w:after="140"/>
    </w:pPr>
    <w:rPr>
      <w:b/><w:bCs/>
      <w:color w:val="1F4E79"/>
      <w:sz w:val="48"/><w:szCs w:val="48"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Subtitle">
    <w:name w:val="Subtitle"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:spacing w:before="0" w:after="220"/>
    </w:pPr>
    <w:rPr>
      <w:color w:val="595959"/>
      <w:sz w:val="26"/><w:szCs w:val="26"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:keepNext/>
      <w:spacing w:before="280" w:after="120"/>
      <w:outlineLvl w:val="0"/>
    </w:pPr>
    <w:rPr>
      <w:b/><w:bCs/>
      <w:color w:val="1F4E79"/>
      <w:sz w:val="34"/><w:szCs w:val="34"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Heading2">
    <w:name w:val="heading 2"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:keepNext/>
      <w:spacing w:before="220" w:after="100"/>
      <w:outlineLvl w:val="1"/>
    </w:pPr>
    <w:rPr>
      <w:b/><w:bCs/>
      <w:color w:val="2E75B6"/>
      <w:sz w:val="28"/><w:szCs w:val="28"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Heading3">
    <w:name w:val="heading 3"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:keepNext/>
      <w:spacing w:before="180" w:after="80"/>
      <w:outlineLvl w:val="2"/>
    </w:pPr>
    <w:rPr>
      <w:b/><w:bCs/>
      <w:color w:val="2E75B6"/>
      <w:sz w:val="24"/><w:szCs w:val="24"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Quote">
    <w:name w:val="Quote"/>
    <w:basedOn w:val="Normal"/>
    <w:pPr>
      <w:pBdr><w:left w:val="single" w:sz="18" w:space="12" w:color="2E75B6"/></w:pBdr>
      <w:shd w:val="clear" w:color="auto" w:fill="F8FAFC"/>
      <w:spacing w:before="100" w:after="120"/>
      <w:ind w:left="360" w:right="240"/>
    </w:pPr>
    <w:rPr>
      <w:i/><w:iCs/>
      <w:color w:val="4A5568"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="CodeBlock">
    <w:name w:val="Code Block"/>
    <w:basedOn w:val="Normal"/>
    <w:pPr>
      <w:pBdr>
        <w:top w:val="single" w:sz="4" w:space="6" w:color="CBD5E1"/>
        <w:left w:val="single" w:sz="4" w:space="8" w:color="CBD5E1"/>
        <w:bottom w:val="single" w:sz="4" w:space="6" w:color="CBD5E1"/>
        <w:right w:val="single" w:sz="4" w:space="8" w:color="CBD5E1"/>
      </w:pBdr>
      <w:shd w:val="clear" w:color="auto" w:fill="F3F4F6"/>
      <w:spacing w:before="100" w:after="140" w:line="240" w:lineRule="auto"/>
      <w:ind w:left="180" w:right="180"/>
    </w:pPr>
    <w:rPr>
      <w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas" w:eastAsia="Consolas"/>
      <w:color w:val="111827"/>
      <w:sz w:val="19"/><w:szCs w:val="19"/>
    </w:rPr>
  </w:style>
</w:styles>
"#
    .to_owned()
}

fn core_props_xml(title: &str, author: &str) -> String {
    let t = xml_escape(title);
    let a = xml_escape(author);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dc:title>{t}</dc:title>
  <dc:creator>{a}</dc:creator>
  <cp:lastModifiedBy>{a}</cp:lastModifiedBy>
</cp:coreProperties>
"#
    )
}

/// Create a formatted `.docx` Word document inside the workspace.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.docx` path inside the workspace
/// - `content`: optional Markdown string OR list of structured block objects
/// - `blocks`: optional list of structured block objects (`heading`, `paragraph`, `bullets`,
///   `numbered`, `table`, `quote`, `code`, `rule`, `page_break`)
/// - `title`: optional document title
/// - `subtitle`: optional document subtitle
/// - `author`: optional author metadata (defaults to `"Hermes"`)
pub(crate) fn docx_write(workspace_root: &Path, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "docx_write requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, path_str, false) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };

    let title = args["title"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let subtitle = args["subtitle"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let author = args["author"].as_str().unwrap_or("Hermes");

    let mut next_list_id = 2usize; // numId 1 is reserved for bullets
    let mut blocks = Vec::new();

    if let Some(blocks_arr) = args.get("blocks").and_then(Value::as_array) {
        match parse_structured_doc_blocks(blocks_arr, &mut next_list_id) {
            Ok(b) => blocks.extend(b),
            Err(err) => return json!({"error": err}),
        }
    }
    match args.get("content") {
        Some(Value::String(md)) if !md.trim().is_empty() => {
            blocks.extend(parse_markdown_to_doc_blocks(md, &mut next_list_id));
        }
        Some(Value::Array(arr)) => match parse_structured_doc_blocks(arr, &mut next_list_id) {
            Ok(b) => blocks.extend(b),
            Err(err) => return json!({"error": err}),
        },
        Some(Value::Null) | None => {}
        Some(Value::String(_)) => {}
        _ => {
            return json!({"error": "'content' must be a Markdown string or a list of block objects"});
        }
    }

    if title.is_none() && subtitle.is_none() && blocks.is_empty() {
        return json!({"error": "docx_write requires non-empty 'content', 'blocks', or 'title'"});
    }

    let mut body = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
"#,
    );

    if let Some(t) = title {
        body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr>"#);
        let runs = parse_inline_runs(t);
        docx_push_runs(&mut body, &runs, false);
        body.push_str("</w:p>");
    }
    if let Some(st) = subtitle {
        body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Subtitle"/></w:pPr>"#);
        let runs = parse_inline_runs(st);
        docx_push_runs(&mut body, &runs, false);
        body.push_str("</w:p>");
    }

    let mut tables_written = 0usize;
    for block in &blocks {
        match block {
            DocBlock::Heading { level, text } => {
                let style = match level {
                    1 => "Heading1",
                    2 => "Heading2",
                    _ => "Heading3",
                };
                let _ = write!(body, r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr>"#);
                let runs = parse_inline_runs(text);
                docx_push_runs(&mut body, &runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Paragraph { runs, align } => {
                body.push_str("<w:p>");
                if let Some(jc) = docx_jc_val(align.as_deref()) {
                    let _ = write!(body, r#"<w:pPr><w:jc w:val="{jc}"/></w:pPr>"#);
                }
                docx_push_runs(&mut body, runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Bullet { level, text } => {
                let ilvl = (*level).min(2);
                let _ = write!(
                    body,
                    r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="1"/></w:numPr><w:spacing w:after="60"/></w:pPr>"#
                );
                let runs = parse_inline_runs(text);
                docx_push_runs(&mut body, &runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Numbered {
                level,
                list_id,
                text,
            } => {
                let ilvl = (*level).min(2);
                let _ = write!(
                    body,
                    r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="{list_id}"/></w:numPr><w:spacing w:after="60"/></w:pPr>"#
                );
                let runs = parse_inline_runs(text);
                docx_push_runs(&mut body, &runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Quote { text } => {
                body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Quote"/></w:pPr>"#);
                let runs = parse_inline_runs(text);
                docx_push_runs(&mut body, &runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Code { code } => {
                body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="CodeBlock"/></w:pPr>"#);
                let run = InlineRun {
                    text: code.clone(),
                    ..Default::default()
                };
                docx_push_run(&mut body, &run, false);
                body.push_str("</w:p>");
            }
            DocBlock::Table(tbl) => {
                tables_written += 1;
                docx_render_table(&mut body, tbl);
            }
            DocBlock::Rule => {
                body.push_str(
                    r#"<w:p><w:pPr><w:pBdr><w:bottom w:val="single" w:sz="6" w:space="1" w:color="CBD5E1"/></w:pBdr><w:spacing w:before="120" w:after="160"/></w:pPr></w:p>"#,
                );
            }
            DocBlock::PageBreak => {
                body.push_str(r#"<w:p><w:r><w:br w:type="page"/></w:r></w:p>"#);
            }
        }
    }

    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>
</w:body>
</w:document>
"#,
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
  <Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
  <Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>
</Types>
"#
    .to_owned();

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
</Relationships>
"#
    .to_owned();

    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>
"#
    .to_owned();

    let app_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties">
  <Application>Hermes</Application>
</Properties>
"#
    .to_owned();

    let doc_title = title.unwrap_or("Document");
    let entries = vec![
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", docx_styles_xml()),
        ("word/numbering.xml", docx_numbering_xml(next_list_id - 1)),
        ("word/document.xml", body),
        ("docProps/core.xml", core_props_xml(doc_title, author)),
        ("docProps/app.xml", app_props),
    ];

    if let Err(err) = write_zip_entries(&resolved, &entries) {
        return json!({"error": err});
    }

    json!({
        "status": "ok",
        "path": path_str,
        "blocks_written": blocks.len() + usize::from(title.is_some()) + usize::from(subtitle.is_some()),
        "tables_written": tables_written,
    })
}

// ── PPTX Reader & Writer ───────────────────────────────────────────────────────────────────────

fn extract_a_paragraph_text(p: &XmlNode, skip_slidenum_fields: bool) -> String {
    let mut out = String::new();
    for child in p.children() {
        match child.local_name() {
            "r" => {
                for sub in child.children() {
                    if sub.local_name() == "t" {
                        out.push_str(&sub.collect_text());
                    }
                }
            }
            "fld" => {
                if skip_slidenum_fields && child.attr("type") == Some("slidenum") {
                    continue;
                }
                for sub in child.children() {
                    if sub.local_name() == "t" {
                        out.push_str(&sub.collect_text());
                    }
                }
            }
            "br" => out.push('\n'),
            _ => {}
        }
    }
    out
}

fn resolve_notes_path(zip: &ZipArchive<'_>, slide_num: usize) -> Option<String> {
    let rels_path = format!("ppt/slides/_rels/slide{slide_num}.xml.rels");
    if let Ok(rels_bytes) = zip.read_entry(&rels_path) {
        if let Ok(root) = parse_xml(&rels_bytes, &rels_path) {
            let mut rels = Vec::new();
            find_descendants(&root, "Relationship", &mut rels);
            for rel in rels {
                let is_notes = rel.attr("Type").is_some_and(|t| t.ends_with("/notesSlide"));
                if !is_notes {
                    continue;
                }
                if let Some(target) = rel.attr("Target") {
                    let norm = if let Some(rest) = target.strip_prefix("../") {
                        format!("ppt/{rest}")
                    } else if let Some(rest) = target.strip_prefix('/') {
                        rest.to_owned()
                    } else {
                        format!("ppt/slides/{target}")
                    };
                    if zip.contains(&norm) {
                        return Some(norm);
                    }
                }
            }
        }
    }
    let fallback = format!("ppt/notesSlides/notesSlide{slide_num}.xml");
    zip.contains(&fallback).then_some(fallback)
}

/// Read a PowerPoint `.pptx` presentation inside the workspace into structured slides, tables,
/// and speaker notes.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.pptx` path inside the workspace
/// - `max_slides`: optional slide cap (default 200, max 1000)
pub(crate) fn pptx_read(workspace_root: &Path, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "pptx_read requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, path_str, true) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };
    let max_slides = args["max_slides"]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(DEFAULT_MAX_SLIDES)
        .clamp(1, HARD_MAX_SLIDES);

    let data = match fs::read(&resolved) {
        Ok(b) => b,
        Err(err) => return json!({"error": format!("failed reading pptx file: {err}")}),
    };
    if data.len() as u64 > MAX_DOCUMENT_BYTES {
        return json!({"error": format!("pptx exceeds maximum size ({} bytes)", MAX_DOCUMENT_BYTES)});
    }
    let zip = match ZipArchive::from_bytes(&data, "PPTX") {
        Ok(z) => z,
        Err(err) => return json!({"error": err}),
    };

    let mut slide_entries: Vec<(usize, String)> = zip
        .entry_names()
        .iter()
        .filter_map(|name| slide_number(name).map(|n| (n, name.clone())))
        .collect();
    slide_entries.sort_by_key(|(n, _)| *n);

    let total_slides = slide_entries.len();
    let truncated = total_slides > max_slides;
    let mut slides_out = Vec::new();

    for (num, entry_name) in slide_entries.into_iter().take(max_slides) {
        let xml_bytes = match zip.read_entry(&entry_name) {
            Ok(b) => b,
            Err(err) => return json!({"error": err}),
        };
        let root = match parse_xml(&xml_bytes, &entry_name) {
            Ok(r) => r,
            Err(err) => return json!({"error": err}),
        };

        let mut title: Option<String> = None;
        let mut texts = Vec::new();
        let mut shapes = Vec::new();
        find_descendants(&root, "sp", &mut shapes);
        for sp in shapes {
            let mut phs = Vec::new();
            find_descendants(sp, "ph", &mut phs);
            let is_title_ph = phs
                .iter()
                .any(|p| matches!(p.attr("type"), Some("title" | "ctrTitle")));
            let is_slide_num_shape = sp
                .children()
                .iter()
                .find(|c| c.local_name() == "nvSpPr")
                .and_then(|nv| nv.children().iter().find(|c| c.local_name() == "cNvPr"))
                .and_then(|c| c.attr("name"))
                .is_some_and(|n| n == "Slide Number");

            let mut paras = Vec::new();
            find_descendants(sp, "p", &mut paras);
            let mut shape_lines = Vec::new();
            for p in paras {
                let text = extract_a_paragraph_text(p, true);
                if !text.trim().is_empty() {
                    shape_lines.push(text);
                }
            }
            if is_slide_num_shape {
                continue;
            }
            if is_title_ph && title.is_none() && !shape_lines.is_empty() {
                title = Some(shape_lines.join(" "));
            }
            texts.extend(shape_lines);
        }
        if title.is_none() {
            title = texts.first().cloned();
        }

        let mut tables = Vec::new();
        let mut tbl_nodes = Vec::new();
        find_descendants(&root, "tbl", &mut tbl_nodes);
        for tbl in tbl_nodes {
            let mut tbl_rows = Vec::new();
            for tr in tbl.children().iter().filter(|n| n.local_name() == "tr") {
                let mut row_cells = Vec::new();
                for tc in tr.children().iter().filter(|n| n.local_name() == "tc") {
                    let mut paras = Vec::new();
                    find_descendants(tc, "p", &mut paras);
                    let cell_text: Vec<String> = paras
                        .into_iter()
                        .map(|p| extract_a_paragraph_text(p, false))
                        .filter(|s| !s.is_empty())
                        .collect();
                    row_cells.push(cell_text.join("\n"));
                }
                if !row_cells.is_empty() {
                    tbl_rows.push(row_cells);
                }
            }
            if !tbl_rows.is_empty() {
                tables.push(tbl_rows);
            }
        }

        let notes = resolve_notes_path(&zip, num).and_then(|notes_path| {
            let bytes = zip.read_entry(&notes_path).ok()?;
            let notes_root = parse_xml(&bytes, &notes_path).ok()?;
            let mut paras = Vec::new();
            find_descendants(&notes_root, "p", &mut paras);
            let lines: Vec<String> = paras
                .into_iter()
                .map(|p| extract_a_paragraph_text(p, true))
                .filter(|s| !s.trim().is_empty())
                .collect();
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        });

        slides_out.push(json!({
            "number": num,
            "title": title,
            "texts": texts,
            "tables": tables,
            "notes": notes,
        }));
    }

    json!({
        "path": path_str,
        "slide_count": total_slides,
        "slides": slides_out,
        "truncated": truncated,
    })
}

#[derive(Clone, Copy)]
struct SlideTheme {
    bg: &'static str,
    cover_bg: &'static str,
    accent: &'static str,
    accent_soft: &'static str,
    title_fg: &'static str,
    cover_title_fg: &'static str,
    cover_sub_fg: &'static str,
    body_fg: &'static str,
    muted_fg: &'static str,
    table_hdr_bg: &'static str,
    table_hdr_fg: &'static str,
    table_alt_bg: &'static str,
    table_border: &'static str,
}

fn resolve_slide_theme(name: Option<&str>) -> SlideTheme {
    match name.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("dark") => SlideTheme {
            bg: "0F172A",
            cover_bg: "0B1120",
            accent: "38BDF8",
            accent_soft: "1E293B",
            title_fg: "F8FAFC",
            cover_title_fg: "FFFFFF",
            cover_sub_fg: "CBD5E1",
            body_fg: "E2E8F0",
            muted_fg: "94A3B8",
            table_hdr_bg: "1E3A5F",
            table_hdr_fg: "FFFFFF",
            table_alt_bg: "1E293B",
            table_border: "334155",
        },
        Some("minimal") => SlideTheme {
            bg: "FFFFFF",
            cover_bg: "111827",
            accent: "059669",
            accent_soft: "ECFDF5",
            title_fg: "111827",
            cover_title_fg: "FFFFFF",
            cover_sub_fg: "D1FAE5",
            body_fg: "374151",
            muted_fg: "6B7280",
            table_hdr_bg: "111827",
            table_hdr_fg: "FFFFFF",
            table_alt_bg: "F9FAFB",
            table_border: "D1D5DB",
        },
        _ => SlideTheme {
            bg: "FFFFFF",
            cover_bg: "1F4E79",
            accent: "1F4E79",
            accent_soft: "E8EEF5",
            title_fg: "1F4E79",
            cover_title_fg: "FFFFFF",
            cover_sub_fg: "DCE6F2",
            body_fg: "1F2933",
            muted_fg: "64748B",
            table_hdr_bg: "1F4E79",
            table_hdr_fg: "FFFFFF",
            table_alt_bg: "F1F5F9",
            table_border: "CBD5E1",
        },
    }
}

#[derive(Clone, Debug, Default)]
struct ColumnSpec {
    title: Option<String>,
    bullets: Vec<String>,
    paragraphs: Vec<String>,
}

#[derive(Clone, Debug)]
struct SlideSpec {
    layout: String,
    title: String,
    subtitle: Option<String>,
    bullets: Vec<String>,
    paragraphs: Vec<String>,
    table: Option<TableData>,
    left: Option<ColumnSpec>,
    right: Option<ColumnSpec>,
    notes: Option<String>,
}

fn parse_column_spec(val: &Value) -> Result<ColumnSpec, String> {
    if let Some(s) = val.as_str() {
        return Ok(ColumnSpec {
            title: None,
            bullets: Vec::new(),
            paragraphs: vec![s.to_owned()],
        });
    }
    if let Some(arr) = val.as_array() {
        return Ok(ColumnSpec {
            title: None,
            bullets: arr.iter().map(value_to_string).collect(),
            paragraphs: Vec::new(),
        });
    }
    let obj = val
        .as_object()
        .ok_or_else(|| "column spec must be an object, list, or string".to_owned())?;
    let title = obj.get("title").and_then(Value::as_str).map(str::to_owned);
    let bullets = obj
        .get("bullets")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(value_to_string).collect())
        .unwrap_or_default();
    let mut paragraphs = Vec::new();
    if let Some(body) = obj.get("body").and_then(Value::as_str) {
        if !body.trim().is_empty() {
            paragraphs.push(body.to_owned());
        }
    }
    if let Some(arr) = obj.get("paragraphs").and_then(Value::as_array) {
        paragraphs.extend(arr.iter().map(value_to_string));
    }
    Ok(ColumnSpec {
        title,
        bullets,
        paragraphs,
    })
}

fn parse_slide_spec(idx: usize, val: &Value) -> Result<SlideSpec, String> {
    if let Some(raw) = val.as_str() {
        let mut title = String::new();
        let mut bullets = Vec::new();
        let mut paragraphs = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if title.is_empty() {
                if let Some((_, h)) = parse_md_heading(trimmed) {
                    title = h.to_owned();
                    continue;
                }
            }
            if let Some(b) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .or_else(|| trimmed.strip_prefix("• "))
            {
                bullets.push(b.to_owned());
            } else if title.is_empty() {
                title = trimmed.to_owned();
            } else {
                paragraphs.push(trimmed.to_owned());
            }
        }
        let layout = if idx == 0 && bullets.is_empty() && paragraphs.len() <= 1 {
            "title".to_owned()
        } else {
            "content".to_owned()
        };
        let subtitle = if layout == "title" && paragraphs.len() == 1 {
            paragraphs.pop()
        } else {
            None
        };
        return Ok(SlideSpec {
            layout,
            title,
            subtitle,
            bullets,
            paragraphs,
            table: None,
            left: None,
            right: None,
            notes: None,
        });
    }

    let obj = val
        .as_object()
        .ok_or_else(|| format!("slides[{idx}] must be an object or string"))?;
    let title = obj.get("title").map(value_to_string).unwrap_or_default();
    let subtitle = obj
        .get("subtitle")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let bullets: Vec<String> = obj
        .get("bullets")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(value_to_string).collect())
        .unwrap_or_default();
    let mut paragraphs = Vec::new();
    if let Some(body) = obj.get("body").and_then(Value::as_str) {
        for part in body.split("\n\n") {
            let t = part.trim();
            if !t.is_empty() {
                paragraphs.push(t.to_owned());
            }
        }
    }
    if let Some(arr) = obj.get("paragraphs").and_then(Value::as_array) {
        paragraphs.extend(arr.iter().map(value_to_string));
    }
    let table = match obj.get("table") {
        Some(Value::Null) | None => None,
        Some(t) => Some(parse_table_value(t)?),
    };
    let left = match obj.get("left") {
        Some(Value::Null) | None => None,
        Some(v) => Some(parse_column_spec(v)?),
    };
    let right = match obj.get("right") {
        Some(Value::Null) | None => None,
        Some(v) => Some(parse_column_spec(v)?),
    };
    let notes = obj
        .get("notes")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let has_body = !bullets.is_empty()
        || !paragraphs.is_empty()
        || table.is_some()
        || left.is_some()
        || right.is_some();
    let layout = obj
        .get("layout")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| {
            if idx == 0 && !has_body {
                "title".to_owned()
            } else {
                "content".to_owned()
            }
        });

    Ok(SlideSpec {
        layout,
        title,
        subtitle,
        bullets,
        paragraphs,
        table,
        left,
        right,
        notes,
    })
}

fn pptx_push_inline_runs(
    out: &mut String,
    text: &str,
    size_hundredths: u32,
    default_color: &str,
    default_bold: bool,
) {
    let runs = parse_inline_runs(text);
    if runs.is_empty() {
        let _ = write!(
            out,
            r#"<a:endParaRPr lang="ru-RU" sz="{size_hundredths}" dirty="0"/>"#
        );
        return;
    }
    for run in runs {
        let lines: Vec<&str> = run.text.split('\n').collect();
        for (l_idx, part) in lines.iter().enumerate() {
            if l_idx > 0 {
                let _ = write!(
                    out,
                    r#"<a:br><a:rPr lang="ru-RU" sz="{size_hundredths}" dirty="0"/></a:br>"#
                );
            }
            if part.is_empty() {
                continue;
            }
            let b_attr = if default_bold || run.bold {
                r#" b="1""#
            } else {
                ""
            };
            let i_attr = if run.italic { r#" i="1""# } else { "" };
            let u_attr = if run.underline { r#" u="sng""# } else { "" };
            let s_attr = if run.strike {
                r#" strike="sngStrike""#
            } else {
                ""
            };
            let color = run.color.as_deref().unwrap_or(default_color);
            let font = if run.code { "Consolas" } else { "Calibri" };
            let sz = run
                .size_pt
                .map(|p| (p * 100.0).round().clamp(800.0, 9600.0) as u32)
                .unwrap_or(size_hundredths);
            let escaped = xml_escape(part);
            let _ = write!(
                out,
                r#"<a:r><a:rPr lang="ru-RU" sz="{sz}"{b_attr}{i_attr}{u_attr}{s_attr} dirty="0"><a:solidFill><a:srgbClr val="{color}"/></a:solidFill><a:latin typeface="{font}"/><a:ea typeface="{font}"/><a:cs typeface="{font}"/></a:rPr><a:t>{escaped}</a:t></a:r>"#
            );
        }
    }
    let _ = write!(
        out,
        r#"<a:endParaRPr lang="ru-RU" sz="{size_hundredths}" dirty="0"/>"#
    );
}

#[allow(clippy::too_many_arguments)]
fn pptx_push_rect(
    out: &mut String,
    shape_id: &mut u32,
    name: &str,
    x: i64,
    y: i64,
    cx: i64,
    cy: i64,
    fill_hex: &str,
) {
    let id = *shape_id;
    *shape_id += 1;
    let esc_name = xml_escape(name);
    let _ = write!(
        out,
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{esc_name}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="{fill_hex}"/></a:solidFill><a:ln><a:noFill/></a:ln></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="ru-RU"/></a:p></p:txBody></p:sp>"#
    );
}

#[allow(clippy::too_many_arguments)]
fn pptx_push_textbox(
    out: &mut String,
    shape_id: &mut u32,
    name: &str,
    x: i64,
    y: i64,
    cx: i64,
    cy: i64,
    anchor: &str,
    paragraphs_xml: &str,
) {
    let id = *shape_id;
    *shape_id += 1;
    let esc_name = xml_escape(name);
    let body = if paragraphs_xml.is_empty() {
        r#"<a:p><a:endParaRPr lang="ru-RU"/></a:p>"#
    } else {
        paragraphs_xml
    };
    let _ = write!(
        out,
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{esc_name}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/></p:spPr><p:txBody><a:bodyPr wrap="square" lIns="91440" tIns="45720" rIns="91440" bIns="45720" anchor="{anchor}"/><a:lstStyle/>{body}</p:txBody></p:sp>"#
    );
}

#[allow(clippy::too_many_arguments)]
fn pptx_render_content_paragraphs(
    heading: Option<&str>,
    paragraphs: &[String],
    bullets: &[String],
    theme: SlideTheme,
    font_sz: u32,
) -> String {
    let mut xml = String::new();
    if let Some(h) = heading {
        if !h.trim().is_empty() {
            xml.push_str(r#"<a:p><a:pPr algn="l"><a:spcAft><a:spcPts val="1000"/></a:spcAft><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(&mut xml, h, font_sz + 300, theme.accent, true);
            xml.push_str("</a:p>");
        }
    }
    for p in paragraphs {
        xml.push_str(
            r#"<a:p><a:pPr algn="l"><a:lnSpc><a:spcPct val="120000"/></a:lnSpc><a:spcAft><a:spcPts val="1000"/></a:spcAft><a:buNone/></a:pPr>"#,
        );
        pptx_push_inline_runs(&mut xml, p, font_sz, theme.body_fg, false);
        xml.push_str("</a:p>");
    }
    for b in bullets {
        let _ = write!(
            xml,
            r#"<a:p><a:pPr marL="342900" indent="-228600" algn="l"><a:lnSpc><a:spcPct val="120000"/></a:lnSpc><a:spcAft><a:spcPts val="800"/></a:spcAft><a:buClr><a:srgbClr val="{}"/></a:buClr><a:buFont typeface="Arial"/><a:buChar char="•"/></a:pPr>"#,
            theme.accent
        );
        pptx_push_inline_runs(&mut xml, b, font_sz, theme.body_fg, false);
        xml.push_str("</a:p>");
    }
    xml
}

#[allow(clippy::too_many_arguments)]
fn pptx_push_table(
    out: &mut String,
    shape_id: &mut u32,
    tbl: &TableData,
    x: i64,
    y: i64,
    cx: i64,
    cy: i64,
    theme: SlideTheme,
) {
    let id = *shape_id;
    *shape_id += 1;
    let col_count = tbl
        .headers
        .as_ref()
        .map(Vec::len)
        .unwrap_or_else(|| tbl.rows.first().map(Vec::len).unwrap_or(1))
        .max(1);
    let total_rows = tbl.rows.len() + usize::from(tbl.headers.is_some());
    let col_w = (cx / (col_count as i64)).max(100_000);
    let row_h = (cy / (total_rows.max(1) as i64)).clamp(280_000, 580_000);
    let frame_h = row_h.saturating_mul(total_rows.max(1) as i64);
    let cell_font_sz = if col_count >= 5 || total_rows >= 8 {
        1200
    } else {
        1400
    };

    let _ = write!(
        out,
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="{id}" name="Table {id}"/><p:cNvGraphicFramePr><a:graphicFrameLocks noGrp="1"/></p:cNvGraphicFramePr><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{frame_h}"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblPr firstRow="1" bandRow="1"><a:tableStyleId>{{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}}</a:tableStyleId></a:tblPr><a:tblGrid>"#
    );
    for _ in 0..col_count {
        let _ = write!(out, r#"<a:gridCol w="{col_w}"/>"#);
    }
    out.push_str("</a:tblGrid>");

    let border_xml = format!(
        r#"<a:lnL w="12700"><a:solidFill><a:srgbClr val="{b}"/></a:solidFill></a:lnL><a:lnR w="12700"><a:solidFill><a:srgbClr val="{b}"/></a:solidFill></a:lnR><a:lnT w="12700"><a:solidFill><a:srgbClr val="{b}"/></a:solidFill></a:lnT><a:lnB w="12700"><a:solidFill><a:srgbClr val="{b}"/></a:solidFill></a:lnB>"#,
        b = theme.table_border
    );

    if let Some(headers) = &tbl.headers {
        let _ = write!(out, r#"<a:tr h="{row_h}">"#);
        for cell in headers {
            out.push_str(r#"<a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(out, cell, cell_font_sz, theme.table_hdr_fg, true);
            let _ = write!(
                out,
                r#"</a:p></a:txBody><a:tcPr marL="91440" marR="91440" marT="45720" marB="45720" anchor="ctr">{border_xml}<a:solidFill><a:srgbClr val="{}"/></a:solidFill></a:tcPr></a:tc>"#,
                theme.table_hdr_bg
            );
        }
        out.push_str("</a:tr>");
    }

    for (r_idx, row) in tbl.rows.iter().enumerate() {
        let _ = write!(out, r#"<a:tr h="{row_h}">"#);
        let fill = if r_idx % 2 == 1 {
            theme.table_alt_bg
        } else {
            theme.bg
        };
        for cell in row {
            out.push_str(r#"<a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(out, cell, cell_font_sz, theme.body_fg, false);
            let _ = write!(
                out,
                r#"</a:p></a:txBody><a:tcPr marL="91440" marR="91440" marT="45720" marB="45720" anchor="ctr">{border_xml}<a:solidFill><a:srgbClr val="{fill}"/></a:solidFill></a:tcPr></a:tc>"#
            );
        }
        out.push_str("</a:tr>");
    }

    out.push_str("</a:tbl></a:graphicData></a:graphic></p:graphicFrame>");
}

fn pptx_render_slide_xml(
    slide: &SlideSpec,
    slide_idx: usize,
    total_slides: usize,
    theme: SlideTheme,
) -> String {
    let is_cover = slide.layout == "title" || slide.layout == "section";
    let bg_hex = if is_cover { theme.cover_bg } else { theme.bg };
    let mut xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld>
<p:bg><p:bgPr><a:solidFill><a:srgbClr val="{bg_hex}"/></a:solidFill><a:effectLst/></p:bgPr></p:bg>
<p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
"#
    );

    let mut shape_id = 2u32;

    if is_cover {
        // Left vertical accent stripe
        pptx_push_rect(
            &mut xml,
            &mut shape_id,
            "Accent Stripe",
            0,
            0,
            240_000,
            6_858_000,
            theme.accent,
        );
        // Subtle horizontal divider above subtitle
        pptx_push_rect(
            &mut xml,
            &mut shape_id,
            "Cover Divider",
            914_400,
            3_650_000,
            2_200_000,
            55_000,
            theme.accent_soft,
        );

        let mut title_p = String::from(r#"<a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
        pptx_push_inline_runs(&mut title_p, &slide.title, 4000, theme.cover_title_fg, true);
        title_p.push_str("</a:p>");
        pptx_push_textbox(
            &mut xml,
            &mut shape_id,
            "Title",
            914_400,
            1_750_000,
            10_363_200,
            1_750_000,
            "b",
            &title_p,
        );

        if let Some(sub) = &slide.subtitle {
            let mut sub_p = String::from(r#"<a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(&mut sub_p, sub, 2000, theme.cover_sub_fg, false);
            sub_p.push_str("</a:p>");
            pptx_push_textbox(
                &mut xml,
                &mut shape_id,
                "Subtitle",
                914_400,
                3_900_000,
                10_363_200,
                1_300_000,
                "t",
                &sub_p,
            );
        }
    } else {
        // Top accent ribbon
        pptx_push_rect(
            &mut xml,
            &mut shape_id,
            "Top Bar",
            0,
            0,
            12_192_000,
            110_000,
            theme.accent,
        );

        // Slide title
        let mut title_p = String::from(r#"<a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
        pptx_push_inline_runs(&mut title_p, &slide.title, 2800, theme.title_fg, true);
        title_p.push_str("</a:p>");
        pptx_push_textbox(
            &mut xml,
            &mut shape_id,
            "Title",
            685_800,
            320_000,
            10_820_400,
            680_000,
            "ctr",
            &title_p,
        );

        let mut content_y = 1_120_000i64;
        if let Some(sub) = &slide.subtitle {
            let mut sub_p = String::from(r#"<a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(&mut sub_p, sub, 1500, theme.muted_fg, false);
            sub_p.push_str("</a:p>");
            pptx_push_textbox(
                &mut xml,
                &mut shape_id,
                "Subtitle",
                685_800,
                1_020_000,
                10_820_400,
                420_000,
                "t",
                &sub_p,
            );
            content_y = 1_520_000;
        }
        let content_h = 6_250_000 - content_y;

        if slide.left.is_some() || slide.right.is_some() {
            // Explicit two-column slide
            if let Some(left) = &slide.left {
                let left_xml = pptx_render_content_paragraphs(
                    left.title.as_deref(),
                    &left.paragraphs,
                    &left.bullets,
                    theme,
                    1600,
                );
                pptx_push_textbox(
                    &mut xml,
                    &mut shape_id,
                    "Left Column",
                    685_800,
                    content_y,
                    5_150_000,
                    content_h,
                    "t",
                    &left_xml,
                );
            }
            if let Some(right) = &slide.right {
                let right_xml = pptx_render_content_paragraphs(
                    right.title.as_deref(),
                    &right.paragraphs,
                    &right.bullets,
                    theme,
                    1600,
                );
                pptx_push_textbox(
                    &mut xml,
                    &mut shape_id,
                    "Right Column",
                    6_356_200,
                    content_y,
                    5_150_000,
                    content_h,
                    "t",
                    &right_xml,
                );
            }
        } else {
            let has_text = !slide.bullets.is_empty() || !slide.paragraphs.is_empty();
            match (&slide.table, has_text) {
                (Some(tbl), true) => {
                    // Side-by-side: text on left, table on right
                    let text_xml = pptx_render_content_paragraphs(
                        None,
                        &slide.paragraphs,
                        &slide.bullets,
                        theme,
                        1600,
                    );
                    pptx_push_textbox(
                        &mut xml,
                        &mut shape_id,
                        "Content",
                        685_800,
                        content_y,
                        4_700_000,
                        content_h,
                        "t",
                        &text_xml,
                    );
                    pptx_push_table(
                        &mut xml,
                        &mut shape_id,
                        tbl,
                        5_650_000,
                        content_y,
                        5_856_200,
                        content_h.min(4_500_000),
                        theme,
                    );
                }
                (Some(tbl), false) => {
                    pptx_push_table(
                        &mut xml,
                        &mut shape_id,
                        tbl,
                        685_800,
                        content_y,
                        10_820_400,
                        content_h.min(4_700_000),
                        theme,
                    );
                }
                (None, true) => {
                    let font_sz = if slide.bullets.len() + slide.paragraphs.len() > 7 {
                        1600
                    } else {
                        1800
                    };
                    let text_xml = pptx_render_content_paragraphs(
                        None,
                        &slide.paragraphs,
                        &slide.bullets,
                        theme,
                        font_sz,
                    );
                    pptx_push_textbox(
                        &mut xml,
                        &mut shape_id,
                        "Content",
                        685_800,
                        content_y,
                        10_820_400,
                        content_h,
                        "t",
                        &text_xml,
                    );
                }
                (None, false) => {}
            }
        }

        // Subtle slide number footer
        let mut num_p = String::from(r#"<a:p><a:pPr algn="r"><a:buNone/></a:pPr>"#);
        let label = format!("{slide_idx} / {total_slides}");
        pptx_push_inline_runs(&mut num_p, &label, 1100, theme.muted_fg, false);
        num_p.push_str("</a:p>");
        pptx_push_textbox(
            &mut xml,
            &mut shape_id,
            "Slide Number",
            10_200_000,
            6_380_000,
            1_306_200,
            320_000,
            "ctr",
            &num_p,
        );
    }

    xml.push_str("</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>\n");
    xml
}

fn pptx_notes_slide_xml(notes: &str) -> String {
    let mut paras = String::new();
    for line in notes.lines() {
        if line.is_empty() {
            paras.push_str(r#"<a:p><a:endParaRPr lang="ru-RU" dirty="0"/></a:p>"#);
        } else {
            let escaped = xml_escape(line);
            let _ = write!(
                paras,
                r#"<a:p><a:r><a:rPr lang="ru-RU" dirty="0"/><a:t>{escaped}</a:t></a:r><a:endParaRPr lang="ru-RU" dirty="0"/></a:p>"#
            );
        }
    }
    if paras.is_empty() {
        paras.push_str(r#"<a:p><a:endParaRPr lang="ru-RU" dirty="0"/></a:p>"#);
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:notes xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Slide Image Placeholder 1"/><p:cNvSpPr><a:spLocks noGrp="1" noRot="1" noChangeAspect="1"/></p:cNvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Notes Placeholder 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
  <p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:notes>
"#
    )
}

fn pptx_theme_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Hermes">
  <a:themeElements>
    <a:clrScheme name="Hermes">
      <a:dk1><a:srgbClr val="1F2933"/></a:dk1>
      <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
      <a:dk2><a:srgbClr val="1F4E79"/></a:dk2>
      <a:lt2><a:srgbClr val="E8EEF5"/></a:lt2>
      <a:accent1><a:srgbClr val="1F4E79"/></a:accent1>
      <a:accent2><a:srgbClr val="2E75B6"/></a:accent2>
      <a:accent3><a:srgbClr val="059669"/></a:accent3>
      <a:accent4><a:srgbClr val="D97706"/></a:accent4>
      <a:accent5><a:srgbClr val="7C3AED"/></a:accent5>
      <a:accent6><a:srgbClr val="DC2626"/></a:accent6>
      <a:hlink><a:srgbClr val="1D4ED8"/></a:hlink>
      <a:folHlink><a:srgbClr val="6D28D9"/></a:folHlink>
    </a:clrScheme>
    <a:fontScheme name="Hermes">
      <a:majorFont><a:latin typeface="Calibri"/><a:ea typeface="Calibri"/><a:cs typeface="Calibri"/></a:majorFont>
      <a:minorFont><a:latin typeface="Calibri"/><a:ea typeface="Calibri"/><a:cs typeface="Calibri"/></a:minorFont>
    </a:fontScheme>
    <a:fmtScheme name="Hermes">
      <a:fillStyleLst>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      </a:fillStyleLst>
      <a:lnStyleLst>
        <a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
        <a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
        <a:ln w="28575"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
      </a:lnStyleLst>
      <a:effectStyleLst>
        <a:effectStyle><a:effectLst/></a:effectStyle>
        <a:effectStyle><a:effectLst/></a:effectStyle>
        <a:effectStyle><a:effectLst/></a:effectStyle>
      </a:effectStyleLst>
      <a:bgFillStyleLst>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      </a:bgFillStyleLst>
    </a:fmtScheme>
  </a:themeElements>
  <a:objectDefaults/>
  <a:extraClrSchemeLst/>
</a:theme>
"#
    .to_owned()
}

/// Create a formatted 16:9 `.pptx` PowerPoint presentation inside the workspace.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.pptx` path inside the workspace
/// - `slides`: list of slide objects (`title`, `subtitle`, `layout`, `bullets`, `body`,
///   `paragraphs`, `table`, `left`, `right`, `notes`) or Markdown strings
/// - `title`: optional presentation metadata title
/// - `author`: optional author metadata (defaults to `"Hermes"`)
/// - `theme`: optional `"corporate"` (default), `"dark"`, or `"minimal"`
pub(crate) fn pptx_write(workspace_root: &Path, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "pptx_write requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, path_str, false) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };
    let Some(slides_arr) = args.get("slides").and_then(Value::as_array) else {
        return json!({"error": "pptx_write requires a 'slides' list"});
    };
    if slides_arr.is_empty() {
        return json!({"error": "'slides' list must contain at least one slide"});
    }

    let mut slides = Vec::with_capacity(slides_arr.len());
    for (idx, item) in slides_arr.iter().enumerate() {
        match parse_slide_spec(idx, item) {
            Ok(s) => slides.push(s),
            Err(err) => return json!({"error": err}),
        }
    }

    let theme = resolve_slide_theme(args["theme"].as_str());
    let author = args["author"].as_str().unwrap_or("Hermes");
    let pres_title = args["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            slides
                .first()
                .map(|s| s.title.as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("Presentation");
    let has_any_notes = slides.iter().any(|s| s.notes.is_some());

    let mut content_types = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
  <Override PartName="/ppt/presProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"/>
  <Override PartName="/ppt/viewProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml"/>
  <Override PartName="/ppt/tableStyles.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"/>
  <Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
  <Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>
"#,
    );
    if has_any_notes {
        content_types.push_str(
            r#"  <Override PartName="/ppt/notesMasters/notesMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"/>
"#,
        );
    }

    let mut pres_rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps" Target="presProps.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/viewProps" Target="viewProps.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles" Target="tableStyles.xml"/>
"#,
    );
    let mut next_pres_rid = 5usize;
    let notes_master_rid = if has_any_notes {
        let rid = next_pres_rid;
        next_pres_rid += 1;
        let _ = writeln!(
            pres_rels,
            r#"  <Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster" Target="notesMasters/notesMaster1.xml"/>"#
        );
        Some(rid)
    } else {
        None
    };

    let mut sld_id_lst = String::new();
    let mut owned_entries: Vec<(String, String)> = Vec::new();
    let mut notes_written = 0usize;

    for (idx, slide) in slides.iter().enumerate() {
        let k = idx + 1;
        let rid = next_pres_rid;
        next_pres_rid += 1;
        let slide_id = 256 + idx;

        let _ = writeln!(
            content_types,
            r#"  <Override PartName="/ppt/slides/slide{k}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        );
        let _ = writeln!(
            pres_rels,
            r#"  <Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{k}.xml"/>"#
        );
        let _ = writeln!(
            sld_id_lst,
            r#"    <p:sldId id="{slide_id}" r:id="rId{rid}"/>"#
        );

        let slide_xml = pptx_render_slide_xml(slide, k, slides.len(), theme);
        owned_entries.push((format!("ppt/slides/slide{k}.xml"), slide_xml));

        let mut slide_rels = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
"#,
        );
        if let Some(notes) = &slide.notes {
            notes_written += 1;
            let _ = writeln!(
                content_types,
                r#"  <Override PartName="/ppt/notesSlides/notesSlide{k}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/>"#
            );
            let _ = writeln!(
                slide_rels,
                r#"  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide{k}.xml"/>"#
            );
            owned_entries.push((
                format!("ppt/notesSlides/notesSlide{k}.xml"),
                pptx_notes_slide_xml(notes),
            ));
            let notes_rels = format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster" Target="../notesMasters/notesMaster1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="../slides/slide{k}.xml"/>
</Relationships>
"#
            );
            owned_entries.push((
                format!("ppt/notesSlides/_rels/notesSlide{k}.xml.rels"),
                notes_rels,
            ));
        }
        slide_rels.push_str("</Relationships>\n");
        owned_entries.push((format!("ppt/slides/_rels/slide{k}.xml.rels"), slide_rels));
    }

    content_types.push_str("</Types>\n");
    pres_rels.push_str("</Relationships>\n");

    let notes_master_lst = notes_master_rid
        .map(|rid| {
            format!(
                r#"  <p:notesMasterIdLst><p:notesMasterId r:id="rId{rid}"/></p:notesMasterIdLst>"#
            )
        })
        .unwrap_or_default();

    let presentation_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" saveSubsetFonts="1">
  <p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
{notes_master_lst}
  <p:sldIdLst>
{sld_id_lst}  </p:sldIdLst>
  <p:sldSz cx="12192000" cy="6858000"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>
"#
    );

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
</Relationships>
"#
    .to_owned();

    let slide_master_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
    </p:spTree>
  </p:cSld>
  <p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
  <p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
  <p:txStyles>
    <p:titleStyle><a:lvl1pPr algn="l"><a:defRPr sz="4000" b="1"/></a:lvl1pPr></p:titleStyle>
    <p:bodyStyle><a:lvl1pPr algn="l"><a:defRPr sz="1800"/></a:lvl1pPr></p:bodyStyle>
    <p:otherStyle><a:defPPr><a:defRPr lang="ru-RU"/></a:defPPr></p:otherStyle>
  </p:txStyles>
</p:sldMaster>
"#
    .to_owned();

    let slide_master_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>
"#
    .to_owned();

    let slide_layout_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1">
  <p:cSld name="Blank">
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
    </p:spTree>
  </p:cSld>
  <p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:sldLayout>
"#
    .to_owned();

    let slide_layout_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>
"#
    .to_owned();

    let pres_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentationPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>
"#
    .to_owned();

    let view_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:viewPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:normalViewPr><p:restoredLeft sz="15620"/><p:restoredTop sz="94660"/></p:normalViewPr>
  <p:slideViewPr>
    <p:cSldViewPr>
      <p:cViewPr varScale="1">
        <p:scale><a:sx n="100" d="100"/><a:sy n="100" d="100"/></p:scale>
        <p:origin x="0" y="0"/>
      </p:cViewPr>
      <p:guideLst/>
    </p:cSldViewPr>
  </p:slideViewPr>
  <p:gridSpacing cx="72008" cy="72008"/>
</p:viewPr>
"#
    .to_owned();

    let table_styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:tblStyleLst xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" def="{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}"/>
"#
    .to_owned();

    let app_props = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties">
  <Slides>{}</Slides>
  <Notes>{}</Notes>
  <Application>Hermes</Application>
</Properties>
"#,
        slides.len(),
        notes_written
    );

    let mut all_entries: Vec<(String, String)> = vec![
        ("[Content_Types].xml".into(), content_types),
        ("_rels/.rels".into(), root_rels),
        ("ppt/presentation.xml".into(), presentation_xml),
        ("ppt/_rels/presentation.xml.rels".into(), pres_rels),
        ("ppt/slideMasters/slideMaster1.xml".into(), slide_master_xml),
        (
            "ppt/slideMasters/_rels/slideMaster1.xml.rels".into(),
            slide_master_rels,
        ),
        ("ppt/slideLayouts/slideLayout1.xml".into(), slide_layout_xml),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
            slide_layout_rels,
        ),
        ("ppt/theme/theme1.xml".into(), pptx_theme_xml()),
        ("ppt/presProps.xml".into(), pres_props),
        ("ppt/viewProps.xml".into(), view_props),
        ("ppt/tableStyles.xml".into(), table_styles),
        (
            "docProps/core.xml".into(),
            core_props_xml(pres_title, author),
        ),
        ("docProps/app.xml".into(), app_props),
    ];

    if has_any_notes {
        let notes_master_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:notesMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Slide Image Placeholder 1"/><p:cNvSpPr><a:spLocks noGrp="1" noRot="1" noChangeAspect="1"/></p:cNvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="1143000"/><a:ext cx="5486400" cy="3086100"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/><a:ln w="12700"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill></a:ln></p:spPr>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Notes Placeholder 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="4572000"/><a:ext cx="5486400" cy="3886200"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
        <p:txBody><a:bodyPr wrap="square" lIns="91440" tIns="45720" rIns="91440" bIns="45720"/><a:lstStyle/><a:p><a:endParaRPr lang="ru-RU"/></a:p></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
  <p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
  <p:notesStyle>
    <a:lvl1pPr marL="0" algn="l"><a:defRPr sz="1200" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl1pPr>
  </p:notesStyle>
</p:notesMaster>
"#
        .to_owned();
        let notes_master_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>
"#
        .to_owned();
        all_entries.push(("ppt/notesMasters/notesMaster1.xml".into(), notes_master_xml));
        all_entries.push((
            "ppt/notesMasters/_rels/notesMaster1.xml.rels".into(),
            notes_master_rels,
        ));
    }

    all_entries.extend(owned_entries);

    if let Err(err) = write_zip_entries(&resolved, &all_entries) {
        return json!({"error": err});
    }

    json!({
        "status": "ok",
        "path": path_str,
        "slides_written": slides.len(),
        "notes_written": notes_written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc_extract::extract_document_bytes;

    fn assert_all_xml_parts_well_formed(path: &Path, kind: &str) {
        let data = fs::read(path).expect("read archive");
        let zip = ZipArchive::from_bytes(&data, kind).expect("valid zip");
        for name in zip.entry_names() {
            if name.ends_with(".xml") || name.ends_with(".rels") {
                let bytes = zip.read_entry(name).expect("read zip entry");
                parse_xml(&bytes, name)
                    .unwrap_or_else(|e| panic!("invalid XML in {kind} part '{name}': {e}"));
            }
        }
    }

    #[test]
    fn roundtrips_docx_markdown_and_blocks_and_enforces_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let md = "# Финансовый отчёт\n\nФайл: **C:\\Temp\\hsr\\financial_stats_example.xlsx** и формула `=SUM(B2:B10)`.\n\n- Пункт *первый*\n- Пункт **второй**\n\n1. Шаг 1\n2. Шаг 2\n\n| Месяц | Выручка |\n|---|---|\n| Январь | 100 |\n| Февраль | 150 |\n\n> Важное примечание\n\n```python\nx = 1 + 2\n```\n";

        let write_res = docx_write(
            dir.path(),
            &json!({
                "path": "reports/summary.docx",
                "title": "Итоги года",
                "subtitle": "Автоматический отчёт",
                "content": md
            }),
        );
        assert_eq!(write_res["status"], "ok", "{write_res}");
        assert_eq!(write_res["tables_written"], 1);

        let full_path = dir.path().join("reports/summary.docx");
        assert_all_xml_parts_well_formed(&full_path, "DOCX");

        let read_res = docx_read(dir.path(), &json!({"path": "reports/summary.docx"}));
        assert!(read_res.get("error").is_none(), "{read_res}");
        let paras = read_res["paragraphs"].as_array().unwrap();
        assert!(paras.iter().any(|p| p == "Итоги года"), "{paras:?}");
        assert!(
            paras.iter().any(|p| p
                .as_str()
                .is_some_and(|s| s.contains(r"C:\Temp\hsr\financial_stats_example.xlsx"))),
            "{paras:?}"
        );
        assert_eq!(read_res["table_count"], 1);
        assert_eq!(
            read_res["tables"][0],
            json!([["Месяц", "Выручка"], ["Январь", "100"], ["Февраль", "150"]])
        );

        // Also readable via read_file's doc_extract
        let extracted = extract_document_bytes(
            &fs::read(&full_path).unwrap(),
            &full_path,
            "reports/summary.docx",
        )
        .unwrap();
        assert!(extracted.contains("Финансовый отчёт"));
        assert!(extracted.contains("Февраль"));

        // Structured blocks API
        let blocks_res = docx_write(
            dir.path(),
            &json!({
                "path": "reports/structured.docx",
                "blocks": [
                    {"type": "heading", "level": 1, "text": "Overview"},
                    {"type": "paragraph", "runs": [{"text": "Bold ", "bold": true}, {"text": "Blue", "color": "1F4E79"}]},
                    {"type": "bullets", "items": ["Alpha", "Beta"]},
                    {"type": "numbered", "items": ["First", "Second"]},
                    {"type": "page_break"},
                    {"type": "table", "headers": ["K", "V"], "rows": [["A", 1]]}
                ]
            }),
        );
        assert_eq!(blocks_res["status"], "ok", "{blocks_res}");
        assert_all_xml_parts_well_formed(&dir.path().join("reports/structured.docx"), "DOCX");

        // Workspace escape refused
        let escaped = docx_write(
            dir.path(),
            &json!({"path": "../outside.docx", "content": "nope"}),
        );
        assert!(escaped["error"].as_str().is_some());
    }

    #[test]
    fn roundtrips_pptx_slides_tables_columns_and_notes_and_enforces_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let write_res = pptx_write(
            dir.path(),
            &json!({
                "path": "decks/q3_review.pptx",
                "title": "Q3 Business Review",
                "theme": "corporate",
                "slides": [
                    {
                        "title": "Квартальный отчёт Q3",
                        "subtitle": "Финансовые итоги и планы"
                    },
                    {
                        "title": "Ключевые метрики",
                        "subtitle": "Рост по всем направлениям",
                        "bullets": [
                            "**Выручка**: +24% г/г",
                            "Файл: `C:\\Temp\\hsr\\financial_stats_example.xlsx`"
                        ],
                        "table": {
                            "headers": ["Месяц", "План", "Факт"],
                            "rows": [
                                ["Июль", 100, 112],
                                ["Август", 110, 128]
                            ]
                        },
                        "notes": "Подчеркнуть перевыполнение плана в августе."
                    },
                    {
                        "title": "Сравнение вариантов",
                        "left": {
                            "title": "Вариант A",
                            "bullets": ["Быстрый запуск", "Низкий бюджет"]
                        },
                        "right": {
                            "title": "Вариант B",
                            "bullets": ["Полная автоматизация", "Масштабирование"]
                        }
                    }
                ]
            }),
        );
        assert_eq!(write_res["status"], "ok", "{write_res}");
        assert_eq!(write_res["slides_written"], 3);
        assert_eq!(write_res["notes_written"], 1);

        let full_path = dir.path().join("decks/q3_review.pptx");
        assert_all_xml_parts_well_formed(&full_path, "PPTX");

        // Verify PowerPoint-critical ECMA-376 invariants (valid viewProps, app.xml order,
        // notesMaster placeholders + notesStyle, no extraneous theme rel on presentation.xml).
        let raw_pptx = fs::read(&full_path).unwrap();
        let pptx_zip = ZipArchive::from_bytes(&raw_pptx, "PPTX").unwrap();
        let view_props_str =
            String::from_utf8(pptx_zip.read_entry("ppt/viewProps.xml").unwrap()).unwrap();
        assert!(view_props_str.contains("<p:cSldViewPr>"));
        let app_xml_str =
            String::from_utf8(pptx_zip.read_entry("docProps/app.xml").unwrap()).unwrap();
        assert!(
            app_xml_str.find("<Slides>").unwrap() < app_xml_str.find("<Application>").unwrap(),
            "app.xml must place <Slides> before <Application>:\n{app_xml_str}"
        );
        let pres_rels_str = String::from_utf8(
            pptx_zip
                .read_entry("ppt/_rels/presentation.xml.rels")
                .unwrap(),
        )
        .unwrap();
        assert!(!pres_rels_str.contains("relationships/theme"));
        let notes_master_str = String::from_utf8(
            pptx_zip
                .read_entry("ppt/notesMasters/notesMaster1.xml")
                .unwrap(),
        )
        .unwrap();
        assert!(notes_master_str.contains(r#"type="sldImg""#));
        assert!(notes_master_str.contains(r#"type="body" idx="1""#));
        assert!(notes_master_str.contains("<p:notesStyle>"));
        let notes_slide_str = String::from_utf8(
            pptx_zip
                .read_entry("ppt/notesSlides/notesSlide2.xml")
                .unwrap(),
        )
        .unwrap();
        assert!(notes_slide_str.contains(r#"type="sldImg""#));
        assert!(notes_slide_str.contains(r#"type="body" idx="1""#));

        let read_res = pptx_read(dir.path(), &json!({"path": "decks/q3_review.pptx"}));
        assert!(read_res.get("error").is_none(), "{read_res}");
        assert_eq!(read_res["slide_count"], 3);
        let slides = read_res["slides"].as_array().unwrap();
        assert_eq!(slides[0]["title"], "Квартальный отчёт Q3");
        assert_eq!(slides[1]["title"], "Ключевые метрики");
        assert_eq!(
            slides[1]["notes"],
            "Подчеркнуть перевыполнение плана в августе."
        );
        assert_eq!(
            slides[1]["tables"][0],
            json!([
                ["Месяц", "План", "Факт"],
                ["Июль", "100", "112"],
                ["Август", "110", "128"]
            ])
        );
        assert!(slides[2]["texts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "Вариант B"));

        // Also readable via read_file's doc_extract
        let extracted = extract_document_bytes(
            &fs::read(&full_path).unwrap(),
            &full_path,
            "decks/q3_review.pptx",
        )
        .unwrap();
        assert!(extracted.contains("# ── Slide 1 ──"));
        assert!(extracted.contains("Квартальный отчёт Q3"));
        assert!(extracted.contains("Сравнение вариантов"));

        // Workspace escape refused
        let escaped = pptx_write(
            dir.path(),
            &json!({"path": "../outside.pptx", "slides": [{"title": "x"}]}),
        );
        assert!(escaped["error"].as_str().is_some());
    }
}
