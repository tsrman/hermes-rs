use std::collections::HashMap;

use gpui::{
    div, prelude::*, px, rgb, ClipboardItem, Context, FontStyle, FontWeight, HighlightStyle,
    MouseButton, SharedString, StrikethroughStyle, StyledText, TextLayout, UnderlineStyle,
};

use crate::diagram::{parse_mermaid, render_diagram};
use crate::markdown::{
    highlight_code, parse_document, parse_inline_with_refs, InlineStyle, MdBlock, TableAlign,
};
use crate::math_render::render_math_formula;
use crate::HermesAppView;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FeedTextKey {
    pub(crate) row_idx: usize,
    pub(crate) block_idx: usize,
    pub(crate) sub_idx: usize,
}

#[derive(Clone)]
pub(crate) struct FeedLayoutSnapshot {
    pub(crate) text: String,
    pub(crate) layout: TextLayout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedTextSelection {
    pub(crate) key: FeedTextKey,
    pub(crate) anchor: usize,
    pub(crate) focus: usize,
    pub(crate) dragging: bool,
}

impl FeedTextSelection {
    pub(crate) fn range(&self) -> Option<std::ops::Range<usize>> {
        let (start, end) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        (start < end).then_some(start..end)
    }
}

fn inline_style_to_highlight(style: InlineStyle) -> HighlightStyle {
    match style {
        InlineStyle::Bold => HighlightStyle {
            color: Some(rgb(0xffffff).into()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
        InlineStyle::Italic => HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        },
        InlineStyle::BoldItalic => HighlightStyle {
            color: Some(rgb(0xffffff).into()),
            font_weight: Some(FontWeight::BOLD),
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        },
        InlineStyle::Code => HighlightStyle {
            color: Some(rgb(0x93c5fd).into()),
            background_color: Some(rgb(0x10141c).into()),
            ..Default::default()
        },
        InlineStyle::Strikethrough => HighlightStyle {
            color: Some(rgb(0x8e9bb0).into()),
            strikethrough: Some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(rgb(0x8e9bb0).into()),
            }),
            ..Default::default()
        },
        InlineStyle::Underline => HighlightStyle {
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(rgb(0xe6edf6).into()),
                wavy: false,
            }),
            ..Default::default()
        },
        InlineStyle::Link => HighlightStyle {
            color: Some(rgb(0x4ea1ff).into()),
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(rgb(0x4ea1ff).into()),
                wavy: false,
            }),
            ..Default::default()
        },
        InlineStyle::Footnote => HighlightStyle {
            color: Some(rgb(0x7dd3fc).into()),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        },
        InlineStyle::Math => HighlightStyle {
            color: Some(rgb(0xfde68a).into()),
            font_style: Some(FontStyle::Italic),
            background_color: Some(rgb(0x10141c).into()),
            ..Default::default()
        },
        InlineStyle::CodeKeyword => HighlightStyle {
            color: Some(rgb(0xc084fc).into()),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        },
        InlineStyle::CodeString => HighlightStyle {
            color: Some(rgb(0x86efac).into()),
            ..Default::default()
        },
        InlineStyle::CodeComment => HighlightStyle {
            color: Some(rgb(0x64748b).into()),
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        },
        InlineStyle::CodeNumber => HighlightStyle {
            color: Some(rgb(0xfbbf24).into()),
            ..Default::default()
        },
    }
}

