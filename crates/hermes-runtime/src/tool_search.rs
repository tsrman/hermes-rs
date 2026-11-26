//! The tool-search bridge: `tool_search`, `tool_describe` and `tool_call`.
//!
//! Mirrors `tools/tool_search.py`, `tool_search_catalog.py` and `tool_search_validation.py`. The
//! tools the profile defers are left out of every request; three bridge tools take their place.
//! The catalog is rebuilt from the session's tools on every use, so it never drifts.
//!
//! Not ported: connector tools, MCP tools (still listed directly), and JSON-schema validation of
//! `tool_call` arguments beyond the required-field probe (the reference runs the full validator
//! only when `jsonschema` happens to be installed).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use rust_stemmers::{Algorithm, Stemmer};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::tool_policy::SearchSettings;
use crate::tools::ToolEnv;

pub(crate) const TOOL_SEARCH: &str = "tool_search";
pub(crate) const TOOL_DESCRIBE: &str = "tool_describe";
pub(crate) const TOOL_CALL: &str = "tool_call";
const BRIDGE: [&str; 3] = [TOOL_SEARCH, TOOL_DESCRIBE, TOOL_CALL];

const CHARS_PER_TOKEN: f64 = 4.0;
const MAX_QUERIES_PER_CALL: usize = 7;
const MAX_DESCRIBE_NAMES_PER_CALL: usize = 10;
const CATALOG_SUMMARY_MAX_CHARS: usize = 80;
const MIN_QUERY_TERM_COVERAGE: f64 = 0.5;
const MIN_ANSWERABLE_TERMS_FOR_COVERAGE: usize = 4;
const ECHO_ARGS_MAX_CHARS: usize = 1500;
const MAX_ERROR_CHARS: usize = 2048;

#[derive(Deserialize)]
struct Registry {
    /// Each registered tool's toolset.
    toolsets: HashMap<String, String>,
    core: Vec<String>,
    direct_surface: Vec<String>,
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    serde_json::from_str(include_str!("tool_registry.json")).expect("valid tool_registry.json")
});

fn is_core(name: &str) -> bool {
    REGISTRY.core.iter().any(|core| core == name)
}

fn registered(name: &str) -> bool {
    REGISTRY.toolsets.contains_key(name)
}

fn toolset_of(name: &str) -> Option<&'static str> {
    REGISTRY.toolsets.get(name).map(String::as_str)
}

pub(crate) fn is_bridge(name: &str) -> bool {
    BRIDGE.contains(&name)
}

// ── Errors ───────────────────────────────────────────────────────────────────────────────────────

fn tool_error(message: &str) -> Value {
    let text = if message.chars().count() <= MAX_ERROR_CHARS {
        message.to_owned()
    } else {
        let head: String = message.chars().take(MAX_ERROR_CHARS).collect();
        format!("{head}… [truncated]")
    };
    json!({"error": text})
}

fn unavailable_in_session_reason(name: &str) -> String {
    format!(
        "'{name}' is not available in this execution context. Don't retry it — state the assumption and proceed with the work."
    )
}

/// A session-gated GUI tool names the surface it needs.
fn out_of_scope_reason(name: &str) -> Option<String> {
    let toolset = toolset_of(name)?;
    REGISTRY
        .direct_surface
        .iter()
        .any(|surface| surface == toolset)
        .then(|| {
            format!(
                "'{name}' needs a desktop-app session with a GUI surface (preview/terminal panes). This session has none: use it only from the Hermes desktop app, not via tool_search."
            )
        })
}

/// A core tool missing from the session's own list, whether or not `defer` moved it.
fn session_unavailable_error(name: &str, session: &HashSet<String>) -> Option<String> {
    if session.contains(name) || !is_core(name) || !registered(name) {
        return None;
    }
    Some(out_of_scope_reason(name).unwrap_or_else(|| unavailable_in_session_reason(name)))
}

