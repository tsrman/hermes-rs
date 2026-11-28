use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use hermes_runtime::turn::ApprovalAction;

use crate::approval;
use crate::ui::markdown::render_markdown_lines;
use crate::ui::{
    build_context_bar, format_context_length, format_duration_ms, format_running_elapsed,
    format_tokens_compact, short_model_label, FeedEntry, NoticeKind, ReplState, ToolStatus,
};

use super::{
    ActiveModal, FeedSnapshot, PaletteEntry, TuiApp, WrappedFeedLine, COPY_STATUS_TTL,
    SPINNER_FRAMES,
};

// ── Pure Ratatui Frame Renderer (directly unit-testable with TestBackend) ───────────

pub(super) struct ComposerLayout {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) cursor_row: usize,
    pub(super) cursor_col: usize,
}

/// Deterministically wrap composer input on explicit newlines and `inner_width` columns so
/// box height, rendered rows, vertical scrolling, selection highlights, and visible cursor
/// coordinates always agree.
pub(super) fn wrap_composer_lines(
    input: &str,
    cursor: usize,
    selection: Option<std::ops::Range<usize>>,
    inner_width: usize,
) -> ComposerLayout {
    let width = inner_width.max(1);
    let clamped_cursor = cursor.min(input.len());
    let sel_style = Style::default().bg(Color::Blue).fg(Color::White);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut row_spans: Vec<Span<'static>> = Vec::new();
    let mut cur_buf = String::new();
    let mut cur_selected = false;
    let mut col: usize = 0;
    let mut cursor_row: usize = 0;
    let mut cursor_col: usize = 0;
    let mut cursor_found = false;

    let flush_row = |lines: &mut Vec<Line<'static>>,
                     row_spans: &mut Vec<Span<'static>>,
                     cur_buf: &mut String,
                     cur_selected: bool| {
        if !cur_buf.is_empty() {
            let text = std::mem::take(cur_buf);
            row_spans.push(if cur_selected {
                Span::styled(text, sel_style)
            } else {
                Span::raw(text)
            });
        }
        lines.push(Line::from(std::mem::take(row_spans)));
    };

    for (byte_idx, ch) in input.char_indices() {
        if !cursor_found && byte_idx >= clamped_cursor {
            cursor_row = lines.len();
            cursor_col = col;
            cursor_found = true;
        }
        if ch == '\n' {
            flush_row(&mut lines, &mut row_spans, &mut cur_buf, cur_selected);
            col = 0;
            continue;
        }
        if col >= width {
            flush_row(&mut lines, &mut row_spans, &mut cur_buf, cur_selected);
            col = 0;
            if byte_idx == clamped_cursor {
                cursor_row = lines.len();
                cursor_col = 0;
            }
        }
        let is_sel = selection
            .as_ref()
            .is_some_and(|r| byte_idx >= r.start && byte_idx < r.end);
        if is_sel != cur_selected && !cur_buf.is_empty() {
            let text = std::mem::take(&mut cur_buf);
            row_spans.push(if cur_selected {
                Span::styled(text, sel_style)
            } else {
                Span::raw(text)
            });
        }
        cur_selected = is_sel;
        cur_buf.push(ch);
        col += 1;
    }

    if !cursor_found {
        if col >= width {
            flush_row(&mut lines, &mut row_spans, &mut cur_buf, cur_selected);
            col = 0;
        }
        cursor_row = lines.len();
        cursor_col = col;
    }
    flush_row(&mut lines, &mut row_spans, &mut cur_buf, cur_selected);

    ComposerLayout {
        lines,
        cursor_row,
        cursor_col,
    }
}

pub fn render_frame(frame: &mut Frame<'_>, app: &TuiApp, state: &ReplState) {
    let area = frame.area();
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let max_composer_lines = (area.height / 3).clamp(3, 8);
    let layout = wrap_composer_lines(&app.input, app.cursor, app.input_selection(), inner_width);
    let composer_lines = (layout.lines.len().max(1) as u16).clamp(1, max_composer_lines);
    let composer_height = composer_lines + 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, chunks[0], state);
    render_feed(frame, chunks[1], app, state);
    render_composer(frame, chunks[2], app, layout);
    render_footer(frame, chunks[3], app);

    // Floating slash-command / `@` context palette above the composer
    if let Some((title, palette)) = app.completion_palette() {
        render_palette(frame, chunks[1], title, &palette, app.palette_index);
    }

    // Centered modal for Approval, Clarify, or SessionPicker
    if let Some(modal) = &app.modal {
        render_modal(frame, area, modal, state.session.as_deref());
    }
}

