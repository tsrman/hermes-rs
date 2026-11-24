//! `browser_dialog`: inspect, accept, or dismiss JS dialogs captured by the CDP supervisor.
//!
//! Mirrors `tools/browser_dialog_tool.py`.

use serde_json::{json, Value};

use crate::browser_supervisor;

pub(crate) const BROWSER_DIALOG: &str = "browser_dialog";

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/browser_dialog.json"))
            .expect("valid browser_dialog schema JSON"),
    })
}

pub(crate) async fn call(task_id: &str, arguments: &str) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("list");
    let dialog_id = args
        .get("dialog_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let prompt_text = args.get("prompt_text").and_then(Value::as_str);

    let Some(supervisor) = browser_supervisor::get(task_id).await else {
        return json!({
            "error": "No browser session is active for this task."
        });
    };

    match action {
        "list" => {
            let (pending, history) = supervisor.list_dialogs();
            json!({
                "pending": pending.into_iter().map(|d| d.redacted_json()).collect::<Vec<_>>(),
                "history": history.into_iter().map(|d| d.redacted_json()).collect::<Vec<_>>(),
            })
        }
        "accept" | "dismiss" => match supervisor
            .respond_to_dialog(action, prompt_text, dialog_id, 10.0)
            .await
        {
            Ok(record) => json!({
                "ok": true,
                "dialog": record.redacted_json(),
            }),
            Err(error) => json!({
                "error": error,
            }),
        },
        other => json!({
            "error": format!("Unsupported browser_dialog action: {other}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn browser_dialog_returns_error_when_unattached() {
        let out = call("nonexistent-dialog-task", "{}").await;
        assert_eq!(
            out["error"],
            json!("No browser session is active for this task.")
        );
    }

    #[tokio::test]
    async fn browser_dialog_lists_and_handles_prompt_dialog() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{port}/devtools/browser/dlg");

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
                            json!({"id": id, "result": {"targetInfos": [{"targetId": "T1", "type": "page", "url": "https://example.com"}]}}).to_string(),
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
                        ws.send(Message::text(
                            json!({
                                "method": "Page.javascriptDialogOpening",
                                "sessionId": "S1",
                                "params": {
                                    "url": "https://example.com/form",
                                    "message": "Enter token sk-ant-api03-secretsecretsecretsecretsecret",
                                    "type": "prompt",
                                    "hasBrowserHandler": true,
                                    "defaultPrompt": "default"
                                }
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Page.handleJavaScriptDialog" => {
                        assert_eq!(req["params"]["accept"], json!(true));
                        assert_eq!(req["params"]["promptText"], json!("override-value"));
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

        let task_id = "dlg-tool-test";
        let _sup = browser_supervisor::ensure(task_id, &ws_url).await.unwrap();
        // Wait briefly for event processing
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let listed = call(task_id, r#"{"action": "list"}"#).await;
        let pending = listed["pending"].as_array().unwrap();
        assert_eq!(pending.len(), 1);
        let msg = pending[0]["message"].as_str().unwrap();
        assert!(!msg.contains("sk-ant-api03-secretsecretsecretsecretsecret"));

        let handled = call(
            task_id,
            r#"{"action": "accept", "prompt_text": "override-value"}"#,
        )
        .await;
        assert_eq!(handled["ok"], json!(true));
        assert_eq!(handled["dialog"]["action"], json!("accepted"));
        assert_eq!(handled["dialog"]["prompt_text"], json!("override-value"));

        browser_supervisor::stop(task_id).await;
        server.abort();
    }
}