fn not_deferrable_error(name: &str, session: &HashSet<String>) -> String {
    if is_core(name) || registered(name) {
        if !session.contains(name) {
            return unavailable_in_session_reason(name);
        }
        return format!(
            "'{name}' is a directly-listed tool, not a deferred one. Call it directly instead of via tool_call."
        );
    }
    let suffix = format!("__{name}");
    let mut candidates: Vec<&String> = REGISTRY
        .toolsets
        .keys()
        .filter(|candidate| candidate.ends_with(&suffix))
        .collect();
    candidates.sort();
    let hint = if candidates.is_empty() {
        " Use tool_search to find the exact name.".to_owned()
    } else {
        format!(
            " Did you mean {}?",
            candidates
                .iter()
                .map(|candidate| format!("'{candidate}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "'{name}' is not a known tool name. Deferred tools must be invoked through tool_call by the exact name tool_search returns (e.g. mcp__<server>__{{tool}}).{hint}"
    )
}

fn scope_block_reason(name: &str) -> String {
    out_of_scope_reason(name)
        .or_else(|| {
            (is_core(name) && registered(name)).then(|| unavailable_in_session_reason(name))
        })
        .unwrap_or_else(|| {
            format!("'{name}' is not available in this session. Use tool_search to find tools you can call.")
        })
}

/// Eligible for deferral: named in the profile's `defer` list, or neither core nor a session-gated
/// GUI surface (a plugin tool).
fn is_deferrable(name: &str, env: &ToolEnv) -> bool {
    if is_bridge(name) {
        return false;
    }
    if env.policy.is_deferred(name) {
        return true;
    }
    if is_core(name) {
        return false;
    }
    match toolset_of(name) {
        None => false,
        Some(toolset) => {
            toolset.starts_with("mcp-")
                || !REGISTRY
                    .direct_surface
                    .iter()
                    .any(|surface| surface == toolset)
        }
    }
}

// ── Catalog ──────────────────────────────────────────────────────────────────────────────────────

struct Entry {
    name: String,
    description: String,
    schema: Value,
    source: &'static str,
    source_name: String,
    tokens: Vec<String>,
}

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9]+").expect("valid regex"));

fn tokenize(text: &str) -> Vec<String> {
    let stemmer = Stemmer::create(Algorithm::English);
    TOKEN
        .find_iter(text)
        .map(|word| {
            stemmer
                .stem(&word.as_str().to_ascii_lowercase())
                .into_owned()
        })
        .collect()
}

fn function_of(schema: &Value) -> &Value {
    schema.get("function").unwrap_or(&Value::Null)
}

fn listing_group_label(source_name: &str) -> String {
    let label = if source_name.is_empty() {
        "other"
    } else {
        source_name
    };
    label.strip_prefix("mcp-").unwrap_or(label).to_owned()
}

fn search_text(function: &Value, source_label: &str) -> String {
    let name = function["name"].as_str().unwrap_or("");
    let name = name.strip_prefix("mcp__").unwrap_or(name);
    let words: String = name
        .chars()
        .map(|c| {
            if matches!(c, '_' | '.' | ':' | '-') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let extra = if !source_label.is_empty() && !words.split_whitespace().any(|w| w == source_label)
    {
        source_label
    } else {
        ""
    };
    let params = function["parameters"]["properties"]
        .as_object()
        .map(|props| props.keys().cloned().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    format!(
        "{words} {extra} {} {params}",
        function["description"].as_str().unwrap_or("")
    )
}

fn build_catalog(defs: &[Value]) -> Vec<Entry> {
    defs.iter()
        .filter_map(|schema| {
            let function = function_of(schema);
            let name = function["name"].as_str().filter(|name| !name.is_empty())?;
            let (source, source_name) = match toolset_of(name) {
                None => ("other", String::new()),
                Some(toolset) if toolset.starts_with("mcp-") => ("mcp", toolset.to_owned()),
                Some(toolset) => ("plugin", toolset.to_owned()),
            };
            let label = if source_name.is_empty() {
                String::new()
            } else {
                listing_group_label(&source_name)
            };
            Some(Entry {
                name: name.to_owned(),
                description: function["description"].as_str().unwrap_or("").to_owned(),
                schema: schema.clone(),
                source,
                source_name,
                tokens: tokenize(&search_text(function, &label)),
            })
        })
        .collect()
}

struct Stats {
    average: f64,
    frequency: HashMap<String, usize>,
    documents: usize,
}

fn corpus_stats(catalog: &[Entry]) -> Stats {
    let total: usize = catalog.iter().map(|entry| entry.tokens.len()).sum();
    let average = total as f64 / catalog.len().max(1) as f64;
    let mut frequency: HashMap<String, usize> = HashMap::new();
    for entry in catalog {
        let unique: HashSet<&String> = entry.tokens.iter().collect();
        for token in unique {
            *frequency.entry(token.clone()).or_default() += 1;
        }
    }
    Stats {
        average,
        frequency,
        documents: catalog.len(),
    }
}

fn idf(stats: &Stats, token: &str) -> f64 {
    let df = stats.frequency.get(token).copied().unwrap_or(0) as f64;
    (1.0 + (stats.documents as f64 - df + 0.5) / (df + 0.5)).ln()
}

fn bm25(query: &[String], document: &[String], stats: &Stats) -> f64 {
    let (k1, b) = (1.5, 0.75);
    let length = document.len() as f64;
    let mut frequencies: HashMap<&str, usize> = HashMap::new();
    for token in document {
        *frequencies.entry(token.as_str()).or_default() += 1;
    }
    let mut score = 0.0;
    for token in query {
        let df = stats.frequency.get(token).copied().unwrap_or(0);
        let tf = frequencies.get(token.as_str()).copied().unwrap_or(0);
        if df > 0 && tf > 0 {
            let tf = tf as f64;
            score += idf(stats, token) * tf * (k1 + 1.0)
                / (tf + k1 * (1.0 - b + b * length / stats.average.max(1.0)));
        }
    }
    score
}

/// Top `limit` entries for `query` by BM25; an exact name ranks first. Admission is by the query's
/// rarest token, plus a share of its answerable terms for long queries.
fn search_catalog<'a>(
    catalog: &'a [Entry],
    stats: &Stats,
    query: &str,
    limit: usize,
) -> Vec<&'a Entry> {
    let query_tokens = if catalog.is_empty() || limit == 0 {
        Vec::new()
    } else {
        tokenize(query)
    };
    if query_tokens.is_empty() {
        return Vec::new();
    }
    // The first token of highest IDF names the intent.
    let mut gate = &query_tokens[0];
    for token in &query_tokens {
        if idf(stats, token) > idf(stats, gate) {
            gate = token;
        }
    }
    let answerable: HashSet<&String> = query_tokens
        .iter()
        .filter(|token| stats.frequency.get(*token).copied().unwrap_or(0) > 0)
        .collect();
    let required = if answerable.len() < MIN_ANSWERABLE_TERMS_FOR_COVERAGE {
        1
    } else {
        (answerable.len() as f64 * MIN_QUERY_TERM_COVERAGE).ceil() as usize
    };
    let exact = query.trim().to_lowercase();
    let mut scored: Vec<(f64, &Entry)> = catalog
        .iter()
        .filter(|entry| {
            if entry.name.to_lowercase() == exact {
                return true;
            }
            let tokens: HashSet<&String> = entry.tokens.iter().collect();
            tokens.contains(gate)
                && answerable
                    .iter()
                    .filter(|token| tokens.contains(**token))
                    .count()
                    >= required
        })
        .map(|entry| {
            let score = if entry.name.to_lowercase() == exact {
                f64::INFINITY
            } else {
                bm25(&query_tokens, &entry.tokens, stats)
            };
            (score, entry)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry)
        .collect()
}

// ── Listing ──────────────────────────────────────────────────────────────────────────────────────

static SENTENCE_END: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?<!\be\.g)(?<!\bi\.e)(?<!\betc)[.!?](?=\s|$)").expect("valid regex")
});

/// The first sentence of a description, clipped to `max_chars` on a word boundary.
fn short_description(description: &str) -> String {
    let text = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = match SENTENCE_END.find(&text) {
        Ok(Some(found)) => text[..found.end()].to_owned(),
        _ => text,
    };
    if text.chars().count() <= CATALOG_SUMMARY_MAX_CHARS {
        return text;
    }
    let clipped: String = text.chars().take(CATALOG_SUMMARY_MAX_CHARS).collect();
    let clipped = match clipped.rsplit_once(' ') {
        Some((head, _)) => head.to_owned(),
        None => clipped,
    };
    format!("{}…", clipped.trim_end_matches([',', ';', ':', ' ']))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Full,
    Names,
    Summary,
}

/// The catalog manifest: `- name: short desc` lines grouped per toolset, degrading per group (all
/// full, all names, then the largest groups collapsed first) until it fits the token budget.
/// Returns the text and its form (`full`, `names`, `mixed`, `groups` or `none`).
fn listing(defs: &[Value], max_tokens: usize) -> (Option<String>, &'static str) {
    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for schema in defs {
        let function = function_of(schema);
        let Some(name) = function["name"].as_str().filter(|name| !name.is_empty()) else {
            continue;
        };
        let label = listing_group_label(toolset_of(name).unwrap_or(""));
        groups.entry(label).or_default().push((
            name.to_owned(),
            short_description(function["description"].as_str().unwrap_or("")),
        ));
    }
    if groups.is_empty() {
        return (None, "none");
    }
    let render = |label: &str, mode: Mode| -> String {
        let mut tools = groups[label].clone();
        tools.sort();
        if mode == Mode::Summary {
            return format!(
                "{label} ({} tools — names not listed; discover via `{TOOL_SEARCH}`)",
                tools.len()
            );
        }
        let lines: Vec<String> = if mode == Mode::Full {
            tools
                .iter()
                .map(|(name, desc)| {
                    if desc.is_empty() {
                        format!("- {name}")
                    } else {
                        format!("- {name}: {desc}")
                    }
                })
                .collect()
        } else if tools.len() > 1 {
            vec![tools
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")]
        } else {
            vec![format!("- {}", tools[0].0)]
        };
        if tools.len() == 1 {
            lines.join("\n")
        } else {
            std::iter::once(format!("{label} tools ({}):", tools.len()))
                .chain(lines)
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let header = format!(
        "Deferred tool catalog (call schemas via `{TOOL_DESCRIBE}`, invoke via `{TOOL_CALL}`):"
    );
    let assemble = |modes: &BTreeMap<&String, Mode>| -> Option<String> {
        let singles: Vec<String> = groups
            .keys()
            .filter(|label| groups[*label].len() == 1 && modes[label] != Mode::Summary)
            .map(|label| render(label, modes[label]))
            .collect();
        let blocks: Vec<String> = groups
            .keys()
            .filter(|label| groups[*label].len() > 1 || modes[label] == Mode::Summary)
            .map(|label| render(label, modes[label]))
            .collect();
        let text = std::iter::once(header.clone())
            .chain(singles)
            .chain(blocks)
            .collect::<Vec<_>>()
            .join("\n");
        ((text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as usize <= max_tokens)
            .then_some(text)
    };
    for (mode, form) in [(Mode::Full, "full"), (Mode::Names, "names")] {
        let modes: BTreeMap<&String, Mode> = groups.keys().map(|label| (label, mode)).collect();
        if let Some(text) = assemble(&modes) {
            return (Some(text), form);
        }
    }
    // Collapse the largest rendered groups first (size, then label).
    let mut order: Vec<&String> = groups.keys().collect();
    order.sort_by(|a, b| {
        render(b, Mode::Names)
            .chars()
            .count()
            .cmp(&render(a, Mode::Names).chars().count())
            .then_with(|| a.cmp(b))
    });
    let mut summarized: HashSet<&String> = HashSet::new();
    for label in order {
        summarized.insert(label);
        for rest in [Mode::Full, Mode::Names] {
            let modes: BTreeMap<&String, Mode> = groups
                .keys()
                .map(|group| {
                    (
                        group,
                        if summarized.contains(group) {
                            Mode::Summary
                        } else {
                            rest
                        },
                    )
                })
                .collect();
            if let Some(text) = assemble(&modes) {
                let form = if summarized.len() == groups.len() {
                    "groups"
                } else {
                    "mixed"
                };
                return (Some(text), form);
            }
        }
    }
    (None, "none")
}

// ── Bridge schemas ───────────────────────────────────────────────────────────────────────────────

fn bridge_schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type": "function", "function": {
        "name": name, "description": description,
        "parameters": {"type": "object", "properties": properties, "required": required}}})
}

fn search_description(deferred: usize, listing: Option<&str>, form: &str) -> String {
    let mut description = format!(
        "Search {deferred} additional tools that are loaded on demand. Send one keyword query per capability you need; returns matching tool names and descriptions. Then `{TOOL_DESCRIBE}` loads schemas and `{TOOL_CALL}` invokes. Tools already in your tool list need no search."
    );
    let Some(listing) = listing else {
        return description;
    };
    if form == "groups" {
        description.push_str(
            "\n\nThe servers below are connected and their tools ARE available through this bridge. For any request in these domains, search here FIRST — do not claim the capability is unavailable and do not substitute a generic tool (terminal/browser) without searching.\n\n",
        );
        description.push_str(listing);
        return description;
    }
    description.push_str(&format!(
        "\n\nEvery tool listed below exists: never call it unavailable, and go straight to `{TOOL_DESCRIBE}` when you see the exact name."
    ));
    if form == "mixed" {
        description.push_str(&format!(
            " For servers marked 'names not listed', the tools exist too — find them with `{TOOL_SEARCH}` before concluding anything is missing."
        ));
    }
    description.push_str("\n\n");
    description.push_str(listing);
    description
}

fn bridge_schemas(deferred: usize, listing: Option<&str>, form: &str) -> Vec<Value> {
    vec![
        bridge_schema(
            TOOL_SEARCH,
            &search_description(deferred, listing, form),
            json!({
                "queries": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "One query per capability: service + action + object keywords (e.g. ['gmail send email', 'github create issue']), never a question; every word must match tool text. Searched in parallel.",
                },
                "limit": {"type": "integer", "description": "Max matches per query (default 5)."},
            }),
            &["queries"],
        ),
        bridge_schema(
            TOOL_DESCRIBE,
            &format!(
                "Load the full JSON schemas for tools returned by `{TOOL_SEARCH}`. Required before `{TOOL_CALL}` if a tool's parameters are unknown. Batch every schema you need into one call."
            ),
            json!({"names": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Exact tool names (as returned by tool_search). A single string is accepted and treated as one name.",
            }}),
            &["names"],
        ),
        bridge_schema(
            TOOL_CALL,
            &format!(
                "Invoke deferred tools: `calls` is an array of {{name, arguments}}, one entry per invocation (batch independent calls; results come back in order). Arguments match the schema from `{TOOL_DESCRIBE}`; approvals and hooks apply as usual."
            ),
            json!({"calls": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Exact tool name to invoke."},
                        "arguments": {"type": "object", "description": "Arguments matching the tool schema."},
                    },
                    "required": ["name", "arguments"],
                },
                "description": "One or more independent deferred tool invocations.",
            }}),
            &["calls"],
        ),
    ]
}

