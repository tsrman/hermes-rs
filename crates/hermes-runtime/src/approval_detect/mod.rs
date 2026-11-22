//! Dangerous-command detection, ported from the reference's `tools/approval_detection.py`.
//!
//! Built bottom-up against corpora recorded from the reference (`compat/create_approval_corpus.py`
//! for verdicts, `compat/create_approval_scan_corpus.py` for the scanner primitives). Text is handled
//! as `char` slices so every offset means what the reference's Python string offsets mean. Only the
//! layers listed here exist so far; there is no verdict function yet, so nothing may rely on this
//! module as a safety boundary.
//!
//! Ported: `budget` (scan work bound), `shell_scan` (quote/escape state machine, substitution ends,
//! `substitution_depth`), `ansi_c` (`$'...'` expansion).

pub mod ansi_c;
pub mod budget;
pub mod shell_scan;
