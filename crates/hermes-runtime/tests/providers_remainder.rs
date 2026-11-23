use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hermes_runtime::config::{
    default_base_url, load_chat_settings, resolve_provider, ChatSettings,
};
use hermes_runtime::providers::bedrock::{
    bedrock_openai_base_url, bedrock_openai_uses_mantle, convert_messages_to_converse,
    convert_tools_to_converse, is_anthropic_bedrock_model, load_aws_credentials_profile,
    normalize_converse_response, parse_bedrock_openai_model_id, resolve_bedrock_region, sign_sigv4,
    AwsAuth, BedrockProvider,
};
use hermes_runtime::providers::nvidia::{is_nvidia_provider, prepare_messages, NVIDIA_BASE_URL};
use hermes_runtime::providers::oauth::{
    anthropic_oauth_reverse_tool_name, anthropic_oauth_wire_names, anthropic_route_is_oauth,
    base64url_encode, decode_jwt_claims, is_anthropic_oauth_token, jwt_is_expiring,
    load_auth_store, minimax_resolve_token_expiry_unix, resolve_mcp_oauth_token,
    resolve_oauth_credentials, save_auth_store, validate_nous_inference_url,
    ANTHROPIC_OAUTH_ONLY_BETAS, CLAUDE_CODE_SYSTEM_PREFIX,
};
use hermes_runtime::providers::vertex::{
    build_service_account_jwt, build_vertex_base_url, mint_token_from_credentials_json,
    resolve_vertex_route, VertexSettings,
};
use hermes_runtime::providers::{
    AnthropicProvider, OpenAiProvider, ProviderError, ResponsesProvider,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// Ephemeral 2048-bit PKCS#8 RSA key generated solely for unit-testing RS256 JWT signing.
// Never used anywhere in production.
const TEST_ONLY_RSA_PKCS8_DER_B64: &str = concat!(
    "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDCgWtgXWQ3hJs+",
    "wACdFnkWSWZHkYnL2aaYTa8Lig5Dih0ZC1Beu/Nhd05+dXufV+kgjLxBk3QoMBEd",
    "dzT49DMxCL4LJ/yEGYB9gpueRo63zzEuQBfY4YItWtkw/3baCkzrBmLpZY1UnYyn",
    "0vDmPN95qM98Wvxi99X1BKvFbh42Xo6JLhiNUgOsw8JxHSRWdqZTjr4XZD4SRKtv",
    "4QQSw8Uc0nMUt6vE2BqwUFpdD5XYCH9J9vfFySL8E7LBFEylCGB1BoyGI1Ss9t/9",
    "b9apWgJ048iNcmiYv+zOVd677ewwgQ378l3HqxdZaIV/B29PMtzsC1ijTGQ8gsnx",
    "hzWzrz+XAgMBAAECggEABgrnPxMnreHcwNJOMpMkJ+EX4FlRIpgcaNbg/UbIsqhz",
    "xvdnA8prATgnpB4aPRUKFQzKPG9MSINVxMiTSLitbnSLEuWnHqN8LmoEU6J1hJhX",
    "Osya8Q3wcWBFJxSIqLVxNkv4fq+8+Ef8uqn++egiOTFZYM+iz6BLcVU30OZe4el4",
    "Hv/f8ZCDzprlUzF+ucJESO6Ct5bPDkhw0oAVK3q2XST9kFvT4z4fIKils8K3XQ7I",
    "m8IkMacRET1QOp5GJkFxJVSn0WOhUA+UjBtfmrbx9nZOrY9S0BEc6kqwInUt1DiL",
    "L1/9WG9RZwtviJeRQ/QmF5NoETLUZGuNokVETAMxoQKBgQD/DURTBu1ZUnVmfkLd",
    "HKXdw5k859FwyG2QkS9qtrLNc8S1MsmTQKxU9dUlu0IJybu3iviRUD9BbIStTsME",
    "UkWbUhRg07x25cVxA92TG1qXatJvlqLjPvFR88vsXYPmV7AIzFzefM+B9hq+v+pY",
    "kOZLbpNW4aRqZ5WzoXZh0kyolQKBgQDDOofUbrJzI6XheEgurN2aZFqkIMvOKDTD",
    "/kP/g0+aohf2i8BL7wvJ58ZZzIA0EYzyrgz930Ab3TdS6LLB+EggUwCcmlRrpeLl",
    "ta732Z9FJC/iSQrSYNuO25g6JWp9X07U6ccyyoXZ/pvpckaZkdyoKEhFlAbVNgBx",
    "nqnnVaFAewKBgQCySZ/dOhZoPg/7GX+7+O6pWrWfsZzibfjI+fRfcF8E50w2Lc5i",
    "tO7CZSYePgwns3EZGHgxfBr7rNQCIfCI8MnfjTzZgqlXo76lBcK5toFZHz8C6hXr",
    "LHCcjQObsrMlLV7Mh67TiAO2IUcI2DcEuiWBzUYJkanRB5/J9bA/KTaVEQKBgDIz",
    "VsIVROW+gOUdXNJDuSf+LiCOHloAlLDhLPMC/PEyRR8siGH+MfXVF3f30e+Ro8Cz",
    "aLvPMFj55UP4eh7OjR1iuoGKXbLgkRuoQDmTSBUrXPS++31lUo/Ff7tT67IZU9gD",
    "1SkGUqzff4/jxquuiiLjWNIrYiZpIGwW9HF7318JAoGBAOT8utCncaY40y4h5bM8",
    "u/18/tYPtsmT2m+kO9YCj7fwCbdvF2S3zA5veVrzKpEc7PvVkX+v5cWL/Z/zxCvJ",
    "QRTRtbUxKa4sBfSdSztPg9HxH+hKl2+5awdwMKn66XEkewQuc3unsyca17kInIne",
    "G+yOKP+wKbLtyTZG/WY1j7Z3"
);

fn test_rsa_pem() -> String {
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for chunk in TEST_ONLY_RSA_PKCS8_DER_B64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    pem
}

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/providers_remainder_cases.json")).unwrap()
}

