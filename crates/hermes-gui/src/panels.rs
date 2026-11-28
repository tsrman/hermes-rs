use gpui::{
    div, prelude::*, px, rgb, ClipboardItem, Context, FontWeight, MouseButton, Rgba, SharedString,
};
use hermes_runtime::local_owner::ApprovalScope;
use hermes_runtime::turn::{ApprovalAction, ApprovalDecision};

use crate::input_helpers::{
    format_selection_preview, render_buffer_input, render_masked_input, InputTarget, TextFieldStyle,
};
use crate::setup_state::{SetupField, GUI_PROVIDER_PRESETS, GUI_WEB_BACKENDS};
use crate::workspace::format_size;
use crate::HermesAppView;

#[derive(Clone, Copy)]
pub(crate) struct ThemeColors {
    pub(crate) bg_app: Rgba,
    pub(crate) bg_sidebar: Rgba,
    pub(crate) bg_card: Rgba,
    pub(crate) bg_user: Rgba,
    pub(crate) border: Rgba,
    pub(crate) text_main: Rgba,
    pub(crate) text_dim: Rgba,
    pub(crate) accent: Rgba,
    pub(crate) green: Rgba,
    pub(crate) red: Rgba,
    pub(crate) amber: Rgba,
}

impl ThemeColors {
    pub(crate) fn dark() -> Self {
        Self {
            bg_app: rgb(0x11141a),
            bg_sidebar: rgb(0x161b24),
            bg_card: rgb(0x1d2432),
            bg_user: rgb(0x223859),
            border: rgb(0x2b3446),
            text_main: rgb(0xe6edf6),
            text_dim: rgb(0x8e9bb0),
            accent: rgb(0x4ea1ff),
            green: rgb(0x3fb950),
            red: rgb(0xf85149),
            amber: rgb(0xd29922),
        }
    }

    pub(crate) fn field_style(&self, caret_h: f32) -> TextFieldStyle {
        TextFieldStyle {
            text_main: self.text_main,
            text_dim: self.text_dim,
            caret: self.accent,
            caret_h,
        }
    }
}

impl HermesAppView {
    pub(crate) fn render_approval_panel(
        &mut self,
        theme: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ThemeColors {
            bg_sidebar,
            bg_card,
            bg_user,
            border,
            text_main,
            text_dim,
            accent,
            green,
            red,
            amber,
            ..
        } = *theme;
        let _ = (
            bg_sidebar, bg_card, bg_user, border, text_main, text_dim, accent, green, red, amber,
        );
        (self.state.pending_approval.clone().map(|pending| {
            let tid_once = pending.turn_id.clone();
            let tid_session = pending.turn_id.clone();
            let tid_always = pending.turn_id.clone();
            let tid_deny = pending.turn_id.clone();
            let aid = pending.approval_id;
            let kind_label = match pending.request.action {
                ApprovalAction::CreateFile => "Create file",
                ApprovalAction::ReplaceFile => "Edit file",
                ApprovalAction::RunCommand => "Run command",
                ApprovalAction::RunCode => "Run Python code",
                ApprovalAction::ManageSkills => "Modify skills",
                ApprovalAction::Clarify => "Clarify question",
            };
            let always_hint = match pending.request.action {
                ApprovalAction::RunCommand => {
                    "Always saves flagged pattern(s) to command_allowlist in config.yaml"
                }
                ApprovalAction::RunCode => "Always saves approvals.code: off in config.yaml",
                ApprovalAction::ManageSkills => "Always saves approvals.skills: off in config.yaml",
                _ => "Always saves approvals.edits: off in config.yaml",
            };
            let title = format!(
                "Approval required: {kind_label} ({})",
                pending.request.target
            );
            let preview: String = pending.request.content.chars().take(600).collect();

            div()
                .mx_4()
                .mb_2()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x282114))
                .border_1()
                .border_color(amber)
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(amber)
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(green)
                                .text_color(rgb(0x0b1017))
                                .text_xs()
                                .child("Approve")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, _, cx| {
                                        view.client.respond_approval(
                                            view.state.active_profile.clone(),
                                            tid_once.clone(),
                                            aid,
                                            ApprovalDecision::Approved,
                                            ApprovalScope::Once,
                                        );
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(accent)
                                .text_color(rgb(0x0b1017))
                                .text_xs()
                                .child("For session")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, _, cx| {
                                        view.client.respond_approval(
                                            view.state.active_profile.clone(),
                                            tid_session.clone(),
                                            aid,
                                            ApprovalDecision::Approved,
                                            ApprovalScope::Session,
                                        );
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(rgb(0x8957e5))
                                .text_color(rgb(0xffffff))
                                .text_xs()
                                .child("Always (save)")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, _, cx| {
                                        view.client.respond_approval(
                                            view.state.active_profile.clone(),
                                            tid_always.clone(),
                                            aid,
                                            ApprovalDecision::Approved,
                                            ApprovalScope::Always,
                                        );
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(red)
                                .text_color(rgb(0xffffff))
                                .text_xs()
                                .child("Deny")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, _, cx| {
                                        view.client.respond_approval(
                                            view.state.active_profile.clone(),
                                            tid_deny.clone(),
                                            aid,
                                            ApprovalDecision::Denied,
                                            ApprovalScope::Once,
                                        );
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(text_dim)
                        .child(SharedString::from(always_hint)),
                )
                .child(
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(0x14161c))
                        .text_xs()
                        .text_color(text_main)
                        .child(SharedString::from(preview)),
                )
        }))
        .map(IntoElement::into_any_element)
    }

