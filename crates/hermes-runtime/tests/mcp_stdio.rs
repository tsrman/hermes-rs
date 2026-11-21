use std::collections::BTreeMap;
use std::path::PathBuf;

use hermes_runtime::mcp::{McpClients, McpServerConfig};
use serde_json::{json, Value};

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
        timeout_secs,
        connect_timeout_secs: 10,
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
    let (mut clients, warnings) = McpClients::start(&[config("my-server", 10)]).await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        names(&clients),
        [
            "mcp_my_server_echo",
            "mcp_my_server_fail",
            "mcp_my_server_env",
            "mcp_my_server_slow",
            "mcp_my_server_spawn_child",
            "mcp_my_server_image"
        ]
    );
    assert_eq!(
        clients.schemas()[0]["function"]["parameters"]["type"],
        "object"
    );
    assert!(clients.owns("mcp_my_server_echo"));
    assert!(!clients.owns("read_file"));

    let echoed = clients
        .call("mcp_my_server_echo", r#"{"text":"привет"}"#)
        .await;
    assert_eq!(echoed, json!({"result": "привет"}));
    let failed = clients.call("mcp_my_server_fail", "").await;
    assert_eq!(failed, json!({"error": "tool failed"}));
    let image = clients.call("mcp_my_server_image", "{}").await;
    assert_eq!(image, json!({"result": "[image content omitted]"}));
    let invalid = clients.call("mcp_my_server_echo", "[1]").await;
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
    let (mut clients, _) = McpClients::start(&[server]).await;
    let output = clients.call("mcp_env_env", "{}").await;
    let flags: Value = serde_json::from_str(output["result"].as_str().unwrap()).unwrap();
    assert_eq!(flags, json!({"parent_secret": false, "configured": "yes"}));
    clients.shutdown().await;
}

#[tokio::test]
async fn call_timeout_is_reported_as_an_error() {
    let (mut clients, _) = McpClients::start(&[config("s", 1)]).await;
    let output = clients.call("mcp_s_slow", "{}").await;
    assert!(output["error"].as_str().unwrap().contains("timed out"));
    clients.shutdown().await;
}

#[tokio::test]
async fn unavailable_server_is_a_warning_and_others_still_work() {
    let mut missing = config("missing", 5);
    missing.command = "/nonexistent/hermes-mcp".into();
    let (mut clients, warnings) = McpClients::start(&[missing, config("ok", 10)]).await;
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("missing"));
    assert!(clients.owns("mcp_ok_echo"));
    clients.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shutdown_stops_the_server_process_group() {
    let (mut clients, _) = McpClients::start(&[config("g", 10)]).await;
    let output = clients.call("mcp_g_spawn_child", "{}").await;
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
