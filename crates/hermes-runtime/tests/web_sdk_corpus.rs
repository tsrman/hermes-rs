//! Replays the Exa and Parallel cases recorded from the Python reference
//! (`compat/create_web_sdk_corpus.py`): same request, same text for the model.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use hermes_runtime::web_tools::{extract, search, Backend, WebSettings};
use serde_json::{json, Value};

/// (path, auth headers, JSON body) of one request.
type Seen = (String, Value, Value);

const CORPUS: &str = include_str!("fixtures/web_sdk_cases.json");

/// Serve the same canned answer `count` times; returns (path, auth headers, JSON body) per request.
fn serve(status: u64, body: String, count: usize) -> (String, thread::JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..count {
            // A request that never comes fails the test instead of hanging it.
            listener.set_nonblocking(true).unwrap();
            let started = std::time::Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if started.elapsed() < Duration::from_secs(8) => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("expected another request: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
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
                head.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case(wanted)
                        .then(|| value.trim().to_owned())
                })
            };
            let length: usize = header("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            while bytes.len() - head_end < length {
                let read = stream.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..read]);
            }
            let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
            let mut auth = serde_json::Map::new();
            for name in ["authorization", "x-api-key"] {
                if let Some(value) = header(name) {
                    auth.insert(
                        name.into(),
                        json!(value.replace("synthetic-key-0000", "<key>")),
                    );
                }
            }
            let request = serde_json::from_slice(&bytes[head_end..]).unwrap_or(Value::Null);
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            seen.push((path, Value::Object(auth), request));
        }
        seen
    });
    (format!("http://{address}"), handle)
}

fn settings(vendor: &str, base: Option<String>) -> WebSettings {
    let backend = if vendor == "exa" {
        Backend::Exa
    } else {
        Backend::Parallel
    };
    let key = base.is_some().then(|| "synthetic-key-0000".to_owned());
    WebSettings {
        search: backend.clone(),
        extract: backend,
        exa_key: key.clone(),
        exa_base: base.clone(),
        parallel_key: key,
        parallel_base: base,
        allow_private_urls: true,
        ..WebSettings::default()
    }
}

fn parsed(output: &Value) -> Value {
    serde_json::from_str(output.as_str().expect("tool output is text")).unwrap()
}

fn answer_text(answer: &Value) -> String {
    // The exact text the recorder sent (Python spacing matters inside error messages).
    answer["raw"].as_str().unwrap().to_owned()
}

/// Run every recorded case; `tamper` lets the mutation check break the expectation.
fn replay(tamper: bool) -> usize {
    let cases: Vec<Value> = serde_json::from_str(CORPUS).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut checked = 0;
    for case in &cases {
        let kind = case["kind"].as_str().unwrap();
        let vendor = case["vendor"].as_str().unwrap();
        let answer = &case["answer"];
        let count = case["requests"].as_array().unwrap().len();
        let (base, handle) = serve(
            answer["status"].as_u64().unwrap(),
            answer_text(answer),
            count,
        );
        let settings = settings(vendor, Some(base));
        let output = if kind == "search" {
            let arguments = json!({"query": case["query"], "limit": case["limit"]}).to_string();
            runtime.block_on(search(&settings, &arguments))
        } else {
            let arguments = json!({"urls": case["urls"]}).to_string();
            runtime.block_on(extract(&settings, &arguments))
        };
        let got = parsed(&output);
        let label = format!("{case}");
        let seen = handle.join().unwrap();
        assert_eq!(seen.len(), count, "{label}");
        for ((path, auth, body), recorded) in seen.iter().zip(case["requests"].as_array().unwrap())
        {
            assert_eq!(path, recorded["path"].as_str().unwrap(), "{label}");
            assert_eq!(auth, &recorded["auth"], "{label}");
            assert_eq!(body, &recorded["body"], "{label}");
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
fn exa_and_parallel_match_the_reference_recording() {
    assert!(replay(false) >= 22);
}

#[test]
#[should_panic]
fn the_replay_notices_a_changed_expectation() {
    replay(true);
}
