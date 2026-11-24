//! `browser_exec`: run Python automation scripts against the live CDP browser via an ephemeral
//! authenticated local HTTP->CDP bridge.
//!
//! Mirrors `tools/browser_use_cli.py`. URLs in script literals and every dynamic CDP call sent
//! through the bridge pass the private-URL and cloud-metadata policy guard.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use crate::browser_cdp::{private_guard, redact_method_result, resolve_endpoint, BrowserSettings};
use crate::browser_supervisor::{self, CdpSupervisor};
use crate::browser_url_policy::{literal_targets_metadata, url_blocked, url_literals};
use crate::providers::oauth::base64_encode;
use crate::redact::redact_sensitive_text;

pub(crate) const BROWSER_EXEC: &str = "browser_exec";
const DEFAULT_TIMEOUT_SECS: f64 = 300.0;
const MIN_TIMEOUT_SECS: f64 = 5.0;
const MAX_TIMEOUT_SECS: f64 = 1800.0;
const STDERR_CAP_CHARS: usize = 4000;
const STDOUT_CAP_CHARS: usize = 32_000;
const MAX_BRIDGE_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_SCREENSHOT_ATTACH_BYTES: usize = 5 * 1024 * 1024;

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/browser_exec.json"))
            .expect("valid browser_exec schema JSON"),
    })
}

fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn read_timeout(raw: Option<&Value>) -> f64 {
    let parsed = match raw {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    parsed
        .filter(|v| v.is_finite() && *v > 0.0)
        .map_or(DEFAULT_TIMEOUT_SECS, |v| {
            v.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)
        })
}

fn clip_tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(count - max_chars).collect();
    format!("... [truncated {} chars]\n{tail}", count - max_chars)
}

fn clip_head(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}\n... [truncated {} chars]", count - max_chars)
}

fn random_hex_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Failed to generate random bridge token".to_owned())?;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

fn detect_python_bin() -> &'static str {
    for candidate in ["python3", "python"] {
        if std::process::Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return candidate;
        }
    }
    "python3"
}

fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

const PYTHON_RUNNER: &str = r#"import base64
import json
import os
import sys
import time
import urllib.error
import urllib.request

_BRIDGE_URL = os.environ["HERMES_CDP_BRIDGE_URL"].rstrip("/")
_BRIDGE_TOKEN = os.environ["HERMES_CDP_BRIDGE_TOKEN"]
_ARTIFACT_DIR = os.environ["HERMES_ARTIFACT_DIR"]
_SHOT_META_PATH = os.path.join(_ARTIFACT_DIR, ".screenshots.json")
_SHOT_COUNTER = 0


def _bridge_request(path, payload=None):
    data = None if payload is None else json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        _BRIDGE_URL + path,
        data=data,
        method="GET" if payload is None else "POST",
        headers={
            "Authorization": "Bearer " + _BRIDGE_TOKEN,
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=180) as resp:
            raw = resp.read().decode("utf-8")
            parsed = json.loads(raw) if raw else {}
    except urllib.error.HTTPError as err:
        raw = err.read().decode("utf-8", errors="replace")
        try:
            detail = json.loads(raw).get("error", raw)
        except Exception:
            detail = raw
        raise RuntimeError(detail or f"Bridge HTTP {err.code}")
    if isinstance(parsed, dict) and parsed.get("error"):
        raise RuntimeError(str(parsed["error"]))
    return parsed


def _active_target():
    res = _bridge_request("/active_target")
    return res.get("target_id")


def _set_active_target(target_id):
    _bridge_request("/active_target", {"target_id": target_id})
    return target_id


def cdp(method, params=None, target_id=None, timeout=30.0):
    tid = target_id
    if tid is None and not method.startswith(("Target.", "Browser.")):
        tid = _active_target()
    res = _bridge_request(
        "/cdp",
        {
            "method": method,
            "params": params or {},
            "target_id": tid,
            "timeout": timeout,
        },
    )
    return res.get("result", {})


def list_tabs():
    res = cdp("Target.getTargets", {})
    return [
        t for t in res.get("targetInfos", [])
        if t.get("type") == "page"
    ]


def ensure_real_tab():
    active = _active_target()
    tabs = list_tabs()
    if active and any(t.get("targetId") == active for t in tabs):
        return active
    for t in tabs:
        url = (t.get("url") or "")
        if not url.startswith(("chrome://", "devtools://", "edge://", "brave://")):
            return _set_active_target(t["targetId"])
    if tabs:
        return _set_active_target(tabs[0]["targetId"])
    return new_tab("about:blank")


