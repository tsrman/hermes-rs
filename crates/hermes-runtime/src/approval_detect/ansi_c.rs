//! Rewrite bash/zsh ANSI-C quoting (`$'...'`) into plain single quotes for detection
//! (`approval_ansi_c.py`). Only escape sequences are decoded; nothing is executed.

/// Whether the `#` at `index` starts a comment: it must begin a word, and an escaped separator
/// (`echo \ #`) belongs to the previous word.
pub fn is_shell_comment_start(chars: &[char], index: usize) -> bool {
    if chars[index] != '#' || index == 0 {
        return chars[index] == '#';
    }
    let prev = chars[index - 1];
    if !(super::py_isspace(prev) || ";&|()<>".contains(prev)) {
        return false;
    }
    let mut backslashes = 0;
    let mut j = index;
    while j >= 2 && chars[j - 2] == '\\' {
        backslashes += 1;
        j -= 1;
    }
    backslashes % 2 == 0
}

fn hex_run(chars: &[char], from: usize, max: usize) -> usize {
    chars[from.min(chars.len())..]
        .iter()
        .take(max)
        .take_while(|c| c.is_ascii_hexdigit())
        .count()
}

fn named_escape(c: char) -> Option<char> {
    Some(match c {
        'a' => '\u{7}',
        'b' => '\u{8}',
        'e' | 'E' => '\u{1b}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        '?' => '?',
        _ => return None,
    })
}

/// `_ESCAPE_RE.sub(_decode_escape, body)`.
fn decode_escapes(body: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c != '\\' || i + 1 >= body.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let kind = body[i + 1];
        let raw = |end: usize| body[i..end].iter().collect::<String>();
        let hex_value = |digits: usize| {
            let s: String = body[i + 2..i + 2 + digits].iter().collect();
            u32::from_str_radix(&s, 16).unwrap_or(u32::MAX)
        };
        let width = |max: usize| (kind, hex_run(body, i + 2, max));
        let (end, text) = match width(2) {
            ('x', n) if n > 0 => {
                let end = i + 2 + n;
                (
                    end,
                    char::from_u32(hex_value(n)).map_or_else(|| raw(end), String::from),
                )
            }
            _ if kind.is_digit(8) => {
                let n = body[i + 1..]
                    .iter()
                    .take(3)
                    .take_while(|c| c.is_digit(8))
                    .count();
                let s: String = body[i + 1..i + 1 + n].iter().collect();
                let code = u32::from_str_radix(&s, 8).unwrap_or(0) & 0xFF;
                (
                    i + 1 + n,
                    char::from_u32(code).map(String::from).unwrap_or_default(),
                )
            }
            ('u', _) if hex_run(body, i + 2, 4) > 0 => {
                let n = hex_run(body, i + 2, 4);
                let end = i + 2 + n;
                (
                    end,
                    char::from_u32(hex_value(n)).map_or_else(|| raw(end), String::from),
                )
            }
            ('U', _) if hex_run(body, i + 2, 8) > 0 => {
                let n = hex_run(body, i + 2, 8);
                let end = i + 2 + n;
                (
                    end,
                    char::from_u32(hex_value(n)).map_or_else(|| raw(end), String::from),
                )
            }
            ('c', _) if i + 2 < body.len() => {
                let code = body[i + 2] as u32 & 0x1F;
                (
                    i + 3,
                    char::from_u32(code).map(String::from).unwrap_or_default(),
                )
            }
            _ => (
                i + 2,
                named_escape(kind).map_or_else(|| raw(i + 2), String::from),
            ),
        };
        out.push_str(&text);
        i = end;
    }
    out
}

/// Offset just past the closing quote of the `$'...'` at `start`; `None` if unterminated.
fn ansi_c_span_end(chars: &[char], start: usize) -> Option<usize> {
    let mut k = start + 2;
    while k < chars.len() {
        match chars[k] {
            '\\' if k + 1 < chars.len() => k += 2,
            '\\' => return None,
            '\'' => return Some(k + 1),
            _ => k += 1,
        }
    }
    None
}

fn single_quoted(body: &[char]) -> String {
    let decoded = decode_escapes(body);
    let literal = decoded.split('\0').next().unwrap_or("");
    format!("'{}'", literal.replace('\'', "'\"'\"'"))
}

fn skip_continuations(chars: &[char], mut i: usize) -> usize {
    while chars.get(i) == Some(&'\\') && chars.get(i + 1) == Some(&'\n') {
        i += 2;
    }
    i
}

struct Context {
    kind: char,
    parens: usize,
}

fn slice(chars: &[char], from: usize, to: usize) -> String {
    chars[from.min(chars.len())..to.min(chars.len())]
        .iter()
        .collect()
}

/// `command` with each shell-recognised `$'...'` span rewritten as `'decoded'`.
pub fn expand_ansi_c_quotes(command: &str) -> String {
    if !command.contains('$') {
        return command.to_owned();
    }
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::new();
    let mut stack = vec![Context {
        kind: 'u',
        parens: 0,
    }];
    let mut i = 0;
    while i < chars.len() {
        i = step(&chars, i, &mut stack, &mut out);
    }
    out
}

fn step(chars: &[char], i: usize, stack: &mut Vec<Context>, out: &mut String) -> usize {
    let ch = chars[i];
    let top = stack.len() - 1;
    if ch == '\\' && chars.get(i + 1) == Some(&'\n') {
        return i + 2;
    }
    if ch == '\\' {
        out.push_str(&slice(chars, i, i + 2));
        return i + 2;
    }
    let after = if ch == '$' {
        skip_continuations(chars, i + 1)
    } else {
        i + 1
    };
    let following = chars.get(after).copied();
    if ch == '$' && following == Some('$') {
        out.push_str("$$");
        return after + 1;
    }
    if stack[top].kind == '"' {
        if ch == '"' {
            stack.pop();
        } else if (ch == '$' && following == Some('(')) || ch == '`' {
            let (kind, text) = if ch == '$' { ('(', "$(") } else { ('`', "`") };
            stack.push(Context { kind, parens: 0 });
            out.push_str(text);
            return if ch == '$' { after + 1 } else { i + 1 };
        }
        out.push(ch);
        return i + 1;
    }
    if ch == '$' && following == Some('\'') {
        if let Some(end) = ansi_c_span_end(chars, after - 1) {
            out.push_str(&single_quoted(&chars[after + 1..end - 1]));
            return end;
        }
    }
    if ch == '$' && following == Some('"') {
        return after;
    }
    if ch == '\'' {
        let end = chars[i + 1..]
            .iter()
            .position(|c| *c == '\'')
            .map_or(chars.len(), |p| i + 1 + p + 1);
        out.push_str(&slice(chars, i, end));
        return end;
    }
    if ch == '#' && is_shell_comment_start(chars, i) {
        let end = chars[i..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(chars.len(), |p| i + p);
        out.push_str(&slice(chars, i, end));
        return end;
    }
    if ch == '"' {
        stack.push(Context {
            kind: '"',
            parens: 0,
        });
    } else if stack[top].kind == '`' && ch == '`' {
        stack.pop();
    } else if stack[top].kind == '(' && ch == '(' {
        stack[top].parens += 1;
    } else if stack[top].kind == '(' && ch == ')' {
        if stack[top].parens > 0 {
            stack[top].parens -= 1;
        } else {
            stack.pop();
        }
    }
    out.push(ch);
    i + 1
}
