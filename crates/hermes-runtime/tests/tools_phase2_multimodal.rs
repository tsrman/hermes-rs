use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hermes_runtime::config::load_chat_settings;
use hermes_runtime::image_gen::{build_fal_payload, ImageGenSettings};
use hermes_runtime::providers::anthropic::AnthropicProvider;
use hermes_runtime::providers::bedrock::convert_messages_to_converse;
use hermes_runtime::providers::oauth::base64_encode;
use hermes_runtime::providers::openai::OpenAiProvider;
use hermes_runtime::providers::responses::ResponsesProvider;
use hermes_runtime::providers::Provider;
use hermes_runtime::stall_guards::IntentAckContinuation;
use hermes_runtime::tool_policy::ToolPolicy;
use hermes_runtime::tts_tool::{chunk_text, TtsCommandProvider, TtsSettings};
use hermes_runtime::turn::{
    run_turn, ApprovalDecision, ModelRoute, TurnHistory, TurnOptions, TurnOutcome,
};
use hermes_runtime::vision_tool::VisionSettings;
use hermes_runtime::x_search::{build_tool_def, XSearchSettings};
use serde_json::{json, Value};

/// Minimal valid 1x1 PNG bytes.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn accept_timeout(listener: &TcpListener) -> Option<std::net::TcpStream> {
    listener.set_nonblocking(true).ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                return Some(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
    None
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Option<(String, String, Vec<u8>)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let body_start;
    loop {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(pos) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            body_start = pos + 4;
            break;
        }
    }
    let head = String::from_utf8_lossy(&bytes[..body_start]).into_owned();
    let first_line = head.lines().next().unwrap_or("").to_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() - body_start < length {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Some((
        first_line,
        head,
        bytes[body_start..body_start + length].to_vec(),
    ))
}