def new_tab(url="about:blank"):
    res = cdp("Target.createTarget", {"url": url})
    tid = res.get("targetId")
    if tid:
        _set_active_target(tid)
    return tid


def switch_tab(target_id):
    tabs = list_tabs()
    if not any(t.get("targetId") == target_id for t in tabs):
        raise RuntimeError(f"Unknown target_id: {target_id}")
    cdp("Target.activateTarget", {"targetId": target_id})
    return _set_active_target(target_id)


def close_tab(target_id=None):
    tid = target_id or _active_target()
    if not tid:
        return False
    res = cdp("Target.closeTarget", {"targetId": tid})
    return bool(res.get("success", True))


def list_frames():
    return _bridge_request("/frame_tree").get("frames", [])


def wait_for_load(timeout=15.0):
    deadline = time.time() + float(timeout)
    while time.time() < deadline:
        try:
            state = js("document.readyState", await_promise=False)
            if state in ("complete", "interactive"):
                return state
        except Exception:
            pass
        time.sleep(0.15)
    return "timeout"


def goto_url(url, wait=True, timeout=30.0):
    tid = ensure_real_tab()
    res = cdp("Page.navigate", {"url": url}, target_id=tid, timeout=timeout)
    if wait:
        wait_for_load(timeout=min(float(timeout), 15.0))
    return res


def js(expression, await_promise=True, timeout=30.0):
    tid = ensure_real_tab()
    res = cdp(
        "Runtime.evaluate",
        {
            "expression": expression,
            "returnByValue": True,
            "awaitPromise": bool(await_promise),
        },
        target_id=tid,
        timeout=timeout,
    )
    if "exceptionDetails" in res:
        exc = res["exceptionDetails"]
        detail = (exc.get("exception") or {}).get("description") or exc.get("text") or "JS exception"
        raise RuntimeError(detail)
    return (res.get("result") or {}).get("value")


def page_info():
    return js("({url: location.href, title: document.title, readyState: document.readyState})", await_promise=False)


def fill_input(selector, value):
    js(
        f"""(() => {{
            const el = document.querySelector({json.dumps(selector)});
            if (!el) throw new Error("Element not found: " + {json.dumps(selector)});
            el.focus();
            el.value = {json.dumps(value)};
            el.dispatchEvent(new Event("input", {{bubbles: true}}));
            el.dispatchEvent(new Event("change", {{bubbles: true}}));
            return true;
        }})()""",
        await_promise=False,
    )


