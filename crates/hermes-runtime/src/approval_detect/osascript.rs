//! AppleScript and JXA (`osascript`) classification for the dangerous-command layer
//! (`approval_applescript.py`).
//!
//! The check fails closed instead of parsing AppleScript. An `osascript` run is flagged when its
//! program text holds a verb that can discard work, mutate files, synthesize input or reach a shell
//! (also in string literals and comments: a prompt costs less than a lost document), when the program
//! is JXA, when the program is not visible on the command line (script file, stdin, expansion, an
//! unquoted heredoc that expands), or when `osascript` is run through a carrier (`sh -c`, `xargs`,
//! `ssh`, an interpreter's `os.system`). A literal, read-only `-e` program is not flagged.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use fancy_regex::Regex as FancyRegex;
use regex::Regex;

use super::ansi_c::expand_ansi_c_quotes;
use super::budget::{BudgetExceeded, Text};
use super::command_starts::{iter_shell_command_word_spans, shell_command_segment};
use super::exec_findings::interpreter_family;
use super::py_isspace;
use super::shell_scan::{scan_shell, Kind, ScanOpts};
use super::shell_tokens::shell_segment_tokens;
use super::shell_words::deobfuscate_shell_word_for_detection;

pub const RISKY_DESCRIPTION: &str = "AppleScript can close or quit apps without saving, delete or move files, send keystrokes, or run shell commands (osascript)";
pub const OPAQUE_DESCRIPTION: &str =
    "osascript program comes from a script file, pipe, carrier or expansion and cannot be reviewed";

fn program_value_options(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "osascript" => Some(&["-l", "-s"]),
        "osacompile" => Some(&["-l", "-o", "-r", "-t", "-c"]),
        _ => None,
    }
}

static RISKY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\b(?:close|quit|delete|erase|trash|move|write|save|revert|empty|keystroke|click|restart|eject|kill)\b",
        r"|\bset\s+(?:the\s+)?(?:(?:every|first|second|third|fourth|fifth|sixth|seventh|eighth|ninth|tenth",
        r"|last|front|back|middle|some|-?\d+(?:st|nd|rd|th))\s+)?",
        r"(?:contents|text|value|data|paragraphs?|words?|characters?|attribute\s+runs?)\b",
        r"|\bshut\s*down\b|\blog\s*out\b|\bkey\s*code\b|\bperform\s+action\b|\bdon.?t\s+save\b",
        r"|\bdo\s+(?:shell\s+)?script\b|\b(?:run|load|store)\s+script\b|\bdo\s+javascript\b|\bexecute\b",
        r"|\bopen\s+for\s+access\b|\bset\s+eof\b|\breplacing\b|\bsaving\s+no\b|\bwithout\s+saving\b",
        r"|«",
    ))
    .expect("valid pattern")
});

static OSASCRIPT_WORD: LazyLock<FancyRegex> = LazyLock::new(|| {
    FancyRegex::new(r"(?i)(?<![\w.-])osa(?:script|compile)(?![\w-]|\.(?!exe\b)\w)")
        .expect("valid pattern")
});

fn mentions_osascript(text: &str) -> bool {
    let plain: String = text
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | '\\'))
        .collect();
    OSASCRIPT_WORD.is_match(&plain).unwrap_or(false)
}

/// Executable basename of a command word (`/usr/bin/osascript`, `o\sascript`, `osa''script`).
fn program_name(word: &str) -> Result<String, BudgetExceeded> {
    let last = word.rsplit(['\\', '/']).next().unwrap_or("");
    let deob = deobfuscate_shell_word_for_detection(word)?;
    let mut names: BTreeSet<String> = BTreeSet::new();
    for candidate in [
        deobfuscate_shell_word_for_detection(last)?,
        deob.rsplit('/').next().unwrap_or("").to_owned(),
    ] {
        let lowered = candidate.trim_matches(['"', '\'']).to_lowercase();
        names.insert(lowered.strip_suffix(".exe").unwrap_or(&lowered).to_owned());
    }
    Ok(names
        .iter()
        .find(|n| program_value_options(n).is_some())
        .or_else(|| names.iter().next())
        .cloned()
        .unwrap_or_default())
}

