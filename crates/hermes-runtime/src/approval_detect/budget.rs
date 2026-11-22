//! Work bound for the shell scanners (`approval_parser_budget.py`).
//!
//! One detection call may spend a fixed allowance plus a per-character one; running out ends the
//! scan unfinished (`BudgetExceeded`) and the caller treats the command as needing a human.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_SCAN_UNITS: i64 = 4_000_000;
pub const SCAN_UNITS_PER_CHAR: i64 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExceeded;

/// A command as chars with an identity, so memoised scanner results are keyed per text.
#[derive(Debug, Clone)]
pub struct Text {
    pub chars: Vec<char>,
    id: u64,
}

impl Text {
    pub fn new(text: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            chars: text.chars().collect(),
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static REMAINING: Cell<i64> = const { Cell::new(0) };
    static MEMO: RefCell<HashMap<(u8, u64, usize), Option<usize>>> = RefCell::new(HashMap::new());
    static STR_MEMO: RefCell<HashMap<(u8, String), String>> = RefCell::new(HashMap::new());
}

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        ACTIVE.with(|a| a.set(false));
        MEMO.with(|m| m.borrow_mut().clear());
        STR_MEMO.with(|m| m.borrow_mut().clear());
    }
}

/// Bound the scanner work of one detection call on a command of `len` chars. A nested call shares
/// the outer budget.
pub fn with_budget<R>(len: usize, f: impl FnOnce() -> R) -> R {
    if ACTIVE.with(Cell::get) {
        return f();
    }
    ACTIVE.with(|a| a.set(true));
    REMAINING.with(|r| r.set(MAX_SCAN_UNITS + SCAN_UNITS_PER_CHAR * len as i64));
    let _guard = Guard;
    f()
}

pub fn active() -> bool {
    ACTIVE.with(Cell::get)
}

/// Spend `units` of the active budget (no-op outside one).
pub fn charge(units: i64) -> Result<(), BudgetExceeded> {
    if !active() {
        return Ok(());
    }
    REMAINING.with(|r| {
        r.set(r.get() - units);
        if r.get() < 0 {
            Err(BudgetExceeded)
        } else {
            Ok(())
        }
    })
}

/// `compute()` for a pure scanner call, memoised for the active detection call (a plain call
/// outside one). A repeat costs one unit; the work of a fresh call is charged by the scanners.
pub fn memo(
    kind: u8,
    text: &Text,
    start: usize,
    compute: impl FnOnce() -> Result<Option<usize>, BudgetExceeded>,
) -> Result<Option<usize>, BudgetExceeded> {
    if !active() {
        return compute();
    }
    let key = (kind, text.id, start);
    let cached = MEMO.with(|m| m.borrow().get(&key).copied());
    let value = match cached {
        Some(value) => value,
        None => {
            let value = compute()?;
            MEMO.with(|m| m.borrow_mut().insert(key, value));
            value
        }
    };
    charge(1)?;
    Ok(value)
}

/// `compute()` for a pure string-to-string function, memoised like [`memo`].
pub fn memo_string(
    kind: u8,
    key: &str,
    compute: impl FnOnce() -> Result<String, BudgetExceeded>,
) -> Result<String, BudgetExceeded> {
    if !active() {
        return compute();
    }
    let key = (kind, key.to_owned());
    let cached = STR_MEMO.with(|m| m.borrow().get(&key).cloned());
    let value = match cached {
        Some(value) => value,
        None => {
            let value = compute()?;
            STR_MEMO.with(|m| m.borrow_mut().insert(key, value.clone()));
            value
        }
    };
    charge(1)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_is_free_outside_a_budget_and_bounded_inside() {
        assert_eq!(charge(i64::MAX / 2), Ok(()));
        with_budget(10, || {
            assert_eq!(charge(MAX_SCAN_UNITS), Ok(()));
            assert_eq!(charge(SCAN_UNITS_PER_CHAR * 10), Ok(()));
            assert_eq!(charge(1), Err(BudgetExceeded));
        });
        assert!(!active());
    }

    #[test]
    fn a_nested_budget_shares_the_outer_one_and_memo_charges_a_unit_per_hit() {
        let text = Text::new("x");
        with_budget(0, || {
            with_budget(1_000_000, || {
                assert_eq!(charge(MAX_SCAN_UNITS), Ok(()));
                assert_eq!(charge(1), Err(BudgetExceeded));
            });
        });
        with_budget(0, || {
            let mut calls = 0;
            for _ in 0..3 {
                let value = memo(9, &text, 0, || {
                    calls += 1;
                    Ok(Some(7))
                });
                assert_eq!(value, Ok(Some(7)));
            }
            assert_eq!(calls, 1);
        });
    }
}