pub(super) fn context_usage_style(percent: u8) -> Style {
    if percent >= 95 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if percent > 80 {
        Style::default().fg(Color::LightRed)
    } else if percent >= 50 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    }
}

pub(super) fn render_header(frame: &mut Frame<'_>, area: Rect, state: &ReplState) {
    let model_raw = state.request.model().unwrap_or("(unset)");
    let model = short_model_label(model_raw);
    let sess = state
        .session
        .as_deref()
        .map(|s| s.chars().take(10).collect::<String>())
        .unwrap_or_else(|| "new".to_owned());
    let yolo = hermes_runtime::approval_policy::installed().is_some_and(|p| p.approves_all());

    let (ctx_len, _is_fallback) = state.request.display_context_length();
    let ctx_used = state.context_tokens.min(ctx_len);
    let pct = ((ctx_used as f64 / ctx_len.max(1) as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8;
    let mark = if state.context_estimated && ctx_used > 0 {
        "~"
    } else {
        ""
    };
    let ctx_style = context_usage_style(pct);
    let pct_label = format!("{mark}{pct}%");
    let wide = area.width >= 76;
    let narrow = area.width < 52;
    let sep = if wide { " │ " } else { " · " };

    let mut spans = vec![
        Span::styled(
            " HERMES ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled("☤ ", Style::default().fg(Color::Cyan)),
        Span::styled(
            model,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    if wide {
        let ctx_detail = format!(
            "{mark}{}/{}",
            format_tokens_compact(ctx_used),
            format_context_length(ctx_len)
        );
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(ctx_detail, Style::default().fg(Color::Gray)));
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(build_context_bar(pct, 10), ctx_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pct_label, ctx_style));
    } else if !narrow {
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(build_context_bar(pct, 10), ctx_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pct_label, ctx_style));
    } else {
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(pct_label, ctx_style));
    }

    if state.compressions > 0 && !narrow {
        let comp_style = if state.compressions >= 10 {
            Style::default().fg(Color::LightRed)
        } else if state.compressions >= 5 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("🗜 {}", state.compressions),
            comp_style,
        ));
    }

    if yolo {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            " YOLO ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if wide {
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            &state.profile_label,
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("sess:{sess}"),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let header = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black));
    frame.render_widget(header, area);
}

