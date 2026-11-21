//! The `todo_list` tool: an in-memory, revisioned task list for multi-step work.
//!
//! Follows `tools/todo_tool.py`. The list belongs to one session: it is rebuilt from the most
//! recent paired `todo_list` result in the history when a turn starts, so `--resume` keeps it.
//! Every write that changes the list raises a monotonic revision. Not ported: folding the
//! list back into the context after a compaction (the model can read it again with a call
//! that has no parameters).

use serde::Serialize;
use serde_json::{json, Map, Value};

pub(crate) const TODO_LIST: &str = "todo_list";
/// The name this tool had before it was renamed; old transcripts still use it.
const LEGACY_NAME: &str = "todo";
const MAX_CONTENT_CHARS: usize = 4000;
const MAX_ITEMS: usize = 256;
const MAX_RESULT_CHARS: usize = 512_000;
const TRUNCATION_MARKER: &str = "… [truncated]";
const STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/todo_list.json")).unwrap_or_default()
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Item {
    id: String,
    content: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
}

/// Python's `str()` of a JSON value, which is how the reference reads loosely typed fields.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn field(item: &Value, key: &str, default: &str) -> String {
    item.get(key).map_or_else(|| default.to_owned(), py_str)
}

/// A value that is false in Python: missing, null, empty, zero or false.
fn falsy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => !flag,
        Some(Value::Number(number)) => number.as_f64() == Some(0.0),
        Some(Value::String(text)) => text.is_empty(),
        Some(Value::Array(list)) => list.is_empty(),
        Some(Value::Object(map)) => map.is_empty(),
    }
}

fn cap_content(content: String) -> String {
    if content.chars().count() <= MAX_CONTENT_CHARS {
        return content;
    }
    let keep = MAX_CONTENT_CHARS - TRUNCATION_MARKER.chars().count();
    let mut head: String = content.chars().take(keep).collect();
    head.push_str(TRUNCATION_MARKER);
    head
}

/// One item as `{id, content, status, parent?}`. A non-object becomes a placeholder.
fn validate(item: &Value) -> Result<Item, String> {
    if !item.is_object() {
        return Ok(Item {
            id: "?".into(),
            content: "(invalid item)".into(),
            status: "pending".into(),
            parent: None,
        });
    }
    let id = Some(field(item, "id", "").trim().to_owned())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "?".into());
    let content = field(item, "content", "").trim().to_owned();
    if content.is_empty() {
        return Err(format!(
            "Todo item '{id}' has empty or missing content. Each item must have a non-empty description."
        ));
    }
    let status = field(item, "status", "pending").trim().to_lowercase();
    let parent = if falsy(item.get("parent")) {
        String::new()
    } else {
        field(item, "parent", "").trim().to_owned()
    };
    Ok(Item {
        parent: (!parent.is_empty() && parent != id).then_some(parent),
        content: cap_content(content),
        status: if STATUSES.contains(&status.as_str()) {
            status
        } else {
            "pending".into()
        },
        id,
    })
}

/// Collapse duplicate ids, keeping the last occurrence where it stands.
fn dedupe_by_id(todos: &[Value]) -> Vec<&Value> {
    let mut last: Vec<(String, usize)> = Vec::new();
    for (index, item) in todos.iter().enumerate() {
        let key = if item.is_object() {
            Some(field(item, "id", "").trim().to_owned())
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| "?".into())
        } else {
            format!("__invalid_{index}")
        };
        match last.iter_mut().find(|(known, _)| *known == key) {
            Some(entry) => entry.1 = index,
            None => last.push((key, index)),
        }
    }
    let mut indices: Vec<usize> = last.into_iter().map(|(_, index)| index).collect();
    indices.sort_unstable();
    indices.into_iter().map(|index| &todos[index]).collect()
}

