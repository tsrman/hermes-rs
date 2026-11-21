//! Which redaction mode terminal output gets, from the command that produced it.
//!
//! The reference redacts terminal output as source code unless the command dumps the
//! environment or reads a secret-bearing file (`.env`, a shell rc, a credential store, Hermes'
//! own `config.yaml`): then every assignment pass runs. Defense in depth, not a boundary:
//! indirect reads (`sudo cat .env`, `$(cat .env)`) are not detected.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::{redact_with, Mode};

const ENV_DUMP_COMMANDS: [&str; 5] = ["env", "printenv", "set", "export", "declare"];
const FILE_READ_COMMANDS: [&str; 17] = [
    "cat",
    "head",
    "tail",
    "type",
    "bat",
    "less",
    "more",
    "nl",
    "zcat",
    "tac",
    "view",
    "batcat",
    "grep",
    "awk",
    "sed",
    "get-content",
    "gc",
];
const PATTERN_FIRST_COMMANDS: [&str; 3] = ["grep", "awk", "sed"];
const SHELL_RC: [&str; 8] = [
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zlogin",
    ".zshenv",
];
const ENV_FILES: [&str; 7] = [
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env.staging",
    ".envrc",
];
const HOME_CREDENTIAL_FILES: [&str; 6] = [
    ".netrc",
    "_netrc",
    ".pgpass",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
];

/// Pipeline and sequence segments, split only on unquoted `| ; &` so an awk program or a grep
/// pattern stays one segment. Backslash is not an escape.
fn command_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut buffer = String::new();
    let mut quote: Option<char> = None;
    for c in command.chars() {
        if let Some(open) = quote {
            buffer.push(c);
            if c == open {
                quote = None;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            buffer.push(c);
            continue;
        }
        if matches!(c, '|' | ';' | '&') {
            let segment = buffer.trim().to_owned();
            if !segment.is_empty() {
                segments.push(segment);
            }
            buffer.clear();
            continue;
        }
        buffer.push(c);
    }
    let segment = buffer.trim().to_owned();
    if !segment.is_empty() {
        segments.push(segment);
    }
    segments
}

/// `shlex.split`, or `None` when it would fail (an unbalanced quote or a trailing backslash).
fn shlex_split(text: &str) -> Option<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '\\' => {
                current.push(chars.next()?);
                started = true;
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => current.push(other),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => {
                            let next = chars.next()?;
                            if !matches!(next, '"' | '\\') {
                                current.push('\\');
                            }
                            current.push(next);
                        }
                        other => current.push(other),
                    }
                }
            }
            other => {
                current.push(other);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    Some(words)
}

/// Whether any segment starts with a command that dumps the environment.
pub fn is_env_dump_command(command: &str) -> bool {
    command_segments(command).iter().any(|segment| {
        let tokens = shlex_split(segment)
            .unwrap_or_else(|| segment.split_whitespace().map(str::to_owned).collect());
        tokens
            .first()
            .is_some_and(|first| ENV_DUMP_COMMANDS.contains(&first.as_str()))
    })
}

/// `.env`-style, shell rc or credential-store basename anywhere; `credentials` and `config` only
/// under `.aws`; `config.yaml` only under a `.hermes` directory, `$HERMES_HOME` or one of the
/// resolved Hermes `homes`.
pub fn is_secret_file_arg(arg: &str, homes: &[&Path]) -> bool {
    let mut path = arg.trim_matches(['"', '\'']).replace('\\', "/");
    let mut hermes_home = false;
    for prefix in ["$HERMES_HOME/", "${HERMES_HOME}/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            path = rest.to_owned();
            hermes_home = true;
            break;
        }
    }
    for prefix in ["$HOME/", "${HOME}/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            path = rest.to_owned();
            break;
        }
    }
    if path.contains('$') {
        return false;
    }
    let parts: Vec<String> = path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect();
    let Some(last) = parts.last() else {
        return false;
    };
    if ENV_FILES.contains(&last.as_str()) || SHELL_RC.contains(&last.as_str()) {
        return true;
    }
    let tail: Vec<&str> = parts
        .iter()
        .rev()
        .take(2)
        .rev()
        .map(String::as_str)
        .collect();
    if HOME_CREDENTIAL_FILES.contains(&last.as_str())
        || tail == [".aws", "credentials"]
        || tail == [".aws", "config"]
    {
        return true;
    }
    // `config.yaml` plus the `config.yaml.good.<stamp>` and `.corrupt.<stamp>` copies.
    if last != "config.yaml"
        && !last.starts_with("config.yaml.good.")
        && !last.starts_with("config.yaml.corrupt.")
    {
        return false;
    }
    hermes_home
        || parts[..parts.len() - 1]
            .iter()
            .any(|part| part == ".hermes")
        || under_hermes_home(&path, homes)
}

