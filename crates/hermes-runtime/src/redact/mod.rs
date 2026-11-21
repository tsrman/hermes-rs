//! Redaction of credentials in text that reaches the model or another session.
//!
//! A port of `agent/redact.py`, built one pass at a time against a corpus recorded from the
//! reference (`compat/create_redaction_corpus.py`). Only the passes listed in `PORTED` exist
//! so far; the rest of `redact_credential_shapes` is not applied yet, so this is not a
//! complete redactor and must not be wired into a safety boundary until `PENDING` is empty.

mod assignments;
mod cfg;
mod cli;
mod extra;
mod fields;
mod headers;
mod kv_connstr;
mod mask;
mod prefix;
mod private_keys;
mod spaced;
mod tokens;
mod urls;

pub use mask::{mask_secret, mask_token};

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

/// Redact credential shapes in `text` (the reference's `force=True` path, so no setting can
/// switch it off). Passes that are not ported yet leave the text as it is.
pub fn redact_sensitive_text(text: &str) -> String {
    redact(text, false)
}

/// Text derived from stored conversations (summaries, search results) re-enters unrelated
/// prompts, so it also loses credential-named URL parameters and user information.
pub fn redact_derived_text(text: &str) -> String {
    redact(text, true)
}

fn redact(text: &str, strict_urls: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    let text = private_keys::redact(text);
    let text = prefix::redact(&text);
    let text = extra::redact_zhipu(&text);
    let text = extra::redact_discord_tokens(&text);
    let text = assignments::redact_env_assignments(&text);
    let text = cfg::redact_config_assignments(&text);
    let text = fields::redact_json_fields(&text);
    let text = cfg::redact_yaml_assignments(&text);
    let text = cli::redact_cli_secrets(&text);
    let text = extra::redact_basic_pairs(&text);
    let text = tokens::redact_auth_headers(&text);
    let mut text = if text.contains(':') {
        let text = headers::redact_header_credentials(&text);
        tokens::redact_telegram(&text)
    } else {
        text
    };
    if text.contains("://") {
        text = tokens::redact_db_urls(&text);
        text = urls::redact_db_url_query_passwords(&text);
        text = tokens::redact_webhooks(&text);
        text = urls::redact_url_bare_tokens(&text);
        text = urls::redact_userinfo_passwords(&text);
    }
    let text = urls::redact_network_path_userinfo(&text);
    let text = kv_connstr::redact_kv_connection_strings(&text);
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
