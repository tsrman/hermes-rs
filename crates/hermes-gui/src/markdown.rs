//! Lightweight Markdown block, inline, math, footnote, HTML-tag, and code-highlight parser for
//! rendering assistant replies in GPUI.

use std::collections::HashMap;
use std::ops::Range;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InlineStyle {
    Bold,
    Italic,
    BoldItalic,
    Code,
    Strikethrough,
    Underline,
    Link,
    Footnote,
    Math,
    CodeKeyword,
    CodeString,
    CodeComment,
    CodeNumber,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedInline {
    pub plain: String,
    pub spans: Vec<(Range<usize>, InlineStyle)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MdBlock {
    Heading {
        level: u8,
        text: String,
    },
    CodeBlock {
        lang: Option<String>,
        code: String,
    },
    MathBlock {
        raw: String,
    },
    Bullet {
        indent: u8,
        text: String,
    },
    TaskItem {
        indent: u8,
        checked: bool,
        text: String,
    },
    OrderedItem {
        indent: u8,
        number: String,
        text: String,
    },
    Definition {
        term: String,
        definition: String,
    },
    Quote {
        depth: u8,
        text: String,
    },
    FootnoteDef {
        id: String,
        text: String,
    },
    Rule,
    Table {
        headers: Vec<String>,
        alignments: Vec<TableAlign>,
        rows: Vec<Vec<String>>,
    },
    Paragraph {
        text: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDoc {
    pub blocks: Vec<MdBlock>,
    pub link_refs: HashMap<String, String>,
}

#[cfg(test)]
pub fn parse_blocks(input: &str) -> Vec<MdBlock> {
    parse_document(input).blocks
}

pub fn parse_document(input: &str) -> ParsedDoc {
    let lines: Vec<&str> = input.lines().collect();

    // Pass 1: collect reference-style link definitions `[id]: https://... "title"` outside fences.
    let mut link_refs: HashMap<String, String> = HashMap::new();
    let mut scan_in_code = false;
    for &raw_line in &lines {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") {
            scan_in_code = !scan_in_code;
            continue;
        }
        if !scan_in_code {
            if let Some((id, url)) = parse_ref_link_def(trimmed) {
                link_refs.entry(id.to_ascii_lowercase()).or_insert(url);
            }
        }
    }

    let mut blocks = Vec::new();
    let mut para_lines: Vec<&str> = Vec::new();
    let mut in_code = false;
    let mut code_lang: Option<String> = None;
    let mut code_lines: Vec<&str> = Vec::new();
    let mut in_math_delim: Option<&'static str> = None;
    let mut math_lines: Vec<&str> = Vec::new();

    let flush_para = |para_lines: &mut Vec<&str>, blocks: &mut Vec<MdBlock>| {
        if !para_lines.is_empty() {
            blocks.push(MdBlock::Paragraph {
                text: para_lines.join("\n"),
            });
            para_lines.clear();
        }
    };

    let mut i = 0;
    while i < lines.len() {
        let raw_line = lines[i];
        let leading_spaces = raw_line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { 2 } else { 1 })
            .sum::<usize>();
        let indent = (leading_spaces / 2).min(4) as u8;
        let trimmed = raw_line.trim_start();

        if in_code {
            if trimmed.starts_with("```") {
                let lang = code_lang.take();
                let code = code_lines.join("\n");
                if lang
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case("math"))
                {
                    blocks.push(MdBlock::MathBlock { raw: code });
                } else {
                    blocks.push(MdBlock::CodeBlock { lang, code });
                }
                code_lines.clear();
                in_code = false;
            } else {
                code_lines.push(raw_line);
            }
            i += 1;
            continue;
        }

        if let Some(close_delim) = in_math_delim {
            if trimmed == close_delim || trimmed.starts_with(close_delim) {
                blocks.push(MdBlock::MathBlock {
                    raw: math_lines.join("\n").trim().to_owned(),
                });
                math_lines.clear();
                in_math_delim = None;
            } else if let Some(before) = trimmed.strip_suffix(close_delim) {
                if !before.trim().is_empty() {
                    math_lines.push(before);
                }
                blocks.push(MdBlock::MathBlock {
                    raw: math_lines.join("\n").trim().to_owned(),
                });
                math_lines.clear();
                in_math_delim = None;
            } else {
                math_lines.push(raw_line);
            }
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("```") {
            flush_para(&mut para_lines, &mut blocks);
            let lang = rest.trim();
            code_lang = (!lang.is_empty()).then(|| lang.to_owned());
            in_code = true;
            i += 1;
            continue;
        }

        // Display math block: `$$...$$` or `\[...\]` on one line or multi-line.
        if let Some(after) = trimmed.strip_prefix("$$") {
            let after_trim = after.trim();
            if after_trim.is_empty() {
                flush_para(&mut para_lines, &mut blocks);
                in_math_delim = Some("$$");
                i += 1;
                continue;
            } else if let Some(inner) = after_trim.strip_suffix("$$") {
                if !inner.trim().is_empty() {
                    flush_para(&mut para_lines, &mut blocks);
                    blocks.push(MdBlock::MathBlock {
                        raw: inner.trim().to_owned(),
                    });
                    i += 1;
                    continue;
                }
            } else {
                flush_para(&mut para_lines, &mut blocks);
                math_lines.push(after_trim);
                in_math_delim = Some("$$");
                i += 1;
                continue;
            }
        }
        if let Some(after) = trimmed.strip_prefix(r"\[") {
            let after_trim = after.trim();
            if after_trim.is_empty() {
                flush_para(&mut para_lines, &mut blocks);
                in_math_delim = Some(r"\]");
                i += 1;
                continue;
            } else if let Some(inner) = after_trim.strip_suffix(r"\]") {
                if !inner.trim().is_empty() {
                    flush_para(&mut para_lines, &mut blocks);
                    blocks.push(MdBlock::MathBlock {
                        raw: inner.trim().to_owned(),
                    });
                    i += 1;
                    continue;
                }
            } else {
                flush_para(&mut para_lines, &mut blocks);
                math_lines.push(after_trim);
                in_math_delim = Some(r"\]");
                i += 1;
                continue;
            }
        }

        if trimmed.is_empty() {
            flush_para(&mut para_lines, &mut blocks);
            i += 1;
            continue;
        }

        // Reference link definitions are consumed silently into `link_refs`.
        if parse_ref_link_def(trimmed).is_some() {
            flush_para(&mut para_lines, &mut blocks);
            i += 1;
            continue;
        }

        // Footnote definition: `[^1]: Содержание первой сноски.`
        if let Some((id, body)) = parse_footnote_def(trimmed) {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::FootnoteDef {
                id: id.to_owned(),
                text: body.to_owned(),
            });
            i += 1;
            continue;
        }

        // Definition list entry: `: Определение` following a term line.
        if let Some(def_text) = parse_definition_line(trimmed) {
            let term = if para_lines.len() == 1 {
                para_lines.pop().unwrap().trim().to_owned()
            } else if let Some(last) = para_lines.pop() {
                flush_para(&mut para_lines, &mut blocks);
                last.trim().to_owned()
            } else {
                String::new()
            };
            blocks.push(MdBlock::Definition {
                term,
                definition: def_text.to_owned(),
            });
            i += 1;
            continue;
        }

        // GFM pipe table: header row followed immediately by alignment separator row.
        if let Some((headers, alignments)) = lines
            .get(i + 1)
            .and_then(|sep| parse_table_header_and_sep(raw_line, sep))
        {
            flush_para(&mut para_lines, &mut blocks);
            let col_count = headers.len();
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() {
                let row_line = lines[i];
                let row_trimmed = row_line.trim();
                if row_trimmed.is_empty()
                    || row_trimmed.starts_with("```")
                    || parse_heading(row_trimmed).is_some()
                    || parse_quote_line(row_trimmed).is_some()
                    || !row_trimmed.contains('|')
                {
                    break;
                }
                let mut cells = split_table_row(row_line);
                if cells.is_empty() {
                    break;
                }
                cells.resize(col_count, String::new());
                rows.push(cells);
                i += 1;
            }
            blocks.push(MdBlock::Table {
                headers,
                alignments,
                rows,
            });
            continue;
        }

        if is_horizontal_rule(trimmed) {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::Rule);
            i += 1;
            continue;
        }

        if let Some((level, heading)) = parse_heading(trimmed) {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::Heading {
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
            flush_para(&mut para_lines, &mut blocks);
            if let Some((checked, task_text)) = parse_task_prefix(bullet) {
                blocks.push(MdBlock::TaskItem {
                    indent,
                    checked,
                    text: task_text.to_owned(),
                });
            } else {
                blocks.push(MdBlock::Bullet {
                    indent,
                    text: bullet.to_owned(),
                });
            }
            i += 1;
            continue;
        }

        if let Some((number, body)) = parse_ordered_item(trimmed) {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::OrderedItem {
                indent,
                number: number.to_owned(),
                text: body.to_owned(),
            });
            i += 1;
            continue;
        }

        if let Some((depth, quote)) = parse_quote_line(trimmed) {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::Quote {
                depth,
                text: quote.to_owned(),
            });
            i += 1;
            continue;
        }

        para_lines.push(raw_line);
        i += 1;
    }

    if in_code {
        let lang = code_lang.take();
        let code = code_lines.join("\n");
        if lang
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("math"))
        {
            blocks.push(MdBlock::MathBlock { raw: code });
        } else {
            blocks.push(MdBlock::CodeBlock { lang, code });
        }
    } else if in_math_delim.is_some() {
        blocks.push(MdBlock::MathBlock {
            raw: math_lines.join("\n").trim().to_owned(),
        });
    } else {
        flush_para(&mut para_lines, &mut blocks);
    }

    ParsedDoc { blocks, link_refs }
}

