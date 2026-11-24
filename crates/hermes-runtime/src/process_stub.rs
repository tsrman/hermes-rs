//! `process_manage` manages background processes through Unix process groups, so it exists only on
//! Unix. This is the stand-in other platforms compile: the tool is never offered (it is offered with
//! the terminal, which is unsupported there too) and a call reports that.

use serde_json::{json, Value};

pub(crate) const PROCESS_MANAGE: &str = "process_manage";

#[allow(dead_code)]
#[derive(Clone, Default)]
pub(crate) struct SpawnOptions {
    pub pty: bool,
    pub notify_on_complete: bool,
    pub watch_patterns: Vec<String>,
}

pub(crate) fn schema() -> Value {
    Value::Null
}

pub fn kill_all() {}

pub fn drain_notifications(_owner: &str) -> Vec<String> {
    Vec::new()
}

pub(crate) async fn call(_caller: &str, _arguments: &str) -> Value {
    json!({"error": "process_manage is not available on this platform"})
}
