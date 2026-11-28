use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use hermes_runtime::mcp::{
    render_call_tool_result_with_cap, render_get_prompt_json, render_prompt_list_json, McpClients,
    McpServerConfig,
};
use hermes_runtime::mcp_media::{
    decode_base64_lenient, generate_preview, inline_truncated, looks_like_image,
    maybe_persist_mcp_result, mcp_resource_filename, pageable_text, parse_mcp_result_size_chars,
    safe_result_filename, sniff_audio_ext,
};
use regex::Regex;
use serde_json::{json, Value};

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/mcp_remainder_cases.json")).unwrap()
}

fn normalize_cache_paths(text: &str, home: &std::path::Path) -> String {
    let text = text.replace(&home.display().to_string(), "<HOME>");
    // Windows: JSON text doubles each backslash of the path, and the reference output uses `/`.
    let text = if cfg!(windows) {
        let escaped = home.display().to_string().replace('\\', "\\\\");
        let text = text.replace(&escaped, "<HOME>").replace("\\\\", "\\");
        hermes_runtime::paths::forward_slashes_after(&text, "<HOME>")
    } else {
        text
    };
    let re = Regex::new(r"(img|audio|doc)_[0-9a-f]{12}").unwrap();
    re.replace_all(&text, "${1}_<HEX12>").into_owned()
}

#[test]
fn image_and_audio_sniffers_and_filenames_match_reference_corpus() {
    let data = corpus();
    for case in data["image_sniff_cases"].as_array().unwrap() {
        let raw = decode_base64_lenient(case["data_b64"].as_str().unwrap()).unwrap();
        assert_eq!(
            looks_like_image(&raw),
            case["expected"].as_bool().unwrap(),
            "image_sniff: {}",
            case["label"]
        );
    }
    for case in data["audio_sniff_cases"].as_array().unwrap() {
        let raw = decode_base64_lenient(case["data_b64"].as_str().unwrap()).unwrap();
        assert_eq!(
            sniff_audio_ext(&raw, case["fallback"].as_str().unwrap()),
            case["expected"].as_str().unwrap(),
            "audio_sniff: {}",
            case["label"]
        );
    }
    for case in data["filename_cases"].as_array().unwrap() {
        assert_eq!(
            mcp_resource_filename(
                case["uri"].as_str().unwrap(),
                case["mime"].as_str().unwrap()
            ),
            case["expected"].as_str().unwrap(),
            "filename: {}",
            case["uri"]
        );
    }
}

#[test]
fn media_and_embedded_blob_caching_matches_reference_corpus() {
    let data = corpus();
    let home = tempfile::tempdir().unwrap();
    for case in data["media_cases"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        let output =
            render_call_tool_result_with_cap(&case["wire"], "fx", Some(home.path()), 2_000_000);
        let normalized = normalize_cache_paths(&output, home.path());
        assert_eq!(
            normalized,
            case["expected"].as_str().unwrap(),
            "media_case: {label}"
        );
        // Verify cached files exist on disk with exact decoded bytes.
        let parsed: Value = serde_json::from_str(&output).unwrap();
        let res = parsed["result"].as_str().unwrap();
        if let Some(media_path) = res.lines().find_map(|line| line.strip_prefix("MEDIA:")) {
            let block = case["wire"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| matches!(b["type"].as_str(), Some("image" | "audio")))
                .unwrap();
            let expected_bytes = decode_base64_lenient(block["data"].as_str().unwrap()).unwrap();
            assert_eq!(
                std::fs::read(media_path).unwrap(),
                expected_bytes,
                "cached bytes for {label}"
            );
        }
    }
}

#[test]
fn structured_content_and_text_hard_cap_matches_reference_corpus() {
    let data = corpus();
    for case in data["structured_cap_cases"].as_array().unwrap() {
        let cap = case["cap"].as_u64().unwrap() as usize;
        let output = render_call_tool_result_with_cap(&case["wire"], "fx", None, cap);
        assert_eq!(
            output,
            case["expected"].as_str().unwrap(),
            "structured_cap: {}",
            case["label"]
        );
    }
}