type CapturedRequests = Arc<Mutex<Vec<(String, HashMap<String, String>, String)>>>;

/// Minimal single-request HTTP capture server returning `(status, content_type, body)`.
async fn start_capture_server(
    status: u16,
    content_type: &'static str,
    response_body: String,
) -> (String, CapturedRequests) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: CapturedRequests = Arc::new(Mutex::new(Vec::new()));
    let cap_clone = Arc::clone(&captured);

    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let header_end;
            loop {
                let n = stream.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    header_end = buf.len();
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            let head_str = String::from_utf8_lossy(&buf[..header_end]).into_owned();
            let mut lines = head_str.lines();
            let req_line = lines.next().unwrap_or_default().to_owned();
            let mut headers = HashMap::new();
            let mut content_len = 0usize;
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    let key = k.trim().to_ascii_lowercase();
                    let val = v.trim().to_owned();
                    if key == "content-length" {
                        content_len = val.parse().unwrap_or(0);
                    }
                    headers.insert(key, val);
                }
            }
            let mut body = buf[header_end..].to_vec();
            while body.len() < content_len {
                let n = stream.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            let body_str =
                String::from_utf8_lossy(&body[..content_len.min(body.len())]).into_owned();
            cap_clone
                .lock()
                .unwrap()
                .push((req_line, headers, body_str));

            let reason = match status {
                200 => "OK",
                400 => "Bad Request",
                _ => "Error",
            };
            let reply = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });

    (format!("http://{addr}"), captured)
}

