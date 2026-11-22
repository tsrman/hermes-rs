//! The reference's custom (non-regex) dangerous-command matchers: pipe remote content to a shell,
//! `/dev/tcp` redirects, `wmic ... shadowcopy delete` and download-then-run
//! (`_PipeRemoteToShellMatcher`, `_DevSocketRedirectMatcher`, `approval_wmic.py`,
//! `approval_fetch_exec.py`). Patterns come from `dangerous_tables`, generated from the reference.

use std::collections::HashMap;
use std::sync::LazyLock;

use fancy_regex::{Captures, Regex};

use super::budget::{BudgetExceeded, Text};
use super::command_starts::{iter_shell_command_word_spans, shell_command_segment};
use super::dangerous_tables as t;
use super::hardline::{contains_shell_carrier, mask_quoted_prose};
use super::py_isspace;
use super::shell_words::{deobfuscate_shell_word_for_detection, read_shell_word};

type R<T> = Result<T, BudgetExceeded>;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("bad pattern {pattern}: {e}"))
}

fn full(pattern: &str) -> Regex {
    re(&format!(r"\A(?:{pattern})\z"))
}

fn search_err<T>(r: Result<T, fancy_regex::Error>) -> R<T> {
    r.map_err(|_| BudgetExceeded)
}

/// Byte offset of char index `i` in `s` (end of string when past it).
pub fn byte_at(s: &str, i: usize) -> usize {
    s.char_indices().nth(i).map_or(s.len(), |(b, _)| b)
}

/// Char index of byte offset `b` in `s`.
pub fn char_at(s: &str, b: usize) -> usize {
    s[..b.min(s.len())].chars().count()
}

/// `regex.match(text, pos)`: a match that starts exactly at byte `pos`.
fn match_at<'t>(regex: &Regex, text: &'t str, pos: usize) -> R<Option<Captures<'t, str>>> {
    let caps = search_err(regex.captures_from_pos(text, pos))?;
    Ok(caps.filter(|c| c.get(0).is_some_and(|m| m.start() == pos)))
}

// ---- pipe remote content to a shell ---------------------------------------------------------

static PIPE_BARE_SINK: LazyLock<Regex> = LazyLock::new(|| re(t::PIPE_BARE_SINK));
static PIPE_SINK: LazyLock<Regex> = LazyLock::new(|| re(t::PIPE_SINK));
static PIPE_LOOKUP_ONLY: LazyLock<Regex> = LazyLock::new(|| re(t::PIPE_LOOKUP_ONLY));
static PIPE_FETCH: LazyLock<Regex> = LazyLock::new(|| re(t::PIPE_FETCH));
static PIPE_OPERATOR: LazyLock<Regex> = LazyLock::new(|| re(t::PIPE_OPERATOR));
static PIPE_ONLY_OPERATOR: LazyLock<Regex> = LazyLock::new(|| re(t::PIPE_ONLY_OPERATOR));

fn pipe_scan(
    text: &str,
    mut pos: usize,
    sink: &Regex,
    operator: &Regex,
    lookup_guard: bool,
) -> R<bool> {
    while let Some(op) = search_err(operator.find_from_pos(text, pos))? {
        if match_at(sink, text, op.start())?.is_some()
            && !(lookup_guard && match_at(&PIPE_LOOKUP_ONLY, text, op.end())?.is_some())
        {
            return Ok(true);
        }
        pos = if op.end() > pos { op.end() } else { pos + 1 };
        if pos > text.len() {
            break;
        }
    }
    Ok(false)
}

/// `curl | sh`: the first fetch, then a sink at each later `|` / `>(`.
pub fn pipe_remote_to_shell(text: &str) -> R<bool> {
    let Some(fetch) = search_err(PIPE_FETCH.find(text))? else {
        return Ok(false);
    };
    if pipe_scan(text, fetch.end(), &PIPE_BARE_SINK, &PIPE_OPERATOR, false)? {
        return Ok(true);
    }
    let widened = if contains_shell_carrier(text)? {
        text.to_owned()
    } else {
        mask_quoted_prose(text)?
    };
    let from = byte_at(&widened, char_at(text, fetch.end()));
    pipe_scan(&widened, from, &PIPE_SINK, &PIPE_ONLY_OPERATOR, true)
}

// ---- /dev/tcp redirects ---------------------------------------------------------------------

static DEV_SOCKET: LazyLock<Regex> = LazyLock::new(|| re(t::DEV_SOCKET));
static DEV_SOCKET_OP: LazyLock<Regex> = LazyLock::new(|| re(t::DEV_SOCKET_REDIRECT_OP));

