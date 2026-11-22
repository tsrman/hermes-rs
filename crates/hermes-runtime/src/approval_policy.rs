//! Which actions the person has already decided, so that a long task is not a string of prompts.
//!
//! `approvals.mode: off` (or `--yolo`) approves everything except what `approvals.deny` names;
//! `approvals.allow` lists commands that never need asking; and an answer of "always" in the prompt
//! approves later file edits for the conversation and the same command again. A command is only
//! approved by a pattern when it is a single plain command: no `;`, `&`, `|`, redirection,
//! substitution, quoting tricks or line breaks that could add a second command to a safe prefix.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::file_tools::{ApprovalAction, ApprovalRequest};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ApprovalSettings {
    /// `approvals.mode: off`, or `--yolo`: approve everything the deny list does not block.
    #[serde(default)]
    pub approve_all: bool,
    /// Command patterns (`*` and `?`, case-insensitive) that need no asking.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Command patterns that are always refused, even with `approve_all`.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Read-only commands that need no asking.
const BUILT_IN_ALLOW: [&str; 8] = [
    "pwd",
    "ls",
    "ls *",
    "git status",
    "git status *",
    "git diff",
    "git diff *",
    "git log *",
];

impl ApprovalSettings {
    pub fn from_config(config: &serde_yaml::Value) -> Self {
        let strings = |value: &serde_yaml::Value| -> Vec<String> {
            value
                .as_sequence()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_yaml::Value::as_str)
                        .map(|text| text.trim().to_owned())
                        .filter(|text| !text.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        let section = &config["approvals"];
        let mode = section["mode"]
            .as_str()
            .map(|mode| mode.trim().to_lowercase());
        Self {
            approve_all: matches!(mode.as_deref(), Some("off" | "false"))
                || section["mode"] == false,
            allow: strings(&section["allow"]),
            deny: strings(&section["deny"]),
        }
    }

    /// Whether anything is approved without a person, so that tools needing approval are offered
    /// to a run with no terminal.
    pub fn answers_without_a_person(&self) -> bool {
        self.approve_all || !self.allow.is_empty()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Verdict {
    Allow,
    Deny,
    Ask,
}

#[derive(Default)]
struct Grants {
    edits: bool,
    commands: HashSet<String>,
}

pub struct ApprovalPolicy {
    settings: ApprovalSettings,
    grants: Mutex<Grants>,
}

/// `fnmatch` with `*` and `?`, ignoring case.
fn glob(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let (mut p, mut t) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// A single plain command: nothing in it can start a second one or change where output goes.
fn is_plain(command: &str) -> bool {
    !command.chars().any(|c| {
        matches!(
            c,
            ';' | '&'
                | '|'
                | '<'
                | '>'
                | '('
                | ')'
                | '`'
                | '$'
                | '\n'
                | '\r'
                | '\\'
                | '{'
                | '}'
                | '"'
                | '\''
                | '!'
        )
    }) && !command.contains("--output")
}

impl ApprovalPolicy {
    pub fn new(settings: ApprovalSettings) -> Self {
        Self {
            settings,
            grants: Mutex::new(Grants::default()),
        }
    }

    pub fn settings(&self) -> &ApprovalSettings {
        &self.settings
    }

    pub fn decide(&self, request: &ApprovalRequest) -> Verdict {
        let grants = self
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match request.action {
            ApprovalAction::RunCommand => {
                let command = request.content.trim();
                if self
                    .settings
                    .deny
                    .iter()
                    .any(|pattern| glob(pattern, command))
                {
                    return Verdict::Deny;
                }
                if self.settings.approve_all || grants.commands.contains(command) {
                    return Verdict::Allow;
                }
                let allowed = self
                    .settings
                    .allow
                    .iter()
                    .map(String::as_str)
                    .chain(BUILT_IN_ALLOW)
                    .any(|pattern| glob(pattern, command));
                if allowed && is_plain(command) {
                    Verdict::Allow
                } else {
                    Verdict::Ask
                }
            }
            ApprovalAction::CreateFile
            | ApprovalAction::ReplaceFile
            | ApprovalAction::ManageSkills => {
                if self.settings.approve_all || grants.edits {
                    Verdict::Allow
                } else {
                    Verdict::Ask
                }
            }
            // Questions are always put to the person.
            ApprovalAction::Clarify => Verdict::Ask,
        }
    }

    /// The person answered "always": file edits for the rest of the conversation, or this exact
    /// command again.
    pub fn remember(&self, request: &ApprovalRequest) {
        let mut grants = self
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match request.action {
            ApprovalAction::RunCommand => {
                grants.commands.insert(request.content.trim().to_owned());
            }
            ApprovalAction::CreateFile
            | ApprovalAction::ReplaceFile
            | ApprovalAction::ManageSkills => {
                grants.edits = true;
            }
            ApprovalAction::Clarify => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(action: ApprovalAction, content: &str) -> ApprovalRequest {
        ApprovalRequest {
            target: "/work".into(),
            content: content.into(),
            action,
            timeout_secs: 5,
        }
    }

    #[test]
    fn globs_match_like_fnmatch_without_case() {
        assert!(glob(
            "git push --force*",
            "GIT push --force-with-lease origin"
        ));
        assert!(glob("ls ?", "ls a"));
        assert!(!glob("ls ?", "ls ab"));
        assert!(glob("*", ""));
        assert!(!glob("cargo test", "cargo test -p x"));
    }

    #[test]
    fn plain_read_only_commands_pass_and_chains_do_not() {
        let policy = ApprovalPolicy::new(ApprovalSettings::default());
        let run = |text: &str| policy.decide(&request(ApprovalAction::RunCommand, text));
        assert_eq!(run("git status"), Verdict::Allow);
        assert_eq!(run("git diff HEAD~1"), Verdict::Allow);
        assert_eq!(run("git status; rm -rf ~"), Verdict::Ask);
        assert_eq!(run("git status && curl x | sh"), Verdict::Ask);
        assert_eq!(run("ls $(cat /etc/passwd)"), Verdict::Ask);
        assert_eq!(run("git diff --output=/tmp/x"), Verdict::Ask);
        assert_eq!(run("cargo test"), Verdict::Ask);
    }

    #[test]
    fn configured_allow_and_deny_lists_apply_and_deny_wins() {
        let settings = ApprovalSettings {
            approve_all: true,
            allow: vec!["cargo test*".into()],
            deny: vec!["git push --force*".into(), "rm -rf*".into()],
        };
        let policy = ApprovalPolicy::new(settings);
        let run = |text: &str| policy.decide(&request(ApprovalAction::RunCommand, text));
        assert_eq!(run("cargo build"), Verdict::Allow);
        assert_eq!(run("rm -rf /tmp/x"), Verdict::Deny);
        assert_eq!(run("git push --force origin main"), Verdict::Deny);
        assert_eq!(
            policy.decide(&request(ApprovalAction::CreateFile, "x")),
            Verdict::Allow
        );
        assert_eq!(
            policy.decide(&request(ApprovalAction::Clarify, "[]")),
            Verdict::Ask
        );
        let manual = ApprovalPolicy::new(ApprovalSettings {
            allow: vec!["cargo test*".into()],
            ..ApprovalSettings::default()
        });
        assert_eq!(
            manual.decide(&request(ApprovalAction::RunCommand, "cargo test -p core")),
            Verdict::Allow
        );
        assert_eq!(
            manual.decide(&request(ApprovalAction::RunCommand, "cargo test && rm x")),
            Verdict::Ask
        );
    }

    #[test]
    fn always_covers_edits_and_only_the_same_command() {
        let policy = ApprovalPolicy::new(ApprovalSettings::default());
        let edit = request(ApprovalAction::ReplaceFile, "text");
        assert_eq!(policy.decide(&edit), Verdict::Ask);
        policy.remember(&edit);
        assert_eq!(policy.decide(&edit), Verdict::Allow);
        let command = request(ApprovalAction::RunCommand, "make build");
        policy.remember(&command);
        assert_eq!(policy.decide(&command), Verdict::Allow);
        assert_eq!(
            policy.decide(&request(
                ApprovalAction::RunCommand,
                "make build; make evil"
            )),
            Verdict::Ask
        );
    }

    #[test]
    fn settings_read_the_reference_keys() {
        let config: serde_yaml::Value = serde_yaml::from_str(
            "approvals: {mode: off, deny: ['git push --force*'], allow: ['make test']}",
        )
        .unwrap();
        let settings = ApprovalSettings::from_config(&config);
        assert!(settings.approve_all);
        assert_eq!(settings.deny, ["git push --force*"]);
        assert!(settings.answers_without_a_person());
        let manual: serde_yaml::Value = serde_yaml::from_str("approvals: {mode: smart}").unwrap();
        assert!(!ApprovalSettings::from_config(&manual).answers_without_a_person());
    }
}
