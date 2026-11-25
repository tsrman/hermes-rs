//! `hsr-gui`: standalone GPUI desktop application for Hermes Agent backed by the shared
//! cross-platform local session owner (`hsr owner`, protocol v2).

mod editor;
mod markdown;
mod owner_client;
mod state;

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, ClipboardItem, Context, FocusHandle,
    KeyDownEvent, MouseButton, SharedString, Window, WindowBounds, WindowOptions,
};
use hermes_runtime::config::profile_home;
use hermes_runtime::turn::{ApprovalAction, ApprovalDecision};

use crate::editor::TextBuffer;
use crate::markdown::{parse_blocks, MdBlock};
use crate::owner_client::{BackendUpdate, OwnerClient};
use crate::state::{FeedRow, GuiState, PendingClarify, ToolCardState};

#[derive(Clone, Copy, Eq, PartialEq)]
enum InputTarget {
    Composer,
    Search,
    Clarify,
}

struct HermesAppView {
    state: GuiState,
    workspace_root: PathBuf,
    client: OwnerClient,
    rx: Receiver<BackendUpdate>,
    focus_handle: FocusHandle,
    input_target: InputTarget,
}

impl HermesAppView {
    fn new(cx: &mut Context<Self>) -> Self {
        let home = profile_home(None)
            .ok()
            .flatten()
            .unwrap_or_else(|| PathBuf::from(".hsr"));
        let (client, rx) = OwnerClient::new().expect("tokio runtime");
        client.connect_and_sync(home.clone(), String::new());
        let focus_handle = cx.focus_handle();

        // Poll backend events smoothly every 40ms so streaming deltas, approvals, and clarify
        // modals appear immediately while keeping GPUI's event loop responsive.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(40))
                .await;
            let keep_running = this
                .update(cx, |view, cx| {
                    if view.drain_updates() {
                        cx.notify();
                    }
                })
                .is_ok();
            if !keep_running {
                break;
            }
        })
        .detach();

        let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            state: GuiState::new(home),
            workspace_root,
            client,
            rx,
            focus_handle,
            input_target: InputTarget::Composer,
        }
    }

    fn drain_updates(&mut self) -> bool {
        let mut changed = false;
        while let Ok(update) = self.rx.try_recv() {
            changed = true;
            match update {
                BackendUpdate::Connected {
                    profile,
                    profiles,
                    sessions,
                    active_turns,
                } => {
                    if profile != self.state.active_profile {
                        continue;
                    }
                    self.state.owner_connected = true;
                    self.state.profiles = profiles;
                    self.state.sessions = sessions;
                    self.state.status_message = "Connected to local owner".into();
                    if let Some((turn_id, after_seq)) = self.state.note_active_turns(&active_turns)
                    {
                        self.client.subscribe_turn(
                            self.state.active_profile.clone(),
                            turn_id,
                            after_seq,
                        );
                    }
                }
                BackendUpdate::Reconnecting { profile, message } => {
                    if profile == self.state.active_profile {
                        self.state.owner_connected = false;
                        self.state.status_message = message;
                    }
                }
                BackendUpdate::OwnerRestarted { profile } => {
                    if profile == self.state.active_profile {
                        self.state.streaming = false;
                        self.state.active_turn_id = None;
                        self.state.pending_approval = None;
                        self.state.pending_clarify = None;
                        self.state.status_message =
                            "Local owner restarted; syncing saved history...".into();
                        self.client.connect_and_sync(
                            profile.clone(),
                            self.state.search_input.as_str().to_owned(),
                        );
                        if let Some(sid) = self.state.selected_session_id.clone() {
                            self.client.load_session(profile, sid);
                        }
                    }
                }
                BackendUpdate::SessionsLoaded(sessions) => {
                    self.state.sessions = sessions;
                }
                BackendUpdate::TranscriptLoaded {
                    session,
                    messages,
                    active_turn_id,
                } => {
                    let reattach = active_turn_id.clone();
                    self.state
                        .load_transcript(session, messages, active_turn_id);
                    if let Some(tid) = reattach {
                        self.client
                            .subscribe_turn(self.state.active_profile.clone(), tid, 0);
                    }
                }
                BackendUpdate::TurnStarted { turn_id } => {
                    self.state.active_turn_id = Some(turn_id);
                    self.state.last_seq = 0;
                    self.state.streaming = true;
                    self.state.status_message = "Streaming...".into();
                }
                BackendUpdate::TurnEvent { turn_id, event } => {
                    self.state.owner_connected = true;
                    let finished = matches!(
                        event.event,
                        hermes_runtime::local_owner::TurnStreamEvent::Done { .. }
                    );
                    self.state.apply_turn_event(&turn_id, event);
                    if self.state.pending_clarify.is_some() {
                        self.input_target = InputTarget::Clarify;
                    } else if self.input_target == InputTarget::Clarify {
                        self.input_target = InputTarget::Composer;
                    }
                    if finished {
                        self.client.list_sessions(
                            self.state.active_profile.clone(),
                            self.state.search_input.as_str().to_owned(),
                        );
                    }
                }
                BackendUpdate::Error(msg) => {
                    self.state.feed.push(FeedRow::Notice {
                        is_error: true,
                        text: msg.clone(),
                    });
                    self.state.status_message = msg;
                    self.state.streaming = false;
                }
            }
        }
        changed
    }

    fn active_buffer_mut(&mut self) -> Option<&mut TextBuffer> {
        match self.input_target {
            InputTarget::Composer => Some(&mut self.state.composer_input),
            InputTarget::Search => Some(&mut self.state.search_input),
            InputTarget::Clarify => self
                .state
                .pending_clarify
                .as_mut()
                .and_then(PendingClarify::active_buffer_mut),
        }
    }

    fn send_composer(&mut self, cx: &mut Context<Self>) {
        let text = self.state.composer_input.as_str().trim().to_owned();
        if text.is_empty() || self.state.streaming {
            return;
        }
        self.state.composer_input.clear();
        self.state.palette_index = 0;
        self.state.palette_dismissed = false;

        // Direct local shell escape: `!<command>`
        if let Some(shell_cmd) = text.strip_prefix('!').filter(|_| !text.contains('\n')) {
            match hermes_runtime::context_refs::run_shell_escape(&self.workspace_root, shell_cmd) {
                Ok(out) => {
                    let mut body = format!("$ {shell_cmd}\n");
                    if !out.stdout.trim_end().is_empty() {
                        body.push_str(out.stdout.trim_end());
                    }
                    if !out.stderr.trim_end().is_empty() {
                        if !body.ends_with('\n') {
                            body.push('\n');
                        }
                        body.push_str(out.stderr.trim_end());
                    }
                    if out.exit_code != 0 {
                        body.push_str(&format!("\n(exit {})", out.exit_code));
                    }
                    self.state.feed.push(FeedRow::Notice {
                        is_error: out.exit_code != 0,
                        text: body,
                    });
                    self.state.status_message = format!("Shell exit {}", out.exit_code);
                }
                Err(err) => {
                    self.state.feed.push(FeedRow::Notice {
                        is_error: true,
                        text: err.clone(),
                    });
                    self.state.status_message = err;
                }
            }
            cx.notify();
            return;
        }

        // Slash commands & installed `/<skill-name> [args]`
        let mut effective_query = text.clone();
        if let Some(command) = text.strip_prefix('/').filter(|_| !text.contains('\n')) {
            let (name, arg) = command
                .split_once(char::is_whitespace)
                .map_or((command, ""), |(n, r)| (n, r.trim()));
            match name {
                "help" => {
                    self.state.feed.push(FeedRow::Notice {
                        is_error: false,
                        text: "Commands: /new (/reset), /clear, /undo, /skills [search], /yolo, /<skill-name> [args]\nShortcuts: @file:path[:a-b], @folder:dir, @diff, @staged, @git:N, @url:https://..., !<shell cmd>, Ctrl+P palette".into(),
                    });
                    cx.notify();
                    return;
                }
                "new" | "reset" => {
                    self.state.start_new_chat();
                    cx.notify();
                    return;
                }
                "clear" => {
                    self.state.feed.clear();
                    self.state.status_message = "Cleared visible feed".into();
                    cx.notify();
                    return;
                }
                "yolo" => {
                    self.state.yolo = !self.state.yolo;
                    self.state.status_message = if self.state.yolo {
                        "YOLO enabled".into()
                    } else {
                        "YOLO disabled".into()
                    };
                    cx.notify();
                    return;
                }
                "undo" => {
                    if let Some(sid) = self.state.selected_session_id.clone() {
                        match hermes_runtime::storage::undo_last_turn(
                            &self.state.active_profile,
                            &sid,
                        ) {
                            Ok(0) => {
                                self.state.status_message =
                                    "Nothing to undo in this session".into();
                            }
                            Ok(removed) => {
                                self.state.status_message =
                                    format!("Undid last exchange ({removed} message(s) removed)");
                                self.client
                                    .load_session(self.state.active_profile.clone(), sid);
                            }
                            Err(err) => {
                                self.state.feed.push(FeedRow::Notice {
                                    is_error: true,
                                    text: format!("Undo failed: {err}"),
                                });
                            }
                        }
                    } else {
                        self.state.status_message = "No active saved session to undo".into();
                    }
                    cx.notify();
                    return;
                }
                "skills" => {
                    let skills = hermes_runtime::context_refs::list_skill_commands(Some(
                        &self.state.active_profile,
                    ));
                    let filter = arg
                        .strip_prefix("search")
                        .unwrap_or(arg)
                        .trim()
                        .to_ascii_lowercase();
                    let matched: Vec<_> = skills
                        .into_iter()
                        .filter(|s| {
                            filter.is_empty()
                                || s.name.contains(&filter)
                                || s.description.to_ascii_lowercase().contains(&filter)
                        })
                        .collect();
                    let body = if matched.is_empty() {
                        "No installed skills found. Run `hsr skills sync` to download bundled skills.".into()
                    } else {
                        let mut lines = vec![format!(
                            "Installed skills ({}) — invoke with /<name> [prompt]:",
                            matched.len()
                        )];
                        for s in matched {
                            lines.push(format!("  /{} — {}", s.name, s.description));
                        }
                        lines.join("\n")
                    };
                    self.state.feed.push(FeedRow::Notice {
                        is_error: false,
                        text: body,
                    });
                    cx.notify();
                    return;
                }
                other => {
                    if let Some(inv) = hermes_runtime::context_refs::resolve_skill_invocation(
                        Some(&self.state.active_profile),
                        &text,
                    ) {
                        self.state.status_message = format!("Running skill /{}...", inv.name);
                        effective_query = inv.prompt;
                    } else {
                        self.state.feed.push(FeedRow::Notice {
                            is_error: true,
                            text: format!("Unknown command /{other}; type /help for commands."),
                        });
                        cx.notify();
                        return;
                    }
                }
            }
        }

        self.state.feed.push(FeedRow::User(text));
        self.state.streaming = true;
        self.state.status_message = "Starting turn...".into();
        self.client.start_turn(
            self.state.active_profile.clone(),
            self.state.selected_session_id.clone(),
            effective_query,
            self.state.yolo,
        );
        cx.notify();
    }

    fn submit_clarify(&mut self, cx: &mut Context<Self>) {
        let Some(clarify) = self.state.pending_clarify.clone() else {
            return;
        };
        let decision = clarify.build_submitted_decision();
        self.client.respond_approval(
            self.state.active_profile.clone(),
            clarify.turn_id,
            clarify.approval_id,
            decision,
        );
        self.input_target = InputTarget::Composer;
        cx.notify();
    }

    fn cancel_clarify(&mut self, cx: &mut Context<Self>) {
        let Some(clarify) = self.state.pending_clarify.clone() else {
            return;
        };
        self.client.respond_approval(
            self.state.active_profile.clone(),
            clarify.turn_id,
            clarify.approval_id,
            PendingClarify::build_cancelled_decision(),
        );
        self.input_target = InputTarget::Composer;
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        let primary = mods.control || mods.platform;

        if primary && key.eq_ignore_ascii_case("v") {
            if let Some(item) = cx.read_from_clipboard() {
                if let Some(text) = item.text() {
                    if let Some(buf) = self.active_buffer_mut() {
                        buf.insert_str(&text);
                    }
                    if self.input_target == InputTarget::Search {
                        self.client.list_sessions(
                            self.state.active_profile.clone(),
                            self.state.search_input.as_str().to_owned(),
                        );
                    }
                    cx.notify();
                }
            }
            return;
        }

        if primary && key.eq_ignore_ascii_case("u") {
            if let Some(buf) = self.active_buffer_mut() {
                buf.clear();
            }
            if self.input_target == InputTarget::Search {
                self.client
                    .list_sessions(self.state.active_profile.clone(), String::new());
            }
            cx.notify();
            return;
        }

        if primary && key.eq_ignore_ascii_case("p") && self.input_target == InputTarget::Composer {
            if self.state.composer_input.is_empty() {
                self.state.composer_input.insert_char('/');
                self.state.palette_index = 0;
                self.state.palette_dismissed = false;
                cx.notify();
            }
            return;
        }

        if primary && key.eq_ignore_ascii_case("c") && self.state.streaming {
            if let Some(tid) = self.state.active_turn_id.clone() {
                self.client
                    .cancel_turn(self.state.active_profile.clone(), tid);
            }
            return;
        }

        // Navigate and apply `@` / `/` completion palette when active in the composer
        if self.input_target == InputTarget::Composer {
            if let Some((_, items)) = self.state.compute_completions(&self.workspace_root) {
                match key {
                    "up" => {
                        self.state.palette_index = self.state.palette_index.saturating_sub(1);
                        cx.notify();
                        return;
                    }
                    "down" => {
                        if self.state.palette_index + 1 < items.len() {
                            self.state.palette_index += 1;
                        }
                        cx.notify();
                        return;
                    }
                    "tab" => {
                        let idx = self.state.palette_index.min(items.len() - 1);
                        self.state.insert_completion(&items[idx]);
                        cx.notify();
                        return;
                    }
                    "enter" if !mods.shift => {
                        let idx = self.state.palette_index.min(items.len() - 1);
                        let chosen = items[idx].clone();
                        if chosen.at_start.is_some() {
                            self.state.insert_completion(&chosen);
                            cx.notify();
                            return;
                        }
                        self.state.composer_input.clear();
                        self.state.composer_input.insert_str(&chosen.replacement);
                        self.send_composer(cx);
                        return;
                    }
                    "escape" => {
                        self.state.palette_dismissed = true;
                        cx.notify();
                        return;
                    }
                    _ => {}
                }
            }
        }

        match key {
            "left" => {
                if let Some(buf) = self.active_buffer_mut() {
                    buf.move_left();
                    cx.notify();
                }
            }
            "right" => {
                if let Some(buf) = self.active_buffer_mut() {
                    buf.move_right();
                    cx.notify();
                }
            }
            "home" => {
                if let Some(buf) = self.active_buffer_mut() {
                    buf.move_home();
                    cx.notify();
                }
            }
            "end" => {
                if let Some(buf) = self.active_buffer_mut() {
                    buf.move_end();
                    cx.notify();
                }
            }
            "backspace" => {
                if let Some(buf) = self.active_buffer_mut() {
                    if buf.backspace() {
                        if self.input_target == InputTarget::Composer {
                            self.state.palette_dismissed = false;
                            self.state.palette_index = 0;
                        } else if self.input_target == InputTarget::Search {
                            self.client.list_sessions(
                                self.state.active_profile.clone(),
                                self.state.search_input.as_str().to_owned(),
                            );
                        }
                        cx.notify();
                    }
                }
            }
            "delete" => {
                if let Some(buf) = self.active_buffer_mut() {
                    if buf.delete() {
                        if self.input_target == InputTarget::Composer {
                            self.state.palette_dismissed = false;
                            self.state.palette_index = 0;
                        } else if self.input_target == InputTarget::Search {
                            self.client.list_sessions(
                                self.state.active_profile.clone(),
                                self.state.search_input.as_str().to_owned(),
                            );
                        }
                        cx.notify();
                    }
                }
            }
            "enter" => match self.input_target {
                InputTarget::Composer => {
                    if mods.shift {
                        self.state.composer_input.insert_char('\n');
                        cx.notify();
                    } else {
                        self.send_composer(cx);
                    }
                }
                InputTarget::Search => {
                    self.client.list_sessions(
                        self.state.active_profile.clone(),
                        self.state.search_input.as_str().to_owned(),
                    );
                }
                InputTarget::Clarify => {
                    self.submit_clarify(cx);
                }
            },
            "escape" => {
                if self.state.pending_clarify.is_some() {
                    self.cancel_clarify(cx);
                } else if let Some(tid) = self.state.active_turn_id.clone() {
                    self.client
                        .cancel_turn(self.state.active_profile.clone(), tid);
                }
            }
            _ => {
                if !mods.control && !mods.alt && !mods.platform {
                    if let Some(ch) = event.keystroke.key_char.as_deref() {
                        if let Some(buf) = self.active_buffer_mut() {
                            buf.insert_str(ch);
                        }
                        if self.input_target == InputTarget::Composer {
                            self.state.palette_dismissed = false;
                            self.state.palette_index = 0;
                        } else if self.input_target == InputTarget::Search {
                            self.client.list_sessions(
                                self.state.active_profile.clone(),
                                self.state.search_input.as_str().to_owned(),
                            );
                        }
                        cx.notify();
                    }
                }
            }
        }
    }
}