/// Parse inline Markdown (`**bold**`, `*italic*`, nested `**bold *italic***`, `` `code` ``,
/// `~~strike~~`, `[label](url)`, `[label][ref]`, `<https://...>`, `[^1]`, `$math$`, inline HTML
/// `<b>`/`<i>`/`<code>`/`<s>`/`<u>`/`<br>`) into clean display text plus sorted, non-overlapping
/// style spans. Preserves Windows paths (`C:\Temp\hsr\financial_stats_example.xlsx`) verbatim.
#[cfg(test)]
pub fn parse_inline(input: &str) -> RenderedInline {
    parse_inline_with_refs(input, &HashMap::new())
}

pub fn parse_inline_with_refs(input: &str, refs: &HashMap<String, String>) -> RenderedInline {
    let mut plain = String::with_capacity(input.len());
    let mut spans = Vec::new();
    parse_inline_into(input, refs, None, &mut plain, &mut spans);
    RenderedInline { plain, spans }
}

fn push_with_style(
    plain: &mut String,
    spans: &mut Vec<(Range<usize>, InlineStyle)>,
    text: &str,
    style: Option<InlineStyle>,
) {
    if text.is_empty() {
        return;
    }
    let start = plain.len();
    plain.push_str(text);
    let end = plain.len();
    if let Some(st) = style {
        if let Some((prev_r, prev_s)) = spans.last_mut() {
            if *prev_s == st && prev_r.end == start {
                prev_r.end = end;
                return;
            }
        }
        spans.push((start..end, st));
    }
}

