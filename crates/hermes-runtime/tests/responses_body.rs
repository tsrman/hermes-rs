//! Reference corpus replay for OpenAI Responses body fields
//! (`prompt_cache_key`, `reasoning`, `include`, `text`) from
//! `compat/create_responses_body_corpus.py` and `compat/create_request_fixture.py`.

use std::time::Duration;

use hermes_runtime::providers::{Extras, ResponsesProvider};
use hermes_runtime::reasoning::{is_xai_responses, ReasoningSettings};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
struct ReasoningCase {
    label: String,
    provider: String,
    base_url: String,
    model: String,
    agent: String,
    reasoning: Option<Value>,
    include: Option<Value>,
    text: Option<Value>,
}

#[derive(Deserialize)]
struct CacheKeyCase {
    label: String,
    provider: String,
    base_url: String,
    instructions: String,
    with_tools: bool,
    session_id: Option<String>,
    override_key: Option<String>,
    prompt_cache_key: Option<String>,
    x_grok_conv_id: Option<String>,
}

#[derive(Deserialize)]
struct Corpus {
    sample_tools: Vec<Value>,
    reasoning_cases: Vec<ReasoningCase>,
    cache_key_cases: Vec<CacheKeyCase>,
}

fn corpus() -> Corpus {
    serde_json::from_str(include_str!("fixtures/responses_body_cases.json")).unwrap()
}

fn agent_requests() -> Value {
    serde_json::from_str(include_str!("fixtures/agent_requests.json")).unwrap()
}

fn reasoning_from_agent_yaml(agent_yaml: &str) -> ReasoningSettings {
    let config = serde_yaml::from_str(&format!("agent: {agent_yaml}\n")).unwrap();
    ReasoningSettings::from_config(&config)
}

#[test]
fn responses_reasoning_include_and_text_match_reference_corpus() {
    let data = corpus();
    assert!(!data.reasoning_cases.is_empty());
    for case in data.reasoning_cases {
        let settings = reasoning_from_agent_yaml(&case.agent);
        let fields = settings.responses_fields(&case.model, &case.provider, &case.base_url);
        assert_eq!(
            fields.get("reasoning").cloned(),
            case.reasoning,
            "case={} reasoning mismatch",
            case.label
        );
        assert_eq!(
            fields.get("include").cloned(),
            case.include,
            "case={} include mismatch",
            case.label
        );
        assert_eq!(
            fields.get("text").cloned(),
            case.text,
            "case={} text mismatch",
            case.label
        );
    }
}

#[test]
fn responses_fields_match_recorded_agent_requests_codex_responses() {
    let rows = agent_requests();
    let cases = rows["codex_responses"].as_array().unwrap();
    assert!(!cases.is_empty());
    for row in cases {
        let agent = row["agent"].as_str().unwrap();
        let model = row["model"].as_str().unwrap();
        let body = &row["body"];
        let settings = reasoning_from_agent_yaml(agent);
        let fields = settings.responses_fields(model, "custom", "http://127.0.0.1:9/v1");
        assert_eq!(
            fields.get("reasoning").cloned(),
            body.get("reasoning").cloned(),
            "agent={agent} reasoning mismatch"
        );
        assert_eq!(
            fields.get("include").cloned(),
            body.get("include").cloned(),
            "agent={agent} include mismatch"
        );
        assert_eq!(
            fields.get("text").cloned(),
            body.get("text").cloned(),
            "agent={agent} text mismatch"
        );
    }
}

#[test]
fn responses_prompt_cache_key_and_xai_conv_header_match_reference_corpus() {
    let data = corpus();
    assert!(!data.cache_key_cases.is_empty());
    for case in data.cache_key_cases {
        let mut extras = Extras::default();
        if let Some(override_key) = case.override_key {
            extras
                .body
                .insert("prompt_cache_key".into(), Value::String(override_key));
        }
        let provider = ResponsesProvider::new(&case.base_url, "k", Duration::from_secs(5))
            .unwrap()
            .with_xai(is_xai_responses(&case.provider, &case.base_url))
            .with_session_id(case.session_id.clone())
            .with_extras(extras);
        let mut req = json!({
            "model": "fixture",
            "messages": [
                {"role": "system", "content": case.instructions},
                {"role": "user", "content": "hi"}
            ]
        });
        if case.with_tools {
            req["tools"] = Value::Array(data.sample_tools.clone());
        }
        let (body, grok_conv_id) = provider.build_request_body(&req, true).unwrap();
        assert_eq!(
            body.get("prompt_cache_key")
                .and_then(Value::as_str)
                .map(str::to_owned),
            case.prompt_cache_key,
            "case={} prompt_cache_key mismatch",
            case.label
        );
        assert_eq!(
            grok_conv_id, case.x_grok_conv_id,
            "case={} x-grok-conv-id mismatch",
            case.label
        );
    }
}