fn render_markdown_blocks(text: &str, cx: &mut Context<HermesAppView>) -> Vec<gpui::AnyElement> {
    let text_main = rgb(0xe6edf6);
    let text_dim = rgb(0x8e9bb0);
    let accent = rgb(0x4ea1ff);
    let border = rgb(0x2b3446);
    let bg_code = rgb(0x10141c);

    parse_blocks(text)
        .into_iter()
        .map(|block| match block {
            MdBlock::Heading { level, text } => {
                let el = div().text_color(accent).child(SharedString::from(text));
                if level <= 2 {
                    el.text_base().into_any_element()
                } else {
                    el.text_sm().into_any_element()
                }
            }
            MdBlock::Paragraph { text } => div()
                .text_sm()
                .text_color(text_main)
                .child(SharedString::from(text))
                .into_any_element(),
            MdBlock::Bullet { text } => div()
                .flex()
                .flex_row()
                .gap_2()
                .text_sm()
                .text_color(text_main)
                .child(div().text_color(accent).child("•"))
                .child(div().flex_1().child(SharedString::from(text)))
                .into_any_element(),
            MdBlock::Quote { text } => div()
                .pl_3()
                .border_l_2()
                .border_color(accent)
                .text_sm()
                .text_color(text_dim)
                .child(SharedString::from(text))
                .into_any_element(),
            MdBlock::CodeBlock { lang, code } => {
                let copy_text = code.clone();
                let header_label = lang.unwrap_or_else(|| "code".into());
                div()
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
                            .text_xs()
                            .text_color(rgb(0xcbd5e1))
                            .child(SharedString::from(code)),
                    )
                    .into_any_element()
            }
        })
        .collect()
}

