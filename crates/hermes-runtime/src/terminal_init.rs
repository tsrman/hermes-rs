//! `terminal.shell_init_files` and `terminal.auto_source_bashrc`.
//!
//! The reference runs a login shell once per session, sourcing `~/.profile`, `~/.bash_profile` and
//! `~/.bashrc` (or the files `shell_init_files` lists) so that `PATH` entries added by nvm, pyenv or
//! cargo are visible to commands, then re-appends the directories of the original `PATH` that the
//! login profile dropped. Here the same shell runs once per process, but only the variables a command
//! may see anyway (the default and `env_passthrough` names) are taken from it: credentials an rc file
//! exports stay out, as does everything else in the login environment.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// What the reference's `bool(...)` of a config value gives.
pub(crate) fn py_truthy(value: &serde_yaml::Value) -> bool {
    use serde_yaml::Value;
    match value {
        Value::Null => false,
        Value::Bool(on) => *on,
        Value::Number(number) => number.as_f64().is_none_or(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Sequence(items) => !items.is_empty(),
        Value::Mapping(map) => !map.is_empty(),
        Value::Tagged(_) => true,
    }
}

fn expand(raw: &str, home: Option<&str>) -> String {
    let mut text = match (raw.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        _ if raw == "~" => home.unwrap_or("~").to_owned(),
        _ => raw.to_owned(),
    };
    // `$VAR` and `${VAR}` that are set; unknown names stay as written, like `os.path.expandvars`.
    let mut out = String::new();
    while let Some(at) = text.find('$') {
        out.push_str(&text[..at]);
        let rest = &text[at + 1..];
        let (name, consumed) = if let Some(inner) = rest.strip_prefix('{') {
            match inner.find('}') {
                Some(end) => (&inner[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            (&rest[..end], end)
        };
        match (name.is_empty(), std::env::var(name)) {
            (false, Ok(value)) => {
                out.push_str(&value);
                text = rest[consumed..].to_owned();
            }
            _ => {
                out.push('$');
                text = rest.to_owned();
            }
        }
    }
    out.push_str(&text);
    out
}

/// The init files to source: the explicit list, else (unless switched off) `~/.profile`,
/// `~/.bash_profile` and `~/.bashrc`; only regular files that exist.
pub(crate) fn resolved_files(
    explicit: &[String],
    auto_source_bashrc: bool,
    home: Option<&str>,
) -> Vec<PathBuf> {
    let candidates: Vec<String> = if !explicit.is_empty() {
        explicit.to_vec()
    } else if auto_source_bashrc && !cfg!(windows) {
        ["~/.profile", "~/.bash_profile", "~/.bashrc"]
            .map(str::to_owned)
            .to_vec()
    } else {
        Vec::new()
    };
    candidates
        .iter()
        .map(|raw| PathBuf::from(expand(raw, home)))
        .filter(|path| path.is_file())
        .collect()
}

pub(crate) fn find_bash() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("bash"))
        .chain([PathBuf::from("/bin/bash"), PathBuf::from("/usr/bin/bash")])
        .find(|candidate| candidate.is_file())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn run(bash: &Path, script: &str, base: &[(String, String)]) -> Option<Vec<u8>> {
    let mut command = Command::new(bash);
    command
        .args(["-l", "-c", script])
        .env_clear()
        .envs(base.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                // A grandchild may hold the pipe open: do not wait for the reader.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    reader.join().ok()
}

/// The values `names` have after a login shell has sourced `files`.
pub(crate) fn login_environment(
    files: &[PathBuf],
    base: &[(String, String)],
    names: &[String],
) -> Vec<(String, String)> {
    let Some(bash) = find_bash().filter(|_| !files.is_empty()) else {
        return Vec::new();
    };
    let mut script = String::from("set +e\n");
    for file in files {
        let quoted = shell_quote(&file.to_string_lossy());
        script.push_str(&format!(
            "[ -r {quoted} ] && . {quoted} 2>/dev/null || true\n"
        ));
    }
    for name in names {
        if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
        {
            script.push_str(&format!(
                "[ -n \"${{{name}+x}}\" ] && printf '%s\\0' \"{name}=${name}\"\n"
            ));
        }
    }
    let Some(output) = run(&bash, &script, base) else {
        return Vec::new();
    };
    output
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let text = String::from_utf8(entry.to_vec()).ok()?;
            let (name, value) = text.split_once('=')?;
            names
                .iter()
                .any(|known| known == name)
                .then(|| (name.to_owned(), value.to_owned()))
        })
        .collect()
}

/// `base` with the login values applied. `PATH` keeps the directories of the original that the login
/// profile dropped, after the login ones; the identity variables stay as they were.
pub(crate) fn apply(
    base: Vec<(String, String)>,
    login: &[(String, String)],
) -> Vec<(String, String)> {
    const KEEP: [&str; 6] = ["HOME", "USER", "LOGNAME", "SHELL", "TERM", "TZ"];
    let mut out = base;
    for (name, value) in login {
        if KEEP.contains(&name.as_str()) || name == "TMPDIR" {
            continue;
        }
        let value = if name == "PATH" {
            let original = out
                .iter()
                .find(|(known, _)| known == "PATH")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let mut parts: Vec<String> = value.split(':').map(str::to_owned).collect();
            for dir in original.split(':') {
                let usable = !dir.is_empty() && (!dir.starts_with('/') || Path::new(dir).is_dir());
                if usable && !parts.iter().any(|known| known == dir) {
                    parts.push(dir.to_owned());
                }
            }
            parts.join(":")
        } else {
            value.clone()
        };
        match out.iter_mut().find(|(known, _)| known == name) {
            Some(slot) => slot.1 = value,
            None => out.push((name.clone(), value)),
        }
    }
    out
}

type Environment = Vec<(String, String)>;

static CACHE: LazyLock<Mutex<HashMap<String, Environment>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `base` after the init files, computed once per distinct configuration for this process.
pub(crate) fn with_init_files(
    base: Vec<(String, String)>,
    explicit: &[String],
    auto_source_bashrc: bool,
    names: &[String],
) -> Vec<(String, String)> {
    let home = std::env::var("HOME").ok();
    let files = resolved_files(explicit, auto_source_bashrc, home.as_deref());
    if files.is_empty() {
        return base;
    }
    let key = format!(
        "{files:?}|{names:?}|{:?}",
        base.iter().find(|(n, _)| n == "PATH")
    );
    let cached = CACHE.lock().ok().and_then(|cache| cache.get(&key).cloned());
    let login = cached.unwrap_or_else(|| {
        let login = login_environment(&files, &base, names);
        if let Ok(mut cache) = CACHE.lock() {
            cache.insert(key, login.clone());
        }
        login
    });
    apply(base, &login)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn rc_files_extend_path_and_named_variables_but_nothing_else() {
        if find_bash().is_none() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("tools/bin");
        let dropped = home.path().join("kept/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&dropped).unwrap();
        std::fs::write(
            home.path().join(".bashrc"),
            format!(
                "export PATH=\"{}:/usr/bin:/bin\"\nexport NVM_DIR=/opt/nvm\nexport SECRET_TOKEN=leak\nexport HOME=/hijacked\n",
                bin.display()
            ),
        )
        .unwrap();
        let files = resolved_files(&[], true, Some(&home.path().to_string_lossy()));
        assert_eq!(files, [home.path().join(".bashrc")]);
        let base = vec![
            (
                "PATH".to_owned(),
                format!("/usr/bin:/bin:{}:/no/such/dir", dropped.display()),
            ),
            ("HOME".to_owned(), home.path().display().to_string()),
        ];
        let login = login_environment(&files, &base, &names(&["PATH", "NVM_DIR", "HOME"]));
        let merged = apply(base, &login);
        let get = |name: &str| {
            merged
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        assert!(get("PATH")
            .unwrap()
            .starts_with(&format!("{}:", bin.display())));
        assert!(get("PATH").unwrap().contains("/usr/bin"));
        // The profile reset PATH: a directory of the original that exists comes back at the end, one that
        // does not stays out.
        assert!(get("PATH")
            .unwrap()
            .ends_with(&format!(":{}", dropped.display())));
        assert!(!get("PATH").unwrap().contains("/no/such/dir"));
        assert_eq!(get("NVM_DIR").as_deref(), Some("/opt/nvm"));
        assert_eq!(get("HOME"), Some(home.path().display().to_string()));
        assert_eq!(get("SECRET_TOKEN"), None);
    }

    #[test]
    fn explicit_lists_replace_the_defaults_and_the_switch_turns_them_off() {
        let home = tempfile::tempdir().unwrap();
        for name in [".profile", ".bashrc", "custom.sh"] {
            std::fs::write(home.path().join(name), "").unwrap();
        }
        let home_text = home.path().to_string_lossy().into_owned();
        assert_eq!(
            resolved_files(
                &["~/custom.sh".into(), "~/missing.sh".into()],
                true,
                Some(&home_text)
            ),
            [home.path().join("custom.sh")]
        );
        assert_eq!(
            resolved_files(&[], true, Some(&home_text)),
            [home.path().join(".profile"), home.path().join(".bashrc")]
        );
        assert!(resolved_files(&[], false, Some(&home_text)).is_empty());
    }

    #[test]
    fn config_truthiness_follows_python() {
        let truthy = |text: &str| py_truthy(&serde_yaml::from_str(text).unwrap());
        assert!(truthy("true") && truthy("'false'") && truthy("1") && truthy("[a]"));
        assert!(
            !truthy("false") && !truthy("0") && !truthy("''") && !truthy("null") && !truthy("[]")
        );
    }
}
