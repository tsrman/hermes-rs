use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hermes_runtime::config::load_chat_settings;
use hermes_runtime::delegation::{parse_child_timeout_seconds, Delegation, DelegationSettings};
use hermes_runtime::prompt::resolve_ephemeral_system_prompt;
use hermes_runtime::providers::bedrock::BedrockSettings;
use hermes_runtime::providers::openai::OpenAiProvider;
use hermes_runtime::providers::Provider;
use hermes_runtime::stall_guards::IntentAckContinuation;
use hermes_runtime::tool_policy::ToolPolicy;
use hermes_runtime::turn::{
    run_turn, ApprovalDecision, ModelRoute, TurnEvent, TurnHistory, TurnOptions, TurnOutcome,
};
use serde_json::{json, Value};

fn single_json_server(status: &str, response_body: String) -> (String, thread::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let status = status.to_owned();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let body_start;
        loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(pos) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                body_start = pos + 4;
                break;
            }
        }
        let headers = String::from_utf8_lossy(&bytes[..body_start]);
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        while bytes.len() - body_start < length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&buffer[..read]);
        }
        let http = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream.write_all(http.as_bytes()).unwrap();
        serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap()
    });
    (format!("http://{address}/v1"), handle)
}

#[test]
fn resolve_ephemeral_system_prompt_follows_upstream_precedence() {
    let kawaii_cfg: serde_yaml::Value = serde_yaml::from_str(
        r#"
display:
  personality: kawaii
agent:
  system_prompt: fallback prompt
"#,
    )
    .unwrap();
    let kawaii = resolve_ephemeral_system_prompt(&kawaii_cfg, None).unwrap();
    assert!(kawaii.contains("kawaii assistant"));
    assert!(kawaii.contains("(◕‿◕)"));

    // Case-insensitive lookup.
    let upper_cfg: serde_yaml::Value = serde_yaml::from_str(
        r#"
display:
  personality: "  KAWAII  "
"#,
    )
    .unwrap();
    assert_eq!(
        resolve_ephemeral_system_prompt(&upper_cfg, None).as_deref(),
        Some(kawaii.as_str())
    );

    // Env override beats display.personality and agent.system_prompt.
    assert_eq!(
        resolve_ephemeral_system_prompt(&kawaii_cfg, Some("  env prompt  ")).as_deref(),
        Some("env prompt")
    );

    // Neutral names fall back to agent.system_prompt (even if a custom map defines one).
    for neutral in ["", "none", "default", "neutral", "DEFAULT"] {
        let cfg: serde_yaml::Value = serde_yaml::from_str(&format!(
            "personalities:\n  none: ignored\n  default: ignored\n  neutral: ignored\ndisplay:\n  personality: '{neutral}'\nagent:\n  system_prompt: custom fallback\n"
        ))
        .unwrap();
        assert_eq!(
            resolve_ephemeral_system_prompt(&cfg, None).as_deref(),
            Some("custom fallback"),
            "neutral={neutral}"
        );
    }

    // agent.personalities overrides root personalities and built-ins; supports structured maps.
    let custom_cfg: serde_yaml::Value = serde_yaml::from_str(
        r#"
personalities:
  helpful: root helpful override
  mentor:
    system_prompt: Guide carefully.
    tone: calm
    style: bullet points
agent:
  personalities:
    helpful: agent helpful override
display:
  personality: helpful
"#,
    )
    .unwrap();
    assert_eq!(
        resolve_ephemeral_system_prompt(&custom_cfg, None).as_deref(),
        Some("agent helpful override")
    );

    let structured_cfg: serde_yaml::Value = serde_yaml::from_str(
        r#"
personalities:
  mentor:
    system_prompt:
      - First line.
      - Second line.
    tone: calm
    style: concise
display:
  personality: mentor
"#,
    )
    .unwrap();
    assert_eq!(
        resolve_ephemeral_system_prompt(&structured_cfg, None).as_deref(),
        Some("First line.\nSecond line.\nTone: calm\nStyle: concise")
    );
}

#[test]
fn load_chat_settings_reads_personality_streaming_timeout_and_env_override() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.yaml"),
        r#"
display:
  personality: kawaii
  streaming: true
streaming:
  enabled: false
delegation:
  child_timeout_seconds: 600