/// `... > /dev/tcp/h/p` reverse shells; the operand is dequoted from the real text.
pub fn dev_socket_redirect(text: &str) -> R<bool> {
    let masked = mask_quoted_prose(text)?;
    let real = Text::new(text);
    let mut pos = 0;
    while let Some(op) = search_err(DEV_SOCKET_OP.find_from_pos(&masked, pos))? {
        let (_, _, word) = read_shell_word(&real, char_at(&masked, op.end()))?;
        let word = deobfuscate_shell_word_for_detection(&word)?;
        if match_at(&DEV_SOCKET, &word, 0)?.is_some() {
            return Ok(true);
        }
        pos = if op.end() > pos { op.end() } else { pos + 1 };
        if pos > masked.len() {
            break;
        }
    }
    Ok(false)
}

// ---- wmic shadow copy delete ------------------------------------------------------------------

static WMIC_DELETE: LazyLock<Regex> = LazyLock::new(|| re(t::WMIC_DELETE_IN_SEGMENT));
static WMIC_SWITCHES: LazyLock<Regex> = LazyLock::new(|| re(t::WMIC_CARRIER_SWITCHES));
static WMIC_TITLE: LazyLock<Regex> = LazyLock::new(|| re(t::WMIC_START_TITLE));
const WMIC_CARRIERS: [&str; 9] = [
    "cmd",
    "pwsh",
    "powershell",
    "start",
    "call",
    "doas",
    "wsl",
    "xargs",
    "ssh",
];

fn replace_first(regex: &Regex, text: &str, with: &str) -> String {
    regex.replacen(text, 1, with).into_owned()
}

fn carrier_script(name: &str, rest: &str) -> Vec<String> {
    static COMMAND_OPT: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^-(?:Command|c)\b\s*"));
    static HOST: LazyLock<Regex> = LazyLock::new(|| re(r"^[^\s;&|]+"));
    let mut rest = replace_first(&WMIC_SWITCHES, rest, "");
    if name == "start" {
        rest = replace_first(&WMIC_SWITCHES, &replace_first(&WMIC_TITLE, &rest, ""), "");
    }
    rest = rest.trim_start_matches(py_isspace).to_owned();
    if name == "ssh" {
        rest = replace_first(&HOST, &rest, "");
        rest = replace_first(&WMIC_SWITCHES, &rest, "")
            .trim_start_matches(py_isspace)
            .to_owned();
    }
    rest = replace_first(&COMMAND_OPT, &rest, "");
    let rest = rest.trim_end_matches(py_isspace).replace("\\\"", "\"");
    let first = rest.chars().next();
    if first != Some('\'') && first != Some('"') {
        return vec![rest];
    }
    let chars: Vec<char> = rest.chars().collect();
    let inner: String = if chars.len() > 1 && chars[chars.len() - 1] == chars[0] {
        chars[1..chars.len() - 1].iter().collect()
    } else {
        chars[1..].iter().collect()
    };
    vec![rest.clone(), inner]
}