fn combine_emphasis(base: Option<InlineStyle>, added: InlineStyle) -> InlineStyle {
    match (base, added) {
        (Some(InlineStyle::Bold), InlineStyle::Italic)
        | (Some(InlineStyle::Italic), InlineStyle::Bold)
        | (Some(InlineStyle::BoldItalic), InlineStyle::Bold | InlineStyle::Italic) => {
            InlineStyle::BoldItalic
        }
        _ => added,
    }
}

fn parse_inline_into(
    input: &str,
    refs: &HashMap<String, String>,
    outer: Option<InlineStyle>,
    plain: &mut String,
    spans: &mut Vec<(Range<usize>, InlineStyle)>,
) {
    let bytes = input.as_bytes();
    let len = input.len();
    let mut i = 0;

    while i < len {
        // 1. Escaped Markdown punctuation (never eats Windows path separators like `\T` or `\f`).
        if bytes[i] == b'\\' && i + 1 < len {
            let next = bytes[i + 1];
            if matches!(
                next,
                b'*' | b'`' | b'_' | b'~' | b'[' | b']' | b'#' | b'|' | b'$' | b'<' | b'>'
            ) {
                let mut buf = [0u8; 4];
                let s = (next as char).encode_utf8(&mut buf);
                push_with_style(plain, spans, s, outer);
                i += 2;
                continue;
            }
        }

        // 2. Inline code: `` `...` `` or ```` ``...`` ````
        if bytes[i] == b'`' {
            let tick_count = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let delim = &input[i..i + tick_count];
            if let Some(rel_end) = input[i + tick_count..].find(delim) {
                let raw_code = &input[i + tick_count..i + tick_count + rel_end];
                let code = if tick_count > 1 && raw_code.starts_with(' ') && raw_code.ends_with(' ')
                {
                    &raw_code[1..raw_code.len() - 1]
                } else {
                    raw_code
                };
                push_with_style(plain, spans, code, Some(InlineStyle::Code));
                i += tick_count + rel_end + tick_count;
                continue;
            }
        }

        // 3. Inline HTML (`<br>`, `<b>...</b>`, `<i>...</i>`, `<code>...</code>`, `<s>...</s>`,
        //    `<u>...</u>`, `<small>`, `<sup>`, `<sub>`) and autolinks (`<https://...>`).
        if bytes[i] == b'<' {
            if let Some((url, next_i)) = parse_autolink_at(input, i) {
                push_with_style(plain, spans, url, Some(InlineStyle::Link));
                i = next_i;
                continue;
            }
            if let Some(next_i) = parse_html_br_at(input, i) {
                push_with_style(plain, spans, "\n", outer);
                i = next_i;
                continue;
            }
            if let Some((inner, html_style, next_i)) = parse_html_tag_at(input, i) {
                let combined = html_style.map(|s| combine_emphasis(outer, s)).or(outer);
                if html_style == Some(InlineStyle::Code) {
                    push_with_style(plain, spans, inner, Some(InlineStyle::Code));
                } else {
                    parse_inline_into(inner, refs, combined, plain, spans);
                }
                i = next_i;
                continue;
            }
        }

        // 4. Footnote reference `[^1]` / `[^note]`
        if input[i..].starts_with("[^") {
            if let Some((id, next_i)) = parse_footnote_ref_at(input, i) {
                let badge = format!("[{id}]");
                push_with_style(plain, spans, &badge, Some(InlineStyle::Footnote));
                i = next_i;
                continue;
            }
        }

        // 5. Links `[label](url)`, images `![alt](url)`, and reference links `[label][ref]`
        if bytes[i] == b'[' || (bytes[i] == b'!' && i + 1 < len && bytes[i + 1] == b'[') {
            let bracket_start = if bytes[i] == b'!' { i + 1 } else { i };
            if let Some((label, next_i)) = parse_link_at(input, bracket_start) {
                push_with_style(plain, spans, label, Some(InlineStyle::Link));
                i = next_i;
                continue;
            }
            if let Some((label, next_i)) = parse_ref_link_at(input, bracket_start, refs) {
                push_with_style(plain, spans, label, Some(InlineStyle::Link));
                i = next_i;
                continue;
            }
        }

        // 6. Inline math `$...$` (not `$$`, not currency like `$100`) and `\(...\)`
        if bytes[i] == b'$' && !input[i..].starts_with("$$") {
            if let Some((expr, next_i)) = parse_inline_math_at(input, i) {
                let pretty = crate::math_render::prettify_latex_inline(expr);
                push_with_style(plain, spans, &pretty, Some(InlineStyle::Math));
                i = next_i;
                continue;
            }
        }
        if input[i..].starts_with(r"\(") {
            if let Some(rel_end) = input[i + 2..].find(r"\)") {
                let expr = &input[i + 2..i + 2 + rel_end];
                if !expr.trim().is_empty() && !expr.contains('\n') {
                    let pretty = crate::math_render::prettify_latex_inline(expr);
                    push_with_style(plain, spans, &pretty, Some(InlineStyle::Math));
                    i += 2 + rel_end + 2;
                    continue;
                }
            }
        }

        // 7. Strikethrough `~~...~~`
        if input[i..].starts_with("~~") {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "~~", false) {
                let inner = &input[i + 2..i + 2 + rel_end];
                parse_inline_into(inner, refs, Some(InlineStyle::Strikethrough), plain, spans);
                i += 2 + rel_end + 2;
                continue;
            }
        }

        // 8. Bold italic `***...***` or `___...___`
        if input[i..].starts_with("***") {
            if let Some(rel_end) = find_closing_delim(input, i + 3, "***", false) {
                let inner = &input[i + 3..i + 3 + rel_end];
                parse_inline_into(inner, refs, Some(InlineStyle::BoldItalic), plain, spans);
                i += 3 + rel_end + 3;
                continue;
            }
        }
        if input[i..].starts_with("___") && is_word_boundary_before(input, i) {
            if let Some(rel_end) = find_closing_delim(input, i + 3, "___", true) {
                let inner = &input[i + 3..i + 3 + rel_end];
                parse_inline_into(inner, refs, Some(InlineStyle::BoldItalic), plain, spans);
                i += 3 + rel_end + 3;
                continue;
            }
        }

        // 9. Bold `**...**` or `__...__` (supports nested `*italic*`, `_italic_`, `` `code` ``)
        if input[i..].starts_with("**") {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "**", false) {
                let inner = &input[i + 2..i + 2 + rel_end];
                let style = combine_emphasis(outer, InlineStyle::Bold);
                parse_inline_into(inner, refs, Some(style), plain, spans);
                i += 2 + rel_end + 2;
                continue;
            }
        }
        if input[i..].starts_with("__") && is_word_boundary_before(input, i) {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "__", true) {
                let inner = &input[i + 2..i + 2 + rel_end];
                let style = combine_emphasis(outer, InlineStyle::Bold);
                parse_inline_into(inner, refs, Some(style), plain, spans);
                i += 2 + rel_end + 2;
                continue;
            }
        }

        // 10. Italic `*...*` or `_..._` (supports nested `**bold**`, `` `code` ``)
        if bytes[i] == b'*' && (i + 1 >= len || bytes[i + 1] != b'*') {
            if let Some(rel_end) = find_closing_single(input, i + 1, b'*', false) {
                let inner = &input[i + 1..i + 1 + rel_end];
                let style = combine_emphasis(outer, InlineStyle::Italic);
                parse_inline_into(inner, refs, Some(style), plain, spans);
                i += 1 + rel_end + 1;
                continue;
            }
        }
        if bytes[i] == b'_'
            && is_word_boundary_before(input, i)
            && (i + 1 >= len || bytes[i + 1] != b'_')
        {
            if let Some(rel_end) = find_closing_single(input, i + 1, b'_', true) {
                let inner = &input[i + 1..i + 1 + rel_end];
                let style = combine_emphasis(outer, InlineStyle::Italic);
                parse_inline_into(inner, refs, Some(style), plain, spans);
                i += 1 + rel_end + 1;
                continue;
            }
        }

        // Plain character
        let ch = input[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        push_with_style(plain, spans, &input[i..i + ch_len], outer);
        i += ch_len;
    }
}

