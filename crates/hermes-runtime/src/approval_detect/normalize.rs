//! Detection-time normalization (`_normalize_command_for_detection`, `_lower_preserving_flags`,
//! `tools/ansi_strip.py::strip_ansi` and `tools/approval_home_fold.py`).
//!
//! POSIX only: the home fold does not know Windows spellings (`C:\Users\me`, `%VAR%` roots,
//! case-insensitive paths). Git Bash drive spellings (`/c/Users/me`) are kept because they are plain
//! string maths. NFKC comes from ICU; the reference uses Python's `unicodedata`, so a character that
//! the two Unicode versions normalise differently is the one place the results can diverge.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use fancy_regex::{Captures, Regex};
use icu_normalizer::ComposingNormalizerBorrowed;

use super::ansi_c::expand_ansi_c_quotes;
use super::py_isspace;

/// The environment the home fold reads, resolved when a command is checked, not at import time.
#[derive(Debug, Clone, Default)]
pub struct HomeEnv {
    /// `os.path.expanduser("~")`: `$HOME` without trailing slashes, else the account home.
    pub user_home: String,
    /// The raw `$HOME`, `""` when unset.
    pub home_var: String,
    /// `get_hermes_home()`: `$HERMES_HOME` expanded, else `~/.hermes` plus `HERMES_DATA_DIR_SUFFIX`.
    pub hermes_home: String,
    pub local_appdata: String,
    pub appdata: String,
    pub userprofile: String,
}

fn expanduser(path: &str, home: &str) -> String {
    if path == "~" {
        home.to_owned()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", home.trim_end_matches('/'), rest)
    } else {
        path.to_owned()
    }
}

fn expandvars(path: &str, var: impl Fn(&str) -> Option<String>) -> String {
    static VAR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\$(?:(\w+)|\{([^}]*)\})").expect("valid pattern"));
    let mut out = String::new();
    let mut last = 0;
    for caps in VAR.captures_iter(path).flatten() {
        let whole = caps.get(0).expect("group 0");
        let name = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map_or("", |m| m.as_str());
        out.push_str(&path[last..whole.start()]);
        match var(name) {
            Some(value) => out.push_str(&value),
            None => out.push_str(whole.as_str()),
        }
        last = whole.end();
    }
    out.push_str(&path[last..]);
    out
}

/// `str(Path(p))`: collapse repeated and trailing slashes and `.` components.
fn path_str(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let lead = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() && lead.is_empty() {
        return ".".into();
    }
    format!("{lead}{}", parts.join("/"))
}

/// `Path.resolve(strict=False)` / `os.path.realpath`: resolve what exists, normalise the rest.
fn resolve(path: &str) -> String {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real.to_string_lossy().into_owned();
    }
    let mut out = PathBuf::new();
    let absolute = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

impl HomeEnv {
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        let home_var = std::env::var("HOME").ok();
        let account = std::env::home_dir().map(|p| p.to_string_lossy().into_owned());
        let user_home = match &home_var {
            Some(h) => {
                let trimmed = h.trim_end_matches('/');
                if trimmed.is_empty() {
                    "/".to_owned()
                } else {
                    trimmed.to_owned()
                }
            }
            None => account.unwrap_or_default(),
        };
        let hermes_var = var("HERMES_HOME");
        let hermes_home = if hermes_var.trim().is_empty() {
            format!(
                "{}/.hermes{}",
                user_home.trim_end_matches('/'),
                var("HERMES_DATA_DIR_SUFFIX")
            )
        } else {
            let expanded = expandvars(hermes_var.trim(), |n| std::env::var(n).ok());
            expanduser(&expanded, &user_home)
        };
        Self {
            user_home,
            home_var: home_var.unwrap_or_default(),
            hermes_home,
            local_appdata: var("LOCALAPPDATA"),
            appdata: var("APPDATA"),
            userprofile: var("USERPROFILE"),
        }
    }
}