/// The bridge tools for the session, or none when no tool is deferred.
pub(crate) fn bridge_for(env: &ToolEnv) -> Vec<Value> {
    let deferred = crate::tools::deferred_schemas(env);
    if deferred.is_empty() {
        return Vec::new();
    }
    let settings = env.policy.search_settings();
    let (text, form) = if settings.listing {
        // Without a known context window the percentage leg is the reference's 10K default.
        listing(&deferred, settings.listing_max_tokens.min(10_000))
    } else {
        (None, "none")
    };
    bridge_schemas(deferred.len(), text.as_deref(), form)
}

// ── Dispatch ─────────────────────────────────────────────────────────────────────────────────────

/// What the bridge decided: a finished answer, or the real tool to run.
pub(crate) enum Bridged {
    Done(Value),
    Call { name: String, arguments: String },
}

/// Python's `str(item or "")` for a bridge list entry.
fn py_text(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(false) => String::new(),
        Value::Number(number) if number.as_f64() == Some(0.0) => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".into(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

fn string_list(
    args: &Value,
    key: &str,
    dedupe: bool,
    max_items: usize,
    retry_hint: &str,
) -> Result<Vec<String>, Value> {
    let raw = args.get(key).cloned().unwrap_or(Value::Null);
    let raw = match raw {
        Value::String(_) => Value::Array(vec![raw]),
        other => other,
    };
    let Value::Array(items) = raw else {
        return Err(tool_error(&format!(
            "{key} is required and must be an array of strings"
        )));
    };
    let mut out: Vec<String> = Vec::new();
    for item in &items {
        let text = py_text(item).trim().to_owned();
        if !text.is_empty() && (!dedupe || !out.contains(&text)) {
            out.push(text);
        }
    }
    if out.is_empty() {
        return Err(tool_error(&format!(
            "{key} is required and must contain at least one non-empty string"
        )));
    }
    if out.len() > max_items {
        return Err(tool_error(&format!(
            "too many {key}: {} > max {max_items}. {retry_hint}",
            out.len()
        )));
    }
    Ok(out)
}

fn clamped_limit(raw: Option<&Value>, settings: &SearchSettings) -> usize {
    let (fallback, high) = (settings.default_limit as i64, settings.max_limit as i64);
    let parsed = match raw {
        None | Some(Value::Null) => return settings.default_limit,
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Some(Value::Bool(flag)) => Some(i64::from(*flag)),
        Some(Value::String(text)) => text.trim().parse().ok(),
        Some(_) => None,
    };
    parsed.unwrap_or(fallback).clamp(1, high) as usize
}

fn clip_description(text: &str) -> String {
    if text.chars().count() <= 500 {
        return text.to_owned();
    }
    format!("{}…", text.chars().take(500).collect::<String>())
}

fn shared_record(entry: &Entry) -> Value {
    let required: Vec<Value> = function_of(&entry.schema)["parameters"]["required"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| json!(item.chars().take(64).collect::<String>()))
                .take(32)
                .collect()
        })
        .unwrap_or_default();
    json!({
        "source": entry.source,
        "source_name": entry.source_name,
        "description": clip_description(&entry.description),
        "required": required,
    })
}

