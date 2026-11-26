use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use hermes_runtime::providers::ResponsesProvider;
use hermes_runtime::web_keyless::{
    fail_msg, is_rate_limitish, is_search_failover_eligible, parse_exa_search_text, parse_mcp_body,
    KeylessSettings,
};
use hermes_runtime::web_tools::{extract, search, Backend, WebSettings};
use hermes_runtime::website_policy::{
    extract_host_from_urlish, match_host_against_rule, normalize_rule, WebsitePolicy,
};
use serde_json::{json, Value};

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/web_remainder_cases.json")).unwrap()
}

fn parsed(output: &Value) -> Value {
    serde_json::from_str(output.as_str().expect("tool output is JSON text")).unwrap()
}

/// Serve canned `(status, content_type, body)` responses and return `(request_head, raw_body)`.
fn serve(
    responses: Vec<(u16, &'static str, String)>,
) -> (String, thread::JoinHandle<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, content_type, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0u8; 4096];
            let head_end = loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
            let len: usize = head
                .lines()
                .find_map(|line| {
                    let (k, v) = line.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() - head_end < len {
                let n = stream.read(&mut buf).unwrap();
                bytes.extend_from_slice(&buf[..n]);
            }
            let req_body = String::from_utf8_lossy(&bytes[head_end..]).into_owned();
            let reply = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            seen.push((head, req_body));
        }
        seen
    });
    (format!("http://{addr}"), handle)
}

#[test]
fn website_policy_helpers_and_blocklist_match_reference_corpus() {
    let wp = &corpus()["website_policy"];

    for case in wp["normalize_cases"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let expected = case["expected"].as_str().map(str::to_owned);
        assert_eq!(normalize_rule(input), expected, "normalize_rule({input:?})");
    }

    for case in wp["extract_host_cases"].as_array().unwrap() {
        let url = case["url"].as_str().unwrap();
        let expected = case["expected"].as_str().unwrap();
        assert_eq!(
            extract_host_from_urlish(url),
            expected,
            "extract_host({url:?})"
        );
    }

    for case in wp["match_cases"].as_array().unwrap() {
        let host = case["host"].as_str().unwrap();
        let pattern = case["pattern"].as_str().unwrap();
        let expected = case["expected"].as_bool().unwrap();
        assert_eq!(
            match_host_against_rule(host, pattern),
            expected,
            "match({host:?}, {pattern:?})"
        );
    }

    let home = tempfile::tempdir().unwrap();
    let shared = home.path().join("shared_blocklist.txt");
    std::fs::write(
        &shared,
        "\u{feff}# Shared list\n\nwww.Shared-Bad.org/path\n*.ads.test\nexample.com\n",
    )
    .unwrap();
    let cfg: serde_yaml::Value = serde_yaml::from_str(
        "security:\n  website_blocklist:\n    enabled: true\n    domains:\n      - Example.COM\n      - https://www.Blocked.io/secret\n      - '# ignored'\n    shared_files:\n      - shared_blocklist.txt\n      - missing_file_ignored.txt\n",
    )
    .unwrap();
    let policy = WebsitePolicy::from_config(&cfg, Some(home.path()));
    let home_str = home
        .path()
        .canonicalize()
        .unwrap_or_else(|_| home.path().to_path_buf())
        .display()
        .to_string();
    let home_str = hermes_runtime::paths::simplify(home_str.into())
        .display()
        .to_string();
    // Windows spells the shared-file path with `\`; the reference output uses `/`.
    let scrub = |text: &str| {
        hermes_runtime::paths::forward_slashes_after(&text.replace(&home_str, "<HOME>"), "<HOME>")
    };

    for case in wp["check_cases"].as_array().unwrap() {
        let url = case["url"].as_str().unwrap();
        let actual = policy.check_access(url).map(|b| {
            json!({
                "url": b.url,
                "host": b.host,
                "rule": b.rule,
                "source": scrub(&b.source),
                "message": scrub(&b.message),
            })
        });
        let actual_val = actual.unwrap_or(Value::Null);
        assert_eq!(actual_val, case["expected"], "check_access({url:?})");
    }

    let cfg_off: serde_yaml::Value = serde_yaml::from_str(
        "security:\n  website_blocklist:\n    enabled: false\n    domains: [example.com]\n",
    )
    .unwrap();
    assert!(WebsitePolicy::from_config(&cfg_off, None)
        .check_access("https://example.com/")
        .is_none());

    let cfg_default: serde_yaml::Value =
        serde_yaml::from_str("security:\n  website_blocklist:\n    domains: [example.com]\n")
            .unwrap();
    assert!(WebsitePolicy::from_config(&cfg_default, None)
        .check_access("https://example.com/")
        .is_none());
}

