use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use hermes_runtime::providers::{ProviderError, ResponsesProvider};
use serde_json::{json, Value};

/// Serve one response and return the request headers and body.
fn server(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let mut seen = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let body_start = loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(pos) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..body_start]).to_string();
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
            stream.write_all(response.as_bytes()).unwrap();
            seen.push((
                headers,
                serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap(),
            ));
        }
        seen
    });
    (format!("http://{address}/v1"), thread)
}

fn sse(events: &[Value]) -> String {
    let body: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn request() -> Value {
    json!({
        "model": "claude-test",
        "messages": [
            {"role": "system", "content": "system text"},
            {"role": "user", "content": "hello"}
        ],
        "tools": [{"type": "function", "function": {
            "name": "read_file", "description": "d", "parameters": {"type": "object"}
        }}]
    })
}

#[tokio::test]
async fn streams_text_and_sends_responses_request() {
    let (url, handle) = server(vec![sse(&[
        json!({"type": "response.created", "response": {}}),
        json!({"type": "response.output_text.delta", "delta": "При"}),
        json!({"type": "response.output_text.delta", "delta": "вет"}),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ])]);
    let provider = ResponsesProvider::new(&url, "secret", Duration::from_secs(5)).unwrap();
    let mut streamed = String::new();
    let completion = provider
        .complete(&request(), |text| streamed.push_str(text))
        .await
        .unwrap();
    assert_eq!(streamed, "Привет");
    assert_eq!(completion.finish_reason.as_deref(), Some("stop"));

    let (headers, body) = handle.join().unwrap().remove(0);
    let lower = headers.to_ascii_lowercase();
    assert!(lower.starts_with("post /v1/responses "));
    assert!(lower.contains("authorization: bearer secret"));
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "system text");
    assert_eq!(body["input"], json!([{"role": "user", "content": "hello"}]));
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["strict"], false);
    assert!(body["prompt_cache_key"]
        .as_str()
        .is_some_and(|key| key.starts_with("pck_") && key.len() == 28));
}

#[tokio::test]
async fn collects_function_calls_in_output_order() {
    let (url, _handle) = server(vec![sse(&[
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "call_id": "c2", "name": "b"}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{}"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "function_call", "call_id": "c2", "name": "b", "arguments": ""}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "call_id": "c1", "name": "read_file", "arguments": "{\"path\":\"a\"}"}}),
        json!({"type": "response.completed", "response": {}}),
    ])]);
    let provider = ResponsesProvider::new(&url, "k", Duration::from_secs(5)).unwrap();
    let completion = provider.complete(&request(), |_| {}).await.unwrap();
    assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(completion.tool_calls[0].id, "c1");
    assert_eq!(completion.tool_calls[0].arguments, r#"{"path":"a"}"#);
    assert_eq!(completion.tool_calls[1].id, "c2");
    assert_eq!(completion.tool_calls[1].arguments, "{}");
}

#[tokio::test]
async fn maps_incomplete_and_failed_responses() {
    let (url, _handle) = server(vec![
        sse(&[
            json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}}}),
        ]),
        sse(&[json!({"type": "response.failed", "response": {"error": {"code": "server_error"}}})]),
        sse(&[
            json!({"type": "response.failed", "response": {"error": {"code": "invalid_prompt"}}}),
        ]),
        sse(&[json!({"type": "response.output_text.delta", "delta": "x"})]),
    ]);
    let provider = ResponsesProvider::new(&url, "k", Duration::from_secs(5)).unwrap();
    let length = provider.complete(&request(), |_| {}).await.unwrap();
    assert_eq!(length.finish_reason.as_deref(), Some("length"));
    let transient = provider.complete(&request(), |_| {}).await.unwrap_err();
    assert!(matches!(transient, ProviderError::IncompleteStream));
    let invalid = provider.complete(&request(), |_| {}).await.unwrap_err();
    assert!(matches!(invalid, ProviderError::InvalidEvent));
    let truncated = provider.complete(&request(), |_| {}).await.unwrap_err();
    assert!(matches!(truncated, ProviderError::IncompleteStream));
}
