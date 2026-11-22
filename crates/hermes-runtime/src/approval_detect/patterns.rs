//! Regex building blocks shared by the hardline and dangerous tables (`_CMDPOS` and friends).
//!
//! Note for every pattern in this module tree: Python's `\s` for `str` also matches `\x1c`-`\x1f`,
//! the `regex` family's does not. The difference only shows on those four control characters.

use std::sync::LazyLock;

/// `_CMD_PATH`: an optional directory prefix of a program path.
pub const CMD_PATH: &str = r"(?:/?+(?:[^\s/)=<>]++/)*+)?+";

/// Start-of-command position: start of string, newline, or a substitution opener, then optional
/// `sudo` / `env VAR=` / `exec|nohup|setsid|time` wrappers, a `busybox`/`toybox` selector and a path.
pub static CMDPOS: LazyLock<String> = LazyLock::new(|| {
    format!(
        r"(?:^|\n(?![^\S\n]*+\n)|`|\$\()\s*+(?:sudo\s++(?:-[^\s]++\s++)*+)?(?:env\s++(?:\w+=\S*+\s++)*+)?(?:(?:exec|nohup|setsid|time)\s++)*+\s*+(?:{CMD_PATH}(?:busybox|toybox)\b\s++)?+{CMD_PATH}"
    )
});

/// `_RM_FLAG_PREFIX`: `rm` as a command word plus its leading flag group.
pub static RM_FLAG_PREFIX: LazyLock<String> =
    LazyLock::new(|| format!(r"{}rm(?:\.exe)?\s+(-[^\s]*\s+)*", *CMDPOS));