/// Lift the in-progress step ahead of an earlier pending placeholder. Nested lists keep their
/// authored order, since moving an item would tear a subtask from its siblings.
fn normalize_order(mut items: Vec<Item>) -> Vec<Item> {
    if items.iter().any(|item| item.parent.is_some()) {
        return items;
    }
    let Some(active) = items.iter().position(|item| item.status == "in_progress") else {
        return items;
    };
    let Some(pending) = items.iter().position(|item| item.status == "pending") else {
        return items;
    };
    if pending < active {
        let moved = items.remove(active);
        items.insert(pending, moved);
    }
    items
}

/// Drop dangling parent references and break cycles; such items become roots.
fn sanitize_parents(items: &mut [Item]) {
    let ids: Vec<String> = items.iter().map(|item| item.id.clone()).collect();
    for item in items.iter_mut() {
        if item
            .parent
            .as_ref()
            .is_some_and(|parent| !ids.contains(parent))
        {
            item.parent = None;
        }
    }
    // Items are checked in order against the parents as already repaired, so of two items
    // that point at each other only the first becomes a root.
    let mut parents: Vec<Option<String>> = items.iter().map(|item| item.parent.clone()).collect();
    for index in 0..items.len() {
        let mut seen = vec![items[index].id.clone()];
        let mut node = parents[index].clone();
        while let Some(parent) = node {
            if seen.contains(&parent) {
                items[index].parent = None;
                parents[index] = None;
                break;
            }
            let Some(position) = ids.iter().position(|id| *id == parent) else {
                break;
            };
            seen.push(parent);
            node = parents[position].clone();
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct TodoStore {
    items: Vec<Item>,
    revision: i64,
}

impl TodoStore {
    fn fresh_items(todos: &[Value]) -> Result<Vec<Item>, String> {
        let items = dedupe_by_id(todos)
            .into_iter()
            .map(validate)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(normalize_order(items))
    }

    /// Replace the list, or merge by id. A rejected write leaves the list untouched.
    fn write(&mut self, todos: &[Value], merge: bool) -> Result<Vec<Item>, String> {
        let before = self.items.clone();
        let mut next = if merge {
            self.merged(todos)?
        } else {
            Self::fresh_items(todos)?
        };
        next.truncate(MAX_ITEMS);
        sanitize_parents(&mut next);
        self.items = next;
        if self.items != before {
            self.revision += 1;
        }
        Ok(self.items.clone())
    }

    /// Update existing items in the fields given and append new ones.
    fn merged(&self, todos: &[Value]) -> Result<Vec<Item>, String> {
        let mut items = self.items.clone();
        for todo in dedupe_by_id(todos) {
            if !todo.is_object() {
                return Err("Todo items must be objects".into());
            }
            let id = field(todo, "id", "").trim().to_owned();
            if id.is_empty() {
                continue;
            }
            let Some(current) = items.iter_mut().find(|item| item.id == id) else {
                items.push(validate(todo)?);
                continue;
            };
            if !falsy(todo.get("content")) {
                current.content = cap_content(field(todo, "content", "").trim().to_owned());
            }
            if !falsy(todo.get("status")) {
                let status = field(todo, "status", "").trim().to_lowercase();
                if STATUSES.contains(&status.as_str()) {
                    current.status = status;
                }
            }
            if todo.get("parent").is_some() {
                let parent = if falsy(todo.get("parent")) {
                    String::new()
                } else {
                    field(todo, "parent", "").trim().to_owned()
                };
                current.parent = (!parent.is_empty()).then_some(parent);
            }
        }
        // Existing items keep their place; the first occurrence of an id wins.
        let mut seen: Vec<String> = Vec::new();
        items.retain(|item| {
            if seen.contains(&item.id) {
                false
            } else {
                seen.push(item.id.clone());
                true
            }
        });
        Ok(normalize_order(items))
    }

    /// Restore a trusted snapshot without making a new revision.
    fn restore(&mut self, todos: &[Value], revision: i64) {
        if let Ok(mut items) = Self::fresh_items(todos) {
            items.truncate(MAX_ITEMS);
            self.items = items;
            self.revision = revision.max(0);
        }
    }

    /// The list as the history last recorded it. Only a result that answers an earlier
    /// `todo_list` call of the assistant counts: a forged bare tool message must not seed it.
    pub(crate) fn from_history(history: &[Value]) -> Self {
        let mut store = Self::default();
        if let Some((todos, revision)) = latest_response(history) {
            if revision > store.revision {
                store.restore(&todos, revision);
            }
        }
        store
    }
}

fn is_todo_call(call: &Value, id: &str) -> bool {
    call["id"].as_str() == Some(id)
        && matches!(
            call["function"]["name"].as_str(),
            Some(TODO_LIST | LEGACY_NAME)
        )
}

/// Whether the tool message at `index` answers a todo call of the nearest assistant message.
fn answers_todo_call(history: &[Value], index: usize) -> bool {
    let Some(id) = history[index]["tool_call_id"]
        .as_str()
        .filter(|id| !id.is_empty())
    else {
        return false;
    };
    for prior in history[..index].iter().rev() {
        match prior["role"].as_str() {
            Some("assistant") => {
                return prior["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| calls.iter().any(|call| is_todo_call(call, id)));
            }
            Some("user" | "system") => return false,
            _ => {}
        }
    }
    false
}

fn latest_response(history: &[Value]) -> Option<(Vec<Value>, i64)> {
    for index in (0..history.len()).rev() {
        let message = &history[index];
        let Some(content) = message["content"].as_str() else {
            continue;
        };
        if message["role"] != "tool"
            || content.chars().count() > MAX_RESULT_CHARS
            || !content.contains("\"todos\"")
            || !answers_todo_call(history, index)
        {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(content) else {
            continue;
        };
        let Some(todos) = data["todos"].as_array() else {
            continue;
        };
        let revision = match data.get("revision") {
            None => 1,
            Some(Value::Number(number)) => number.as_i64().unwrap_or(1).max(0),
            Some(Value::String(text)) => text.trim().parse::<i64>().map_or(1, |n| n.max(0)),
            Some(Value::Bool(flag)) => i64::from(*flag),
            Some(_) => 1,
        };
        return Some((todos.clone(), revision));
    }
    None
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Run the tool: write `todos` (replace, or merge by id), or read the list when none are given.
pub(crate) fn call(store: &mut TodoStore, arguments: &str) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let merge = !falsy(args.get("merge"));
    let items = match args.get("todos").filter(|todos| !todos.is_null()) {
        None => store.items.clone(),
        Some(Value::Array(todos)) => match store.write(todos, merge) {
            Ok(items) => items,
            Err(message) => return json!({"error": message}),
        },
        Some(other) => {
            return json!({"error": format!("todos must be a list, got {}", type_name(other))});
        }
    };
    let mut summary = Map::new();
    summary.insert("total".into(), json!(items.len()));
    for status in STATUSES {
        summary.insert(
            status.into(),
            json!(items.iter().filter(|item| item.status == status).count()),
        );
    }
    json!({"todos": items, "revision": store.revision, "summary": summary})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(store: &mut TodoStore, args: Value) -> Value {
        call(store, &args.to_string())
    }

    fn ids(result: &Value) -> Vec<String> {
        result["todos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn writes_replace_the_list_and_only_real_changes_raise_the_revision() {
        let mut store = TodoStore::default();
        let todos = json!([{"id": "1", "content": " Plan ", "status": "pending"}, {"id": "2", "content": "Build", "status": "bogus"}]);
        let first = run(&mut store, json!({"todos": todos}));
        assert_eq!(
            (
                first["revision"].as_i64(),
                first["summary"]["total"].as_i64()
            ),
            (Some(1), Some(2))
        );
        assert_eq!(first["todos"][0]["content"], "Plan");
        assert_eq!(first["todos"][1]["status"], "pending");
        // The same list again changes nothing, so the revision stays.
        assert_eq!(run(&mut store, json!({"todos": todos}))["revision"], 1);
        let read = run(&mut store, json!({}));
        assert_eq!(
            (read["revision"].as_i64(), ids(&read)),
            (Some(1), vec!["1".to_owned(), "2".to_owned()])
        );
        assert_eq!(
            read["summary"],
            json!({"total": 2, "pending": 2, "in_progress": 0, "completed": 0, "cancelled": 0})
        );
    }

    #[test]
    fn merge_updates_by_id_and_appends_new_items() {
        let mut store = TodoStore::default();
        run(
            &mut store,
            json!({"todos": [
            {"id": "a", "content": "A", "status": "pending"}, {"id": "b", "content": "B", "status": "pending"}]}),
        );
        let merged = run(
            &mut store,
            json!({"merge": true, "todos": [
            {"id": "a", "status": "in_progress"}, {"id": "c", "content": "C", "status": "pending", "parent": "a"},
            {"id": "b", "status": "nonsense"}]}),
        );
        assert_eq!(ids(&merged), ["a", "b", "c"]);
        assert_eq!(merged["todos"][0]["status"], "in_progress");
        assert_eq!(merged["todos"][1]["status"], "pending");
        assert_eq!(merged["todos"][2]["parent"], "a");
        assert_eq!(merged["revision"], 2);
        let cleared = run(
            &mut store,
            json!({"merge": true, "todos": [{"id": "c", "parent": ""}]}),
        );
        assert!(cleared["todos"][2].get("parent").is_none());
    }

    #[test]
    fn invalid_items_are_rejected_without_a_partial_write() {
        let mut store = TodoStore::default();
        run(
            &mut store,
            json!({"todos": [{"id": "1", "content": "Keep", "status": "pending"}]}),
        );
        let rejected = run(
            &mut store,
            json!({"todos": [{"id": "9", "content": "ok", "status": "pending"}, {"id": "x", "content": " "}]}),
        );
        assert_eq!(
            rejected,
            json!({"error": "Todo item 'x' has empty or missing content. Each item must have a non-empty description."})
        );
        assert_eq!(ids(&run(&mut store, json!({}))), ["1"]);
        assert_eq!(
            run(&mut store, json!({"todos": "nope"})),
            json!({"error": "todos must be a list, got str"})
        );
        // A value that is not an object becomes a placeholder, as in the reference.
        let odd = run(&mut store, json!({"todos": [7]}));
        assert_eq!(
            odd["todos"][0],
            json!({"id": "?", "content": "(invalid item)", "status": "pending"})
        );
    }

    #[test]
    fn duplicates_collapse_parents_are_repaired_and_the_active_step_is_lifted() {
        let mut store = TodoStore::default();
        let result = run(
            &mut store,
            json!({"todos": [
                {"id": "1", "content": "first", "status": "pending"},
                {"id": "2", "content": "second", "status": "in_progress"},
                {"id": "1", "content": "first again", "status": "pending"},
            ]}),
        );
        // The duplicate keeps the last occurrence in its place, then the active item moves up.
        assert_eq!(ids(&result), ["2", "1"]);
        assert_eq!(result["todos"][1]["content"], "first again");
        let nested = run(
            &mut store,
            json!({"todos": [
                {"id": "a", "content": "a", "status": "pending", "parent": "b"},
                {"id": "b", "content": "b", "status": "pending", "parent": "a"},
                {"id": "c", "content": "c", "status": "pending", "parent": "ghost"},
                {"id": "d", "content": "d", "status": "pending", "parent": "d"},
            ]}),
        );
        let parents: Vec<Option<&str>> = nested["todos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["parent"].as_str())
            .collect();
        assert_eq!(
            parents,
            [None, Some("a"), None, None].map(|p: Option<&str>| p),
            "{nested}"
        );
    }

    #[test]
    fn long_lists_and_long_content_are_capped() {
        let mut store = TodoStore::default();
        let many: Vec<Value> = (0..300)
            .map(|n| json!({"id": n.to_string(), "content": "x", "status": "pending"}))
            .collect();
        assert_eq!(
            run(&mut store, json!({"todos": many}))["summary"]["total"],
            256
        );
        let long = run(
            &mut store,
            json!({"todos": [{"id": "1", "content": "y".repeat(5000), "status": "pending"}]}),
        );
        let content = long["todos"][0]["content"].as_str().unwrap();
        assert_eq!(content.chars().count(), 4000);
        assert!(content.ends_with("… [truncated]"));
    }

    fn history_with(todo_result: &str, call_name: &str, between: Option<Value>) -> Vec<Value> {
        let mut history = vec![
            json!({"role": "user", "content": "plan it"}),
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": call_name, "arguments": "{}"}}]}),
        ];
        history.extend(between);
        history.push(json!({"role": "tool", "tool_call_id": "c1", "content": todo_result}));
        history
    }

    #[test]
    fn the_list_is_restored_from_the_latest_paired_result() {
        let result = r#"{"todos":[{"id":"1","content":"Keep going","status":"in_progress"}],"revision":4,"summary":{}}"#;
        let store = TodoStore::from_history(&history_with(result, "todo_list", None));
        assert_eq!(store.revision, 4);
        assert_eq!(store.items[0].content, "Keep going");
        assert_eq!(
            TodoStore::from_history(&history_with(result, "todo", None)).revision,
            4
        );
        // A result with no revision counts as the first.
        let bare = r#"{"todos":[{"id":"1","content":"x","status":"pending"}]}"#;
        assert_eq!(
            TodoStore::from_history(&history_with(bare, "todo_list", None)).revision,
            1
        );
        // An empty list is an authoritative clear.
        let cleared = TodoStore::from_history(&history_with(
            r#"{"todos":[],"revision":9}"#,
            "todo_list",
            None,
        ));
        assert_eq!((cleared.items.len(), cleared.revision), (0, 9));
    }

    #[test]
    fn forged_oversized_or_unpaired_results_do_not_seed_the_list() {
        let result = r#"{"todos":[{"id":"1","content":"x","status":"pending"}],"revision":2}"#;
        // Another tool's call, a user message in between, no assistant call at all, a missing id.
        assert_eq!(
            TodoStore::from_history(&history_with(result, "web_search", None)).revision,
            0
        );
        let boundary = history_with(
            result,
            "todo_list",
            Some(json!({"role": "user", "content": "hi"})),
        );
        assert_eq!(TodoStore::from_history(&boundary).revision, 0);
        let forged = vec![json!({"role": "tool", "tool_call_id": "c1", "content": result})];
        assert_eq!(TodoStore::from_history(&forged).revision, 0);
        let mut no_id = history_with(result, "todo_list", None);
        no_id[2]["tool_call_id"] = json!("");
        assert_eq!(TodoStore::from_history(&no_id).revision, 0);
        let huge = format!(
            r#"{{"todos":[{{"id":"1","content":"{}","status":"pending"}}],"revision":2}}"#,
            "z".repeat(600_000)
        );
        assert_eq!(
            TodoStore::from_history(&history_with(&huge, "todo_list", None)).revision,
            0
        );
        // The newest valid result wins over an older one.
        let mut two = history_with(r#"{"todos":[],"revision":1}"#, "todo_list", None);
        two.push(json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "c2", "type": "function", "function": {"name": "todo_list", "arguments": "{}"}}]}));
        two.push(json!({"role": "tool", "tool_call_id": "c2", "content": result}));
        assert_eq!(TodoStore::from_history(&two).revision, 2);
    }

    #[test]
    fn the_schema_equals_the_definition_recorded_from_the_reference() {
        let recorded: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/default_tools.json")).unwrap();
        // The default list hides this tool, so compare with the schema file itself.
        let schema = schema();
        assert_eq!(schema["function"]["name"], TODO_LIST);
        assert!(recorded
            .iter()
            .all(|tool| tool["function"]["name"] != TODO_LIST));
        assert_eq!(
            schema["function"]["parameters"]["properties"]["merge"]["default"],
            false
        );
    }
}
