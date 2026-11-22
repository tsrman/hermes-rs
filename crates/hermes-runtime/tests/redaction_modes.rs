//! The redactor's file and terminal modes against the Python reference
//! (`compat/create_redaction_corpus.py` writes `redaction_modes.json` beside the main corpus).

use std::path::Path;

use hermes_runtime::redact::{
    command_reads_secret_file, is_env_dump_command, is_secret_file_arg, redact_terminal_output,
    redact_with, Mode,
};
use serde_json::Value;

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/redaction_modes.json")).unwrap()
}

#[test]
fn modes_match_the_reference() {
    let mut problems = Vec::new();
    for row in corpus()["modes"].as_array().unwrap() {
        let mode = Mode {
            code_file: row["code_file"] == true,
            file_read: row["file_read"] == true,
            secret_file: row["secret_file"] == true,
            strict_urls: false,
            keep_phone_numbers: false,
            // The rows were recorded from the reference's forced path.
            force: true,
        };
        let input = row["input"].as_str().unwrap();
        let got = redact_with(input, mode);
        if got != row["output"].as_str().unwrap() {
            problems.push(format!(
                "code={} read={} secret={}: {input:?}\n   ours {got:?}\n   want {:?}",
                mode.code_file, mode.file_read, mode.secret_file, row["output"]
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} rows differ:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn terminal_output_follows_the_command() {
    for row in corpus()["terminal"].as_array().unwrap() {
        let command = row["command"].as_str().unwrap();
        let got = redact_terminal_output(row["input"].as_str().unwrap(), Some(command), &[]);
        assert_eq!(got, row["output"].as_str().unwrap(), "{command:?}");
    }
}

#[test]
fn secret_files_and_commands_are_classified_as_the_reference_does() {
    let home = tempfile::tempdir().unwrap();
    let home_text = home.path().to_str().unwrap().to_owned();
    for row in corpus()["secret_args"].as_array().unwrap() {
        let arg = row["arg"].as_str().unwrap().replace("<home>", &home_text);
        let homes: [&Path; 1] = [home.path()];
        assert_eq!(
            is_secret_file_arg(&arg, &homes),
            row["result"] == true,
            "{arg:?}"
        );
    }
    for row in corpus()["reads"].as_array().unwrap() {
        let command = row["command"].as_str().unwrap();
        assert_eq!(
            command_reads_secret_file(command, &[]),
            row["reads"] == true,
            "{command:?}"
        );
        assert_eq!(
            is_env_dump_command(command),
            row["env_dump"] == true,
            "{command:?}"
        );
    }
}