#[test]
fn spillover_helpers_and_persisted_message_match_reference_corpus() {
    let data = corpus();
    for case in data["pageable_cases"].as_array().unwrap() {
        assert_eq!(
            pageable_text(case["input"].as_str().unwrap()),
            case["expected"].as_str().unwrap(),
            "pageable: {}",
            case["label"]
        );
    }
    for case in data["preview_cases"].as_array().unwrap() {
        let (preview, has_more) = generate_preview(
            case["text"].as_str().unwrap(),
            case["max_chars"].as_u64().unwrap() as usize,
        );
        assert_eq!(
            preview,
            case["preview"].as_str().unwrap(),
            "preview text: {}",
            case["label"]
        );
        assert_eq!(
            has_more,
            case["has_more"].as_bool().unwrap(),
            "preview has_more: {}",
            case["label"]
        );
    }
    for case in data["safe_id_cases"].as_array().unwrap() {
        assert_eq!(
            safe_result_filename(case["raw_id"].as_str().unwrap()),
            case["expected"].as_str().unwrap(),
            "safe_id: {}",
            case["raw_id"]
        );
    }

    let spill = &data["spillover_example"];
    let home = tempfile::tempdir().unwrap();
    let msg = maybe_persist_mcp_result(
        spill["input"].as_str().unwrap(),
        spill["tool_call_id"].as_str().unwrap(),
        Some(home.path()),
        spill["threshold"].as_u64().unwrap() as usize,
        spill["preview_size"].as_u64().unwrap() as usize,
    );
    assert_eq!(
        normalize_cache_paths(&msg, home.path()),
        spill["message"].as_str().unwrap()
    );
    let saved_path = home
        .path()
        .join("cache")
        .join("spillover")
        .join("call-spill-1.txt");
    assert_eq!(
        std::fs::read_to_string(&saved_path).unwrap(),
        spill["spilled_body"].as_str().unwrap()
    );
    assert_eq!(
        inline_truncated("prev", 12345),
        spill["inline_fallback"].as_str().unwrap()
    );
    // Below threshold stays inline untouched.
    let small = r#"{"result": "tiny"}"#;
    assert_eq!(
        maybe_persist_mcp_result(small, "call-small", Some(home.path()), 1000, 200),
        small
    );
}

#[test]
fn prompt_rendering_and_config_budget_parsing_match_reference() {
    let data = corpus();
    let prompts = &data["prompts"];
    assert_eq!(
        render_prompt_list_json(prompts["list_wire"].as_array().unwrap()),
        prompts["list_output"].as_str().unwrap()
    );
    assert_eq!(
        render_get_prompt_json(&prompts["get_wire"]),
        prompts["get_output"].as_str().unwrap()
    );

    let default_cfg: serde_yaml::Value = serde_yaml::from_str("{}").unwrap();
    assert_eq!(parse_mcp_result_size_chars(&default_cfg), 50_000);
    let custom_cfg: serde_yaml::Value =
        serde_yaml::from_str("tool_budget:\n  mcp_result_size_chars: 12000\n").unwrap();
    assert_eq!(parse_mcp_result_size_chars(&custom_cfg), 12_000);
    let capped_cfg: serde_yaml::Value =
        serde_yaml::from_str("tool_budget:\n  mcp_result_size_chars: 500000\n").unwrap();
    assert_eq!(parse_mcp_result_size_chars(&capped_cfg), 100_000);
    let bad_cfg: serde_yaml::Value =
        serde_yaml::from_str("tool_budget:\n  mcp_result_size_chars: -5\n").unwrap();
    assert_eq!(parse_mcp_result_size_chars(&bad_cfg), 50_000);
}