/// Merge non-overlapping inline Markdown highlight spans with an optional active selection range
/// into strictly sorted, non-overlapping highlight runs safe for `StyledText::with_highlights`.
pub(crate) fn merge_highlights_with_selection(
    text_len: usize,
    base: &[(std::ops::Range<usize>, InlineStyle)],
    selection: Option<std::ops::Range<usize>>,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    let mut points = Vec::with_capacity(base.len() * 2 + 4);
    for (r, _) in base {
        if r.start < r.end && r.end <= text_len {
            points.push(r.start);
            points.push(r.end);
        }
    }
    if let Some(ref sel) = selection {
        if sel.start < sel.end && sel.end <= text_len {
            points.push(sel.start);
            points.push(sel.end);
        }
    }
    if points.is_empty() {
        return Vec::new();
    }
    points.sort_unstable();
    points.dedup();

    let mut merged = Vec::new();
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a >= b {
            continue;
        }
        let base_style = base
            .iter()
            .find(|(r, _)| r.start <= a && b <= r.end)
            .map(|(_, s)| inline_style_to_highlight(*s));
        let in_sel = selection
            .as_ref()
            .is_some_and(|sel| sel.start <= a && b <= sel.end);
        match (base_style, in_sel) {
            (Some(mut hs), true) => {
                hs.color = Some(rgb(0xffffff).into());
                hs.background_color = Some(rgb(0x264f78).into());
                merged.push((a..b, hs));
            }
            (Some(hs), false) => merged.push((a..b, hs)),
            (None, true) => merged.push((
                a..b,
                HighlightStyle {
                    color: Some(rgb(0xffffff).into()),
                    background_color: Some(rgb(0x264f78).into()),
                    ..Default::default()
                },
            )),
            (None, false) => {}
        }
    }
    merged
}

pub(crate) fn render_selectable_feed_text_styled(
    feed_layouts: &mut HashMap<FeedTextKey, FeedLayoutSnapshot>,
    active_selection: Option<FeedTextSelection>,
    key: FeedTextKey,
    text: String,
    inline_spans: &[(std::ops::Range<usize>, InlineStyle)],
    cx: &mut Context<HermesAppView>,
) -> gpui::AnyElement {
    let selection = active_selection
        .filter(|sel| sel.key == key)
        .and_then(|sel| sel.range())
        .filter(|r| {
            r.end <= text.len() && text.is_char_boundary(r.start) && text.is_char_boundary(r.end)
        });
    let highlights = merge_highlights_with_selection(text.len(), inline_spans, selection);
    let styled = if highlights.is_empty() {
        StyledText::new(text.clone())
    } else {
        StyledText::new(text.clone()).with_highlights(highlights)
    };
    feed_layouts.insert(
        key,
        FeedLayoutSnapshot {
            text,
            layout: styled.layout().clone(),
        },
    );
    div()
        .w_full()
        .min_w(px(0.0))
        .cursor_text()
        .child(styled)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, ev, window, cx| {
                cx.stop_propagation();
                view.handle_feed_mouse_down(key, ev, window, cx);
            }),
        )
        .into_any_element()
}

pub(crate) fn render_selectable_feed_text(
    feed_layouts: &mut HashMap<FeedTextKey, FeedLayoutSnapshot>,
    active_selection: Option<FeedTextSelection>,
    key: FeedTextKey,
    text: String,
    cx: &mut Context<HermesAppView>,
) -> gpui::AnyElement {
    render_selectable_feed_text_styled(feed_layouts, active_selection, key, text, &[], cx)
}

pub(crate) fn render_selectable_markdown_inline(
    feed_layouts: &mut HashMap<FeedTextKey, FeedLayoutSnapshot>,
    active_selection: Option<FeedTextSelection>,
    key: FeedTextKey,
    raw: &str,
    link_refs: &HashMap<String, String>,
    cx: &mut Context<HermesAppView>,
) -> gpui::AnyElement {
    let parsed = parse_inline_with_refs(raw, link_refs);
    render_selectable_feed_text_styled(
        feed_layouts,
        active_selection,
        key,
        parsed.plain,
        &parsed.spans,
        cx,
    )
}

