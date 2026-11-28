use std::collections::BTreeMap;
use std::path::PathBuf;

use hermes_runtime::mcp::{McpClients, McpServerConfig};
use serde_json::{json, Value};

/// A tool answer is the JSON text the reference writes (Python separators); the tests read it as a value.
fn parsed(output: Value) -> Value {
    serde_json::from_str(output.as_str().expect("an MCP answer is JSON text"))
        .expect("an MCP answer is valid JSON")
}

fn config(name: &str, timeout_secs: u64) -> McpServerConfig {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/mcp_fixture_server.py");
    McpServerConfig {
        name: name.to_owned(),
        command: "python3".to_owned(),
        args: vec!["-I".to_owned(), script.to_string_lossy().into_owned()],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        transport: None,
        timeout_secs: timeout_secs as f64,
        connect_timeout_secs: 10,
        resources: true,
        prompts: true,
    }
}

fn names(clients: &McpClients) -> Vec<String> {
    clients
        .schemas()
        .iter()
        .map(|schema| schema["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn lists_paginated_tools_and_returns_results() {
    let (clients, warnings) = McpClients::start(&[config("my-server", 10)]).await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        names(&clients),
        [
            "mcp__my_server__echo",
            "mcp__my_server__fail",
            "mcp__my_server__env",
            "mcp__my_server__slow",
            "mcp__my_server__spawn_child",
            "mcp__my_server__image"
        ]
    );
    assert_eq!(
        clients.schemas()[0]["function"]["parameters"]["type"],
        "object"
    );
    assert!(clients.owns("mcp__my_server__echo"));
    assert!(!clients.owns("read_file"));

    let echoed = parsed(
        clients
            .call("mcp__my_server__echo", r#"{"text":"привет"}"#)
            .await,
    );
    assert_eq!(echoed, json!({"result": "привет"}));
    let failed = parsed(clients.call("mcp__my_server__fail", "").await);
    assert_eq!(failed, json!({"error": "tool failed"}));
    let image = parsed(clients.call("mcp__my_server__image", "{}").await);
    assert_eq!(image, json!({"result": ""}));
    let invalid = parsed(clients.call("mcp__my_server__echo", "[1]").await);
    assert!(invalid["error"].is_string());
    clients.shutdown().await;
}

#[tokio::test]
async fn server_gets_a_filtered_environment() {
    std::env::set_var("HERMES_TEST_SECRET", "parent");
    let mut server = config("env", 10);
    server
        .env
        .insert("HERMES_TEST_CONFIGURED".into(), "yes".into());
    let (clients, _) = McpClients::start(&[server]).await;
    let output = parsed(clients.call("mcp__env__env", "{}").await);
    let flags: Value = serde_json::from_str(output["result"].as_str().unwrap()).unwrap();
    assert_eq!(flags, json!({"parent_secret": false, "configured": "yes"}));
    clients.shutdown().await;
}

#[tokio::test]
async fn call_timeout_is_reported_as_an_error() {
    let (clients, _) = McpClients::start(&[config("s", 1)]).await;
    let output = parsed(clients.call("mcp__s__slow", "{}").await);
    assert!(output["error"].as_str().unwrap().contains("timed out"));
    clients.shutdown().await;
}

#[tokio::test]
async fn unavailable_server_is_a_warning_and_others_still_work() {
    let mut missing = config("missing", 5);
    missing.command = "/nonexistent/hermes-mcp".into();
    let (clients, warnings) = McpClients::start(&[missing, config("ok", 10)]).await;
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("missing"));
    assert!(clients.owns("mcp__ok__echo"));
    clients.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shutdown_stops_the_server_process_group() {
    let (clients, _) = McpClients::start(&[config("g", 10)]).await;
    let output = parsed(clients.call("mcp__g__spawn_child", "{}").await);
    let pid: u32 = output["result"].as_str().unwrap().parse().unwrap();
    let alive = |pid: u32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| !stat.contains(") Z "))
            .unwrap_or(false)
    };
    assert!(alive(pid));
    clients.shutdown().await;
    for _ in 0..50 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("grandchild {pid} survived shutdown");
}

