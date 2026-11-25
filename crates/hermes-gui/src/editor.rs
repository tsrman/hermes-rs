//! Cursor- and selection-aware single/multi-line text buffer for GPUI inputs (unit-testable headless).

use std::ops::Range;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextBuffer {
    text: String,
    /// Byte offset into `text`, always kept on a UTF-8 char boundary.
    cursor: usize,
    /// Optional selection anchor byte offset; when `Some(a)` with `a != cursor`, the range
    /// `min(a, cursor)..max(a, cursor)` is selected.
    anchor: Option<usize>,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selection_range(&self) -> Option<Range<usize>> {
        let a = self.anchor?.min(self.text.len());
        let c = self.cursor.min(self.text.len());
        if a == c {
            None
        } else {
            Some(a.min(c)..a.max(c))
        }
    }

    pub fn selected_text(&self) -> Option<&str> {
        let range = self.selection_range()?;
        self.text.get(range)
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    pub fn select_all(&mut self) {
        if self.text.is_empty() {
            self.anchor = None;
            self.cursor = 0;
        } else {
            self.anchor = Some(0);
            self.cursor = self.text.len();
        }
    }

    pub fn set_cursor(&mut self, byte_offset: usize, extend_selection: bool) {
        let idx = snap_char_boundary(&self.text, byte_offset);
        if extend_selection {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
            self.cursor = idx;
            if self.anchor == Some(self.cursor) {
                self.anchor = None;
            }
        } else {
            self.anchor = None;
            self.cursor = idx;
        }
    }

    pub fn select_word_at(&mut self, byte_offset: usize) {
        if self.text.is_empty() {
            self.clear_selection();
            self.cursor = 0;
            return;
        }
        let mut idx = snap_char_boundary(&self.text, byte_offset);
        if idx >= self.text.len() {
            idx = prev_char_boundary(&self.text, self.text.len());
        }
        let target_ws = self.text[idx..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        let mut start = idx;
        while start > 0 {
            let prev = prev_char_boundary(&self.text, start);
            if self.text[prev..]
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() == target_ws)
            {
                start = prev;
            } else {
                break;
            }
        }
        let mut end = next_char_boundary(&self.text, idx);
        while end < self.text.len() {
            if self.text[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() == target_ws)
            {
                end = next_char_boundary(&self.text, end);
            } else {
                break;
            }
        }
        self.anchor = Some(start);
        self.cursor = end;
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection_range() else {
            self.anchor = None;
            return false;
        };
        self.text.drain(range.clone());
        self.cursor = range.start;
        self.anchor = None;
        true
    }

    /// Return the text slices immediately before and after the current cursor position.
    #[cfg(test)]
    pub fn split_at_cursor(&self) -> (&str, &str) {
        let idx = self.cursor.min(self.text.len());
        (&self.text[..idx], &self.text[idx..])
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = None;
    }

    pub fn replace_range_to_cursor(&mut self, start: usize, replacement: &str) {
        let clamped = start.min(self.cursor);
        if self.text.is_char_boundary(clamped) && self.text.is_char_boundary(self.cursor) {
            self.text.replace_range(clamped..self.cursor, replacement);
            self.cursor = clamped + replacement.len();
            self.anchor = None;
        }
    }

    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.delete_selection();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor >= self.text.len() {
            return false;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
        true
    }

    pub fn move_left(&mut self, extend: bool) {
        if !extend {
            if let Some(range) = self.selection_range() {
                self.cursor = range.start;
                self.anchor = None;
                return;
            }
        }
        if self.cursor > 0 {
            let next = prev_char_boundary(&self.text, self.cursor);
            self.set_cursor(next, extend);
        } else if !extend {
            self.anchor = None;
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        if !extend {
            if let Some(range) = self.selection_range() {
                self.cursor = range.end;
                self.anchor = None;
                return;
            }
        }
        if self.cursor < self.text.len() {
            let next = next_char_boundary(&self.text, self.cursor);
            self.set_cursor(next, extend);
        } else if !extend {
            self.anchor = None;
        }
    }

    pub fn move_home(&mut self, extend: bool) {
        self.set_cursor(0, extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        let len = self.text.len();
        self.set_cursor(len, extend);
    }

    /// Format the text with a visible caret when focused, or the placeholder when empty & unfocused.
    #[cfg(test)]
    pub fn display(&self, focused: bool, placeholder: &str) -> String {
        if self.text.is_empty() {
            return if focused {
                format!("▏ {placeholder}")
            } else {
                placeholder.to_owned()
            };
        }
        if !focused {
            return self.text.clone();
        }
        let mut out = String::with_capacity(self.text.len() + 4);
        out.push_str(&self.text[..self.cursor]);
        out.push('▏');
        out.push_str(&self.text[self.cursor..]);
        out
    }
}

fn snap_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_add(1).min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_utf8_cursor_navigation_and_editing() {
        let mut buf = TextBuffer::new();
        buf.insert_str("Привет мир");
        buf.move_left(false);
        buf.move_left(false);
        buf.move_left(false);
        buf.insert_str("новый ");
        assert_eq!(buf.as_str(), "Привет новый мир");
        buf.backspace();
        assert_eq!(buf.as_str(), "Привет новыймир");
        buf.move_home(false);
        buf.delete();
        assert_eq!(buf.as_str(), "ривет новыймир");
        buf.move_end(false);
        buf.insert_char('!');
        assert_eq!(buf.as_str(), "ривет новыймир!");
        assert_eq!(buf.split_at_cursor(), ("ривет новыймир!", ""));
        buf.move_left(false);
        assert_eq!(buf.split_at_cursor(), ("ривет новыймир", "!"));
        buf.move_end(false);
        assert!(buf.display(true, "hint").ends_with("!▏"));
        assert_eq!(buf.display(false, "hint"), "ривет новыймир!");
        buf.clear();
        assert_eq!(buf.display(false, "hint"), "hint");
        assert_eq!(buf.display(true, "hint"), "▏ hint");
    }

    #[test]
    fn supports_selection_drag_word_select_select_all_and_delete() {
        let mut buf = TextBuffer::new();
        buf.insert_str("hello world test");
        buf.set_cursor(6, false);
        buf.set_cursor(11, true);
        assert_eq!(buf.selected_text(), Some("world"));
        assert!(buf.backspace());
        assert_eq!(buf.as_str(), "hello  test");
        assert_eq!(buf.selected_text(), None);

        buf.select_word_at(8);
        assert_eq!(buf.selected_text(), Some("test"));
        buf.insert_str("done");
        assert_eq!(buf.as_str(), "hello  done");

        buf.select_all();
        assert_eq!(buf.selected_text(), Some("hello  done"));
        assert!(buf.delete());
        assert!(buf.is_empty());
    }
}