/// Lightweight syntax highlighter for fenced code blocks, emitting sorted non-overlapping spans.
pub fn highlight_code(lang: Option<&str>, code: &str) -> Vec<(Range<usize>, InlineStyle)> {
    let lang = lang.unwrap_or("").trim().to_ascii_lowercase();
    if lang.is_empty() || matches!(lang.as_str(), "text" | "txt" | "plain" | "plaintext") {
        return Vec::new();
    }

    let is_json = matches!(lang.as_str(), "json" | "jsonc");
    let is_hash_comment = matches!(
        lang.as_str(),
        "py" | "python"
            | "sh"
            | "bash"
            | "zsh"
            | "shell"
            | "yaml"
            | "yml"
            | "toml"
            | "rb"
            | "ruby"
            | "ps1"
            | "powershell"
    );
    let is_slash_comment = matches!(
        lang.as_str(),
        "rs" | "rust"
            | "js"
            | "javascript"
            | "ts"
            | "typescript"
            | "jsx"
            | "tsx"
            | "c"
            | "cpp"
            | "c++"
            | "h"
            | "hpp"
            | "java"
            | "go"
            | "cs"
            | "csharp"
            | "swift"
            | "kt"
            | "kotlin"
            | "jsonc"
    );
    let is_sql = lang == "sql";

    let keywords: &[&str] = match lang.as_str() {
        "py" | "python" => &[
            "def", "class", "return", "if", "elif", "else", "for", "while", "in", "not", "and",
            "or", "import", "from", "as", "with", "try", "except", "finally", "raise", "pass",
            "break", "continue", "yield", "lambda", "async", "await", "True", "False", "None",
        ],
        "rs" | "rust" => &[
            "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "for", "while", "loop",
            "if", "else", "match", "return", "use", "mod", "crate", "const", "static", "type",
            "where", "async", "await", "move", "ref", "in", "as", "true", "false", "Self", "self",
        ],
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" => &[
            "function",
            "const",
            "let",
            "var",
            "return",
            "if",
            "else",
            "for",
            "while",
            "switch",
            "case",
            "break",
            "continue",
            "import",
            "export",
            "from",
            "default",
            "class",
            "extends",
            "new",
            "try",
            "catch",
            "finally",
            "throw",
            "async",
            "await",
            "type",
            "interface",
            "true",
            "false",
            "null",
            "undefined",
        ],
        "json" | "jsonc" => &["true", "false", "null"],
        "sh" | "bash" | "zsh" | "shell" => &[
            "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "case", "esac",
            "function", "export", "local", "return", "set", "exit",
        ],
        "sql" => &[
            "SELECT", "FROM", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "ON", "GROUP", "BY",
            "ORDER", "LIMIT", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE",
            "TABLE", "AND", "OR", "NOT", "NULL", "AS", "select", "from", "where", "join", "and",
            "or", "not", "null", "as",
        ],
        _ => &[
            "if", "else", "for", "while", "return", "true", "false", "null", "fn", "def", "class",
            "import", "const", "let", "var",
        ],
    };

    let bytes = code.as_bytes();
    let len = code.len();
    let mut spans = Vec::new();
    let mut i = 0;

    while i < len {
        // Line comments
        if (is_hash_comment && bytes[i] == b'#')
            || (is_slash_comment && code[i..].starts_with("//"))
            || (is_sql && code[i..].starts_with("--"))
        {
            let end = code[i..].find('\n').map_or(len, |r| i + r);
            spans.push((i..end, InlineStyle::CodeComment));
            i = end;
            continue;
        }
        // Block comments `/* ... */`
        if is_slash_comment && code[i..].starts_with("/*") {
            let end = code[i + 2..].find("*/").map_or(len, |r| i + 2 + r + 2);
            spans.push((i..end, InlineStyle::CodeComment));
            i = end;
            continue;
        }
        // Triple-quoted Python strings
        if matches!(lang.as_str(), "py" | "python")
            && (code[i..].starts_with("\"\"\"") || code[i..].starts_with("'''"))
        {
            let delim = &code[i..i + 3];
            let end = code[i + 3..].find(delim).map_or(len, |r| i + 3 + r + 3);
            spans.push((i..end, InlineStyle::CodeString));
            i = end;
            continue;
        }
        // Double or single quoted strings
        if bytes[i] == b'"' || (!is_json && bytes[i] == b'\'') {
            let quote = bytes[i];
            let start = i;
            i += 1;
            while i < len && bytes[i] != b'\n' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            // In JSON, distinguish object keys (`"key":`) from string values.
            let style = if is_json && code[i..].trim_start().starts_with(':') {
                InlineStyle::CodeKeyword
            } else {
                InlineStyle::CodeString
            };
            spans.push((start..i, style));
            continue;
        }
        // Numbers
        if bytes[i].is_ascii_digit()
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_')
        {
            let start = i;
            i += 1;
            while i < len
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.' || bytes[i] == b'_')
            {
                i += 1;
            }
            spans.push((start..i, InlineStyle::CodeNumber));
            continue;
        }
        // Identifiers / keywords
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &code[start..i];
            if keywords.contains(&word) {
                spans.push((start..i, InlineStyle::CodeKeyword));
            }
            continue;
        }

        let ch = code[i..].chars().next().unwrap();
        i += ch.len_utf8();
    }
    spans
}

