//! Replays the Keenable and xAI cases recorded from the Python reference
//! (`compat/create_web_rest_corpus.py`): same request, same text for the model.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use hermes_runtime::config::load_chat_settings;
use hermes_runtime::web_tools::{extract, search, Backend, WebSettings};
use serde_json::{json, Value};

/// (method, path, query, authorization, JSON body) of one request.
type Seen = (String, String, Value, String, Value);

const CORPUS: &str = include_str!("fixtures/web_rest_cases.json");

/// Serve the same canned answer `count` times.
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
            let mut line = head.split_whitespace();
            let method = line.next().unwrap_or("").to_owned();
            let target = line.next().unwrap_or("");
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let query: serde_json::Map<String, Value> = query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| {
                    (
                        k.to_owned(),
                        json!(v.replace("%3A", ":").replace("%2F", "/")),
                    )
                })
                .collect();
            let auth = header("authorization")
                .map(|value| value.replace("synthetic-key-0000", "<key>"))
                .unwrap_or_default();
            let request = serde_json::from_slice(&bytes[head_end..]).unwrap_or(Value::Null);
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            seen.push((method, path.to_owned(), Value::Object(query), auth, request));
        }
        seen
    });
    (format!("http://{address}"), handle)
}

/// Settings as the CLI would load them: keys and the `web.xai` block come from a temporary profile.
fn settings(case: &Value, base: Option<&str>) -> WebSettings {
    let vendor = case["vendor"].as_str().unwrap();
    let backend = if vendor == "xai" {
        Backend::Xai
    } else {
        Backend::Keenable
    };
    let dir = tempfile::tempdir().unwrap();
    let block = case["config_text"].as_str().unwrap_or("");
    let web = if block.is_empty() {
        "web:\n  backend: x\n".to_owned()
    } else {
        block.replacen("web:\n", "web:\n  backend: x\n", 1)
    };
    std::fs::write(
        dir.path().join("config.yaml"),
        format!("model: {{default: m, provider: custom, base_url: 'http://x/v1'}}\n{web}"),
    )
    .unwrap();
    let mut env =
        "KEENABLE_API_KEY=synthetic-key-0000\nXAI_API_KEY=synthetic-key-0000\n".to_owned();
    if vendor == "xai" {
        env.push_str(&format!(
            "XAI_BASE_URL={}\n",
            base.unwrap_or("http://127.0.0.1:1")
        ));
    }
    std::fs::write(dir.path().join(".env"), env).unwrap();
    let mut web = load_chat_settings(Some(dir.path())).unwrap().web;
    web.search = backend.clone();
    web.extract = backend;
    web.allow_private_urls = true;
    if vendor == "keenable" {
        web.keenable_base = base.map(str::to_owned);
    }
    web
}

fn parsed(output: &Value) -> Value {
    serde_json::from_str(output.as_str().expect("tool output is text")).unwrap()
}

fn answer_text(answer: &Value) -> String {
    // The exact text the recorder sent (Python spacing matters inside error messages).
    answer["text"].as_str().unwrap().to_owned()
}

/// Run every recorded case; `tamper` lets the mutation check break the expectation.
fn replay(tamper: bool) -> usize {
    let cases: Vec<Value> = serde_json::from_str(CORPUS).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut checked = 0;
    for case in &cases {
        let kind = case["kind"].as_str().unwrap();
        let answer = &case["answer"];
        let count = case["requests"].as_array().unwrap().len();
        // A case that sent nothing (a rejected configuration) needs no server.
        let (base, handle) = if count == 0 {
            (None, None)
        } else {
            let (base, handle) = serve(
                answer["status"].as_u64().unwrap(),
                answer_text(answer),
                count,
            );
            (Some(base), Some(handle))
        };
        let settings = settings(case, base.as_deref());
        let output = if kind == "search" {
            let arguments = json!({"query": case["query"], "limit": case["limit"]}).to_string();
            runtime.block_on(search(&settings, &arguments))
        } else {
            let arguments = json!({"urls": case["urls"]}).to_string();
            runtime.block_on(extract(&settings, &arguments))
        };
        let got = parsed(&output);
        let label = format!("{case}");
        let seen = handle
            .map(|handle| handle.join().unwrap())
            .unwrap_or_default();
        assert_eq!(seen.len(), count, "{label}");
        for ((method, path, query, auth, body), recorded) in
            seen.iter().zip(case["requests"].as_array().unwrap())
        {
            assert_eq!(method, recorded["method"].as_str().unwrap(), "{label}");
            assert_eq!(path, recorded["path"].as_str().unwrap(), "{label}");
            assert_eq!(query, &recorded["query"], "{label}");
            assert_eq!(auth, recorded["auth"].as_str().unwrap(), "{label}");
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
                        "url": doc["url"], "title": doc["title"].as_str().unwrap_or(""), "content": doc["content"],
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
fn keenable_and_xai_match_the_reference_recording() {
    assert!(replay(false) >= 60);
}

#[test]
#[should_panic]
fn the_replay_notices_a_changed_expectation() {
    replay(true);
}
