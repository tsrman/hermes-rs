//! Conservative heredoc masking for the approval scanners (`tools/shell_heredoc.py`, only
//! `mask_heredoc_bodies_for_shell_scan` and what it needs). A body is dropped ONLY when every
//! delimiter is quoted, each heredoc has an exact terminator line, the owning simple command is an
//! allow-listed interpreter and no list operator follows; otherwise the command is untouched.

use std::sync::LazyLock;

use fancy_regex::Regex;

use super::py_isspace;

static INERT_CONSUMER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:[A-Z_][A-Z0-9_]*=\S+\s+)*(?:env\s+)?(?:[A-Za-z0-9_./-]+/)?(?:python(?:3(?:\.\d+)*)?|osascript|cat)(?=\s|$)",
    )
    .expect("valid pattern")
});
static DATA_CONSUMER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:[A-Z_][A-Z0-9_]*=\S+\s+)*(?:env\s+)?(?:[A-Za-z0-9_./-]+/)?cat(?=\s|$)")
        .expect("valid pattern")
});

fn starts_with(chars: &[char], at: usize, pattern: &str) -> bool {
    pattern
        .chars()
        .enumerate()
        .all(|(k, p)| chars.get(at + k) == Some(&p))
}

fn find_from(chars: &[char], ch: char, from: usize) -> Option<usize> {
    (from..chars.len()).find(|&k| chars[k] == ch)
}

fn span_end(chars: &[char], cursor: usize, closer: char) -> usize {
    let mut end = cursor + 1;
    while end < chars.len() {
        if chars[end] == closer {
            return end + 1;
        }
        end += if chars[end] == '\\' && end + 1 < chars.len() {
            2
        } else {
            1
        };
    }
    end
}

/// Blank inert quoted spans; keep `$(`/backtick-bearing ones visible.
fn mask_simple_quotes(command: &[char]) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    while cursor < command.len() {
        let c = command[cursor];
        if c == '\'' || c == '"' {
            let end = if c == '\'' {
                find_from(command, '\'', cursor + 1).map_or(0, |k| k + 1)
            } else {
                span_end(command, cursor, '"')
            };
            let segment = if end > cursor {
                &command[cursor..end]
            } else {
                &command[cursor..cursor]
            };
            if segment.last() != Some(&c) || segment.len() < 2 {
                result.extend(&command[cursor..]);
                break;
            }
            let text: String = segment.iter().collect();
            let keep = c == '"' && (text.contains("$(") || text.contains('`'));
            if keep {
                result.push_str(&text);
            } else {
                result.push(c);
                result.push(c);
            }
            cursor = end;
        } else if c == '`' {
            let end = span_end(command, cursor, '`');
            result.extend(&command[cursor..end]);
            cursor = end;
        } else {
            result.push(c);
            cursor += 1;
        }
    }
    result
}

/// One `<<` opener: `(end_index, delimiter, strip_tabs, quoted)`.
fn parse_heredoc_operator(command: &[char], index: usize) -> Option<(usize, String, bool, bool)> {
    if !starts_with(command, index, "<<") || starts_with(command, index, "<<<") {
        return None;
    }
    let strip_tabs = command.get(index + 2) == Some(&'-');
    let mut cursor = if strip_tabs { index + 3 } else { index + 2 };
    while matches!(command.get(cursor), Some(' ' | '\t')) {
        cursor += 1;
    }
    if cursor >= command.len() || matches!(command[cursor], '\r' | '\n') {
        return None;
    }
    let mut delimiter = String::new();
    let mut quoted = false;
    while cursor < command.len()
        && !(py_isspace(command[cursor]) || ";&|<>()".contains(command[cursor]))
    {
        let c = command[cursor];
        if c == '$' {
            return None;
        }
        if c == '\\' {
            if cursor + 1 >= command.len() || matches!(command[cursor + 1], '\r' | '\n') {
                return None;
            }
            quoted = true;
            delimiter.push(command[cursor + 1]);
            cursor += 2;
        } else if c == '\'' || c == '"' {
            quoted = true;
            cursor += 1;
            while cursor < command.len() && command[cursor] != c {
                let mut current = command[cursor];
                if matches!(current, '\r' | '\n') {
                    return None;
                }
                if c == '"' && current == '\\' {
                    if cursor + 1 >= command.len() {
                        return None;
                    }
                    if "$`\"\\\n".contains(command[cursor + 1]) {
                        cursor += 1;
                        current = command[cursor];
                    }
                }
                delimiter.push(current);
                cursor += 1;
            }
            if cursor >= command.len() {
                return None;
            }
            cursor += 1;
        } else {
            delimiter.push(c);
            cursor += 1;
        }
    }
    if delimiter.is_empty() && !quoted {
        return None;
    }
    Some((cursor, delimiter, strip_tabs, quoted))
}

fn is_fd_redirect_ampersand(command: &[char], index: usize) -> bool {
    let before = if index > 0 { command[index - 1] } else { '\0' };
    let after = command.get(index + 1).copied().unwrap_or('\0');
    before == '<' || before == '>' || after == '>'
}

struct Unit {
    end: usize,
    specs: Vec<(String, bool, bool)>,
    unknown: bool,
    post_list_operator: bool,
    owner_start: usize,
}