"#,
    )
    .unwrap();
    let loaded = load_chat_settings(Some(home.path())).unwrap();
    assert!(
        loaded.streaming,
        "display.streaming: true overrides streaming.enabled: false"
    );
    assert!(loaded
        .prompt
        .ephemeral_system_prompt
        .as_deref()
        .is_some_and(|p| p.contains("kawaii assistant")));
    assert_eq!(loaded.delegation.child_timeout_seconds, Some(600.0));

    // display.streaming: false disables streaming.
    std::fs::write(
        home.path().join("config.yaml"),
        "display:\n  streaming: false\n",
    )
    .unwrap();
    let loaded = load_chat_settings(Some(home.path())).unwrap();
    assert!(!loaded.streaming);

    // Fallback to streaming.enabled when display.streaming is omitted.
    std::fs::write(
        home.path().join("config.yaml"),
        "streaming:\n  enabled: false\n",
    )
    .unwrap();
    let loaded = load_chat_settings(Some(home.path())).unwrap();
    assert!(!loaded.streaming);

    // .env overrides: HERMES_EPHEMERAL_SYSTEM_PROMPT and DELEGATION_CHILD_TIMEOUT_SECONDS.
    std::fs::write(
        home.path().join("config.yaml"),
        "display:\n  personality: pirate\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join(".env"),
        "HERMES_EPHEMERAL_SYSTEM_PROMPT=\"Secret Persona\"\nDELEGATION_CHILD_TIMEOUT_SECONDS=12\n",
    )
    .unwrap();
    let loaded = load_chat_settings(Some(home.path())).unwrap();
    assert_eq!(
        loaded.prompt.ephemeral_system_prompt.as_deref(),
        Some("Secret Persona")
    );
    assert_eq!(loaded.delegation.child_timeout_seconds, Some(30.0));

    // Explicit YAML 0 disables child timeout even if .env has a value.
    std::fs::write(
        home.path().join("config.yaml"),
        "delegation:\n  child_timeout_seconds: 0\n",
    )
    .unwrap();
    let loaded = load_chat_settings(Some(home.path())).unwrap();
    assert_eq!(loaded.delegation.child_timeout_seconds, None);
}

#[test]
fn parse_child_timeout_seconds_enforces_floor_and_disable_rules() {
    assert_eq!(
        parse_child_timeout_seconds(&serde_yaml::Value::from(0)),
        None
    );
    assert_eq!(
        parse_child_timeout_seconds(&serde_yaml::Value::from(-5)),
        None
    );
    assert_eq!(
        parse_child_timeout_seconds(&serde_yaml::Value::from(10)),
        Some(30.0)
    );
    assert_eq!(
        parse_child_timeout_seconds(&serde_yaml::Value::from(30)),
        Some(30.0)
    );
    assert_eq!(
        parse_child_timeout_seconds(&serde_yaml::Value::from(600)),
        Some(600.0)
    );
    assert_eq!(
        parse_child_timeout_seconds(&serde_yaml::Value::from("45.5")),
        Some(45.5)
    );
    assert_eq!(
        parse_child_timeout_seconds(&serde_yaml::Value::from("0")),
        None
    );
}

