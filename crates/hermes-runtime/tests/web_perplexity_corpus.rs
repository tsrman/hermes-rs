//! Replays the Perplexity cases recorded from the Python reference
//! (`compat/create_web_backend_corpus.py`): same request, same text for the model.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use hermes_runtime::web_tools::{extract, search, Backend, WebSettings};
use serde_json::{json, Value};

const CORPUS: &str = include_str!("fixtures/web_perplexity_cases.json");

/// One canned answer; returns (path, authorization header, JSON body) of the request.
fn serve(status: u64, body: String) -> (String, thread::JoinHandle<(String, String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let head_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&bytes[..head_end]).to_string();
        let header = |wanted: &str| {
            head.lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case(wanted)
                        .then(|| value.trim().to_owned())
                })
                .unwrap_or_default()
        };
        let length: usize = header("content-length").parse().unwrap_or(0);
        while bytes.len() - head_end < length {
            let read = stream.read(&mut buffer).unwrap();
            bytes.extend_from_slice(&buffer[..read]);
        }
        let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
        let request = serde_json::from_slice(&bytes[head_end..]).unwrap_or(Value::Null);
        let reply = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).unwrap();
        (path, header("authorization"), request)
    });
    (format!("http://{address}"), handle)
}

fn settings(base: Option<String>, key: bool) -> WebSettings {
    WebSettings {
        search: Backend::Perplexity,
        extract: Backend::Perplexity,
        perplexity_key: key.then(|| "pplx-synthetic-key-0000".to_owned()),
        perplexity_base: base,
        allow_private_urls: true,
        ..WebSettings::default()
    }
}

fn parsed(output: &Value) -> Value {
    serde_json::from_str(output.as_str().expect("tool output is text")).unwrap()
}

fn answer_text(answer: &Value) -> String {
    match answer.get("body") {
        Some(body) => body.to_string(),
        None => answer["text"].as_str().unwrap().to_owned(),
    }
}

/// Run every recorded case; `tamper` lets the mutation check break the expectation.
fn replay(tamper: bool) -> usize {
    let cases: Vec<Value> = serde_json::from_str(CORPUS).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut checked = 0;
    for case in &cases {
        let kind = case["kind"].as_str().unwrap();
        let no_key = case["no_key"].as_bool().unwrap_or(false);
        let (base, handle) = if no_key {
            (None, None)
        } else {
            let answer = &case["answer"];
            let (base, handle) = serve(answer["status"].as_u64().unwrap(), answer_text(answer));
            (Some(base), Some(handle))
        };
        let settings = settings(base, !no_key);
        let output = if kind == "search" {
            let arguments = json!({"query": case["query"], "limit": case["limit"]}).to_string();
            runtime.block_on(search(&settings, &arguments))
        } else {
            let arguments = json!({"urls": case["urls"]}).to_string();
            runtime.block_on(extract(&settings, &arguments))
        };
        let got = parsed(&output);
        let label = format!("{case}");
        if let Some(handle) = handle {
            let (path, authorization, body) = handle.join().unwrap();
            let recorded = &case["requests"][0];
            assert_eq!(path, recorded["path"].as_str().unwrap(), "{label}");
            assert_eq!(authorization, "Bearer pplx-synthetic-key-0000", "{label}");
            assert_eq!(body, recorded["body"], "{label}");
        } else {
            assert!(case["requests"].as_array().unwrap().is_empty());
        }
        let mut expected = case["result"].clone();
        if kind == "extract" {
            // The tool keeps url, title, content and error of each document.
            let pages: Vec<Value> = expected
                .as_array()
                .unwrap()
                .iter()
                .map(|doc| {
                    json!({
                        "url": doc["url"], "title": doc["title"], "content": doc["content"],
                        "error": doc.get("error").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect();
            expected = json!({"results": pages});
        }
        if tamper {
            expected = json!({"tampered": expected});
        }
        assert_eq!(got, expected, "{label}");
        checked += 1;
    }
    checked
}

#[test]
fn perplexity_matches_the_reference_recording() {
    assert!(replay(false) >= 31);
}

#[test]
#[should_panic]
fn the_replay_notices_a_changed_expectation() {
    replay(true);
}