#[test]
fn reference_corpus_nvidia_vertex_bedrock_and_oauth() {
    let data = corpus();

    // 1. NVIDIA
    let nv = &data["nvidia"];
    assert_eq!(NVIDIA_BASE_URL, nv["base_url"].as_str().unwrap());
    assert!(is_nvidia_provider("nvidia"));
    for alias in nv["aliases"].as_array().unwrap() {
        let a = alias.as_str().unwrap();
        assert!(is_nvidia_provider(a), "alias {a} should match nvidia");
        assert_eq!(resolve_provider(a), Some(("nvidia", false)));
    }
    for case in nv["prepare_messages"].as_array().unwrap() {
        let input = case["input"].as_array().unwrap();
        let expected = case["output"].as_array().unwrap();
        let actual = prepare_messages(input);
        assert_eq!(&actual, expected, "nvidia case {}", case["label"]);
    }

    // 2. Vertex URLs
    for case in data["vertex"]["base_urls"].as_array().unwrap() {
        let actual = build_vertex_base_url(
            case["project_id"].as_str().unwrap(),
            case["region"].as_str().unwrap(),
        );
        assert_eq!(actual, case["url"].as_str().unwrap());
    }

    // 3. Bedrock model routing + Converse conversions
    let br = &data["bedrock"];
    assert_eq!(
        bedrock_openai_base_url("us-east-1"),
        br["mantle_url_us_east_1"].as_str().unwrap()
    );
    for case in br["models"].as_array().unwrap() {
        let id = case["model_id"].as_str().unwrap();
        let parsed = parse_bedrock_openai_model_id(id);
        let expected_parsed = case["parsed_openai"].as_array().map(|arr| {
            (
                arr[0].as_str().unwrap().to_owned(),
                arr[1].as_str().unwrap().to_owned(),
            )
        });
        assert_eq!(parsed, expected_parsed, "parsed_openai for {id}");
        assert_eq!(
            bedrock_openai_uses_mantle(id),
            case["uses_mantle"].as_bool().unwrap(),
            "uses_mantle for {id}"
        );
        assert_eq!(
            is_anthropic_bedrock_model(id),
            case["is_anthropic"].as_bool().unwrap(),
            "is_anthropic for {id}"
        );
    }

    let sample_tools = vec![json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }
    })];
    assert_eq!(
        Value::Array(convert_tools_to_converse(&sample_tools)),
        br["converse_tools"]
    );

    for case in br["converse_messages"].as_array().unwrap() {
        let input = case["input"].as_array().unwrap();
        let (sys, msgs) = convert_messages_to_converse(input);
        let expected_sys = case["system"].as_array().cloned();
        assert_eq!(sys, expected_sys, "converse system for {}", case["label"]);
        assert_eq!(
            Value::Array(msgs),
            case["messages"],
            "converse messages for {}",
            case["label"]
        );
    }

    for case in br["normalize_response"].as_array().unwrap() {
        let comp = normalize_converse_response(&case["input"]);
        assert_eq!(comp.content, case["content"].as_str().unwrap());
        assert_eq!(
            comp.finish_reason.as_deref(),
            case["finish_reason"].as_str()
        );
        let usage = comp.usage.unwrap();
        assert_eq!(usage.input_tokens, case["prompt_tokens"].as_u64().unwrap());
        assert_eq!(
            usage.output_tokens,
            case["completion_tokens"].as_u64().unwrap()
        );
        let expected_tcs = case["tool_calls"].as_array().unwrap();
        assert_eq!(comp.tool_calls.len(), expected_tcs.len());
        for (actual, exp) in comp.tool_calls.iter().zip(expected_tcs.iter()) {
            assert_eq!(actual.id, exp["id"].as_str().unwrap());
            assert_eq!(actual.name, exp["name"].as_str().unwrap());
            let a_json: Value = serde_json::from_str(&actual.arguments).unwrap();
            let e_json: Value = serde_json::from_str(exp["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(a_json, e_json);
        }
    }

    // 4. OAuth helpers
    let oa = &data["oauth"];
    for case in oa["jwt_claims"].as_array().unwrap() {
        let claims = decode_jwt_claims(case["token"].as_str().unwrap());
        assert_eq!(Value::Object(claims), case["claims"]);
    }
    for case in oa["anthropic_oauth"].as_array().unwrap() {
        let key = case["key"].as_str().unwrap();
        let url = case["base_url"].as_str().unwrap();
        let prov = case["provider"].as_str();
        assert_eq!(
            is_anthropic_oauth_token(key),
            case["is_oauth_token"].as_bool().unwrap(),
            "is_anthropic_oauth_token({key})"
        );
        assert_eq!(
            anthropic_route_is_oauth(url, key, prov),
            case["route_is_oauth"].as_bool().unwrap(),
            "anthropic_route_is_oauth({url}, {key}, {prov:?})"
        );
    }
    assert_eq!(
        CLAUDE_CODE_SYSTEM_PREFIX,
        oa["anthropic_system_prefix"].as_str().unwrap()
    );
    let expected_betas: Vec<&str> = oa["anthropic_oauth_betas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(ANTHROPIC_OAUTH_ONLY_BETAS, expected_betas.as_slice());

    let wire_expected = oa["anthropic_wire_names"].as_object().unwrap();
    let names: Vec<&str> = wire_expected.keys().map(String::as_str).collect();
    let wire_actual = anthropic_oauth_wire_names(&names);
    for (orig, expected_wire) in wire_expected {
        let ew = expected_wire.as_str().unwrap();
        assert_eq!(wire_actual.get(orig).map(String::as_str), Some(ew));
        assert_eq!(anthropic_oauth_reverse_tool_name(ew, &wire_actual), *orig);
    }

    for case in oa["nous_urls"].as_array().unwrap() {
        let raw = case["url"].as_str().unwrap();
        let expected = case["validated"].as_str().map(str::to_owned);
        assert_eq!(
            validate_nous_inference_url(raw, None),
            expected,
            "nous url {raw}"
        );
    }

    let now = oa["minimax_expiry_now"].as_f64().unwrap();
    for case in oa["minimax_expiry"].as_array().unwrap() {
        let raw = case["expired_in"].as_i64().unwrap();
        let expected = case["unix"].as_f64().unwrap();
        assert_eq!(minimax_resolve_token_expiry_unix(raw, now), expected);
    }
}

#[tokio::test]
async fn nvidia_openai_provider_strips_tool_message_names_on_wire() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, captured) = start_capture_server(200, "text/event-stream", sse.into()).await;
    let provider = OpenAiProvider::new(&base_url, "nv-key", Duration::from_secs(5))
        .unwrap()
        .with_strip_tool_message_names(true);
    let req = json!({
        "model": "nvidia/nemotron",
        "messages": [
            {"role": "user", "name": "alice", "content": "hi"},
            {"role": "tool", "tool_call_id": "c1", "name": "read_file", "tool_name": "read_file", "content": "done"}
        ]
    });
    let comp = provider.complete(&req, |_| {}).await.unwrap();
    assert_eq!(comp.content, "ok");

    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 1);
    let sent: Value = serde_json::from_str(&cap[0].2).unwrap();
    let msgs = sent["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["name"], "alice");
    assert!(msgs[1].get("name").is_none());
    assert!(msgs[1].get("tool_name").is_none());
    assert_eq!(msgs[1]["tool_call_id"], "c1");
}