fn search(env: &ToolEnv, args: &Value) -> Value {
    let settings = env.policy.search_settings();
    let queries = match string_list(
        args,
        "queries",
        false,
        MAX_QUERIES_PER_CALL,
        "Retry with fewer, more targeted queries.",
    ) {
        Ok(queries) => queries,
        Err(error) => return error,
    };
    let limit = clamped_limit(args.get("limit"), &settings);
    let catalog = build_catalog(&crate::tools::deferred_schemas(env));
    let stats = corpus_stats(&catalog);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &catalog {
        *counts
            .entry(listing_group_label(&entry.source_name))
            .or_default() += 1;
    }
    let sources: Vec<Value> = counts
        .iter()
        .map(|(name, count)| json!({"name": name, "tool_count": count}))
        .collect();
    let mut results = Vec::new();
    let mut tools = Map::new();
    for query in &queries {
        let hits = search_catalog(&catalog, &stats, query, limit);
        for hit in &hits {
            tools
                .entry(hit.name.clone())
                .or_insert_with(|| shared_record(hit));
        }
        let mut group = Map::new();
        group.insert("query".into(), json!(query));
        group.insert(
            "matches".into(),
            json!(hits.iter().map(|hit| hit.name.as_str()).collect::<Vec<_>>()),
        );
        if hits.is_empty() && !sources.is_empty() {
            group.insert("available_sources".into(), json!(sources));
            group.insert("hint".into(), json!("This query returned no lexical matches, but the sources above are connected and their tools remain available. Retry tool_search with the service name plus a concrete action or object before concluding the capability is unavailable."));
        }
        results.push(Value::Object(group));
    }
    json!({
        "queries": queries,
        "total_available": catalog.len(),
        "results": results,
        "tools": tools,
    })
}

