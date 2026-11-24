//! Live browser discovery, optional Chromium launch, and CDP supervisor attachment for
//! `/browser connect`, `/browser status`, and `/browser disconnect`.

use std::collections::HashMap;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::browser_cdp::resolve_endpoint;
use crate::browser_supervisor;

const DEFAULT_DEBUG_PORTS: &[u16] = &[9222, 9229, 9333];

#[derive(Clone, Debug, Default)]
pub struct BrowserConnectOptions {
    /// Explicit endpoint URL (`ws://...`, `http://...`) or numeric loopback port (`"9222"`).
    pub target: Option<String>,
    /// Launch a local Chromium-family browser if requested.
    pub launch: bool,
    /// When launching, pass `--headless=new`.
    pub headless: bool,
    /// Optional explicit debug port when launching.
    pub port: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserConnectionInfo {
    pub ws_url: String,
    pub browser_version: Option<String>,
    pub page_count: usize,
    pub launched: bool,
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserTabSummary {
    pub target_id: String,
    pub title: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserStatusInfo {
    pub configured_url: Option<String>,
    pub connected: bool,
    pub ws_url: Option<String>,
    pub page_tabs: Vec<BrowserTabSummary>,
    pub pending_dialogs: usize,
    pub spawned_pid: Option<u32>,
}

static SPAWNED_BROWSERS: OnceLock<Mutex<HashMap<String, Child>>> = OnceLock::new();

fn spawned_registry() -> &'static Mutex<HashMap<String, Child>> {
    SPAWNED_BROWSERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn scope_key(task_id: &str) -> String {
    let trimmed = task_id.trim();
    if trimmed.is_empty() {
        "default".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Terminate every Chromium process spawned via `/browser connect --launch`.
pub fn kill_spawned_browsers() {
    if let Ok(mut map) = spawned_registry().lock() {
        for (_, mut child) in map.drain() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn kill_spawned_for(task_id: &str) -> bool {
    let key = scope_key(task_id);
    if let Ok(mut map) = spawned_registry().lock() {
        if let Some(mut child) = map.remove(&key) {
            let _ = child.kill();
            let _ = child.wait();
            return true;
        }
    }
    false
}

fn spawned_pid_for(task_id: &str) -> Option<u32> {
    let key = scope_key(task_id);
    let mut map = spawned_registry().lock().ok()?;
    let child = map.get_mut(&key)?;
    if matches!(child.try_wait(), Ok(Some(_))) {
        map.remove(&key);
        return None;
    }
    Some(child.id())
}

/// Normalize a user-supplied connect target (`"9222"`, `"localhost:9222"`, `"http://..."`, `"ws://..."`).
pub fn normalize_connect_target(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return format!("http://127.0.0.1:{trimmed}");
    }
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("ws://")
        || lowered.starts_with("wss://")
        || lowered.starts_with("http://")
        || lowered.starts_with("https://")
    {
        return trimmed.to_owned();
    }
    format!("http://{trimmed}")
}

/// Query `/json/version` on an HTTP DevTools root with a short timeout and no proxy on loopback.
pub async fn probe_cdp_version(
    http_base: &str,
    timeout_ms: u64,
) -> Option<(String, Option<String>)> {
    let cleaned = http_base.trim().trim_end_matches('/');
    let version_url = if cleaned.to_ascii_lowercase().ends_with("/json/version") {
        cleaned.to_owned()
    } else {
        format!("{cleaned}/json/version")
    };
    let mut builder = reqwest::Client::builder().timeout(Duration::from_millis(timeout_ms));
    if reqwest::Url::parse(&version_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase))
        .is_some_and(|h| matches!(h.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
    {
        builder = builder.no_proxy();
    }
    let client = builder.build().ok()?;
    let resp = client.get(&version_url).send().await.ok()?;
    let payload: Value = resp.error_for_status().ok()?.json().await.ok()?;
    let ws = payload
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_owned();
    let browser = payload
        .get("Browser")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some((ws, browser))
}

/// Scan standard loopback DevTools ports (`9222`, `9229`, `9333`) for a running Chromium browser.
pub async fn discover_local_cdp() -> Option<(String, Option<String>)> {
    for &port in DEFAULT_DEBUG_PORTS {
        let base = format!("http://127.0.0.1:{port}");
        if let Some(found) = probe_cdp_version(&base, 600).await {
            return Some(found);
        }
    }
    None
}

/// Locate an installed Chromium-family executable on the host OS.
pub fn find_chromium_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("HERMES_BROWSER_BIN") {
        let p = PathBuf::from(explicit.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    #[cfg(target_os = "macos")]
    let candidates: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "google-chrome",
        "chromium",
    ];
    #[cfg(target_os = "windows")]
    let candidates: &[&str] = &[
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
        "chrome.exe",
        "msedge.exe",
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[&str] = &[
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "brave-browser",
        "microsoft-edge",
        "microsoft-edge-stable",
    ];

    let path_var = std::env::var_os("PATH");
    for &cand in candidates {
        let direct = PathBuf::from(cand);
        if direct.is_absolute() && direct.is_file() {
            return Some(direct);
        }
        if let Some(ref paths) = path_var {
            for dir in std::env::split_paths(paths) {
                let full = dir.join(cand);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
    }
    None
}

fn pick_free_port() -> Result<u16, String> {
    let sock = StdTcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Cannot allocate loopback debug port: {e}"))?;
    sock.local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("Cannot read allocated port: {e}"))
}

async fn launch_local_browser(
    task_id: &str,
    home: Option<&Path>,
    headless: bool,
    port_override: Option<u16>,
) -> Result<(String, Option<String>, u32), String> {
    let binary = find_chromium_binary().ok_or_else(|| {
        "No Chromium-family browser (Chrome, Chromium, Brave, Edge) found in PATH. Start your browser with `--remote-debugging-port=9222` and run `/browser connect 9222`."
            .to_owned()
    })?;
    let port = match port_override {
        Some(p) if p > 0 => p,
        _ => pick_free_port()?,
    };
    let user_data_dir = home
        .map(|h| h.join("browser_profile"))
        .unwrap_or_else(|| std::env::temp_dir().join("hsr-browser-profile"));
    std::fs::create_dir_all(&user_data_dir)
        .map_err(|e| format!("Cannot create browser profile directory: {e}"))?;

    // Stop any prior browser we launched for this task before spawning a new one.
    kill_spawned_for(task_id);

    let mut cmd = Command::new(&binary);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if headless {
        cmd.arg("--headless=new");
    }
    cmd.arg("about:blank");

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to launch {}: {e}", binary.display()))?;
    let pid = child.id();

    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let found = loop {
        if let Some(info) = probe_cdp_version(&base, 500).await {
            break Some(info);
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            break None;
        }
        if Instant::now() >= deadline {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    };

    let Some((ws_url, browser_version)) = found else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "Launched {} (pid {pid}) on port {port}, but DevTools did not respond at {base}/json/version within 10s.",
            binary.display()
        ));
    };

    if let Ok(mut map) = spawned_registry().lock() {
        map.insert(scope_key(task_id), child);
    }
    Ok((ws_url, browser_version, pid))
}

/// Discover, optionally launch, and attach the persistent CDP supervisor for `task_id`.
pub async fn connect_browser(
    task_id: &str,
    home: Option<&Path>,
    configured_url: Option<&str>,
    opts: BrowserConnectOptions,
) -> Result<BrowserConnectionInfo, String> {
    let mut launched = false;
    let mut pid = None;
    let mut browser_version = None;

    let ws_url = if opts.launch {
        let (ws, ver, child_pid) =
            launch_local_browser(task_id, home, opts.headless, opts.port).await?;
        launched = true;
        pid = Some(child_pid);
        browser_version = ver;
        ws
    } else if let Some(raw) = opts
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let normalized = normalize_connect_target(raw);
        let lowered = normalized.to_ascii_lowercase();
        if lowered.starts_with("http://") || lowered.starts_with("https://") {
            let (ws, ver) = probe_cdp_version(&normalized, 3000).await.ok_or_else(|| {
                format!("Could not reach DevTools endpoint at {normalized}/json/version.")
            })?;
            browser_version = ver;
            ws
        } else {
            resolve_endpoint(&normalized).await
        }
    } else if let Some(existing) = std::env::var("BROWSER_CDP_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            configured_url
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
    {
        let normalized = normalize_connect_target(&existing);
        let lowered = normalized.to_ascii_lowercase();
        if lowered.starts_with("http://") || lowered.starts_with("https://") {
            if let Some((ws, ver)) = probe_cdp_version(&normalized, 3000).await {
                browser_version = ver;
                ws
            } else {
                resolve_endpoint(&normalized).await
            }
        } else {
            resolve_endpoint(&normalized).await
        }
    } else if let Some((ws, ver)) = discover_local_cdp().await {
        browser_version = ver;
        ws
    } else {
        return Err(
            "No running Chromium DevTools endpoint found on ports 9222, 9229, or 9333. Start Chrome/Brave/Edge with `--remote-debugging-port=9222`, pass `/browser connect <url|port>`, or use `/browser connect --launch`."
                .into(),
        );
    };

    if !ws_url.starts_with("ws://") && !ws_url.starts_with("wss://") {
        return Err(format!(
            "Resolved CDP endpoint is not a WebSocket URL: {ws_url}"
        ));
    }

    let supervisor = browser_supervisor::ensure(task_id, &ws_url).await?;
    let page_count = match supervisor
        .call_cdp("Target.getTargets", json!({}), None, None, 5.0)
        .await
    {
        Ok(val) => val
            .get("targetInfos")
            .and_then(Value::as_array)
            .map(|infos| {
                infos
                    .iter()
                    .filter(|info| info.get("type").and_then(Value::as_str) == Some("page"))
                    .count()
            })
            .unwrap_or(0),
        Err(_) => 0,
    };

    Ok(BrowserConnectionInfo {
        ws_url,
        browser_version,
        page_count,
        launched,
        pid,
    })
}

/// Disconnect the live CDP supervisor for `task_id` and stop any browser process launched for it.
pub async fn disconnect_browser(task_id: &str) -> bool {
    let was_attached = browser_supervisor::get(task_id).await.is_some();
    browser_supervisor::stop(task_id).await;
    let killed = kill_spawned_for(task_id);
    was_attached || killed
}

/// Inspect the current browser connection, open page tabs, and pending dialogs for `task_id`.
pub async fn browser_status(task_id: &str, configured_url: Option<&str>) -> BrowserStatusInfo {
    let spawned_pid = spawned_pid_for(task_id);
    let Some(sup) = browser_supervisor::get(task_id).await else {
        return BrowserStatusInfo {
            configured_url: configured_url.map(str::to_owned),
            connected: false,
            ws_url: None,
            page_tabs: Vec::new(),
            pending_dialogs: 0,
            spawned_pid,
        };
    };

    let (pending, _) = sup.list_dialogs();
    let page_tabs = match sup
        .call_cdp("Target.getTargets", json!({}), None, None, 3.0)
        .await
    {
        Ok(val) => val
            .get("targetInfos")
            .and_then(Value::as_array)
            .map(|infos| {
                infos
                    .iter()
                    .filter(|info| info.get("type").and_then(Value::as_str) == Some("page"))
                    .map(|info| BrowserTabSummary {
                        target_id: info
                            .get("targetId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        title: info
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        url: info
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    BrowserStatusInfo {
        configured_url: configured_url.map(str::to_owned),
        connected: true,
        ws_url: Some(sup.endpoint().to_owned()),
        page_tabs,
        pending_dialogs: pending.len(),
        spawned_pid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn normalizes_ports_hosts_and_urls() {
        assert_eq!(normalize_connect_target("9222"), "http://127.0.0.1:9222");
        assert_eq!(
            normalize_connect_target("localhost:9333"),
            "http://localhost:9333"
        );
        assert_eq!(
            normalize_connect_target("ws://127.0.0.1:9222/devtools/browser/abc"),
            "ws://127.0.0.1:9222/devtools/browser/abc"
        );
    }

    #[tokio::test]
    async fn connect_status_and_disconnect_work_over_http_discovery_and_ws() {
        // 1. Start mock CDP WebSocket server
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_port = ws_listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{ws_port}/devtools/browser/live-test");

        let ws_server = tokio::spawn(async move {
            let (stream, _) = ws_listener.accept().await.unwrap();
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
                                        {
                                            "targetId": "T1",
                                            "type": "page",
                                            "title": "Example Home",
                                            "url": "https://example.com/"
                                        }
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
                    _ => {
                        ws.send(Message::text(json!({"id": id, "result": {}}).to_string()))
                            .await
                            .unwrap();
                    }
                }
            }
        });

        // 2. Start mock HTTP `/json/version` discovery server
        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        let ws_for_http = ws_url.clone();
        let http_server = tokio::spawn(async move {
            let (mut stream, _) = http_listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await;
            let body = json!({
                "Browser": "Chrome/131.0.6778.0",
                "Protocol-Version": "1.3",
                "webSocketDebuggerUrl": ws_for_http
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });

        let task = "browser-connect-unit-test";
        let info = connect_browser(
            task,
            None,
            None,
            BrowserConnectOptions {
                target: Some(http_port.to_string()),
                ..BrowserConnectOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(info.ws_url, ws_url);
        assert_eq!(info.browser_version.as_deref(), Some("Chrome/131.0.6778.0"));
        assert_eq!(info.page_count, 1);
        assert!(!info.launched);

        let st = browser_status(task, Some(&info.ws_url)).await;
        assert!(st.connected);
        assert_eq!(st.page_tabs.len(), 1);
        assert_eq!(st.page_tabs[0].title, "Example Home");

        assert!(disconnect_browser(task).await);
        let st_after = browser_status(task, None).await;
        assert!(!st_after.connected);

        http_server.abort();
        ws_server.abort();
    }
}