pub(crate) fn render_markdown_blocks(
    row_idx: usize,
    text: &str,
    feed_layouts: &mut HashMap<FeedTextKey, FeedLayoutSnapshot>,
    active_selection: Option<FeedTextSelection>,
    cx: &mut Context<HermesAppView>,
) -> Vec<gpui::AnyElement> {
    let text_main = rgb(0xe6edf6);
    let text_dim = rgb(0x8e9bb0);
    let accent = rgb(0x4ea1ff);
    let green = rgb(0x4ade80);
    let border = rgb(0x2b3446);
    let bg_code = rgb(0x10141c);
    let mono_font = if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "monospace"
    };

    let doc = parse_document(text);
    let refs = &doc.link_refs;

    doc.blocks
        .into_iter()
        .enumerate()
        .map(|(block_idx, block)| match block {
            MdBlock::Heading { level, text } => {
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                let el = div().w_full().min_w(px(0.0)).text_color(accent).child(
                    render_selectable_markdown_inline(
                        feed_layouts,
                        active_selection,
                        key,
                        &text,
                        refs,
                        cx,
                    ),
                );
                if level <= 2 {
                    el.text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .into_any_element()
                } else {
                    el.text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .into_any_element()
                }
            }
            MdBlock::Paragraph { text } => {
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .text_sm()
                    .text_color(text_main)
                    .child(render_selectable_markdown_inline(
                        feed_layouts,
                        active_selection,
                        key,
                        &text,
                        refs,
                        cx,
                    ))
                    .into_any_element()
            }
            MdBlock::Bullet { indent, text } => {
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .pl(px(f32::from(indent) * 14.0))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .text_sm()
                    .text_color(text_main)
                    .child(div().flex_shrink_0().text_color(accent).child("•"))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(render_selectable_markdown_inline(
                                feed_layouts,
                                active_selection,
                                key,
                                &text,
                                refs,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
            MdBlock::TaskItem {
                indent,
                checked,
                text,
            } => {
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .pl(px(f32::from(indent) * 14.0))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .text_sm()
                    .text_color(if checked { text_dim } else { text_main })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(if checked { green } else { text_dim })
                            .child(if checked { "☑" } else { "☐" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(render_selectable_markdown_inline(
                                feed_layouts,
                                active_selection,
                                key,
                                &text,
                                refs,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
            MdBlock::OrderedItem {
                indent,
                number,
                text,
            } => {
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .pl(px(f32::from(indent) * 14.0))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .text_sm()
                    .text_color(text_main)
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(accent)
                            .child(SharedString::from(number)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(render_selectable_markdown_inline(
                                feed_layouts,
                                active_selection,
                                key,
                                &text,
                                refs,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
            MdBlock::Definition { term, definition } => {
                let term_key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                let def_key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 1,
                };
                let mut container = div().w_full().min_w(px(0.0)).flex().flex_col().gap_1();
                if !term.is_empty() {
                    container = container.child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0xffffff))
                            .child(render_selectable_markdown_inline(
                                feed_layouts,
                                active_selection,
                                term_key,
                                &term,
                                refs,
                                cx,
                            )),
                    );
                }
                container
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .pl_4()
                            .border_l_2()
                            .border_color(border)
                            .text_sm()
                            .text_color(text_main)
                            .child(render_selectable_markdown_inline(
                                feed_layouts,
                                active_selection,
                                def_key,
                                &definition,
                                refs,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
            MdBlock::Quote { depth, text } => {
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                let mut el = div()
                    .w_full()
                    .min_w(px(0.0))
                    .pl_3()
                    .border_l_2()
                    .border_color(accent)
                    .text_sm()
                    .text_color(text_dim)
                    .child(render_selectable_markdown_inline(
                        feed_layouts,
                        active_selection,
                        key,
                        &text,
                        refs,
                        cx,
                    ));
                for _ in 1..depth {
                    el = div()
                        .w_full()
                        .min_w(px(0.0))
                        .pl_3()
                        .border_l_2()
                        .border_color(border)
                        .child(el);
                }
                el.into_any_element()
            }
            MdBlock::MathBlock { raw } => {
                let copy_latex = raw.clone();
                let quote_latex = format!("$$\n{}\n$$", raw.trim());
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .my_1()
                    .rounded_md()
                    .bg(bg_code)
                    .border_1()
                    .border_color(border)
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .border_b_1()
                            .border_color(border)
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(div().text_xs().text_color(text_dim).child("Math"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(
                                        div()
                                            .px_2()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .bg(rgb(0x1d2432))
                                            .text_xs()
                                            .text_color(accent)
                                            .child("❝ Quote")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |view, _, window, cx| {
                                                    cx.stop_propagation();
                                                    view.quote_text_into_composer(
                                                        &quote_latex,
                                                        window,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .bg(rgb(0x1d2432))
                                            .text_xs()
                                            .text_color(accent)
                                            .child("Copy LaTeX")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |view, _, _, cx| {
                                                    cx.stop_propagation();
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(
                                                            copy_latex.clone(),
                                                        ),
                                                    );
                                                    view.state.status_message =
                                                        "Copied LaTeX formula to clipboard".into();
                                                    cx.notify();
                                                }),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .px_3()
                            .py_2()
                            .child(render_math_formula(&raw)),
                    )
                    .into_any_element()
            }
            MdBlock::FootnoteDef { id, text } => {
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .text_xs()
                    .text_color(text_dim)
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(rgb(0x7dd3fc))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(format!("[{id}]"))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(render_selectable_markdown_inline(
                                feed_layouts,
                                active_selection,
                                key,
                                &text,
                                refs,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
            MdBlock::Rule => div()
                .w_full()
                .h(px(1.0))
                .my_1()
                .bg(border)
                .into_any_element(),
            MdBlock::Table {
                headers,
                alignments,
                rows,
            } => {
                let mut tsv_lines = Vec::with_capacity(rows.len() + 1);
                tsv_lines.push(
                    headers
                        .iter()
                        .map(|h| parse_inline_with_refs(h, refs).plain)
                        .collect::<Vec<_>>()
                        .join("\t"),
                );
                for r in &rows {
                    tsv_lines.push(
                        r.iter()
                            .map(|c| parse_inline_with_refs(c, refs).plain)
                            .collect::<Vec<_>>()
                            .join("\t"),
                    );
                }
                let tsv_copy = tsv_lines.join("\n");
                let col_count = headers.len().max(1);

                let header_cells: Vec<_> = headers
                    .into_iter()
                    .enumerate()
                    .map(|(col_idx, cell)| {
                        let key = FeedTextKey {
                            row_idx,
                            block_idx,
                            sub_idx: col_idx,
                        };
                        let align = alignments.get(col_idx).copied().unwrap_or(TableAlign::Left);
                        let mut cell_div = div()
                            .flex_1()
                            .min_w(px(0.0))
                            .px_2()
                            .py_1()
                            .flex()
                            .flex_row()
                            .text_xs()
                            .text_color(accent);
                        cell_div = match align {
                            TableAlign::Left => cell_div.justify_start(),
                            TableAlign::Center => cell_div.justify_center(),
                            TableAlign::Right => cell_div.justify_end(),
                        };
                        if col_idx > 0 {
                            cell_div = cell_div.border_l_1().border_color(border);
                        }
                        cell_div
                            .child(render_selectable_markdown_inline(
                                feed_layouts,
                                active_selection,
                                key,
                                &cell,
                                refs,
                                cx,
                            ))
                            .into_any_element()
                    })
                    .collect();

                let body_rows: Vec<_> = rows
                    .into_iter()
                    .enumerate()
                    .map(|(r_idx, row_cells)| {
                        let row_bg = if r_idx % 2 == 0 {
                            rgb(0x141923)
                        } else {
                            rgb(0x181f2c)
                        };
                        let cells_ui: Vec<_> = row_cells
                            .into_iter()
                            .enumerate()
                            .map(|(col_idx, cell)| {
                                let key = FeedTextKey {
                                    row_idx,
                                    block_idx,
                                    sub_idx: (r_idx + 1) * col_count + col_idx,
                                };
                                let align =
                                    alignments.get(col_idx).copied().unwrap_or(TableAlign::Left);
                                let mut cell_div = div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .px_2()
                                    .py_1()
                                    .flex()
                                    .flex_row()
                                    .text_xs()
                                    .text_color(text_main);
                                cell_div = match align {
                                    TableAlign::Left => cell_div.justify_start(),
                                    TableAlign::Center => cell_div.justify_center(),
                                    TableAlign::Right => cell_div.justify_end(),
                                };
                                if col_idx > 0 {
                                    cell_div = cell_div.border_l_1().border_color(border);
                                }
                                cell_div
                                    .child(render_selectable_markdown_inline(
                                        feed_layouts,
                                        active_selection,
                                        key,
                                        &cell,
                                        refs,
                                        cx,
                                    ))
                                    .into_any_element()
                            })
                            .collect();
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .bg(row_bg)
                            .border_t_1()
                            .border_color(border)
                            .flex()
                            .flex_row()
                            .children(cells_ui)
                            .into_any_element()
                    })
                    .collect();

                div()
                    .w_full()
                    .min_w(px(0.0))
                    .my_1()
                    .rounded_md()
                    .bg(rgb(0x141923))
                    .border_1()
                    .border_color(border)
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .bg(rgb(0x10141c))
                            .border_b_1()
                            .border_color(border)
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(div().text_xs().text_color(text_dim).child("Table"))
                            .child(
                                div()
                                    .px_2()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .bg(rgb(0x1d2432))
                                    .text_xs()
                                    .text_color(accent)
                                    .child("Copy TSV")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |view, _, _, cx| {
                                            cx.stop_propagation();
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                tsv_copy.clone(),
                                            ));
                                            view.state.status_message =
                                                "Copied table to clipboard".into();
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .bg(rgb(0x1f2838))
                            .flex()
                            .flex_row()
                            .children(header_cells),
                    )
                    .children(body_rows)
                    .into_any_element()
            }
            MdBlock::CodeBlock { lang, code } => {
                if lang
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case("mermaid"))
                {
                    if let Some(diagram) = parse_mermaid(&code) {
                        let copy_src = code.clone();
                        let quote_src = format!("```mermaid\n{}\n```", code.trim());
                        let header = format!("Mermaid · {}", diagram.kind_label());
                        return div()
                            .w_full()
                            .min_w(px(0.0))
                            .my_1()
                            .rounded_md()
                            .bg(bg_code)
                            .border_1()
                            .border_color(border)
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .border_b_1()
                                    .border_color(border)
                                    .flex()
                                    .flex_row()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(text_dim)
                                            .child(SharedString::from(header)),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .px_2()
                                                    .rounded_sm()
                                                    .cursor_pointer()
                                                    .bg(rgb(0x1d2432))
                                                    .text_xs()
                                                    .text_color(accent)
                                                    .child("❝ Quote")
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |view, _, window, cx| {
                                                            cx.stop_propagation();
                                                            view.quote_text_into_composer(
                                                                &quote_src, window, cx,
                                                            );
                                                        }),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .px_2()
                                                    .rounded_sm()
                                                    .cursor_pointer()
                                                    .bg(rgb(0x1d2432))
                                                    .text_xs()
                                                    .text_color(accent)
                                                    .child("Copy Mermaid")
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |view, _, _, cx| {
                                                            cx.stop_propagation();
                                                            cx.write_to_clipboard(
                                                                ClipboardItem::new_string(
                                                                    copy_src.clone(),
                                                                ),
                                                            );
                                                            view.state.status_message =
                                                                "Copied Mermaid source to clipboard"
                                                                    .into();
                                                            cx.notify();
                                                        }),
                                                    ),
                                            ),
                                    ),
                            )
                            .child(div().w_full().p_2().child(render_diagram(diagram)))
                            .into_any_element();
                    }
                }
                let copy_text = code.clone();
                let code_spans = highlight_code(lang.as_deref(), &code);
                let header_label = lang.unwrap_or_else(|| "code".into());
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .my_1()
                    .rounded_md()
                    .bg(bg_code)
                    .border_1()
                    .border_color(border)
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .border_b_1()
                            .border_color(border)
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(text_dim)
                                    .child(SharedString::from(header_label)),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .bg(rgb(0x1d2432))
                                    .text_xs()
                                    .text_color(accent)
                                    .child("Copy")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |view, _, _, cx| {
                                            cx.stop_propagation();
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_text.clone(),
                                            ));
                                            view.state.status_message =
                                                "Copied code block to clipboard".into();
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .font_family(mono_font)
                            .text_xs()
                            .text_color(rgb(0xcbd5e1))
                            .child(render_selectable_feed_text_styled(
                                feed_layouts,
                                active_selection,
                                key,
                                code,
                                &code_spans,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
        })
        .collect()
}
