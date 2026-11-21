//! Keyword/value connection strings: ADO.NET, ODBC and JDBC `Host=db;Password=…` and libpq
//! `host=db password=…`. The value's extent is found by a scan, so that each grammar (ODBC
//! braces, doubled quotes, backslash escapes) is exact.

use std::sync::LazyLock;

use fancy_regex::Regex as Fancy;
use regex::Regex;

const KEYS: &str = r"host|server|data\s+source|address|addr|network\s+address|user\s*id|uid|user(?:name)?|database|dbname|hostaddr|initial\s+catalog|port";

static GATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:password|pwd)\s*=").expect("valid pattern"));
static PASSWORD_KEY: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r#"(?i)(?<![^\s;"'])(?:password|pwd)[ \t]*=[ \t]*"#).expect("valid pattern")
});
/// A password key counts only on a line that also carries a companion connection key.
static COMPANION: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(r#"(?i)(?<![^\s;"'])(?:{KEYS})[ \t]*="#)).expect("valid pattern")
});
/// A `;` followed by a known field marks the `;`-delimited grammar; libpq values may hold a bare `;`.
static SEMICOLON_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i);[ \t]*(?:{KEYS}|password|pwd|driver|encrypt|pooling|integrated\s+security|trusted_connection|trustservercertificate|sslmode|ssl\s+mode|timeout|connect(?:ion)?\s+timeout|application\s+name|search\s+path)[ \t]*="
    ))
    .expect("valid pattern")
});

/// A quote inside an unquoted value is literal unless it closes the string that wraps the
/// connection string: then a delimiter or an operator follows it.
fn closes_wrapper(line: &[char], j: usize) -> bool {
    j + 1 >= line.len() || " \t\r\n,;)]}>/.+&|".contains(line[j + 1])
}

/// End of the value starting at `line[i]`.
fn value_end(line: &[char], i: usize, semicolon: bool) -> usize {
    let n = line.len();
    if i >= n {
        return i;
    }
    let c = line[i];
    if semicolon {
        if c == '{' {
            let mut j = i + 1;
            while j < n {
                if line[j] == '}' {
                    if j + 1 < n && line[j + 1] == '}' {
                        j += 2;
                        continue;
                    }
                    return j + 1;
                }
                j += 1;
            }
            return n;
        }
        if c == '"' || c == '\'' {
            let mut j = i + 1;
            while j < n {
                if line[j] == c {
                    if j + 1 < n && line[j + 1] == c {
                        j += 2;
                        continue;
                    }
                    return j + 1;
                }
                j += 1;
            }
            return n;
        }
        let mut j = i;
        while j < n
            && !matches!(line[j], ';' | '\r' | '\n')
            && !(matches!(line[j], '"' | '\'') && closes_wrapper(line, j))
        {
            j += 1;
        }
        while j > i && matches!(line[j - 1], ' ' | '\t') {
            j -= 1;
        }
        return j;
    }
    if c == '"' || c == '\'' {
        let mut j = i + 1;
        while j < n {
            if line[j] == '\\' {
                j += 2;
                continue;
            }
            if line[j] == c {
                return j + 1;
            }
            j += 1;
        }
        return n;
    }
    let mut j = i;
    while j < n {
        if line[j] == '\\' {
            j += 2;
            continue;
        }
        if matches!(line[j], ' ' | '\t' | '\r' | '\n')
            || (matches!(line[j], '"' | '\'') && closes_wrapper(line, j))
        {
            break;
        }
        j += 1;
    }
    j.min(n)
}

static FORMAT_SLOT: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r"^\{\s*(?>(?:[A-Za-z0-9_](?:[^;{}@$&#!]|![rsa](?=\s*(?>[:}])))*)?)\}$")
        .expect("valid pattern")
});

fn redact_line(line: &str, code_file: bool) -> String {
    if !GATE.is_match(line) || !COMPANION.is_match(line).unwrap_or(false) {
        return line.to_owned();
    }
    let semicolon = SEMICOLON_FIELD.is_match(line);
    let chars: Vec<char> = line.chars().collect();
    let char_of = |byte: usize| line[..byte].chars().count();
    let mut out = String::new();
    let mut position = 0;
    let mut scanned = 0;
    let mut from = 0;
    while let Ok(Some(found)) = PASSWORD_KEY.find_from_pos(line, from) {
        from = if found.end() > found.start() {
            found.end()
        } else {
            found.end() + 1
        };
        let (start, end) = (char_of(found.start()), char_of(found.end()));
        if start < scanned {
            continue;
        }
        let value_start = end;
        let value_finish = value_end(&chars, value_start, semicolon);
        let value: String = chars[value_start..value_finish].iter().collect();
        // `the pwd= flag`: a space after `=` but not before it is prose, not an empty value.
        let equals = (start..end)
            .rev()
            .find(|&k| chars[k] == '=')
            .unwrap_or(start);
        let padded_after = matches!(chars[end - 1], ' ' | '\t');
        let padded_before = equals > 0 && matches!(chars[equals - 1], ' ' | '\t');
        if value.is_empty() || (!semicolon && padded_after && !padded_before) {
            continue;
        }
        scanned = value_finish;
        // A template, not a credential: `Password={0}`, `Password=${DB_PASS}`. ODBC braces a
        // literal value, so on a `;` line only a format slot or a `${…}` counts.
        let bare = value.trim_matches(['"', '\'']);
        if code_file
            && (bare.starts_with('{') || bare.starts_with("${"))
            && bare.ends_with('}')
            && (!semicolon || bare.starts_with("${") || FORMAT_SLOT.is_match(bare).unwrap_or(false))
            && !PASSWORD_KEY
                .is_match(
                    &line[line
                        .char_indices()
                        .nth(value_start)
                        .map_or(line.len(), |(i, _)| i)
                        ..line
                            .char_indices()
                            .nth(value_finish)
                            .map_or(line.len(), |(i, _)| i)],
                )
                .unwrap_or(false)
        {
            continue;
        }
        let first = chars[value_start];
        let masked = if first == '{' && semicolon {
            "{***}".to_owned()
        } else if (first == '"' || first == '\'')
            && value.chars().count() > 1
            && value.ends_with(first)
        {
            format!("{first}***{first}")
        } else {
            "***".to_owned()
        };
        out.extend(&chars[position..value_start]);
        out.push_str(&masked);
        position = value_finish;
    }
    if position == 0 && out.is_empty() {
        return line.to_owned();
    }
    out.extend(&chars[position..]);
    out
}

/// Python's `str.splitlines(keepends=True)`.
fn lines(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut iter = text.char_indices().peekable();
    while let Some((index, c)) = iter.next() {
        let breaks = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !breaks {
            continue;
        }
        let mut end = index + c.len_utf8();
        if c == '\r' && iter.peek().is_some_and(|(_, next)| *next == '\n') {
            iter.next();
            end += 1;
        }
        parts.push(&text[start..end]);
        start = end;
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

/// Mask `Password=` and `Pwd=` values in connection strings, one companion-key scan per line.
pub(super) fn redact_kv_connection_strings(text: &str, code_file: bool) -> String {
    if !GATE.is_match(text) {
        return text.to_owned();
    }
    lines(text)
        .into_iter()
        .map(|line| redact_line(line, code_file))
        .collect()
}