#[test]
fn keyless_mcp_parsers_and_failover_predicates_match_reference_corpus() {
    let km = &corpus()["keyless_mcp"];

    for case in km["mcp_body_cases"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        let body = case["body"].as_str().unwrap();
        let res = parse_mcp_body(body);
        if case["ok"].as_bool().unwrap() {
            assert_eq!(res.unwrap(), case["value"].as_str().unwrap(), "{label}");
        } else {
            assert_eq!(res.unwrap_err(), case["error"].as_str().unwrap(), "{label}");
        }
    }

    for case in km["exa_parse_cases"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let limit = case["limit"].as_u64().unwrap() as usize;
        assert_eq!(json!(parse_exa_search_text(text, limit)), case["expected"]);
    }

    for case in km["failover_cases"].as_array().unwrap() {
        let msg = case["message"].as_str().unwrap();
        assert_eq!(
            is_rate_limitish(msg),
            case["rate_limitish"].as_bool().unwrap(),
            "rate_limitish({msg:?})"
        );
        assert_eq!(
            is_search_failover_eligible(msg),
            case["search_failover_eligible"].as_bool().unwrap(),
            "failover_eligible({msg:?})"
        );
    }

    assert_eq!(
        fail_msg("exa", "search", "HTTP 429", true),
        km["fail_msg_exa_search"].as_str().unwrap()
    );
    assert_eq!(
        fail_msg("firecrawl", "extract", "HTTP 429", false),
        km["fail_msg_firecrawl_extract"].as_str().unwrap()
    );
}

#[tokio::test]
async fn web_extract_enforces_website_policy_before_fetch_and_on_redirects() {
    let cfg: serde_yaml::Value = serde_yaml::from_str(
        "security:\n  website_blocklist:\n    enabled: true\n    domains: [1.1.1.2, blocked.test]\n",
    )
    .unwrap();
    let policy = WebsitePolicy::from_config(&cfg, None);

    // One allowed URL redirects to blocked.test; the directly blocked URL (1.1.1.2) must never be sent.
    let reply = json!({"results": [
        {"url": "https://sub.blocked.test/landing", "title": "Redirected", "raw_content": "secret"}
    ]});
    let (base, handle) = serve(vec![(200, "application/json", reply.to_string())]);
    let web = WebSettings {
        search: Backend::Tavily,
        extract: Backend::Tavily,
        tavily_base: Some(base),
        website_policy: policy,
        ..WebSettings::default()
    };

    let output = parsed(
        &extract(
            &web,
            r#"{"urls":["http://1.1.1.2/nope","http://1.1.1.1/redirect"]}"#,
        )
        .await,
    );
    let results = output["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0]["error"],
        "Blocked by website policy: '1.1.1.2' matched rule '1.1.1.2' from config"
    );
    assert_eq!(
        results[0]["blocked_by_policy"],
        json!({"host": "1.1.1.2", "rule": "1.1.1.2", "source": "config"})
    );
    assert_eq!(results[1]["content"], "");
    assert_eq!(
        results[1]["error"],
        "Blocked by website policy: 'sub.blocked.test' matched rule 'blocked.test' from config"
    );
    assert_eq!(
        results[1]["blocked_by_policy"],
        json!({"host": "sub.blocked.test", "rule": "blocked.test", "source": "config"})
    );

    let reqs = handle.join().unwrap();
    let body: Value = serde_json::from_str(&reqs[0].1).unwrap();
    assert_eq!(body["urls"], json!(["http://1.1.1.1/redirect"]));
}

