use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use hermes_runtime::mcp::{McpClients, McpServerConfig};
use serde_json::{json, Value};

struct Fixture {
    child: Child,
    url: String,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.json");
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../compat/mcp_http_fixture_server.py");
        let mut child = Command::new("python3")
            .args(["-I"])
            .arg(script)
            .arg("--log")
            .arg(&log)
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

    fn config(&self, token: &str, timeout_secs: u64) -> McpServerConfig {
        McpServerConfig {
            name: "web".into(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some(self.url.clone()),
            headers: BTreeMap::from([("Authorization".into(), format!("Bearer {token}"))]),
            timeout_secs,
            connect_timeout_secs: 10,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn lists_calls_and_ends_the_session() {
    let fixture = Fixture::start();
    let (mut clients, warnings) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(clients.owns("mcp_web_echo"));
    // The answer arrives in an event stream that also carries a notification and a request.
    let echoed = clients.call("mcp_web_echo", r#"{"text":"привет"}"#).await;
    assert_eq!(echoed, json!({"result": "привет"}));
    clients.shutdown().await;

    let entries = fixture.entries();
    let methods: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    // Later requests carry the session and the negotiated protocol version.
    for entry in &entries[1..4] {
        assert_eq!(entry["session"], "fixture-session");
        assert_eq!(entry["protocol"], "2025-06-18");
    }
    assert_eq!(entries[0]["session"], Value::Null);
    let refusal = entries
        .iter()
        .find(|e| e.get("client_response").is_some())
        .unwrap();
    assert_eq!(refusal["client_response"]["error"]["code"], -32601);
    assert_eq!(
        entries.last().unwrap(),
        &json!({"delete": "fixture-session"})
    );
}

#[tokio::test]
async fn rejected_credentials_make_the_server_unavailable_without_leaking_them() {
    let fixture = Fixture::start();
    let (clients, warnings) = McpClients::start(&[fixture.config("wrong", 10)]).await;
    assert!(clients.schemas().is_empty());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("HTTP 401"), "{warnings:?}");
    assert!(!warnings[0].contains("wrong") && !warnings[0].contains("127.0.0.1"));
}

#[tokio::test]
async fn call_timeout_is_an_error() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 1)]).await;
    let output = clients.call("mcp_web_slow", "{}").await;
    assert!(output["error"].as_str().unwrap().contains("timed out"));
    clients.shutdown().await;
}

#[tokio::test]
async fn invalid_urls_and_headers_are_rejected() {
    let fixture = Fixture::start();
    let mut bad_scheme = fixture.config("t", 5);
    bad_scheme.url = Some("ftp://example.test/mcp".into());
    let mut bad_header = fixture.config("t", 5);
    bad_header.headers.insert("Bad Name".into(), "x".into());
    let (clients, warnings) = McpClients::start(&[bad_scheme, bad_header]).await;
    assert!(clients.schemas().is_empty());
    assert_eq!(warnings.len(), 2);
}
