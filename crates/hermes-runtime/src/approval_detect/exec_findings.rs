//! Scoped execution mechanisms and the payloads they carry (`_execution_flag_findings`,
//! `_eval_payloads`, `_nested_payloads` and the option grammars behind them).

use std::sync::LazyLock;

use regex::Regex;

use super::budget::{BudgetExceeded, Text};
use super::command_starts::{iter_shell_command_word_spans, shell_command_segment};
use super::shell_tokens::{iter_top_level_shell_segments, shell_segment_tokens};
use super::shell_words::deobfuscate_shell_word_for_detection;

pub const MALFORMED_EXEC_DESCRIPTION: &str = "command parser limit or malformed executable payload";
pub const SHELL_NAMES: [&str; 5] = ["bash", "sh", "zsh", "ksh", "dash"];
pub const MAX_EVAL_NESTING: usize = 8;

fn interpreter_regexes() -> &'static [(&'static str, Regex)] {
    static RES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
        [
            ("python", r"py(?:\.exe)?|python[23]?(?:\.\d+)*(?:\.exe)?"),
            ("node", r"node(?:js)?(?:\.exe)?"),
            ("perl", r"perl[0-9]*(?:\.\d+)*(?:\.exe)?"),
            ("ruby", r"ruby[0-9.]*(?:\.exe)?"),
            ("php", r"php(?:\.exe)?"),
            ("powershell", r"powershell(?:\.exe)?|pwsh(?:\.exe)?"),
            ("bun", r"bun(?:\.exe)?"),
            ("deno", r"deno(?:\.exe)?"),
        ]
        .into_iter()
        .map(|(family, pattern)| {
            (
                family,
                Regex::new(&format!(r"\A(?:{pattern})\z")).expect("valid pattern"),
            )
        })
        .collect()
    });
    &RES
}

fn basename_lower(word: &str) -> String {
    word.rsplit('/').next().unwrap_or("").to_lowercase()
}

pub fn interpreter_family(executable: &str) -> Option<&'static str> {
    let name = basename_lower(executable);
    interpreter_regexes()
        .iter()
        .find(|(_, re)| re.is_match(&name))
        .map(|(family, _)| *family)
}

fn interpreter_exec_flags(family: &str) -> &'static [&'static str] {
    match family {
        "python" => &["-c"],
        "node" => &["-e", "--eval", "-p", "--print"],
        "perl" => &["-e", "--eval"],
        "ruby" => &["-e"],
        "php" => &["-r"],
        "powershell" => &["-command", "-c", "-file", "-f"],
        "bun" => &["-e", "--eval"],
        "deno" => &["eval", "-e", "--eval"],
        _ => &[],
    }
}

fn interpreter_with_arg(family: &str) -> &'static [&'static str] {
    match family {
        "python" => &["-W", "-X", "--check-hash-based-pycs"],
        "node" => &[
            "-C",
            "--conditions",
            "--cpu-prof-dir",
            "--diagnostic-dir",
            "--icu-data-dir",
            "--import",
            "--loader",
            "--openssl-config",
            "--require",
            "--title",
        ],
        "perl" => &["-0", "-F", "-I", "-M", "-m", "-x"],
        "ruby" => &["-C", "-E", "-F", "-I", "-K", "-r"],
        "php" => &["-c", "-d", "-z"],
        "powershell" => &[
            "-configurationname",
            "-custompipename",
            "-executionpolicy",
            "-inputformat",
            "-outputformat",
            "-settingsfile",
            "-version",
            "-windowstyle",
            "-workingdirectory",
        ],
        "bun" => &["--config", "--cwd", "--env-file", "--preload", "--require"],
        "deno" => &["-L", "--log-level"],
        _ => &[],
    }
}

fn read_tool_exec_flags(tool: &str) -> Option<&'static [&'static str]> {
    Some(match tool {
        "sort" => &["--compress-program"],
        "rg" => &["--pre", "--hostname-bin"],
        "ag" => &["--pager"],
        "man" => &["--pager", "--html", "-P", "-H"],
        _ => return None,
    })
}

fn read_tool_long_with_arg(tool: &str) -> &'static [&'static str] {
    match tool {
        "rg" => &[
            "--after-context",
            "--before-context",
            "--color",
            "--colors",
            "--context",
            "--context-separator",
            "--dfa-size-limit",
            "--encoding",
            "--engine",
            "--field-context-separator",
            "--field-match-separator",
            "--file",
            "--generate",
            "--glob",
            "--hostname-bin",
            "--hyperlink-format",
            "--iglob",
            "--ignore-file",
            "--max-columns",
            "--max-count",
            "--max-depth",
            "--max-filesize",
            "--path-separator",
            "--pre",
            "--pre-glob",
            "--regex-size-limit",
            "--regexp",
            "--replace",
            "--sort",
            "--sortr",
            "--threads",
            "--type",
            "--type-add",
            "--type-clear",
            "--type-not",
        ],
        "sort" => &[
            "--batch-size",
            "--buffer-size",
            "--compress-program",
            "--field-separator",
            "--files0-from",
            "--key",
            "--output",
            "--parallel",
            "--random-source",
            "--sort",
            "--temporary-directory",
        ],
        "man" => &[
            "--config-file",
            "--encoding",
            "--extension",
            "--locale",
            "--manpath",
            "--pager",
            "--preprocessor",
            "--prompt",
            "--recode",
            "--sections",
            "--systems",
        ],
        "ag" => &[
            "--ackmate-dir-filter",
            "--color-line-number",
            "--color-match",
            "--color-path",
            "--depth",
            "--filename-pattern",
            "--file-search-regex",
            "--ignore",
            "--ignore-dir",
            "--max-count",
            "--pager",
            "--path-to-ignore",
            "--width",
            "--workers",
        ],
        _ => &[],
    }
}

