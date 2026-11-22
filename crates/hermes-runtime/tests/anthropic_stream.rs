use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use hermes_runtime::providers::{AnthropicProvider, ProviderError};
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
async fn reads_token_usage_from_message_start_and_delta() {
    let (url, handle) = server(vec![sse(&[
        json!({"type": "message_start", "message": {"id": "m", "usage": {"input_tokens": 5, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 30, "output_tokens": 1}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
        json!({"type": "message_stop"}),
    ])]);
    let provider = AnthropicProvider::new(&url, "secret", Duration::from_secs(5)).unwrap();
    let completion = provider.complete(&request(), |_| {}).await.unwrap();
    let usage = completion.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (55, 9));
    handle.join().unwrap();
}

#[tokio::test]
async fn streams_text_and_sends_messages_request() {
    let (url, handle) = server(vec![sse(&[
        json!({"type": "message_start", "message": {"id": "m"}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "ping"}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "При"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "вет"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        json!({"type": "message_stop"}),
    ])]);
    let provider = AnthropicProvider::new(&url, "secret", Duration::from_secs(5)).unwrap();
    let mut streamed = String::new();
    let completion = provider
        .complete(&request(), |text| streamed.push_str(text))
        .await
        .unwrap();
    assert_eq!(streamed, "Привет");
    assert_eq!(completion.content, "Привет");
    assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
    assert!(completion.tool_calls.is_empty());

    let (headers, body) = handle.join().unwrap().remove(0);
    let lower = headers.to_ascii_lowercase();
    assert!(lower.starts_with("post /v1/messages "));
    assert!(lower.contains("x-api-key: secret"));
    assert!(lower.contains("anthropic-version: 2023-06-01"));
    assert!(!lower.contains("authorization"));
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"][0]["text"], "system text");
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": "hello"}])
    );
    assert_eq!(body["tools"][0]["name"], "read_file");
}

#[tokio::test]
async fn assembles_tool_use_blocks_by_index() {
    let (url, _handle) = server(vec![sse(&[
        json!({"type": "message_start", "message": {}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t2", "name": "noargs", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"a.txt\"}"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        json!({"type": "message_stop"}),
    ])]);
    let provider = AnthropicProvider::new(&url, "k", Duration::from_secs(5)).unwrap();
    let completion = provider.complete(&request(), |_| {}).await.unwrap();
    assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(completion.tool_calls.len(), 2);
    assert_eq!(completion.tool_calls[0].id, "t1");
    assert_eq!(completion.tool_calls[0].arguments, r#"{"path":"a.txt"}"#);
    assert_eq!(completion.tool_calls[1].arguments, "{}");
}

#[tokio::test]
async fn truncated_stream_is_incomplete_and_allows_fallback() {
    let (url, _handle) = server(vec![sse(&[
        json!({"type": "message_start", "message": {}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "x"}}),
    ])]);
    let provider = AnthropicProvider::new(&url, "k", Duration::from_secs(5)).unwrap();
    let error = provider.complete(&request(), |_| {}).await.unwrap_err();
    assert!(matches!(error, ProviderError::IncompleteStream));
}

#[tokio::test]
async fn stream_error_event_is_reported() {
    let (url, _handle) = server(vec![sse(&[
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}}),
    ])]);
    let provider = AnthropicProvider::new(&url, "k", Duration::from_secs(5)).unwrap();
    let error = provider.complete(&request(), |_| {}).await.unwrap_err();
    assert!(matches!(error, ProviderError::InvalidEvent));
}

#[tokio::test]
async fn retries_overloaded_status_then_succeeds() {
    let overloaded = "HTTP/1.1 529 Overloaded\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned();
    let ok = sse(&[
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        json!({"type": "message_stop"}),
    ]);
    let (url, handle) = server(vec![overloaded, ok]);
    let provider = AnthropicProvider::new(&url, "k", Duration::from_secs(5)).unwrap();
    let completion = provider.complete(&request(), |_| {}).await.unwrap();
    assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
    assert_eq!(handle.join().unwrap().len(), 2);
}

#[tokio::test]
async fn rejects_non_object_requests_and_bad_urls() {
    assert!(AnthropicProvider::new("ftp://x", "k", Duration::from_secs(1)).is_err());
    let provider =
        AnthropicProvider::new("http://127.0.0.1:9/v1", "k", Duration::from_secs(1)).unwrap();
    let error = provider.complete(&json!([]), |_| {}).await.unwrap_err();
    assert!(matches!(error, ProviderError::InvalidRequest));
}
