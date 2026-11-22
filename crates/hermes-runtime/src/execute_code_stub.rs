//! `execute_code` keeps a Python kernel alive over a Unix socket, so it exists only on Unix. This is
//! the stand-in other platforms compile: the tool is never offered (it needs the terminal, which is
//! unsupported there too) and a call reports that.

use serde_json::{json, Value};

use crate::tools::{BoxFuture, ToolContext, ToolEnv};

pub(crate) const EXECUTE_CODE: &str = "execute_code";

pub(crate) fn sandbox_tools(_env: &ToolEnv) -> Vec<&'static str> {
    Vec::new()
}

pub(crate) fn schema(_tools: &[&str]) -> Value {
    Value::Null
}

pub fn kill_all_kernels() {}

pub(crate) async fn end_session(_session: &str) {}

pub(crate) fn call<'a>(
    _cx: &'a mut ToolContext<'_, '_>,
    _arguments: &'a str,
) -> BoxFuture<'a, Value> {
    Box::pin(async { json!({"error": "execute_code is not available on this platform"}) })
}