fn is_word_boundary_before(input: &str, byte_idx: usize) -> bool {
    if byte_idx == 0 {
        return true;
    }
    input[..byte_idx]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric())
}

fn is_word_boundary_after(input: &str, byte_idx: usize) -> bool {
    if byte_idx >= input.len() {
        return true;
    }
    input[byte_idx..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_alphanumeric())
}

fn find_closing_delim(
    input: &str,
    content_start: usize,
    delim: &str,
    require_word_after: bool,
) -> Option<usize> {
    let rest = input.get(content_start..)?;
    let first = rest.chars().next()?;
    if first.is_whitespace() {
        return None;
    }
    let mut search_from = 0;
    while let Some(found) = rest[search_from..].find(delim) {
        let rel = search_from + found;
        let content = &rest[..rel];
        let after_idx = content_start + rel + delim.len();
        let valid_end = !content.is_empty()
            && !content.chars().next_back().is_some_and(char::is_whitespace)
            && (!require_word_after || is_word_boundary_after(input, after_idx));
        if valid_end {
            return Some(rel);
        }
        search_from = rel + delim.len();
    }
    None
}

fn find_closing_single(
    input: &str,
    content_start: usize,
    delim: u8,
    require_word_after: bool,
) -> Option<usize> {
    let rest = input.get(content_start..)?;
    let first = rest.chars().next()?;
    if first.is_whitespace() || first == delim as char {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'\n' {
            return None;
        }
        if bytes[idx] == b'\\' && idx + 1 < bytes.len() {
            idx += 2;
            continue;
        }
        if bytes[idx] == b'`' {
            let ticks = bytes[idx..].iter().take_while(|&&b| b == b'`').count();
            let d = &rest[idx..idx + ticks];
            if let Some(end) = rest[idx + ticks..].find(d) {
                idx += ticks + end + ticks;
                continue;
            }
        }
        if bytes[idx] == delim {
            let doubled = (idx + 1 < bytes.len() && bytes[idx + 1] == delim)
                || (idx > 0 && bytes[idx - 1] == delim);
            if !doubled {
                let content = &rest[..idx];
                let after_idx = content_start + idx + 1;
                if !content.is_empty()
                    && !content.chars().next_back().is_some_and(char::is_whitespace)
                    && (!require_word_after || is_word_boundary_after(input, after_idx))
                {
                    return Some(idx);
                }
            }
        }
        idx += 1;
    }
    None
}