impl Render for HermesAppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg_app = rgb(0x11141a);
        let bg_sidebar = rgb(0x161b24);
        let bg_card = rgb(0x1d2432);
        let bg_user = rgb(0x223859);
        let border = rgb(0x2b3446);
        let text_main = rgb(0xe6edf6);
        let text_dim = rgb(0x8e9bb0);
        let accent = rgb(0x4ea1ff);
        let green = rgb(0x3fb950);
        let red = rgb(0xf85149);
        let amber = rgb(0xd29922);

        let profiles_ui: Vec<_> = self
            .state
            .profiles
            .iter()
            .map(|prof| {
                let path = PathBuf::from(&prof.path);
                let active = path == self.state.active_profile;
                let label = SharedString::from(prof.name.clone());
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if active { accent } else { bg_card })
                    .text_color(if active { rgb(0x0b1017) } else { text_main })
                    .text_xs()
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, _, cx| {
                            view.state.reset_for_profile(path.clone());
                            view.client.connect_and_sync(
                                path.clone(),
                                view.state.search_input.as_str().to_owned(),
                            );
                            cx.notify();
                        }),
                    )
            })
            .collect();

        let sessions_ui: Vec<_> = self
            .state
            .sessions
            .iter()
            .map(|sess| {
                let sid = sess.id.clone();
                let selected = self.state.selected_session_id.as_deref() == Some(sid.as_str());
                let title = sess
                    .title
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| sid.clone());
                let subtitle = match sess.model.as_deref() {
                    Some(model) => format!("{} msgs · {model}", sess.message_count),
                    None => format!("{} msgs", sess.message_count),
                };
                div()
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if selected { bg_user } else { bg_sidebar })
                    .border_1()
                    .border_color(if selected { accent } else { border })
                    .child(
                        div()
                            .text_sm()
                            .text_color(text_main)
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(text_dim)
                            .child(SharedString::from(subtitle)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, _, cx| {
                            view.client
                                .load_session(view.state.active_profile.clone(), sid.clone());
                            cx.notify();
                        }),
                    )
            })
            .collect();

        let feed_rows = self.state.feed.clone();
        let feed_ui: Vec<_> = feed_rows
            .iter()
            .map(|row| match row {
                FeedRow::User(text) => div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .max_w(px(720.0))
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(bg_user)
                            .text_color(text_main)
                            .text_sm()
                            .child(SharedString::from(text.clone())),
                    )
                    .into_any_element(),
                FeedRow::Assistant(text) => {
                    let blocks = render_markdown_blocks(text, cx);
                    div()
                        .w_full()
                        .flex()
                        .justify_start()
                        .child(
                            div()
                                .max_w(px(780.0))
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .bg(bg_card)
                                .border_1()
                                .border_color(border)
                                .flex()
                                .flex_col()
                                .gap_1()
                                .children(blocks),
                        )
                        .into_any_element()
                }
                FeedRow::Tool {
                    name,
                    detail,
                    summary,
                    state,
                } => {
                    let (icon, col) = match state {
                        ToolCardState::Running => ("⚙", amber),
                        ToolCardState::Succeeded => ("✓", green),
                        ToolCardState::Failed => ("✗", red),
                    };
                    let mut line = format!("{icon} {name}");
                    if let Some(d) = detail {
                        line.push_str(" — ");
                        line.push_str(d);
                    }
                    if let Some(s) = summary {
                        line.push_str(" · ");
                        line.push_str(s);
                    }
                    div()
                        .w_full()
                        .flex()
                        .justify_start()
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(0x181e29))
                                .border_1()
                                .border_color(border)
                                .text_xs()
                                .text_color(col)
                                .child(SharedString::from(line)),
                        )
                        .into_any_element()
                }
                FeedRow::Notice { is_error, text } => div()
                    .w_full()
                    .flex()
                    .justify_start()
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .text_xs()
                            .text_color(if *is_error { red } else { text_dim })
                            .child(SharedString::from(text.clone())),
                    )
                    .into_any_element(),
            })
            .collect();

        let approval_panel = self.state.pending_approval.clone().map(|pending| {
            let tid_a = pending.turn_id.clone();
            let tid_d = pending.turn_id.clone();
            let aid = pending.approval_id;
            let kind_label = match pending.request.action {
                ApprovalAction::CreateFile => "Create file",
                ApprovalAction::ReplaceFile => "Edit file",
                ApprovalAction::RunCommand => "Run command",
                ApprovalAction::RunCode => "Run Python code",
                ApprovalAction::ManageSkills => "Modify skills",
                ApprovalAction::Clarify => "Clarify question",
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
                        .p_2()
                        .rounded_md()
                        .bg(rgb(0x14161c))
                        .text_xs()
                        .text_color(text_main)
                        .child(SharedString::from(preview)),
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
                                .child("Approve")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, _, cx| {
                                        view.client.respond_approval(
                                            view.state.active_profile.clone(),
                                            tid_a.clone(),
                                            aid,
                                            ApprovalDecision::Approved,
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
                                            tid_d.clone(),
                                            aid,
                                            ApprovalDecision::Denied,
                                        );
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
        });

        let clarify_panel = self.state.pending_clarify.clone().map(|clarify| {
            let total = clarify.questions.len();
            let active_idx = clarify.active_index.min(total.saturating_sub(1));
            let active_q = clarify.questions[active_idx].clone();
            let qid = active_q.qid.clone();
            let multi = active_q.multi_select;
            let chosen_set = clarify.selected.get(&qid).cloned().unwrap_or_default();
            let custom_display = clarify
                .custom_answers
                .get(&qid)
                .cloned()
                .unwrap_or_default()
                .display(
                    self.input_target == InputTarget::Clarify,
                    "Type a custom answer (Enter to submit)...",
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
                        .text_color(text_main)
                        .child(SharedString::from(custom_display))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, _, window, cx| {
                                view.input_target = InputTarget::Clarify;
                                window.focus(&view.focus_handle);
                                cx.notify();
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
        });

        let completion_panel = if self.input_target == InputTarget::Composer {
            self.state
                .compute_completions(&self.workspace_root)
                .map(|(title, items)| {
                    let selected_idx = self.state.palette_index.min(items.len().saturating_sub(1));
                    let rows: Vec<_> = items
                        .into_iter()
                        .enumerate()
                        .map(|(idx, item)| {
                            let is_sel = idx == selected_idx;
                            let item_clone = item.clone();
                            div()
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
                                        .text_xs()
                                        .text_color(if is_sel { accent } else { text_main })
                                        .child(SharedString::from(item.display)),
                                )
                                .child(
                                    div()
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
                                .child(SharedString::from(title)),
                        )
                        .children(rows)
                })
        } else {
            None
        };

        let search_display = self.state.search_input.display(
            self.input_target == InputTarget::Search,
            "Search sessions...",
        );
        let composer_display = self.state.composer_input.display(
            self.input_target == InputTarget::Composer,
            "Message Hermes... (@file/@diff, /help, !<cmd>, Shift+Enter newline)",
        );
        let header_session = self
            .state
            .selected_session_title
            .clone()
            .or_else(|| self.state.selected_session_id.clone())
            .unwrap_or_else(|| "New session".into());

        div()
            .id("hermes-gui-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_row()
            .bg(bg_app)
            .text_color(text_main)
            // Sidebar
            .child(
                div()
                    .w(px(280.0))
                    .h_full()
                    .bg(bg_sidebar)
                    .border_r_1()
                    .border_color(border)
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(div().text_base().text_color(accent).child("Hermes GPUI"))
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(bg_card)
                                    .text_xs()
                                    .child("+ New")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, _, window, cx| {
                                            view.state.start_new_chat();
                                            view.input_target = InputTarget::Composer;
                                            window.focus(&view.focus_handle);
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(text_dim).child("Profiles"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_1()
                                    .children(profiles_ui),
                            ),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_text()
                            .bg(bg_card)
                            .border_1()
                            .border_color(if self.input_target == InputTarget::Search {
                                accent
                            } else {
                                border
                            })
                            .text_xs()
                            .text_color(if self.state.search_input.is_empty() {
                                text_dim
                            } else {
                                text_main
                            })
                            .child(SharedString::from(search_display))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|view, _, window, cx| {
                                    view.input_target = InputTarget::Search;
                                    window.focus(&view.focus_handle);
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("sessions-list")
                            .flex_1()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(sessions_ui),
                    ),
            )
            // Main chat column
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    // Header
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .border_b_1()
                            .border_color(border)
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
                                            .text_sm()
                                            .text_color(text_main)
                                            .child(SharedString::from(header_session)),
                                    )
                                    .child(div().text_xs().text_color(text_dim).child(
                                        SharedString::from(self.state.status_message.clone()),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(if self.state.owner_connected {
                                                green
                                            } else {
                                                amber
                                            })
                                            .child(if self.state.owner_connected {
                                                "● Owner connected"
                                            } else {
                                                "○ Reconnecting"
                                            }),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .bg(if self.state.yolo { amber } else { bg_card })
                                            .text_color(if self.state.yolo {
                                                rgb(0x0b1017)
                                            } else {
                                                text_main
                                            })
                                            .text_xs()
                                            .child("YOLO")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|view, _, _, cx| {
                                                    view.state.yolo = !view.state.yolo;
                                                    cx.notify();
                                                }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .bg(bg_card)
                                            .text_xs()
                                            .child("Reconnect")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|view, _, _, cx| {
                                                    view.client.connect_and_sync(
                                                        view.state.active_profile.clone(),
                                                        view.state.search_input.as_str().to_owned(),
                                                    );
                                                    cx.notify();
                                                }),
                                            ),
                                    ),
                            ),
                    )
                    // Scrollable feed
                    .child(
                        div()
                            .id("chat-feed")
                            .flex_1()
                            .overflow_y_scroll()
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .children(feed_ui),
                    )
                    // Pending approval, clarify modal, or `@` / `/` completion popup
                    .children(approval_panel)
                    .children(clarify_panel)
                    .children(completion_panel)
                    // Composer bar
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .border_t_1()
                            .border_color(border)
                            .flex()
                            .flex_row()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_h(px(38.0))
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_text()
                                    .bg(bg_card)
                                    .border_1()
                                    .border_color(if self.input_target == InputTarget::Composer {
                                        accent
                                    } else {
                                        border
                                    })
                                    .text_sm()
                                    .text_color(if self.state.composer_input.is_empty() {
                                        text_dim
                                    } else {
                                        text_main
                                    })
                                    .child(SharedString::from(composer_display))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, _, window, cx| {
                                            view.input_target = InputTarget::Composer;
                                            window.focus(&view.focus_handle);
                                            cx.notify();
                                        }),
                                    ),
                            )
                            .child(if self.state.streaming {
                                div()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(red)
                                    .text_color(rgb(0xffffff))
                                    .text_sm()
                                    .child("Cancel")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, _, _, cx| {
                                            if let Some(tid) = view.state.active_turn_id.clone() {
                                                view.client.cancel_turn(
                                                    view.state.active_profile.clone(),
                                                    tid,
                                                );
                                                cx.notify();
                                            }
                                        }),
                                    )
                            } else {
                                div()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(accent)
                                    .text_color(rgb(0x0b1017))
                                    .text_sm()
                                    .child("Send")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, _, _, cx| {
                                            view.send_composer(cx);
                                        }),
                                    )
                            }),
                    ),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1160.0), px(780.0)), cx);
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(HermesAppView::new);
                let handle = view.read(cx).focus_handle.clone();
                window.focus(&handle);
                view
            },
        );
    });
}