fn read_tool_short_with_arg(tool: &str) -> &'static str {
    match tool {
        "rg" => "efEmjgdtTABCMr",
        "sort" => "koStT",
        "man" => "CRLmMSserEPp",
        "ag" => "gGmpW",
        _ => "",
    }
}

const BASH_OPTIONS_WITH_ARG: [&str; 6] = ["-O", "+O", "-o", "+o", "--init-file", "--rcfile"];
const BASH_SHORT_OPTION_LETTERS: &str = "ilrsDcabefhkmnptuvxBCEHPTOo";

/// An execution-bearing interpreter option, if present.
pub fn interpreter_exec_flag(family: &str, args: &[String]) -> Option<String> {
    let flags = interpreter_exec_flags(family);
    let with_arg = interpreter_with_arg(family);
    let powershell = family == "powershell";
    let mut skip_value = false;
    for token in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if token == "--" || (!powershell && !token.starts_with('-')) {
            if family == "deno" && token.to_lowercase() == "eval" {
                return Some("eval".to_owned());
            }
            break;
        }
        let (option, equals) = match token.split_once('=') {
            Some((o, _)) => (o, true),
            None => (token.as_str(), false),
        };
        let comparable = if powershell {
            option.to_lowercase()
        } else {
            option.to_owned()
        };
        if flags.contains(&comparable.as_str()) {
            return Some(comparable);
        }
        let option_len = option.chars().count();
        let has_attached_option_value = with_arg.iter().any(|short| {
            short.starts_with('-')
                && !short.starts_with("--")
                && option.starts_with(short)
                && option_len > short.chars().count()
        });
        if !powershell && !option.starts_with("--") && option_len > 2 && !has_attached_option_value
        {
            if let Some(c) = option
                .chars()
                .skip(1)
                .find(|c| flags.contains(&format!("-{c}").as_str()))
            {
                return Some(format!("-{c}"));
            }
        }
        skip_value = with_arg.contains(&comparable.as_str()) && !equals;
    }
    None
}

/// Whether bash `-c` occurs and the command string it owns.
pub fn bash_exec_payload(args: &[String]) -> (bool, Option<String>) {
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        if token == "--" || !(token.starts_with('-') || token.starts_with('+')) {
            break;
        }
        if BASH_OPTIONS_WITH_ARG.contains(&token.as_str()) {
            index += 2;
            continue;
        }
        let chars: Vec<char> = token.chars().skip(1).collect();
        if token.starts_with("--") || !chars.iter().all(|c| BASH_SHORT_OPTION_LETTERS.contains(*c))
        {
            index += 1;
            continue;
        }
        let consumed = usize::from(chars.contains(&'O') || chars.contains(&'o'));
        if chars.contains(&'c') {
            return (true, args.get(index + 1 + consumed).cloned());
        }
        index += 1 + consumed;
    }
    (false, None)
}

/// `(option, program)` for a read-only tool's program-running flag.
pub fn read_tool_exec_flag(tool: &str, args: &[String]) -> Option<(String, String)> {
    let flags = read_tool_exec_flags(tool)?;
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        if token == "--" {
            break;
        }
        let (option, mut payload) = match token.split_once('=') {
            Some((o, p)) => (o.to_owned(), Some(p.to_owned())),
            None => (token.clone(), None),
        };
        let mut matched = flags.contains(&option.as_str()).then(|| option.clone());
        if tool == "man"
            && (token.starts_with("-P") || token.starts_with("-H"))
            && token.chars().count() > 2
        {
            matched = Some(token.chars().take(2).collect());
            payload = Some(token.chars().skip(2).collect());
        }
        if let Some(matched) = matched {
            if payload.is_none() && index + 1 < args.len() {
                payload = Some(args[index + 1].clone());
            }
            if let Some(p) = payload.as_ref().filter(|p| !p.is_empty()) {
                return Some((matched, p.clone()));
            }
            index += if payload.is_some() && !token.contains('=') {
                2
            } else {
                1
            };
        } else if read_tool_long_with_arg(tool).contains(&option.as_str()) && payload.is_none() {
            index += 2;
        } else if token.starts_with('-') && !token.starts_with("--") && token.chars().count() > 1 {
            let shorts = read_tool_short_with_arg(tool);
            let owner = token
                .chars()
                .enumerate()
                .skip(1)
                .find(|(_, c)| shorts.contains(*c));
            index += if owner.is_some_and(|(k, _)| k == token.chars().count() - 1) {
                2
            } else {
                1
            };
        } else {
            index += 1;
        }
    }
    None
}

