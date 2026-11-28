use std::collections::HashMap;
use std::time::Duration;

use gpui::{div, prelude::*, px, rgb, HighlightStyle, Rgba, SharedString, StyledText, TextLayout};

use crate::editor::TextBuffer;
use crate::setup_state::SetupField;

/// Shared color & caret style for single/multiline GPUI text fields.
#[derive(Clone, Copy)]
pub(crate) struct TextFieldStyle {
    pub(crate) text_main: Rgba,
    pub(crate) text_dim: Rgba,
    pub(crate) caret: Rgba,
    pub(crate) caret_h: f32,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum InputTarget {
    Composer,
    Search,
    WorkspacePath,
    Clarify,
    Setup(SetupField),
}

#[derive(Clone)]
pub(crate) struct InputLayoutSnapshot {
    pub(crate) layout: TextLayout,
    /// Byte offset inside the rendered string where the synthetic `|` caret was inserted, if any.
    pub(crate) caret_inserted_at: Option<usize>,
    /// True when each source character was rendered as `•`.
    pub(crate) masked: bool,
}

pub(crate) fn shortcut_letter(key: &str, key_char: Option<&str>) -> Option<char> {
    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if ch.is_ascii_alphabetic() {
            return Some(ch.to_ascii_lowercase());
        }
        let mapped = match ch {
            'ф' | 'Ф' => Some('a'),
            'с' | 'С' => Some('c'),
            'м' | 'М' => Some('v'),
            'ч' | 'Ч' => Some('x'),
            'г' | 'Г' => Some('u'),
            'з' | 'З' => Some('p'),
            'й' | 'Й' => Some('q'),
            'я' | 'Я' => Some('z'),
            _ => None,
        };
        if mapped.is_some() {
            return mapped;
        }
    }
    if let Some(s) = key_char {
        let mut kc = s.chars();
        if let (Some(ch), None) = (kc.next(), kc.next()) {
            let mapped = match ch {
                '\x01' => Some('a'),
                '\x03' => Some('c'),
                '\x10' => Some('p'),
                '\x11' => Some('q'),
                '\x15' => Some('u'),
                '\x16' => Some('v'),
                '\x18' => Some('x'),
                '\x1a' => Some('z'),
                _ => None,
            };
            if mapped.is_some() {
                return mapped;
            }
        }
    }
    None
}

pub(crate) fn format_quote_insertion(existing: &str, fragment: &str) -> Option<String> {
    let trimmed = fragment.trim();
    if trimmed.is_empty() {
        return None;
    }
    let quoted = trimmed
        .lines()
        .map(|line| {
            let clean = line.trim_end();
            if clean.is_empty() {
                ">".to_owned()
            } else {
                format!("> {clean}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prefix = if existing.is_empty() || existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    Some(format!("{prefix}{quoted}\n\n"))
}

pub(crate) fn format_selection_preview(fragment: &str, max_chars: usize) -> String {
    let collapsed: String = fragment.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Extract insertable text from a GPUI keystroke.
///
/// On Windows, `gpui 0.2.2` maps `VK_SPACE` via `parse_immutable` to `key = "space"` with
/// `key_char = None`, so `"space"` must be handled explicitly alongside `key_char`.
pub(crate) fn text_from_keystroke<'a>(
    key: &'a str,
    key_char: Option<&'a str>,
    control: bool,
    alt: bool,
    platform: bool,
) -> Option<&'a str> {
    if control || alt || platform {
        return None;
    }
    if key == "space" {
        return Some(" ");
    }
    if let Some(s) = key_char.filter(|s| !s.is_empty() && !s.chars().all(char::is_control)) {
        return Some(s);
    }
    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if !ch.is_control() {
            return Some(key);
        }
    }
    None
}

struct TextFieldData<'a> {
    full_text: &'a str,
    cursor: usize,
    selection: Option<std::ops::Range<usize>>,
    focused: bool,
    masked: bool,
    placeholder: &'a str,
}

fn render_text_field_content(
    data: TextFieldData<'_>,
    style: TextFieldStyle,
) -> (gpui::AnyElement, Option<InputLayoutSnapshot>) {
    let TextFieldData {
        full_text,
        cursor,
        selection,
        focused,
        masked,
        placeholder,
    } = data;
    let TextFieldStyle {
        text_main,
        text_dim,
        caret,
        caret_h,
    } = style;
    if full_text.is_empty() {
        if focused {
            return (
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .w(px(2.0))
                            .h(px(caret_h))
                            .bg(caret)
                            .rounded_sm()
                            .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_color(text_dim)
                            .child(SharedString::from(placeholder.to_owned())),
                    )
                    .into_any_element(),
                None,
            );
        }
        return (
            div()
                .w_full()
                .min_w(px(0.0))
                .text_color(text_dim)
                .child(SharedString::from(placeholder.to_owned()))
                .into_any_element(),
            None,
        );
    }

    if !focused {
        let styled = StyledText::new(full_text.to_owned());
        let snap = InputLayoutSnapshot {
            layout: styled.layout().clone(),
            caret_inserted_at: None,
            masked,
        };
        return (
            div()
                .w_full()
                .min_w(px(0.0))
                .text_color(text_main)
                .child(styled)
                .into_any_element(),
            Some(snap),
        );
    }

    if let Some(sel) = selection {
        let styled = StyledText::new(full_text.to_owned()).with_highlights([(
            sel,
            HighlightStyle {
                color: Some(rgb(0xffffff).into()),
                background_color: Some(rgb(0x264f78).into()),
                ..Default::default()
            },
        )]);
        let snap = InputLayoutSnapshot {
            layout: styled.layout().clone(),
            caret_inserted_at: None,
            masked,
        };
        return (
            div()
                .w_full()
                .min_w(px(0.0))
                .text_color(text_main)
                .child(styled)
                .into_any_element(),
            Some(snap),
        );
    }

    let split = cursor.min(full_text.len());
    let mut combined = String::with_capacity(full_text.len() + 1);
    combined.push_str(&full_text[..split]);
    let caret_start = combined.len();
    combined.push('|');
    let caret_end = combined.len();
    combined.push_str(&full_text[split..]);
    let styled = StyledText::new(combined).with_highlights([(
        caret_start..caret_end,
        HighlightStyle {
            color: Some(caret.into()),
            background_color: Some(caret.into()),
            ..Default::default()
        },
    )]);
    let snap = InputLayoutSnapshot {
        layout: styled.layout().clone(),
        caret_inserted_at: Some(caret_start),
        masked,
    };
    (
        div()
            .w_full()
            .min_w(px(0.0))
            .text_color(text_main)
            .child(styled)
            .into_any_element(),
        Some(snap),
    )
}