#[tokio::test]
async fn openai_non_streaming_parses_text_usage_and_tool_calls() {
    // 1. Text + usage response.
    let text_payload = json!({
        "id": "cmpl-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello non-stream"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    })
    .to_string();
    let (base_url, handle) = single_json_server("200 OK", text_payload);
    let provider = OpenAiProvider::new(&base_url, "test-key", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let mut streamed = String::new();
    let completion = provider
        .complete(&json!({"model": "fixture", "messages": []}), |delta| {
            streamed.push_str(delta)
        })
        .await
        .unwrap();
    let sent = handle.join().unwrap();
    assert_eq!(sent["stream"], json!(false));
    assert!(sent.get("stream_options").is_none());
    assert_eq!(streamed, "Hello non-stream");
    assert_eq!(completion.content, "Hello non-stream");
    assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
    assert_eq!(completion.usage.unwrap().input_tokens, 11);
    assert_eq!(completion.usage.unwrap().output_tokens, 7);

    // 2. Tool call response.
    let tool_payload = json!({
        "id": "cmpl-2",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
    .to_string();
    let (base_url, handle) = single_json_server("200 OK", tool_payload);
    let provider = OpenAiProvider::new(&base_url, "test-key", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let completion = provider
        .complete(&json!({"model": "fixture", "messages": []}), |_| {})
        .await
        .unwrap();
    let sent = handle.join().unwrap();
    assert_eq!(sent["stream"], json!(false));
    assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(completion.tool_calls.len(), 1);
    assert_eq!(completion.tool_calls[0].id, "call_1");
    assert_eq!(completion.tool_calls[0].name, "read_file");
    assert_eq!(completion.tool_calls[0].arguments, "{\"path\":\"a.txt\"}");
}

#[tokio::test]
async fn ephemeral_prompt_applies_to_api_requests_and_failover_without_polluting_saved_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen_server = Arc::clone(&seen);
    let handle = thread::spawn(move || {
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let body_start;
            loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(pos) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    body_start = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&bytes[..body_start]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() - body_start < length {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buffer[..read]);
            }
            let body: Value =
                serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
            seen_server.lock().unwrap().push(body);
            let http = if attempt == 0 {
                // Force primary route failure to verify failover also gets the ephemeral prompt.
                let err = r#"{"error":{"message":"unauthorized"}}"#;
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{err}",
                    err.len()
                )
            } else {
                let ok = json!({
                    "id": "cmpl-ok",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "nya~"},
                        "finish_reason": "stop"
                    }]
                })
                .to_string();
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok}",
                    ok.len()
                )
            };
            stream.write_all(http.as_bytes()).unwrap();
        }
    });

    let base_url = format!("http://{address}/v1");
    let make_route = |model: &str| {
        let provider = OpenAiProvider::new(&base_url, "k", Duration::from_secs(5))
            .unwrap()
            .with_max_attempts(1)
            .with_streaming(false);
        ModelRoute {
            provider: Provider::OpenAi(provider),
            provider_name: "custom",
            model: model.to_owned(),
        }
    };

    let primary = make_route("primary-model");
    let fallback = make_route("backup-model");
    let dir = tempfile::tempdir().unwrap();
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let options = TurnOptions {
        mcp: None,
        web: None,
        browser: None,
        memory: None,
        vision: None,
        vision_route: None,
        image_gen: None,
        tts: None,
        x_search: None,
        root: dir.path(),
        profile: None,
        session_id: None,
        single_query: true,
        max_turns: 4,
        approval_timeout_secs: None,
        terminal_approval_timeout_secs: None,
        terminal_local: false,
        compression: None,
        summarizer: None,
        compaction_guard: None,
        keep_phone_numbers: false,
        stall_guards: true,
        intent_ack_continuation: &ack,
        terminal_env: None,
        clarify_timeout_secs: 0,
        tools: &policy,
        journal: None,
        delegation: None,
        system_prompt_override: None,
        ephemeral_system_prompt: Some("BE KAWAII DESU"),
        child: None,
        delegate_depth: 0,
        cancel_flag: None,
        steer_queue: None,
        prefill: None,
        state_scope: None,
    };

    let mut reply = String::new();
    let outcome = run_turn(
        &primary,
        &[fallback],
        "hi",
        options,
        TurnHistory::default(),
        |ev| {
            if let TurnEvent::Text(text) = ev {
                reply.push_str(text);
            }
        },
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    handle.join().unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert_eq!(reply, "nya~");
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let first_sys = requests[0]["messages"][0]["content"].as_str().unwrap();
    let second_sys = requests[1]["messages"][0]["content"].as_str().unwrap();
    assert!(first_sys.contains("Model: primary-model"));
    assert!(first_sys.ends_with("\n\nBE KAWAII DESU"));
    assert!(second_sys.contains("Model: backup-model"));
    assert!(second_sys.ends_with("\n\nBE KAWAII DESU"));
}

