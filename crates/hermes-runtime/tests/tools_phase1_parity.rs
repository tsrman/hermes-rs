use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hermes_runtime::config::load_chat_settings;
use hermes_runtime::delegation::{Delegation, DelegationSettings};
use hermes_runtime::providers::openai::OpenAiProvider;
use hermes_runtime::providers::Provider;
use hermes_runtime::stall_guards::IntentAckContinuation;
use hermes_runtime::tool_policy::ToolPolicy;
use hermes_runtime::turn::{
    run_turn, ApprovalDecision, ModelRoute, TurnEvent, TurnHistory, TurnOptions, TurnOutcome,
};
use serde_json::{json, Value};

fn accept_timeout(listener: &TcpListener) -> Option<std::net::TcpStream> {
    listener.set_nonblocking(true).ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                return Some(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
    None
}

fn read_http_json(stream: &mut std::net::TcpStream) -> Option<Value> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let body_start;
    loop {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
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
        .unwrap_or(0);
    while bytes.len() - body_start < length {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    serde_json::from_slice(&bytes[body_start..body_start + length]).ok()
}

fn write_http_json(stream: &mut std::net::TcpStream, body: &Value) {
    let encoded = body.to_string();
    let http = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{encoded}",
        encoded.len()
    );
    let _ = stream.write_all(http.as_bytes());
}

fn text_completion(content: &str) -> Value {
    json!({
        "id": "cmpl",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    })
}

fn tool_completion(id: &str, name: &str, args: Value) -> Value {
    json!({
        "id": "cmpl",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": args.to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
}

fn base_options<'a>(
    root: &'a std::path::Path,
    policy: &'a ToolPolicy,
    ack: &'a IntentAckContinuation,
    delegation: Option<&'a Delegation>,
) -> TurnOptions<'a> {
    TurnOptions {
        mcp: None,
        web: None,
        browser: None,
        memory: None,
        vision: None,
        vision_route: None,
        image_gen: None,
        tts: None,
        x_search: None,
        root,
        profile: None,
        session_id: None,
        single_query: false,
        max_turns: 8,
        approval_timeout_secs: Some(5),
        terminal_approval_timeout_secs: None,
        terminal_local: false,
        compression: None,
        summarizer: None,
        compaction_guard: None,
        keep_phone_numbers: false,
        stall_guards: true,
        intent_ack_continuation: ack,
        terminal_env: None,
        clarify_timeout_secs: 0,
        tools: policy,
        journal: None,
        delegation,
        system_prompt_override: None,
        ephemeral_system_prompt: None,
        child: None,
        delegate_depth: 0,
        cancel_flag: None,
        steer_queue: None,
        prefill: None,
        state_scope: None,
        approval_policy: None,
    }
}

#[tokio::test]
async fn search_files_returns_absolute_paths_when_given_absolute_path_inside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/note.txt"), "needle here\n").unwrap();
    let abs_sub = root.join("sub").display().to_string();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let tool_outputs = Arc::new(Mutex::new(Vec::<Value>::new()));
    let outputs_server = Arc::clone(&tool_outputs);
    let abs_sub_srv = abs_sub.clone();
    let handle = thread::spawn(move || {
        for step in 0..4 {
            let Some(mut stream) = accept_timeout(&listener) else {
                break;
            };
            let req = read_http_json(&mut stream).unwrap();
            if step > 0 {
                let last_tool = req["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "tool")
                    .and_then(|m| m["content"].as_str())
                    .unwrap();
                outputs_server
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(last_tool).unwrap());
            }
            let reply = match step {
                0 => tool_completion(
                    "c1",
                    "search_files",
                    json!({"pattern": "needle", "path": abs_sub_srv}),
                ),
                1 => tool_completion(
                    "c2",
                    "search_files",
                    json!({"pattern": "*.txt", "target": "files", "path": abs_sub_srv}),
                ),
                2 => tool_completion(
                    "c3",
                    "search_files",
                    json!({"pattern": "needle", "path": "sub"}),
                ),
                _ => text_completion("done"),
            };
            write_http_json(&mut stream, &reply);
        }
    });

    let provider = OpenAiProvider::new(&format!("http://{addr}/v1"), "k", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let route = ModelRoute {
        provider: Provider::OpenAi(provider),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let outcome = run_turn(
        &route,
        &[],
        "search",
        base_options(&root, &policy, &ack, None),
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    handle.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let results = tool_outputs.lock().unwrap();
    assert_eq!(results.len(), 3);
    let expected_abs = root.join("sub/note.txt").display().to_string();
    assert_eq!(results[0]["matches"][0]["path"], json!(expected_abs));
    assert_eq!(results[1]["files"][0], json!(expected_abs));
    assert_eq!(results[2]["matches"][0]["path"], json!("sub/note.txt"));
}

#[tokio::test]
async fn write_file_and_patch_leave_no_temp_files_and_preserve_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = thread::spawn(move || {
        for step in 0..3 {
            let Some(mut stream) = accept_timeout(&listener) else {
                break;
            };
            let _ = read_http_json(&mut stream).unwrap();
            let reply = match step {
                0 => tool_completion(
                    "w1",
                    "write_file",
                    json!({"path": "script.sh", "content": "#!/bin/sh\necho hi\n"}),
                ),
                1 => tool_completion(
                    "p1",
                    "patch",
                    json!({"path": "script.sh", "old_string": "echo hi", "new_string": "echo ok"}),
                ),
                _ => text_completion("wrote"),
            };
            write_http_json(&mut stream, &reply);
        }
    });

    let provider = OpenAiProvider::new(&format!("http://{addr}/v1"), "k", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let route = ModelRoute {
        provider: Provider::OpenAi(provider),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let root_for_chmod = root.clone();
    let outcome = run_turn(
        &route,
        &[],
        "write and patch",
        base_options(&root, &policy, &ack, None),
        TurnHistory::default(),
        move |ev| {
            if let TurnEvent::ToolFinished {
                name, succeeded, ..
            } = ev
            {
                if name == "write_file" && succeeded {
                    let _ = std::fs::set_permissions(
                        root_for_chmod.join("script.sh"),
                        std::fs::Permissions::from_mode(0o750),
                    );
                }
            }
        },
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    handle.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let content = std::fs::read_to_string(root.join("script.sh")).unwrap();
    assert_eq!(content, "#!/bin/sh\necho ok\n");
    let mode = std::fs::metadata(root.join("script.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o750, "patch must preserve existing file mode");
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".hermes-tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
}

#[tokio::test]
async fn delegate_task_supports_top_level_goal_continue_and_control_actions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let captured_continue_messages = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured_server = Arc::clone(&captured_continue_messages);
    let handle = thread::spawn(move || {
        // Step 0 (parent): list (empty)
        // Step 1 (parent): spawn via top-level `goal`
        // Step 2 (child 1st turn): answers "first answer"
        // Step 3 (parent): continue with same subagent_id and message "follow up"
        // Step 4 (child 2nd turn): sees previous history + "follow up", answers "second answer"
        // Step 5 (parent): done
        let mut spawned_sid = String::new();
        for step in 0..6 {
            let Some(mut stream) = accept_timeout(&listener) else {
                break;
            };
            let req = read_http_json(&mut stream).unwrap();
            let reply = match step {
                0 => tool_completion("t0", "delegate_task", json!({"action": "list"})),
                1 => {
                    let list_out: Value = serde_json::from_str(
                        req["messages"].as_array().unwrap().last().unwrap()["content"]
                            .as_str()
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(list_out["action"], json!("list"));
                    assert_eq!(list_out["count"], json!(0));
                    tool_completion(
                        "t1",
                        "delegate_task",
                        json!({"goal": "analyze top-level goal", "context": "ctx"}),
                    )
                }
                2 => {
                    // Child initial turn
                    text_completion("first answer")
                }
                3 => {
                    let spawn_out: Value = serde_json::from_str(
                        req["messages"].as_array().unwrap().last().unwrap()["content"]
                            .as_str()
                            .unwrap(),
                    )
                    .unwrap();
                    spawned_sid = spawn_out["results"][0]["subagent_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    assert!(spawned_sid.starts_with("sa_"), "{spawned_sid}");
                    assert_eq!(spawn_out["results"][0]["summary"], json!("first answer"));
                    tool_completion(
                        "t2",
                        "delegate_task",
                        json!({
                            "action": "continue",
                            "subagent_id": spawned_sid,
                            "message": "now refine your answer"
                        }),
                    )
                }
                4 => {
                    // Child continued turn: verify previous conversation is replayed!
                    *captured_server.lock().unwrap() = req["messages"].as_array().unwrap().clone();
                    text_completion("second answer")
                }
                _ => {
                    let cont_out: Value = serde_json::from_str(
                        req["messages"].as_array().unwrap().last().unwrap()["content"]
                            .as_str()
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(cont_out["results"][0]["subagent_id"], json!(spawned_sid));
                    assert_eq!(cont_out["results"][0]["summary"], json!("second answer"));
                    text_completion("all finished")
                }
            };
            write_http_json(&mut stream, &reply);
        }
    });

    let provider = OpenAiProvider::new(&format!("http://{addr}/v1"), "k", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let route = ModelRoute {
        provider: Provider::OpenAi(provider),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let delegation = Delegation::new(DelegationSettings::default(), false);
    let mut reply = String::new();
    let outcome = run_turn(
        &route,
        &[],
        "go",
        base_options(&root, &policy, &ack, Some(&delegation)),
        TurnHistory::default(),
        |ev| {
            if let TurnEvent::Text(t) = ev {
                reply.push_str(t);
            }
        },
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    handle.join().unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert_eq!(reply, "all finished");
    let msgs = captured_continue_messages.lock().unwrap();
    assert!(msgs.iter().any(|m| m["content"] == json!("first answer")));
    assert_eq!(
        msgs.last().unwrap()["content"],
        json!("now refine your answer")
    );
}

#[tokio::test]
async fn delegate_task_steer_appends_to_tool_result_and_stop_interrupts_child() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("data.txt"), "hello world\n").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let delegation = Arc::new(Delegation::new(DelegationSettings::default(), false));
    let del_server = Arc::clone(&delegation);

    let saw_guidance = Arc::new(Mutex::new(false));
    let saw_guidance_srv = Arc::clone(&saw_guidance);
    let final_tool_out = Arc::new(Mutex::new(Value::Null));
    let final_tool_srv = Arc::clone(&final_tool_out);

    let handle = thread::spawn(move || {
        // 0: parent spawns child
        // 1: child calls read_file -> before answering, queue steer!
        // 2: child gets read_file result + [Supervisor guidance]: ..., we trigger stop_live!
        // 3: parent receives interrupted child result
        for step in 0..4 {
            let Some(mut stream) = accept_timeout(&listener) else {
                break;
            };
            let req = read_http_json(&mut stream).unwrap();
            let reply = match step {
                0 => tool_completion(
                    "p0",
                    "delegate_task",
                    json!({"tasks": [{"goal": "read data file carefully"}]}),
                ),
                1 => {
                    // Child is live right now; inspect list_live and queue steering.
                    let live = del_server.list_live();
                    assert_eq!(live.len(), 1);
                    let sid = live[0]["subagent_id"].as_str().unwrap();
                    assert!(del_server.steer_live(sid, "focus on line 1 only"));
                    tool_completion("c1", "read_file", json!({"path": "data.txt"}))
                }
                2 => {
                    let tool_msg = req["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .rev()
                        .find(|m| m["role"] == "tool")
                        .and_then(|m| m["content"].as_str())
                        .unwrap_or("");
                    if tool_msg.contains("[Supervisor guidance]: focus on line 1 only") {
                        *saw_guidance_srv.lock().unwrap() = true;
                    }
                    // Now request stop before the child's next iteration.
                    let live = del_server.list_live();
                    assert_eq!(live.len(), 1);
                    let sid = live[0]["subagent_id"].as_str().unwrap();
                    assert!(del_server.stop_live(sid));
                    // Return another tool call so the loop enters the next iteration and hits cancel_flag.
                    tool_completion("c2", "read_file", json!({"path": "data.txt"}))
                }
                _ => {
                    let out: Value = serde_json::from_str(
                        req["messages"].as_array().unwrap().last().unwrap()["content"]
                            .as_str()
                            .unwrap(),
                    )
                    .unwrap();
                    *final_tool_srv.lock().unwrap() = out;
                    text_completion("parent done")
                }
            };
            write_http_json(&mut stream, &reply);
        }
    });

    let provider = OpenAiProvider::new(&format!("http://{addr}/v1"), "k", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let route = ModelRoute {
        provider: Provider::OpenAi(provider),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let outcome = run_turn(
        &route,
        &[],
        "go",
        base_options(&root, &policy, &ack, Some(&delegation)),
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    handle.join().unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(
        *saw_guidance.lock().unwrap(),
        "steer guidance was not delivered"
    );
    let out = final_tool_out.lock().unwrap();
    assert_eq!(out["results"][0]["status"], json!("interrupted"));
    assert_eq!(out["results"][0]["exit_reason"], json!("interrupted"));
    assert!(delegation.list_live().is_empty());
}

#[tokio::test]
async fn delegate_task_honors_max_spawn_depth_for_nested_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    // Config parsing check for delegation.max_spawn_depth + .env fallback.
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.yaml"),
        "delegation:\n  max_spawn_depth: 2\n",
    )
    .unwrap();
    assert_eq!(
        load_chat_settings(Some(home.path()))
            .unwrap()
            .delegation
            .max_spawn_depth,
        2
    );
    std::fs::write(home.path().join("config.yaml"), "{}\n").unwrap();
    std::fs::write(home.path().join(".env"), "DELEGATION_MAX_SPAWN_DEPTH=3\n").unwrap();
    assert_eq!(
        load_chat_settings(Some(home.path()))
            .unwrap()
            .delegation
            .max_spawn_depth,
        3
    );

    // Live nested spawn: parent (depth 0) -> child (depth 1, sees delegate_task) -> grandchild (depth 2, does NOT see delegate_task).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let child_saw_delegate = Arc::new(Mutex::new(false));
    let grandchild_saw_delegate = Arc::new(Mutex::new(true));
    let c_flag = Arc::clone(&child_saw_delegate);
    let g_flag = Arc::clone(&grandchild_saw_delegate);

    let handle = thread::spawn(move || {
        for step in 0..5 {
            let Some(mut stream) = accept_timeout(&listener) else {
                break;
            };
            let req = read_http_json(&mut stream).unwrap();
            let has_delegate = req["tools"].as_array().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|t| t["function"]["name"] == json!("delegate_task"))
            });
            let reply = match step {
                0 => {
                    assert!(has_delegate, "parent must see delegate_task");
                    tool_completion("p", "delegate_task", json!({"goal": "spawn a grandchild"}))
                }
                1 => {
                    *c_flag.lock().unwrap() = has_delegate;
                    tool_completion(
                        "c",
                        "delegate_task",
                        json!({"goal": "leaf grandchild work"}),
                    )
                }
                2 => {
                    *g_flag.lock().unwrap() = has_delegate;
                    text_completion("grandchild ok")
                }
                3 => text_completion("child ok"),
                _ => text_completion("parent ok"),
            };
            write_http_json(&mut stream, &reply);
        }
    });

    let provider = OpenAiProvider::new(&format!("http://{addr}/v1"), "k", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let route = ModelRoute {
        provider: Provider::OpenAi(provider),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let delegation = Delegation::new(
        DelegationSettings {
            max_spawn_depth: 2,
            ..DelegationSettings::default()
        },
        false,
    );
    let outcome = run_turn(
        &route,
        &[],
        "nested",
        base_options(&root, &policy, &ack, Some(&delegation)),
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    handle.join().unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(
        *child_saw_delegate.lock().unwrap(),
        "depth-1 child should see delegate_task when max_spawn_depth=2"
    );
    assert!(
        !*grandchild_saw_delegate.lock().unwrap(),
        "depth-2 grandchild must NOT see delegate_task when max_spawn_depth=2"
    );
}

#[tokio::test]
async fn delegate_task_oneshot_refuses_continue_and_unknown_control_ids() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let outputs = Arc::new(Mutex::new(Vec::<Value>::new()));
    let outputs_srv = Arc::clone(&outputs);

    let handle = thread::spawn(move || {
        for step in 0..4 {
            let Some(mut stream) = accept_timeout(&listener) else {
                break;
            };
            let req = read_http_json(&mut stream).unwrap();
            if step > 0 {
                let last_tool = req["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                outputs_srv
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(last_tool).unwrap());
            }
            let reply = match step {
                0 => tool_completion(
                    "c0",
                    "delegate_task",
                    json!({"action": "continue", "subagent_id": "sa_00000001", "message": "more"}),
                ),
                1 => tool_completion(
                    "c1",
                    "delegate_task",
                    json!({"action": "steer", "subagent_id": "sa_missing", "message": "hi"}),
                ),
                2 => tool_completion(
                    "c2",
                    "delegate_task",
                    json!({"action": "stop", "subagent_id": "sa_missing"}),
                ),
                _ => text_completion("done"),
            };
            write_http_json(&mut stream, &reply);
        }
    });

    let provider = OpenAiProvider::new(&format!("http://{addr}/v1"), "k", Duration::from_secs(5))
        .unwrap()
        .with_streaming(false);
    let route = ModelRoute {
        provider: Provider::OpenAi(provider),
        provider_name: "custom",
        model: "fixture".into(),
    };
    let policy = ToolPolicy::default();
    let ack = IntentAckContinuation::default();
    let delegation = Delegation::new(DelegationSettings::default(), true);
    let outcome = run_turn(
        &route,
        &[],
        "checks",
        base_options(&root, &policy, &ack, Some(&delegation)),
        TurnHistory::default(),
        |_| {},
        |_| async { ApprovalDecision::Approved },
    )
    .await
    .unwrap();
    handle.join().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let res = outputs.lock().unwrap();
    assert_eq!(res.len(), 3);
    assert!(res[0]["error"]
        .as_str()
        .is_some_and(|e| e.contains("unavailable in one-shot sessions")));
    assert!(res[1]["error"]
        .as_str()
        .is_some_and(|e| e.contains("No live subagent 'sa_missing'")));
    assert!(res[2]["error"]
        .as_str()
        .is_some_and(|e| e.contains("No live subagent 'sa_missing'")));
}