fn session_names(env: &ToolEnv) -> HashSet<String> {
    let mut names: HashSet<String> = crate::tools::offered_schemas(env)
        .iter()
        .chain(crate::tools::deferred_schemas(env).iter())
        .filter_map(|schema| function_of(schema)["name"].as_str().map(str::to_owned))
        .collect();
    names.retain(|name| !name.is_empty());
    names
}

fn describe(env: &ToolEnv, args: &Value) -> Value {
    let names = match string_list(
        args,
        "names",
        true,
        MAX_DESCRIBE_NAMES_PER_CALL,
        "Retry with fewer names per call.",
    ) {
        Ok(names) => names,
        Err(error) => return error,
    };
    let deferred = crate::tools::deferred_schemas(env);
    let by_name: HashMap<&str, &Value> = deferred
        .iter()
        .filter_map(|schema| Some((function_of(schema)["name"].as_str()?, function_of(schema))))
        .collect();
    let session = session_names(env);
    let mut tools = Map::new();
    let mut not_found: Vec<String> = Vec::new();
    let mut errors = Map::new();
    for name in names {
        if let Some(function) = by_name.get(name.as_str()) {
            tools.insert(
                name,
                json!({
                    "description": function.get("description").cloned().unwrap_or(json!("")),
                    "parameters": function.get("parameters").cloned().unwrap_or(json!({})),
                }),
            );
        } else if name.starts_with("connectors__") {
            not_found.push(name);
        } else if let Some(message) = session_unavailable_error(&name, &session) {
            errors.insert(name, json!(message));
        } else if registered(&name) && !is_deferrable(&name, env) {
            let message = not_deferrable_error(&name, &session);
            errors.insert(name, json!(message));
        } else {
            not_found.push(name);
        }
    }
    let mut result = Map::new();
    result.insert("tools".into(), Value::Object(tools));
    if !not_found.is_empty() {
        result.insert("not_found".into(), json!(not_found));
        result.insert(
            "hint".into(),
            json!("Names in not_found are not currently available. Re-run tool_search to refresh."),
        );
    }
    if !errors.is_empty() {
        result.insert("errors".into(), Value::Object(errors));
    }
    Value::Object(result)
}

