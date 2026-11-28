use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::ui::clipboard;

use super::{ActiveModal, FeedPos, FeedSelection, FeedSnapshot, TuiApp, DOUBLE_CLICK_WINDOW};

/// Map a Ctrl/Cmd keystroke to its physical Latin letter across English and Russian (ЙЦУКЕН)
/// layouts so standard shortcuts (`Ctrl+A`, `Ctrl+C`, `Ctrl+V`, `Ctrl+X`, `Ctrl+W`, etc.) work
/// consistently over PuTTY/SSH and local terminals regardless of active keyboard layout.
pub(super) fn ctrl_shortcut_letter(key: KeyEvent) -> Option<char> {
    let m = key.modifiers;
    if !(m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER))
        || m.contains(KeyModifiers::ALT)
    {
        return None;
    }
    let KeyCode::Char(ch) = key.code else {
        return None;
    };
    if ch.is_ascii_alphabetic() {
        return Some(ch.to_ascii_lowercase());
    }
    match ch {
        'ф' | 'Ф' => Some('a'),
        'с' | 'С' => Some('c'),
        'в' | 'В' => Some('d'),
        'у' | 'У' => Some('e'),
        'р' | 'Р' => Some('h'),
        'о' | 'О' => Some('j'),
        'л' | 'Л' => Some('k'),
        'з' | 'З' => Some('p'),
        'г' | 'Г' => Some('u'),
        'м' | 'М' => Some('v'),
        'ц' | 'Ц' => Some('w'),
        'ч' | 'Ч' => Some('x'),
        'я' | 'Я' => Some('z'),
        _ => None,
    }
}

/// True for key combinations that explicitly request clipboard paste when the
/// terminal emulator forwards the keypress directly instead of bracketed paste.
/// Covers `Ctrl+V`, `Ctrl+Shift+V`, Russian-layout `Ctrl+М`, macOS `Cmd+V`, and `Shift+Insert`.
pub(super) fn is_paste_key(key: KeyEvent) -> bool {
    let m = key.modifiers;
    if m.contains(KeyModifiers::ALT) {
        return false;
    }
    match key.code {
        KeyCode::Char('v' | 'V' | 'м' | 'М') => {
            m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER)
        }
        KeyCode::Insert => m.contains(KeyModifiers::SHIFT),
        _ => false,
    }
}