// ---- strip_ansi ---------------------------------------------------------------------------

static ANSI_ESCAPE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?s)\x1b(?:\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\][\s\S]*?(?:\x07|\x1b\\)|[PX^_][\s\S]*?(?:\x1b\\)|[\x20-\x2f]+[\x30-\x7e]|[\x30-\x7e])|\x{9b}[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\x{9d}[\s\S]*?(?:\x07|\x{9c})|[\x{80}-\x{9f}]",
    )
    .expect("valid pattern")
});

/// Remove ANSI escape sequences (CSI, OSC, DCS/SOS/PM/APC, nF, Fp/Fe/Fs and 8-bit C1 controls).
pub fn strip_ansi(text: &str) -> String {
    ANSI_ESCAPE.replace_all(text, "").into_owned()
}

// ---- home fold ----------------------------------------------------------------------------

/// Shell metacharacters, quotes and whitespace that end a path token, as the inside of a class.
const PATH_TOKEN_STOP: &str = r#"\s\x1c-\x1f'"`;|&<>()"#;

fn path_tail() -> String {
    format!(r"(?P<tail>(?:[/\\][^/\\{PATH_TOKEN_STOP}]*)+)")
}

fn components(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|c| !c.is_empty()).collect()
}

/// `_spell_component`: the component as a regex, tolerating the escape Git Bash writes before a space.
fn spell_component(component: &str) -> String {
    let mut out = String::new();
    for c in component.chars() {
        if c == ' ' {
            out.push_str(r"\\? ");
        } else {
            out.push_str(&fancy_regex::escape(&c.to_string()));
        }
    }
    out
}