#[tokio::test]
async fn port_level_errors_use_python_json_separators() {
    let clients = McpClients::default();
    let unknown = clients.call("mcp__missing__tool", "{}").await;
    assert_eq!(
        unknown.as_str(),
        Some(r#"{"error": "Unknown tool: mcp__missing__tool"}"#)
    );
}

struct SseFixture {
    child: Child,
    url: String,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl SseFixture {
    fn start(flags: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sse_log.json");
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../compat/mcp_sse_fixture_server.py");
        let mut child = Command::new("python3")
            .args(["-I"])
            .arg(script)
            .arg("--log")
            .arg(&log)
            .args(flags)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut url = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut url)
            .unwrap();
        Self {
            child,
            url: url.trim().to_owned(),
            log,
            _dir: dir,
        }
    }

    fn entries(&self) -> Vec<Value> {
        serde_json::from_str(&std::fs::read_to_string(&self.log).unwrap_or("[]".into())).unwrap()
    }

    fn config(&self, timeout_secs: f64, prompts: bool) -> McpServerConfig {
        McpServerConfig {
            name: "legacy".into(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some(self.url.clone()),
            headers: BTreeMap::from([("Authorization".into(), "Bearer fixture-token".into())]),
            transport: Some("sse".into()),
            timeout_secs,
            connect_timeout_secs: 10,
            resources: true,
            prompts,
        }
    }
}

impl Drop for SseFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn legacy_sse_transport_calls_tools_prompts_cancels_and_spills_over() {
    let fixture = SseFixture::start(&["--prompts"]);
    let profile = tempfile::tempdir().unwrap();
    let (clients, warnings) = McpClients::start_with_options(
        &[fixture.config(1.0, true)],
        Some(profile.path().to_path_buf()),
        200,
    )
    .await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(clients.owns("mcp__legacy__echo"));
    assert!(clients.owns("mcp__legacy__list_prompts"));
    assert!(clients.owns("mcp__legacy__get_prompt"));

    // Frozen prompt schemas match upstream corpus.
    let recorded_schemas = corpus()["prompts"]["schemas"].as_array().unwrap().clone();
    let schemas = clients.schemas();
    let tail = &schemas[schemas.len() - 2..];
    for (schema, expected) in tail.iter().zip(&recorded_schemas) {
        let function = &schema["function"];
        let key = expected["key"].as_str().unwrap();
        assert_eq!(function["name"], format!("mcp__legacy__{key}"));
        assert_eq!(
            function["description"],
            expected["description"]
                .as_str()
                .unwrap()
                .replace("{server}", "legacy")
        );
        assert_eq!(function["parameters"], expected["parameters"]);
    }

    // Bad arguments error uses Python separators.
    let bad_args = clients.call("mcp__legacy__echo", "[1,2]").await;
    assert_eq!(
        bad_args.as_str(),
        Some(r#"{"error": "Tool arguments must be a JSON object"}"#)
    );

    // Basic SSE tool call works and refuses server-initiated ping with -32601.
    let echo = clients
        .call("mcp__legacy__echo", r#"{"text":"hello"}"#)
        .await;
    assert_eq!(echo.as_str(), Some(r#"{"result": "sse:hello"}"#));

    // Paginated prompts/list and prompts/get (including missing required 'name').
    let listed = clients.call("mcp__legacy__list_prompts", "{}").await;
    let listed_val: Value = serde_json::from_str(listed.as_str().unwrap()).unwrap();
    let names: Vec<&str> = listed_val["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["summarize", "bare"]);

    let prompt = clients
        .call(
            "mcp__legacy__get_prompt",
            r#"{"name":"summarize","arguments":{"topic":"rust"}}"#,
        )
        .await;
    assert_eq!(
        prompt.as_str(),
        Some(
            r#"{"messages": [{"role": "user", "content": "Summarize rust"}], "description": "Summary prompt"}"#
        )
    );

    let missing_name = clients.call("mcp__legacy__get_prompt", "{}").await;
    assert_eq!(
        missing_name.as_str(),
        Some(r#"{"error": "Missing required parameter 'name'"}"#)
    );

    // Oversized output (>200 chars threshold) spills to <profile>/cache/spillover/<call_id>.txt.
    let big = "line\n".repeat(80);
    let spilled = clients
        .call_with_id(
            "mcp__legacy__echo",
            &json!({"text": big}).to_string(),
            "call_sse_big",
        )
        .await;
    let spilled_text = spilled.as_str().unwrap();
    assert!(
        spilled_text.starts_with("<persisted-output>"),
        "expected spillover: {spilled_text}"
    );
    let spill_file = profile
        .path()
        .join("cache")
        .join("spillover")
        .join("call_sse_big.txt");
    assert!(std::fs::read_to_string(&spill_file)
        .unwrap()
        .starts_with("sse:line\n"));

    // Timeout sends notifications/cancelled to the advertised SSE POST endpoint.
    let timed_out = clients.call("mcp__legacy__slow", "{}").await;
    assert!(timed_out.as_str().unwrap().contains("TimeoutError"));
    clients.shutdown().await;

    let entries = fixture.entries();
    assert!(
        entries
            .iter()
            .any(|e| e["method"] == "notifications/cancelled"
                && e["path"] == "/messages?session_id=sse-1"),
        "expected cancellation POST: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|e| e["client_response"]["error"]["code"] == -32601),
        "expected -32601 reply to server ping: {entries:?}"
    );

    // prompts: false disables prompt utilities even when server advertises capability.
    let (disabled, _) = McpClients::start(&[fixture.config(5.0, false)]).await;
    assert!(disabled.owns("mcp__legacy__echo"));
    assert!(!disabled.owns("mcp__legacy__list_prompts"));
    assert!(!disabled.owns("mcp__legacy__get_prompt"));
    disabled.shutdown().await;
}

#[tokio::test]
async fn legacy_sse_rejects_cross_origin_endpoint() {
    let fixture = SseFixture::start(&["--bad-origin"]);
    let (clients, warnings) = McpClients::start(&[fixture.config(5.0, false)]).await;
    assert!(clients.schemas().is_empty());
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains("origin does not match"),
        "{warnings:?}"
    );
}