pub(super) fn render_feed(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, state: &ReplState) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let spinner_glyph = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];

    for entry in &state.feed_entries {
        match entry {
            FeedEntry::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(Color::Cyan)),
                    Span::styled(
                        "You",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                for l in text.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(Color::Cyan)),
                        Span::raw(l.to_owned()),
                    ]));
                }
                lines.push(Line::from(""));
            }
            FeedEntry::Assistant(text) => {
                if lines.last().is_some_and(|l| !l.spans.is_empty()) {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(Span::styled(
                    "Hermes",
                    Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD),
                )));
                let cleaned = text.trim_start_matches(['\r', '\n']);
                lines.extend(render_markdown_lines(cleaned, state.color));
                lines.push(Line::from(""));
            }
            FeedEntry::Tool {
                name,
                detail,
                status,
                summary,
                elapsed_ms,
            } => {
                let (icon, icon_color) = match status {
                    ToolStatus::Running => (spinner_glyph, Color::Cyan),
                    ToolStatus::Succeeded => ("✓", Color::Green),
                    ToolStatus::Failed => ("✗", Color::Red),
                };
                let clean_detail = detail
                    .as_deref()
                    .map(approval::sanitize_terminal_text)
                    .filter(|s| !s.is_empty());
                let clean_summary = summary
                    .as_deref()
                    .map(approval::sanitize_terminal_text)
                    .filter(|s| !s.is_empty());
                // Split `exit <code> · <err>` so the header stays compact and any error message
                // sits cleanly on a secondary indented line underneath the card.
                let (inline_status, sub_note) = match (status, clean_summary) {
                    (ToolStatus::Running, _) => (Some("running…".to_owned()), None),
                    (ToolStatus::Failed, Some(s)) => {
                        if let Some((head, tail)) = s.split_once(" · ") {
                            (Some(head.to_owned()), Some(tail.to_owned()))
                        } else if s.starts_with("exit ") {
                            (Some(s), None)
                        } else {
                            (Some("failed".to_owned()), Some(s))
                        }
                    }
                    (ToolStatus::Failed, None) => (Some("failed".to_owned()), None),
                    (ToolStatus::Succeeded, Some(s)) => (Some(s), None),
                    (ToolStatus::Succeeded, None) => (None, None),
                };
                let dur = match status {
                    ToolStatus::Running => app
                        .active_tools
                        .get(name)
                        .map(|started| format_running_elapsed(started.elapsed())),
                    _ => elapsed_ms.map(format_duration_ms),
                };
                let meta = match (inline_status, dur) {
                    (Some(st), Some(d)) => format!("{st} · {d}"),
                    (Some(st), None) => st,
                    (None, Some(d)) => d,
                    (None, None) => "ok".to_owned(),
                };
                let mut card_spans = vec![
                    Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        icon,
                        Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                if let Some(d) = clean_detail {
                    card_spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                    card_spans.push(Span::styled(d, Style::default().fg(Color::Gray)));
                }
                card_spans.push(Span::styled(
                    format!(" ({meta})"),
                    Style::default().fg(if matches!(status, ToolStatus::Failed) {
                        Color::LightRed
                    } else {
                        Color::DarkGray
                    }),
                ));
                lines.push(Line::from(card_spans));
                if let Some(note) = sub_note {
                    lines.push(Line::from(vec![
                        Span::styled("  │   ↳ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(note, Style::default().fg(Color::Red)),
                    ]));
                }
                if matches!(status, ToolStatus::Running) {
                    if let Some((buf_lines, partial)) = app.active_tool_output.get(name) {
                        for preview_line in crate::ui::preview_tail_lines(buf_lines, partial, 3) {
                            lines.push(Line::from(vec![
                                Span::styled("  │   ┆ ", Style::default().fg(Color::DarkGray)),
                                Span::styled(preview_line, Style::default().fg(Color::Gray)),
                            ]));
                        }
                    }
                }
            }
            FeedEntry::Notice { kind, text } => {
                let (prefix, style) = match kind {
                    NoticeKind::Info => ("· ", Style::default().fg(Color::DarkGray)),
                    NoticeKind::Warn => ("⚠ ", Style::default().fg(Color::Yellow)),
                    NoticeKind::Error => ("✗ ", Style::default().fg(Color::Red)),
                };
                for (i, l) in text.lines().enumerate() {
                    lines.push(Line::from(Span::styled(
                        format!("{}{l}", if i == 0 { prefix } else { "  " }),
                        style,
                    )));
                }
            }
        }
    }

    let (inline_think_lines, visible_stream) = if !app.streaming_assistant.is_empty() {
        crate::ui::split_streaming_think(&app.streaming_assistant, 3)
    } else {
        (Vec::new(), "")
    };
    let reasoning_preview = if !inline_think_lines.is_empty() {
        inline_think_lines
    } else {
        crate::ui::preview_tail_lines(
            &app.streaming_reasoning_lines,
            &app.streaming_reasoning_partial,
            3,
        )
    };

    if !visible_stream.is_empty() {
        if lines.last().is_some_and(|l| !l.spans.is_empty()) {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(
                "Hermes ",
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(spinner_glyph, Style::default().fg(Color::Cyan)),
        ]));
        let cleaned = visible_stream.trim_start_matches(['\r', '\n']);
        lines.extend(render_markdown_lines(cleaned, state.color));
    } else if app.is_busy() && app.active_tools.is_empty() {
        if lines.last().is_some_and(|l| !l.spans.is_empty()) {
            lines.push(Line::from(""));
        }
        let busy_label = app.background_status.as_deref().unwrap_or("Thinking…");
        lines.push(Line::from(vec![
            Span::styled(spinner_glyph, Style::default().fg(Color::Cyan)),
            Span::styled(
                format!(" {busy_label}"),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        for thought_line in reasoning_preview {
            lines.push(Line::from(vec![
                Span::styled("  ┆ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    thought_line,
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
    }

    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let inner_height = inner.height as usize;
    let inner_width = inner.width.max(1) as usize;
    let wrapped = wrap_styled_lines(&lines, inner_width);
    let total_visual_lines = wrapped.len();
    let max_scroll = total_visual_lines.saturating_sub(inner_height);
    app.last_max_scroll.set(max_scroll);
    let clamped_from_bottom = app.scroll_from_bottom.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(clamped_from_bottom);

    let title = if clamped_from_bottom > 0 {
        " Conversation [scrolled · Wheel/PgDn or Ctrl+End ↓] "
    } else {
        " Conversation "
    };

    let visible_end = (scroll_y + inner_height).min(wrapped.len());
    let visible_lines: Vec<Line<'static>> = wrapped[scroll_y..visible_end]
        .iter()
        .map(|row| row.line.clone())
        .collect();

    let feed = Paragraph::new(visible_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(title, Style::default().fg(Color::Cyan))),
    );
    frame.render_widget(feed, area);

    if let Some(sel) = app.selection {
        if !(sel.dragging && sel.anchor == sel.focus) && inner.width > 0 && inner.height > 0 {
            let (start, end) = sel.ordered();
            let buf = frame.buffer_mut();
            let sel_style = if state.color {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default().add_modifier(Modifier::REVERSED)
            };
            for vis_row in 0..inner.height as usize {
                let abs_line = scroll_y + vis_row;
                if abs_line < start.line || abs_line > end.line || abs_line >= wrapped.len() {
                    continue;
                }
                let line_len = wrapped[abs_line].plain.chars().count();
                let max_col = (inner.width as usize).saturating_sub(1);
                let c_start = if abs_line == start.line {
                    start.col.min(max_col)
                } else {
                    0
                };
                let c_end = if abs_line == end.line {
                    end.col.min(max_col)
                } else {
                    line_len.max(1).saturating_sub(1).min(max_col)
                };
                if c_start > c_end {
                    continue;
                }
                let y = inner.y + vis_row as u16;
                for c in c_start..=c_end {
                    let x = inner.x + c as u16;
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(sel_style);
                    }
                }
            }
        }
    }

    *app.feed_snapshot.borrow_mut() = FeedSnapshot {
        inner_area: inner,
        scroll_y,
        lines: wrapped,
    };
}

fn build_wrapped_row(chars: &[(char, Style)], soft_wrapped: bool) -> WrappedFeedLine {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::with_capacity(chars.len());
    let mut cur_style: Option<Style> = None;
    let mut buf = String::new();
    for &(ch, style) in chars {
        plain.push(ch);
        if cur_style == Some(style) {
            buf.push(ch);
        } else {
            if !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    cur_style.unwrap_or_default(),
                ));
            }
            cur_style = Some(style);
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur_style.unwrap_or_default()));
    }
    WrappedFeedLine {
        line: Line::from(spans),
        plain,
        soft_wrapped,
    }
}

pub(super) fn wrap_styled_lines(lines: &[Line<'static>], width: usize) -> Vec<WrappedFeedLine> {
    let max_w = width.max(1);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut flat: Vec<(char, Style)> = Vec::new();
        for span in &line.spans {
            let style = span.style;
            for ch in span.content.chars() {
                flat.push((ch, style));
            }
        }
        if flat.is_empty() {
            out.push(WrappedFeedLine {
                line: Line::from(""),
                plain: String::new(),
                soft_wrapped: false,
            });
            continue;
        }
        let mut row: Vec<(char, Style)> = Vec::new();
        let mut soft = false;
        let mut i = 0usize;
        while i < flat.len() {
            if flat[i].0.is_whitespace() {
                let item = flat[i];
                i += 1;
                if row.len() >= max_w {
                    out.push(build_wrapped_row(&row, soft));
                    row.clear();
                    soft = true;
                }
                row.push(item);
            } else {
                let start = i;
                while i < flat.len() && !flat[i].0.is_whitespace() {
                    i += 1;
                }
                let word = &flat[start..i];
                if !row.is_empty() && row.len() + word.len() > max_w {
                    out.push(build_wrapped_row(&row, soft));
                    row.clear();
                    soft = true;
                }
                for &item in word {
                    if row.len() >= max_w {
                        out.push(build_wrapped_row(&row, soft));
                        row.clear();
                        soft = true;
                    }
                    row.push(item);
                }
            }
        }
        if !row.is_empty() {
            out.push(build_wrapped_row(&row, soft));
        }
    }
    out
}

pub(super) fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    layout: ComposerLayout,
) {
    let title = if let Some(started) = app.turn_started {
        let glyph = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
        let secs = started.elapsed().as_secs_f64();
        Span::styled(
            format!(" {glyph} Running ({secs:.1}s) · Ctrl-C to stop "),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::styled(
            " › Message (/ for commands) ",
            Style::default().fg(Color::Cyan),
        )
    };

    let border_color = if app.is_busy() {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    let total_rows = layout.lines.len().max(1);
    let scroll_y = if layout.cursor_row >= visible_rows {
        (layout.cursor_row + 1 - visible_rows).min(total_rows.saturating_sub(visible_rows))
    } else {
        0
    };

    let composer = if app.input.is_empty() && !app.is_busy() {
        Paragraph::new(Line::from(Span::styled(
            "Type a request or /help…",
            Style::default().fg(Color::DarkGray),
        )))
    } else {
        Paragraph::new(layout.lines).scroll((scroll_y as u16, 0))
    }
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title),
    );
    frame.render_widget(composer, area);

    // Position visible terminal cursor inside the composer when idle and no modal is active.
    if !app.is_busy() && app.modal.is_none() {
        let max_col = area.width.saturating_sub(3);
        let max_row = area.height.saturating_sub(3);
        let cx = area.x + 1 + (layout.cursor_col as u16).min(max_col);
        let cy = area.y + 1 + (layout.cursor_row.saturating_sub(scroll_y) as u16).min(max_row);
        frame.set_cursor_position((cx, cy));
    }
}

pub(super) fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    if let Some((when, is_err, msg)) = &app.copy_status {
        if when.elapsed() < COPY_STATUS_TTL {
            let style = if *is_err {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Green)
            };
            let footer = Paragraph::new(Span::styled(format!(" {msg}"), style));
            frame.render_widget(footer, area);
            return;
        }
    }
    let text = match &app.modal {
        Some(ActiveModal::SessionPicker { .. }) => {
            " ↑/↓ select session · Enter resume · Esc cancel"
        }
        Some(_) => " Modal active · follow on-screen keys · Ctrl-C cancel turn",
        None if app.selection.is_some() => {
            " Selection active (copied) · Ctrl-C / Right-click copy · Esc or click clears"
        }
        None if !app.mouse_capture => {
            " Native terminal mouse active · PgUp/PgDn scroll · Alt+M restore wheel · /copy"
        }
        None if app.is_busy() => " Ctrl-C stop turn · Drag select & copy · PgUp/PgDn scroll",
        None => {
            " Enter send · Shift+Enter newline · Drag copy · /copy · Wheel/PgUp scroll · Alt+M mouse · Ctrl-D quit"
        }
    };
    let footer = Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, area);
}