/// `json.loads`, with Python's wording for the two commonest failures: text that is not JSON at
/// all and trailing text after a value. Other failures keep the parser's own words.
fn parse_json_like_python(text: &str) -> Result<Value, String> {
    let position = |char_index: usize| {
        let before: Vec<char> = text.chars().take(char_index).collect();
        let line = before.iter().filter(|c| **c == '\n').count() + 1;
        let column = before
            .iter()
            .rev()
            .position(|c| *c == '\n')
            .unwrap_or(before.len())
            + 1;
        format!("line {line} column {column} (char {char_index})")
    };
    let skipped = text.chars().take_while(|c| c.is_whitespace()).count();
    let from_value: String = text.chars().skip(skipped).collect();
    let starts_value = from_value
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '{' | '[' | '"' | '-') || c.is_ascii_digit())
        || ["true", "false", "null", "NaN", "Infinity"]
            .iter()
            .any(|literal| from_value.starts_with(literal));
    if !starts_value {
        return Err(format!("Expecting value: {}", position(skipped)));
    }
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    match stream.next() {
        Some(Ok(value)) => {
            let consumed = stream.byte_offset();
            let rest = &text[consumed..];
            let extra = rest.chars().take_while(|c| c.is_whitespace()).count();
            if rest.chars().nth(extra).is_some() {
                let index = text[..consumed].chars().count() + extra;
                return Err(format!("Extra data: {}", position(index)));
            }
            Ok(value)
        }
        Some(Err(error)) => Err(error.to_string()),
        None => Err(format!("Expecting value: {}", position(skipped))),
    }
}

/// The batch `calls: [{name, arguments}]`, tolerating a single legacy `{name, arguments}`.
fn normalize_entries(args: &Value) -> Result<Vec<(String, Value)>, String> {
    let mut raw = match args.get("calls") {
        None | Some(Value::Null) => {
            let name = py_text(args.get("name").unwrap_or(&Value::Null));
            if name.trim().is_empty() {
                return Err("tool_call requires 'calls' (an array of {name, arguments})".into());
            }
            Value::Array(vec![json!({
                "name": args.get("name"), "arguments": args.get("arguments")
            })])
        }
        Some(other) => other.clone(),
    };
    if let Value::String(text) = &raw {
        raw = parse_json_like_python(text)
            .map_err(|error| format!("tool_call 'calls' is not valid JSON: {error}"))?;
    }
    if raw.is_object() {
        raw = Value::Array(vec![raw]);
    }
    let Value::Array(items) = raw else {
        return Err("tool_call 'calls' must be a non-empty array of {name, arguments}".into());
    };
    if items.is_empty() {
        return Err("tool_call 'calls' must be a non-empty array of {name, arguments}".into());
    }
    let mut entries = Vec::new();
    for (position, item) in items.iter().enumerate() {
        let Value::Object(map) = item else {
            return Err(format!(
                "tool_call calls[{position}] must be an object with 'name' and 'arguments'"
            ));
        };
        let name = py_text(map.get("name").unwrap_or(&Value::Null))
            .trim()
            .to_owned();
        if name.is_empty() {
            return Err(format!("tool_call calls[{position}] requires a 'name'"));
        }
        if is_bridge(&name) {
            return Err(format!(
                "tool_call cannot invoke '{name}' (it is itself a bridge tool)"
            ));
        }
        let mut arguments = map.get("arguments").cloned().unwrap_or(Value::Null);
        if arguments.is_null()
            || arguments
                .as_str()
                .is_some_and(|text| text.trim().is_empty())
        {
            arguments = json!({});
        }
        if let Value::String(text) = &arguments {
            arguments = parse_json_like_python(text).map_err(|error| {
                format!("tool_call calls[{position}].arguments is not valid JSON: {error}")
            })?;
        }
        if !arguments.is_object() {
            return Err(format!(
                "tool_call calls[{position}].arguments must be an object"
            ));
        }
        entries.push((name, arguments));
    }
    Ok(entries)
}

