//! `agent.reasoning_effort`, `agent.reasoning_overrides` and `agent.text_verbosity`: what they put on
//! the wire. A port of `hermes_constants.resolve_reasoning_config` and the `custom` provider's
//! `build_api_kwargs_extras`, recorded in `compat/create_request_fixture.py`.
//!
//! Only the `custom` chat-completions route is covered: the reference sends
//! `reasoning_effort: medium` there when nothing is configured, and the configured level (clamped
//! to the OpenAI-compatible set) otherwise. Named providers keep the body their profile gives them.

use serde_yaml::Value as Yaml;

const VALID_EFFORTS: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const KNOWN_PROVIDERS: [&str; 12] = [
    "anthropic",
    "openai",
    "google",
    "openrouter",
    "groq",
    "mistral",
    "xai",
    "cohere",
    "perplexity",
    "together",
    "fireworks",
    "deepseek",
];
const KNOWN_AGGREGATORS: [&str; 5] = ["openrouter", "opencode", "fireworks", "groq", "together"];

/// What a reasoning setting asks for.
#[derive(Clone, Debug, PartialEq)]
enum Asked {
    Off,
    Effort(String),
}

#[derive(Clone, Default)]
pub struct ReasoningSettings {
    effort: Yaml,
    overrides: Vec<(String, Yaml)>,
    verbosity: Option<String>,
}

/// YAML 1.1 reads the bare words `no`, `off` and `n` as false, which is how the reference sees a
/// config written that way. The parser here gives them as text; a quoted one cannot be told apart
/// (the reference would read it as the word, not as false).
fn yaml_false(value: &Yaml) -> bool {
    match value {
        Yaml::Bool(flag) => !flag,
        Yaml::String(word) => matches!(
            word.as_str(),
            "no" | "No" | "NO" | "off" | "Off" | "OFF" | "n" | "N"
        ),
        _ => false,
    }
}

/// `parse_reasoning_effort`: `None` for an empty or unknown setting (the default applies).
fn parse_effort(value: &Yaml) -> Option<Asked> {
    match value {
        Yaml::Null | Yaml::Bool(true) => None,
        Yaml::Mapping(map) => {
            if map.get("enabled").is_some_and(yaml_false) {
                return Some(Asked::Off);
            }
            let level = match map.get("effort")? {
                Yaml::String(text) => text.trim().to_owned(),
                Yaml::Number(number) if number.as_f64() != Some(0.0) => number.to_string(),
                _ => return None,
            };
            (!level.is_empty()).then_some(Asked::Effort(level))
        }
        other => {
            let word = if yaml_false(other) {
                "false".to_owned()
            } else {
                match other {
                    Yaml::String(text) => text.trim().to_lowercase(),
                    _ => return None,
                }
            };
            if matches!(word.as_str(), "none" | "false" | "disabled") {
                Some(Asked::Off)
            } else {
                VALID_EFFORTS
                    .contains(&word.as_str())
                    .then_some(Asked::Effort(word))
            }
        }
    }
}

/// `needs_reasoning_echo` for a `custom` route (the reference's `_REASONING_ECHO_RULES`): a model
/// name that holds `deepseek`, `mimo` or `stealth/missingno`, or a Kimi, DeepSeek or MiMo host.
fn echoes_reasoning(model: &str, base_url: &str) -> bool {
    let model = model.to_lowercase();
    if ["deepseek", "mimo", "stealth/missingno"]
        .iter()
        .any(|part| model.contains(part))
    {
        return true;
    }
    let (host, _) = host_and_port(base_url);
    [
        "api.kimi.com",
        "moonshot.ai",
        "moonshot.cn",
        "api.deepseek.com",
        "api.xiaomimimo.com",
        "xiaomimimo.com",
    ]
    .iter()
    .any(|domain| host_matches(&host, domain))
}

/// The lowercased host (no trailing dot) and the port of a base URL.
fn host_and_port(base_url: &str) -> (String, Option<u16>) {
    let rest = base_url
        .trim()
        .split_once("://")
        .map_or(base_url.trim(), |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || host.starts_with('[') => {
            (host, port.parse().ok())
        }
        _ => (authority, None),
    };
    (host.to_lowercase().trim_end_matches('.').to_owned(), port)
}

/// `base_url_host_matches`: the host is the domain or one of its subdomains.
fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The reference's `is_xai_responses`: the `xai` provider or the host `api.x.ai`. Its Responses
/// endpoint rejects unknown top-level fields such as `text`.
pub fn is_xai_responses(provider: &str, base_url: &str) -> bool {
    matches!(provider, "xai" | "xai-oauth") || host_and_port(base_url).0 == "api.x.ai"
}

/// `_looks_like_ollama_endpoint`: port 11434, ollama.com, or an `ollama` label in the host.
fn looks_like_ollama(base_url: &str) -> bool {
    let (host, port) = host_and_port(base_url);
    port == Some(11434)
        || host_matches(&host, "ollama.com")
        || host.split('.').any(|label| label == "ollama")
}

fn digit(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_digit())
}