#[tokio::test]
async fn keyless_exa_parallel_keenable_firecrawl_and_ring_failover_work() {
    // 1. Exa keyless search (SSE) fails over on 429 to Parallel (JSON) and sets data.served_by.
    let (exa_url, exa_h) = serve(vec![(429, "text/plain", "Too Many Requests".into())]);
    let parallel_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"content": [{"type": "text", "text": json!({
            "results": [{"url": "https://p.test", "title": "P", "excerpts": ["d"]}]
        }).to_string()}]}
    })
    .to_string();
    let (par_url, par_h) = serve(vec![(200, "application/json", parallel_body)]);

    let web = WebSettings {
        search: Backend::Exa,
        extract: Backend::Exa,
        keyless: KeylessSettings {
            keyless_fallback: true,
            explicit_backend: Some("exa".into()),
            exa_mcp_url: Some(exa_url),
            parallel_mcp_url: Some(par_url),
            ..KeylessSettings::default()
        },
        ..WebSettings::default()
    };
    let out = parsed(&search(&web, r#"{"query":"rust","limit":5}"#).await);
    assert_eq!(out, corpus()["keyless_mcp"]["failover_success"]);
    exa_h.join().unwrap();
    par_h.join().unwrap();

    // 2. Exa keyless search (SSE direct success) + extract with CRAWL_HTTP_429 does NOT fail over.
    let exa_sse_search = format!(
        "event: message\ndata: {}\n\n",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "Title: Rust\nURL: https://rust-lang.org/\nHighlights:\nFast and safe\n"}]}
        })
    );
    let exa_origin_429 = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"isError": true, "content": [{"type": "text", "text": "Failed: CRAWL_HTTP_429"}]}
    })
    .to_string();
    let (exa_url2, exa_h2) = serve(vec![
        (200, "text/event-stream", exa_sse_search),
        (200, "application/json", exa_origin_429),
    ]);
    let web2 = WebSettings {
        search: Backend::Exa,
        extract: Backend::Exa,
        keyless: KeylessSettings {
            keyless_fallback: true,
            explicit_backend: Some("exa".into()),
            exa_mcp_url: Some(exa_url2),
            parallel_mcp_url: Some("http://127.0.0.1:1".into()),
            ..KeylessSettings::default()
        },
        ..WebSettings::default()
    };
    let s2 = parsed(&search(&web2, r#"{"query":"rust","limit":5}"#).await);
    assert_eq!(s2["success"], true);
    assert_eq!(s2["data"]["web"][0]["url"], "https://rust-lang.org/");
    assert_eq!(s2["data"]["web"][0]["description"], "Fast and safe");
    let e2 = parsed(&extract(&web2, r#"{"urls":["http://1.1.1.1/rate-limited"]}"#).await);
    assert_eq!(e2["results"][0]["error"], "HTTP 429 from origin");
    exa_h2.join().unwrap();

    // 3. Keenable keyless public search & extract send X-Keenable-Title without Authorization.
    let (keen_url, keen_h) = serve(vec![
        (
            200,
            "application/json",
            json!({"results": [{"url": "https://k.test", "title": "K", "snippet": "ks"}]})
                .to_string(),
        ),
        (
            200,
            "application/json",
            json!({"url": "http://1.1.1.1/page", "title": "KP", "content": "hello keenable"})
                .to_string(),
        ),
    ]);
    let web_keen = WebSettings {
        search: Backend::Keenable,
        extract: Backend::Keenable,
        keyless: KeylessSettings {
            keyless_fallback: true,
            explicit_backend: Some("keenable".into()),
            keenable_keyless_url: Some(keen_url),
            ..KeylessSettings::default()
        },
        ..WebSettings::default()
    };
    let ks = parsed(&search(&web_keen, r#"{"query":"hello","limit":3}"#).await);
    assert_eq!(ks["data"]["web"][0]["title"], "K");
    let ke = parsed(&extract(&web_keen, r#"{"urls":["http://1.1.1.1/page"]}"#).await);
    assert_eq!(ke["results"][0]["content"], "hello keenable");
    let keen_reqs = keen_h.join().unwrap();
    assert!(keen_reqs[0].0.starts_with("POST /v1/search/public "));
    assert!(keen_reqs[0]
        .0
        .to_ascii_lowercase()
        .contains("x-keenable-title: hermes-agent"));
    assert!(!keen_reqs[0]
        .0
        .to_ascii_lowercase()
        .contains("authorization:"));
    assert!(keen_reqs[1]
        .0
        .starts_with("GET /v1/fetch/public?url=http%3A%2F%2F1.1.1.1%2Fpage "));

    // 4. Exhausted ring appends all-unavailable suffix; paid tier pin skips keyless.
    let (e429, eh) = serve(vec![(429, "text/plain", "rate limit".into())]);
    let (p503, ph) = serve(vec![(503, "text/plain", "unavailable".into())]);
    let (f403, fh) = serve(vec![(403, "text/plain", "forbidden".into())]);
    let (k429, kh) = serve(vec![(429, "text/plain", "rate limit".into())]);
    let web_exhaust = WebSettings {
        search: Backend::Exa,
        extract: Backend::Exa,
        keyless: KeylessSettings {
            keyless_fallback: true,
            explicit_backend: Some("exa".into()),
            exa_mcp_url: Some(e429),
            parallel_mcp_url: Some(p503),
            firecrawl_keyless_url: Some(f403),
            keenable_keyless_url: Some(k429),
            ..KeylessSettings::default()
        },
        ..WebSettings::default()
    };
    let ex_out = parsed(&search(&web_exhaust, r#"{"query":"rust"}"#).await);
    assert_eq!(ex_out["success"], false);
    assert!(
        ex_out["error"]
            .as_str()
            .unwrap()
            .ends_with("(all keyless vendors unavailable: exa, parallel, firecrawl, keenable)"),
        "{ex_out}"
    );
    eh.join().unwrap();
    ph.join().unwrap();
    fh.join().unwrap();
    kh.join().unwrap();

    // Paid pin without key returns missing-key error instead of hitting keyless endpoint.
    let web_paid = WebSettings {
        search: Backend::Exa,
        extract: Backend::Exa,
        keyless: KeylessSettings {
            keyless_fallback: true,
            provider_tier: BTreeMap::from([("exa".into(), "paid".into())]),
            ..KeylessSettings::default()
        },
        ..WebSettings::default()
    };
    let paid_out = parsed(&search(&web_paid, r#"{"query":"rust"}"#).await);
    assert!(
        paid_out["error"]
            .as_str()
            .unwrap()
            .contains("EXA_API_KEY environment variable not set"),
        "{paid_out}"
    );

    // Free pin WITH key still uses keyless endpoint.
    let (free_url, free_h) = serve(vec![(
        200,
        "application/json",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "Title: FreeHit\nURL: https://free.test/\nHighlights:\nfree\n"}]}
        })
        .to_string(),
    )]);
    let web_free_pin = WebSettings {
        search: Backend::Exa,
        extract: Backend::Exa,
        exa_key: Some("exa-paid-key".into()),
        exa_base: Some("http://127.0.0.1:1".into()),
        keyless: KeylessSettings {
            keyless_fallback: false,
            provider_tier: BTreeMap::from([("exa".into(), "free".into())]),
            explicit_backend: Some("exa".into()),
            exa_mcp_url: Some(free_url),
            ..KeylessSettings::default()
        },
        ..WebSettings::default()
    };
    let free_out = parsed(&search(&web_free_pin, r#"{"query":"rust"}"#).await);
    assert_eq!(free_out["data"]["web"][0]["title"], "FreeHit");
    free_h.join().unwrap();
}

#[tokio::test]
async fn ddgs_parses_html_results_and_refuses_extract() {
    let html = r#"
    <html><body>
      <div class="result">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=abc">Rust &amp; Cargo</a>
        <a class="result__snippet">A <b>fast</b> &amp; reliable language&#39;s home.</a>
      </div>
      <div class="result">
        <a href="https://docs.rs/tokio" class="result__a">Tokio Docs</a>
        <div class="result__snippet">Async runtime for Rust.</div>
      </div>
      <div class="result">
        <a class="result__a" href="https://third.test/">Third</a>
        <span class="result__snippet">Trimmed by limit.</span>
      </div>
    </body></html>
    "#;
    let (base, handle) = serve(vec![(200, "text/html; charset=utf-8", html.to_owned())]);
    let web = WebSettings {
        search: Backend::Ddgs,
        extract: Backend::Ddgs,
        ddgs_base: Some(base),
        ..WebSettings::default()
    };
    let out = parsed(&search(&web, r#"{"query":"rust async","limit":2}"#).await);
    assert_eq!(out["success"], true);
    assert_eq!(
        out["data"]["web"],
        json!([
            {
                "title": "Rust & Cargo",
                "url": "https://www.rust-lang.org/",
                "description": "A fast & reliable language's home.",
                "position": 1
            },
            {
                "title": "Tokio Docs",
                "url": "https://docs.rs/tokio",
                "description": "Async runtime for Rust.",
                "position": 2
            }
        ])
    );
    let reqs = handle.join().unwrap();
    assert!(reqs[0].0.starts_with("POST / "), "{}", reqs[0].0);
    assert_eq!(reqs[0].1, "q=rust+async");

    let ext = parsed(&extract(&web, r#"{"urls":["http://1.1.1.1/a"]}"#).await);
    assert_eq!(
        ext["error"],
        "DuckDuckGo (ddgs) is a search-only backend and cannot extract URL content. Set web.extract_backend to firecrawl, tavily, keenable, exa, or parallel."
    );
}

#[tokio::test]
async fn openai_native_errors_client_side_and_swaps_responses_tool_schema() {
    let web = WebSettings {
        search: Backend::OpenaiNative,
        extract: Backend::OpenaiNative,
        ..WebSettings::default()
    };
    let s = parsed(&search(&web, r#"{"query":"rust"}"#).await);
    assert_eq!(s, corpus()["providers"]["openai_native_search_error"]);
    let e = parsed(&extract(&web, r#"{"urls":["http://1.1.1.1/a"]}"#).await);
    assert_eq!(
        e["error"],
        "OpenAI Native Web Search (Codex Responses) is a search-only backend and cannot extract URL content. Set web.extract_backend to firecrawl, tavily, keenable, exa, or parallel."
    );

    // ResponsesProvider swaps client `web_search` 1:1 for `{"type": "web_search"}` when enabled.
    let provider = ResponsesProvider::new(
        "https://api.openai.com/v1",
        "sk-test",
        Duration::from_secs(30),
    )
    .unwrap()
    .with_openai_native_web_search(true);
    let req = json!({
        "model": "gpt-5",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"type": "function", "function": {"name": "read_file", "description": "r", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "web_search", "description": "s", "parameters": {"type": "object"}}}
        ]
    });
    let (body, _) = provider.build_request_body(&req, true).unwrap();
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "read_file");
    assert_eq!(tools[1], json!({"type": "web_search"}));

    // When web_search was not offered, openai_native does NOT add it.
    let req_no_ws = json!({
        "model": "gpt-5",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"type": "function", "function": {"name": "read_file", "description": "r", "parameters": {"type": "object"}}}
        ]
    });
    let (body2, _) = provider.build_request_body(&req_no_ws, true).unwrap();
    let tools2 = body2["tools"].as_array().unwrap();
    assert_eq!(tools2.len(), 1);
    assert_eq!(tools2[0]["name"], "read_file");
}
