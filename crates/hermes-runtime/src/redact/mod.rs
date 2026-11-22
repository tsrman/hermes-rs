//! Redaction of credentials in text that reaches the model or another session.
//!
//! A port of `agent/redact.py`, built one pass at a time against a corpus recorded from the
//! reference (`compat/create_redaction_corpus.py`). Only the passes listed in `PORTED` exist
//! so far; the rest of `redact_credential_shapes` is not applied yet, so this is not a
//! complete redactor and must not be wired into a safety boundary until `PENDING` is empty.

mod assignments;
mod cfg;
mod cli;
mod credential_stores;
mod extra;
mod fields;
mod headers;
mod kv_connstr;
mod mask;
mod output;
mod prefix;
mod private_keys;
mod spaced;
mod tokens;
mod urls;

pub use mask::{mask_secret, mask_token};
pub use output::{
    command_reads_secret_file, is_env_dump_command, is_secret_file_arg, redact_terminal_output,
};

/// The passes of the reference's `redact_credential_shapes` that this module applies, in order.
pub const PORTED: [&str; 17] = [
    "private_key",
    "prefix",
    "env_assignment",
    "json_field",
    "python_repr",
    "cli_flag",
    "connection_string",
    "auth_header",
    "header_credential",
    "telegram",
    "db_connstr",
    "url",
    "webhook",
    "jwt",
    "webapp_link",
    "phone",
    "plain",
];

/// How deep a walk over a nested value goes before the rest is replaced by [`WALK_DEPTH_MARKER`].
pub const WALK_MAX_DEPTH: usize = 320;
pub const WALK_DEPTH_MARKER: &str = "[redacted: nesting limit]";

/// What kind of text is being redacted. The reference calls source files `code_file` (assignment
/// passes are skipped, so `MAX_TOKENS=100` and `"apiKey": "test"` fixtures survive), file content
/// shown to the agent `file_read` (masks cannot be written back as a dead credential), and
/// files known to hold credentials `secret_file` (every assignment pass runs, plus the
/// credential-store lines; it wins over the other two).
#[derive(Clone, Copy, Default)]
pub struct Mode {
    pub code_file: bool,
    pub file_read: bool,
    pub secret_file: bool,
    /// Also redact credential-named URL parameters and user information.
    pub strict_urls: bool,
    /// Redact even when `security.redact_secrets` is off (the reference's `force=True`).
    pub force: bool,
}

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Turn redaction of tool output on or off for this run (`security.redact_secrets`). Forced
/// redaction (stored-conversation text, the debug journal, CDP results) is not affected.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Redact credential shapes in `text` (the reference's `force=True` path, so no setting can
/// switch it off). Passes that are not ported yet leave the text as it is.
pub fn redact_sensitive_text(text: &str) -> String {
    redact(
        text,
        Mode {
            force: true,
            ..Mode::default()
        },
    )
}

/// Text derived from stored conversations (summaries, search results) re-enters unrelated
/// prompts, so it also loses credential-named URL parameters and user information.
pub fn redact_derived_text(text: &str) -> String {
    redact(
        text,
        Mode {
            strict_urls: true,
            force: true,
            ..Mode::default()
        },
    )
}

/// File content on its way to the agent: credentials become a sentinel that cannot be mistaken
/// for a usable key. `secret_file` says the source is known to hold credentials.
pub fn redact_file_read(text: &str, secret_file: bool) -> String {
    redact(
        text,
        Mode {
            file_read: true,
            secret_file,
            ..Mode::default()
        },
    )
}

/// Redact with an explicit mode.
pub fn redact_with(text: &str, mode: Mode) -> String {
    redact(text, mode)
}

fn redact(text: &str, mode: Mode) -> String {
    if text.is_empty() {
        return String::new();
    }
    if !mode.force && !enabled() {
        return text.to_owned();
    }
    let Mode {
        file_read,
        secret_file,
        strict_urls,
        ..
    } = mode;
    // `secret_file` is authoritative: it wins over `code_file`, so a caller cannot be fail-open.
    let code_file = (mode.code_file || file_read) && !secret_file;
    let sentinel = |body: &dyn Fn() -> String| mask::with_nonreusable(file_read, body);
    let text = private_keys::redact(text);
    let text = sentinel(&|| prefix::redact(&text));
    let text = sentinel(&|| extra::redact_zhipu(&text));
    let text = sentinel(&|| extra::redact_discord_tokens(&text));
    let mut text = text;
    let mut aws_command_lines = Vec::new();
    if secret_file {
        text = sentinel(&|| credential_stores::redact_credential_store_lines(&text));
        aws_command_lines = credential_stores::aws_credential_process_keys(&text);
    }
    if !code_file {
        text = credential_stores::outside_spans(&text, &aws_command_lines, |segment| {
            sentinel(&|| {
                let text = assignments::redact_env_assignments(segment);
                let text = cfg::redact_config_assignments(&text);
                let text = fields::redact_json_fields(&text);
                cfg::redact_yaml_assignments(&text)
            })
        });
    }
    let text = sentinel(&|| cli::redact_cli_secrets(&text));
    let text = extra::redact_basic_pairs(&text);
    let text = tokens::redact_auth_headers(&text);
    let mut text = if text.contains(':') {
        let text = headers::redact_header_credentials(&text);
        tokens::redact_telegram(&text)
    } else {
        text
    };
    if text.contains("://") {
        text = tokens::redact_db_urls(&text, code_file);
        text = urls::redact_db_url_query_passwords(&text);
        text = sentinel(&|| tokens::redact_webhooks(&text));
        text = urls::redact_url_bare_tokens(&text);
        text = urls::redact_userinfo_passwords(&text);
    }
    let text = urls::redact_network_path_userinfo(&text);
    let text = kv_connstr::redact_kv_connection_strings(&text, code_file);
    let text = tokens::redact_jwt(&text);
    let text = tokens::redact_webapp_links(&text);
    let text = if strict_urls {
        urls::redact_strict_url_credentials(&text)
    } else {
        text
    };
    let text = if text.contains('&') && text.contains('=') {
        urls::redact_form_body(&text)
    } else {
        text
    };
    tokens::redact_phone_numbers(&text)
}