fn write_http(stream: &mut std::net::TcpStream, status: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn text_completion(content: &str) -> Value {
    json!({
        "id": "cmpl",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    })
}

fn tool_completion(id: &str, name: &str, args: Value) -> Value {
    json!({
        "id": "cmpl",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
}

fn base_options<'a>(
    root: &'a std::path::Path,
    profile: Option<&'a std::path::Path>,
    policy: &'a ToolPolicy,
    ack: &'a IntentAckContinuation,
) -> TurnOptions<'a> {
    TurnOptions {
        mcp: None,
        web: None,
        browser: None,
        memory: None,
        vision: None,
        vision_route: None,
        image_gen: None,
        tts: None,
        x_search: None,
        root,
        profile,
        session_id: None,
        single_query: false,
        max_turns: 6,
        approval_timeout_secs: Some(5),
        terminal_approval_timeout_secs: None,
        terminal_local: false,
        compression: None,
        summarizer: None,
        compaction_guard: None,
        keep_phone_numbers: false,
        stall_guards: true,
        intent_ack_continuation: ack,
        terminal_env: None,
        clarify_timeout_secs: 0,
        tools: policy,
        journal: None,
        delegation: None,
        system_prompt_override: None,
        ephemeral_system_prompt: None,
        child: None,
        delegate_depth: 0,
        cancel_flag: None,
        steer_queue: None,
        prefill: None,
        state_scope: None,
        approval_policy: None,
        image_input_mode_text: false,
    }
}

#[tokio::test]
async fn multimodal_image_blocks_translate_for_anthropic_responses_and_bedrock() {
    let b64 = base64_encode(TINY_PNG);
    let data_url = format!("data:image/png;base64,{b64}");

    // 1. Bedrock Converse native image block.
    let (_, converse) = convert_messages_to_converse(&[json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "describe"},
            {"type": "image_url", "image_url": {"url": data_url}}
        ]
    })]);
    assert_eq!(converse[0]["content"][1]["image"]["format"], json!("png"));
    assert_eq!(
        converse[0]["content"][1]["image"]["source"]["bytes"],
        json!(b64)
    );

    // 2. Anthropic Messages translates image_url data URL and merges adjacent user turns after tool_result.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Value::Null));
    let cap_srv = Arc::clone(&captured);
    let handle = thread::spawn(move || {
        if let Some(mut stream) = accept_timeout(&listener) {
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            *cap_srv.lock().unwrap() = serde_json::from_slice(&body).unwrap();
            let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"seen\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            write_http(&mut stream, "200 OK", "text/event-stream", sse.as_bytes());
        }
    });
    let anth = AnthropicProvider::new(
        &format!("http://{addr}"),
        "test-key",
        Duration::from_secs(5),
    )
    .unwrap()
    .with_max_attempts(1);
    let res = anth
        .complete(
            &json!({
                "model": "claude-sonnet",
                "messages": [
                    {"role": "user", "content": "look"},
                    {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "vision_analyze", "arguments": "{}"}}]},
                    {"role": "tool", "tool_call_id": "c1", "content": "{\"success\":true}"},
                    {"role": "user", "content": [
                        {"type": "text", "text": "Attached image"},
                        {"type": "image_url", "image_url": {"url": data_url}}
                    ]}
                ]
            }),
            |_| {},
        )
        .await
        .unwrap();
    handle.join().unwrap();
    assert_eq!(res.content, "seen");
    {
        let anth_body = captured.lock().unwrap();
        // Tool result and follow-up multimodal user message merged into a single Anthropic user turn.
        let last_msg = anth_body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last_msg["role"], json!("user"));
        let blocks = last_msg["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], json!("tool_result"));
        assert_eq!(blocks[2]["type"], json!("image"));
        assert_eq!(blocks[2]["source"]["media_type"], json!("image/png"));
        assert_eq!(blocks[2]["source"]["data"], json!(b64));
    }

    // 3. OpenAI Responses translates image_url -> input_image.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let captured_resp = Arc::new(Mutex::new(Value::Null));
    let cap_r = Arc::clone(&captured_resp);
    let handle = thread::spawn(move || {
        if let Some(mut stream) = accept_timeout(&listener) {
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            *cap_r.lock().unwrap() = serde_json::from_slice(&body).unwrap();
            let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
            write_http(&mut stream, "200 OK", "text/event-stream", sse.as_bytes());
        }
    });
    let resp_prov = ResponsesProvider::new(
        &format!("http://{addr}/v1"),
        "test-key",
        Duration::from_secs(5),
    )
    .unwrap()
    .with_max_attempts(1);
    let res = resp_prov
        .complete(
            &json!({
                "model": "gpt-5",
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "inspect"},
                        {"type": "image_url", "image_url": {"url": data_url}}
                    ]
                }]
            }),
            |_| {},
        )
        .await
        .unwrap();
    handle.join().unwrap();
    assert_eq!(res.content, "ok");
    let resp_body = captured_resp.lock().unwrap();
    assert_eq!(
        resp_body["input"][0]["content"][0]["type"],
        json!("input_text")
    );
    assert_eq!(
        resp_body["input"][0]["content"][1]["type"],
        json!("input_image")
    );
    assert_eq!(
        resp_body["input"][0]["content"][1]["image_url"],
        json!(data_url)
    );
}

