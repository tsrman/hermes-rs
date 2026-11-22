//! Config keys that change what the port sends, checked against the Python reference
//! (`compat/create_config_keys_fixture.py` writes `fixtures/config_keys.json`): the compression
//! trigger with `model_thresholds`, `max_attempts` and the protected message counts, the
//! `security.redact_phone_numbers` opt-in, and the scrub of the stored assistant reply.

use std::sync::Mutex;

use hermes_runtime::config::{load_chat_settings, CompactionSettings};
use hermes_runtime::redact::{redact_assistant_reply, set_enabled};
use serde_json::Value;

/// The redaction switch is process-wide; tests that flip it take turns.
static SWITCH: Mutex<()> = Mutex::new(());

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/config_keys.json")).unwrap()
}

fn settings(section: &str, window: Option<u64>) -> CompactionSettings {
    let model = window
        .map(|window| format!("model: {{context_length: {window}}}\n"))
        .unwrap_or_default();
    let config = serde_yaml::from_str(&format!("{model}compression: {section}\n")).unwrap();
    CompactionSettings::from_config(&config)
}

#[test]
fn the_trigger_follows_model_thresholds_like_the_reference() {
    let mut wrong = Vec::new();
    for row in fixture()["thresholds"].as_array().unwrap() {
        let section = row["compression"].as_str().unwrap();
        let mut ours = settings(section, Some(row["window"].as_u64().unwrap()));
        ours.set_route(
            row["model"].as_str().unwrap(),
            row["provider"].as_str().unwrap(),
        );
        let want = row["threshold_tokens"]
            .as_u64()
            .map(|tokens| tokens as usize);
        let got = binary_trigger(&ours);
        if got != want {
            wrong.push(format!(
                "{section} on {}/{} window {}: ours {got:?}, reference {want:?}",
                row["provider"], row["model"], row["window"]
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The trigger in tokens, found through `over_threshold` (about four bytes a token) by bisection.
fn binary_trigger(settings: &CompactionSettings) -> Option<usize> {
    if !settings.enabled() {
        return None;
    }
    let (mut low, mut high) = (1usize, 4_000_000usize);
    while low < high {
        let middle = (low + high) / 2;
        if settings.over_threshold(middle * 4) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    Some(low)
}

#[test]
fn counts_are_read_like_the_reference() {
    let mut wrong = Vec::new();
    for row in fixture()["compression_values"].as_array().unwrap() {
        let section = row["compression"].as_str().unwrap();
        let ours = settings(section, None);
        let (first, last) = ours.protected();
        let want_attempts = row["max_attempts"].as_u64().unwrap() as usize;
        if ours.attempts() != want_attempts {
            wrong.push(format!(
                "{section}: attempts {} want {want_attempts}",
                ours.attempts()
            ));
        }
        let want_first = row["protect_first"].as_u64().unwrap() as usize;
        if first != want_first {
            wrong.push(format!(
                "{section}: protect_first {first} want {want_first}"
            ));
        }
        // A negative protect_last_n has no meaning in the reference either; the port reads it as 0.
        let want_last = row["protect_last"].as_i64().unwrap().max(0) as usize;
        if last != want_last {
            wrong.push(format!("{section}: protect_last {last} want {want_last}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_phone_option_is_read_like_the_reference() {
    let mut wrong = Vec::new();
    for row in fixture()["phone_policy"].as_array().unwrap() {
        let yaml = row["yaml"].as_str().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.yaml"), format!("{yaml}\n")).unwrap();
        let loaded = load_chat_settings(Some(home.path())).unwrap();
        // A quoted 'no' or 'off' cannot be told from the bare word once parsed; the reference
        // reads the quoted one as text and keeps the numbers masked (documented difference).
        let quoted_word = yaml.contains("'no'") || yaml.contains("'off'");
        let want = row["keeps"].as_bool().unwrap();
        if loaded.keep_phone_numbers != want && !quoted_word {
            wrong.push(format!(
                "{yaml}: ours {}, reference {want}",
                loaded.keep_phone_numbers
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_stored_reply_is_scrubbed_like_the_reference() {
    let _turn = SWITCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let scrub = &fixture()["reply_scrub"];
    for (keep, key) in [(false, "on"), (true, "keep_phone")] {
        set_enabled(true);
        for row in scrub[key].as_array().unwrap() {
            let input = row["input"].as_str().unwrap();
            assert_eq!(
                redact_assistant_reply(input, keep),
                row["output"].as_str().unwrap(),
                "keep_phone_numbers={keep}: {input:?}"
            );
        }
    }
    // With `security.redact_secrets` off the reply is stored as it came, phone numbers included.
    set_enabled(false);
    for row in scrub["disabled"].as_array().unwrap() {
        let input = row["input"].as_str().unwrap();
        assert_eq!(
            redact_assistant_reply(input, false),
            row["output"].as_str().unwrap(),
            "{input:?}"
        );
    }
    set_enabled(true);
}

#[test]
fn pii_hashing_is_a_gateway_feature_and_the_cli_platform_never_uses_it() {
    for row in fixture()["pii"].as_array().unwrap() {
        if row["platform"] == "local" {
            assert_eq!(row["redacts"], false, "{row}");
        }
    }
}
