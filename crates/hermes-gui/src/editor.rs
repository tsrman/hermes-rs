//! Cursor-aware single/multi-line text buffer for GPUI inputs (unit-testable headless).

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextBuffer {
    text: String,
    /// Byte offset into `text`, always kept on a UTF-8 char boundary.
    cursor: usize,
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

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
        true
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = prev_char_boundary(&self.text, self.cursor);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = next_char_boundary(&self.text, self.cursor);
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Format the text with a visible caret when focused, or the placeholder when empty & unfocused.
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
        buf.move_left();
        buf.move_left();
        buf.move_left();
        buf.insert_str("новый ");
        assert_eq!(buf.as_str(), "Привет новый мир");
        buf.backspace();
        assert_eq!(buf.as_str(), "Привет новыймир");
        buf.move_home();
        buf.delete();
        assert_eq!(buf.as_str(), "ривет новыймир");
        buf.move_end();
        buf.insert_char('!');
        assert_eq!(buf.as_str(), "ривет новыймир!");
        assert!(buf.display(true, "hint").ends_with("!▏"));
        assert_eq!(buf.display(false, "hint"), "ривет новыймир!");
        buf.clear();
        assert_eq!(buf.display(false, "hint"), "hint");
        assert_eq!(buf.display(true, "hint"), "▏ hint");
    }
}
