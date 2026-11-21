//! Which tools the profile's toolset configuration exposes to the CLI.
//!
//! Mirrors `hermes_cli.tools_config._get_platform_tools` for the `cli` platform:
//! `platform_toolsets.cli` names the enabled toolsets (default `hermes-cli`) and
//! `agent.disabled_toolsets` removes toolsets, and every tool they contain, afterwards.
//! `toolsets.json` is generated from the reference by `compat/create_tool_schema_fixture.py`.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

static TOOLSETS: LazyLock<HashMap<String, Vec<String>>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("toolsets.json")).expect("valid toolsets.json")
});

fn resolve(name: &str) -> &'static [String] {
    TOOLSETS.get(name).map_or(&[], Vec::as_slice)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolPolicy {
    /// `None` allows every tool, as without a profile.
    allowed: Option<BTreeSet<String>>,
}

impl ToolPolicy {
    pub fn allow_all() -> Self {
        Self { allowed: None }
    }

    pub(crate) fn from_config(config: &serde_yaml::Value) -> Self {
        Self::for_platform(config, "cli")
    }

    /// The policy of `platform` (`cli`, `cron`): its `platform_toolsets` entry, else the
    /// platform's composite `hermes-<platform>`.
    pub(crate) fn for_platform(config: &serde_yaml::Value, platform: &str) -> Self {
        let listed = |value: Option<&serde_yaml::Value>| -> Option<Vec<String>> {
            match value? {
                serde_yaml::Value::Sequence(items) => Some(
                    items
                        .iter()
                        .map(|item| match item {
                            serde_yaml::Value::String(text) => text.clone(),
                            other => serde_yaml::to_string(other)
                                .unwrap_or_default()
                                .trim()
                                .to_owned(),
                        })
                        .collect(),
                ),
                serde_yaml::Value::String(text) => Some(split_names(text)),
                _ => None,
            }
        };
        let names = listed(
            config
                .get("platform_toolsets")
                .and_then(|sets| sets.get(platform)),
        )
        .unwrap_or_else(|| vec![format!("hermes-{platform}")]);
        let mut allowed: BTreeSet<String> = BTreeSet::new();
        for name in names {
            // Older configs name the composite just "hermes".
            let expanded: &[&str] = if name == "hermes" {
                &["hermes-cli", "hermes-api-server"]
            } else {
                &[]
            };
            if expanded.is_empty() {
                allowed.extend(resolve(&name).iter().cloned());
            }
            for alias in expanded {
                allowed.extend(resolve(alias).iter().cloned());
            }
        }
        // A disabled toolset removes every tool it contains, composites included.
        let disabled = config
            .get("agent")
            .and_then(|agent| agent.get("disabled_toolsets"))
            .and_then(|value| listed(Some(value)));
        for name in disabled.into_iter().flatten() {
            for tool in resolve(name.trim()) {
                allowed.remove(tool);
            }
        }
        Self {
            allowed: Some(allowed),
        }
    }

    pub fn allows(&self, tool: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(tool))
    }

    /// The skills index is built only when a skills tool is enabled.
    pub fn allows_skills(&self) -> bool {
        ["skills_list", "skill_view", "skill_manage"]
            .iter()
            .any(|tool| self.allows(tool))
    }
}

/// A list written as a JSON array string (`"['memory']"`) or as comma-separated names.
fn split_names(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        let json = trimmed.replace('\'', "\"");
        if let Ok(names) = serde_json::from_str::<Vec<String>>(&json) {
            return names;
        }
    }
    trimmed
        .split(',')
        .map(|name| name.trim().trim_matches(['[', ']', '\'', '"']).to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tools this crate implements; the recorded cases list only these.
    const IMPLEMENTED: [&str; 10] = [
        "read_file",
        "write_file",
        "patch",
        "search_files",
        "terminal",
        "skills_list",
        "skill_view",
        "web_search",
        "web_extract",
        "memory",
    ];

    #[test]
    fn matches_the_tools_the_reference_exposes_for_sample_configs() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/toolset_cases.json")).unwrap();
        assert!(cases.len() >= 10);
        for case in cases {
            let config: serde_yaml::Value =
                serde_yaml::from_str(&case["config"].to_string()).unwrap();
            let policy = ToolPolicy::from_config(&config);
            let mut exposed: Vec<&str> = IMPLEMENTED
                .into_iter()
                .filter(|tool| policy.allows(tool))
                .collect();
            exposed.sort_unstable();
            let expected: Vec<&str> = case["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t.as_str())
                .collect();
            assert_eq!(exposed, expected, "{}", case["name"]);
        }
    }

    #[test]
    fn without_a_policy_everything_is_allowed() {
        assert!(ToolPolicy::allow_all().allows("anything"));
        assert!(ToolPolicy::default().allows("anything"));
    }
}
