//! How a secret is shown once it is masked.

use std::sync::LazyLock;

use regex::Regex;

static DISPLAY_CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\x00-\x1f\x7f\x{80}-\x{9f}\x{200b}-\x{200f}\x{202a}-\x{202e}\x{2060}-\x{2064}]")
        .expect("valid pattern")
});

/// Keep `head` and `tail` characters around `...`; a value shorter than `floor` becomes the
/// placeholder. Every control character is dropped first, so a masked secret never emits
/// multiline or invisible bytes.
pub fn mask_secret(
    value: &str,
    head: usize,
    tail: usize,
    floor: usize,
    placeholder: &str,
) -> String {
    let value = DISPLAY_CONTROL.replace_all(value, "");
    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() < floor {
        return placeholder.to_owned();
    }
    let first: String = chars[..head].iter().collect();
    let last: String = chars[chars.len() - tail..].iter().collect();
    format!("{first}...{last}")
}

thread_local! {
    static NONREUSABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `body` with credential masks switched to the non-reusable sentinel when `on`: file content
/// shown to the agent must not carry a head/tail mask it could write back as a dead credential.
pub(super) fn with_nonreusable<T>(on: bool, body: impl FnOnce() -> T) -> T {
    let previous = NONREUSABLE.with(|flag| flag.replace(on));
    let out = body();
    NONREUSABLE.with(|flag| flag.set(previous));
    out
}

/// `«redacted:<vendor prefix>…»`: no head or tail characters, only the kind of credential.
fn mask_token_nonreusable(token: &str) -> String {
    let label = if token.is_empty() {
        None
    } else {
        super::prefix::literal_prefixes()
            .iter()
            .find(|prefix| token.starts_with(prefix.as_str()))
    };
    match label {
        Some(label) => format!("«redacted:{label}…»"),
        None => "«redacted-secret»".into(),
    }
}

/// Mask a log token: an 18-character floor, six characters kept in front and four behind.
pub fn mask_token(token: &str) -> String {
    if NONREUSABLE.with(std::cell::Cell::get) {
        return mask_token_nonreusable(token);
    }
    if token.is_empty() {
        return "***".into();
    }
    mask_secret(token, 6, 4, 18, "***")
}
