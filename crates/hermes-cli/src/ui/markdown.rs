//! Markdown + syntax highlighting renderer shared by both Inline REPL (streaming ANSI)
//! and Fullscreen TUI (`ratatui::text::Line` / `Span`).
//!
//! Design invariant: plain prose without Markdown markers passes through verbatim without
//! injecting stray ANSI sequences inside words (preserving exact text matching in PTY tests).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";
const FG_CYAN: &str = "\x1b[36m";
const FG_BRIGHT_CYAN: &str = "\x1b[96m";
const FG_BLUE: &str = "\x1b[34m";
const FG_GREEN: &str = "\x1b[32m";
const FG_YELLOW: &str = "\x1b[33m";
const FG_MAGENTA: &str = "\x1b[35m";
const FG_GRAY: &str = "\x1b[90m";
const BG_CODE: &str = "\x1b[48;5;236m";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InlineKind {
    Plain,
    Bold,
    Italic,
    BoldItalic,
    Strike,
    Code,
    LinkText,
    LinkUrl,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InlineSegment {
    kind: InlineKind,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyntaxKind {
    Plain,
    Keyword,
    TypeName,
    StringLit,
    Comment,
    Number,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SyntaxToken {
    kind: SyntaxKind,
    text: String,
}

/// Streaming ANSI Markdown renderer for the Inline REPL.
///
/// Buffers only the current incomplete line when Markdown formatting is enabled, or flushes
/// plain lines immediately so token streaming remains snappy while headings, code blocks,
/// lists, tables, and inline spans render cleanly.
pub struct StreamMarkdown {
    color: bool,
    in_code_fence: bool,
    code_lang: String,
    pending_line: String,
    /// Whether `pending_line` might contain Markdown syntax; if false and not in a code block,
    /// we can stream chunks immediately without waiting for `\n`!
    buffered_for_md: bool,
}

impl StreamMarkdown {
    pub fn new(color: bool) -> Self {
        Self {
            color,
            in_code_fence: false,
            code_lang: String::new(),
            pending_line: String::new(),
            buffered_for_md: false,
        }
    }

    /// Feed a streamed text delta and return ANSI-formatted output ready to write to stdout.
    pub fn push(&mut self, chunk: &str) -> String {
        if !self.color {
            return chunk.to_owned();
        }
        let mut out = String::new();
        for ch in chunk.chars() {
            if ch == '\n' {
                if self.buffered_for_md || self.in_code_fence {
                    let line = std::mem::take(&mut self.pending_line);
                    out.push_str(&self.render_ansi_line(&line));
                    out.push('\n');
                    self.buffered_for_md = false;
                } else {
                    if !self.pending_line.is_empty() {
                        out.push_str(&std::mem::take(&mut self.pending_line));
                    }
                    out.push('\n');
                }
            } else {
                self.pending_line.push(ch);
                if !self.buffered_for_md && !self.in_code_fence {
                    if line_needs_markdown(&self.pending_line) {
                        self.buffered_for_md = true;
                    } else if self.pending_line.len() > 4
                        && !could_become_prefix(&self.pending_line)
                    {
                        // Safe plain prose: flush immediately for instant token streaming!
                        out.push_str(&std::mem::take(&mut self.pending_line));
                    }
                }
            }
        }
        out
    }

    /// Flush any remaining buffered line at the end of an assistant turn.
    pub fn finish(&mut self) -> String {
        if !self.color {
            return String::new();
        }
        let mut out = String::new();
        if !self.pending_line.is_empty() {
            let line = std::mem::take(&mut self.pending_line);
            if self.buffered_for_md || self.in_code_fence || line_needs_markdown(&line) {
                out.push_str(&self.render_ansi_line(&line));
            } else {
                out.push_str(&line);
            }
        }
        if self.in_code_fence {
            out.push_str(RESET);
            self.in_code_fence = false;
            self.code_lang.clear();
        }
        self.buffered_for_md = false;
        out
    }

    fn render_ansi_line(&mut self, line: &str) -> String {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if self.in_code_fence {
                self.in_code_fence = false;
                self.code_lang.clear();
                return format!("{FG_GRAY}╰────────────────────────────────────────{RESET}");
            } else {
                self.in_code_fence = true;
                self.code_lang = rest.trim().to_ascii_lowercase();
                let label = if self.code_lang.is_empty() {
                    "code".to_owned()
                } else {
                    self.code_lang.clone()
                };
                return format!(
                    "{FG_GRAY}╭─ {FG_CYAN}{label}{FG_GRAY} ─────────────────────────────{RESET}"
                );
            }
        }
        if self.in_code_fence {
            let mut out = format!("{FG_GRAY}│ {RESET}");
            for tok in highlight_code_line(line, &self.code_lang) {
                out.push_str(&format_syntax_ansi(&tok));
            }
            return out;
        }
        render_ansi_markdown_line(line)
    }
}

fn could_become_prefix(s: &str) -> bool {
    let t = s.trim_start();
    t.is_empty()
        || t.starts_with('#')
        || t.starts_with('`')
        || t.starts_with('-')
        || t.starts_with('*')
        || t.starts_with('+')
        || t.starts_with('>')
        || t.starts_with('|')
        || t.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn line_needs_markdown(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#')
        || t.starts_with("```")
        || t.starts_with("> ")
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || t.starts_with('|')
        || is_horizontal_rule(t)
        || is_ordered_list_prefix(t).is_some()
        || line.contains("**")
        || line.contains('`')
        || line.contains("~~")
        || (line.contains('[') && line.contains("]("))
        || has_inline_italic(line)
}

fn has_inline_italic(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut stars = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'*' {
            let prev_star = i > 0 && bytes[i - 1] == b'*';
            let next_star = i + 1 < bytes.len() && bytes[i + 1] == b'*';
            if !prev_star && !next_star {
                stars += 1;
            }
        }
    }
    stars >= 2
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let clean: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    clean.len() >= 3
        && (clean.chars().all(|c| c == '-')
            || clean.chars().all(|c| c == '*')
            || clean.chars().all(|c| c == '_'))
}

fn is_ordered_list_prefix(trimmed: &str) -> Option<(&str, &str)> {
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 4 {
        return None;
    }
    let rest = &trimmed[digits..];
    if let Some(after) = rest.strip_prefix(". ") {
        Some((&trimmed[..digits + 1], after))
    } else if let Some(after) = rest.strip_prefix(") ") {
        Some((&trimmed[..digits + 1], after))
    } else {
        None
    }
}

fn is_table_separator(trimmed: &str) -> bool {
    trimmed.starts_with('|')
        && trimmed.contains('-')
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn render_ansi_markdown_line(line: &str) -> String {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let trimmed = &line[indent_len..];

    if is_horizontal_rule(trimmed) {
        return format!("{FG_GRAY}────────────────────────────────────────{RESET}");
    }
    // Headings
    if let Some(level) = heading_level(trimmed) {
        let text = trimmed[level..].trim_start();
        let style = match level {
            1 => format!("{BOLD}{UNDERLINE}{FG_BRIGHT_CYAN}"),
            2 => format!("{BOLD}{FG_CYAN}"),
            3 => format!("{BOLD}{FG_BLUE}"),
            _ => format!("{BOLD}{FG_MAGENTA}"),
        };
        return format!("{style}{text}{RESET}");
    }
    // Blockquote
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return format!(
            "{indent}{FG_GRAY}│ {ITALIC}{}{RESET}",
            render_inline_ansi(rest)
        );
    }
    // Markdown table
    if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 3 {
        if is_table_separator(trimmed) {
            return format!("{indent}{FG_GRAY}{}{RESET}", trimmed.replace('-', "─"));
        }
        let cells: Vec<&str> = trimmed
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        let mut out = format!("{indent}{FG_GRAY}│{RESET}");
        for cell in cells {
            out.push(' ');
            out.push_str(&render_inline_ansi(cell));
            out.push_str(&format!(" {FG_GRAY}│{RESET}"));
        }
        return out;
    }
    // Unordered list
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return format!("{indent}{FG_CYAN}•{RESET} {}", render_inline_ansi(rest));
    }
    // Ordered list
    if let Some((num, rest)) = is_ordered_list_prefix(trimmed) {
        return format!("{indent}{FG_CYAN}{num}{RESET} {}", render_inline_ansi(rest));
    }
    render_inline_ansi(line)
}

fn heading_level(trimmed: &str) -> Option<usize> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
        Some(hashes)
    } else {
        None
    }
}