def click_at_xy(x, y):
    tid = ensure_real_tab()
    cdp("Input.dispatchMouseEvent", {"type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1}, target_id=tid)
    cdp("Input.dispatchMouseEvent", {"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1}, target_id=tid)


def capture_screenshot(path=None, full_page=False):
    global _SHOT_COUNTER
    tid = ensure_real_tab()
    params = {"format": "png"}
    if full_page:
        params["captureBeyondViewport"] = True
    res = cdp("Page.captureScreenshot", params, target_id=tid)
    data = base64.b64decode(res.get("data", ""))
    if path is None:
        _SHOT_COUNTER += 1
        out_path = os.path.join(_ARTIFACT_DIR, f"screenshot_{_SHOT_COUNTER}.png")
    else:
        out_path = os.path.abspath(path)
    parent = os.path.dirname(out_path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(out_path, "wb") as f:
        f.write(data)
    recorded = []
    if os.path.exists(_SHOT_META_PATH):
        try:
            with open(_SHOT_META_PATH, "r", encoding="utf-8") as mf:
                recorded = json.load(mf)
        except Exception:
            recorded = []
    if out_path not in recorded:
        recorded.append(out_path)
    with open(_SHOT_META_PATH, "w", encoding="utf-8") as mf:
        json.dump(recorded, mf)
    return out_path


def main():
    user_path = sys.argv[1]
    with open(user_path, "r", encoding="utf-8") as f:
        user_code = f.read()
    env = {
        "__name__": "__main__",
        "cdp": cdp,
        "list_tabs": list_tabs,
        "ensure_real_tab": ensure_real_tab,
        "new_tab": new_tab,
        "switch_tab": switch_tab,
        "close_tab": close_tab,
        "list_frames": list_frames,
        "wait_for_load": wait_for_load,
        "goto_url": goto_url,
        "js": js,
        "page_info": page_info,
        "fill_input": fill_input,
        "click_at_xy": click_at_xy,
        "capture_screenshot": capture_screenshot,
    }
    exec(compile(user_code, "<browser_exec>", "exec"), env, env)


if __name__ == "__main__":
    main()
"#;

async fn handle_bridge_connection(
    mut stream: TcpStream,
    token: String,
    supervisor: Arc<CdpSupervisor>,
    allow_private_urls: bool,
) {
    let Ok((method, path, body)) = read_http_request(&mut stream, &token).await else {
        return;
    };
    let (status, payload) = match (method.as_str(), path.as_str()) {
        ("GET", "/active_target") => {
            let tid = supervisor.active_target_id().await;
            (200, json!({"target_id": tid}))
        }
        ("POST", "/active_target") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Some(tid) = req.get("target_id").and_then(Value::as_str) {
                supervisor.set_active_target(tid);
                (200, json!({"ok": true, "target_id": tid}))
            } else {
                (400, json!({"error": "target_id is required"}))
            }
        }
        ("GET", "/frame_tree") => match supervisor.list_frame_tree().await {
            Ok(frames) => (200, json!({"frames": frames})),
            Err(err) => (500, json!({"error": redact_sensitive_text(&err)})),
        },
        ("POST", "/cdp") => {
            let req: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(err) => {
                    let _ =
                        write_http_response(&mut stream, 400, &json!({"error": err.to_string()}))
                            .await;
                    return;
                }
            };
            let cdp_method = req
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if cdp_method.is_empty() {
                let _ =
                    write_http_response(&mut stream, 400, &json!({"error": "CDP method required"}))
                        .await;
                return;
            }
            let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
            let target_id = req
                .get("target_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let timeout = req
                .get("timeout")
                .and_then(Value::as_f64)
                .unwrap_or(30.0)
                .clamp(1.0, 300.0);

            if let Some(refusal) = private_guard(
                cdp_method,
                &params,
                supervisor.endpoint(),
                target_id,
                allow_private_urls,
            )
            .await
            {
                let msg = refusal
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Blocked by browser URL policy");
                let _ = write_http_response(&mut stream, 403, &json!({"error": msg})).await;
                return;
            }

            match supervisor
                .call_cdp(cdp_method, params, target_id, None, timeout)
                .await
            {
                Ok(result) => (
                    200,
                    json!({"result": redact_method_result(cdp_method, &result)}),
                ),
                Err(err) => (502, json!({"error": redact_sensitive_text(&err)})),
            }
        }
        _ => (404, json!({"error": "Not found"})),
    };
    let _ = write_http_response(&mut stream, status, &payload).await;
}

async fn read_http_request(
    stream: &mut TcpStream,
    expected_token: &str,
) -> Result<(String, String, Vec<u8>), ()> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            let _ = write_http_response(stream, 431, &json!({"error": "Headers too large"})).await;
            return Err(());
        }
    }
    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines.next().ok_or(())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(())?.to_owned();
    let path = parts.next().ok_or(())?.to_owned();

    let mut content_length: usize = 0;
    let mut authorized = false;
    let expected_header = format!("Bearer {expected_token}");
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            } else if key == "authorization" && val == expected_header {
                authorized = true;
            }
        }
    }
    if !authorized {
        let _ = write_http_response(stream, 401, &json!({"error": "Unauthorized"})).await;
        return Err(());
    }
    if content_length > MAX_BRIDGE_BODY_BYTES {
        let _ = write_http_response(stream, 413, &json!({"error": "Body too large"})).await;
        return Err(());
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok((method, path, body))
}

async fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    payload: &Value,
) -> std::io::Result<()> {
    let body = payload.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

async fn start_ephemeral_bridge(
    supervisor: Arc<CdpSupervisor>,
    token: String,
    allow_private_urls: bool,
) -> Result<(String, oneshot::Sender<()>), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Failed to bind local CDP bridge: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Bridge local_addr failed: {e}"))?
        .port();
    let url = format!("http://127.0.0.1:{port}");
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let tok = token.clone();
                    let sup = Arc::clone(&supervisor);
                    tokio::spawn(async move {
                        handle_bridge_connection(stream, tok, sup, allow_private_urls).await;
                    });
                }
            }
        }
    });

    Ok((url, stop_tx))
}