fn parse_autolink_at(input: &str, start: usize) -> Option<(&str, usize)> {
    let rest = input.get(start + 1..)?;
    let close = rest.find('>')?;
    let inner = &rest[..close];
    if inner.contains(char::is_whitespace) {
        return None;
    }
    if inner.starts_with("https://") || inner.starts_with("http://") || inner.starts_with("mailto:")
    {
        let display = inner.strip_prefix("mailto:").unwrap_or(inner);
        Some((display, start + 1 + close + 1))
    } else {
        None
    }
}

fn parse_html_br_at(input: &str, start: usize) -> Option<usize> {
    let rest = input.get(start..)?;
    let close = rest.find('>')?;
    if close > 8 {
        return None;
    }
    let tag = rest[1..close].trim().to_ascii_lowercase();
    if tag == "br" || tag == "br/" || tag == "br /" {
        Some(start + close + 1)
    } else {
        None
    }
}

fn parse_html_tag_at(input: &str, start: usize) -> Option<(&str, Option<InlineStyle>, usize)> {
    let rest = input.get(start..)?;
    let close_open = rest.find('>')?;
    if close_open == 0 || close_open > 16 {
        return None;
    }
    let tag_raw = rest[1..close_open].trim();
    if tag_raw.contains(char::is_whitespace) || tag_raw.starts_with('/') {
        return None;
    }
    let tag = tag_raw.to_ascii_lowercase();
    let style = match tag.as_str() {
        "b" | "strong" => Some(InlineStyle::Bold),
        "i" | "em" => Some(InlineStyle::Italic),
        "code" | "kbd" => Some(InlineStyle::Code),
        "s" | "del" | "strike" => Some(InlineStyle::Strikethrough),
        "u" => Some(InlineStyle::Underline),
        "small" | "sup" | "sub" | "mark" => None,
        _ => return None,
    };
    let close_pat = format!("</{tag}>");
    let after_open = &rest[close_open + 1..];
    let lower_after = after_open.to_ascii_lowercase();
    let rel_close = lower_after.find(&close_pat)?;
    let inner = &after_open[..rel_close];
    let next_i = start + close_open + 1 + rel_close + close_pat.len();
    Some((inner, style, next_i))
}

fn parse_footnote_ref_at(input: &str, start: usize) -> Option<(&str, usize)> {
    let rest = input.get(start + 2..)?;
    let close = rest.find(']')?;
    let id = &rest[..close];
    if id.is_empty()
        || id.len() > 24
        || !id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some((id, start + 2 + close + 1))
}

fn parse_inline_math_at(input: &str, start: usize) -> Option<(&str, usize)> {
    let rest = input.get(start + 1..)?;
    let first = rest.chars().next()?;
    if first.is_whitespace() || first == '$' {
        return None;
    }
    let close = rest.find('$')?;
    if close == 0 {
        return None;
    }
    let expr = &rest[..close];
    if expr.contains('\n') || expr.chars().next_back().is_some_and(char::is_whitespace) {
        return None;
    }
    // Guard against currency values like `$100` ... `$200` (pure digits/commas/dots).
    if expr
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | ' '))
    {
        return None;
    }
    Some((expr, start + 1 + close + 1))
}

fn parse_link_at(input: &str, bracket_start: usize) -> Option<(&str, usize)> {
    let rest = input.get(bracket_start + 1..)?;
    let close_bracket = rest.find(']')?;
    let label = &rest[..close_bracket];
    if label.is_empty() || label.contains('\n') {
        return None;
    }
    let after_bracket = &rest[close_bracket + 1..];
    if !after_bracket.starts_with('(') {
        return None;
    }
    let close_paren = after_bracket[1..].find(')')?;
    let url = &after_bracket[1..1 + close_paren];
    if url.is_empty() || url.contains('\n') {
        return None;
    }
    let next_i = bracket_start + 1 + close_bracket + 1 + 1 + close_paren + 1;
    Some((label, next_i))
}

fn parse_ref_link_at<'a>(
    input: &'a str,
    bracket_start: usize,
    refs: &HashMap<String, String>,
) -> Option<(&'a str, usize)> {
    if refs.is_empty() {
        return None;
    }
    let rest = input.get(bracket_start + 1..)?;
    let close_bracket = rest.find(']')?;
    let label = &rest[..close_bracket];
    if label.is_empty() || label.contains('\n') {
        return None;
    }
    let after = &rest[close_bracket + 1..];
    if let Some(after_open) = after.strip_prefix('[') {
        let close_ref = after_open.find(']')?;
        let ref_id = &after_open[..close_ref];
        let lookup = if ref_id.is_empty() { label } else { ref_id };
        if refs.contains_key(&lookup.to_ascii_lowercase()) {
            let next_i = bracket_start + 1 + close_bracket + 1 + 1 + close_ref + 1;
            return Some((label, next_i));
        }
    }
    None
}

fn parse_ref_link_def(trimmed: &str) -> Option<(&str, String)> {
    let rest = trimmed.strip_prefix('[')?;
    if rest.starts_with('^') {
        return None; // Footnote definition, not a link ref
    }
    let close = rest.find("]:")?;
    let id = rest[..close].trim();
    if id.is_empty() || id.contains('[') || id.contains(']') {
        return None;
    }
    let rhs = rest[close + 2..].trim();
    let url = rhs.split_whitespace().next()?;
    let url = url
        .strip_prefix('<')
        .and_then(|u| u.strip_suffix('>'))
        .unwrap_or(url);
    if url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("mailto:")
        || url.starts_with('/')
    {
        Some((id, url.to_owned()))
    } else {
        None
    }
}

