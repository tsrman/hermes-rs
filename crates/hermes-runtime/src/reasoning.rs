//! `agent.reasoning_effort`, `agent.reasoning_overrides` and `agent.text_verbosity`: what they put on
//! the wire. A port of `hermes_constants.resolve_reasoning_config`, the `custom` provider's
//! `build_api_kwargs_extras`, and `agent/transports/codex.py` Responses reasoning/include/text
//! shaping (`compat/create_request_fixture.py`, `compat/create_responses_body_corpus.py`).

use serde_json::{json, Map, Value};
use serde_yaml::Value as Yaml;

const VALID_EFFORTS: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const EFFORT_LADDER: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];
const OPENAI_COMPAT_WIRE_EFFORTS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const CODEX_GPT56_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
const CODEX_LEGACY_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];
const CODEX_ASTRA_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const XAI_GROK46_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
const XAI_LEGACY_EFFORTS: &[&str] = &["low", "medium", "high"];
const ASTRA_MODEL_IDS: [&str; 2] = ["gpt-6-astra", "gpt-6-astra-900k"];
const GPT62_MODEL_IDS: [&str; 2] = ["gpt-6.2-astra", "gpt-6.2-sol"];
const GPT6_TIER_PREFIXES: [&str; 2] = ["gpt-6-sol", "gpt-6-luna"];
const NO_DISABLE_TIER_PREFIXES: [&str; 1] = ["gpt-6.1-sol"];
const DAYBREAK_MODEL_IDS: [&str; 2] = ["gpt-daybreak-blue-latest", "gpt-daybreak-blue-latest-900k"];
const OPENAI_ALL_TURNS_FAMILIES: [&str; 4] = ["gpt-5.4", "gpt-5.5", "gpt-5.6", "gpt-6"];
const GROK_EFFORT_CAPABLE_PREFIXES: [&str; 5] = [
    "grok-3-mini",
    "grok-4.20-multi-agent",
    "grok-4.3",
    "grok-4.5",
    "grok-4.6",
];
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
enum Asked {
    Off,
    Effort(String),
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct ReasoningSettings {
    #[serde(default)]
    effort: Option<Asked>,
    #[serde(default)]
    overrides: Vec<(String, Asked)>,
    #[serde(default)]
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
                    .filter_map(|(key, value)| {
                        Some((key.as_str()?.to_owned(), parse_effort(value)?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Only low, medium and high are sent; anything else means "not set".
        let verbosity = agent["text_verbosity"]
            .as_str()
            .map(|text| text.trim().to_lowercase())
            .filter(|text| matches!(text.as_str(), "low" | "medium" | "high"));
        Self {
            effort: parse_effort(&agent["reasoning_effort"]),
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
                .map(|(_, value)| value.clone())
        };
        for spelling in &spellings {
            if let Some(asked) = lookup(spelling) {
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
                return Some(value.clone());
            }
        }
        None
    }

    /// The `reasoning_effort` a `custom` chat-completions request carries for `model`.
    pub fn custom_effort(&self, model: &str) -> String {
        let asked = self
            .per_model(model)
            .or_else(|| self.effort.clone())
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
    pub fn custom_fields(&self, model: &str, base_url: &str) -> Map<String, Value> {
        let mut fields = Map::new();
        let configured = self.per_model(model).or_else(|| self.effort.clone());
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

    /// `reasoning`, `include`, and `text` fields for an OpenAI Responses request, matching
    /// `agent/transports/codex.py` (`_resolve_reasoning` + `_reasoning_fields` + `text.verbosity`).
    pub fn responses_fields(
        &self,
        model: &str,
        provider: &str,
        base_url: &str,
    ) -> Map<String, Value> {
        let xai = is_xai_responses(provider, base_url);
        let (host, _) = host_and_port(base_url);
        let openai_origin = host == "api.openai.com";
        let asked = self
            .per_model(model)
            .or_else(|| self.effort.clone())
            .unwrap_or_else(|| Asked::Effort("medium".into()));

        let supported: &[&str] = if xai {
            if is_grok_46_family(model) {
                XAI_GROK46_EFFORTS
            } else {
                XAI_LEGACY_EFFORTS
            }
        } else if !openai_origin && provider.eq_ignore_ascii_case("custom") {
            // CustomProfile.supported_reasoning_efforts returns the widest OpenAI-compat set.
            OPENAI_COMPAT_WIRE_EFFORTS
        } else {
            codex_efforts_for_route(model, &host)
        };

        let (effort, enabled) = if supported.is_empty() {
            (None, false)
        } else {
            match asked {
                Asked::Off => {
                    let has_none = supported.contains(&"none");
                    (has_none.then(|| "none".to_owned()), false)
                }
                Asked::Effort(raw) => (Some(clamp_effort(&raw, supported)), true),
            }
        };

        let mut fields = Map::new();
        if enabled && xai {
            fields.insert("include".into(), json!(["reasoning.encrypted_content"]));
            if grok_supports_reasoning_effort(model) {
                if let Some(effort) = effort {
                    fields.insert("reasoning".into(), json!({"effort": effort}));
                }
            }
        } else if enabled {
            if let Some(mut effort) = effort {
                if is_official_openai_responses_route(model, &host) {
                    effort = if effort.trim().is_empty() {
                        "low".to_owned()
                    } else {
                        clamp_effort(&effort, CODEX_ASTRA_EFFORTS)
                    };
                }
                let mut reasoning = Map::new();
                reasoning.insert("effort".into(), Value::String(effort));
                reasoning.insert("summary".into(), Value::String("auto".into()));
                if openai_origin && openai_reasoning_family(model).is_some() {
                    reasoning.insert("context".into(), Value::String("all_turns".into()));
                }
                fields.insert("reasoning".into(), Value::Object(reasoning));
            }
            fields.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else if !xai {
            fields.insert("include".into(), json!([]));
            if effort.as_deref() == Some("none") {
                fields.insert("reasoning".into(), json!({"effort": "none"}));
            }
        }

        if !xai {
            if let Some(verbosity) = self.verbosity() {
                fields.insert("text".into(), json!({"verbosity": verbosity}));
            }
        }

        fields
    }
}

fn bare_model(model: &str) -> String {
    let trimmed = model.trim().to_ascii_lowercase();
    trimmed
        .rsplit_once('/')
        .map_or(trimmed.as_str(), |(_, tail)| tail)
        .to_owned()
}

fn is_astra_model(model: &str) -> bool {
    ASTRA_MODEL_IDS.contains(&bare_model(model).as_str())
}

fn is_official_openai_responses_route(model: &str, host: &str) -> bool {
    let bare = bare_model(model);
    if GPT62_MODEL_IDS.contains(&bare.as_str()) {
        return matches!(
            host,
            "api.openai.com" | "us.api.openai.com" | "eu.api.openai.com" | "chatgpt.com"
        );
    }
    is_astra_model(model) && host == "api.openai.com"
}

fn openai_model_rejects_reasoning(model: &str) -> bool {
    let bare = bare_model(model);
    let name = bare.strip_prefix("ft:").unwrap_or(&bare);
    if name.starts_with("gpt-3.5") || name.starts_with("chatgpt-") {
        return true;
    }
    if let Some(rest) = name.strip_prefix("gpt-4") {
        return !rest.starts_with(|ch: char| ch.is_ascii_digit());
    }
    false
}

fn codex_supported_efforts(model: &str) -> &'static [&'static str] {
    let bare = bare_model(model);
    if is_astra_model(model)
        || GPT62_MODEL_IDS.contains(&bare.as_str())
        || NO_DISABLE_TIER_PREFIXES
            .iter()
            .any(|prefix| bare.starts_with(prefix))
    {
        return CODEX_ASTRA_EFFORTS;
    }
    if bare.contains("gpt-5.6")
        || GPT6_TIER_PREFIXES
            .iter()
            .any(|prefix| bare.starts_with(prefix))
        || DAYBREAK_MODEL_IDS.contains(&bare.as_str())
    {
        CODEX_GPT56_EFFORTS
    } else {
        CODEX_LEGACY_EFFORTS
    }
}

fn codex_efforts_for_route(model: &str, host: &str) -> &'static [&'static str] {
    if host == "api.openai.com" && openai_model_rejects_reasoning(model) {
        return &[];
    }
    if is_astra_model(model) && !is_official_openai_responses_route(model, host) {
        return CODEX_LEGACY_EFFORTS;
    }
    codex_supported_efforts(model)
}

fn openai_reasoning_family(model: &str) -> Option<&'static str> {
    let name = model.trim().to_ascii_lowercase();
    OPENAI_ALL_TURNS_FAMILIES.into_iter().find(|family| {
        name == *family
            || name.starts_with(&format!("{family}-"))
            || name.starts_with(&format!("{family}."))
    })
}

fn is_grok_46_family(model: &str) -> bool {
    let normalized = bare_model(model).replace('_', "-");
    normalized == "grok-4.6" || normalized.starts_with("grok-4.6-")
}

fn grok_supports_reasoning_effort(model: &str) -> bool {
    let bare = bare_model(model);
    !bare.is_empty()
        && GROK_EFFORT_CAPABLE_PREFIXES
            .iter()
            .any(|prefix| bare.starts_with(prefix))
}

fn clamp_effort(effort: &str, supported: &[&str]) -> String {
    let requested = effort.trim().to_ascii_lowercase();
    if requested.is_empty() || supported.is_empty() {
        return effort.to_owned();
    }
    if supported.contains(&requested.as_str()) {
        return requested;
    }
    let Some(requested_idx) = EFFORT_LADDER.iter().position(|lvl| *lvl == requested) else {
        return effort.to_owned();
    };
    let candidates: Vec<(usize, &str)> = supported
        .iter()
        .filter(|lvl| **lvl != "none")
        .filter_map(|lvl| {
            EFFORT_LADDER
                .iter()
                .position(|entry| entry == lvl)
                .map(|idx| (idx, *lvl))
        })
        .collect();
    if candidates.is_empty() {
        return effort.to_owned();
    }
    if let Some((_, best)) = candidates
        .iter()
        .filter(|(idx, _)| *idx < requested_idx)
        .max_by_key(|(idx, _)| *idx)
    {
        return (*best).to_owned();
    }
    candidates
        .iter()
        .min_by_key(|(idx, _)| *idx)
        .map(|(_, lvl)| (*lvl).to_owned())
        .unwrap_or_else(|| effort.to_owned())
}