fn collect_screenshots(
    artifact_dir: &Path,
    stdout: &str,
    vision_native: bool,
) -> (Vec<String>, Vec<Value>) {
    let mut paths: Vec<PathBuf> = Vec::new();
    let meta = artifact_dir.join(".screenshots.json");
    if let Ok(raw) = std::fs::read_to_string(&meta) {
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&raw) {
            for p in list {
                paths.push(PathBuf::from(p));
            }
        }
    }
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("MEDIA:") {
            let p = PathBuf::from(rest.trim());
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    let mut seen = HashSet::new();
    let mut recorded_paths = Vec::new();
    let mut multimodal = Vec::new();
    for path in paths {
        if !seen.insert(path.clone()) || !path.is_file() {
            continue;
        }
        let path_str = path.to_string_lossy().into_owned();
        recorded_paths.push(path_str.clone());
        if vision_native {
            if let Ok(bytes) = std::fs::read(&path) {
                if bytes.len() <= MAX_SCREENSHOT_ATTACH_BYTES {
                    if let Some(mime) = detect_image_mime(&bytes) {
                        let data_url = format!("data:{mime};base64,{}", base64_encode(&bytes));
                        multimodal.push(json!({
                            "data_url": data_url,
                            "media_type": mime,
                            "label": format!("Screenshot from browser_exec({path_str})"),
                        }));
                    }
                }
            }
        }
    }
    (recorded_paths, multimodal)
}