fn fold_matches(
    pattern: &Regex,
    command: &str,
    has_tail: bool,
    replace: &dyn Fn(&Captures<'_, str>) -> String,
) -> String {
    let mut out = String::new();
    let mut last = 0;
    let mut pos = 0;
    while pos <= command.len() {
        let caps = match pattern.captures_from_pos(command, pos) {
            Ok(Some(caps)) => caps,
            _ => break,
        };
        let whole = caps.get(0).expect("group 0");
        let text = whole.as_str();
        out.push_str(&command[last..whole.start()]);
        let trailing = text.chars().rev().take_while(|c| *c == '\\').count();
        let next_is_blank = command[whole.end()..].starts_with([' ', '\t']);
        let folded = if !(next_is_blank && trailing % 2 == 1) {
            replace(&caps)
        } else if has_tail
            && caps
                .name("tail")
                .is_some_and(|t| !t.as_str().trim_matches('\\').is_empty())
        {
            let shorter_end = whole.end() - 1;
            match pattern.captures_from_pos(&command[..shorter_end], whole.start()) {
                Ok(Some(short)) if short.get(0).is_some_and(|m| m.start() == whole.start()) => {
                    let end = short.get(0).expect("group 0").end();
                    format!("{}{}", replace(&short), &command[end..whole.end()])
                }
                _ => text.to_owned(),
            }
        } else {
            text.to_owned()
        };
        out.push_str(&folded);
        last = whole.end();
        pos = if whole.end() == whole.start() {
            whole.end()
                + command[whole.end()..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8)
        } else {
            whole.end()
        };
    }
    out.push_str(&command[last.min(command.len())..]);
    out
}

fn is_drive(component: &str) -> bool {
    let mut it = component.chars();
    matches!((it.next(), it.next(), it.next()), (Some(l), Some(':'), None) if l.is_ascii_alphabetic())
}

/// `_home_prefix_fold_regex` for POSIX.
fn home_prefix_fold_regex(path: &str) -> Option<Regex> {
    let comps = components(path);
    if path.is_empty() || comps.is_empty() || (comps.len() == 1 && is_drive(comps[0])) {
        return None;
    }
    let single_posix = comps.len() == 1 && path.starts_with('/');
    let sep = if single_posix { r"(?:\\?/)" } else { r"[/\\]" };
    let lead = if comps.len() == 1 {
        format!(r"(?<![\w.~$}}/\\-]){sep}+")
    } else {
        format!("{sep}*")
    };
    let tail = if single_posix {
        format!(r"(?P<tail>(?:\\?/[^/\\{PATH_TOKEN_STOP}]*)+)")
    } else {
        path_tail()
    };
    let spelled: Vec<String> = comps.iter().map(|c| spell_component(c)).collect();
    Regex::new(&format!("{lead}{}{tail}", spelled.join(&format!("{sep}+")))).ok()
}

/// `_bare_home_fold_regex`: the path as a whole shell word.
fn bare_home_fold_regex(path: &str) -> Option<Regex> {
    let comps = components(path);
    if comps.len() < 2 {
        return None;
    }
    let spelled: Vec<String> = comps.iter().map(|c| spell_component(c)).collect();
    Regex::new(&format!(
        r#"(?<![^\s\x1c-\x1f'"`=;|&<>(])[/\\]*{}[/\\]?(?=[{PATH_TOKEN_STOP}*]|$)"#,
        spelled.join(r"[/\\]+")
    ))
    .ok()
}

fn dedupe(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

fn fold_home_prefixes(command: &str, paths: &[String], replacement: &str) -> String {
    let mut sorted: Vec<String> = paths.iter().filter(|p| !p.is_empty()).cloned().collect();
    sorted.sort_by_key(|p| std::cmp::Reverse(p.chars().count()));
    let mut command = command.to_owned();
    for path in dedupe(sorted) {
        if let Some(pattern) = home_prefix_fold_regex(&path) {
            command = fold_matches(&pattern, &command, true, &|caps| {
                let tail = caps.name("tail").map_or("", |m| m.as_str());
                format!(
                    "{replacement}{}",
                    tail.replace("\\/", "/").replace('\\', "/")
                )
            });
        }
    }
    command
}

/// `/c/Users/me` and `/C/Users/me` for `C:\Users\me`; empty without a drive.
fn msys_drive_spellings(path: &str) -> Vec<String> {
    let mut chars = path.chars();
    let (Some(letter), Some(':'), Some(sep)) = (chars.next(), chars.next(), chars.next()) else {
        return Vec::new();
    };
    let rest: String = chars.collect();
    if !letter.is_ascii_alphabetic() || !matches!(sep, '/' | '\\') || rest.is_empty() {
        return Vec::new();
    }
    vec![
        format!("/{}/{rest}", letter.to_ascii_lowercase()),
        format!("/{}/{rest}", letter.to_ascii_uppercase()),
    ]
}

fn path_below(root: &str, path: &str) -> Option<String> {
    let root_parts = components(root);
    let path_parts = components(path);
    if root_parts.is_empty() || path_parts.len() <= root_parts.len() {
        return None;
    }
    if path_parts[..root_parts.len()] != root_parts[..] {
        return None;
    }
    Some(path_parts[root_parts.len()..].join("/"))
}

fn home_spellings(paths: &[String], command: &str, env: &HomeEnv) -> Vec<String> {
    let absolute = dedupe(paths.iter().flat_map(|p| {
        let mut all = vec![p.clone()];
        all.extend(msys_drive_spellings(p));
        all
    }));
    if !["~", "$", "%"].iter().any(|m| command.contains(m)) {
        return absolute;
    }
    let home = if env.home_var.is_empty() {
        env.user_home.clone()
    } else {
        env.home_var.clone()
    };
    let named = [
        ("HOME", home.clone()),
        ("LOCALAPPDATA", env.local_appdata.clone()),
        ("APPDATA", env.appdata.clone()),
        ("USERPROFILE", env.userprofile.clone()),
    ];
    let mut roots = vec![("~".to_owned(), home)];
    for (name, root) in &named {
        for spelled in [
            format!("${name}"),
            format!("${{{name}}}"),
            format!("%{name}%"),
        ] {
            roots.push((spelled, root.clone()));
        }
    }
    let mut relative = Vec::new();
    for (prefix, root) in &roots {
        let mut spelled_roots = vec![root.clone()];
        spelled_roots.extend(msys_drive_spellings(root));
        for spelled_root in dedupe(spelled_roots) {
            for candidate in &absolute {
                if let Some(tail) = path_below(&spelled_root, candidate) {
                    relative.push(format!("{prefix}/{tail}"));
                }
            }
        }
    }
    let mut all = absolute;
    all.extend(dedupe(relative));
    all
}

fn rewrite_resolved_user_home(command: &str, env: &HomeEnv) -> String {
    let home = env.user_home.clone();
    let mut paths = vec![home.clone(), resolve(&home), env.home_var.clone()];
    let extra: Vec<String> = paths
        .iter()
        .filter(|p| !p.is_empty())
        .flat_map(|p| msys_drive_spellings(p))
        .collect();
    paths.extend(extra);
    fold_home_prefixes(command, &paths, "~")
}

fn rewrite_resolved_hermes_home(command: &str, env: &HomeEnv) -> String {
    let home = path_str(&env.hermes_home);
    let paths = home_spellings(&[home.clone(), path_str(&resolve(&home))], command, env);
    let mut command = fold_home_prefixes(command, &paths, "~/.hermes");
    let mut sorted: Vec<String> = paths.iter().filter(|p| !p.is_empty()).cloned().collect();
    sorted.sort_by_key(|p| std::cmp::Reverse(p.chars().count()));
    for path in dedupe(sorted) {
        if let Some(pattern) = bare_home_fold_regex(&path) {
            command = fold_matches(&pattern, &command, false, &|_| "~/.hermes".to_owned());
        }
    }
    command
}

// ---- normalization ------------------------------------------------------------------------

fn nfkc(text: &str) -> String {
    ComposingNormalizerBorrowed::new_nfkc()
        .normalize(text)
        .into_owned()
}

/// Normalize a command before pattern matching so ANSI escapes, null bytes, fullwidth forms, line
/// continuations, home spellings, backslash escapes, empty quotes and `$IFS` cannot hide a verb.
pub fn normalize_command_for_detection(command: &str, env: &HomeEnv) -> String {
    static CONTINUATION: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\\\r?\n").expect("valid pattern"));
    static ESCAPE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\\([^\n])").expect("valid pattern"));
    static EMPTY_QUOTES: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"''|"""#).expect("valid pattern"));
    static IFS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\$\{IFS\b[^}]*\}|\$IFS\b").expect("valid pattern"));

    let command = expand_ansi_c_quotes(command);
    let command = nfkc(&strip_ansi(&command).replace('\0', ""));
    let command = CONTINUATION.replace_all(&command, "").into_owned();
    let command = rewrite_resolved_hermes_home(&command, env);
    let command = rewrite_resolved_user_home(&command, env);
    let command = ESCAPE.replace_all(&command, "$1").into_owned();
    let command = EMPTY_QUOTES.replace_all(&command, "").into_owned();
    IFS.replace_all(&command, " ").into_owned()
}

/// Lowercase a variant for the pattern pass while keeping dash-prefixed tokens exactly as written.
pub fn lower_preserving_flags(command: &str) -> String {
    let mut out = String::new();
    let mut token = String::new();
    let mut in_space = false;
    let flush = |token: &mut String, out: &mut String, in_space: bool| {
        if token.is_empty() {
            return;
        }
        if in_space || token.starts_with('-') {
            out.push_str(token);
        } else {
            out.push_str(&token.to_lowercase());
        }
        token.clear();
    };
    for c in command.chars() {
        if py_isspace(c) != in_space {
            flush(&mut token, &mut out, in_space);
            in_space = py_isspace(c);
        }
        token.push(c);
    }
    flush(&mut token, &mut out, in_space);
    out
}
