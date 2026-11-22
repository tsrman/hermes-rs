//! Dangerous-command detection, ported from the reference's `tools/approval_detection.py`.
//!
//! Built bottom-up against corpora recorded from the reference (`compat/create_approval_corpus.py`
//! for verdicts, `compat/create_approval_scan_corpus.py` for the scanner primitives). Text is handled
//! as `char` slices so every offset means what the reference's Python string offsets mean. Only the
//! layers listed here exist so far; there is no verdict function yet, so nothing may rely on this
//! module as a safety boundary.
//!
//! Ported: `budget` (scan work bound), `shell_scan` (quote/escape state machine, substitution ends,
//! `substitution_depth`), `ansi_c` (`$'...'` expansion), `normalize` (detection-time normalization;
//! POSIX home folding only, see its header), `shell_words` (word reader, literal-substitution
//! deobfuscator), `wrappers` (wrapper tables, partial) and `command_starts` (command positions).

pub mod ansi_c;
pub mod budget;
pub mod command_starts;
pub mod normalize;
pub mod shell_scan;
pub mod shell_words;
pub mod wrappers;

/// Python's `str.isspace` / regex `\s` for `str`: Unicode White_Space plus the separators
/// `\x1c`-`\x1f`, which `char::is_whitespace` leaves out.
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}