fn scan_command_unit(command: &[char], start: usize) -> Unit {
    let mut cursor = start;
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut unit = Unit {
        end: 0,
        specs: Vec::new(),
        unknown: false,
        post_list_operator: false,
        owner_start: start,
    };
    while cursor < command.len() {
        let c = command[cursor];
        if c == '\n' && (comment || quote.is_none()) {
            break;
        }
        let escaped = c == '\\' && quote != Some('\'') && !comment && cursor + 1 < command.len();
        if comment || quote.is_some() || escaped {
            if Some(c) == quote {
                quote = None;
            }
            cursor += if escaped { 2 } else { 1 };
        } else if c == '\'' || c == '"' || c == '`' {
            quote = Some(c);
            cursor += 1;
        } else if c == '#'
            && (cursor == start
                || py_isspace(command[cursor - 1])
                || ";&|()".contains(command[cursor - 1]))
        {
            comment = true;
            cursor += 1;
        } else if starts_with(command, cursor, "<<<") {
            cursor += 3;
        } else if starts_with(command, cursor, "<<") {
            match parse_heredoc_operator(command, cursor) {
                None => {
                    unit.unknown = true;
                    cursor += 2;
                }
                Some((end, delimiter, strip_tabs, quoted)) => {
                    cursor = end;
                    unit.specs.push((delimiter, strip_tabs, quoted));
                }
            }
        } else {
            if ";|&".contains(c) && !(c == '&' && is_fd_redirect_ampersand(command, cursor)) {
                if unit.specs.is_empty() {
                    unit.owner_start = cursor + 1;
                } else {
                    unit.post_list_operator = true;
                }
            }
            cursor += 1;
        }
    }
    unit.end = cursor;
    unit
}

/// Position after an exact shell heredoc terminator line.
fn find_heredoc_close(
    command: &[char],
    body_start: usize,
    delimiter: &str,
    strip_tabs: bool,
) -> Option<usize> {
    let mut cursor = body_start;
    loop {
        let newline = find_from(command, '\n', cursor);
        let after = newline.map_or(command.len(), |n| n + 1);
        let mut line: String = command[cursor.min(command.len())..after].iter().collect();
        if line.ends_with('\n') {
            line.pop();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        let candidate = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        if candidate == delimiter {
            return Some(after);
        }
        newline?;
        cursor = after;
    }
}

fn inert_body_ranges(command: &[char], consumer: &Regex) -> Vec<(usize, usize)> {
    let Some(last_opener) = (0..command.len().saturating_sub(1))
        .rev()
        .find(|&k| command[k] == '<' && command[k + 1] == '<')
    else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    let mut command_start = 0;
    while command_start <= last_opener {
        let unit = scan_command_unit(command, command_start);
        if unit.unknown {
            return Vec::new();
        }
        if unit.specs.is_empty() {
            if unit.end >= command.len() {
                break;
            }
            command_start = unit.end + 1;
            continue;
        }
        if unit.end >= command.len() {
            return Vec::new();
        }
        let mut body_cursor = unit.end + 1;
        let mut body_ranges = Vec::new();
        for (delimiter, strip_tabs, _) in &unit.specs {
            let Some(close_end) = find_heredoc_close(command, body_cursor, delimiter, *strip_tabs)
            else {
                return Vec::new();
            };
            body_ranges.push((body_cursor, close_end));
            body_cursor = close_end;
        }
        if unit.specs.iter().all(|(_, _, quoted)| *quoted) && !unit.post_list_operator {
            let masked_opener = mask_simple_quotes(&command[command_start..unit.end]);
            let masked_owner = mask_simple_quotes(&command[unit.owner_start..unit.end]);
            let markers = ["$(", "`", "<(", ">(", "(", ")", "{", "}"];
            if !markers.iter().any(|m| masked_opener.contains(m))
                && consumer.is_match(&masked_owner).unwrap_or(false)
            {
                ranges.extend(body_ranges);
            }
        }
        command_start = body_cursor;
    }
    ranges
}

/// Drop provably inert heredoc bodies (not their terminator lines). `code_consumers` also drops
/// python/osascript bodies, which are code: that copy may only be scanned IN ADDITION to the raw one.
pub fn mask_heredoc_bodies_for_shell_scan(command: &str, code_consumers: bool) -> String {
    if !command.contains("<<") {
        return command.to_owned();
    }
    let chars: Vec<char> = command.chars().collect();
    let consumer = if code_consumers {
        &*INERT_CONSUMER
    } else {
        &*DATA_CONSUMER
    };
    let bodies: Vec<(usize, usize)> = inert_body_ranges(&chars, consumer)
        .into_iter()
        .map(|(start, end)| {
            let last_newline = (start..end.saturating_sub(1).max(start))
                .rev()
                .find(|&k| chars[k] == '\n')
                .map_or(0, |k| k + 1);
            (start, start.max(last_newline))
        })
        .collect();
    let mut out = String::new();
    let mut previous = 0;
    for (start, end) in bodies {
        out.extend(&chars[previous..start]);
        previous = end;
    }
    out.extend(&chars[previous..]);
    out
}
