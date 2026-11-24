//! Persistent CDP WebSocket supervisor: session routing, OOPIF frame tracking, and JS dialogs.
//!
//! Mirrors `tools/browser_supervisor.py`, `tools/browser_supervisor_dialogs.py`, and
//! `tools/browser_supervisor_frames.py`. One supervisor connects per task and stays attached so
//! `browser_cdp(frame_id=...)`, `browser_dialog`, `browser_exec`, and `browser_vault` share live
//! session state without opening duplicate sockets.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::browser_cdp::resolve_endpoint;
use crate::providers::oauth::base64_encode;
use crate::redact::redact_sensitive_text;

pub(crate) const FRAME_TREE_MAX_ENTRIES: usize = 30;
pub(crate) const FRAME_TREE_MAX_OOPIF_DEPTH: usize = 2;
const MAX_DIALOG_HISTORY: usize = 50;

pub(crate) const DIALOG_BRIDGE_HOST: &str = "https://127.0.0.1/__hermes_dialog_bridge__";

/// Synchronous XHR dialog hook injected into attached pages so JS `alert`/`confirm`/`prompt`
/// can pause inside headless or CDP-controlled documents without deadlocking the browser.
pub(crate) const DIALOG_BRIDGE_SCRIPT: &str = r#"(function(){
  if (window.__hermesDialogBridgeInstalled) return;
  window.__hermesDialogBridgeInstalled = true;
  var ENDPOINT = "https://127.0.0.1/__hermes_dialog_bridge__";
  function dispatchDialog(type, message, defaultPrompt) {
    try {
      var xhr = new XMLHttpRequest();
      xhr.open("POST", ENDPOINT, false);
      xhr.setRequestHeader("Content-Type", "application/json");
      xhr.send(JSON.stringify({
        type: String(type || "alert"),
        message: message === undefined || message === null ? "" : String(message),
        defaultPrompt: defaultPrompt === undefined || defaultPrompt === null ? "" : String(defaultPrompt),
        url: String(window.location && window.location.href || "")
      }));
      if (xhr.status >= 200 && xhr.status < 300 && xhr.responseText) {
        return JSON.parse(xhr.responseText);
      }
    } catch (_err) {}
    return null;
  }
  window.alert = function(msg) {
    dispatchDialog("alert", msg, "");
  };
  window.confirm = function(msg) {
    var res = dispatchDialog("confirm", msg, "");
    return !!(res && res.accepted);
  };
  window.prompt = function(msg, def) {
    var res = dispatchDialog("prompt", msg, def === undefined ? "" : def);
    if (!res || !res.accepted) return null;
    return res.promptText === undefined || res.promptText === null ? "" : String(res.promptText);
  };
})();"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingDialog {
    pub(crate) dialog_id: String,
    #[serde(rename = "type")]
    pub(crate) dialog_type: String,
    pub(crate) message: String,
    pub(crate) default_prompt: String,
    pub(crate) url: String,
    pub(crate) has_browser_handler: bool,
    pub(crate) session_id: String,
    pub(crate) source: String,
    pub(crate) created_at: f64,
    #[serde(skip_serializing)]
    pub(crate) request_id: Option<String>,
    pub(crate) origin: String,
}

