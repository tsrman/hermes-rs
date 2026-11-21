//! Redaction of credentials in text that reaches the model or another session.
//!
//! A port of `agent/redact.py`, built one pass at a time against a corpus recorded from the
//! reference (`compat/create_redaction_corpus.py`). Only the passes listed in `PORTED` exist
//! so far; the rest of `redact_credential_shapes` is not applied yet, so this is not a
//! complete redactor and must not be wired into a safety boundary until `PENDING` is empty.

mod mask;
mod prefix;
mod private_keys;
mod tokens;

pub use mask::{mask_secret, mask_token};

/// The passes of the reference's `redact_credential_shapes` that this module applies, in order.
pub const PORTED: [&str; 10] = [
    "private_key",
    "prefix",
    "auth_header",
    "telegram",
    "db_connstr",
    "webhook",
    "jwt",
    "webapp_link",
    "phone",
    "plain",
];

/// Redact credential shapes in `text` (the reference's `force=True` path, so no setting can
/// switch it off). Passes that are not ported yet leave the text as it is.
pub fn redact_sensitive_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let text = private_keys::redact(text);
    let text = prefix::redact(&text);
    let text = tokens::redact_auth_headers(&text);
    let text = tokens::redact_telegram(&text);
    let text = tokens::redact_db_urls(&text);
    let text = tokens::redact_webhooks(&text);
    let text = tokens::redact_jwt(&text);
    let text = tokens::redact_webapp_links(&text);
    tokens::redact_phone_numbers(&text)
}