// What the reference's own client (the `mcp` SDK) did against the same fixture server, recorded
// by `compat/create_mcp_corpus.py`.
fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/mcp_reference.json")).unwrap()
}

/// A server that logs the requests it gets to a file of JSON lines.
fn logging(name: &str, timeout_secs: u64) -> (McpServerConfig, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let mut server = config(name, timeout_secs);
    server
        .env
        .insert("HERMES_MCP_LOG".into(), log.to_string_lossy().into_owned());
    (server, log, dir)
}

fn logged(log: &PathBuf) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_cancelled_as_recorded(log: &PathBuf) {
    let entries = logged(log);
    let call = entries
        .iter()
        .find(|entry| entry["method"] == "tools/call")
        .unwrap();
    let cancels: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry["method"] == "notifications/cancelled")
        .collect();
    assert_eq!(cancels.len(), 1, "{entries:?}");
    let recorded = &corpus()["wire"]["cancel_stdio"];
    let params = cancels[0]["params"].as_object().unwrap();
    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort();
    assert_eq!(json!(keys), recorded["params_keys"]);
    assert_eq!(params["reason"], recorded["reason"]);
    assert_eq!(params["requestId"], call["id"]);
}

#[tokio::test]
async fn a_dropped_call_is_cancelled_on_the_server_as_the_reference_does() {
    let (server, log, _dir) = logging("s", 30);
    let (clients, _) = McpClients::start(&[server]).await;
    // The turn is cancelled while the call waits: its future is dropped.
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(400),
        clients.call("mcp__s__slow", "{}"),
    )
    .await;
    assert!(waited.is_err());
    // Shutdown waits for the notification that is still on its way.
    clients.shutdown().await;
    assert_cancelled_as_recorded(&log);
}

