//! Lightweight Markdown block parser for rendering assistant replies in GPUI.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableAlign {
    Left,
    Center,
    Right,
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
        text: String,
    },
    Quote {
        text: String,
    },
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
            .or_else(|| trimmed.strip_prefix("• "))
        {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::Bullet {
                text: bullet.to_owned(),
            });
            i += 1;
            continue;
        }

        if let Some(quote) = trimmed.strip_prefix("> ") {
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
* Second item

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
                    text: "First item".into(),
                },
                MdBlock::Bullet {
                    text: "Second item".into(),
                },
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
}