pub(crate) fn render_buffer_input(
    layouts: &mut HashMap<InputTarget, InputLayoutSnapshot>,
    target: InputTarget,
    buf: &TextBuffer,
    focused: bool,
    placeholder: &str,
    style: TextFieldStyle,
) -> gpui::AnyElement {
    let (el, snap) = render_text_field_content(
        TextFieldData {
            full_text: buf.as_str(),
            cursor: buf.cursor(),
            selection: buf.selection_range(),
            focused,
            masked: false,
            placeholder,
        },
        style,
    );
    match snap {
        Some(s) => {
            layouts.insert(target, s);
        }
        None => {
            layouts.remove(&target);
        }
    }
    el
}

pub(crate) fn render_masked_input(
    layouts: &mut HashMap<InputTarget, InputLayoutSnapshot>,
    target: InputTarget,
    buf: &TextBuffer,
    focused: bool,
    placeholder: &str,
    style: TextFieldStyle,
) -> gpui::AnyElement {
    let raw = buf.as_str();
    let bullet_len = '•'.len_utf8();
    let masked_text = "•".repeat(raw.chars().count());
    let cursor_chars = raw[..buf.cursor().min(raw.len())].chars().count();
    let masked_cursor = cursor_chars * bullet_len;
    let masked_sel = buf.selection_range().map(|r| {
        let s = raw[..r.start.min(raw.len())].chars().count() * bullet_len;
        let e = raw[..r.end.min(raw.len())].chars().count() * bullet_len;
        s..e
    });
    let (el, snap) = render_text_field_content(
        TextFieldData {
            full_text: &masked_text,
            cursor: masked_cursor,
            selection: masked_sel,
            focused,
            masked: true,
            placeholder,
        },
        style,
    );
    match snap {
        Some(s) => {
            layouts.insert(target, s);
        }
        None => {
            layouts.remove(&target);
        }
    }
    el
}

pub(crate) fn word_bounds_at(text: &str, idx: usize) -> (usize, usize) {
    let mut pos = idx.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = pos;
    while start > 0 {
        let prev = text[..start]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
        let ch = text[prev..start].chars().next().unwrap_or(' ');
        if is_word(ch) {
            start = prev;
        } else {
            break;
        }
    }
    let mut end = pos;
    while end < text.len() {
        let ch = text[end..].chars().next().unwrap_or(' ');
        if is_word(ch) {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    if start == end && end < text.len() {
        end += text[end..].chars().next().map_or(0, char::len_utf8);
    }
    (start, end)
}

pub(crate) fn format_running_elapsed(dur: Duration) -> String {
    let total_secs = dur.as_secs();
    if total_secs < 60 {
        format!("{total_secs}s")
    } else if total_secs < 3_600 {
        format!("{}m {:02}s", total_secs / 60, total_secs % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            total_secs / 3_600,
            (total_secs % 3_600) / 60,
            total_secs % 60
        )
    }
}