#[tokio::test]
async fn a_call_that_times_out_is_cancelled_and_the_connection_stays_usable() {
    let (server, log, _dir) = logging("s", 1);
    let (clients, _) = McpClients::start(&[server]).await;
    let output = parsed(clients.call("mcp__s__slow", "{}").await);
    assert_eq!(
        output["error"],
        "MCP call failed: TimeoutError: MCP call timed out after 1.0s (configured timeout: 1.0s)"
    );
    // The server never answers a cancelled call, and the next call is not confused by it.
    let echoed = parsed(clients.call("mcp__s__echo", r#"{"text":"next"}"#).await);
    assert_eq!(echoed, json!({"result": "next"}));
    clients.shutdown().await;
    assert_cancelled_as_recorded(&log);
}

#[tokio::test]
async fn finished_calls_are_never_cancelled() {
    let (server, log, _dir) = logging("s", 10);
    let (clients, _) = McpClients::start(&[server]).await;
    parsed(clients.call("mcp__s__echo", r#"{"text":"a"}"#).await);
    parsed(clients.call("mcp__s__fail", "{}").await);
    clients.shutdown().await;
    assert!(logged(&log)
        .iter()
        .all(|entry| entry["method"] != "notifications/cancelled"));
}

#[tokio::test]
async fn resources_are_offered_only_when_the_server_advertises_them() {
    let (clients, _) = McpClients::start(&[config("plain", 10)]).await;
    assert!(!clients.owns("mcp__plain__list_resources"));
    clients.shutdown().await;

    let mut server = config("docs", 10);
    server.env.insert("HERMES_MCP_RESOURCES".into(), "1".into());
    let (clients, _) = McpClients::start(&[server.clone()]).await;
    // The utilities come after the server's own tools.
    let all = names(&clients);
    assert_eq!(
        all[all.len() - 2..],
        ["mcp__docs__list_resources", "mcp__docs__read_resource"]
    );
    let recorded = corpus();
    let resources = &recorded["resources"];
    let listed = clients.call("mcp__docs__list_resources", "").await;
    assert_eq!(listed, json!(resources["list_output"].as_str().unwrap()));
    let read = clients
        .call("mcp__docs__read_resource", r#"{"uri":"mem://plain"}"#)
        .await;
    assert_eq!(
        read,
        json!(resources["reads"]["mem://plain"].as_str().unwrap())
    );
    let missing = clients
        .call("mcp__docs__read_resource", r#"{"uri":"file:///missing"}"#)
        .await;
    let message = resources["missing"]["message"].as_str().unwrap();
    assert_eq!(missing, json!(format!(r#"{{"error": "{message}"}}"#)));
    clients.shutdown().await;

    server.resources = false;
    let (clients, _) = McpClients::start(&[server]).await;
    assert!(!clients.owns("mcp__docs__list_resources"));
    clients.shutdown().await;
}

#[tokio::test]
async fn answers_are_the_reference_text_with_python_separators() {
    let mut server = config("my-server", 10);
    server.env.insert("HERMES_MCP_EXTRA".into(), "1".into());
    let (clients, warnings) = McpClients::start(&[server]).await;
    assert!(warnings.is_empty(), "{warnings:?}");
    let echoed = clients
        .call("mcp__my_server__echo", r#"{"text":"x"}"#)
        .await;
    assert_eq!(echoed, json!(r#"{"result": "x"}"#));
    let failed = clients.call("mcp__my_server__fail", "{}").await;
    assert_eq!(failed, json!(r#"{"error": "tool failed"}"#));
    // An error without text is replaced by the reference's sentence.
    let empty = clients.call("mcp__my_server__empty_error", "{}").await;
    assert_eq!(empty, json!(r#"{"error": "MCP tool returned an error"}"#));
    // structuredContent stays next to a summary, reserved `_meta` keys are dropped.
    let rich = clients.call("mcp__my_server__rich", "{}").await;
    assert_eq!(
        rich,
        json!(
            r#"{"result": "summary", "structuredContent": {"n": 1, "ok": true}, "_meta": {"com.example/k": "v"}}"#
        )
    );
    clients.shutdown().await;
}

#[tokio::test]
async fn names_two_tools_share_are_given_to_neither_and_a_server_keeps_what_it_has() {
    let mut first = config("a-b", 10);
    first.env.insert("HERMES_MCP_EXTRA".into(), "1".into());
    first.env.insert("HERMES_MCP_RESOURCES".into(), "1".into());
    let mut second = config("a_b", 10);
    second.env.insert("HERMES_MCP_EXTRA".into(), "1".into());
    let (clients, warnings) = McpClients::start(&[first, second]).await;
    assert!(warnings.is_empty(), "{warnings:?}");
    // `dup-x` and `dup_x` are the same name once sanitised: neither is exposed. A tool listed
    // twice is exposed once. `list.resources` and `list-resources` are one name, so neither is exposed and the utility
    // of that name is dropped as well; `read_resource` stays. The second server sanitises to the same names and
    // gets none of them.
    assert_eq!(
        names(&clients),
        [
            "mcp__a_b__echo",
            "mcp__a_b__fail",
            "mcp__a_b__env",
            "mcp__a_b__slow",
            "mcp__a_b__spawn_child",
            "mcp__a_b__image",
            "mcp__a_b__empty_error",
            "mcp__a_b__rich",
            "mcp__a_b__read_resource",
        ]
    );
    clients.shutdown().await;
}

#[tokio::test]
async fn a_server_that_lists_its_tools_too_slowly_gets_the_connect_timeout_not_the_call_timeout() {
    // The reference bounds the whole connection (and so `tools/list`) with `connect_timeout`; the
    // time a tool call may take must not stretch it.
    let mut server = config("slow", 30);
    server.connect_timeout_secs = 1;
    server.env.insert("HERMES_MCP_SLOW_LIST".into(), "1".into());
    let started = std::time::Instant::now();
    let (clients, warnings) = McpClients::start(&[server]).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("timed out"), "{warnings:?}");
    clients.shutdown().await;
}