/// True for key combinations that copy an active TUI text selection (`Ctrl+C`, `Ctrl+Shift+C`,
/// Russian `Ctrl+С`, macOS `Cmd+C`, or `Ctrl+Insert`).
pub(super) fn is_copy_selection_key(key: KeyEvent) -> bool {
    let m = key.modifiers;
    if m.contains(KeyModifiers::ALT) {
        return false;
    }
    match key.code {
        KeyCode::Char('c' | 'C' | 'с' | 'С') => {
            m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER)
        }
        KeyCode::Insert => m.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

pub(super) fn toggle_mouse_capture(app: &mut TuiApp) {
    app.mouse_capture = !app.mouse_capture;
    app.clear_selection();
    if app.mouse_capture {
        app.set_copy_status(
            false,
            "TUI mouse enabled (drag to select & copy, wheel to scroll · Alt+M toggle)",
        );
    } else {
        app.set_copy_status(
            false,
            "Native terminal mouse enabled (PgUp/PgDn to scroll · Alt+M restore wheel)",
        );
    }
}

pub(super) fn copy_active_selection(app: &mut TuiApp, clear_after: bool) -> bool {
    let Some(sel) = app.selection else {
        return false;
    };
    let text = {
        let snap = app.feed_snapshot.borrow();
        extract_selected_text(&snap, sel)
    };
    if clear_after {
        app.clear_selection();
    }
    if text.is_empty() {
        return false;
    }
    let chars = text.chars().count();
    match clipboard::write_text(&text) {
        Ok(()) => {
            app.set_copy_status(
                false,
                if clear_after {
                    format!("✓ Copied {chars} chars to clipboard")
                } else {
                    format!("✓ Copied {chars} chars to clipboard · Esc/click clears")
                },
            );
            true
        }
        Err(err) => {
            app.set_copy_status(true, format!("Copy failed: {err}"));
            true
        }
    }
}

pub(super) fn screen_to_feed_pos(snap: &FeedSnapshot, column: u16, row: u16) -> Option<FeedPos> {
    let inner = snap.inner_area;
    if inner.width == 0 || inner.height == 0 || snap.lines.is_empty() {
        return None;
    }
    let clamped_row = row.clamp(inner.y, inner.y + inner.height - 1);
    let clamped_col = column.clamp(inner.x, inner.x + inner.width - 1);
    let rel_row = (clamped_row - inner.y) as usize;
    let rel_col = (clamped_col - inner.x) as usize;
    let line = (snap.scroll_y + rel_row).min(snap.lines.len() - 1);
    Some(FeedPos { line, col: rel_col })
}

pub(super) fn point_in_rect(rect: Rect, column: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && column >= rect.x
        && column < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
}

pub(super) fn select_word_at(snap: &FeedSnapshot, pos: FeedPos) -> Option<FeedSelection> {
    let line = snap.lines.get(pos.line)?;
    let chars: Vec<char> = line.plain.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let idx = pos.col.min(chars.len() - 1);
    let target_ws = chars[idx].is_whitespace();
    let mut start = idx;
    while start > 0 && chars[start - 1].is_whitespace() == target_ws {
        start -= 1;
    }
    let mut end = idx;
    while end + 1 < chars.len() && chars[end + 1].is_whitespace() == target_ws {
        end += 1;
    }
    Some(FeedSelection {
        anchor: FeedPos {
            line: pos.line,
            col: start,
        },
        focus: FeedPos {
            line: pos.line,
            col: end,
        },
        dragging: false,
    })
}

pub(super) fn select_logical_line_at(snap: &FeedSnapshot, pos: FeedPos) -> Option<FeedSelection> {
    if pos.line >= snap.lines.len() {
        return None;
    }
    let mut first = pos.line;
    while first > 0 && snap.lines[first].soft_wrapped {
        first -= 1;
    }
    let mut last = pos.line;
    while last + 1 < snap.lines.len() && snap.lines[last + 1].soft_wrapped {
        last += 1;
    }
    let end_col = snap.lines[last]
        .plain
        .trim_end_matches(' ')
        .chars()
        .count()
        .saturating_sub(1);
    Some(FeedSelection {
        anchor: FeedPos {
            line: first,
            col: 0,
        },
        focus: FeedPos {
            line: last,
            col: end_col,
        },
        dragging: false,
    })
}

pub(super) fn extract_selected_text(snap: &FeedSnapshot, sel: FeedSelection) -> String {
    if snap.lines.is_empty() {
        return String::new();
    }
    let (start, end) = sel.ordered();
    let last_idx = end.line.min(snap.lines.len() - 1);
    if start.line > last_idx {
        return String::new();
    }
    let mut out = String::new();
    for line_idx in start.line..=last_idx {
        let row = &snap.lines[line_idx];
        if line_idx > start.line && !row.soft_wrapped {
            out.push('\n');
        }
        let chars: Vec<char> = row.plain.chars().collect();
        if chars.is_empty() {
            continue;
        }
        let c_start = if line_idx == start.line { start.col } else { 0 };
        if c_start >= chars.len() {
            continue;
        }
        let c_end = if line_idx == last_idx {
            end.col.min(chars.len() - 1)
        } else {
            chars.len() - 1
        };
        if c_start > c_end {
            continue;
        }
        let mut piece: String = chars[c_start..=c_end].iter().collect();
        let next_is_soft =
            line_idx < last_idx && snap.lines.get(line_idx + 1).is_some_and(|l| l.soft_wrapped);
        let reached_line_end = line_idx < last_idx || end.col >= chars.len() - 1;
        if !next_is_soft && reached_line_end {
            while piece.ends_with(' ') {
                piece.pop();
            }
        }
        out.push_str(&piece);
    }
    out
}

/// Handle mouse wheel scrolling and drag-to-select/copy in the conversation feed and modals.
/// Returns `true` when state changed and the frame should be redrawn.
pub(super) fn handle_mouse_event(app: &mut TuiApp, mouse: MouseEvent) -> bool {
    const WHEEL_LINES: usize = 3;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if app.modal.is_none() => {
            let (in_feed, pos, snap_word, snap_line) = {
                let snap = app.feed_snapshot.borrow();
                let in_feed = point_in_rect(snap.inner_area, mouse.column, mouse.row);
                let pos = in_feed
                    .then(|| screen_to_feed_pos(&snap, mouse.column, mouse.row))
                    .flatten();
                let word = pos.and_then(|p| select_word_at(&snap, p));
                let full_line = pos.and_then(|p| select_logical_line_at(&snap, p));
                (in_feed, pos, word, full_line)
            };
            if !in_feed {
                app.last_click = None;
                let had = app.selection.take().is_some();
                return had;
            }
            let Some(pos) = pos else {
                app.last_click = None;
                return app.selection.take().is_some();
            };
            let now = Instant::now();
            let click_count = match app.last_click {
                Some((prev_t, prev_pos, prev_count))
                    if now.duration_since(prev_t) <= DOUBLE_CLICK_WINDOW && prev_pos == pos =>
                {
                    (prev_count % 3) + 1
                }
                _ => 1,
            };
            app.last_click = Some((now, pos, click_count));
            if click_count == 2 {
                app.selection = snap_word;
                copy_active_selection(app, false);
                return true;
            }
            if click_count == 3 {
                app.selection = snap_line;
                copy_active_selection(app, false);
                return true;
            }
            app.selection = Some(FeedSelection {
                anchor: pos,
                focus: pos,
                dragging: true,
            });
            true
        }
        MouseEventKind::Drag(MouseButton::Left) if app.modal.is_none() => {
            let Some(mut sel) = app.selection else {
                return false;
            };
            if !sel.dragging {
                return false;
            }
            let inner = app.feed_snapshot.borrow().inner_area;
            if inner.height > 0 {
                if mouse.row < inner.y {
                    app.scroll_feed_up(1);
                } else if mouse.row >= inner.y + inner.height {
                    app.scroll_feed_down(1);
                }
            }
            let new_pos = {
                let snap = app.feed_snapshot.borrow();
                let max_scroll = app.last_max_scroll.get();
                let scroll_y = max_scroll.saturating_sub(app.scroll_from_bottom.min(max_scroll));
                if snap.inner_area.width == 0
                    || snap.inner_area.height == 0
                    || snap.lines.is_empty()
                {
                    None
                } else {
                    let clamped_row = mouse.row.clamp(
                        snap.inner_area.y,
                        snap.inner_area.y + snap.inner_area.height - 1,
                    );
                    let clamped_col = mouse.column.clamp(
                        snap.inner_area.x,
                        snap.inner_area.x + snap.inner_area.width - 1,
                    );
                    let rel_row = (clamped_row - snap.inner_area.y) as usize;
                    let rel_col = (clamped_col - snap.inner_area.x) as usize;
                    let line = (scroll_y + rel_row).min(snap.lines.len() - 1);
                    Some(FeedPos { line, col: rel_col })
                }
            };
            let Some(new_pos) = new_pos else {
                return false;
            };
            let changed = sel.focus != new_pos;
            if new_pos != sel.anchor {
                app.last_click = None;
            }
            sel.focus = new_pos;
            app.selection = Some(sel);
            changed
        }
        MouseEventKind::Up(MouseButton::Left) if app.modal.is_none() => {
            let Some(mut sel) = app.selection else {
                return false;
            };
            if !sel.dragging {
                return false;
            }
            sel.dragging = false;
            if sel.anchor == sel.focus {
                app.selection = None;
                return true;
            }
            app.selection = Some(sel);
            copy_active_selection(app, false);
            true
        }
        MouseEventKind::Down(MouseButton::Right) if app.modal.is_none() => {
            if app.selection.is_some() {
                copy_active_selection(app, true);
                true
            } else {
                false
            }
        }
        MouseEventKind::ScrollUp => match &mut app.modal {
            Some(ActiveModal::Approval { scroll, .. }) => {
                let before = *scroll;
                *scroll = scroll.saturating_sub(WHEEL_LINES as u16);
                *scroll != before
            }
            Some(ActiveModal::SessionPicker { cursor, scroll, .. }) => {
                let before = *cursor;
                *cursor = cursor.saturating_sub(1);
                if *cursor < *scroll {
                    *scroll = *cursor;
                }
                *cursor != before
            }
            Some(ActiveModal::Clarify {
                questions,
                current,
                cursor,
                ..
            }) => {
                if questions
                    .get(*current)
                    .is_some_and(|q| !q.choices.is_empty())
                {
                    let before = *cursor;
                    *cursor = cursor.saturating_sub(1);
                    *cursor != before
                } else {
                    false
                }
            }
            None => {
                let before = app.scroll_from_bottom;
                app.scroll_feed_up(WHEEL_LINES);
                app.scroll_from_bottom != before
            }
        },
        MouseEventKind::ScrollDown => match &mut app.modal {
            Some(ActiveModal::Approval { scroll, .. }) => {
                let before = *scroll;
                *scroll = scroll.saturating_add(WHEEL_LINES as u16);
                *scroll != before
            }
            Some(ActiveModal::SessionPicker {
                sessions, cursor, ..
            }) => {
                let before = *cursor;
                if *cursor + 1 < sessions.len() {
                    *cursor += 1;
                }
                *cursor != before
            }
            Some(ActiveModal::Clarify {
                questions,
                current,
                cursor,
                ..
            }) => {
                if let Some(q) = questions.get(*current) {
                    let before = *cursor;
                    if *cursor + 1 < q.choices.len() {
                        *cursor += 1;
                    }
                    *cursor != before
                } else {
                    false
                }
            }
            None => {
                let before = app.scroll_from_bottom;
                app.scroll_feed_down(WHEEL_LINES);
                app.scroll_from_bottom != before
            }
        },
        _ => false,
    }
}