    pub(crate) fn render_clarify_panel(
        &mut self,
        theme: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ThemeColors {
            bg_sidebar,
            bg_card,
            bg_user,
            border,
            text_main,
            text_dim,
            accent,
            green,
            red,
            amber,
            ..
        } = *theme;
        let _ = (
            bg_sidebar, bg_card, bg_user, border, text_main, text_dim, accent, green, red, amber,
        );
        (self.state.pending_clarify.clone().map(|clarify| {
            let total = clarify.questions.len();
            let active_idx = clarify.active_index.min(total.saturating_sub(1));
            let active_q = clarify.questions[active_idx].clone();
            let qid = active_q.qid.clone();
            let multi = active_q.multi_select;
            let chosen_set = clarify.selected.get(&qid).cloned().unwrap_or_default();
            let custom_buf = clarify
                .custom_answers
                .get(&qid)
                .cloned()
                .unwrap_or_default();
            let custom_display = render_buffer_input(
                &mut self.input_layouts,
                InputTarget::Clarify,
                &custom_buf,
                self.input_target == InputTarget::Clarify,
                "Type a custom answer (Enter to submit)...",
                theme.field_style(14.0),
            );

            let tabs_ui: Vec<_> = (0..total)
                .map(|idx| {
                    let is_active = idx == active_idx;
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(if is_active { accent } else { bg_card })
                        .text_color(if is_active { rgb(0x0b1017) } else { text_main })
                        .text_xs()
                        .child(SharedString::from(format!("Q{}", idx + 1)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _, window, cx| {
                                if let Some(c) = view.state.pending_clarify.as_mut() {
                                    c.active_index = idx;
                                }
                                view.input_target = InputTarget::Clarify;
                                window.focus(&view.focus_handle);
                                cx.notify();
                            }),
                        )
                })
                .collect();

            let choices_ui: Vec<_> = active_q
                .choices
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|choice| {
                    let selected = chosen_set.contains(&choice);
                    let marker = if multi {
                        if selected {
                            "[✓] "
                        } else {
                            "[ ] "
                        }
                    } else if selected {
                        "● "
                    } else {
                        "○ "
                    };
                    let qid_c = qid.clone();
                    let choice_c = choice.clone();
                    div()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(if selected { bg_user } else { bg_card })
                        .border_1()
                        .border_color(if selected { accent } else { border })
                        .text_xs()
                        .text_color(text_main)
                        .child(SharedString::from(format!("{marker}{choice}")))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _, _, cx| {
                                if let Some(c) = view.state.pending_clarify.as_mut() {
                                    c.toggle_choice(&qid_c, &choice_c, multi);
                                }
                                cx.notify();
                            }),
                        )
                })
                .collect();

            let header = if multi {
                format!(
                    "Question {}/{total} (select one or more): {}",
                    active_idx + 1,
                    active_q.question
                )
            } else {
                format!("Question {}/{total}: {}", active_idx + 1, active_q.question)
            };

            div()
                .mx_4()
                .mb_2()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x162233))
                .border_1()
                .border_color(accent)
                .flex()
                .flex_col()
                .gap_2()
                .when(total > 1, |el| {
                    el.child(div().flex().flex_row().gap_1().children(tabs_ui))
                })
                .child(
                    div()
                        .text_sm()
                        .text_color(accent)
                        .child(SharedString::from(header)),
                )
                .when(!choices_ui.is_empty(), |el| {
                    el.child(div().flex().flex_col().gap_1().children(choices_ui))
                })
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_text()
                        .bg(bg_card)
                        .border_1()
                        .border_color(if self.input_target == InputTarget::Clarify {
                            accent
                        } else {
                            border
                        })
                        .text_xs()
                        .child(custom_display)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, ev, window, cx| {
                                view.handle_input_mouse_down(InputTarget::Clarify, ev, window, cx);
                            }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(green)
                                .text_color(rgb(0x0b1017))
                                .text_xs()
                                .child("Submit")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, _, _, cx| {
                                        view.submit_clarify(cx);
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(bg_card)
                                .border_1()
                                .border_color(border)
                                .text_color(text_dim)
                                .text_xs()
                                .child("Cancel")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, _, _, cx| {
                                        view.cancel_clarify(cx);
                                    }),
                                ),
                        ),
                )
        }))
        .map(IntoElement::into_any_element)
    }

    pub(crate) fn render_completion_panel(
        &mut self,
        theme: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ThemeColors {
            bg_sidebar,
            bg_card,
            bg_user,
            border,
            text_main,
            text_dim,
            accent,
            green,
            red,
            amber,
            ..
        } = *theme;
        let _ = (
            bg_sidebar, bg_card, bg_user, border, text_main, text_dim, accent, green, red, amber,
        );
        (if self.input_target == InputTarget::Composer {
            self.state
                .compute_completions(&self.workspace_root)
                .map(|(title, items)| {
                    let total = items.len();
                    let visible_len = total.min(10);
                    let selected_idx = self.state.palette_index.min(total.saturating_sub(1));
                    let start = selected_idx.saturating_add(1).saturating_sub(visible_len);
                    let end = (start + visible_len).min(total);
                    let header_title = if total > visible_len {
                        format!("{title} ({}/{total})", selected_idx + 1)
                    } else {
                        title.to_owned()
                    };
                    let rows: Vec<_> = items
                        .into_iter()
                        .enumerate()
                        .skip(start)
                        .take(end.saturating_sub(start))
                        .map(|(idx, item)| {
                            let is_sel = idx == selected_idx;
                            let item_clone = item.clone();
                            div()
                                .w_full()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .bg(if is_sel { bg_user } else { bg_sidebar })
                                .flex()
                                .flex_row()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(if is_sel { accent } else { text_main })
                                        .child(SharedString::from(item.display)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .overflow_hidden()
                                        .text_xs()
                                        .text_color(text_dim)
                                        .child(SharedString::from(item.detail)),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, window, cx| {
                                        view.state.insert_completion(&item_clone);
                                        view.input_target = InputTarget::Composer;
                                        window.focus(&view.focus_handle);
                                        cx.notify();
                                    }),
                                )
                        })
                        .collect();

                    div()
                        .mx_4()
                        .mb_2()
                        .p_2()
                        .rounded_lg()
                        .bg(bg_sidebar)
                        .border_1()
                        .border_color(accent)
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(accent)
                                .child(SharedString::from(header_title)),
                        )
                        .children(rows)
                })
        } else {
            None
        })
        .map(IntoElement::into_any_element)
    }

    pub(crate) fn render_setup_view(
        &mut self,
        theme: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ThemeColors {
            bg_sidebar,
            bg_card,
            bg_user,
            border,
            text_main,
            text_dim,
            accent,
            green,
            red,
            amber,
            ..
        } = *theme;
        let _ = (
            bg_sidebar, bg_card, bg_user, border, text_main, text_dim, accent, green, red, amber,
        );
        ( self.setup.open.then(|| {
            let is_custom = self.setup.provider == "custom";
            let is_ollama = self.setup.provider == "ollama";
            let is_oauth = self.setup.preset().is_some_and(|p| p.is_oauth);
            let is_keyless = self.setup.preset().is_some_and(|p| p.keyless);

            let provider_pills: Vec<_> = GUI_PROVIDER_PRESETS
                .iter()
                .map(|preset| {
                    let active = self.setup.provider == preset.id;
                    let id = preset.id;
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(if active { accent } else { bg_card })
                        .border_1()
                        .border_color(if active { accent } else { border })
                        .text_color(if active { rgb(0x0b1017) } else { text_main })
                        .text_xs()
                        .child(SharedString::from(preset.label))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _, window, cx| {
                                let profile = view.state.active_profile.clone();
                                view.setup.select_provider(&profile, id);
                                view.input_target = InputTarget::Setup(SetupField::Model);
                                window.focus(&view.focus_handle);
                                cx.notify();
                            }),
                        )
                })
                .collect();

            let api_mode_pills: Vec<_> = ["openai", "responses", "anthropic"]
                .into_iter()
                .map(|mode| {
                    let active = self.setup.api_mode == mode;
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(if active { accent } else { bg_card })
                        .text_color(if active { rgb(0x0b1017) } else { text_main })
                        .text_xs()
                        .child(SharedString::from(mode))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _, _, cx| {
                                view.setup.api_mode = mode.to_owned();
                                cx.notify();
                            }),
                        )
                })
                .collect();

            let web_pills: Vec<_> = GUI_WEB_BACKENDS
                .iter()
                .map(|&(name, _)| {
                    let active = self.setup.web_backend == name;
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(if active { accent } else { bg_card })
                        .text_color(if active { rgb(0x0b1017) } else { text_main })
                        .text_xs()
                        .child(SharedString::from(name))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _, _, cx| {
                                view.setup.web_backend = name.to_owned();
                                cx.notify();
                            }),
                        )
                })
                .collect();

            let base_url_display = render_buffer_input(
                &mut self.input_layouts,
                InputTarget::Setup(SetupField::BaseUrl),
                &self.setup.base_url,
                self.input_target == InputTarget::Setup(SetupField::BaseUrl),
                "https://api.example.com/v1",
                theme.field_style(14.0),
            );
            let model_display = render_buffer_input(
                &mut self.input_layouts,
                InputTarget::Setup(SetupField::Model),
                &self.setup.model,
                self.input_target == InputTarget::Setup(SetupField::Model),
                "Model identifier (e.g. gpt-4.1, deepseek-chat)",
                theme.field_style(14.0),
            );
            let key_label = self.setup.key_env_name().unwrap_or("HERMES_API_KEY");
            let key_placeholder = if self.setup.has_existing_api_key {
                format!("{key_label} already configured in .env (leave blank to keep)")
            } else {
                format!("Paste {key_label} (saved to .env with chmod 600)")
            };
            let api_key_display = render_masked_input(
                &mut self.input_layouts,
                InputTarget::Setup(SetupField::ApiKey),
                &self.setup.api_key,
                self.input_target == InputTarget::Setup(SetupField::ApiKey),
                &key_placeholder,
                theme.field_style(14.0),
            );

            let web_env_name = self.setup.web_env_name();
            let web_key_display = web_env_name.map(|var| {
                render_masked_input(
                    &mut self.input_layouts,
                    InputTarget::Setup(SetupField::WebApiKey),
                    &self.setup.web_api_key,
                    self.input_target == InputTarget::Setup(SetupField::WebApiKey),
                    &format!("Optional {var} (leave blank to keep existing)"),
                    theme.field_style(14.0),
                )
            });

            let oauth_provider = self.setup.provider.clone();
            let oauth_busy = self.setup.oauth_in_progress;
            let skills_busy = self.setup.syncing_skills;
            let skills_count = self.setup.installed_skills_count;
            let notice = self.setup.notice.clone();

            div()
                .id("setup-scroll")
                .flex_1()
                .min_h(px(0.0))
                .min_w(px(0.0))
                .overflow_y_scroll()
                .p_4()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_base()
                                        .text_color(accent)
                                        .child("Profile Setup & Settings"),
                                )
                                .child(div().text_xs().text_color(text_dim).child(
                                    SharedString::from(format!(
                                        "Profile: {} (saving initializes all directories, starter files & state.db)",
                                        self.state.active_profile.display()
                                    )),
                                )),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(bg_card)
                                .border_1()
                                .border_color(border)
                                .text_xs()
                                .text_color(text_dim)
                                .child("Close (Esc)")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, _, window, cx| {
                                        view.setup.open = false;
                                        view.input_target = InputTarget::Composer;
                                        window.focus(&view.focus_handle);
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
                .children(notice.map(|(is_err, text)| {
                    div()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(if is_err { rgb(0x301619) } else { rgb(0x162638) })
                        .border_1()
                        .border_color(if is_err { red } else { accent })
                        .text_xs()
                        .text_color(if is_err { red } else { text_main })
                        .child(SharedString::from(text))
                }))
                // Provider selector
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_xs().text_color(text_dim).child("1. LLM Provider"))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap_1()
                                .children(provider_pills),
                        ),
                )
                // OAuth button when an OAuth provider is selected
                .when(is_oauth, |el| {
                    el.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(accent)
                                    .text_color(rgb(0x0b1017))
                                    .text_xs()
                                    .child(if oauth_busy {
                                        "Waiting for browser sign-in..."
                                    } else {
                                        "Sign in via Browser (OAuth)"
                                    })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |view, _, _, cx| {
                                            if !view.setup.oauth_in_progress {
                                                view.setup.oauth_in_progress = true;
                                                view.setup.notice = Some((
                                                    false,
                                                    format!("Starting OAuth login for {oauth_provider}..."),
                                                ));
                                                view.client.run_oauth_login(
                                                    view.state.active_profile.clone(),
                                                    oauth_provider.clone(),
                                                );
                                                cx.notify();
                                            }
                                        }),
                                    ),
                            ),
                    )
                })
                // Base URL + API mode for custom / ollama
                .when(is_custom || is_ollama, |el| {
                    el.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(text_dim)
                                    .child("Endpoint Base URL"),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_text()
                                    .bg(bg_card)
                                    .border_1()
                                    .border_color(
                                        if self.input_target
                                            == InputTarget::Setup(SetupField::BaseUrl)
                                        {
                                            accent
                                        } else {
                                            border
                                        },
                                    )
                                    .text_xs()
                                    .child(base_url_display)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, ev, window, cx| {
                                            view.handle_input_mouse_down(
                                                InputTarget::Setup(SetupField::BaseUrl),
                                                ev,
                                                window,
                                                cx,
                                            );
                                        }),
                                    ),
                            ),
                    )
                    .when(is_custom, |el2| {
                        el2.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(div().text_xs().text_color(text_dim).child("API Mode:"))
                                .children(api_mode_pills),
                        )
                    })
                })
                // Default Model
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_xs().text_color(text_dim).child("2. Default Model"))
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .cursor_text()
                                .bg(bg_card)
                                .border_1()
                                .border_color(
                                    if self.input_target == InputTarget::Setup(SetupField::Model) {
                                        accent
                                    } else {
                                        border
                                    },
                                )
                                .text_xs()
                                .child(model_display)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, ev, window, cx| {
                                        view.handle_input_mouse_down(
                                            InputTarget::Setup(SetupField::Model),
                                            ev,
                                            window,
                                            cx,
                                        );
                                    }),
                                ),
                        ),
                )
                // API Key (masked, saved to .env 0600)
                .when(!is_oauth && !is_keyless, |el| {
                    el.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(text_dim).child(
                                SharedString::from(format!("3. API Key ({key_label} → .env 0600)")),
                            ))
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_text()
                                    .bg(bg_card)
                                    .border_1()
                                    .border_color(
                                        if self.input_target
                                            == InputTarget::Setup(SetupField::ApiKey)
                                        {
                                            accent
                                        } else {
                                            border
                                        },
                                    )
                                    .text_xs()
                                    .child(api_key_display)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, ev, window, cx| {
                                            view.handle_input_mouse_down(
                                                InputTarget::Setup(SetupField::ApiKey),
                                                ev,
                                                window,
                                                cx,
                                            );
                                        }),
                                    ),
                            ),
                    )
                })
                // Web search backend
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(text_dim)
                                .child("4. Web Search Backend (optional)"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap_1()
                                .children(web_pills),
                        )
                        .children(web_key_display.map(|display| {
                            div()
                                .mt_1()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .cursor_text()
                                .bg(bg_card)
                                .border_1()
                                .border_color(
                                    if self.input_target
                                        == InputTarget::Setup(SetupField::WebApiKey)
                                    {
                                        accent
                                    } else {
                                        border
                                    },
                                )
                                .text_xs()
                                .child(display)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, ev, window, cx| {
                                        view.handle_input_mouse_down(
                                            InputTarget::Setup(SetupField::WebApiKey),
                                            ev,
                                            window,
                                            cx,
                                        );
                                    }),
                                )
                        })),
                )
                // Memory & Bundled Skills row
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(if self.setup.memory_enabled {
                                    bg_user
                                } else {
                                    bg_card
                                })
                                .border_1()
                                .border_color(if self.setup.memory_enabled {
                                    accent
                                } else {
                                    border
                                })
                                .text_xs()
                                .text_color(text_main)
                                .child(if self.setup.memory_enabled {
                                    "✓ Memory (MEMORY.md & USER.md)"
                                } else {
                                    "○ Memory disabled"
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, _, _, cx| {
                                        view.setup.memory_enabled = !view.setup.memory_enabled;
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(bg_card)
                                .border_1()
                                .border_color(accent)
                                .text_xs()
                                .text_color(accent)
                                .child(SharedString::from(if skills_busy {
                                    "Downloading bundled skills...".into()
                                } else {
                                    format!(
                                        "⬇ Download Official Skills ({skills_count} installed)"
                                    )
                                }))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, _, _, cx| {
                                        if !view.setup.syncing_skills {
                                            view.setup.syncing_skills = true;
                                            view.setup.notice = Some((
                                                false,
                                                "Downloading bundled skills from GitHub...".into(),
                                            ));
                                            view.client.sync_bundled_skills(
                                                view.state.active_profile.clone(),
                                            );
                                            cx.notify();
                                        }
                                    }),
                                ),
                        ),
                )
                // Save button
                .child(
                    div()
                        .pt_2()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(green)
                                .text_color(rgb(0x0b1017))
                                .text_sm()
                                .child("Save & Initialize Profile")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, _, _, cx| {
                                        view.save_setup(cx);
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(bg_card)
                                .border_1()
                                .border_color(border)
                                .text_color(text_main)
                                .text_sm()
                                .child("Back to Chat")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|view, _, window, cx| {
                                        view.setup.open = false;
                                        view.input_target = InputTarget::Composer;
                                        window.focus(&view.focus_handle);
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
        })).map(IntoElement::into_any_element)
    }

    pub(crate) fn render_selection_action_panel(
        &mut self,
        theme: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ThemeColors {
            bg_sidebar,
            bg_card,
            bg_user,
            border,
            text_main,
            text_dim,
            accent,
            green,
            red,
            amber,
            ..
        } = *theme;
        let _ = (
            bg_sidebar, bg_card, bg_user, border, text_main, text_dim, accent, green, red, amber,
        );
        (self.selected_feed_text().and_then(|sel_text| {
            if sel_text.trim().is_empty() {
                return None;
            }
            let preview = format_selection_preview(&sel_text, 76);
            let copy_sel = sel_text.clone();
            Some(
                div()
                    .mx_4()
                    .mb_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x162233))
                    .border_1()
                    .border_color(accent)
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(accent)
                                    .child("❝"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_xs()
                                    .text_color(text_main)
                                    .child(SharedString::from(preview)),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .bg(accent)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(0x0b1017))
                                    .child("Quote in reply (Ctrl+Shift+Q)")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, _, window, cx| {
                                            cx.stop_propagation();
                                            view.quote_selected_feed_text(window, cx);
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .bg(bg_card)
                                    .border_1()
                                    .border_color(border)
                                    .text_xs()
                                    .text_color(text_main)
                                    .child("Copy")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |view, _, _, cx| {
                                            cx.stop_propagation();
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_sel.clone(),
                                            ));
                                            view.state.status_message =
                                                "Copied selection to clipboard".into();
                                            cx.notify();
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .px_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_xs()
                                    .text_color(text_dim)
                                    .child("×")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, _, _, cx| {
                                            cx.stop_propagation();
                                            view.feed_selection = None;
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    ),
            )
        }))
        .map(IntoElement::into_any_element)
    }

    pub(crate) fn render_attachments_panel(
        &mut self,
        theme: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ThemeColors {
            bg_sidebar,
            bg_card,
            bg_user,
            border,
            text_main,
            text_dim,
            accent,
            green,
            red,
            amber,
            ..
        } = *theme;
        let _ = (
            bg_sidebar, bg_card, bg_user, border, text_main, text_dim, accent, green, red, amber,
        );
        ((!self.state.attachments.is_empty()).then(|| {
            let chips: Vec<_> = self
                .state
                .attachments
                .iter()
                .enumerate()
                .map(|(idx, att)| {
                    let icon = if att.is_media_or_doc { "🖼" } else { "📄" };
                    let suffix = if att.imported { " · copied" } else { "" };
                    let label = format!(
                        "{icon} {} ({}{suffix})",
                        att.display_name,
                        format_size(att.size_bytes)
                    );
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(bg_user)
                        .border_1()
                        .border_color(accent)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(text_main)
                                .child(SharedString::from(label)),
                        )
                        .child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .text_xs()
                                .text_color(text_dim)
                                .child("×")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, _, cx| {
                                        view.state.remove_attachment(idx);
                                        cx.notify();
                                    }),
                                ),
                        )
                })
                .collect();

            div()
                .mx_4()
                .mb_2()
                .p_2()
                .rounded_md()
                .bg(bg_sidebar)
                .border_1()
                .border_color(border)
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .children(chips)
        }))
        .map(IntoElement::into_any_element)
    }
}