pub(super) fn render_palette(
    frame: &mut Frame<'_>,
    feed_area: Rect,
    title: &str,
    palette: &[PaletteEntry],
    selected: usize,
) {
    if palette.is_empty() {
        return;
    }
    let visible_len = palette.len().min(8);
    let sel = selected.min(palette.len() - 1);
    let start = sel.saturating_add(1).saturating_sub(visible_len);
    let end = (start + visible_len).min(palette.len());
    let height = visible_len as u16 + 2;
    let width = feed_area.width.min(68);
    let rect = Rect {
        x: feed_area.x + 1,
        y: feed_area.y + feed_area.height.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let lines: Vec<Line<'static>> = palette[start..end]
        .iter()
        .enumerate()
        .map(|(offset, cmd)| {
            let is_sel = start + offset == sel;
            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(
                format!(" {:<22} {}", cmd.usage, cmd.description),
                style,
            ))
        })
        .collect();
    let header_title = if palette.len() > visible_len {
        format!("{} ({}/{}) ", title.trim_end(), sel + 1, palette.len())
    } else {
        title.to_owned()
    };
    let popup = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(header_title, Style::default().fg(Color::Cyan))),
    );
    frame.render_widget(popup, rect);
}

pub(super) fn render_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    modal: &ActiveModal,
    active_session: Option<&str>,
) {
    let rect = centered_rect(82, 72, area);
    frame.render_widget(Clear, rect);

    match modal {
        ActiveModal::Approval {
            request,
            reason,
            scroll,
            ..
        } => {
            let action = match request.action {
                ApprovalAction::CreateFile => "Create file",
                ApprovalAction::ReplaceFile => "Replace file",
                ApprovalAction::RunCommand => "Run command",
                ApprovalAction::RunCode => "Run Python code",
                ApprovalAction::ManageSkills => "Change skills",
                ApprovalAction::Clarify => "Clarify",
            };
            let is_command = matches!(request.action, ApprovalAction::RunCommand);
            let keys_hint = if is_command {
                "[y] Approve once   [s] Session   [a] Always (allowlist)   [n / Esc] Deny"
            } else {
                "[y] Approve once   [s] Session   [a] Always (config.yaml)   [n / Esc] Deny"
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " Approval Required (↑/↓ scroll) ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);

            let mut header_lines = vec![
                Line::from(vec![
                    Span::styled("Action: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        action,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Target: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        approval::sanitize_terminal_text(&request.target),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
            ];
            if let Some(why) = reason {
                header_lines.push(Line::from(vec![
                    Span::styled("Flagged: ", Style::default().fg(Color::Red)),
                    Span::styled(
                        approval::sanitize_terminal_text(why),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            }
            header_lines.push(Line::from(Span::styled(
                keys_hint,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            header_lines.push(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(Color::DarkGray),
            )));
            let header_height = (header_lines.len() as u16).min(inner.height);
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(header_height), Constraint::Min(0)])
                .split(inner);

            let header = Paragraph::new(header_lines).wrap(Wrap { trim: false });
            frame.render_widget(header, sections[0]);

            let preview = approval::sanitize_terminal_text(&request.content);
            let preview_lines: Vec<Line<'static>> = preview
                .lines()
                .take(200)
                .map(|l| {
                    Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(Color::White),
                    ))
                })
                .collect();
            let body = Paragraph::new(preview_lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0));
            frame.render_widget(body, sections[1]);
        }
        ActiveModal::Clarify {
            questions,
            current,
            cursor,
            selected,
            custom,
            ..
        } => {
            let q = &questions[*current];
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("Question {} of {}", current + 1, questions.len()),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    q.prompt.clone(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];
            for (idx, choice) in q.choices.iter().enumerate() {
                let is_cur = idx == *cursor;
                let mark = if q.multi_select {
                    if selected.contains(&idx) {
                        "[x]"
                    } else {
                        "[ ]"
                    }
                } else if is_cur {
                    "▸"
                } else {
                    " "
                };
                let style = if is_cur {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(Span::styled(
                    format!(" {mark} {}) {choice}", idx + 1),
                    style,
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Custom answer: ", Style::default().fg(Color::DarkGray)),
                Span::styled(custom.clone(), Style::default().fg(Color::Yellow)),
            ]));
            lines.push(Line::from(""));
            let hint = if q.multi_select {
                "↑/↓ move · Space toggle · type custom · Enter submit · Esc cancel"
            } else {
                "↑/↓ choose · type custom · Enter submit · Esc cancel"
            };
            lines.push(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::Cyan),
            )));

            let widget = Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(Span::styled(
                            " Clarification ",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        )),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(widget, rect);
        }
        ActiveModal::SessionPicker {
            sessions,
            cursor,
            scroll,
        } => {
            let visible_rows = rect.height.saturating_sub(5).max(1) as usize;
            let eff_scroll = if *cursor < *scroll {
                *cursor
            } else if *cursor >= *scroll + visible_rows {
                *cursor + 1 - visible_rows
            } else {
                *scroll
            };
            let max_preview = rect.width.saturating_sub(36) as usize;
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    "Select a session to resume ({} of {}):",
                    cursor.saturating_add(1).min(sessions.len()),
                    sessions.len()
                ),
                Style::default().fg(Color::DarkGray),
            ))];
            for (idx, sess) in sessions
                .iter()
                .enumerate()
                .skip(eff_scroll)
                .take(visible_rows)
            {
                let is_cur = idx == *cursor;
                let mark = if is_cur { "▸" } else { " " };
                let active_tag = if active_session == Some(sess.id.as_str()) {
                    "*"
                } else {
                    " "
                };
                let short_id: String = sess.id.chars().take(16).collect();
                let raw_preview = sess
                    .title
                    .as_deref()
                    .or(sess.preview.as_deref())
                    .unwrap_or("");
                let clean = approval::sanitize_terminal_text(raw_preview).replace('\n', " ");
                let preview: String = if max_preview > 3 && clean.chars().count() > max_preview {
                    let mut p: String = clean.chars().take(max_preview - 3).collect();
                    p.push_str("...");
                    p
                } else {
                    clean
                };
                let style = if is_cur {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        " {mark}{active_tag}{:>2}. {:<16} {:>4}m  {}",
                        idx + 1,
                        short_id,
                        sess.message_count,
                        preview
                    ),
                    style,
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " ↑/↓ or j/k select · Enter resume · Esc/q cancel",
                Style::default().fg(Color::Cyan),
            )));

            let widget = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(Span::styled(
                        " Sessions ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
            frame.render_widget(widget, rect);
        }
    }
}

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}