/// The script text `-Command`/`-c` runs (the remaining arguments joined).
pub fn powershell_command_payload(args: &[String]) -> Option<String> {
    let mut skip_value = false;
    for (index, token) in args.iter().enumerate() {
        if skip_value {
            skip_value = false;
            continue;
        }
        let option = token.to_lowercase();
        if option == "-command" || option == "-c" {
            let joined = args[index + 1..].join(" ");
            return (!joined.is_empty()).then_some(joined);
        }
        if option == "-file" || option == "-f" || !option.starts_with('-') {
            return None;
        }
        skip_value = interpreter_with_arg("powershell").contains(&option.as_str());
    }
    None
}

/// Scoped execution mechanisms and their payloads: `(description, payload)`.
pub fn execution_flag_findings(
    command: &str,
) -> Result<Vec<(String, Option<String>)>, BudgetExceeded> {
    let mut out = Vec::new();
    for segment in iter_top_level_shell_segments(command)? {
        let seg_text = Text::new(&segment);
        for (start, _, word) in iter_shell_command_word_spans(&seg_text)? {
            let executable = deobfuscate_shell_word_for_detection(&word)?;
            let tokens = shell_segment_tokens(&segment, start);
            let name = basename_lower(&executable);
            let family = interpreter_family(&executable);
            let Some(tokens) = tokens else {
                if family.is_some() || read_tool_exec_flags(&name).is_some() {
                    out.push((MALFORMED_EXEC_DESCRIPTION.to_owned(), None));
                }
                continue;
            };
            if tokens.is_empty() {
                continue;
            }
            let args = &tokens[1..];
            if family.is_some_and(|f| interpreter_exec_flag(f, args).is_some()) {
                let payload = if family == Some("powershell") {
                    powershell_command_payload(args)
                } else {
                    None
                };
                out.push(("script execution via -e/-c flag".to_owned(), payload));
            } else if family.is_some() && args.iter().any(|t| t.starts_with("<<")) {
                out.push(("script execution via heredoc".to_owned(), None));
            } else {
                if SHELL_NAMES.contains(&name.as_str()) {
                    let (found, payload) = bash_exec_payload(args);
                    if found {
                        out.push(("shell command via -c/-lc flag".to_owned(), payload));
                    }
                }
                if read_tool_exec_flags(&name).is_some() {
                    if let Some((option, program)) = read_tool_exec_flag(&name, args) {
                        out.push((
                            format!("arbitrary program execution via {name} {option}"),
                            Some(program),
                        ));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The code eval would run: its arguments joined by spaces.
pub fn eval_payload(args: &[String]) -> Option<String> {
    let args = if args.first().is_some_and(|a| a == "--") {
        &args[1..]
    } else {
        args
    };
    let joined = args.join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// The code each `eval` in `command` hands back to the shell.
pub fn eval_payloads(command: &str) -> Result<Vec<String>, BudgetExceeded> {
    let mut out = Vec::new();
    for segment in iter_top_level_shell_segments(command)? {
        let seg_text = Text::new(&segment);
        for (start, _, word) in iter_shell_command_word_spans(&seg_text)? {
            if basename_lower(&deobfuscate_shell_word_for_detection(&word)?) != "eval" {
                continue;
            }
            let bounded = shell_command_segment(&seg_text, start)?;
            let tokens = shell_segment_tokens(&bounded, 0);
            if let Some(payload) = tokens
                .as_deref()
                .filter(|t| !t.is_empty())
                .and_then(|t| eval_payload(&t[1..]))
            {
                out.push(payload);
            }
        }
    }
    Ok(out)
}

/// `(payload, eval_depth)` for each program-bearing option and eval in `source`. eval code is one
/// level deeper; past `MAX_EVAL_NESTING` it is not surfaced and `source` is recorded in `capped`.
pub fn nested_payloads(
    source: &str,
    eval_depth: usize,
    capped: Option<&mut Vec<String>>,
) -> Result<Vec<(String, usize)>, BudgetExceeded> {
    let mut out = Vec::new();
    for (_, payload) in execution_flag_findings(source)? {
        if let Some(payload) = payload.filter(|p| !p.is_empty()) {
            out.push((payload, eval_depth));
        }
    }
    if eval_depth < MAX_EVAL_NESTING {
        for payload in eval_payloads(source)? {
            out.push((payload, eval_depth + 1));
        }
    } else if let Some(capped) = capped {
        if !eval_payloads(source)?.is_empty() {
            capped.push(source.to_owned());
        }
    }
    Ok(out)
}
