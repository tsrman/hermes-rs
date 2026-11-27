//! Lightweight Markdown block & inline parser for rendering assistant replies in GPUI.

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
    Link,
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
    Bullet {
        indent: u8,
        text: String,
    },
    OrderedItem {
        indent: u8,
        number: String,
        text: String,
    },
    Quote {
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

pub fn parse_blocks(input: &str) -> Vec<MdBlock> {
    let lines: Vec<&str> = input.lines().collect();
    let mut blocks = Vec::new();
    let mut para_lines: Vec<&str> = Vec::new();
    let mut in_code = false;
    let mut code_lang: Option<String> = None;
    let mut code_lines: Vec<&str> = Vec::new();

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
                blocks.push(MdBlock::CodeBlock {
                    lang: code_lang.take(),
                    code: code_lines.join("\n"),
                });
                code_lines.clear();
                in_code = false;
            } else {
                code_lines.push(raw_line);
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

        if trimmed.is_empty() {
            flush_para(&mut para_lines, &mut blocks);
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
                    || row_trimmed.starts_with("> ")
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
            blocks.push(MdBlock::Bullet {
                indent,
                text: bullet.to_owned(),
            });
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

        if let Some(quote) = trimmed
            .strip_prefix("> ")
            .or_else(|| (trimmed == ">").then_some(""))
        {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::Quote {
                text: quote.to_owned(),
            });
            i += 1;
            continue;
        }

        para_lines.push(raw_line);
        i += 1;
    }

    if in_code {
        blocks.push(MdBlock::CodeBlock {
            lang: code_lang.take(),
            code: code_lines.join("\n"),
        });
    } else {
        flush_para(&mut para_lines, &mut blocks);
    }

    blocks
}

