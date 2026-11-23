use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use hermes_runtime::mcp::{McpClients, McpServerConfig};
use serde_json::{json, Value};

/// A tool answer is the JSON text the reference writes (Python separators); the tests read it as a value.
fn parsed(output: Value) -> Value {
    serde_json::from_str(output.as_str().expect("an MCP answer is JSON text"))
        .expect("an MCP answer is valid JSON")
}

struct Fixture {
    child: Child,
    url: String,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn start() -> Self {
        Self::start_with(&[])
    }

    fn start_with(flags: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.json");
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../compat/mcp_http_fixture_server.py");
        let mut child = Command::new("python3")
            .args(["-I"])
            .arg(script)
            .arg("--log")
            .arg(&log)
            .args(flags)
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
            timeout_secs: timeout_secs as f64,
            connect_timeout_secs: 10,
            resources: true,
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
    assert!(clients.owns("mcp__web__echo"));
    // The answer arrives in an event stream that also carries a notification and a request.
    let echoed = parsed(clients.call("mcp__web__echo", r#"{"text":"привет"}"#).await);
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
    let output = parsed(clients.call("mcp__web__slow", "{}").await);
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

// What the reference's own client (the `mcp` SDK) did against the same fixture server, recorded
// by `compat/create_mcp_corpus.py`.
fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/mcp_reference.json")).unwrap()
}

fn text(output: &Value) -> &str {
    output
        .as_str()
        .expect("the reference answers with JSON text")
}

fn cancellations(fixture: &Fixture) -> Vec<Value> {
    fixture
        .entries()
        .into_iter()
        .filter(|entry| entry["method"] == "notifications/cancelled")
        .collect()
}

fn resumptions(fixture: &Fixture) -> Vec<Value> {
    fixture
        .entries()
        .into_iter()
        .filter_map(|entry| entry.get("get").cloned())
        .filter(|get| !get["last_event_id"].is_null())
        .collect()
}

#[tokio::test]
async fn a_dropped_call_is_cancelled_on_the_server_as_the_reference_does() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 30)]).await;
    // The turn is cancelled while the call waits: its future is dropped.
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(400),
        clients.call("mcp__web__slow", "{}"),
    )
    .await;
    assert!(waited.is_err());
    // Shutdown waits for the notification that is still on its way.
    clients.shutdown().await;
    let recorded = corpus();
    let recorded = &recorded["wire"]["cancel_http"];
    let cancels = cancellations(&fixture);
    assert_eq!(cancels.len(), 1, "{:?}", fixture.entries());
    let entries = fixture.entries();
    let call = entries
        .iter()
        .find(|entry| entry["params"]["name"] == "slow")
        .unwrap();
    let params = cancels[0]["params"].as_object().unwrap();
    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort();
    assert_eq!(json!(keys), recorded["params_keys"]);
    assert_eq!(params["reason"], recorded["reason"]);
    assert_eq!(params["requestId"], call["id"]);
    assert_eq!(cancels[0]["session"], "fixture-session");
    assert_eq!(cancels[0]["protocol"], recorded["protocol"]);
}

#[tokio::test]
async fn a_call_that_times_out_is_cancelled_and_reported_like_the_reference() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 1)]).await;
    let output = parsed(clients.call("mcp__web__slow", "{}").await);
    assert_eq!(
        output["error"],
        "MCP call failed: TimeoutError: MCP call timed out after 1.0s (configured timeout: 1.0s)"
    );
    // The notification was written before the call returned.
    let cancels = cancellations(&fixture);
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0]["params"]["reason"], "caller cancelled");
    clients.shutdown().await;
}

