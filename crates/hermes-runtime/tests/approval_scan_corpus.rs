//! The shell-scanner primitives against the Python reference
//! (`compat/create_approval_scan_corpus.py`): `expand_ansi_c_quotes`, `substitution_depth` and
//! the `_scan_shell` steps under four option sets.

use hermes_runtime::approval_detect::ansi_c::expand_ansi_c_quotes;
use hermes_runtime::approval_detect::budget::Text;
use hermes_runtime::approval_detect::shell_scan::{scan_shell, substitution_depth, ScanOpts};
use serde_json::{json, Value};

fn option_sets() -> Vec<(&'static str, ScanOpts)> {
    vec![
        (
            "uq_brace",
            ScanOpts {
                subst_unquoted: true,
                subst_quoted: true,
                brace: true,
                ..ScanOpts::default()
            },
        ),
        (
            "q_naive",
            ScanOpts {
                subst_quoted: true,
                naive_backtick: true,
                ..ScanOpts::default()
            },
        ),
        (
            "comments",
            ScanOpts {
                comments: true,
                ..ScanOpts::default()
            },
        ),
        (
            "u_stop",
            ScanOpts {
                subst_unquoted: true,
                stop_unterminated: true,
                ..ScanOpts::default()
            },
        ),
    ]
}

fn steps(command: &str, opts: ScanOpts) -> Value {
    let text = Text::new(command);
    let rows: Vec<Value> = scan_shell(&text, 0, None, opts)
        .map(|step| {
            let step = step.expect("no budget is active");
            json!([
                step.kind.name(),
                step.i,
                step.j,
                step.quote.map(String::from)
            ])
        })
        .collect();
    Value::Array(rows)
}

#[test]
fn scanner_primitives_match_the_reference() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_scan_corpus.json")).unwrap();
    let mut failures = Vec::new();
    for row in corpus.as_array().unwrap() {
        let command = row["command"].as_str().unwrap();
        let shown: String = command.chars().take(60).collect();
        if expand_ansi_c_quotes(command) != row["ansi_c"].as_str().unwrap() {
            failures.push(format!("ansi_c: {shown:?}"));
        }
        if substitution_depth(command) as u64 != row["depth"].as_u64().unwrap() {
            failures.push(format!(
                "depth: {shown:?} got {} want {}",
                substitution_depth(command),
                row["depth"]
            ));
        }
        if let Some(expected) = row.get("steps") {
            for (name, opts) in option_sets() {
                if steps(command, opts) != expected[name] {
                    failures.push(format!("steps {name}: {shown:?}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} rows differ:\n{}",
        failures.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