fn under_hermes_home(path: &str, homes: &[&Path]) -> bool {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => std::env::home_dir().map(|home| home.join(rest)),
        None => Some(std::path::PathBuf::from(path)),
    };
    let Some(target) = expanded.map(|p| p.canonicalize().unwrap_or(p)) else {
        return false;
    };
    homes.iter().any(|home| {
        let base = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        target.starts_with(&base)
    })
}

/// Whether `command` reads a secret-bearing file to standard output.
pub fn command_reads_secret_file(command: &str, homes: &[&Path]) -> bool {
    static PS_COLON_PARAM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^-[A-Za-z]+:(.+)$").expect("valid pattern"));
    for segment in command_segments(command) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        let reader = first.rsplit('/').next().unwrap_or("").to_lowercase();
        if !FILE_READ_COMMANDS.contains(&reader.as_str()) {
            continue;
        }
        let pattern_first = PATTERN_FIRST_COMMANDS.contains(&reader.as_str());
        let mut positional: Vec<&str> = Vec::new();
        for arg in &tokens[1..] {
            if !arg.starts_with('-') {
                positional.push(arg);
            } else if !pattern_first {
                if let Some(caps) = PS_COLON_PARAM.captures(arg) {
                    positional.push(caps.get(1).map_or("", |m| m.as_str()));
                }
            }
        }
        if pattern_first && !positional.is_empty() {
            positional.remove(0);
        }
        if positional.iter().any(|arg| is_secret_file_arg(arg, homes)) {
            return true;
        }
    }
    false
}

static PYTEST_DIAGNOSTIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<prefix>[ \t]*E[ \t]{2,})(?P<body>.*)$").expect("valid pattern")
});
static PYTHON_EXCEPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?P<prefix>(?:[A-Za-z_]\w*\.)*[A-Za-z_]\w*(?:Error|Exception|Warning):[ \t]*)(?P<body>.*)$",
    )
    .expect("valid pattern")
});

/// Mask repr fields only on pytest and error lines in source-preserving output.
fn redact_python_diagnostic_repr_fields(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let (body, ending) = match line.strip_suffix("\r\n") {
            Some(body) => (body, "\r\n"),
            None => match line.strip_suffix('\n') {
                Some(body) => (body, "\n"),
                None => (line, ""),
            },
        };
        let found = PYTEST_DIAGNOSTIC
            .captures(body)
            .or_else(|| PYTHON_EXCEPTION.captures(body));
        match found {
            Some(caps) => {
                out.push_str(&caps["prefix"]);
                out.push_str(&super::fields::redact_python_repr_fields(&caps["body"]));
                out.push_str(ending);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Terminal output, redacted by what the command was: an environment dump or a read of a
/// secret-bearing file gets every assignment pass, anything else is treated as source.
pub fn redact_terminal_output(output: &str, command: Option<&str>, homes: &[&Path]) -> String {
    if output.is_empty() {
        return String::new();
    }
    let command = command.unwrap_or("");
    let secret_file = command_reads_secret_file(command, homes);
    let code_file = !(secret_file || is_env_dump_command(command));
    let redacted = redact_with(
        output,
        Mode {
            code_file,
            secret_file,
            ..Mode::default()
        },
    );
    // Source-preserving output still gets the Python-repr pass on high-confidence diagnostic
    // lines: that is where `{'BRAVE_API_KEY': '…'}` leaks, not in source dumps.
    if code_file && redacted.contains(':') && redacted.contains('\'') {
        return redact_python_diagnostic_repr_fields(&redacted);
    }
    redacted
}