#[tokio::test]
async fn delegation_child_timeout_enforces_deadline_and_returns_timeout_result() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let tool_result_holder = Arc::new(Mutex::new(String::new()));
    let tool_result_server = Arc::clone(&tool_result_holder);
    let server_handle = thread::spawn(move || {
        // Request 0 (parent): asks to delegate_task.
        // Request 1 (child): stalls longer than the child's timeout (handled on its own thread so
        // request 2 from the parent is served immediately once the child times out!).
        // Request 2 (parent): receives the timeout tool result and replies.
        let mut child_thread = None;
        for req_idx in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let holder = Arc::clone(&tool_result_server);
            let mut handle_conn = move || {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let body_start;
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(pos) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        body_start = pos + 4;
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&bytes[..body_start]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() - body_start < length {
                    let more = stream.read(&mut buffer).unwrap();
                    if more == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..more]);
                }
                let body: Value =
                    serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
                if req_idx == 0 {
                    let ok = json!({
                        "id": "p1",
                        "object": "chat.completion",
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "del-1",
                                    "type": "function",
                                    "function": {
                                        "name": "delegate_task",
                                        "arguments": "{\"tasks\":[{\"goal\":\"slow task that stalls\"}]}"
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }]
                    })
                    .to_string();
                    let http = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok}",
                        ok.len()
                    );
                    let _ = stream.write_all(http.as_bytes());
                } else if req_idx == 1 {
                    // Stall the child request so tokio::time::timeout triggers.
                    thread::sleep(Duration::from_secs(2));
                } else {
                    let found = body["messages"]
                        .as_array()
                        .and_then(|m| m.iter().rev().find(|item| item["role"] == "tool"))
                        .and_then(|m| m["content"].as_str())
                        .unwrap_or_default()
                        .to_owned();
                    *holder.lock().unwrap() = found;
                    let ok = json!({
                        "id": "p2",
                        "object": "chat.completion",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "handled timeout"},
                            "finish_reason": "stop"
                        }]
                    })
                    .to_string();
                    let http = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok}",
                        ok.len()
                    );
                    let _ = stream.write_all(http.as_bytes());
                }
            };
            if req_idx == 1 {
                child_thread = Some(thread::spawn(handle_conn));
            } else {
                handle_conn();
            }
        }
        child_thread
    });

    let base_url = format!("http://{address}/v1");
    let provider = OpenAiProvider::new(&base_url, "k", Duration::from_secs(10))
        .unwrap()
        .with_max_attempts(1)
        .with_streaming(false);
    let route = ModelRoute {
        provider: Provider::OpenAi(provider),
        provider_name: "custom",
        model: "fixture".to_owned(),
    };
    let dir = tempfile::tempdir().unwrap();
    let policy = ToolPolicy::default();
    let delegation = Delegation::new(
        DelegationSettings {
            child_timeout_seconds: Some(0.15),
            ..DelegationSettings::default()
        },
        true,
    );
    let ack = IntentAckContinuation::default();
    let options = TurnOptions {
        mcp: None,
        web: None,
        browser: None,
        memory: None,
        vision: None,
        vision_route: None,
        image_gen: None,
        tts: None,
        x_search: None,
        root: dir.path(),
        profile: None,
        session_id: None,
        single_query: true,
        max_turns: 4,
        approval_timeout_secs: None,
        terminal_approval_timeout_secs: None,
        terminal_local: false,
        compression: None,
        summarizer: None,
        compaction_guard: None,
        keep_phone_numbers: false,
        stall_guards: true,
        intent_ack_continuation: &ack,
        terminal_env: None,
        clarify_timeout_secs: 0,
        tools: &policy,
        journal: None,
        delegation: Some(&delegation),
        system_prompt_override: None,
        ephemeral_system_prompt: None,
        child: None,
        delegate_depth: 0,
        cancel_flag: None,
        steer_queue: None,
        prefill: None,
        state_scope: None,
    };

    let started = Instant::now();
    let mut reply = String::new();
    let outcome = run_turn(
        &route,
        &[],
        "delegate now",
        options,
        TurnHistory::default(),
        |ev| {
            if let TurnEvent::Text(text) = ev {
                reply.push_str(text);
            }
        },
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    let elapsed = started.elapsed();
    let child_thread = server_handle.join().unwrap();
    let tool_result_str = tool_result_holder.lock().unwrap().clone();
    if let Some(t) = child_thread {
        let _ = t.join();
    }

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert_eq!(reply, "handled timeout");
    assert!(
        elapsed < Duration::from_secs(2),
        "child timeout should trigger quickly, took {elapsed:?}"
    );
    let parsed: Value = serde_json::from_str(&tool_result_str).unwrap();
    assert_eq!(parsed["results"][0]["status"], json!("timeout"));
    assert_eq!(parsed["results"][0]["exit_reason"], json!("timeout"));
    assert!(parsed["results"][0]["error"]
        .as_str()
        .is_some_and(|e| e.contains("timed out") && e.contains("child_timeout_seconds")));
}

#[test]
fn bedrock_guardrail_reads_snake_case_and_optional_fields() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
bedrock:
  region: us-west-2
  guardrail:
    guardrail_identifier: gr-12345
    guardrail_version: "2"
    stream_processing_mode: async
    trace: disabled
"#,
    )
    .unwrap();
    let settings = BedrockSettings::from_config(&yaml);
    assert_eq!(settings.region.as_deref(), Some("us-west-2"));
    assert_eq!(
        settings.guardrail_config(),
        Some(json!({
            "guardrailIdentifier": "gr-12345",
            "guardrailVersion": "2",
            "streamProcessingMode": "async",
            "trace": "disabled"
        }))
    );

    // Empty guardrail_identifier (as in default config.yaml) disables guardrail.
    let empty_yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
bedrock:
  guardrail:
    guardrail_identifier: ""
    guardrail_version: DRAFT
    stream_processing_mode: sync
    trace: enabled
"#,
    )
    .unwrap();
    assert_eq!(
        BedrockSettings::from_config(&empty_yaml).guardrail_config(),
        None
    );

    // Legacy id/version aliases and default DRAFT version still work.
    let legacy_yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
bedrock:
  guardrail:
    id: gr-legacy
"#,
    )
    .unwrap();
    assert_eq!(
        BedrockSettings::from_config(&legacy_yaml).guardrail_config(),
        Some(json!({
            "guardrailIdentifier": "gr-legacy",
            "guardrailVersion": "DRAFT"
        }))
    );
}