/// AppleScript layout replaced by spaces: `¬` continuations, nested `(* *)` and `--` / `#` comments.
fn strip_layout(program: &str) -> String {
    static LAYOUT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?s)"(?:\\.|[^"\\])*"?|\|[^|]*\|?|\(\*|--[^\n]*|#[^\n]*|¬"#)
            .expect("valid pattern")
    });
    static BLOCK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\(\*|\*\)").expect("valid pattern"));
    let mut out = String::new();
    let mut i = 0;
    while let Some(m) = LAYOUT.find_at(program, i) {
        out.push_str(&program[i..m.start()]);
        let token = m.as_str();
        i = m.end();
        if token.starts_with(['"', '|']) {
            out.push_str(token);
            continue;
        }
        out.push(' ');
        let mut depth = usize::from(token == "(*");
        while depth > 0 {
            let Some(inner) = BLOCK.find_at(program, i) else {
                break;
            };
            if inner.as_str() == "(*" {
                depth += 1;
            } else {
                depth -= 1;
            }
            i = inner.end();
        }
        if depth > 0 {
            return out;
        }
    }
    out.push_str(&program[i..]);
    out
}

/// Whether the command `name` executes a program named in `args` (a carrier).
fn runs_its_arguments(name: &str, args: &str) -> bool {
    static FIND: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|\s)-(?:exec|execdir|ok|okdir)\b").expect("valid"));
    static ENV: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|\s)(?:-\w*S|--split-string)").expect("valid"));
    match name {
        "find" => FIND.is_match(args),
        "env" => ENV.is_match(args),
        _ => {
            const REAL: [&str; 30] = [
                "sh",
                "bash",
                "zsh",
                "ksh",
                "dash",
                "fish",
                "csh",
                "tcsh",
                "ash",
                "eval",
                "source",
                ".",
                "xargs",
                "parallel",
                "ssh",
                "doas",
                "su",
                "watch",
                "script",
                "launchctl",
                "caffeinate",
                "arch",
                "sandbox-exec",
                "unbuffer",
                "perl",
                "ruby",
                "php",
                "node",
                "nodejs",
                "automator",
            ];
            REAL.contains(&name) || interpreter_family(name).is_some()
        }
    }
}

