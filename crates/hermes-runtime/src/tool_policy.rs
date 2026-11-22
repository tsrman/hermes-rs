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

/// Tools the reference hides behind the tool-search bridge unless the profile says otherwise.
static DEFAULT_DEFERRED: LazyLock<Vec<String>> = LazyLock::new(|| {
    #[derive(Deserialize)]
    struct Defaults {
        defer: Vec<String>,
    }
    serde_json::from_str::<Defaults>(include_str!("tool_search_defaults.json"))
        .expect("valid tool_search_defaults.json")
        .defer
});

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolPolicy {
    /// `None` allows every tool, as without a profile.
    allowed: Option<BTreeSet<String>>,
    /// Tools served through the tool-search bridge instead of being listed in every request.
    /// Empty when the bridge is off, so every allowed tool is listed.
    #[serde(default)]
    deferred: BTreeSet<String>,
    /// How the bridge searches and lists the deferred tools.
    #[serde(default)]
    search: SearchSettings,
    /// Tools withheld whatever the toolsets say (a subagent's blocked tools).
    #[serde(default)]
    denied: BTreeSet<String>,
}

/// `tools.tool_search`'s limits and listing, clamped as the reference clamps them.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SearchSettings {
    pub default_limit: usize,
    pub max_limit: usize,
    /// `listing: off` sends the bare bridge without the catalog in its description.
    pub listing: bool,
    pub listing_max_tokens: usize,
    pub threshold_pct: f64,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            default_limit: 5,
            max_limit: 25,
            listing: true,
            listing_max_tokens: 4000,
            threshold_pct: 5.0,
        }
    }
}

fn clamped(value: Option<&serde_yaml::Value>, fallback: i64, low: i64, high: i64) -> i64 {
    let parsed = match value {
        Some(serde_yaml::Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Some(serde_yaml::Value::String(text)) => text.trim().parse().ok(),
        Some(serde_yaml::Value::Bool(flag)) => Some(i64::from(*flag)),
        _ => None,
    };
    parsed.unwrap_or(fallback).clamp(low, high)
}

impl SearchSettings {
    fn from_config(config: &serde_yaml::Value) -> Self {
        let section = config
            .get("tools")
            .and_then(|tools| tools.get("tool_search"))
            .filter(|section| section.is_mapping());
        let get = |key: &str| section.and_then(|section| section.get(key));
        let max_limit = clamped(get("max_search_limit"), 25, 1, 50);
        let listing = match get("listing") {
            None => true,
            Some(serde_yaml::Value::Bool(flag)) => *flag,
            Some(other) => {
                let text = match other {
                    serde_yaml::Value::String(text) => text.trim().to_lowercase(),
                    other => serde_yaml::to_string(other)
                        .unwrap_or_default()
                        .trim()
                        .to_lowercase(),
                };
                !matches!(text.as_str(), "off" | "false" | "0" | "no")
            }
        };
        let threshold = match get("threshold_pct") {
            Some(serde_yaml::Value::Number(number)) => number.as_f64(),
            Some(serde_yaml::Value::String(text)) => text.trim().parse().ok(),
            _ => None,
        };
        Self {
            default_limit: clamped(get("search_default_limit"), 5, 1, max_limit) as usize,
            max_limit: max_limit as usize,
            listing,
            listing_max_tokens: clamped(get("listing_max_tokens"), 4000, 200, 60000) as usize,
            threshold_pct: threshold.unwrap_or(5.0).clamp(0.0, 100.0),
        }
    }
}

impl ToolPolicy {
    pub fn allow_all() -> Self {
        Self {
            allowed: None,
            deferred: BTreeSet::new(),
            search: SearchSettings::default(),
            denied: BTreeSet::new(),
        }
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
            deferred: deferred_tools(config),
            search: SearchSettings::from_config(config),
            denied: BTreeSet::new(),
        }
    }

    /// Whether the tool is reached through the tool-search bridge rather than listed directly.
    pub fn is_deferred(&self, tool: &str) -> bool {
        self.deferred.contains(tool)
    }

    pub fn search_settings(&self) -> SearchSettings {
        self.search
    }

    /// A copy that defers nothing: what the tool list would be without the bridge.
    pub(crate) fn without_deferral(&self) -> Self {
        Self {
            deferred: BTreeSet::new(),
            ..self.clone()
        }
    }

    /// A copy without `tools`: offered to nobody, callable by nobody, bridge included.
    pub(crate) fn without_tools(&self, tools: &[&str]) -> Self {
        let mut copy = self.clone();
        copy.denied
            .extend(tools.iter().map(|tool| (*tool).to_owned()));
        copy
    }

    pub fn allows(&self, tool: &str) -> bool {
        !self.denied.contains(tool)
            && self
                .allowed
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

/// The tools behind the bridge: `tools.tool_search.defer` (default: the reference's curated list;
/// an explicit list, even an empty one, replaces it), none when `tools.tool_search.enabled` is off.
fn deferred_tools(config: &serde_yaml::Value) -> BTreeSet<String> {
    let section = config
        .get("tools")
        .and_then(|tools| tools.get("tool_search"));
    let enabled = match section {
        Some(serde_yaml::Value::Bool(flag)) => *flag,
        Some(serde_yaml::Value::Mapping(_)) => {
            let raw = section.and_then(|s| s.get("enabled"));
            let text = match raw {
                None => "auto".to_owned(),
                Some(serde_yaml::Value::Bool(flag)) => flag.to_string(),
                Some(other) => serde_yaml::to_string(other)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            };
            !matches!(text.to_lowercase().as_str(), "false" | "0" | "no" | "off")
        }
        _ => true,
    };
    if !enabled {
        return BTreeSet::new();
    }
    match section.and_then(|section| section.get("defer")) {
        Some(serde_yaml::Value::Sequence(names)) => names
            .iter()
            .filter_map(|name| name.as_str())
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .collect(),
        // A scalar is a typo; the reference falls back to its default list.
        _ => DEFAULT_DEFERRED.iter().cloned().collect(),
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
    const IMPLEMENTED: [&str; 11] = [
        "clarify",
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
    fn deferral_defaults_to_the_curated_list_and_follows_the_config() {
        let deferred = |yaml: &str, tool: &str| {
            ToolPolicy::from_config(&serde_yaml::from_str(yaml).unwrap()).is_deferred(tool)
        };
        assert!(deferred("{}", "todo_list"));
        assert!(deferred("{}", "process_manage"));
        assert!(!deferred("{}", "clarify"));
        assert!(!deferred("{}", "memory"));
        assert!(!deferred(
            "tools: {tool_search: {enabled: off}}",
            "todo_list"
        ));
        assert!(!deferred("tools: {tool_search: false}", "todo_list"));
        assert!(deferred(
            "tools: {tool_search: {enabled: auto}}",
            "todo_list"
        ));
        // An explicit list replaces the default wholesale; an empty one defers nothing.
        assert!(!deferred("tools: {tool_search: {defer: []}}", "todo_list"));
        assert!(deferred(
            "tools: {tool_search: {defer: [memory]}}",
            "memory"
        ));
        assert!(!deferred(
            "tools: {tool_search: {defer: [memory]}}",
            "todo_list"
        ));
        assert!(deferred(
            "tools: {tool_search: {defer: nonsense}}",
            "todo_list"
        ));
    }

    #[test]
    fn without_a_policy_everything_is_allowed() {
        assert!(ToolPolicy::allow_all().allows("anything"));
        assert!(ToolPolicy::default().allows("anything"));
    }
}