/// Parse inline Markdown (`**bold**`, `*italic*`, `***bold italic***`, `` `code` ``,
/// `~~strike~~`, `[label](url)`) into clean display text plus sorted, non-overlapping style spans.
/// Preserves Windows paths (e.g. `C:\Temp\hsr\financial_stats_example.xlsx`) verbatim.
pub fn parse_inline(input: &str) -> RenderedInline {
    let mut plain = String::with_capacity(input.len());
    let mut spans = Vec::new();
    let bytes = input.as_bytes();
    let len = input.len();
    let mut i = 0;

    while i < len {
        // 1. Escaped Markdown punctuation (never eats Windows path separators like `\T` or `\f`).
        if bytes[i] == b'\\' && i + 1 < len {
            let next = bytes[i + 1];
            if matches!(next, b'*' | b'`' | b'_' | b'~' | b'[' | b']') {
                plain.push(next as char);
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
                if !code.is_empty() {
                    let start = plain.len();
                    plain.push_str(code);
                    let end = plain.len();
                    spans.push((start..end, InlineStyle::Code));
                }
                i += tick_count + rel_end + tick_count;
                continue;
            }
        }

        // 3. Links `[label](url)` and images `![alt](url)`
        if bytes[i] == b'[' || (bytes[i] == b'!' && i + 1 < len && bytes[i + 1] == b'[') {
            let bracket_start = if bytes[i] == b'!' { i + 1 } else { i };
            if let Some((label, next_i)) = parse_link_at(input, bracket_start) {
                let start = plain.len();
                plain.push_str(label);
                let end = plain.len();
                if start < end {
                    spans.push((start..end, InlineStyle::Link));
                }
                i = next_i;
                continue;
            }
        }

        // 4. Strikethrough `~~...~~`
        if input[i..].starts_with("~~") {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "~~", false) {
                let inner = &input[i + 2..i + 2 + rel_end];
                let start = plain.len();
                plain.push_str(inner);
                let end = plain.len();
                if start < end {
                    spans.push((start..end, InlineStyle::Strikethrough));
                }
                i += 2 + rel_end + 2;
                continue;
            }
        }

        // 5. Bold italic `***...***` or `___...___`
        if input[i..].starts_with("***") {
            if let Some(rel_end) = find_closing_delim(input, i + 3, "***", false) {
                let inner = &input[i + 3..i + 3 + rel_end];
                let start = plain.len();
                plain.push_str(inner);
                let end = plain.len();
                if start < end {
                    spans.push((start..end, InlineStyle::BoldItalic));
                }
                i += 3 + rel_end + 3;
                continue;
            }
        }
        if input[i..].starts_with("___") && is_word_boundary_before(input, i) {
            if let Some(rel_end) = find_closing_delim(input, i + 3, "___", true) {
                let inner = &input[i + 3..i + 3 + rel_end];
                let start = plain.len();
                plain.push_str(inner);
                let end = plain.len();
                if start < end {
                    spans.push((start..end, InlineStyle::BoldItalic));
                }
                i += 3 + rel_end + 3;
                continue;
            }
        }

        // 6. Bold `**...**` or `__...__`
        if input[i..].starts_with("**") {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "**", false) {
                let inner = &input[i + 2..i + 2 + rel_end];
                push_styled_nested(&mut plain, &mut spans, inner, InlineStyle::Bold);
                i += 2 + rel_end + 2;
                continue;
            }
        }
        if input[i..].starts_with("__") && is_word_boundary_before(input, i) {
            if let Some(rel_end) = find_closing_delim(input, i + 2, "__", true) {
                let inner = &input[i + 2..i + 2 + rel_end];
                push_styled_nested(&mut plain, &mut spans, inner, InlineStyle::Bold);
                i += 2 + rel_end + 2;
                continue;
            }
        }

        // 7. Italic `*...*` or `_..._`
        if bytes[i] == b'*' && (i + 1 >= len || bytes[i + 1] != b'*') {
            if let Some(rel_end) = find_closing_single(input, i + 1, b'*', false) {
                let inner = &input[i + 1..i + 1 + rel_end];
                push_styled_nested(&mut plain, &mut spans, inner, InlineStyle::Italic);
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
                push_styled_nested(&mut plain, &mut spans, inner, InlineStyle::Italic);
                i += 1 + rel_end + 1;
                continue;
            }
        }

        // Plain character
        let ch = input[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }

    RenderedInline { plain, spans }
}

/// Emit `inner` with `outer_style`, still recognizing inline `` `code` `` spans inside it.
fn push_styled_nested(
    plain: &mut String,
    spans: &mut Vec<(Range<usize>, InlineStyle)>,
    inner: &str,
    outer_style: InlineStyle,
) {
    let mut pos = 0;
    let bytes = inner.as_bytes();
    while pos < inner.len() {
        if bytes[pos] == b'`' {
            let tick_count = bytes[pos..].iter().take_while(|&&b| b == b'`').count();
            let delim = &inner[pos..pos + tick_count];
            if let Some(rel_end) = inner[pos + tick_count..].find(delim) {
                let code = &inner[pos + tick_count..pos + tick_count + rel_end];
                if !code.is_empty() {
                    let start = plain.len();
                    plain.push_str(code);
                    let end = plain.len();
                    spans.push((start..end, InlineStyle::Code));
                }
                pos += tick_count + rel_end + tick_count;
                continue;
            }
        }
        let next_tick = inner[pos..].find('`').map_or(inner.len(), |r| pos + r);
        let chunk = if next_tick == pos {
            let ch = inner[pos..].chars().next().unwrap();
            &inner[pos..pos + ch.len_utf8()]
        } else {
            &inner[pos..next_tick]
        };
        let start = plain.len();
        plain.push_str(chunk);
        let end = plain.len();
        if start < end {
            spans.push((start..end, outer_style));
        }
        pos += chunk.len();
    }
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
        if bytes[idx] == b'`' {
            // Skip over inline code spans while searching for closing emphasis marker.
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
                    text: "Important note".into(),
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
}