pub(crate) async fn call(
    settings: &BrowserSettings,
    task_id: &str,
    vision_native: bool,
    arguments: &str,
) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
    if code.trim().is_empty() {
        return json!({"error": "'code' is required for browser_exec."});
    }
    if let Some(session) = args.get("session").and_then(Value::as_str) {
        if !valid_session_name(session) {
            return json!({
                "error": "Invalid session name: must be 1-64 chars matching [A-Za-z0-9_.-]."
            });
        }
    }
    let raw_url = settings.cdp_url.as_deref().unwrap_or("");
    let endpoint = resolve_endpoint(raw_url).await;
    if endpoint.is_empty() {
        return json!({
            "error": "No CDP endpoint is available. Run '/browser connect' to attach to a running browser, or set 'browser.cdp_url' in config.yaml."
        });
    }

    // Screen literal URLs in code before launching Python.
    for literal in url_literals(code, false) {
        if literal_targets_metadata(&literal).await {
            return json!({
                "error": format!("Blocked: script targets a cloud metadata endpoint ({literal}).")
            });
        }
        if !settings.allow_private_urls && url_blocked(&literal).await {
            return json!({
                "error": format!("Blocked: script targets a private or internal address ({literal}).")
            });
        }
    }

    let timeout_secs = read_timeout(args.get("timeout"));
    let supervisor = match browser_supervisor::ensure(task_id, &endpoint).await {
        Ok(sup) => sup,
        Err(err) => return json!({"error": err}),
    };

    let token = match random_hex_token() {
        Ok(t) => t,
        Err(err) => return json!({"error": err}),
    };
    let (bridge_url, stop_bridge) = match start_ephemeral_bridge(
        supervisor,
        token.clone(),
        settings.allow_private_urls,
    )
    .await
    {
        Ok(pair) => pair,
        Err(err) => return json!({"error": err}),
    };

    let Ok(tempdir) = tempfile::Builder::new()
        .prefix("hermes-browser-exec-")
        .tempdir()
    else {
        let _ = stop_bridge.send(());
        return json!({"error": "Failed to allocate temporary workspace for browser_exec."});
    };
    let runner_path = tempdir.path().join("_runner.py");
    let user_path = tempdir.path().join("script.py");
    let artifact_dir = tempdir.path().join("artifacts");
    if let Err(err) = std::fs::create_dir_all(&artifact_dir) {
        let _ = stop_bridge.send(());
        return json!({"error": format!("Failed to prepare artifact dir: {err}")});
    }
    if std::fs::write(&runner_path, PYTHON_RUNNER).is_err()
        || std::fs::write(&user_path, code).is_err()
    {
        let _ = stop_bridge.send(());
        return json!({"error": "Failed to write script to temporary workspace."});
    }

    let python = detect_python_bin();
    let started = Instant::now();
    let mut cmd = tokio::process::Command::new(python);
    cmd.arg("-I")
        .arg(&runner_path)
        .arg(&user_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("HERMES_CDP_BRIDGE_URL", &bridge_url)
        .env("HERMES_CDP_BRIDGE_TOKEN", &token)
        .env("HERMES_ARTIFACT_DIR", &artifact_dir)
        .env(
            "HERMES_BROWSER_SESSION",
            args.get("session")
                .and_then(Value::as_str)
                .unwrap_or("default"),
        );

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            let _ = stop_bridge.send(());
            return json!({"error": format!("Failed to spawn {python}: {err}")});
        }
    };

    let output = tokio::time::timeout(
        Duration::from_secs_f64(timeout_secs),
        child.wait_with_output(),
    )
    .await;
    let _ = stop_bridge.send(());
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match output {
        Err(_) => json!({
            "success": false,
            "timed_out": true,
            "elapsed_ms": elapsed_ms,
            "error": format!("browser_exec timed out after {timeout_secs}s"),
        }),
        Ok(Err(err)) => json!({
            "success": false,
            "elapsed_ms": elapsed_ms,
            "error": format!("browser_exec process failed: {err}"),
        }),
        Ok(Ok(out)) => {
            let exit_code = out.status.code().unwrap_or(-1);
            let raw_stdout = String::from_utf8_lossy(&out.stdout);
            let raw_stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = clip_head(
                &crate::browser_vault::scrub_registered_secrets(&redact_sensitive_text(
                    &raw_stdout,
                )),
                STDOUT_CAP_CHARS,
            );
            let stderr = clip_tail(
                &crate::browser_vault::scrub_registered_secrets(&redact_sensitive_text(
                    &raw_stderr,
                )),
                STDERR_CAP_CHARS,
            );
            let (screenshots, multimodal) =
                collect_screenshots(&artifact_dir, &stdout, vision_native);

            let mut obj = Map::new();
            obj.insert("success".into(), json!(out.status.success()));
            obj.insert("exit_code".into(), json!(exit_code));
            obj.insert("elapsed_ms".into(), json!(elapsed_ms));
            obj.insert("stdout".into(), json!(stdout));
            if !stderr.is_empty() {
                obj.insert("stderr".into(), json!(stderr));
            }
            if !screenshots.is_empty() {
                obj.insert("screenshots".into(), json!(screenshots));
            }
            if !multimodal.is_empty() {
                obj.insert("_hermes_multimodal_images".into(), Value::Array(multimodal));
            }
            if !out.status.success() {
                obj.insert(
                    "error".into(),
                    json!(if stderr.is_empty() {
                        format!("Script exited with status {exit_code}")
                    } else {
                        stderr.clone()
                    }),
                );
            }
            Value::Object(obj)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn browser_exec_blocks_private_url_literals() {
        let settings = BrowserSettings {
            cdp_url: Some("ws://127.0.0.1:9222/devtools/browser/test".into()),
            allow_private_urls: false,
        };
        let out = call(
            &settings,
            "exec-block-test",
            false,
            r#"{"code": "goto_url('http://127.0.0.1:8080/admin')"}"#,
        )
        .await;
        assert!(out["error"].as_str().unwrap().contains("Blocked:"));
    }

    #[tokio::test]
    async fn browser_exec_runs_python_against_mock_cdp_bridge_and_blocks_dynamic_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{port}/devtools/browser/exec");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(text) = msg else { continue };
                let req: Value = serde_json::from_str(&text).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap();
                match method {
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
                        ws.send(Message::text(
                            json!({"id": id, "result": {"sessionId": "S1"}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Runtime.evaluate" => {
                        ws.send(Message::text(
                            json!({
                                "id": id,
                                "result": {"result": {"type": "string", "value": "Example Title"}}
                            })
                            .to_string(),
                        ))
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

        let settings = BrowserSettings {
            cdp_url: Some(ws_url),
            allow_private_urls: true,
        };
        let out = call(
            &settings,
            "exec-mock-test",
            false,
            r#"{"code": "info = js('document.title')\nprint('TITLE=' + str(info))\ntry:\n    cdp('Page.navigate', {'url': 'http://' + '169.254.169.254/latest'})\nexcept Exception as e:\n    print('META_BLOCKED=' + str(e))"}"#,
        )
        .await;
        assert_eq!(out["success"], json!(true), "out was: {out}");
        let stdout = out["stdout"].as_str().unwrap();
        assert!(stdout.contains("TITLE=Example Title"));
        assert!(stdout.contains("META_BLOCKED=Blocked:"));

        browser_supervisor::stop("exec-mock-test").await;
        server.abort();
    }
}