fn render_inline_ansi(text: &str) -> String {
    let segments = parse_inline_segments(text);
    if segments.iter().all(|s| s.kind == InlineKind::Plain) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 16);
    for seg in segments {
        match seg.kind {
            InlineKind::Plain => out.push_str(&seg.text),
            InlineKind::Bold => {
                out.push_str(BOLD);
                out.push_str(&seg.text);
                out.push_str(RESET);
            }
            InlineKind::Italic => {
                out.push_str(ITALIC);
                out.push_str(&seg.text);
                out.push_str(RESET);
            }
            InlineKind::BoldItalic => {
                out.push_str(BOLD);
                out.push_str(ITALIC);
                out.push_str(&seg.text);
                out.push_str(RESET);
            }
            InlineKind::Strike => {
                out.push_str(STRIKE);
                out.push_str(DIM);
                out.push_str(&seg.text);
                out.push_str(RESET);
            }
            InlineKind::Code => {
                out.push_str(BG_CODE);
                out.push_str(FG_YELLOW);
                out.push(' ');
                out.push_str(&seg.text);
                out.push(' ');
                out.push_str(RESET);
            }
            InlineKind::LinkText => {
                out.push_str(UNDERLINE);
                out.push_str(FG_CYAN);
                out.push_str(&seg.text);
                out.push_str(RESET);
            }
            InlineKind::LinkUrl => {
                out.push_str(DIM);
                out.push_str(" (");
                out.push_str(&seg.text);
                out.push(')');
                out.push_str(RESET);
            }
        }
    }
    out
}