/// `wmic ... shadowcopy ... delete` run as a command, also inside carriers (cmd /c, pwsh, ssh ...).
pub fn wmic_shadow_delete(text: &str, depth: usize) -> R<bool> {
    if !text.to_lowercase().contains("wmic") {
        return Ok(false);
    }
    let parsed = Text::new(text);
    for (_, end, word) in iter_shell_command_word_spans(&parsed)? {
        let last = word.rsplit(['\\', '/']).next().unwrap_or("");
        let name = deobfuscate_shell_word_for_detection(last)?;
        let name = name.trim_matches(['"', '\'']).to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        let segment = shell_command_segment(&parsed, end)?;
        if name == "wmic" {
            if search_err(WMIC_DELETE.is_match(&format!(" {segment}")))? {
                return Ok(true);
            }
        } else if WMIC_CARRIERS.contains(&name) && depth < 3 {
            for script in carrier_script(name, &segment) {
                if wmic_shadow_delete(&script, depth + 1)? {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

// ---- download then run ------------------------------------------------------------------------

static FETCH: LazyLock<Regex> = LazyLock::new(|| re(t::FETCH));
static CAT_WRITER: LazyLock<Regex> = LazyLock::new(|| re(r"\bcat\b"));
static SEGMENT_END: LazyLock<Regex> = LazyLock::new(|| re(t::SEGMENT_END));
static TARGET: LazyLock<Regex> = LazyLock::new(|| re(t::TARGET));
static REDIRECT: LazyLock<Regex> = LazyLock::new(|| re(t::REDIRECT));
static ARGS: LazyLock<Regex> = LazyLock::new(|| re(t::ARGS));
static STDIN: LazyLock<Regex> = LazyLock::new(|| re(t::STDIN));
static LOOKUP_ONLY: LazyLock<Regex> = LazyLock::new(|| re(t::LOOKUP_ONLY));
static CURL_REMOTE_NAME: LazyLock<Regex> = LazyLock::new(|| re(t::CURL_REMOTE_NAME));
static URL: LazyLock<Regex> = LazyLock::new(|| re(t::URL));
static PS_READ_RUN: LazyLock<Regex> = LazyLock::new(|| re(t::PS_READ_RUN));
static RUN: LazyLock<Regex> = LazyLock::new(|| re(t::DOWNLOAD_RUN));
static READ_RUN: LazyLock<Regex> = LazyLock::new(|| re(t::DOWNLOAD_READ_RUN));
const SHELL_NAMES: [&str; 5] = ["bash", "sh", "zsh", "ksh", "dash"];

fn base(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_owned()
}

fn check_flag(tool: &str) -> Option<&'static str> {
    if SHELL_NAMES.contains(&tool) {
        return Some(t::SHELL_CHECK_FLAG);
    }
    t::CHECK_ONLY_FLAGS
        .iter()
        .find(|(n, _)| *n == tool)
        .map(|(_, p)| *p)
}

fn lookup<'a>(table: &'a [(&str, &str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(n, _)| *n == key).map(|(_, p)| *p)
}

fn fullmatch(pattern: &str, text: &str) -> bool {
    full(pattern).is_match(text).unwrap_or(false)
}

fn split_words(text: &str) -> Vec<String> {
    text.split(py_isspace)
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

fn split_args(tool: &str, words: &[String]) -> (Vec<String>, Vec<String>) {
    let shell = check_flag(tool) == Some(t::SHELL_CHECK_FLAG);
    let value = if shell {
        t::SHELL_VALUE_FLAG
    } else {
        let key = if tool.starts_with("python") {
            "python"
        } else {
            tool
        };
        lookup(t::VALUE_FLAGS, key).unwrap_or("(?!)")
    };
    let option = if shell {
        r"[-+]\S*"
    } else {
        lookup(t::OPTION_WORDS, tool).unwrap_or(r"-\S*")
    };
    let (mut flags, mut i) = (Vec::new(), 0);
    while i < words.len() && fullmatch(option, &words[i]) {
        flags.push(words[i].clone());
        i += if fullmatch(value, &words[i]) { 2 } else { 1 };
    }
    (flags, words[i.min(words.len())..].to_vec())
}

fn check_only(tool: &str, flags: &[String]) -> bool {
    match check_flag(tool) {
        Some(check) if !flags.is_empty() => flags.iter().all(|f| fullmatch(check, f)),
        _ => false,
    }
}

fn group<'t>(caps: &Captures<'t, str>, name: &str) -> Option<&'t str> {
    caps.name(name).map(|m| m.as_str())
}

fn syntax_check_only(run: &Captures<'_, str>) -> bool {
    let tool = group(run, "tool").unwrap_or("").to_lowercase();
    let flags = group(run, "flags").unwrap_or("");
    match check_flag(&tool) {
        Some(check) if !flags.is_empty() => {
            let words = split_words(flags);
            !words.is_empty() && words.iter().all(|f| fullmatch(check, f))
        }
        _ => false,
    }
}

fn operand_ran(run: &Captures<'_, str>, text: &str, targets: &HashMap<String, usize>) -> R<bool> {
    let tool = group(run, "tool").unwrap_or("").to_lowercase();
    let prog_end = run.name("prog").map_or(0, |m| m.end());
    let args = match_at(&ARGS, text, prog_end)?
        .and_then(|c| c.get(0).map(|m| m.as_str().to_owned()))
        .unwrap_or_default();
    let cleaned = REDIRECT.replace_all(&args, " ").into_owned();
    let (flags, rest) = split_args(&tool, &split_words(&cleaned));
    let stdin_operand =
        search_err(STDIN.captures(&args))?.and_then(|c| c.get(1).map(|m| m.as_str().to_owned()));
    let operand = rest.first().cloned().or(stdin_operand);
    let Some(operand) = operand else {
        return Ok(false);
    };
    if check_only(&tool, &flags) {
        return Ok(false);
    }
    let name = base(&operand.trim_matches(['"', '\'']).replace('\\', "/"));
    let start = run.get(0).map_or(0, |m| m.start());
    Ok(start >= targets.get(&name).copied().unwrap_or(text.len()))
}

fn runs_content(read: &Captures<'_, str>) -> bool {
    let tool = group(read, "rtool")
        .or_else(|| group(read, "ptool"))
        .unwrap_or("")
        .to_lowercase();
    let raw_flags = group(read, "rflags")
        .or_else(|| group(read, "pargs"))
        .unwrap_or("");
    let cleaned = REDIRECT.replace_all(raw_flags, " ").into_owned();
    let (flags, rest) = split_args(&tool, &split_words(&cleaned));
    if check_only(&tool, &flags) {
        return false;
    }
    let shell = check_flag(&tool) == Some(t::SHELL_CHECK_FLAG);
    if group(read, "piped").is_none() {
        return rest.is_empty();
    }
    rest.is_empty()
        || flags
            .iter()
            .any(|f| f == "-" || (shell && fullmatch(r"-[a-zA-Z]*s[a-zA-Z]*", f)))
}

fn windowed_targets(text: &str, from: usize, to: usize) -> R<Vec<(String, usize)>> {
    let window = &text[..to];
    let mut out = Vec::new();
    let mut pos = from;
    while pos <= window.len() {
        let Some(caps) = search_err(TARGET.captures_from_pos(window, pos))? else {
            break;
        };
        let whole = caps.get(0).expect("group 0");
        out.push((base(caps.get(1).map_or("", |m| m.as_str())), whole.end()));
        pos = if whole.end() > pos {
            whole.end()
        } else {
            pos + 1
        };
    }
    Ok(out)
}

/// Whether a curl/wget (or, with `cat_writer`, a `cat`) writes FILE and a later command in the same
/// string runs that FILE.
pub fn download_then_run(text: &str, cat_writer: bool) -> R<bool> {
    let writer: &Regex = if cat_writer { &CAT_WRITER } else { &FETCH };
    let mut targets: HashMap<String, usize> = HashMap::new();
    let mut pos = 0;
    while let Some(fetch) = search_err(writer.find_from_pos(text, pos))? {
        let seg_end = search_err(SEGMENT_END.find_from_pos(text, fetch.end()))?
            .map_or(text.len(), |m| m.start());
        for (name, end) in windowed_targets(text, fetch.end(), seg_end)? {
            targets.entry(name).or_insert(end);
        }
        if fetch.as_str().to_lowercase() == "curl" {
            let window = &text[..seg_end];
            let mut p = fetch.end();
            let mut remote = false;
            while p <= window.len() {
                let Some(c) = search_err(CURL_REMOTE_NAME.captures_from_pos(window, p))? else {
                    break;
                };
                let whole = c.get(0).expect("group 0");
                if c.get(1).is_none_or(|g| g.as_str().contains('O')) {
                    remote = true;
                    break;
                }
                p = if whole.end() > p { whole.end() } else { p + 1 };
            }
            if remote {
                let mut p = fetch.end();
                while p <= window.len() {
                    let Some(m) = search_err(URL.find_from_pos(window, p))? else {
                        break;
                    };
                    let name = m
                        .as_str()
                        .split('?')
                        .next()
                        .unwrap_or("")
                        .split('#')
                        .next()
                        .unwrap_or("")
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    if !name.is_empty() {
                        targets.entry(name).or_insert(seg_end);
                    }
                    p = if m.end() > p { m.end() } else { p + 1 };
                }
            }
        }
        pos = seg_end;
    }
    let Some(&min_pos) = targets.values().min() else {
        return Ok(false);
    };
    let ran_before =
        |start: usize, name: &str| start >= targets.get(name).copied().unwrap_or(text.len());
    let mut p = min_pos;
    while p <= text.len() {
        let Some(run) = search_err(RUN.captures_from_pos(text, p))? else {
            break;
        };
        let whole = run.get(0).expect("group 0");
        let prog = group(&run, "prog")
            .unwrap_or("")
            .trim_matches(['"', '\''])
            .to_owned();
        let prog_ran = prog.contains('/') && ran_before(whole.start(), &base(&prog));
        let file = group(&run, "file").unwrap_or("");
        let mut file_ran = ran_before(whole.start(), &base(file)) && !syntax_check_only(&run);
        file_ran = file_ran
            || (group(&run, "tool").is_some_and(|t| !t.is_empty())
                && operand_ran(&run, text, &targets)?);
        if (prog_ran || file_ran) && !search_err(LOOKUP_ONLY.is_match(whole.as_str()))? {
            return Ok(true);
        }
        p = if whole.end() > p { whole.end() } else { p + 1 };
    }
    let mut p = min_pos;
    while p <= text.len() {
        let Some(read) = search_err(READ_RUN.captures_from_pos(text, p))? else {
            break;
        };
        let whole = read.get(0).expect("group 0");
        let file = group(&read, "file")
            .or_else(|| group(&read, "piped"))
            .unwrap_or("");
        if ran_before(whole.start(), &base(file)) && runs_content(&read) {
            return Ok(true);
        }
        p = if whole.end() > p { whole.end() } else { p + 1 };
    }
    let mut p = min_pos;
    while p <= text.len() {
        let Some(read) = search_err(PS_READ_RUN.captures_from_pos(text, p))? else {
            break;
        };
        let whole = read.get(0).expect("group 0");
        let file = ["iex", "block", "piped", "sourced"]
            .iter()
            .find_map(|n| group(&read, n))
            .unwrap_or("");
        if ran_before(whole.start(), &base(&file.replace('\\', "/"))) {
            return Ok(true);
        }
        p = if whole.end() > p { whole.end() } else { p + 1 };
    }
    Ok(false)
}