#[tokio::test]
async fn anthropic_oauth_provider_sends_bearer_headers_system_prefix_and_remaps_tools() {
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"mcp__context_notes\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"action\\\":\\\"add\\\"}\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let (base_url, captured) = start_capture_server(200, "text/event-stream", sse.into()).await;
    let provider = AnthropicProvider::new(&base_url, "sk-ant-oat01-test", Duration::from_secs(5))
        .unwrap()
        .with_oauth_identity(true);
    let req = json!({
        "model": "claude-sonnet-4-20250514",
        "messages": [
            {"role": "system", "content": "Custom sys"},
            {"role": "user", "content": "save memory"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "prev_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "prev_1", "content": "contents"}
        ],
        "tools": [
            {"type": "function", "function": {"name": "read_file", "description": "Read", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "memory", "description": "Mem", "parameters": {"type": "object"}}}
        ]
    });
    let comp = provider.complete(&req, |_| {}).await.unwrap();
    assert_eq!(comp.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(comp.tool_calls.len(), 1);
    assert_eq!(comp.tool_calls[0].name, "memory");
    assert_eq!(comp.tool_calls[0].id, "tu_1");

    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 1);
    let (_, headers, body_str) = &cap[0];
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer sk-ant-oat01-test")
    );
    assert!(headers.get("x-api-key").is_none());
    assert_eq!(headers.get("x-app").map(String::as_str), Some("cli"));
    assert!(headers
        .get("anthropic-beta")
        .is_some_and(|v| v.contains("oauth-2025-04-20") && v.contains("claude-code-20250219")));
    assert!(headers
        .get("user-agent")
        .is_some_and(|v| v.starts_with("claude-code/")));

    let sent: Value = serde_json::from_str(body_str).unwrap();
    assert_eq!(sent["system"][0]["text"], CLAUDE_CODE_SYSTEM_PREFIX);
    assert_eq!(sent["system"][1]["text"], "Custom sys");
    assert_eq!(sent["tools"][0]["name"], "mcp__read_file");
    assert_eq!(sent["tools"][1]["name"], "mcp__context_notes");
    assert_eq!(sent["messages"][1]["content"][0]["name"], "mcp__read_file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_oauth_refreshes_expiring_jwt_and_persists_rotated_tokens() {
    let header = base64url_encode(br#"{"alg":"none"}"#);
    let expired_payload = base64url_encode(br#"{"exp":1000,"sub":"u1"}"#);
    let fresh_payload = base64url_encode(br#"{"exp":4102444800,"sub":"u1"}"#);
    let old_jwt = format!("{header}.{expired_payload}.sig");
    let new_jwt = format!("{header}.{fresh_payload}.sig");

    assert!(jwt_is_expiring(&old_jwt, 120, 1700000000.0));
    assert!(!jwt_is_expiring(&new_jwt, 120, 1700000000.0));

    let resp = json!({
        "access_token": new_jwt,
        "refresh_token": "rt_rotated"
    })
    .to_string();
    let (token_server, captured) = start_capture_server(200, "application/json", resp).await;

    let home = tempfile::tempdir().unwrap();
    save_auth_store(
        home.path(),
        &json!({
            "version": 1,
            "providers": {
                "openai-codex": {
                    "tokens": {
                        "access_token": old_jwt,
                        "refresh_token": "rt_old"
                    }
                }
            }
        }),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path().join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    let mut secrets = HashMap::new();
    secrets.insert(
        "HERMES_CODEX_TOKEN_URL".into(),
        format!("{token_server}/oauth/token"),
    );

    let resolved = resolve_oauth_credentials(Some(home.path()), "openai-codex", &secrets).unwrap();
    assert_eq!(resolved.access_token, new_jwt);
    assert!(resolved.responses);

    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 1);
    assert!(cap[0].2.contains("refresh_token=rt_old"));

    let persisted = load_auth_store(home.path());
    assert_eq!(
        persisted["providers"]["openai-codex"]["tokens"]["access_token"],
        new_jwt
    );
    assert_eq!(
        persisted["providers"]["openai-codex"]["tokens"]["refresh_token"],
        "rt_rotated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_oauth_token_loads_refreshes_and_attaches_to_http_server_config() {
    let resp = json!({
        "access_token": "mcp_fresh_tok",
        "refresh_token": "mcp_rt_2",
        "expires_in": 3600
    })
    .to_string();
    let (token_server, _) = start_capture_server(200, "application/json", resp).await;

    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("mcp_oauth")).unwrap();
    std::fs::write(
        home.path().join("mcp_oauth/docs_srv.json"),
        json!({
            "access_token": "mcp_stale_tok",
            "refresh_token": "mcp_rt_1",
            "token_endpoint": format!("{token_server}/token"),
            "client_id": "cid",
            "expires_at": 100.0
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        home.path().join("config.yaml"),
        "mcp_servers:\n  docs_srv:\n    url: https://docs.example.test/mcp\n",
    )
    .unwrap();

    let token = resolve_mcp_oauth_token(home.path(), "docs_srv").unwrap();
    assert_eq!(token, "mcp_fresh_tok");

    let settings = load_chat_settings(Some(home.path())).unwrap();
    assert_eq!(settings.mcp_servers.len(), 1);
    assert_eq!(
        settings.mcp_servers[0]
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer mcp_fresh_tok")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vertex_service_account_rs256_jwt_and_authorized_user_exchange() {
    // 1. Service account RS256 JWT assertion
    let pem = test_rsa_pem();
    let jwt = build_service_account_jwt(
        "bot@demo-proj.iam.gserviceaccount.com",
        &pem,
        "https://oauth2.googleapis.com/token",
        1700000000,
    )
    .unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let claims = decode_jwt_claims(&jwt);
    assert_eq!(claims["iss"], "bot@demo-proj.iam.gserviceaccount.com");
    assert_eq!(claims["iat"], 1700000000);
    assert_eq!(claims["exp"], 1700003600);

    let (sa_server, sa_cap) = start_capture_server(
        200,
        "application/json",
        json!({"access_token": "ya29.sa_token", "expires_in": 3600}).to_string(),
    )
    .await;
    let sa_doc = json!({
        "type": "service_account",
        "project_id": "demo-proj",
        "client_email": "bot@demo-proj.iam.gserviceaccount.com",
        "private_key": pem,
        "token_uri": format!("{sa_server}/token")
    });
    let (sa_tok, sa_proj) = mint_token_from_credentials_json(&sa_doc, None).unwrap();
    assert_eq!(sa_tok, "ya29.sa_token");
    assert_eq!(sa_proj.as_deref(), Some("demo-proj"));
    assert!(sa_cap.lock().unwrap()[0].2.contains("assertion="));

    // 2. Authorized user ADC refresh token exchange
    let (au_server, au_cap) = start_capture_server(
        200,
        "application/json",
        json!({"access_token": "ya29.user_token"}).to_string(),
    )
    .await;
    let au_doc = json!({
        "type": "authorized_user",
        "quota_project_id": "quota-proj",
        "client_id": "cid",
        "client_secret": "csec",
        "refresh_token": "rt_user"
    });
    let (au_tok, au_proj) =
        mint_token_from_credentials_json(&au_doc, Some(&format!("{au_server}/token"))).unwrap();
    assert_eq!(au_tok, "ya29.user_token");
    assert_eq!(au_proj.as_deref(), Some("quota-proj"));
    assert!(au_cap.lock().unwrap()[0]
        .2
        .contains("refresh_token=rt_user"));

    // 3. resolve_vertex_route with VERTEX_ACCESS_TOKEN
    let mut secrets = HashMap::new();
    secrets.insert("VERTEX_ACCESS_TOKEN".into(), "ya29.direct".into());
    secrets.insert("VERTEX_PROJECT_ID".into(), "p1".into());
    secrets.insert("VERTEX_REGION".into(), "us-central1".into());
    let (tok, url) = resolve_vertex_route(&VertexSettings::default(), &secrets).unwrap();
    assert_eq!(tok, "ya29.direct");
    assert_eq!(
        url,
        "https://us-central1-aiplatform.googleapis.com/v1beta1/projects/p1/locations/us-central1/endpoints/openapi"
    );
}

#[tokio::test]
async fn bedrock_converse_sigv4_and_mantle_sigv4_and_overflow() {
    // 1. SigV4 deterministic signing check
    let url = reqwest::Url::parse(
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-sonnet-4/converse",
    )
    .unwrap();
    let now = DateTime::<Utc>::from_timestamp(1700000000, 0).unwrap();
    let sig_headers = sign_sigv4(
        "POST",
        &url,
        b"{\"messages\":[]}",
        "bedrock",
        "us-east-1",
        "AKIAIOSFODNN7EXAMPLE",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        Some("sess_tok"),
        now,
    );
    let sig_map: HashMap<String, String> = sig_headers.into_iter().collect();
    let auth_hdr = &sig_map["authorization"];
    assert!(auth_hdr.starts_with(
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231114/us-east-1/bedrock/aws4_request"
    ));
    assert!(auth_hdr
        .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"));
    assert_eq!(sig_map["x-amz-date"], "20231114T221320Z");
    assert_eq!(sig_map["x-amz-security-token"], "sess_tok");

    // 2. ~/.aws/credentials INI loader
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("credentials");
    std::fs::write(
        &creds,
        "[default]\naws_access_key_id = AK_DEF\naws_secret_access_key = SK_DEF\n\n[work]\naws_access_key_id = AK_WORK\naws_secret_access_key = SK_WORK\naws_session_token = ST_WORK\n",
    )
    .unwrap();
    assert_eq!(
        load_aws_credentials_profile(&creds, "work"),
        Some(AwsAuth::SigV4 {
            access_key_id: "AK_WORK".into(),
            secret_access_key: "SK_WORK".into(),
            session_token: Some("ST_WORK".into()),
        })
    );

    // 3. BedrockProvider Converse live HTTP call with SigV4 + guardrail
    let converse_resp = json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {"text": "Working."},
                    {"toolUse": {"toolUseId": "tu_9", "name": "read_file", "input": {"path": "a.txt"}}}
                ]
            }
        },
        "stopReason": "tool_use",
        "usage": {
            "inputTokens": 30,
            "outputTokens": 12,
            "cacheReadInputTokens": 20
        }
    })
    .to_string();
    let (base_url, captured) = start_capture_server(200, "application/json", converse_resp).await;
    let auth = AwsAuth::SigV4 {
        access_key_id: "AK_TEST".into(),
        secret_access_key: "SK_TEST".into(),
        session_token: None,
    };
    let provider =
        BedrockProvider::new(&base_url, "us-west-2", auth.clone(), Duration::from_secs(5))
            .unwrap()
            .with_guardrail(Some(json!({
                "guardrailIdentifier": "gr-123",
                "guardrailVersion": "1"
            })));
    let mut streamed = String::new();
    let comp = provider
        .complete(
            &json!({
                "model": "anthropic.claude-sonnet-4-20250514-v1:0",
                "messages": [
                    {"role": "system", "content": "Be brief"},
                    {"role": "user", "content": "Read a.txt"}
                ],
                "tools": [{"type": "function", "function": {"name": "read_file", "description": "Read", "parameters": {"type": "object"}}}]
            }),
            |s| streamed.push_str(s),
        )
        .await
        .unwrap();
    assert_eq!(streamed, "Working.");
    assert_eq!(comp.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(comp.tool_calls[0].id, "tu_9");
    assert_eq!(comp.tool_calls[0].name, "read_file");
    assert_eq!(comp.usage.unwrap().input_tokens, 50);

    {
        let cap = captured.lock().unwrap();
        assert_eq!(cap.len(), 1);
        assert!(
            cap[0]
                .0
                .starts_with("POST /model/anthropic.claude-sonnet-4-20250514-v1%3A0/converse "),
            "unexpected request line: {}",
            cap[0].0
        );
        assert!(cap[0].1["authorization"].starts_with("AWS4-HMAC-SHA256 Credential=AK_TEST/"));
        assert!(cap[0].1["authorization"].contains("/us-west-2/bedrock/aws4_request"));
        let body: Value = serde_json::from_str(&cap[0].2).unwrap();
        assert_eq!(body["guardrailConfig"]["guardrailIdentifier"], "gr-123");
        assert_eq!(body["system"][0]["text"], "Be brief");
    }

    // 4. Context overflow detection on Bedrock 400
    let (ov_url, _) = start_capture_server(
        400,
        "application/json",
        json!({"message": "Input is too long for requested model."}).to_string(),
    )
    .await;
    let ov_provider = BedrockProvider::new(
        &ov_url,
        "us-east-1",
        AwsAuth::Bearer("b".into()),
        Duration::from_secs(5),
    )
    .unwrap()
    .with_max_attempts(1);
    let err = ov_provider
        .complete(
            &json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
            |_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::ContextOverflow));

    // 5. Mantle OpenAI Responses SigV4 signing (`bedrock-mantle` service)
    let sse = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"mantle ok\"}\n\n",
        "data: {\"type\":\"response.completed\"}\n\n"
    );
    let (mantle_url, mantle_cap) = start_capture_server(200, "text/event-stream", sse.into()).await;
    let mantle = ResponsesProvider::new(&mantle_url, "", Duration::from_secs(5))
        .unwrap()
        .with_sigv4("us-east-1".into(), auth);
    let mcomp = mantle
        .complete(
            &json!({"model": "openai.gpt-5.4", "messages": [{"role": "user", "content": "hi"}]}),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(mcomp.content, "mantle ok");
    let mc = mantle_cap.lock().unwrap();
    assert!(mc[0].1["authorization"].contains("/us-east-1/bedrock-mantle/aws4_request"));
}

#[test]
fn config_resolves_all_new_providers_and_settings() {
    assert_eq!(resolve_provider("nvidia"), Some(("nvidia", false)));
    assert_eq!(resolve_provider("nemotron"), Some(("nvidia", false)));
    assert_eq!(resolve_provider("bedrock"), Some(("bedrock", false)));
    assert_eq!(resolve_provider("aws-bedrock"), Some(("bedrock", false)));
    assert_eq!(resolve_provider("vertex"), Some(("vertex", false)));
    assert_eq!(resolve_provider("vertex-ai"), Some(("vertex", false)));
    assert_eq!(
        resolve_provider("openai-codex"),
        Some(("openai-codex", true))
    );
    assert_eq!(resolve_provider("codex"), Some(("openai-codex", true)));
    assert_eq!(resolve_provider("nous"), Some(("nous", false)));
    assert_eq!(resolve_provider("xai-oauth"), Some(("xai-oauth", true)));
    assert_eq!(resolve_provider("qwen-oauth"), Some(("qwen-oauth", false)));
    assert_eq!(
        resolve_provider("minimax-oauth"),
        Some(("minimax-oauth", false))
    );

    assert_eq!(default_base_url("nvidia"), Some(NVIDIA_BASE_URL));
    assert!(default_base_url("openai-codex").is_some());
    assert!(default_base_url("bedrock").is_some());
    assert!(!ChatSettings::keyless_endpoint("nvidia"));
    assert!(!ChatSettings::keyless_endpoint("bedrock"));
    assert!(!ChatSettings::keyless_endpoint("vertex"));
    assert!(!ChatSettings::keyless_endpoint("openai-codex"));

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.yaml"),
        "model: {default: anthropic.claude-sonnet-4, provider: bedrock}\nbedrock:\n  region: eu-central-1\n  guardrail:\n    id: gr-abc\n    version: '2'\n",
    )
    .unwrap();
    let settings = load_chat_settings(Some(dir.path())).unwrap();
    assert_eq!(settings.provider, "bedrock");
    assert_eq!(
        resolve_bedrock_region(&settings.bedrock, settings.secrets()),
        "eu-central-1"
    );
    assert_eq!(
        settings.base_url.as_deref(),
        Some("https://bedrock-runtime.eu-central-1.amazonaws.com")
    );
    assert_eq!(
        settings.bedrock.guardrail_config(),
        Some(json!({"guardrailIdentifier": "gr-abc", "guardrailVersion": "2"}))
    );

    // Anthropic OAuth token loaded from CLAUDE_CODE_OAUTH_TOKEN in profile .env
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::write(
        dir2.path().join("config.yaml"),
        "model: {default: claude-sonnet-4, provider: anthropic}\n",
    )
    .unwrap();
    std::fs::write(
        dir2.path().join(".env"),
        "CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-from-dotenv\n",
    )
    .unwrap();
    let s2 = load_chat_settings(Some(dir2.path())).unwrap();
    assert_eq!(
        s2.anthropic_key_for("https://api.anthropic.com/v1")
            .as_deref(),
        Some("sk-ant-oat01-from-dotenv")
    );
}
