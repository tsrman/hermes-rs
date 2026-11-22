use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use hermes_runtime::web_tools::{extract, search, Backend, WebSettings};
use serde_json::{json, Value};

/// Serve one canned response per entry and return (request head, body) pairs.
fn server(responses: Vec<(u16, String)>) -> (String, thread::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, body) in responses {
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
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() - head_end < length {
                let read = stream.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..read]);
            }
            let request_body = serde_json::from_slice(&bytes[head_end..]).unwrap_or(Value::Null);
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            seen.push((head, request_body));
        }
        seen
    });
    (format!("http://{address}"), handle)
}

fn settings(backend: Backend) -> WebSettings {
    WebSettings {
        search: backend.clone(),
        extract: backend,
        ..WebSettings::default()
    }
}

fn parsed(output: &Value) -> Value {
    serde_json::from_str(output.as_str().expect("tool output is text")).unwrap()
}

#[tokio::test]
async fn tavily_search_sends_a_bearer_request_and_formats_rows() {
    let reply = json!({"results": [
        {"title": "Один", "url": "https://a.test/", "content": "первый"},
        {"title": "Two", "url": "https://b.test/", "content": "second"}
    ]});
    let (base, handle) = server(vec![(200, reply.to_string())]);
    let mut web = settings(Backend::Tavily);
    web.tavily_key = Some("tvly-secret".into());
    web.tavily_base = Some(base);
    let output = search(&web, r#"{"query":"rust","limit":500}"#).await;
    let text = output.as_str().unwrap();
    // Indented JSON with readable non-ASCII text, as the reference prints it.
    assert!(
        text.starts_with("{\n  \"success\": true,\n  \"data\": {\n    \"web\": [\n"),
        "{text}"
    );
    assert!(text.contains("Один"));
    let rows = parsed(&output)["data"]["web"].clone();
    assert_eq!(
        rows[1],
        json!({"title": "Two", "url": "https://b.test/", "description": "second", "position": 2})
    );

    let (head, body) = handle.join().unwrap().remove(0);
    assert!(head.starts_with("POST /search "));
    assert!(head
        .to_ascii_lowercase()
        .contains("authorization: bearer tvly-secret"));
    assert_eq!(body["max_results"], 20);
    assert_eq!(body["query"], "rust");
}

#[tokio::test]
async fn tavily_errors_reach_the_model_without_the_key_or_url() {
    let (base, _handle) = server(vec![(429, "Rate limit reached for your plan".into())]);
    let mut web = settings(Backend::Tavily);
    web.tavily_key = Some("tvly-secret".into());
    web.tavily_base = Some(base.clone());
    let output = parsed(&search(&web, r#"{"query":"x"}"#).await);
    assert_eq!(
        output,
        json!({"success": false, "error": "Rate limit reached for your plan"})
    );
    web.tavily_base = Some("http://127.0.0.1:1".into());
    let unreachable = parsed(&search(&web, r#"{"query":"x"}"#).await);
    let message = unreachable["error"].as_str().unwrap();
    assert!(message.starts_with("Tavily search failed"), "{message}");
    assert!(!message.contains("tvly-secret") && !message.contains("127.0.0.1"));
}

#[tokio::test]
async fn searxng_orders_by_score_and_applies_the_limit() {
    let reply = json!({"results": [
        {"title": "low", "url": "https://l.test", "content": "c", "score": 0.1},
        {"title": "high", "url": "https://h.test", "content": "c", "score": 2.5},
        {"title": "mid", "url": "https://m.test", "content": "c", "score": 1}
    ]});
    let (base, handle) = server(vec![(200, reply.to_string()), (500, String::new())]);
    let mut web = settings(Backend::Searxng);
    web.searxng_url = Some(format!("{base}/"));
    let rows = parsed(&search(&web, r#"{"query":"q & a","limit":2}"#).await)["data"]["web"].clone();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(
        (rows[0]["title"].as_str(), rows[1]["title"].as_str()),
        (Some("high"), Some("mid"))
    );
    assert_eq!(rows[0]["position"], 1);
    let failed = parsed(&search(&web, r#"{"query":"q"}"#).await);
    assert_eq!(
        failed,
        json!({"success": false, "error": "SearXNG returned HTTP 500"})
    );
    let requests = handle.join().unwrap();
    assert!(
        requests[0]
            .0
            .starts_with("GET /search?q=q+%26+a&format=json&pageno=1 "),
        "{}",
        requests[0].0
    );
}

#[tokio::test]
async fn search_reports_missing_input_and_unsupported_backends() {
    let web = settings(Backend::Searxng);
    assert_eq!(
        parsed(&search(&web, r#"{"query":"  "}"#).await),
        json!({"error": "web_search needs a non-empty 'query' string.", "success": false})
    );
    assert_eq!(
        parsed(&search(&web, r#"{"query":"x"}"#).await),
        json!({"success": false, "error": "SEARXNG_URL is not set"})
    );
    let exa = parsed(
        &search(
            &settings(Backend::Unsupported("keenable".into())),
            r#"{"query":"x"}"#,
        )
        .await,
    );
    assert!(exa["error"]
        .as_str()
        .unwrap()
        .contains("'keenable' is not supported"));
}

#[tokio::test]
async fn extract_gates_urls_and_keeps_input_order() {
    let page = "x".repeat(5000);
    let reply = json!({"results": [
        {"url": "http://1.1.1.1/a", "title": "A", "raw_content": page},
        {"url": "http://1.1.1.1/c", "title": "C", "content": "short"}
    ], "failed_results": [{"url": "http://1.1.1.1/b", "error": "boom"}]});
    let (base, handle) = server(vec![(200, reply.to_string())]);
    let mut web = settings(Backend::Tavily);
    web.tavily_base = Some(base);
    let arguments = json!({"urls": [
        "http://1.1.1.1/a",
        {"href": "http://127.0.0.1/secret"},
        42,
        "http://1.1.1.1/b",
        "http://169.254.169.254/latest",
        "http://1.1.1.1/c"
    ], "char_limit": 2000})
    .to_string();
    let output = parsed(&extract(&web, &arguments).await);
    let results = output["results"].as_array().unwrap();
    assert_eq!(results.len(), 6);
    assert_eq!(results[0]["url"], "http://1.1.1.1/a");
    assert!(results[0]["content"]
        .as_str()
        .unwrap()
        .contains("[TRUNCATED]"));
    assert_eq!(
        results[1]["error"],
        "Blocked: URL targets a private or internal network address"
    );
    assert!(results[2]["error"]
        .as_str()
        .unwrap()
        .starts_with("Invalid URL item at index 2"));
    assert_eq!(
        (results[3]["url"].as_str(), results[3]["error"].as_str()),
        (Some("http://1.1.1.1/b"), Some("boom"))
    );
    assert_eq!(
        results[4]["error"],
        "Blocked: URL targets a private or internal network address"
    );
    assert_eq!(
        (results[5]["title"].as_str(), results[5]["content"].as_str()),
        (Some("C"), Some("short"))
    );
    // Only the allowed URLs were sent to the provider, without credentials.
    let (head, body) = handle.join().unwrap().remove(0);
    assert!(head.starts_with("POST /extract "));
    assert!(head
        .to_ascii_lowercase()
        .contains("x-tavily-access-mode: keyless"));
    assert_eq!(
        body["urls"],
        json!(["http://1.1.1.1/a", "http://1.1.1.1/b", "http://1.1.1.1/c"])
    );
}

#[tokio::test]
async fn extract_regates_redirect_targets_and_refuses_secret_urls() {
    let reply = json!({"results": [{"url": "http://10.0.0.5/internal", "title": "T", "content": "private page"}]});
    let (base, _handle) = server(vec![(200, reply.to_string())]);
    let mut web = settings(Backend::Tavily);
    web.tavily_base = Some(base);
    let output = parsed(&extract(&web, r#"{"urls":["http://1.1.1.1/start"]}"#).await);
    assert_eq!(output["results"][0]["content"], "");
    assert_eq!(
        output["results"][0]["error"],
        "Blocked: URL targets a private or internal network address"
    );

    let refused = parsed(
        &extract(
            &web,
            r#"{"urls":["https://x.test/?key=sk-abcdefghijklmnop1234"]}"#,
        )
        .await,
    );
    assert_eq!(refused["success"], false);
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("API key or token"));
}

#[tokio::test]
async fn extract_rejects_search_only_backends_and_allows_private_urls_on_request() {
    let searx = parsed(
        &extract(
            &settings(Backend::Searxng),
            r#"{"urls":["http://1.1.1.1/"]}"#,
        )
        .await,
    );
    assert!(searx["error"]
        .as_str()
        .unwrap()
        .contains("search-only backend"));

    let reply =
        json!({"results": [{"url": "http://127.0.0.1:9/p", "title": "L", "content": "local"}]});
    let (base, _handle) = server(vec![(200, reply.to_string())]);
    let mut web = settings(Backend::Tavily);
    web.tavily_base = Some(base);
    web.allow_private_urls = true;
    let output = parsed(&extract(&web, r#"{"urls":["http://127.0.0.1:9/p"]}"#).await);
    assert_eq!(output["results"][0]["content"], "local");
}