fn parse_footnote_def(trimmed: &str) -> Option<(&str, &str)> {
    let rest = trimmed.strip_prefix("[^")?;
    let close = rest.find("]:")?;
    let id = rest[..close].trim();
    if id.is_empty()
        || id.len() > 24
        || !id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let body = rest[close + 2..].trim();
    Some((id, body))
}

fn parse_definition_line(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix(':')?;
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let body = rest.trim();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

fn parse_task_prefix(bullet: &str) -> Option<(bool, &str)> {
    if let Some(rest) = bullet
        .strip_prefix("[x] ")
        .or_else(|| bullet.strip_prefix("[X] "))
    {
        Some((true, rest))
    } else if let Some(rest) = bullet.strip_prefix("[ ] ") {
        Some((false, rest))
    } else {
        None
    }
}

fn parse_quote_line(trimmed: &str) -> Option<(u8, &str)> {
    if !trimmed.starts_with('>') {
        return None;
    }
    let mut rest = trimmed;
    let mut depth = 0u8;
    while let Some(after) = rest.strip_prefix('>') {
        depth = depth.saturating_add(1).min(4);
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    Some((depth, rest))
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let compact: Vec<char> = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && matches!(compact[0], '-' | '*' | '_')
        && compact.iter().all(|&c| c == compact[0])
}

fn parse_ordered_item(trimmed: &str) -> Option<(&str, &str)> {
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 6 {
        return None;
    }
    let after_digits = &trimmed[digits..];
    let marker = after_digits.as_bytes().first().copied()?;
    if marker != b'.' && marker != b')' {
        return None;
    }
    let body = after_digits[1..].strip_prefix(' ')?;
    Some((&trimmed[..digits + 1], body))
}

fn parse_table_header_and_sep(
    header_line: &str,
    sep_line: &str,
) -> Option<(Vec<String>, Vec<TableAlign>)> {
    if !header_line.contains('|') || !sep_line.contains('-') {
        return None;
    }
    let headers = split_table_row(header_line);
    if headers.len() < 2 {
        return None;
    }
    let sep_cells = split_table_row(sep_line);
    if sep_cells.len() != headers.len() {
        return None;
    }
    let mut alignments = Vec::with_capacity(sep_cells.len());
    for cell in sep_cells {
        let c = cell.trim();
        let left_colon = c.starts_with(':');
        let right_colon = c.ends_with(':');
        let core = c.strip_prefix(':').unwrap_or(c);
        let core = core.strip_suffix(':').unwrap_or(core);
        if core.is_empty() || !core.bytes().all(|b| b == b'-') {
            return None;
        }
        let align = match (left_colon, right_colon) {
            (true, true) => TableAlign::Center,
            (false, true) => TableAlign::Right,
            _ => TableAlign::Left,
        };
        alignments.push(align);
    }
    Some((headers, alignments))
}

fn split_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = if trimmed.ends_with('|') && !trimmed.ends_with("\\|") {
        &trimmed[..trimmed.len() - 1]
    } else {
        trimmed
    };
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            current.push('|');
        } else if ch == '|' {
            cells.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    cells.push(current.trim().to_owned());
    cells
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    let text = rest.trim();
    if text.is_empty() {
        return None;
    }
    Some((hashes as u8, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_bullets_quotes_and_fenced_code() {
        let md = "\
# Summary
Here is a line.
Second line.

- First item
  * Nested item
1. Ordered one
2. Ordered two

---

> Important note
>> Nested quote
>>> Deep quote

```rust
fn main() {
    println!(\"hi\");
}
```";
        let blocks = parse_blocks(md);
        assert_eq!(
            blocks,
            vec![
                MdBlock::Heading {
                    level: 1,
                    text: "Summary".into(),
                },
                MdBlock::Paragraph {
                    text: "Here is a line.\nSecond line.".into(),
                },
                MdBlock::Bullet {
                    indent: 0,
                    text: "First item".into(),
                },
                MdBlock::Bullet {
                    indent: 1,
                    text: "Nested item".into(),
                },
                MdBlock::OrderedItem {
                    indent: 0,
                    number: "1.".into(),
                    text: "Ordered one".into(),
                },
                MdBlock::OrderedItem {
                    indent: 0,
                    number: "2.".into(),
                    text: "Ordered two".into(),
                },
                MdBlock::Rule,
                MdBlock::Quote {
                    depth: 1,
                    text: "Important note".into(),
                },
                MdBlock::Quote {
                    depth: 2,
                    text: "Nested quote".into(),
                },
                MdBlock::Quote {
                    depth: 3,
                    text: "Deep quote".into(),
                },
                MdBlock::CodeBlock {
                    lang: Some("rust".into()),
                    code: "fn main() {\n    println!(\"hi\");\n}".into(),
                },
            ]
        );
    }

    #[test]
    fn handles_unclosed_streaming_code_fence() {
        let blocks = parse_blocks("```python\nprint(1)");
        assert_eq!(
            blocks,
            vec![MdBlock::CodeBlock {
                lang: Some("python".into()),
                code: "print(1)".into(),
            }]
        );
    }

    #[test]
    fn parses_gfm_tables_with_alignment_and_escaped_pipes() {
        let md = "\
Intro line
| Name | Count | Notes |
| :--- | :---: | ---: |
| alpha | 10 | a \\| b |
| beta | 20 | ok |

After table";
        let blocks = parse_blocks(md);
        assert_eq!(
            blocks,
            vec![
                MdBlock::Paragraph {
                    text: "Intro line".into(),
                },
                MdBlock::Table {
                    headers: vec!["Name".into(), "Count".into(), "Notes".into()],
                    alignments: vec![TableAlign::Left, TableAlign::Center, TableAlign::Right],
                    rows: vec![
                        vec!["alpha".into(), "10".into(), "a | b".into()],
                        vec!["beta".into(), "20".into(), "ok".into()],
                    ],
                },
                MdBlock::Paragraph {
                    text: "After table".into(),
                },
            ]
        );
    }

    #[test]
    fn parses_inline_bold_code_italic_strike_link_and_preserves_windows_paths() {
        let path_line =
            parse_inline(r"Готово. Файл создан: **C:\Temp\hsr\financial_stats_example.xlsx**");
        assert_eq!(
            path_line.plain,
            r"Готово. Файл создан: C:\Temp\hsr\financial_stats_example.xlsx"
        );
        assert_eq!(path_line.spans.len(), 1);
        assert_eq!(
            &path_line.plain[path_line.spans[0].0.clone()],
            r"C:\Temp\hsr\financial_stats_example.xlsx"
        );
        assert_eq!(path_line.spans[0].1, InlineStyle::Bold);

        let rich = parse_inline(
            "**По месяцам** — формулы `=B-C` и `=ROUND(D/B*100,1)`, *курсив*, ~~старое~~, [docs](https://example.com) и my_snake_var.",
        );
        assert_eq!(
            rich.plain,
            "По месяцам — формулы =B-C и =ROUND(D/B*100,1), курсив, старое, docs и my_snake_var."
        );
        let labels: Vec<(&str, InlineStyle)> = rich
            .spans
            .iter()
            .map(|(r, s)| (&rich.plain[r.clone()], *s))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("По месяцам", InlineStyle::Bold),
                ("=B-C", InlineStyle::Code),
                ("=ROUND(D/B*100,1)", InlineStyle::Code),
                ("курсив", InlineStyle::Italic),
                ("старое", InlineStyle::Strikethrough),
                ("docs", InlineStyle::Link),
            ]
        );
    }

    #[test]
    fn parses_extended_md_nested_emphasis_tasks_defs_refs_html_math_and_footnotes() {
        let md = "\
- [x] Выполнено
- [ ] Не выполнено

Термин
: Определение термина

Автолинк <https://example.org>, референс [пример][ref], сноска[^1], формула $E = mc^2$.

[ref]: https://example.net \"Референс\"

$$
\\sum_{i=1}^{n} x_i
$$

[^1]: Содержание первой сноски.";

        let doc = parse_document(md);
        assert_eq!(
            doc.link_refs.get("ref").map(String::as_str),
            Some("https://example.net")
        );
        assert!(doc.blocks.contains(&MdBlock::TaskItem {
            indent: 0,
            checked: true,
            text: "Выполнено".into(),
        }));
        assert!(doc.blocks.contains(&MdBlock::TaskItem {
            indent: 0,
            checked: false,
            text: "Не выполнено".into(),
        }));
        assert!(doc.blocks.contains(&MdBlock::Definition {
            term: "Термин".into(),
            definition: "Определение термина".into(),
        }));
        assert!(doc.blocks.contains(&MdBlock::MathBlock {
            raw: r"\sum_{i=1}^{n} x_i".into(),
        }));
        assert!(doc.blocks.contains(&MdBlock::FootnoteDef {
            id: "1".into(),
            text: "Содержание первой сноски.".into(),
        }));

        // Display math with `\[ ... \]` and fenced ```` ```math ```` plus inline `\(...\)`.
        let extra_math = parse_document(
            "Inline \\(\\alpha + \\beta\\)\n\n\\[\n\\frac{a}{b}\n\\]\n\n```math\nx^2 + y^2 = r^2\n```",
        );
        assert!(extra_math.blocks.contains(&MdBlock::MathBlock {
            raw: r"\frac{a}{b}".into(),
        }));
        assert!(extra_math.blocks.contains(&MdBlock::MathBlock {
            raw: "x^2 + y^2 = r^2".into(),
        }));
        assert_eq!(
            parse_inline(r"Inline \(\alpha + \beta\)").plain,
            "Inline α + β"
        );

        // Nested bold+italic, HTML tags, autolinks, ref links, footnotes, inline math, escaped `#`.
        let nested = parse_inline("**жирный с *курсивом* внутри** и <b>HTML</b><br>\\# тег");
        assert_eq!(nested.plain, "жирный с курсивом внутри и HTML\n# тег");
        let nested_spans: Vec<(&str, InlineStyle)> = nested
            .spans
            .iter()
            .map(|(r, s)| (&nested.plain[r.clone()], *s))
            .collect();
        assert!(nested_spans.contains(&("курсивом", InlineStyle::BoldItalic)));
        assert!(nested_spans.contains(&("HTML", InlineStyle::Bold)));

        let with_refs = parse_inline_with_refs(
            "Авто <https://example.org>, [пример][ref], сноска[^1], $E = mc^2$, цена $100 и $200.",
            &doc.link_refs,
        );
        assert_eq!(
            with_refs.plain,
            "Авто https://example.org, пример, сноска[1], E = mc², цена $100 и $200."
        );

        let py_spans = highlight_code(Some("python"), "def fib(n):\n    return 42 # ok\n");
        assert!(py_spans.iter().any(|(_, s)| *s == InlineStyle::CodeKeyword));
        assert!(py_spans.iter().any(|(_, s)| *s == InlineStyle::CodeNumber));
        assert!(py_spans.iter().any(|(_, s)| *s == InlineStyle::CodeComment));
    }
}
