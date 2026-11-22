//! GNU `env -S` / `--split-string` projection (`approval_env_split.py`).

use super::wrappers::{command_wrapper_options_with_arg, short_bundle};

/// Project GNU env -S literal argv, not POSIX shell words; `None` on any malformed escape.
pub fn split_env_string(payload: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = payload.chars().collect();
    let mut args = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        index += 1;
        if c == '\\' {
            if index == chars.len() {
                return None;
            }
            let escaped = chars[index];
            if quote == Some('\'') && escaped != '\'' && escaped != '\\' {
                word.push(c);
                started = true;
                continue;
            }
            index += 1;
            if escaped == 'c' {
                if quote.is_some() {
                    return None;
                }
                break;
            }
            if escaped == '_' && quote.is_none() {
                if started {
                    args.push(std::mem::take(&mut word));
                }
                word.clear();
                started = false;
                continue;
            }
            let mapped = match escaped {
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\u{b}',
                '#' | '$' | '"' | '\'' | '\\' => escaped,
                '_' => ' ',
                _ => return None,
            };
            word.push(mapped);
            started = true;
            continue;
        }
        if (c == '\'' || c == '"') && (quote.is_none() || Some(c) == quote) {
            quote = if quote.is_none() { Some(c) } else { None };
            started = true;
            continue;
        }
        if quote.is_none() && matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}') {
            if started {
                args.push(std::mem::take(&mut word));
            }
            word.clear();
            started = false;
            continue;
        }
        if quote.is_none() && c == '#' && !started {
            break;
        }
        if c == '$' && quote != Some('\'') {
            return None;
        }
        word.push(c);
        started = true;
    }
    if quote.is_some() {
        return None;
    }
    if started {
        args.push(word);
    }
    Some(args)
}

/// `shlex.quote`.
pub fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_owned();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if s.chars().all(safe) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// `shlex.join`.
pub fn shlex_join(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|t| shlex_quote(t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The command string `env -S` would run, from the env argv tokens.
pub fn env_split_payload(tokens: &[String], depth: usize) -> Option<String> {
    let mut index = 1;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--" || !token.starts_with('-') {
            return None;
        }
        let (option, equals, value) = match token.split_once('=') {
            Some((o, v)) => (o, true, v),
            None => (token.as_str(), false, ""),
        };
        let attached: Option<String> = if option == "--split-string" {
            equals.then(|| value.to_owned())
        } else if token.starts_with("--") {
            let takes_value = !equals && command_wrapper_options_with_arg("env").contains(&option);
            index += if takes_value { 2 } else { 1 };
            continue;
        } else {
            let (_, owner, rest) = short_bundle("env", token);
            if owner.as_deref() != Some("-S") {
                index += if owner.is_some() && rest.is_empty() {
                    2
                } else {
                    1
                };
                continue;
            }
            (!rest.is_empty()).then_some(rest)
        };
        if attached.is_none() {
            index += 1;
        }
        let payload = match attached {
            Some(p) => p,
            None => tokens.get(index).cloned().unwrap_or_default(),
        };
        let args = split_env_string(&payload)?;
        let mut rebuilt = args;
        rebuilt.extend(tokens[(index + 1).min(tokens.len())..].iter().cloned());
        if rebuilt.first().is_some_and(|t| t.starts_with('-')) && depth < 8 {
            let mut nested_tokens = vec!["env".to_owned()];
            nested_tokens.extend(rebuilt.iter().cloned());
            if let Some(nested) = env_split_payload(&nested_tokens, depth + 1) {
                return Some(nested);
            }
        }
        return Some(shlex_join(&rebuilt));
    }
    None
}