/// `re.sub(r'(\d)<from>(\d)', r'\1<to>\2', text)`: matches do not overlap.
fn swap_between_digits(text: &str, from: char, to: char) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if digit(chars.get(index).copied())
            && chars.get(index + 1) == Some(&from)
            && digit(chars.get(index + 2).copied())
        {
            out.push(chars[index]);
            out.push(to);
            out.push(chars[index + 2]);
            index += 3;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

/// `_canonical_model_variants`: spellings of a model name, the exact one first.
fn variants(model: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    fn add(seen: &mut Vec<String>, value: String) {
        if !value.is_empty() && !seen.contains(&value) {
            seen.push(value);
        }
    }
    fn add_with_derivatives(seen: &mut Vec<String>, text: &str) {
        let dashed = text.replace('.', "-");
        let dotted = text.replace('-', ".");
        for value in [
            text.to_owned(),
            dashed.clone(),
            dotted.clone(),
            swap_between_digits(text, '-', '.'),
            swap_between_digits(text, '.', '-'),
            swap_between_digits(&dashed, '-', '.'),
            swap_between_digits(&dotted, '.', '-'),
        ] {
            add(seen, value);
        }
    }
    add_with_derivatives(&mut seen, model);
    let parts: Vec<&str> = model.split('/').collect();
    if parts.len() >= 2 {
        add_with_derivatives(&mut seen, parts[parts.len() - 1]);
    }
    if parts.len() >= 3 {
        add_with_derivatives(&mut seen, &parts[1..].join("/"));
    }
    let bare: Vec<String> = seen.iter().filter(|v| !v.contains('/')).cloned().collect();
    for variant in &bare {
        for provider in KNOWN_PROVIDERS {
            add(&mut seen, format!("{provider}/{variant}"));
        }
    }
    let single: Vec<String> = seen
        .iter()
        .filter(|v| v.matches('/').count() == 1)
        .cloned()
        .collect();
    for variant in &single {
        for aggregator in KNOWN_AGGREGATORS {
            add(&mut seen, format!("{aggregator}/{variant}"));
        }
    }
    seen
}

impl ReasoningSettings {
    pub fn from_config(config: &Yaml) -> Self {
        let agent = &config["agent"];
        let overrides = agent["reasoning_overrides"]
            .as_mapping()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(key, value)| Some((key.as_str()?.to_owned(), value.clone())))
                    .collect()
            })
            .unwrap_or_default();
        // Only low, medium and high are sent; anything else means "not set".
        let verbosity = agent["text_verbosity"]
            .as_str()
            .map(|text| text.trim().to_lowercase())
            .filter(|text| matches!(text.as_str(), "low" | "medium" | "high"));
        Self {
            effort: agent["reasoning_effort"].clone(),
            overrides,
            verbosity,
        }
    }

    /// `resolve_per_model_reasoning_effort`: an exact or respelled key first, then keys that carry a
    /// provider prefix the model name lost.
    fn per_model(&self, model: &str) -> Option<Asked> {
        if self.overrides.is_empty() || model.is_empty() {
            return None;
        }
        let spellings = variants(model);
        let lookup = |key: &str| {
            self.overrides
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value)
        };
        for spelling in &spellings {
            if let Some(asked) = lookup(spelling).and_then(parse_effort) {
                return Some(asked);
            }
        }
        for (key, value) in &self.overrides {
            if !key.contains('/') {
                continue;
            }
            let parts: Vec<&str> = key.split('/').collect();
            let mut forms = variants(parts[parts.len() - 1]);
            if parts.len() >= 3 {
                forms.extend(variants(&parts[1..].join("/")));
            }
            if forms.iter().any(|form| spellings.contains(form)) {
                if let Some(asked) = parse_effort(value) {
                    return Some(asked);
                }
            }
        }
        None
    }

    /// The `reasoning_effort` a `custom` chat-completions request carries for `model`.
    pub fn custom_effort(&self, model: &str) -> String {
        let asked = self
            .per_model(model)
            .or_else(|| parse_effort(&self.effort))
            .unwrap_or_else(|| Asked::Effort("medium".into()));
        match asked {
            Asked::Off => "none".into(),
            Asked::Effort(level) => {
                let level = level.trim().to_lowercase();
                // `clamp_effort`: levels the wire knows and names it does not know pass through;
                // `ultra` is stepped down to the nearest weaker level, `max`.
                if level == "ultra" {
                    "max".into()
                } else {
                    level
                }
            }
        }
    }

    /// The fields the `custom` profile adds to a chat-completions request for `model` at `base_url`
    /// (its `build_api_kwargs_extras`): `reasoning_effort`, which Groq accepts only as `default` or
    /// `none`, and `think: false` when reasoning is off on an Ollama endpoint.
    ///
    /// Deliberate differences from the reference, both for the case where nothing is configured and
    /// so the reference would send `medium`: the field is left off (a) on routes that insist on
    /// their reasoning being echoed back (DeepSeek, Kimi, MiMo), because the reference pads those
    /// replays with `reasoning_content: " "` and the port does not replay reasoning at all, so
    /// asking such a route to think could end a tool turn in a 400; (b) on Ollama endpoints, where
    /// the reference sends nothing for a model without the `thinking` capability and the port
    /// cannot probe `/api/show`. A configured level is always sent.
    pub fn custom_fields(
        &self,
        model: &str,
        base_url: &str,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = serde_json::Map::new();
        let configured = self.per_model(model).or_else(|| parse_effort(&self.effort));
        let ollama = looks_like_ollama(base_url);
        if configured.is_none() && (ollama || echoes_reasoning(model, base_url)) {
            return fields;
        }
        let (host, _) = host_and_port(base_url);
        let mut effort = self.custom_effort(model);
        if effort == "none" {
            if ollama {
                fields.insert("think".into(), false.into());
            }
        } else if host_matches(&host, "api.groq.com") {
            effort = "default".to_owned();
        }
        fields.insert("reasoning_effort".into(), effort.into());
        fields
    }

    /// `agent.text_verbosity` when it is low, medium or high, for the Responses `text` field.
    pub fn verbosity(&self) -> Option<&str> {
        self.verbosity.as_deref()
    }
}