/// Rejection for a multi-entry batch that names a local tool: restates the valid shape with the
/// caller's own first entry.
fn local_batch_error(entries: &[(String, Value)]) -> String {
    let (name, arguments) = &entries[0];
    let mut args = arguments.to_string();
    if args.chars().count() > ECHO_ARGS_MAX_CHARS {
        args = "{...}".into();
    }
    let retry = format!(
        "{{\"calls\":[{{\"name\":{},\"arguments\":{args}}}]}}",
        json!(name)
    );
    let remaining = if entries.len() > 1 {
        format!(
            " then issue the remaining {} call(s) as separate tool_call invocations",
            entries.len() - 1
        )
    } else {
        String::new()
    };
    format!(
        "tool_call takes exactly one entry for local tools; you sent {}. Retry with only: {retry}{remaining}. Only connectors__ names may be batched together.",
        entries.len()
    )
}

/// The required-field probe the reference runs before dispatching a deferred tool.
fn missing_required(env: &ToolEnv, name: &str, arguments: &Value) -> Option<Value> {
    let deferred = crate::tools::deferred_schemas(env);
    let schema = deferred
        .iter()
        .find(|schema| function_of(schema)["name"] == name)?;
    let parameters = function_of(schema)["parameters"].as_object()?;
    let required = parameters.get("required")?.as_array()?;
    let missing: Vec<&str> = required
        .iter()
        .filter_map(Value::as_str)
        .filter(|field| arguments.get(*field).is_none())
        .collect();
    if missing.is_empty() {
        return None;
    }
    let message = format!(
        "tool_call to '{name}' is missing required argument(s): {}. The tool was NOT invoked.",
        missing.join(", ")
    );
    let mut error = tool_error(&message);
    error["path"] = json!("arguments");
    error["constraint"] = json!("required");
    error["parameters"] = Value::Object(parameters.clone());
    error["hint"] = json!("Retry tool_call with 'arguments' matching the parameters schema above.");
    Some(error)
}

fn resolve_call(env: &ToolEnv, args: &Value) -> Bridged {
    let entries = match normalize_entries(args) {
        Ok(entries) => entries,
        Err(message) => return Bridged::Done(tool_error(&message)),
    };
    let connector = |name: &str| name.starts_with("connectors__");
    if entries.len() > 1 && entries.iter().any(|(name, _)| !connector(name)) {
        return Bridged::Done(tool_error(&local_batch_error(&entries)));
    }
    if connector(&entries[0].0) {
        return Bridged::Done(tool_error("Connectors are not available in this session."));
    }
    let (name, arguments) = entries.into_iter().next().expect("one entry");
    let session = session_names(env);
    if let Some(message) = session_unavailable_error(&name, &session) {
        return Bridged::Done(tool_error(&message));
    }
    if !is_deferrable(&name, env) {
        return Bridged::Done(tool_error(&not_deferrable_error(&name, &session)));
    }
    let in_scope = crate::tools::deferred_schemas(env)
        .iter()
        .any(|schema| function_of(schema)["name"] == name.as_str());
    if !in_scope {
        return Bridged::Done(tool_error(&scope_block_reason(&name)));
    }
    if let Some(error) = missing_required(env, &name, &arguments) {
        return Bridged::Done(error);
    }
    Bridged::Call {
        name,
        arguments: arguments.to_string(),
    }
}