impl PendingDialog {
    pub(crate) fn redacted_json(&self) -> Value {
        json!({
            "dialog_id": self.dialog_id,
            "type": self.dialog_type,
            "message": redact_sensitive_text(&self.message),
            "default_prompt": redact_sensitive_text(&self.default_prompt),
            "url": redact_sensitive_text(&self.url),
            "origin": redact_sensitive_text(&self.origin),
            "has_browser_handler": self.has_browser_handler,
            "source": self.source,
            "created_at": self.created_at,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DialogRecord {
    pub(crate) dialog_id: String,
    #[serde(rename = "type")]
    pub(crate) dialog_type: String,
    pub(crate) message: String,
    pub(crate) default_prompt: String,
    pub(crate) url: String,
    pub(crate) origin: String,
    pub(crate) handled_at: f64,
    pub(crate) action: String,
    pub(crate) prompt_text: Option<String>,
    pub(crate) policy: String,
    pub(crate) source: String,
}

impl DialogRecord {
    pub(crate) fn redacted_json(&self) -> Value {
        json!({
            "dialog_id": self.dialog_id,
            "type": self.dialog_type,
            "message": redact_sensitive_text(&self.message),
            "default_prompt": redact_sensitive_text(&self.default_prompt),
            "url": redact_sensitive_text(&self.url),
            "origin": redact_sensitive_text(&self.origin),
            "handled_at": self.handled_at,
            "action": self.action,
            "prompt_text": self.prompt_text.as_deref().map(redact_sensitive_text),
            "policy": self.policy,
            "source": self.source,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FrameInfo {
    pub(crate) frame_id: String,
    pub(crate) parent_frame_id: Option<String>,
    pub(crate) url: String,
    pub(crate) name: String,
    pub(crate) security_origin: String,
    pub(crate) target_id: String,
    pub(crate) session_id: String,
    pub(crate) cross_origin: bool,
    pub(crate) depth: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct TargetSession {
    pub(crate) target_id: String,
    pub(crate) session_id: String,
    pub(crate) target_type: String,
    pub(crate) url: String,
}

#[derive(Debug, Clone)]
struct FrameRecord {
    session_id: String,
    root_frame_id: Option<String>,
}

struct OutboundCommand {
    id: u64,
    method: String,
    params: Value,
    session_id: Option<String>,
    reply: oneshot::Sender<Result<Value, String>>,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Default)]
struct SupervisorState {
    connected: bool,
    sessions: HashMap<String, TargetSession>,
    target_to_session: HashMap<String, String>,
    active_target_id: Option<String>,
    active_session_id: Option<String>,
    pending_dialogs: Vec<PendingDialog>,
    dialog_history: VecDeque<DialogRecord>,
    frames: HashMap<String, FrameRecord>,
    next_dialog_seq: u64,
}

impl SupervisorState {
    fn alloc_dialog_id(&mut self) -> String {
        self.next_dialog_seq += 1;
        format!("dlg-{}", self.next_dialog_seq)
    }

    fn push_history(&mut self, record: DialogRecord) {
        if self.dialog_history.len() >= MAX_DIALOG_HISTORY {
            self.dialog_history.pop_front();
        }
        self.dialog_history.push_back(record);
    }

    fn record_session(&mut self, session: TargetSession) {
        let is_page = session.target_type == "page";
        self.target_to_session
            .insert(session.target_id.clone(), session.session_id.clone());
        if is_page && self.active_target_id.is_none() {
            self.active_target_id = Some(session.target_id.clone());
            self.active_session_id = Some(session.session_id.clone());
        }
        self.sessions.insert(session.session_id.clone(), session);
    }

    fn remove_session(&mut self, session_id: &str) {
        if let Some(removed) = self.sessions.remove(session_id) {
            if self.target_to_session.get(&removed.target_id) == Some(&removed.session_id) {
                self.target_to_session.remove(&removed.target_id);
            }
            if self.active_session_id.as_deref() == Some(session_id) {
                self.active_session_id = None;
                self.active_target_id = None;
                if let Some(fallback) = self.sessions.values().find(|s| s.target_type == "page") {
                    self.active_target_id = Some(fallback.target_id.clone());
                    self.active_session_id = Some(fallback.session_id.clone());
                }
            }
        }
        self.frames
            .retain(|_, record| record.session_id != session_id);
        self.pending_dialogs
            .retain(|dlg| dlg.session_id != session_id);
    }
}

pub(crate) struct CdpSupervisor {
    endpoint: String,
    state: Arc<Mutex<SupervisorState>>,
    tx: mpsc::UnboundedSender<OutboundCommand>,
    next_call_id: std::sync::atomic::AtomicU64,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

static SUPERVISORS: OnceLock<AsyncMutex<HashMap<String, Arc<CdpSupervisor>>>> = OnceLock::new();

fn registry() -> &'static AsyncMutex<HashMap<String, Arc<CdpSupervisor>>> {
    SUPERVISORS.get_or_init(|| AsyncMutex::new(HashMap::new()))
}

fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn url_origin(raw: &str) -> String {
    reqwest::Url::parse(raw)
        .ok()
        .map(|u| u.origin().ascii_serialization())
        .filter(|o| o != "null")
        .unwrap_or_default()
}

fn parse_unbounded_json(text: &str) -> Option<Value> {
    let mut de = serde_json::Deserializer::from_str(text);
    de.disable_recursion_limit();
    let value = Value::deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(value)
}

impl CdpSupervisor {
    pub(crate) async fn connect(raw_endpoint: &str) -> Result<Arc<Self>, String> {
        let resolved = resolve_endpoint(raw_endpoint).await;
        if resolved.is_empty() {
            return Err("No CDP endpoint configured.".into());
        }
        if !resolved.starts_with("ws://") && !resolved.starts_with("wss://") {
            return Err(format!("CDP endpoint is not a WebSocket URL: {resolved}"));
        }
        let config = WebSocketConfig::default()
            .max_message_size(None)
            .max_frame_size(None);
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_with_config(&resolved, Some(config), false),
        )
        .await
        .map_err(|_| format!("Timed out connecting to CDP endpoint {resolved}"))?
        .map_err(|err| format!("WebSocket connection failed for {resolved}: {err}"))?;

        let state = Arc::new(Mutex::new(SupervisorState {
            connected: true,
            ..SupervisorState::default()
        }));
        let (tx, rx) = mpsc::unbounded_channel::<OutboundCommand>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let supervisor = Arc::new(Self {
            endpoint: resolved,
            state: Arc::clone(&state),
            tx,
            next_call_id: std::sync::atomic::AtomicU64::new(1),
            shutdown_tx: Some(shutdown_tx),
        });

        spawn_io_loop(Arc::clone(&supervisor), socket, rx, shutdown_rx);
        supervisor.initialize_auto_attach().await?;
        Ok(supervisor)
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.state.lock().map(|s| s.connected).unwrap_or(false)
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn initialize_auto_attach(&self) -> Result<(), String> {
        // Best-effort auto-attach setup: minimal mock CDP endpoints may not implement all
        // Target.setAutoAttach params, so only initial target discovery fails setup.
        let _ = self
            .call_cdp_raw(
                "Target.setAutoAttach",
                json!({
                    "autoAttach": true,
                    "waitForDebuggerOnStart": false,
                    "flatten": true,
                    "filter": [
                        {"type": "page", "exclude": false},
                        {"type": "iframe", "exclude": false}
                    ]
                }),
                None,
                5.0,
            )
            .await;

        let targets = match self
            .call_cdp_raw("Target.getTargets", json!({}), None, 5.0)
            .await
        {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        let infos = targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for info in infos {
            let ttype = info
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if ttype != "page" && ttype != "iframe" {
                continue;
            }
            let target_id = info
                .get("targetId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if target_id.is_empty() {
                continue;
            }
            let already = self
                .state
                .lock()
                .map(|s| s.target_to_session.contains_key(&target_id))
                .unwrap_or(false);
            if !already {
                let _ = self.attach_target_internal(&target_id, &info).await;
            }
        }
        Ok(())
    }

    async fn attach_target_internal(
        &self,
        target_id: &str,
        info: &Value,
    ) -> Result<String, String> {
        let attached = self
            .call_cdp_raw(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
                None,
                5.0,
            )
            .await?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Target.attachToTarget returned no sessionId".to_owned())?
            .to_owned();
        let target_type = info
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("page")
            .to_owned();
        let url = info
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if let Ok(mut state) = self.state.lock() {
            state.record_session(TargetSession {
                target_id: target_id.to_owned(),
                session_id: session_id.clone(),
                target_type,
                url,
            });
        }
        self.enable_session_domains(&session_id).await;
        Ok(session_id)
    }

    async fn enable_session_domains(&self, session_id: &str) {
        let _ = self
            .call_cdp_raw("Page.enable", json!({}), Some(session_id), 2.5)
            .await;
        let _ = self
            .call_cdp_raw("Runtime.enable", json!({}), Some(session_id), 2.5)
            .await;
        let _ = self
            .call_cdp_raw(
                "Fetch.enable",
                json!({
                    "patterns": [{"urlPattern": "https://127.0.0.1/__hermes_dialog_bridge__*"}]
                }),
                Some(session_id),
                2.5,
            )
            .await;
        let _ = self
            .call_cdp_raw(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": DIALOG_BRIDGE_SCRIPT}),
                Some(session_id),
                2.5,
            )
            .await;
    }

    pub(crate) async fn call_cdp_raw(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout_secs: f64,
    ) -> Result<Value, String> {
        if !self.is_connected() {
            return Err("CDP supervisor WebSocket is disconnected.".into());
        }
        let call_id = self
            .next_call_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(OutboundCommand {
                id: call_id,
                method: method.to_owned(),
                params,
                session_id: session_id.map(str::to_owned),
                reply: reply_tx,
            })
            .map_err(|_| "CDP supervisor channel closed".to_owned())?;

        let limit = Duration::from_secs_f64(timeout_secs.clamp(0.2, 300.0));
        tokio::time::timeout(limit, reply_rx)
            .await
            .map_err(|_| format!("Timed out waiting for CDP response to {method}"))?
            .map_err(|_| format!("CDP command {method} cancelled (connection closed)"))?
    }

    pub(crate) async fn resolve_or_attach_target(&self, target_id: &str) -> Result<String, String> {
        if let Some(session_id) = self
            .state
            .lock()
            .ok()
            .and_then(|s| s.target_to_session.get(target_id).cloned())
        {
            return Ok(session_id);
        }
        self.attach_target_internal(
            target_id,
            &json!({"targetId": target_id, "type": "page", "url": ""}),
        )
        .await
    }

    pub(crate) async fn active_session_id(&self) -> Option<String> {
        if let Some(id) = self
            .state
            .lock()
            .ok()
            .and_then(|s| s.active_session_id.clone())
        {
            return Some(id);
        }
        // Refresh from Target.getTargets if nothing cached yet.
        let targets = self
            .call_cdp_raw("Target.getTargets", json!({}), None, 5.0)
            .await
            .ok()?;
        let infos = targets.get("targetInfos").and_then(Value::as_array)?;
        for info in infos {
            if info.get("type").and_then(Value::as_str) == Some("page") {
                if let Some(tid) = info.get("targetId").and_then(Value::as_str) {
                    if let Ok(sid) = self.attach_target_internal(tid, info).await {
                        return Some(sid);
                    }
                }
            }
        }
        None
    }

    pub(crate) async fn active_target_id(&self) -> Option<String> {
        if let Some(tid) = self
            .state
            .lock()
            .ok()
            .and_then(|s| s.active_target_id.clone())
        {
            return Some(tid);
        }
        let _ = self.active_session_id().await;
        self.state
            .lock()
            .ok()
            .and_then(|s| s.active_target_id.clone())
    }

    pub(crate) fn set_active_target(&self, target_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.active_target_id = Some(target_id.to_owned());
            if let Some(sid) = state.target_to_session.get(target_id).cloned() {
                state.active_session_id = Some(sid);
            }
        }
    }

    pub(crate) async fn call_cdp(
        &self,
        method: &str,
        params: Value,
        target_id: Option<&str>,
        session_id: Option<&str>,
        timeout_secs: f64,
    ) -> Result<Value, String> {
        let resolved_session = if let Some(sid) = session_id {
            Some(sid.to_owned())
        } else if let Some(tid) = target_id {
            Some(self.resolve_or_attach_target(tid).await?)
        } else {
            None
        };
        self.call_cdp_raw(method, params, resolved_session.as_deref(), timeout_secs)
            .await
    }

    pub(crate) async fn list_sessions(&self) -> Vec<TargetSession> {
        self.state
            .lock()
            .map(|s| s.sessions.values().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) async fn list_frame_tree(&self) -> Result<Vec<FrameInfo>, String> {
        let _ = self.active_session_id().await;
        let sessions = self.list_sessions().await;
        let mut out = Vec::new();
        let mut updates = Vec::new();

        for sess in sessions {
            if out.len() >= FRAME_TREE_MAX_ENTRIES {
                break;
            }
            let Ok(tree) = self
                .call_cdp_raw("Page.getFrameTree", json!({}), Some(&sess.session_id), 5.0)
                .await
            else {
                continue;
            };
            let Some(root) = tree.get("frameTree") else {
                continue;
            };
            let root_id = root
                .pointer("/frame/id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let root_origin = root
                .pointer("/frame/securityOrigin")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            collect_frames(
                root,
                &sess,
                root_id.as_deref(),
                &root_origin,
                0,
                &mut out,
                &mut updates,
            );
        }

        if let Ok(mut state) = self.state.lock() {
            for (fid, rec) in updates {
                state.frames.insert(fid, rec);
            }
        }
        Ok(out)
    }

    pub(crate) async fn call_in_frame(
        &self,
        frame_id: &str,
        method: &str,
        params: &Value,
        timeout_secs: f64,
    ) -> Result<Value, String> {
        let cached = self
            .state
            .lock()
            .ok()
            .and_then(|s| s.frames.get(frame_id).cloned());
        let record = match cached {
            Some(rec) => rec,
            None => {
                let _ = self.list_frame_tree().await?;
                self.state
                    .lock()
                    .ok()
                    .and_then(|s| s.frames.get(frame_id).cloned())
                    .or_else(|| {
                        // Also allow addressing an OOPIF directly when frame_id == target_id
                        self.state.lock().ok().and_then(|s| {
                            s.target_to_session.get(frame_id).map(|sid| FrameRecord {
                                session_id: sid.clone(),
                                root_frame_id: Some(frame_id.to_owned()),
                            })
                        })
                    })
                    .ok_or_else(|| {
                        format!(
                            "Unknown frame_id '{frame_id}'. Call Page.getFrameTree to list active frame ids."
                        )
                    })?
            }
        };

        let mut call_params = if params.is_object() {
            params.clone()
        } else {
            json!({})
        };

        // Same-process subframe: if Runtime.evaluate has no explicit contextId and the frame is a
        // child frame inside this session, try creating an isolated world for that frame.
        if method == "Runtime.evaluate"
            && call_params.get("contextId").is_none()
            && record.root_frame_id.as_deref() != Some(frame_id)
        {
            if let Ok(world) = self
                .call_cdp_raw(
                    "Page.createIsolatedWorld",
                    json!({
                        "frameId": frame_id,
                        "worldName": "hermes_frame",
                        "grantUniveralAccess": false
                    }),
                    Some(&record.session_id),
                    5.0,
                )
                .await
            {
                if let Some(ctx_id) = world.get("executionContextId").and_then(Value::as_i64) {
                    if let Some(map) = call_params.as_object_mut() {
                        map.insert("contextId".into(), json!(ctx_id));
                    }
                }
            }
        }

        self.call_cdp_raw(method, call_params, Some(&record.session_id), timeout_secs)
            .await
    }

    pub(crate) async fn evaluate_runtime(
        &self,
        expression: &str,
        target_id: Option<&str>,
        frame_id: Option<&str>,
        await_promise: bool,
        timeout_secs: f64,
    ) -> Result<Value, String> {
        let params = json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": await_promise,
        });
        let raw = if let Some(fid) = frame_id {
            self.call_in_frame(fid, "Runtime.evaluate", &params, timeout_secs)
                .await?
        } else {
            let sid = match target_id {
                Some(tid) => Some(self.resolve_or_attach_target(tid).await?),
                None => self.active_session_id().await,
            };
            self.call_cdp_raw("Runtime.evaluate", params, sid.as_deref(), timeout_secs)
                .await?
        };
        if let Some(exc) = raw.get("exceptionDetails") {
            let detail = exc
                .pointer("/exception/description")
                .or_else(|| exc.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("Runtime evaluation threw an exception");
            return Err(redact_sensitive_text(detail));
        }
        Ok(raw.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    pub(crate) fn list_dialogs(&self) -> (Vec<PendingDialog>, Vec<DialogRecord>) {
        self.state
            .lock()
            .map(|s| {
                (
                    s.pending_dialogs.clone(),
                    s.dialog_history.iter().cloned().collect(),
                )
            })
            .unwrap_or_default()
    }

    pub(crate) async fn respond_to_dialog(
        &self,
        action: &str,
        prompt_text: Option<&str>,
        dialog_id: Option<&str>,
        timeout_secs: f64,
    ) -> Result<DialogRecord, String> {
        let accept = match action {
            "accept" => true,
            "dismiss" => false,
            other => return Err(format!("Unsupported dialog action: {other}")),
        };
        let target_dialog = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Supervisor state lock poisoned".to_owned())?;
            let idx = if let Some(wanted) = dialog_id {
                state
                    .pending_dialogs
                    .iter()
                    .position(|d| d.dialog_id == wanted)
                    .ok_or_else(|| format!("No pending dialog with dialog_id='{wanted}'"))?
            } else {
                if state.pending_dialogs.is_empty() {
                    return Err("No pending browser dialog.".into());
                }
                state.pending_dialogs.len() - 1
            };
            state.pending_dialogs.remove(idx)
        };

        let result = if target_dialog.source == "bridge" {
            let req_id = target_dialog
                .request_id
                .as_deref()
                .ok_or_else(|| "Bridge dialog missing request_id".to_owned())?;
            let payload = json!({
                "accepted": accept,
                "promptText": if accept {
                    prompt_text.unwrap_or(&target_dialog.default_prompt)
                } else {
                    ""
                }
            })
            .to_string();
            let body_b64 = base64_encode(payload.as_bytes());
            self.call_cdp_raw(
                "Fetch.fulfillRequest",
                json!({
                    "requestId": req_id,
                    "responseCode": 200,
                    "responseHeaders": [{"name": "Content-Type", "value": "application/json"}],
                    "body": body_b64
                }),
                Some(&target_dialog.session_id),
                timeout_secs,
            )
            .await
        } else {
            let mut params = Map::new();
            params.insert("accept".into(), json!(accept));
            if accept {
                if let Some(text) = prompt_text {
                    params.insert("promptText".into(), json!(text));
                }
            }
            self.call_cdp_raw(
                "Page.handleJavaScriptDialog",
                Value::Object(params),
                Some(&target_dialog.session_id),
                timeout_secs,
            )
            .await
        };

        if let Err(err) = result {
            // Re-queue if transport failed and not due to already-closed dialog.
            if !err.to_lowercase().contains("no dialog") {
                if let Ok(mut state) = self.state.lock() {
                    state.pending_dialogs.push(target_dialog);
                }
            }
            return Err(err);
        }

        let record = DialogRecord {
            dialog_id: target_dialog.dialog_id,
            dialog_type: target_dialog.dialog_type,
            message: target_dialog.message,
            default_prompt: target_dialog.default_prompt,
            url: target_dialog.url,
            origin: target_dialog.origin,
            handled_at: now_epoch_secs(),
            action: if accept { "accepted" } else { "dismissed" }.to_owned(),
            prompt_text: if accept {
                prompt_text.map(str::to_owned)
            } else {
                None
            },
            policy: "manual".into(),
            source: target_dialog.source,
        };
        if let Ok(mut state) = self.state.lock() {
            state.push_history(record.clone());
        }
        Ok(record)
    }

    fn handle_event(&self, method: &str, params: &Value, session_id: Option<&str>) {
        match method {
            "Target.attachedToTarget" => {
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
                let target_id = info
                    .get("targetId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let target_type = info
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("page")
                    .to_owned();
                let url = info
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let waiting = params
                    .get("waitingForDebugger")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !sid.is_empty() && !target_id.is_empty() {
                    if let Ok(mut state) = self.state.lock() {
                        state.record_session(TargetSession {
                            target_id,
                            session_id: sid.clone(),
                            target_type: target_type.clone(),
                            url,
                        });
                    }
                    if target_type == "page" || target_type == "iframe" {
                        let tx = self.tx.clone();
                        let next_id = &self.next_call_id;
                        enqueue_fire_and_forget(&tx, next_id, "Page.enable", json!({}), Some(&sid));
                        enqueue_fire_and_forget(
                            &tx,
                            next_id,
                            "Runtime.enable",
                            json!({}),
                            Some(&sid),
                        );
                        enqueue_fire_and_forget(
                            &tx,
                            next_id,
                            "Fetch.enable",
                            json!({
                                "patterns": [{"urlPattern": "https://127.0.0.1/__hermes_dialog_bridge__*"}]
                            }),
                            Some(&sid),
                        );
                        enqueue_fire_and_forget(
                            &tx,
                            next_id,
                            "Page.addScriptToEvaluateOnNewDocument",
                            json!({"source": DIALOG_BRIDGE_SCRIPT}),
                            Some(&sid),
                        );
                        if waiting {
                            enqueue_fire_and_forget(
                                &tx,
                                next_id,
                                "Runtime.runIfWaitingForDebugger",
                                json!({}),
                                Some(&sid),
                            );
                        }
                    }
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(sid) = params.get("sessionId").and_then(Value::as_str) {
                    if let Ok(mut state) = self.state.lock() {
                        state.remove_session(sid);
                    }
                }
            }
            "Target.targetInfoChanged" => {
                let info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
                let target_id = info
                    .get("targetId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let new_url = info.get("url").and_then(Value::as_str).unwrap_or_default();
                if !target_id.is_empty() {
                    if let Ok(mut state) = self.state.lock() {
                        if let Some(sid) = state.target_to_session.get(target_id).cloned() {
                            if let Some(sess) = state.sessions.get_mut(&sid) {
                                sess.url = new_url.to_owned();
                            }
                        }
                    }
                }
            }
            "Page.javascriptDialogOpening" => {
                let Some(sid) = session_id else {
                    return;
                };
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let dialog_type = params
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("alert")
                    .to_owned();
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let default_prompt = params
                    .get("defaultPrompt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let has_browser_handler = params
                    .get("hasBrowserHandler")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Ok(mut state) = self.state.lock() {
                    let dialog_id = state.alloc_dialog_id();
                    state.pending_dialogs.push(PendingDialog {
                        dialog_id,
                        dialog_type,
                        message,
                        default_prompt,
                        origin: url_origin(&url),
                        url,
                        has_browser_handler,
                        session_id: sid.to_owned(),
                        source: "cdp".into(),
                        created_at: now_epoch_secs(),
                        request_id: None,
                    });
                }
            }
            "Page.javascriptDialogClosed" => {
                let Some(sid) = session_id else {
                    return;
                };
                let accepted = params
                    .get("result")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let user_input = params
                    .get("userInput")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Ok(mut state) = self.state.lock() {
                    if let Some(pos) = state
                        .pending_dialogs
                        .iter()
                        .position(|d| d.session_id == sid && d.source == "cdp")
                    {
                        let dlg = state.pending_dialogs.remove(pos);
                        state.push_history(DialogRecord {
                            dialog_id: dlg.dialog_id,
                            dialog_type: dlg.dialog_type,
                            message: dlg.message,
                            default_prompt: dlg.default_prompt,
                            url: dlg.url,
                            origin: dlg.origin,
                            handled_at: now_epoch_secs(),
                            action: if accepted { "accepted" } else { "dismissed" }.to_owned(),
                            prompt_text: user_input,
                            policy: "browser".into(),
                            source: dlg.source,
                        });
                    }
                }
            }
            "Fetch.requestPaused" => {
                let Some(sid) = session_id else {
                    return;
                };
                let url = params
                    .pointer("/request/url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let req_id = params
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if req_id.is_empty() {
                    return;
                }
                if !url.starts_with(DIALOG_BRIDGE_HOST) {
                    enqueue_fire_and_forget(
                        &self.tx,
                        &self.next_call_id,
                        "Fetch.continueRequest",
                        json!({"requestId": req_id}),
                        Some(sid),
                    );
                    return;
                }
                let post_data = params
                    .pointer("/request/postData")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let parsed: Value = serde_json::from_str(post_data).unwrap_or(Value::Null);
                let dlg_url = parsed
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let dlg_type = parsed
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("alert")
                    .to_owned();
                let message = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let default_prompt = parsed
                    .get("defaultPrompt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if let Ok(mut state) = self.state.lock() {
                    let dialog_id = state.alloc_dialog_id();
                    state.pending_dialogs.push(PendingDialog {
                        dialog_id,
                        dialog_type: dlg_type,
                        message,
                        default_prompt,
                        origin: url_origin(&dlg_url),
                        url: dlg_url,
                        has_browser_handler: false,
                        session_id: sid.to_owned(),
                        source: "bridge".into(),
                        created_at: now_epoch_secs(),
                        request_id: Some(req_id.to_owned()),
                    });
                }
            }
            "Page.frameDetached" => {
                if let Some(fid) = params.get("frameId").and_then(Value::as_str) {
                    if let Ok(mut state) = self.state.lock() {
                        state.frames.remove(fid);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Drop for CdpSupervisor {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn enqueue_fire_and_forget(
    tx: &mpsc::UnboundedSender<OutboundCommand>,
    next_call_id: &std::sync::atomic::AtomicU64,
    method: &str,
    params: Value,
    session_id: Option<&str>,
) {
    let id = next_call_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (reply, _) = oneshot::channel();
    let _ = tx.send(OutboundCommand {
        id,
        method: method.to_owned(),
        params,
        session_id: session_id.map(str::to_owned),
        reply,
    });
}

fn collect_frames(
    node: &Value,
    sess: &TargetSession,
    root_frame_id: Option<&str>,
    root_origin: &str,
    depth: usize,
    out: &mut Vec<FrameInfo>,
    updates: &mut Vec<(String, FrameRecord)>,
) {
    if depth > FRAME_TREE_MAX_OOPIF_DEPTH || out.len() >= FRAME_TREE_MAX_ENTRIES {
        return;
    }
    if let Some(frame) = node.get("frame") {
        let frame_id = frame
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !frame_id.is_empty() {
            let security_origin = frame
                .get("securityOrigin")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let cross_origin = !root_origin.is_empty()
                && !security_origin.is_empty()
                && security_origin != root_origin;
            out.push(FrameInfo {
                frame_id: frame_id.clone(),
                parent_frame_id: frame
                    .get("parentId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                url: frame
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                name: frame
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                security_origin,
                target_id: sess.target_id.clone(),
                session_id: sess.session_id.clone(),
                cross_origin,
                depth,
            });
            updates.push((
                frame_id,
                FrameRecord {
                    session_id: sess.session_id.clone(),
                    root_frame_id: root_frame_id.map(str::to_owned),
                },
            ));
        }
    }
    if let Some(children) = node.get("childFrames").and_then(Value::as_array) {
        for child in children {
            if out.len() >= FRAME_TREE_MAX_ENTRIES {
                break;
            }
            collect_frames(
                child,
                sess,
                root_frame_id,
                root_origin,
                depth + 1,
                out,
                updates,
            );
        }
    }
}

fn spawn_io_loop(
    supervisor: Arc<CdpSupervisor>,
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    mut rx: mpsc::UnboundedReceiver<OutboundCommand>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let (mut ws_write, mut ws_read) = socket.split();

    let writer_pending = Arc::clone(&pending);
    let writer_state = Arc::clone(&supervisor.state);
    let writer_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    let mut payload = json!({
                        "id": cmd.id,
                        "method": cmd.method,
                        "params": cmd.params,
                    });
                    if let Some(sid) = cmd.session_id {
                        payload["sessionId"] = json!(sid);
                    }
                    if let Ok(mut map) = writer_pending.lock() {
                        map.insert(cmd.id, cmd.reply);
                    }
                    if ws_write.send(Message::text(payload.to_string())).await.is_err() {
                        if let Ok(mut map) = writer_pending.lock() {
                            if let Some(tx) = map.remove(&cmd.id) {
                                let _ = tx.send(Err("WebSocket send failed".into()));
                            }
                        }
                        break;
                    }
                }
            }
        }
        if let Ok(mut s) = writer_state.lock() {
            s.connected = false;
        }
    });

    tokio::spawn(async move {
        while let Some(item) = ws_read.next().await {
            let text = match item {
                Ok(Message::Text(t)) => t.to_string(),
                Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            let Some(msg) = parse_unbounded_json(&text) else {
                continue;
            };
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                let tx = pending.lock().ok().and_then(|mut map| map.remove(&id));
                if let Some(tx) = tx {
                    if let Some(err) = msg.get("error") {
                        let _ = tx.send(Err(format!("CDP error: {err}")));
                    } else {
                        let _ =
                            tx.send(Ok(msg.get("result").cloned().unwrap_or_else(|| json!({}))));
                    }
                }
            } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let session_id = msg.get("sessionId").and_then(Value::as_str);
                supervisor.handle_event(method, &params, session_id);
            }
        }

        if let Ok(mut state) = supervisor.state.lock() {
            state.connected = false;
            state.pending_dialogs.clear();
        }
        if let Ok(mut map) = pending.lock() {
            for (_, tx) in map.drain() {
                let _ = tx.send(Err("CDP WebSocket disconnected".into()));
            }
        }
        writer_handle.abort();
    });
}

/// Return the live supervisor currently attached for `task_id`, if any.
pub(crate) async fn get(task_id: &str) -> Option<Arc<CdpSupervisor>> {
    let key = if task_id.trim().is_empty() {
        "default"
    } else {
        task_id
    };
    let guard = registry().lock().await;
    guard.get(key).filter(|sup| sup.is_connected()).cloned()
}

/// Return the live supervisor for `task_id`, connecting to `raw_endpoint` if needed.
pub(crate) async fn ensure(
    task_id: &str,
    raw_endpoint: &str,
) -> Result<Arc<CdpSupervisor>, String> {
    let key = if task_id.trim().is_empty() {
        "default"
    } else {
        task_id
    };
    let resolved = resolve_endpoint(raw_endpoint).await;
    if resolved.is_empty() {
        return Err(
            "No CDP endpoint is available. Run '/browser connect' or set 'browser.cdp_url'.".into(),
        );
    }
    if let Some(existing) = {
        let guard = registry().lock().await;
        guard.get(key).cloned()
    } {
        if existing.is_connected() && existing.endpoint() == resolved {
            return Ok(existing);
        }
    }
    let supervisor = CdpSupervisor::connect(&resolved).await?;
    let mut guard = registry().lock().await;
    guard.insert(key.to_owned(), Arc::clone(&supervisor));
    Ok(supervisor)
}

#[allow(dead_code)]
pub(crate) async fn stop(task_id: &str) {
    let key = if task_id.trim().is_empty() {
        "default"
    } else {
        task_id
    };
    let mut guard = registry().lock().await;
    guard.remove(key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn supervisor_tracks_sessions_frames_and_dialogs() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{port}/devtools/browser/test");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(text) = msg else { continue };
                let req: Value = serde_json::from_str(&text).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap();
                let session_id = req.get("sessionId").and_then(Value::as_str);

                match method {
                    "Target.setAutoAttach" => {
                        ws.send(Message::text(json!({"id": id, "result": {}}).to_string()))
                            .await
                            .unwrap();
                    }
                    "Target.getTargets" => {
                        ws.send(Message::text(
                            json!({
                                "id": id,
                                "result": {
                                    "targetInfos": [
                                        {"targetId": "T1", "type": "page", "url": "https://example.com/"}
                                    ]
                                }
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Target.attachToTarget" => {
                        let tid = req["params"]["targetId"].as_str().unwrap();
                        let sid = if tid == "T1" { "S1" } else { "S2" };
                        ws.send(Message::text(
                            json!({"id": id, "result": {"sessionId": sid}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Page.enable"
                    | "Runtime.enable"
                    | "Fetch.enable"
                    | "Page.addScriptToEvaluateOnNewDocument" => {
                        ws.send(Message::text(json!({"id": id, "result": {}}).to_string()))
                            .await
                            .unwrap();
                    }
                    "Page.getFrameTree" => {
                        ws.send(Message::text(
                            json!({
                                "id": id,
                                "result": {
                                    "frameTree": {
                                        "frame": {
                                            "id": "F-root",
                                            "url": "https://example.com/",
                                            "securityOrigin": "https://example.com"
                                        },
                                        "childFrames": [
                                            {
                                                "frame": {
                                                    "id": "F-child",
                                                    "parentId": "F-root",
                                                    "url": "https://sub.example.com/embed",
                                                    "securityOrigin": "https://sub.example.com"
                                                }
                                            }
                                        ]
                                    }
                                }
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                        // Emit a dialog opening right after frame tree query for testing.
                        ws.send(Message::text(
                            json!({
                                "method": "Page.javascriptDialogOpening",
                                "sessionId": "S1",
                                "params": {
                                    "url": "https://example.com/",
                                    "message": "Continue?",
                                    "type": "confirm",
                                    "hasBrowserHandler": true,
                                    "defaultPrompt": ""
                                }
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Page.createIsolatedWorld" => {
                        ws.send(Message::text(
                            json!({"id": id, "result": {"executionContextId": 42}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Runtime.evaluate" => {
                        let ctx = req["params"].get("contextId").and_then(Value::as_i64);
                        assert_eq!(session_id, Some("S1"));
                        ws.send(Message::text(
                            json!({"id": id, "result": {"result": {"type": "number", "value": ctx.unwrap_or(1)}}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Page.handleJavaScriptDialog" => {
                        assert_eq!(req["params"]["accept"], json!(true));
                        ws.send(Message::text(json!({"id": id, "result": {}}).to_string()))
                            .await
                            .unwrap();
                    }
                    _ => {
                        ws.send(Message::text(json!({"id": id, "result": {}}).to_string()))
                            .await
                            .unwrap();
                    }
                }
            }
        });

        let task_id = "sup-unit-test";
        let sup = ensure(task_id, &ws_url).await.unwrap();
        let frames = sup.list_frame_tree().await.unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames
            .iter()
            .any(|f| f.frame_id == "F-child" && f.cross_origin));

        let eval = sup
            .call_in_frame(
                "F-child",
                "Runtime.evaluate",
                &json!({"expression": "1+1"}),
                5.0,
            )
            .await
            .unwrap();
        assert_eq!(eval["result"]["value"], json!(42));

        let (pending, _) = sup.list_dialogs();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message, "Continue?");

        let handled = sup
            .respond_to_dialog("accept", None, Some(&pending[0].dialog_id), 5.0)
            .await
            .unwrap();
        assert_eq!(handled.action, "accepted");
        let (pending_after, history_after) = sup.list_dialogs();
        assert!(pending_after.is_empty());
        assert_eq!(history_after.len(), 1);

        stop(task_id).await;
        server.abort();
    }
}