/// True when `raw` holds `$` or a backtick the shell expands (anything outside single quotes).
fn expands(raw: &str) -> Result<bool, BudgetExceeded> {
    let text = Text::new(raw);
    for step in scan_shell(&text, 0, None, ScanOpts::default()) {
        let step = step?;
        if step.kind != Kind::Quote
            && step.quote != Some('\'')
            && matches!(text.chars[step.i], '$' | '`')
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `args` with each output-redirect target that only expands parameters blanked.
fn mask_redirect_targets(args: &str) -> Result<String, BudgetExceeded> {
    let text = Text::new(args);
    let chars = &text.chars;
    let mut steps: Vec<(usize, usize, Option<char>)> = Vec::new();
    for step in scan_shell(&text, 0, None, ScanOpts::default()) {
        let step = step?;
        steps.push((step.i, step.j.unwrap_or(step.i + 1), step.quote));
    }
    let mut out = chars.clone();
    let mut k = 0;
    while k < steps.len() {
        let (i, _, quote) = steps[k];
        k += 1;
        if quote.is_some() || chars[i] != '>' {
            continue;
        }
        while k < steps.len() && steps[k].2.is_none() && ">|&".contains(chars[steps[k].0]) {
            k += 1;
        }
        while k < steps.len() && steps[k].2.is_none() && " \t".contains(chars[steps[k].0]) {
            k += 1;
        }
        let first = k;
        while k < steps.len()
            && !(steps[k].2.is_none() && " \t\n;&|<>()".contains(chars[steps[k].0]))
        {
            k += 1;
        }
        if first == k {
            continue;
        }
        let (start, end) = (steps[first].0, steps[k - 1].1);
        let word: String = chars[start..(end + 1).min(chars.len())].iter().collect();
        if !word.contains("$(") && !word.contains('`') {
            for c in &mut out[start..end] {
                *c = ' ';
            }
        }
    }
    Ok(out.into_iter().collect())
}

type Heredoc = (String, bool, bool);

/// `(delimiter, quoted, strip_tabs)` of the first unquoted `<<` operator in `args`.
fn heredoc_operator(args: &str) -> Result<Option<Heredoc>, BudgetExceeded> {
    static HEREDOC: LazyLock<FancyRegex> = LazyLock::new(|| {
        FancyRegex::new(
            r#"<<(?P<dash>-?)[ \t]*(?P<q>['"]?)(?P<delim>[^\s'"<>;&|()]+)\k<q>(?=[\s;&|<>)]|$)"#,
        )
        .expect("valid pattern")
    });
    let text = Text::new(args);
    let chars = &text.chars;
    for step in scan_shell(&text, 0, None, ScanOpts::default()) {
        let step = step?;
        let i = step.i;
        let starts = chars.get(i) == Some(&'<') && chars.get(i + 1) == Some(&'<');
        if step.kind == Kind::Char
            && step.quote.is_none()
            && starts
            && chars.get(i + 2) != Some(&'<')
            && (i == 0 || chars[i - 1] != '<')
        {
            let rest: String = chars[i..].iter().collect();
            let caps = HEREDOC.captures(&rest).ok().flatten();
            return Ok(caps
                .filter(|c| c.get(0).is_some_and(|m| m.start() == 0))
                .map(|c| {
                    let group = |name: &str| c.name(name).map_or("", |m| m.as_str());
                    (
                        group("delim").to_owned(),
                        !group("q").is_empty(),
                        !group("dash").is_empty(),
                    )
                }));
        }
    }
    Ok(None)
}

/// `(body, (start, end))` (char offsets) of the heredoc that starts on the line after `after`.
fn heredoc_body(
    text: &[char],
    after: usize,
    delim: &str,
    strip_tabs: bool,
) -> (String, (usize, usize)) {
    let len = text.len();
    let Some(newline) = (after..len).find(|&k| text[k] == '\n') else {
        return (String::new(), (len, len));
    };
    let body_start = newline + 1;
    let tabs = if strip_tabs { r"\t*" } else { "" };
    let pattern = format!("(?m)^{tabs}{}$", fancy_regex::escape(delim));
    let haystack: String = text.iter().collect();
    let byte_start: usize = text[..body_start.min(len)]
        .iter()
        .map(|c| c.len_utf8())
        .sum();
    let found = FancyRegex::new(&pattern)
        .ok()
        .and_then(|re| re.find_from_pos(&haystack, byte_start).ok().flatten());
    let char_of = |byte: usize| haystack[..byte].chars().count();
    match found {
        None => (
            text[body_start.min(len)..].iter().collect(),
            (body_start, len),
        ),
        Some(m) => (
            text[body_start..char_of(m.start())].iter().collect(),
            (body_start, char_of(m.end())),
        ),
    }
}

struct Program {
    text: String,
    opaque: bool,
    jxa: bool,
    heredoc: Option<Heredoc>,
}

/// The program of one `osascript` / `osacompile` argv tail, read from the shell-decoded words.
fn osascript_program(args: &str, value_options: &[&str]) -> Result<Program, BudgetExceeded> {
    let Some(tokens) = shell_segment_tokens(args, 0) else {
        return Ok(Program {
            text: String::new(),
            opaque: true,
            jxa: false,
            heredoc: None,
        });
    };
    let text = Text::new(args);
    let chars = &text.chars;
    let mut pieces: Vec<String> = Vec::new();
    let mut heredoc: Option<Heredoc> = None;
    let mut jxa = false;
    let mut opaque = false;
    for step in scan_shell(&text, 0, None, ScanOpts::default()) {
        let step = step?;
        let i = step.i;
        if step.kind == Kind::Char
            && step.quote.is_none()
            && chars[i] == '<'
            && chars.get(i + 1) != Some(&'<')
            && !(i > 0 && chars[i - 1] == '<')
        {
            opaque = true;
        }
    }
    let finish = |pieces: &[String], opaque, jxa, heredoc| Program {
        text: pieces.join(" "),
        opaque,
        jxa,
        heredoc,
    };
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        let next = tokens.get(i + 1).map_or("", String::as_str);
        if token.chars().all(py_isspace) {
            i += 1;
            continue;
        }
        let is_digits = !token.is_empty() && token.chars().all(|c| c.is_ascii_digit());
        if matches!(token, ">" | ">>" | ">|" | ">&")
            || ((is_digits || token == "&") && next.starts_with('>'))
        {
            i += if token.starts_with('>') {
                if next == "&" || next == "|" {
                    3
                } else {
                    2
                }
            } else {
                1
            };
            continue;
        }
        if token == "<<<" {
            pieces.push(next.to_owned());
            i += 2;
            continue;
        }
        if token == "<<" {
            if heredoc.is_some() {
                opaque = true;
            }
            heredoc = heredoc_operator(args)?;
            if heredoc.is_none() {
                opaque = true;
            }
            i += if next == "-" { 3 } else { 2 };
            continue;
        }
        if token == "<" || token == "<>" {
            return Ok(finish(&pieces, true, jxa, heredoc));
        }
        let long = token.starts_with("-l") && token.chars().count() > 2;
        if token == "-e" || value_options.contains(&token) || long {
            if token == "-e" {
                pieces.push(next.to_owned());
            }
            let language = if token == "-l" {
                next.to_owned()
            } else if token.starts_with("-l") {
                token.chars().skip(2).collect()
            } else {
                String::new()
            };
            jxa = jxa || (!language.is_empty() && language.to_lowercase() != "applescript");
            i += if token.chars().count() > 2 { 1 } else { 2 };
            continue;
        }
        if token.starts_with("-e") && token.chars().count() > 2 {
            pieces.push(token.chars().skip(2).collect());
        } else if !token.starts_with('-') && (!pieces.is_empty() || heredoc.is_some()) {
            break;
        } else if token.starts_with("-i") || token == "-" || !token.starts_with('-') {
            return Ok(finish(&pieces, true, jxa, heredoc));
        }
        i += 1;
    }
    if pieces.is_empty() && heredoc.is_none() {
        opaque = true;
    }
    Ok(Program {
        text: pieces.join("\n"),
        opaque,
        jxa,
        heredoc,
    })
}

fn classify_reading(text: &str) -> Result<BTreeSet<&'static str>, BudgetExceeded> {
    let mut found = BTreeSet::new();
    if !mentions_osascript(text) {
        return Ok(found);
    }
    let parsed = Text::new(text);
    let mut body_span = (usize::MAX, 0usize);
    for (start, end, word) in iter_shell_command_word_spans(&parsed)? {
        if body_span.0 != usize::MAX && body_span.0 <= start && start < body_span.1 {
            continue;
        }
        let name = program_name(&word)?;
        let segment = shell_command_segment(&parsed, start)?;
        let args: String = segment.chars().skip(end - start).collect();
        let Some(value_options) = program_value_options(&name) else {
            // The executable is chosen at run time (`X=osascript; $X`), or a carrier runs a program
            // that names osascript (`sh -c "osascript ..."`, `xargs osascript`).
            let chosen_at_run_time =
                expands(&word)? && (mentions_osascript(&segment) || word.starts_with(['$', '`']));
            if chosen_at_run_time || (mentions_osascript(&args) && runs_its_arguments(&name, &args))
            {
                found.insert(OPAQUE_DESCRIPTION);
            }
            continue;
        };
        let mut program = osascript_program(&args, value_options)?;
        if let Some((delim, quoted, strip_tabs)) = program.heredoc.clone() {
            let (body, span) = heredoc_body(
                &parsed.chars,
                start + segment.chars().count(),
                &delim,
                strip_tabs,
            );
            body_span = span;
            static EXPANDING: LazyLock<FancyRegex> =
                LazyLock::new(|| FancyRegex::new(r"(?<!\\)(?:\\\\)*[$`]").expect("valid pattern"));
            program.opaque =
                program.opaque || (!quoted && EXPANDING.is_match(&body).unwrap_or(false));
            program.text.push('\n');
            program.text.push_str(&body);
        }
        if program.opaque || expands(&mask_redirect_targets(&args)?)? {
            found.insert(OPAQUE_DESCRIPTION);
        }
        if program.jxa
            || RISKY.is_match(&program.text)
            || RISKY.is_match(&strip_layout(&program.text))
        {
            found.insert(RISKY_DESCRIPTION);
        }
    }
    Ok(found)
}

/// Descriptions that apply to `text`; empty when every `osascript` run is a visible, read-only
/// program. The raw text, the `$'...'`-decoded text and each one backslash-newline-joined are read
/// and the findings united (a variant only adds findings).
pub fn classify_osascript(text: &str) -> Result<BTreeSet<&'static str>, BudgetExceeded> {
    static JOIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\\r?\n").expect("valid pattern"));
    let mut readings: Vec<String> = vec![text.to_owned()];
    let decoded = expand_ansi_c_quotes(text);
    if decoded != text {
        readings.push(decoded);
    }
    let joined: Vec<String> = readings
        .iter()
        .map(|r| JOIN.replace_all(r, "").into_owned())
        .collect();
    for reading in joined {
        if !readings.contains(&reading) {
            readings.push(reading);
        }
    }
    let mut all = BTreeSet::new();
    for reading in &readings {
        all.extend(classify_reading(reading)?);
    }
    Ok(all)
}