/// Render a full Markdown document into `ratatui` styled `Line<'static>`s for the Fullscreen TUI.
pub fn render_markdown_lines(text: &str, color: bool) -> Vec<Line<'static>> {
    if !color {
        return text.lines().map(|l| Line::from(l.to_owned())).collect();
    }
    let mut out = Vec::new();
    let mut in_code = false;
    let mut lang = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if in_code {
                in_code = false;
                lang.clear();
                out.push(Line::from(Span::styled(
                    "╰────────────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                in_code = true;
                lang = rest.trim().to_ascii_lowercase();
                let label = if lang.is_empty() {
                    "code".to_owned()
                } else {
                    lang.clone()
                };
                out.push(Line::from(vec![
                    Span::styled("╭─ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " ─────────────────────────────",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            continue;
        }
        if in_code {
            let mut spans = vec![Span::styled("│ ", Style::default().fg(Color::DarkGray))];
            for tok in highlight_code_line(line, &lang) {
                spans.push(syntax_token_to_span(tok));
            }
            out.push(Line::from(spans));
            continue;
        }
        out.push(render_ratatui_markdown_line(line));
    }
    if text.is_empty() {
        out.push(Line::from(""));
    }
    out
}

fn render_ratatui_markdown_line(line: &str) -> Line<'static> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = line[..indent_len].to_owned();
    let trimmed = &line[indent_len..];

    if is_horizontal_rule(trimmed) {
        return Line::from(Span::styled(
            "────────────────────────────────────────",
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(level) = heading_level(trimmed) {
        let text = trimmed[level..].trim_start().to_owned();
        let style = match level {
            1 => Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            3 => Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
            _ => Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        };
        return Line::from(Span::styled(text, style));
    }
    if let Some(rest) = trimmed.strip_prefix("> ") {
        let mut spans = vec![
            Span::raw(indent),
            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        ];
        for seg in parse_inline_segments(rest) {
            spans.push(inline_segment_to_span(seg, Modifier::ITALIC));
        }
        return Line::from(spans);
    }
    if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 3 {
        if is_table_separator(trimmed) {
            return Line::from(Span::styled(
                format!("{indent}{}", trimmed.replace('-', "─")),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let mut spans = vec![
            Span::raw(indent),
            Span::styled("│", Style::default().fg(Color::DarkGray)),
        ];
        for cell in trimmed.trim_matches('|').split('|').map(str::trim) {
            spans.push(Span::raw(" "));
            for seg in parse_inline_segments(cell) {
                spans.push(inline_segment_to_span(seg, Modifier::empty()));
            }
            spans.push(Span::styled(" │", Style::default().fg(Color::DarkGray)));
        }
        return Line::from(spans);
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        let mut spans = vec![
            Span::raw(indent),
            Span::styled("• ", Style::default().fg(Color::Cyan)),
        ];
        for seg in parse_inline_segments(rest) {
            spans.push(inline_segment_to_span(seg, Modifier::empty()));
        }
        return Line::from(spans);
    }
    if let Some((num, rest)) = is_ordered_list_prefix(trimmed) {
        let mut spans = vec![
            Span::raw(indent),
            Span::styled(format!("{num} "), Style::default().fg(Color::Cyan)),
        ];
        for seg in parse_inline_segments(rest) {
            spans.push(inline_segment_to_span(seg, Modifier::empty()));
        }
        return Line::from(spans);
    }
    let spans: Vec<Span<'static>> = parse_inline_segments(line)
        .into_iter()
        .map(|seg| inline_segment_to_span(seg, Modifier::empty()))
        .collect();
    Line::from(spans)
}

fn inline_segment_to_span(seg: InlineSegment, extra_mod: Modifier) -> Span<'static> {
    let base = Style::default().add_modifier(extra_mod);
    match seg.kind {
        InlineKind::Plain => Span::styled(seg.text, base),
        InlineKind::Bold => Span::styled(seg.text, base.add_modifier(Modifier::BOLD)),
        InlineKind::Italic => Span::styled(seg.text, base.add_modifier(Modifier::ITALIC)),
        InlineKind::BoldItalic => Span::styled(
            seg.text,
            base.add_modifier(Modifier::BOLD | Modifier::ITALIC),
        ),
        InlineKind::Strike => Span::styled(
            seg.text,
            base.fg(Color::DarkGray).add_modifier(Modifier::CROSSED_OUT),
        ),
        InlineKind::Code => Span::styled(
            format!(" {} ", seg.text),
            Style::default().fg(Color::Yellow).bg(Color::DarkGray),
        ),
        InlineKind::LinkText => Span::styled(
            seg.text,
            base.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED),
        ),
        InlineKind::LinkUrl => Span::styled(
            format!(" ({})", seg.text),
            Style::default().fg(Color::DarkGray),
        ),
    }
}

/// Parse inline Markdown spans: `` `code` ``, `***bold italic***`, `**bold**`, `*italic*`,
/// `~~strike~~`, and `[label](url)`.
fn parse_inline_segments(text: &str) -> Vec<InlineSegment> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;

    let flush_plain = |plain: &mut String, out: &mut Vec<InlineSegment>| {
        if !plain.is_empty() {
            out.push(InlineSegment {
                kind: InlineKind::Plain,
                text: std::mem::take(plain),
            });
        }
    };

    while i < n {
        // Inline code: `...`
        if chars[i] == '`' {
            if let Some(end) = ((i + 1)..n).find(|&j| chars[j] == '`') {
                if end > i + 1 {
                    flush_plain(&mut plain, &mut out);
                    let code: String = chars[i + 1..end].iter().collect();
                    out.push(InlineSegment {
                        kind: InlineKind::Code,
                        text: code,
                    });
                    i = end + 1;
                    continue;
                }
            }
        }
        // Bold+Italic: ***...***
        if i + 2 < n && chars[i] == '*' && chars[i + 1] == '*' && chars[i + 2] == '*' {
            if let Some(end) = find_delimiter(&chars, i + 3, "***") {
                if end > i + 3 {
                    flush_plain(&mut plain, &mut out);
                    out.push(InlineSegment {
                        kind: InlineKind::BoldItalic,
                        text: chars[i + 3..end].iter().collect(),
                    });
                    i = end + 3;
                    continue;
                }
            }
        }
        // Bold: **...**
        if i + 1 < n && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_delimiter(&chars, i + 2, "**") {
                if end > i + 2 {
                    flush_plain(&mut plain, &mut out);
                    out.push(InlineSegment {
                        kind: InlineKind::Bold,
                        text: chars[i + 2..end].iter().collect(),
                    });
                    i = end + 2;
                    continue;
                }
            }
        }
        // Strikethrough: ~~...~~
        if i + 1 < n && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_delimiter(&chars, i + 2, "~~") {
                if end > i + 2 {
                    flush_plain(&mut plain, &mut out);
                    out.push(InlineSegment {
                        kind: InlineKind::Strike,
                        text: chars[i + 2..end].iter().collect(),
                    });
                    i = end + 2;
                    continue;
                }
            }
        }
        // Italic: *...* (not adjacent to spaces at opening)
        if chars[i] == '*' && i + 1 < n && !chars[i + 1].is_whitespace() && chars[i + 1] != '*' {
            if let Some(end) =
                ((i + 1)..n).find(|&j| chars[j] == '*' && !chars[j - 1].is_whitespace())
            {
                flush_plain(&mut plain, &mut out);
                out.push(InlineSegment {
                    kind: InlineKind::Italic,
                    text: chars[i + 1..end].iter().collect(),
                });
                i = end + 1;
                continue;
            }
        }
        // Link: [label](url)
        if chars[i] == '[' {
            if let Some(close_br) = ((i + 1)..n).find(|&j| chars[j] == ']') {
                if close_br + 1 < n && chars[close_br + 1] == '(' {
                    if let Some(close_par) = ((close_br + 2)..n).find(|&j| chars[j] == ')') {
                        flush_plain(&mut plain, &mut out);
                        out.push(InlineSegment {
                            kind: InlineKind::LinkText,
                            text: chars[i + 1..close_br].iter().collect(),
                        });
                        out.push(InlineSegment {
                            kind: InlineKind::LinkUrl,
                            text: chars[close_br + 2..close_par].iter().collect(),
                        });
                        i = close_par + 1;
                        continue;
                    }
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush_plain(&mut plain, &mut out);
    out
}

fn find_delimiter(chars: &[char], start: usize, delim: &str) -> Option<usize> {
    let pat: Vec<char> = delim.chars().collect();
    let k = pat.len();
    if start + k > chars.len() {
        return None;
    }
    (start..=chars.len() - k).find(|&idx| chars[idx..idx + k] == pat[..])
}

// ── Zero-dep Syntax Highlighting for Fenced Code Blocks ─────────────────────────────

fn language_keywords(lang: &str) -> (&'static [&'static str], &'static str) {
    match lang {
        "rust" | "rs" => (
            &[
                "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "for", "while",
                "loop", "if", "else", "match", "return", "use", "mod", "async", "await", "const",
                "static", "type", "where", "crate", "self", "Self", "true", "false", "as", "in",
                "ref", "move", "unsafe",
            ],
            "//",
        ),
        "python" | "py" => (
            &[
                "def", "class", "import", "from", "return", "if", "elif", "else", "for", "while",
                "in", "not", "and", "or", "with", "as", "try", "except", "finally", "raise",
                "yield", "async", "await", "pass", "break", "continue", "lambda", "True", "False",
                "None", "self",
            ],
            "#",
        ),
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" => (
            &[
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
                "async",
                "await",
                "try",
                "catch",
                "finally",
                "throw",
                "type",
                "interface",
                "enum",
                "true",
                "false",
                "null",
                "undefined",
                "this",
            ],
            "//",
        ),
        "bash" | "sh" | "zsh" | "shell" => (
            &[
                "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "case",
                "esac", "function", "export", "local", "return", "exit", "set", "echo", "cd",
            ],
            "#",
        ),
        "json" | "jsonc" => (&["true", "false", "null"], "//"),
        "yaml" | "yml" | "toml" => (&["true", "false", "null", "yes", "no"], "#"),
        "sql" => (
            &[
                "SELECT", "FROM", "WHERE", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE",
                "CREATE", "TABLE", "INDEX", "JOIN", "LEFT", "INNER", "ON", "ORDER", "BY", "GROUP",
                "LIMIT", "AND", "OR", "NOT", "NULL", "AS", "select", "from", "where", "insert",
                "update", "delete", "create", "table", "join", "order", "by", "limit",
            ],
            "--",
        ),
        "go" | "golang" => (
            &[
                "func",
                "package",
                "import",
                "var",
                "const",
                "type",
                "struct",
                "interface",
                "if",
                "else",
                "for",
                "range",
                "return",
                "go",
                "defer",
                "select",
                "case",
                "switch",
                "chan",
                "map",
                "true",
                "false",
                "nil",
            ],
            "//",
        ),
        "c" | "cpp" | "c++" | "h" | "hpp" => (
            &[
                "int", "char", "void", "bool", "float", "double", "const", "static", "struct",
                "enum", "class", "public", "private", "return", "if", "else", "for", "while",
                "switch", "case", "break", "continue", "sizeof", "typedef", "auto", "nullptr",
                "true", "false",
            ],
            "//",
        ),
        _ => (&[], "//"),
    }
}

fn highlight_code_line(line: &str, lang: &str) -> Vec<SyntaxToken> {
    let (keywords, comment_prefix) = language_keywords(lang);
    let mut tokens = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        // Line comment
        if !comment_prefix.is_empty() && line_starts_with_at(&chars, i, comment_prefix) {
            tokens.push(SyntaxToken {
                kind: SyntaxKind::Comment,
                text: chars[i..].iter().collect(),
            });
            break;
        }
        // Quoted string literal
        if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            let start = i;
            i += 1;
            while i < n {
                if chars[i] == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                let ch = chars[i];
                i += 1;
                if ch == quote {
                    break;
                }
            }
            tokens.push(SyntaxToken {
                kind: SyntaxKind::StringLit,
                text: chars[start..i].iter().collect(),
            });
            continue;
        }
        // Number literal
        if chars[i].is_ascii_digit()
            && (i == 0 || !(chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_'))
        {
            let start = i;
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            tokens.push(SyntaxToken {
                kind: SyntaxKind::Number,
                text: chars[start..i].iter().collect(),
            });
            continue;
        }
        // Identifier / keyword / PascalCase type
        if chars[i].is_ascii_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let kind = if keywords.contains(&word.as_str()) {
                SyntaxKind::Keyword
            } else if word.len() >= 2
                && word.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && word.chars().any(|c| c.is_ascii_lowercase())
            {
                SyntaxKind::TypeName
            } else {
                SyntaxKind::Plain
            };
            tokens.push(SyntaxToken { kind, text: word });
            continue;
        }
        // Other punctuation/whitespace
        let start = i;
        i += 1;
        while i < n
            && !chars[i].is_ascii_alphanumeric()
            && chars[i] != '_'
            && chars[i] != '"'
            && chars[i] != '\''
            && (comment_prefix.is_empty() || !line_starts_with_at(&chars, i, comment_prefix))
        {
            i += 1;
        }
        tokens.push(SyntaxToken {
            kind: SyntaxKind::Plain,
            text: chars[start..i].iter().collect(),
        });
    }
    tokens
}

fn line_starts_with_at(chars: &[char], idx: usize, prefix: &str) -> bool {
    let pat: Vec<char> = prefix.chars().collect();
    idx + pat.len() <= chars.len() && chars[idx..idx + pat.len()] == pat[..]
}

fn format_syntax_ansi(tok: &SyntaxToken) -> String {
    match tok.kind {
        SyntaxKind::Plain => tok.text.clone(),
        SyntaxKind::Keyword => format!("{BOLD}{FG_MAGENTA}{}{RESET}", tok.text),
        SyntaxKind::TypeName => format!("{FG_CYAN}{}{RESET}", tok.text),
        SyntaxKind::StringLit => format!("{FG_GREEN}{}{RESET}", tok.text),
        SyntaxKind::Comment => format!("{FG_GRAY}{ITALIC}{}{RESET}", tok.text),
        SyntaxKind::Number => format!("{FG_YELLOW}{}{RESET}", tok.text),
    }
}

fn syntax_token_to_span(tok: SyntaxToken) -> Span<'static> {
    match tok.kind {
        SyntaxKind::Plain => Span::raw(tok.text),
        SyntaxKind::Keyword => Span::styled(
            tok.text,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        SyntaxKind::TypeName => Span::styled(tok.text, Style::default().fg(Color::Cyan)),
        SyntaxKind::StringLit => Span::styled(tok.text, Style::default().fg(Color::Green)),
        SyntaxKind::Comment => Span::styled(
            tok.text,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
        SyntaxKind::Number => Span::styled(tok.text, Style::default().fg(Color::Yellow)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prose_passes_through_without_ansi_noise() {
        let mut md = StreamMarkdown::new(true);
        let mut out = md.push("Ответ ");
        out.push_str(&md.push("модели"));
        out.push_str(&md.finish());
        assert_eq!(out, "Ответ модели");
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn streams_headings_bold_inline_code_and_fenced_rust_highlighting() {
        let mut md = StreamMarkdown::new(true);
        let input = "# Title\n\nUse **bold** and `cargo test`.\n```rust\nfn main() { let x = 42; // hi\n}\n```\n";
        let mut out = String::new();
        for chunk in input.as_bytes().chunks(7) {
            out.push_str(&md.push(std::str::from_utf8(chunk).unwrap()));
        }
        out.push_str(&md.finish());
        assert!(out.contains(&format!("{BOLD}{UNDERLINE}{FG_BRIGHT_CYAN}Title{RESET}")));
        assert!(!out.contains("# Title"));
        assert!(out.contains(BOLD));
        assert!(out.contains("cargo test"));
        assert!(out.contains("╭─ "));
        assert!(out.contains("rust"));
        assert!(out.contains(&format!("{BOLD}{FG_MAGENTA}fn{RESET}")));
        assert!(out.contains(&format!("{FG_YELLOW}42{RESET}")));
        assert!(out.contains(&format!("{FG_GRAY}{ITALIC}// hi{RESET}")));
        assert!(out.contains("╰──"));
    }

    #[test]
    fn disabled_color_returns_raw_markdown() {
        let mut md = StreamMarkdown::new(false);
        let out = format!("{}{}", md.push("# Hi **x**"), md.finish());
        assert_eq!(out, "# Hi **x**");
    }

    #[test]
    fn ratatui_markdown_renders_headings_lists_tables_and_code() {
        let doc = "## Plan\n- First **item**\n| A | B |\n|---|---|\n| 1 | `x` |\n```python\ndef f(): return \"ok\"\n```";
        let lines = render_markdown_lines(doc, true);
        assert!(lines.len() >= 7);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Plan"));
        assert!(!text.contains("## Plan"));
        assert!(text.contains("• First item"));
        assert!(text.contains("python"));
        assert!(text.contains("def f(): return \"ok\""));
    }
}