/// Run a bridge call, or `None` when `name` is not a bridge tool or no tool is deferred.
pub(crate) fn dispatch(env: &ToolEnv, name: &str, arguments: &str) -> Option<Bridged> {
    if !is_bridge(name) || crate::tools::deferred_schemas(env).is_empty() {
        return None;
    }
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let args = if args.is_object() { args } else { json!({}) };
    Some(match name {
        TOOL_SEARCH => Bridged::Done(search(env, &args)),
        TOOL_DESCRIBE => Bridged::Done(describe(env, &args)),
        _ => resolve_call(env, &args),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_tool::MemorySettings;
    use crate::tool_policy::ToolPolicy;
    use crate::web_tools::{Backend, WebSettings};

    /// The recorded cases, each with the tools hidden to reproduce the tool set it was recorded
    /// over. The Windows cases lack process_manage, which Windows does not offer (see
    /// `process_stub`); hiding it lets Linux and macOS check them too.
    fn fixtures() -> Vec<(Value, &'static [&'static str])> {
        let windows = include_str!("../tests/fixtures/tool_search_cases_windows.json");
        let mut fixtures = vec![(
            serde_json::from_str(windows).unwrap(),
            &["process_manage"][..],
        )];
        if crate::terminal::SUPPORTED {
            let unix = include_str!("../tests/fixtures/tool_search_cases.json");
            fixtures.push((serde_json::from_str(unix).unwrap(), &[][..]));
        }
        fixtures
    }

    /// Every implemented tool on except `hidden`, with the profile's `tools.tool_search` set as
    /// recorded.
    fn env(raw: &Value, hidden: &[&str]) -> ToolEnv {
        let config: serde_yaml::Value = if raw.is_null() {
            serde_yaml::Value::Null
        } else {
            serde_yaml::from_str(&json!({"tools": {"tool_search": raw}}).to_string()).unwrap()
        };
        // Exclude cronjob_manage from the 17-tool fixtures recorded before it was added.
        let hidden: Vec<&str> = ["cronjob_manage"].iter().chain(hidden).copied().collect();
        ToolEnv {
            write_files: true,
            terminal: true,
            delegation: Some(10),
            has_profile: true,
            skills_indexed: true,
            memory: Some(MemorySettings::default()),
            web: Some(WebSettings {
                search: Backend::Tavily,
                extract: Backend::Tavily,
                ..WebSettings::default()
            }),
            policy: ToolPolicy::from_config(&config).without_tools(&hidden),
            ..ToolEnv::default()
        }
    }

    fn names(tools: &[Value]) -> Vec<String> {
        tools
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_bridge_is_assembled_as_the_reference_assembles_it() {
        for (fixture, hidden) in fixtures() {
            for case in fixture["assemble"].as_array().unwrap() {
                let label = case["name"].as_str().unwrap();
                let env = env(&case["config"], hidden);
                let mut ours = crate::tools::offered_schemas(&env);
                crate::tools::sort_by_name(&mut ours);
                crate::tools::append_bridge(&env, &mut ours);
                let expected: Vec<Value> = case["tools"].as_array().unwrap().clone();
                assert_eq!(
                    names(&ours),
                    names(&expected),
                    "{label} {hidden:?}: tool names"
                );
                if case["activated"] == json!(true) {
                    let count = ours.len();
                    assert_eq!(
                        ours[count - 3..],
                        expected[count - 3..],
                        "{label} {hidden:?}: bridge"
                    );
                }
            }
        }
    }

    #[test]
    fn searches_match_the_reference() {
        for (fixture, hidden) in fixtures() {
            for case in fixture["search"].as_array().unwrap() {
                let env = env(&case["config"], hidden);
                let answer = match dispatch(&env, TOOL_SEARCH, &case["arguments"].to_string()) {
                    Some(Bridged::Done(answer)) => answer,
                    _ => panic!("tool_search answers itself"),
                };
                assert_eq!(
                    answer, case["result"],
                    "{} {} {hidden:?}",
                    case["config"], case["arguments"]
                );
            }
        }
    }

    #[test]
    fn descriptions_match_the_reference() {
        for (fixture, hidden) in fixtures() {
            for case in fixture["describe"].as_array().unwrap() {
                let env = env(&case["config"], hidden);
                let answer = match dispatch(&env, TOOL_DESCRIBE, &case["arguments"].to_string()) {
                    Some(Bridged::Done(answer)) => answer,
                    _ => panic!("tool_describe answers itself"),
                };
                assert_eq!(
                    answer, case["result"],
                    "{} {} {hidden:?}",
                    case["config"], case["arguments"]
                );
            }
        }
    }

    #[test]
    fn calls_resolve_as_the_reference_resolves_them() {
        for (fixture, hidden) in fixtures() {
            for case in fixture["call"].as_array().unwrap() {
                let env = env(&case["config"], hidden);
                let outcome = match dispatch(&env, TOOL_CALL, &case["arguments"].to_string()) {
                    Some(Bridged::Done(answer)) => answer,
                    Some(Bridged::Call { name, arguments }) => {
                        json!({"call": name, "arguments": serde_json::from_str::<Value>(&arguments).unwrap()})
                    }
                    None => panic!("the bridge is active"),
                };
                assert_eq!(
                    outcome, case["outcome"],
                    "{} {} {hidden:?}",
                    case["config"], case["arguments"]
                );
            }
        }
    }

    #[test]
    fn nothing_deferred_means_no_bridge() {
        let env = env(&json!({"defer": []}), &[]);
        assert!(bridge_for(&env).is_empty());
        assert!(dispatch(&env, TOOL_SEARCH, "{}").is_none());
    }
}
