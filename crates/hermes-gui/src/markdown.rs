//! Lightweight Markdown block parser for rendering assistant replies in GPUI.

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MdBlock {
    Heading { level: u8, text: String },
    CodeBlock { lang: Option<String>, code: String },
    Bullet { text: String },
    Quote { text: String },
    Paragraph { text: String },
}

pub fn parse_blocks(input: &str) -> Vec<MdBlock> {
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

    for raw_line in input.lines() {
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
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("```") {
            flush_para(&mut para_lines, &mut blocks);
            let lang = rest.trim();
            code_lang = (!lang.is_empty()).then(|| lang.to_owned());
            in_code = true;
            continue;
        }

        if trimmed.is_empty() {
            flush_para(&mut para_lines, &mut blocks);
            continue;
        }

        if let Some((level, heading)) = parse_heading(trimmed) {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::Heading {
                level,
                text: heading.to_owned(),
            });
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
            continue;
        }

        if let Some(quote) = trimmed.strip_prefix("> ") {
            flush_para(&mut para_lines, &mut blocks);
            blocks.push(MdBlock::Quote {
                text: quote.to_owned(),
            });
            continue;
        }

        para_lines.push(raw_line);
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
}
