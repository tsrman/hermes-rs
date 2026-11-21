use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use hermes_runtime::providers::openai::{OpenAiProvider, ProviderError};
use serde_json::json;

fn server(response: String) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
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
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8(bytes[body_start..body_start + length].to_vec()).unwrap()
    });
    (format!("http://{address}/v1"), thread)
}

fn ok_response(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

#[tokio::test]
async fn retries_server_error_before_streaming() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 8192];
            assert!(socket.read(&mut request).unwrap() > 0);
            let response = if attempt == 0 {
                "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
            } else {
                ok_response("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            };
            socket.write_all(response.as_bytes()).unwrap();
        }
    });
    let provider = OpenAiProvider::new(
        &format!("http://{address}/v1"),
        "fixture",
        Duration::from_secs(5),
    )
    .unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await
        .unwrap();
    assert_eq!(result.content, "ok");
    handle.join().unwrap();
}

#[tokio::test]
async fn times_out_when_provider_stalls_after_a_text_delta() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release, hold) = mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buffer = [0; 4096];
        assert!(socket.read(&mut buffer).unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n")
            .unwrap();
        let _ = hold.recv_timeout(Duration::from_secs(5));
    });
    let provider = OpenAiProvider::new(
        &format!("http://{address}/v1"),
        "fixture",
        Duration::from_secs(1),
    )
    .unwrap()
    .with_max_attempts(1);
    let mut output = String::new();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |delta| {
            output.push_str(delta)
        })
        .await;
    drop(release);
    handle.join().unwrap();
    assert!(matches!(result, Err(ProviderError::Http(error)) if error.is_timeout()));
    assert_eq!(output, "partial");
}

#[tokio::test]
async fn stops_after_three_server_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        }
    });
    let provider = OpenAiProvider::new(
        &format!("http://{address}/v1"),
        "fixture",
        Duration::from_secs(5),
    )
    .unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await;
    assert!(matches!(result, Err(ProviderError::HttpStatus(503))));
    handle.join().unwrap();
}

#[tokio::test]
async fn backoff_can_be_cancelled_without_waiting_for_retry_after() {
    let (base, handle) = server("HTTP/1.1 503 Service Unavailable\r\nRetry-After: 10\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned());
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(15)).unwrap();
    let request = json!({"model":"test", "messages":[]});
    let result = tokio::time::timeout(
        Duration::from_millis(300),
        provider.complete(&request, |_| {}),
    )
    .await;
    assert!(result.is_err());
    handle.join().unwrap();
}

#[tokio::test]
async fn accumulates_streamed_text_and_fragmented_tool_arguments() {
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Привет\",\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base, handle) = server(ok_response(body));
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(5)).unwrap();
    let mut text = String::new();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |delta| {
            text.push_str(delta)
        })
        .await
        .unwrap();
    assert_eq!(text, "Привет");
    assert_eq!(result.content, "Привет");
    assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(result.tool_calls[0].id, "call-1");
    assert_eq!(result.tool_calls[0].name, "read_file");
    assert_eq!(result.tool_calls[0].arguments, "{\"path\":\"a.txt\"}");
    let request: serde_json::Value = serde_json::from_str(&handle.join().unwrap()).unwrap();
    assert_eq!(request["stream"], true);
    assert_eq!(request["model"], "test");
}

#[tokio::test]
async fn reports_rate_limit_status() {
    let (base, handle) = server(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned(),
    );
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(5)).unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await;
    assert!(matches!(result, Err(ProviderError::HttpStatus(429))));
    handle.join().unwrap();
}

#[tokio::test]
async fn retries_explicit_transient_rate_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            assert!(socket.read(&mut request).unwrap() > 0);
            let response = if attempt == 0 {
                let body =
                    r#"{"error":{"code":"rate_limit_exceeded","message":"Rate limit exceeded"}}"#;
                format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            } else {
                ok_response("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            };
            socket.write_all(response.as_bytes()).unwrap();
        }
    });
    let provider = OpenAiProvider::new(
        &format!("http://{address}/v1"),
        "fixture",
        Duration::from_secs(5),
    )
    .unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await
        .unwrap();
    assert_eq!(result.content, "ok");
    handle.join().unwrap();
}

#[tokio::test]
async fn retries_usage_window_with_reset_header() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            assert!(socket.read(&mut request).unwrap() > 0);
            let response = if attempt == 0 {
                let body =
                    r#"{"error":{"code":"usage_limit_reached","message":"Daily quota exceeded"}}"#;
                format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            } else {
                ok_response("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            };
            socket.write_all(response.as_bytes()).unwrap();
        }
    });
    let provider = OpenAiProvider::new(
        &format!("http://{address}/v1"),
        "fixture",
        Duration::from_secs(5),
    )
    .unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await
        .unwrap();
    assert_eq!(result.content, "ok");
    handle.join().unwrap();
}

#[tokio::test]
async fn does_not_retry_exhausted_quota_even_with_retry_after() {
    let body = r#"{"error":{"code":"insufficient_quota","message":"Rate limit exceeded"}}"#;
    let response = format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let (base, handle) = server(response);
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(5)).unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await;
    assert!(matches!(result, Err(ProviderError::HttpStatus(429))));
    handle.join().unwrap();
}

#[tokio::test]
async fn does_not_retry_daily_quota_wall() {
    let body = r#"{"error":{"message":"Daily quota exceeded; rate limit reached"}}"#;
    let response = format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (base, handle) = server(response);
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(5)).unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await;
    assert!(matches!(result, Err(ProviderError::HttpStatus(429))));
    handle.join().unwrap();
}

#[tokio::test]
async fn does_not_retry_invalid_request() {
    let (base, handle) = server(
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
    );
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(5)).unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await;
    assert!(matches!(result, Err(ProviderError::HttpStatus(400))));
    handle.join().unwrap();
}

#[tokio::test]
async fn rejects_stream_without_done_marker() {
    let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let (base, handle) = server(ok_response(body));
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(5)).unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await;
    assert!(matches!(result, Err(ProviderError::IncompleteStream)));
    handle.join().unwrap();
}

#[tokio::test]
async fn rejects_done_without_finish_reason() {
    let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\ndata: [DONE]\n\n";
    let (base, handle) = server(ok_response(body));
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(5)).unwrap();
    let result = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await;
    assert!(matches!(result, Err(ProviderError::IncompleteStream)));
    handle.join().unwrap();
}

#[tokio::test]
async fn assembles_interleaved_tool_calls_in_index_order() {
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"second\",\"arguments\":\"{\"}},{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"first\",\"arguments\":\"{\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"}},{\"index\":1,\"function\":{\"arguments\":\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base, handle) = server(ok_response(body));
    let provider = OpenAiProvider::new(&base, "fixture", Duration::from_secs(5)).unwrap();
    let completion = provider
        .complete(&json!({"model":"test", "messages":[]}), |_| {})
        .await
        .unwrap();
    assert_eq!(completion.tool_calls.len(), 2);
    assert_eq!(completion.tool_calls[0].id, "a");
    assert_eq!(completion.tool_calls[0].name, "first");
    assert_eq!(completion.tool_calls[0].arguments, "{}");
    assert_eq!(completion.tool_calls[1].id, "b");
    assert_eq!(completion.tool_calls[1].name, "second");
    assert_eq!(completion.tool_calls[1].arguments, "{}");
    handle.join().unwrap();
}