#[tokio::test]
async fn vision_analyze_supports_native_attachment_read_file_images_and_auxiliary_route() {
    let dir = tempfile::tempdir().unwrap();
    let root = hermes_runtime::paths::canonical(dir.path()).unwrap();
    std::fs::write(root.join("pixel.png"), TINY_PNG).unwrap();

    // 1. Native mode: both read_file(pixel.png) and vision_analyze(pixel.png) strip internal envelope
    // from tool JSON and attach image_url to `sent`.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let reqs_srv = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        for step in 0..3 {
            let Some(mut stream) = accept_timeout(&listener) else {
                break;
            };
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            reqs_srv.lock().unwrap().push(parsed);
            let reply = match step {
                0 => tool_completion("r1", "read_file", json!({"path": "pixel.png"})),
                1 => tool_completion(
                    "v1",
                    "vision_analyze",
                    json!({"image_url": "pixel.png", "question": "What color is the pixel?"}),
                ),
                _ => text_completion("It is a 1x1 PNG."),
            };
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                reply.to_string().as_bytes(),
            );
        }
    });

    let provider = OpenAiProvider::new(&format!("http://{addr}/v1"), "k", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let route = ModelRoute {
        provider: Provider::OpenAi(provider),
        provider_name: "custom",
        model: "main-vision".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let vision_settings = VisionSettings::default();
    let mut opts = base_options(&root, None, &policy, &ack);
    opts.vision = Some(&vision_settings);

    let outcome = run_turn(
        &route,
        &[],
        "check pixel.png",
        opts,
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    handle.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    {
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        // Request 1 (after read_file) has clean tool JSON (no `_hermes_multimodal_images`) + attached multimodal user turn.
        let msgs1 = recorded[1]["messages"].as_array().unwrap();
        let tool_msg1 = msgs1.iter().find(|m| m["role"] == "tool").unwrap();
        assert!(!tool_msg1["content"]
            .as_str()
            .unwrap()
            .contains("_hermes_multimodal_images"));
        assert!(tool_msg1["content"]
            .as_str()
            .unwrap()
            .contains("\"is_image\":true"));
        let last1 = msgs1.last().unwrap();
        assert_eq!(last1["role"], json!("user"));
        assert_eq!(last1["content"][1]["type"], json!("image_url"));
        assert!(last1["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));

        // Request 2 (after vision_analyze) also attached the image natively.
        let last2 = recorded[2]["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last2["role"], json!("user"));
        assert_eq!(last2["content"][1]["type"], json!("image_url"));
    }

    // 2. Auxiliary vision route mode: calls dedicated vision model instead of attaching to main model.
    let aux_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let aux_addr = aux_listener.local_addr().unwrap();
    let aux_reqs = Arc::new(Mutex::new(Vec::<Value>::new()));
    let aux_srv = Arc::clone(&aux_reqs);
    let aux_handle = thread::spawn(move || {
        if let Some(mut stream) = accept_timeout(&aux_listener) {
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            aux_srv
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                text_completion("Auxiliary sees a blue pixel")
                    .to_string()
                    .as_bytes(),
            );
        }
    });

    let main_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let main_addr = main_listener.local_addr().unwrap();
    let main_tool_res = Arc::new(Mutex::new(String::new()));
    let main_tool_srv = Arc::clone(&main_tool_res);
    let main_handle = thread::spawn(move || {
        for step in 0..2 {
            let Some(mut stream) = accept_timeout(&main_listener) else {
                break;
            };
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            if step == 1 {
                *main_tool_srv.lock().unwrap() =
                    parsed["messages"].as_array().unwrap().last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .to_owned();
            }
            let reply = if step == 0 {
                tool_completion(
                    "va",
                    "vision_analyze",
                    json!({"image_url": "pixel.png", "question": "describe it"}),
                )
            } else {
                text_completion("done")
            };
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                reply.to_string().as_bytes(),
            );
        }
    });

    let main_route = ModelRoute {
        provider: Provider::OpenAi(
            OpenAiProvider::new(
                &format!("http://{main_addr}/v1"),
                "k",
                Duration::from_secs(5),
            )
            .unwrap()
            .with_streaming(false),
        ),
        provider_name: "custom",
        model: "main-text".into(),
    };
    let aux_route = ModelRoute {
        provider: Provider::OpenAi(
            OpenAiProvider::new(
                &format!("http://{aux_addr}/v1"),
                "k",
                Duration::from_secs(5),
            )
            .unwrap()
            .with_streaming(false),
        ),
        provider_name: "custom",
        model: "aux-vision-v1".into(),
    };
    let mut opts = base_options(&root, None, &policy, &ack);
    opts.vision = Some(&vision_settings);
    opts.vision_route = Some(&aux_route);

    let outcome = run_turn(
        &main_route,
        &[],
        "describe pixel",
        opts,
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    main_handle.join().unwrap();
    aux_handle.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    let aux_seen = aux_reqs.lock().unwrap();
    assert_eq!(aux_seen.len(), 1);
    assert_eq!(aux_seen[0]["model"], json!("aux-vision-v1"));
    assert_eq!(
        aux_seen[0]["messages"][0]["content"][1]["type"],
        json!("image_url")
    );
    let tool_json: Value = serde_json::from_str(&main_tool_res.lock().unwrap()).unwrap();
    assert_eq!(tool_json["success"], json!(true));
    assert_eq!(tool_json["analysis"], json!("Auxiliary sees a blue pixel"));
    assert_eq!(tool_json["model_used"], json!("aux-vision-v1"));
}

#[tokio::test]
async fn image_generate_filters_fal_payload_downloads_image_and_returns_media_tag() {
    // Catalog payload & whitelist checks.
    let (ep, payload) =
        build_fal_payload("fal-ai/nano-banana-pro", "a cat", "portrait", &[], Some(42)).unwrap();
    assert_eq!(ep, "fal-ai/nano-banana-pro");
    assert_eq!(payload["aspect_ratio"], json!("9:16"));
    assert_eq!(payload["seed"], json!(42));
    assert!(payload.get("image_size").is_none());

    let err = build_fal_payload(
        "fal-ai/z-image/turbo",
        "edit",
        "square",
        &["https://example.com/a.png".into()],
        None,
    )
    .unwrap_err();
    assert!(err.contains("does not support image editing"));

    // End-to-end tool call against a local mock FAL server.
    let dir = tempfile::tempdir().unwrap();
    let root = hermes_runtime::paths::canonical(dir.path()).unwrap();
    let profile = tempfile::tempdir().unwrap();

    let fal_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fal_addr = fal_listener.local_addr().unwrap();
    let fal_reqs = Arc::new(Mutex::new(Vec::<(String, String, Value)>::new()));
    let fal_srv = Arc::clone(&fal_reqs);
    let fal_handle = thread::spawn(move || {
        for _ in 0..2 {
            let Some(mut stream) = accept_timeout(&fal_listener) else {
                break;
            };
            let (first, head, body) = read_http_request(&mut stream).unwrap();
            if first.starts_with("POST ") {
                let parsed: Value = serde_json::from_slice(&body).unwrap();
                fal_srv.lock().unwrap().push((first, head, parsed));
                let resp = json!({"images": [{"url": format!("http://{fal_addr}/out.png")}]});
                write_http(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    resp.to_string().as_bytes(),
                );
            } else {
                write_http(&mut stream, "200 OK", "image/png", TINY_PNG);
            }
        }
    });

    let llm_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let llm_addr = llm_listener.local_addr().unwrap();
    let tool_out = Arc::new(Mutex::new(Value::Null));
    let tool_srv = Arc::clone(&tool_out);
    let llm_handle = thread::spawn(move || {
        for step in 0..2 {
            let Some(mut stream) = accept_timeout(&llm_listener) else {
                break;
            };
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            if step == 1 {
                let raw = parsed["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                *tool_srv.lock().unwrap() = serde_json::from_str(raw).unwrap();
            }
            let reply = if step == 0 {
                tool_completion(
                    "ig1",
                    "image_generate",
                    json!({"prompt": "cyberpunk lighthouse", "aspect_ratio": "square"}),
                )
            } else {
                text_completion("generated")
            };
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                reply.to_string().as_bytes(),
            );
        }
    });

    let route = ModelRoute {
        provider: Provider::OpenAi(
            OpenAiProvider::new(
                &format!("http://{llm_addr}/v1"),
                "k",
                Duration::from_secs(5),
            )
            .unwrap()
            .with_streaming(false),
        ),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let img_settings = ImageGenSettings {
        fal_key: Some("fal-secret-key".into()),
        model: "fal-ai/flux-2/klein/9b".into(),
        base_url: format!("http://{fal_addr}"),
        default_aspect_ratio: "landscape".into(),
        allow_private_urls: true,
    };
    let mut opts = base_options(&root, Some(profile.path()), &policy, &ack);
    opts.image_gen = Some(&img_settings);

    let outcome = run_turn(
        &route,
        &[],
        "make an image",
        opts,
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    llm_handle.join().unwrap();
    fal_handle.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let fal_seen = fal_reqs.lock().unwrap();
    assert_eq!(fal_seen.len(), 1);
    assert!(fal_seen[0].0.contains("/fal-ai/flux-2/klein/9b"));
    assert!(fal_seen[0].1.contains("Key fal-secret-key"));
    assert_eq!(fal_seen[0].2["image_size"], json!("square_hd"));

    let out = tool_out.lock().unwrap();
    assert_eq!(out["success"], json!(true));
    assert!(out["media"].as_str().unwrap().starts_with("MEDIA:"));
    let saved = out["file_path"].as_str().unwrap();
    assert_eq!(std::fs::read(saved).unwrap(), TINY_PNG);
}

#[tokio::test]
async fn text_to_speech_chunks_guards_media_injection_and_supports_http_and_command() {
    // Chunking preserves all words without truncation.
    let chunks = chunk_text("First sentence. Second sentence. Third sentence.", 20);
    assert!(chunks.len() >= 2);
    assert_eq!(
        chunks.join(" "),
        "First sentence. Second sentence. Third sentence."
    );

    let dir = tempfile::tempdir().unwrap();
    let root = hermes_runtime::paths::canonical(dir.path()).unwrap();

    // 1. OpenAI HTTP backend + MEDIA: injection refusal.
    let tts_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let tts_addr = tts_listener.local_addr().unwrap();
    let tts_reqs = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tts_srv = Arc::clone(&tts_reqs);
    let tts_handle = thread::spawn(move || {
        if let Some(mut stream) = accept_timeout(&tts_listener) {
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            tts_srv
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            write_http(&mut stream, "200 OK", "audio/mpeg", b"ID3-MOCK-MP3");
        }
    });

    let llm_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let llm_addr = llm_listener.local_addr().unwrap();
    let tool_outs = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tool_srv = Arc::clone(&tool_outs);
    let llm_handle = thread::spawn(move || {
        for step in 0..3 {
            let Some(mut stream) = accept_timeout(&llm_listener) else {
                break;
            };
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            if step > 0 {
                let raw = parsed["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                tool_srv
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(raw).unwrap());
            }
            let reply = match step {
                0 => tool_completion(
                    "t1",
                    "text_to_speech",
                    json!({
                        "text": "Hello world",
                        "output_path": "out.mp3",
                        "speed": 1.25,
                        "instructions": "Speak warmly"
                    }),
                ),
                1 => tool_completion(
                    "t2",
                    "text_to_speech",
                    json!({"text": "Inject", "output_path": "MEDIA:/etc/passwd.mp3"}),
                ),
                _ => text_completion("spoken"),
            };
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                reply.to_string().as_bytes(),
            );
        }
    });

    let route = ModelRoute {
        provider: Provider::OpenAi(
            OpenAiProvider::new(
                &format!("http://{llm_addr}/v1"),
                "k",
                Duration::from_secs(5),
            )
            .unwrap()
            .with_streaming(false),
        ),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let tts_settings = TtsSettings {
        provider: "openai".into(),
        model: Some("gpt-4o-mini-tts".into()),
        voice: Some("nova".into()),
        speed: None,
        response_format: Some("mp3".into()),
        base_url: Some(format!("http://{tts_addr}/v1")),
        api_key: Some("sk-tts-test".into()),
        command_provider: None,
    };
    let mut opts = base_options(&root, None, &policy, &ack);
    opts.tts = Some(&tts_settings);

    let outcome = run_turn(
        &route,
        &[],
        "speak",
        opts,
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    llm_handle.join().unwrap();
    tts_handle.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    {
        let sent_tts = tts_reqs.lock().unwrap();
        assert_eq!(sent_tts.len(), 1);
        assert_eq!(sent_tts[0]["voice"], json!("nova"));
        assert_eq!(sent_tts[0]["speed"], json!(1.25));
        assert_eq!(sent_tts[0]["instructions"], json!("Speak warmly"));

        let outs = tool_outs.lock().unwrap();
        assert_eq!(outs[0]["success"], json!(true));
        assert_eq!(
            std::fs::read(root.join("out.mp3")).unwrap(),
            b"ID3-MOCK-MP3"
        );
        assert!(outs[0]["media"].as_str().unwrap().starts_with("MEDIA:"));
        assert_eq!(outs[1]["success"], json!(false));
        assert!(outs[1]["error"].as_str().unwrap().contains("MEDIA:"));
    }

    // 2. Command provider (`type: command`) writes audio file from stdin.
    let llm2 = TcpListener::bind("127.0.0.1:0").unwrap();
    let llm2_addr = llm2.local_addr().unwrap();
    let cmd_out = Arc::new(Mutex::new(Value::Null));
    let cmd_srv = Arc::clone(&cmd_out);
    let llm2_handle = thread::spawn(move || {
        for step in 0..2 {
            let Some(mut stream) = accept_timeout(&llm2) else {
                break;
            };
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            if step == 1 {
                let raw = parsed["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                *cmd_srv.lock().unwrap() = serde_json::from_str(raw).unwrap();
            }
            let reply = if step == 0 {
                tool_completion(
                    "tc",
                    "text_to_speech",
                    json!({"text": "local piper audio", "output_path": "cmd.wav"}),
                )
            } else {
                text_completion("cmd ok")
            };
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                reply.to_string().as_bytes(),
            );
        }
    });
    let route2 = ModelRoute {
        provider: Provider::OpenAi(
            OpenAiProvider::new(
                &format!("http://{llm2_addr}/v1"),
                "k",
                Duration::from_secs(5),
            )
            .unwrap()
            .with_streaming(false),
        ),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let cmd_tts = TtsSettings {
        provider: "piper".into(),
        command_provider: Some(TtsCommandProvider {
            command: vec!["cat".into()],
            extension: Some("wav".into()),
        }),
        ..TtsSettings::default()
    };
    let mut opts2 = base_options(&root, None, &policy, &ack);
    opts2.tts = Some(&cmd_tts);
    run_turn(
        &route2,
        &[],
        "cmd speak",
        opts2,
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    llm2_handle.join().unwrap();
    assert_eq!(cmd_out.lock().unwrap()["success"], json!(true));
    assert_eq!(
        std::fs::read_to_string(root.join("cmd.wav")).unwrap(),
        "local piper audio"
    );
}

#[tokio::test]
async fn x_search_validates_filters_retries_429_and_flags_degraded() {
    // Client-side validation tests.
    assert!(build_tool_def(&json!({"from_date": "2026-05-10", "to_date": "2026-05-01"})).is_err());
    assert!(build_tool_def(&json!({"from_date": "not-a-date"})).is_err());
    assert!(build_tool_def(&json!({
        "allowed_x_handles": ["a"],
        "excluded_x_handles": ["b"]
    }))
    .is_err());
    let (def, filters) = build_tool_def(&json!({
        "allowed_x_handles": ["@elonmusk", "NousResearch"],
        "from_date": "2026-01-01"
    }))
    .unwrap();
    assert_eq!(
        def["allowed_x_handles"],
        json!(["elonmusk", "NousResearch"])
    );
    assert_eq!(filters, vec!["allowed_x_handles", "from_date"]);

    // Config loading check (`XAI_API_KEY` wins over oauth).
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.yaml"),
        "x_search:\n  model: grok-4.5\n  timeout_seconds: 10\n",
    )
    .unwrap();
    std::fs::write(home.path().join(".env"), "XAI_API_KEY=xai-key-123\n").unwrap();
    let cfg = load_chat_settings(Some(home.path())).unwrap();
    assert!(cfg.x_search.is_available());
    assert_eq!(cfg.x_search.credential_source, "api_key");
    assert_eq!(cfg.x_search.timeout_seconds, 30); // floored at 30s

    // End-to-end call: 429 retry -> 200 with no citations despite filter => `degraded: true`.
    let xai_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let xai_addr = xai_listener.local_addr().unwrap();
    let attempts = Arc::new(Mutex::new(0usize));
    let attempts_srv = Arc::clone(&attempts);
    let xai_handle = thread::spawn(move || {
        for attempt in 0..2 {
            let Some(mut stream) = accept_timeout(&xai_listener) else {
                break;
            };
            let (_, _, _) = read_http_request(&mut stream).unwrap();
            *attempts_srv.lock().unwrap() += 1;
            if attempt == 0 {
                write_http(
                    &mut stream,
                    "429 Too Many Requests",
                    "application/json",
                    br#"{"error":"rate limited"}"#,
                );
            } else {
                let body = json!({
                    "output": [{
                        "type": "message",
                        "content": [{"type": "output_text", "text": "Summary from X"}]
                    }]
                });
                write_http(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    body.to_string().as_bytes(),
                );
            }
        }
    });

    let llm_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let llm_addr = llm_listener.local_addr().unwrap();
    let tool_out = Arc::new(Mutex::new(Value::Null));
    let tool_srv = Arc::clone(&tool_out);
    let llm_handle = thread::spawn(move || {
        for step in 0..2 {
            let Some(mut stream) = accept_timeout(&llm_listener) else {
                break;
            };
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            if step == 1 {
                let raw = parsed["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                *tool_srv.lock().unwrap() = serde_json::from_str(raw).unwrap();
            }
            let reply = if step == 0 {
                tool_completion(
                    "xs1",
                    "x_search",
                    json!({
                        "query": "hermes rust port",
                        "allowed_x_handles": ["@NousResearch"]
                    }),
                )
            } else {
                text_completion("found")
            };
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                reply.to_string().as_bytes(),
            );
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let root = hermes_runtime::paths::canonical(dir.path()).unwrap();
    let route = ModelRoute {
        provider: Provider::OpenAi(
            OpenAiProvider::new(
                &format!("http://{llm_addr}/v1"),
                "k",
                Duration::from_secs(5),
            )
            .unwrap()
            .with_streaming(false),
        ),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let xs_settings = XSearchSettings {
        api_key: Some("xai-test-key".into()),
        credential_source: "api_key".into(),
        base_url: format!("http://{xai_addr}/v1"),
        model: "grok-4.5".into(),
        reasoning_effort: Some("medium".into()),
        timeout_seconds: 30,
        retries: 1,
    };
    let mut opts = base_options(&root, None, &policy, &ack);
    opts.x_search = Some(&xs_settings);

    let outcome = run_turn(
        &route,
        &[],
        "search X",
        opts,
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    llm_handle.join().unwrap();
    xai_handle.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert_eq!(*attempts.lock().unwrap(), 2, "429 should trigger retry");

    let out = tool_out.lock().unwrap();
    assert_eq!(out["success"], json!(true));
    assert_eq!(out["answer"], json!("Summary from X"));
    assert_eq!(out["degraded"], json!(true));
    assert!(out["degraded_reason"]
        .as_str()
        .unwrap()
        .contains("allowed_x_handles"));
}

#[tokio::test]
async fn image_input_mode_text_suppresses_inline_multimodal_tool_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let root = hermes_runtime::paths::canonical(dir.path()).unwrap();
    std::fs::write(root.join("chart.png"), TINY_PNG).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen_srv = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        for step in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let (_, _, body) = read_http_request(&mut stream).unwrap();
            let req: Value = serde_json::from_slice(&body).unwrap();
            seen_srv.lock().unwrap().push(req);
            let reply = if step == 0 {
                json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_img_1",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": "{\"path\":\"chart.png\"}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }]
                })
            } else {
                json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "Saw chart metadata"},
                        "finish_reason": "stop"
                    }]
                })
            };
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                reply.to_string().as_bytes(),
            );
        }
    });

    let route = ModelRoute {
        provider: Provider::OpenAi(
            OpenAiProvider::new(&format!("http://{addr}/v1"), "k", Duration::from_secs(5))
                .unwrap()
                .with_streaming(false),
        ),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let mut opts = base_options(&root, None, &policy, &ack);
    opts.image_input_mode_text = true;

    let outcome = run_turn(
        &route,
        &[],
        "Inspect chart.png",
        opts,
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    server.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let reqs = seen_requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    let second_msgs = reqs[1]["messages"].as_array().unwrap();
    // No follow-up multimodal image_url block should have been attached in text mode.
    let has_image_block = second_msgs.iter().any(|m| {
        m["content"].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|p| p["type"].as_str() == Some("image_url"))
        })
    });
    assert!(
        !has_image_block,
        "image_input_mode=text must not attach inline image_url blocks: {second_msgs:?}"
    );
    let tool_msg = second_msgs
        .iter()
        .find(|m| m["role"].as_str() == Some("tool"))
        .unwrap();
    assert!(
        tool_msg["content"]
            .as_str()
            .unwrap()
            .contains("\"is_image\":true"),
        "{tool_msg}"
    );
}