#[tokio::test]
async fn finished_calls_and_initialize_are_never_cancelled() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    parsed(clients.call("mcp__web__echo", r#"{"text":"a"}"#).await);
    clients.shutdown().await;
    assert!(cancellations(&fixture).is_empty());
}

#[tokio::test]
async fn an_expired_session_is_renewed_and_a_read_only_call_is_repeated() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    // The server forgets the session; the next request gets a 404.
    let ended = parsed(clients.call("mcp__web__expire", "{}").await);
    assert_eq!(ended, json!({"result": ""}));
    let echoed = parsed(clients.call("mcp__web__echo", r#"{"text":"again"}"#).await);
    assert_eq!(echoed, json!({"result": "again"}));
    clients.shutdown().await;
    let entries = fixture.entries();
    let steps: Vec<(String, Value)> = entries
        .iter()
        .filter_map(|e| Some((e["method"].as_str()?.to_owned(), e["session"].clone())))
        .collect();
    let expected: Vec<(&str, Value)> = vec![
        ("initialize", Value::Null),
        ("notifications/initialized", json!("fixture-session")),
        ("tools/list", json!("fixture-session")),
        ("tools/call", json!("fixture-session")), // expire
        ("tools/call", json!("fixture-session")), // echo, answered with a 404
        ("initialize", Value::Null),
        ("notifications/initialized", json!("fixture-session-2")),
        ("tools/list", json!("fixture-session-2")),
        ("tools/call", json!("fixture-session-2")), // echo again
    ];
    let expected: Vec<(String, Value)> = expected
        .into_iter()
        .map(|(method, session)| (method.to_owned(), session))
        .collect();
    assert_eq!(steps, expected);
    // The new session is ended, the expired one is not.
    assert_eq!(
        entries.last().unwrap(),
        &json!({"delete": "fixture-session-2"})
    );
}

#[tokio::test]
async fn an_expired_session_is_renewed_but_a_write_call_is_not_repeated() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    parsed(clients.call("mcp__web__expire", "{}").await);
    let output = parsed(clients.call("mcp__web__write", r#"{"text":"once"}"#).await);
    let recorded = corpus()["messages"]["session_outcome_uncertain"]
        .as_str()
        .unwrap()
        .replace("{s}", "web")
        .replace("{state}", "been re-established");
    assert_eq!(
        output,
        json!({"error": recorded, "outcome_uncertain": true, "server": "web"})
    );
    // The connection is usable again, and the write reached the server only once.
    let echoed = parsed(clients.call("mcp__web__echo", r#"{"text":"after"}"#).await);
    assert_eq!(echoed, json!({"result": "after"}));
    clients.shutdown().await;
    let writes = fixture
        .entries()
        .into_iter()
        .filter(|e| e["params"]["name"] == "write")
        .count();
    assert_eq!(writes, 1);
}

#[tokio::test]
async fn a_session_that_cannot_be_renewed_reports_the_first_failure() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    // The session ends and the server refuses to open another.
    parsed(clients.call("mcp__web__expire_for_good", "{}").await);
    let recorded = corpus()["wire"]["session_expired"]["error"]["message"].clone();
    let output = parsed(clients.call("mcp__web__echo", r#"{"text":"x"}"#).await);
    assert_eq!(output, json!({"error": recorded}));
    // The next call tries again; it never reaches the server, so a write is not "uncertain".
    let output = parsed(clients.call("mcp__web__write", "{}").await);
    assert!(output["error"]
        .as_str()
        .unwrap()
        .starts_with("MCP server 'web' transport is down; reconnect requested."));
    assert!(output["outcome_uncertain"].is_null());
    let writes = fixture
        .entries()
        .iter()
        .filter(|e| e["params"]["name"] == "write")
        .count();
    assert_eq!(writes, 0);
    clients.shutdown().await;
}

#[tokio::test]
async fn a_stream_that_ends_early_is_resumed_with_last_event_id_as_the_reference_does() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    let recorded = corpus();
    let output = parsed(clients.call("mcp__web__resume", "{}").await);
    assert_eq!(output, json!({"result": "resumed"}));
    assert_eq!(
        json!(resumptions(&fixture)),
        recorded["wire"]["resume"]["gets"]
    );
    // The answer came after a second drop: the new stream carries the new event id.
    let output = parsed(clients.call("mcp__web__resume_twice", "{}").await);
    assert_eq!(output, json!({"result": "resumed"}));
    let gets = resumptions(&fixture);
    assert_eq!(
        json!(gets[1..]),
        recorded["wire"]["resume_twice"]["gets"],
        "{gets:?}"
    );
    clients.shutdown().await;
}

#[tokio::test]
async fn resumption_follows_every_new_stream_and_starts_from_a_priming_event() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    let recorded = corpus();
    // Three drops in a row would exhaust two attempts, but each resumed stream ends well first.
    let started = std::time::Instant::now();
    let output = parsed(clients.call("mcp__web__resume_thrice", "{}").await);
    assert_eq!(output, json!({"result": "resumed"}));
    // The server's `retry: 20` is the pause, not the default of a second.
    assert!(started.elapsed() < std::time::Duration::from_millis(800));
    assert_eq!(
        json!(resumptions(&fixture)),
        recorded["wire"]["resume_thrice"]["gets"]
    );
    // The only id is the one of the priming event, which has no data.
    let before = resumptions(&fixture).len();
    let output = parsed(clients.call("mcp__web__resume_primed", "{}").await);
    assert_eq!(output, json!({"result": "resumed"}));
    assert_eq!(
        json!(resumptions(&fixture)[before..]),
        recorded["wire"]["resume_primed"]["gets"]
    );
    clients.shutdown().await;
}

#[tokio::test]
async fn a_tool_of_the_server_wins_over_a_resource_utility_of_the_same_name() {
    let fixture = Fixture::start_with(&["--resources", "--native-resource-tool"]);
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    let schemas = clients.schemas();
    let readers: Vec<&Value> = schemas
        .iter()
        .filter(|schema| schema["function"]["name"] == "mcp__web__read_resource")
        .collect();
    assert_eq!(readers.len(), 1);
    assert_eq!(readers[0]["function"]["description"], "Native.");
    assert!(clients.owns("mcp__web__list_resources"));
    clients.shutdown().await;
}

#[tokio::test]
async fn resumption_gives_up_after_two_failed_attempts_with_the_reference_message() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    let recorded = corpus();
    let output = parsed(clients.call("mcp__web__resume_fail", "{}").await);
    let failed = &recorded["wire"]["resume_fail"];
    assert_eq!(output["error"], failed["outcome"]["error"]["message"]);
    assert_eq!(json!(resumptions(&fixture)), failed["gets"]);
    // Without an event id there is nothing to resume from.
    let output = parsed(clients.call("mcp__web__drop", "{}").await);
    let dropped = &recorded["wire"]["drop"];
    assert_eq!(output["error"], dropped["outcome"]["error"]["message"]);
    assert_eq!(resumptions(&fixture).len(), 2);
    clients.shutdown().await;
}

#[tokio::test]
async fn a_call_dropped_before_its_stream_is_answered_cancels_its_request() {
    let fixture = Fixture::start();
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    // `resume_fail` never answers: the call is dropped early, whatever it is doing.
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(10),
        clients.call("mcp__web__resume_fail", "{}"),
    )
    .await;
    assert!(waited.is_err());
    clients.shutdown().await;
    assert_eq!(cancellations(&fixture).len(), 1);
}

#[tokio::test]
async fn resource_tools_follow_the_servers_capability_and_the_config() {
    let plain = Fixture::start();
    let (clients, _) = McpClients::start(&[plain.config("fixture-token", 10)]).await;
    assert!(!clients.owns("mcp__web__list_resources"));
    drop(clients);

    let fixture = Fixture::start_with(&["--resources"]);
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    let recorded = corpus();
    let schemas = clients.schemas();
    let tail = &schemas[schemas.len() - 2..];
    for (schema, expected) in tail
        .iter()
        .zip(recorded["resources"]["schemas"].as_array().unwrap())
    {
        let function = &schema["function"];
        let key = expected["key"].as_str().unwrap();
        assert_eq!(function["name"], format!("mcp__web__{key}"));
        assert_eq!(
            function["description"],
            expected["description"]
                .as_str()
                .unwrap()
                .replace("{server}", "web")
        );
        assert_eq!(function["parameters"], expected["parameters"]);
    }
    clients.shutdown().await;

    let mut config = fixture.config("fixture-token", 10);
    config.resources = false;
    let (clients, _) = McpClients::start(&[config]).await;
    assert!(!clients.owns("mcp__web__list_resources"));
}

#[tokio::test]
async fn resources_are_listed_and_read_in_the_text_the_reference_gives() {
    let fixture = Fixture::start_with(&["--resources"]);
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    let recorded = corpus();
    let resources = &recorded["resources"];
    // Two pages of the list come back as one.
    let listed = clients.call("mcp__web__list_resources", "{}").await;
    assert_eq!(text(&listed), resources["list_output"].as_str().unwrap());
    for uri in ["file:///notes/a.txt", "mem://plain"] {
        let read = clients
            .call("mcp__web__read_resource", &json!({"uri": uri}).to_string())
            .await;
        assert_eq!(text(&read), resources["reads"][uri].as_str().unwrap());
    }
    // A binary resource is not saved: the model is told so instead of getting a path.
    let read = clients
        .call(
            "mcp__web__read_resource",
            r#"{"uri":"file:///notes/b.bin"}"#,
        )
        .await;
    assert_eq!(
        text(&read),
        r#"{"result": "[MCP embedded resource received (4 bytes, application/octet-stream) but document cache unavailable in this process]"}"#
    );
    // The server's error and a missing argument.
    let missing = clients
        .call("mcp__web__read_resource", r#"{"uri":"file:///missing"}"#)
        .await;
    let message = resources["missing"]["message"].as_str().unwrap();
    assert_eq!(text(&missing), format!(r#"{{"error": "{message}"}}"#));
    let no_uri = clients.call("mcp__web__read_resource", "{}").await;
    assert_eq!(
        text(&no_uri),
        corpus()["messages"]["missing_uri"].as_str().unwrap()
    );
    clients.shutdown().await;
    // The read asks the server for the uri it was given.
    let reads: Vec<Value> = fixture
        .entries()
        .into_iter()
        .filter(|e| e["method"] == "resources/read")
        .map(|e| e["params"].clone())
        .collect();
    assert_eq!(reads[0], json!({"uri": "file:///notes/a.txt"}));
}

#[tokio::test]
async fn resource_calls_are_repeated_after_a_session_expired() {
    let fixture = Fixture::start_with(&["--resources"]);
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    let recorded = corpus();
    parsed(clients.call("mcp__web__expire", "{}").await);
    let listed = clients.call("mcp__web__list_resources", "{}").await;
    assert_eq!(
        text(&listed),
        recorded["resources"]["list_output"].as_str().unwrap()
    );
    parsed(clients.call("mcp__web__expire", "{}").await);
    let read = clients
        .call("mcp__web__read_resource", r#"{"uri":"mem://plain"}"#)
        .await;
    assert_eq!(
        text(&read),
        recorded["resources"]["reads"]["mem://plain"]
            .as_str()
            .unwrap()
    );
    clients.shutdown().await;
}

#[tokio::test]
async fn resumption_works_when_the_stream_arrives_one_byte_at_a_time() {
    let fixture = Fixture::start_with(&["--dribble"]);
    let (mut clients, _) = McpClients::start(&[fixture.config("fixture-token", 10)]).await;
    // The id and retry fields of the first stream, and the whole replay, are split across reads.
    let output = parsed(clients.call("mcp__web__resume_twice", "{}").await);
    assert_eq!(output, json!({"result": "resumed"}));
    let gets = resumptions(&fixture);
    assert_eq!(
        json!(gets),
        corpus()["wire"]["resume_twice"]["gets"],
        "{gets:?}"
    );
    clients.shutdown().await;
}

#[tokio::test]
async fn an_initialize_that_times_out_is_not_cancelled() {
    // The protocol forbids cancelling `initialize`.
    let fixture = Fixture::start_with(&["--hang-initialize"]);
    let mut config = fixture.config("fixture-token", 10);
    config.connect_timeout_secs = 1;
    let (clients, warnings) = McpClients::start(&[config]).await;
    assert!(clients.schemas().is_empty());
    assert!(warnings[0].contains("initialize timed out"), "{warnings:?}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(cancellations(&fixture).is_empty());
}
