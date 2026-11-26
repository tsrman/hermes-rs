//! `hsr-gui`: standalone GPUI desktop application for Hermes Agent backed by the shared
//! cross-platform local session owner (`hsr owner`, protocol v2).

mod editor;
mod markdown;
mod owner_client;
mod setup_state;
mod state;
mod workspace;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

#[cfg(target_os = "windows")]
use gpui::WindowControlArea;
use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, ClipboardItem, Context,
    ExternalPaths, FocusHandle, HighlightStyle, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, PathPromptOptions, Pixels, Point, Rgba, ScrollHandle, ScrollWheelEvent,
    SharedString, StyledText, TextLayout, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use hermes_runtime::config::profile_home;
use hermes_runtime::local_owner::ApprovalScope;
use hermes_runtime::turn::{ApprovalAction, ApprovalDecision};

use crate::editor::TextBuffer;
use crate::markdown::{parse_blocks, MdBlock, TableAlign};
use crate::owner_client::{BackendUpdate, OwnerClient, ScopedUpdate};
use crate::setup_state::{SetupDraft, SetupField, GUI_PROVIDER_PRESETS, GUI_WEB_BACKENDS};
use crate::state::{FeedRow, GuiState, PendingClarify, ToolCardState};
use crate::workspace::{
    attach_path_to_workspace, format_prompt_with_attachments, format_size, resolve_workspace_dir,
    short_folder_label, WorkspacePrefs,
};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum InputTarget {
    Composer,
    Search,
    WorkspacePath,
    Clarify,
    Setup(SetupField),
}

#[derive(Clone)]
struct InputLayoutSnapshot {
    layout: TextLayout,
    /// Byte offset inside the rendered string where the synthetic `|` caret was inserted, if any.
    caret_inserted_at: Option<usize>,
    /// True when each source character was rendered as `•`.
    masked: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FeedTextKey {
    row_idx: usize,
    block_idx: usize,
    sub_idx: usize,
}

#[derive(Clone)]
struct FeedLayoutSnapshot {
    text: String,
    layout: TextLayout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FeedTextSelection {
    key: FeedTextKey,
    anchor: usize,
    focus: usize,
    dragging: bool,
}

impl FeedTextSelection {
    fn range(&self) -> Option<std::ops::Range<usize>> {
        let (start, end) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        (start < end).then_some(start..end)
    }
}

struct HermesAppView {
    state: GuiState,
    setup: SetupDraft,
    workspace_prefs: WorkspacePrefs,
    workspace_root: PathBuf,
    client: OwnerClient,
    rx: Receiver<ScopedUpdate>,
    focus_handle: FocusHandle,
    input_target: InputTarget,
    input_layouts: HashMap<InputTarget, InputLayoutSnapshot>,
    dragging_input: Option<InputTarget>,
    feed_layouts: HashMap<FeedTextKey, FeedLayoutSnapshot>,
    feed_selection: Option<FeedTextSelection>,
    feed_scroll: ScrollHandle,
    feed_follow_bottom: bool,
}

impl HermesAppView {
    fn new(cx: &mut Context<Self>) -> Self {
        let home = profile_home(None)
            .ok()
            .flatten()
            .unwrap_or_else(|| PathBuf::from(".hsr"));
        hermes_runtime::context_refs::prewarm_skill_commands(Some(&home));
        let (mut client, rx) = OwnerClient::new().expect("tokio runtime");
        client.connect_and_sync(home.clone());
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

        let workspace_prefs = WorkspacePrefs::load(&home);
        let workspace_root = workspace_prefs.resolve_initial();
        let setup = SetupDraft::load_from_profile(&home);
        let initial_target = if setup.open {
            InputTarget::Setup(SetupField::Model)
        } else {
            InputTarget::Composer
        };
        let mut state = GuiState::new(home);
        state.set_workspace_path_text(&workspace_root);
        Self {
            state,
            setup,
            workspace_prefs,
            workspace_root,
            client,
            rx,
            focus_handle,
            input_target: initial_target,
            input_layouts: HashMap::new(),
            dragging_input: None,
            feed_layouts: HashMap::new(),
            feed_selection: None,
            feed_scroll: ScrollHandle::new(),
            feed_follow_bottom: true,
        }
    }

    fn drain_updates(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.rx.try_recv() {
            let Some(update) = self.client.current_update(reply) else {
                continue;
            };
            changed = true;
            match update {
                BackendUpdate::Connected {
                    profile,
                    profiles,
                    view_revision,
                    active_turns,
                } => {
                    if profile != self.state.active_profile {
                        continue;
                    }
                    self.state.owner_connected = true;
                    self.state.profiles = profiles;
                    self.client
                        .list_sessions(profile, self.state.search_input.as_str().to_owned());
                    if self.client.view_revision() != view_revision {
                        continue;
                    }
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
                        self.client.connect_and_sync(profile.clone());
                        if let Some(sid) = self.state.selected_session_id.clone() {
                            self.load_session(sid);
                        }
                    }
                }
                BackendUpdate::SessionsLoaded(sessions) => {
                    self.state.sessions = sessions;
                }
                BackendUpdate::SessionDeleted { session_id } => {
                    if self.state.selected_session_id.as_deref() == Some(session_id.as_str()) {
                        self.client.change_view();
                    }
                    self.state.handle_session_deleted(&session_id);
                    self.client.list_sessions(
                        self.state.active_profile.clone(),
                        self.state.search_input.as_str().to_owned(),
                    );
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
                    self.client.subscribe_turn(
                        self.state.active_profile.clone(),
                        turn_id.clone(),
                        0,
                    );
                    self.state.active_turn_id = Some(turn_id);
                    self.state.last_seq = 0;
                    self.state.streaming = true;
                    self.state.status_message = "Streaming...".into();
                }
                BackendUpdate::TurnEvent { turn_id, event } => {
                    let finished = matches!(
                        event.event,
                        hermes_runtime::local_owner::TurnStreamEvent::Done { .. }
                    );
                    if !self.state.apply_turn_event(&turn_id, event) {
                        continue;
                    }
                    self.state.owner_connected = true;
                    if self.state.pending_clarify.is_some() {
                        self.input_target = InputTarget::Clarify;
                    } else if self.input_target == InputTarget::Clarify {
                        self.input_target = InputTarget::Composer;
                    }
                    if finished {
                        self.client.change_view();
                        if self.state.reload_after_gap {
                            if let Some(id) = self.state.selected_session_id.clone() {
                                self.load_session(id);
                            }
                        }
                        self.client.list_sessions(
                            self.state.active_profile.clone(),
                            self.state.search_input.as_str().to_owned(),
                        );
                    }
                }
                BackendUpdate::SkillsSynced {
                    profile,
                    count,
                    message,
                    is_error,
                } => {
                    if profile == self.state.active_profile {
                        self.setup.syncing_skills = false;
                        self.setup.installed_skills_count = count;
                        self.setup.notice = Some((is_error, message.clone()));
                        self.state.status_message = message;
                    }
                }
                BackendUpdate::OAuthProgress { profile, message } => {
                    if profile == self.state.active_profile {
                        self.setup.notice = Some((false, message.clone()));
                        self.state.status_message = message;
                    }
                }
                BackendUpdate::OAuthFinished {
                    profile,
                    message,
                    is_error,
                } => {
                    if profile == self.state.active_profile {
                        self.setup.oauth_in_progress = false;
                        self.setup.notice = Some((is_error, message.clone()));
                        self.state.status_message = message;
                    }
                }
                BackendUpdate::ConversationError(msg) => {
                    self.state.streaming = false;
                    self.state.loading_session = false;
                    self.state.pending_approval = None;
                    self.state.pending_clarify = None;
                    self.state.active_turn_id = None;
                    self.show_backend_error(msg);
                }
                BackendUpdate::Error(msg) => self.show_backend_error(msg),
            }
        }
        changed
    }

    fn show_backend_error(&mut self, msg: String) {
        self.state.feed.push(FeedRow::Notice {
            is_error: true,
            text: msg.clone(),
        });
        self.state.status_message = msg;
    }

    fn load_session(&mut self, id: String) {
        self.state.select_session(id.clone());
        self.feed_selection = None;
        self.feed_follow_bottom = true;
        self.feed_scroll.scroll_to_bottom();
        self.input_target = InputTarget::Composer;
        self.client
            .load_session(self.state.active_profile.clone(), id);
    }

    fn new_chat(&mut self) {
        self.client.change_view();
        self.state.start_new_chat();
        self.feed_selection = None;
        self.feed_follow_bottom = true;
        self.feed_scroll.scroll_to_bottom();
        self.input_target = InputTarget::Composer;
    }

    fn buffer_mut(&mut self, target: InputTarget) -> Option<&mut TextBuffer> {
        match target {
            InputTarget::Composer => Some(&mut self.state.composer_input),
            InputTarget::Search => Some(&mut self.state.search_input),
            InputTarget::WorkspacePath => Some(&mut self.state.workspace_path_input),
            InputTarget::Clarify => self
                .state
                .pending_clarify
                .as_mut()
                .and_then(PendingClarify::active_buffer_mut),
            InputTarget::Setup(field) => Some(self.setup.buffer_mut(field)),
        }
    }

    fn active_buffer_mut(&mut self) -> Option<&mut TextBuffer> {
        self.buffer_mut(self.input_target)
    }

    fn hit_test_input(&self, target: InputTarget, position: Point<Pixels>, text: &str) -> usize {
        let Some(snap) = self.input_layouts.get(&target) else {
            return text.len();
        };
        let rendered_idx = match snap.layout.index_for_position(position) {
            Ok(i) | Err(i) => i,
        };
        let adj = match snap.caret_inserted_at {
            Some(caret) if rendered_idx > caret => rendered_idx.saturating_sub(1),
            _ => rendered_idx,
        };
        if snap.masked {
            // Each masked character is `•` (3 UTF-8 bytes).
            let char_idx = adj / '•'.len_utf8();
            text.char_indices()
                .nth(char_idx)
                .map_or(text.len(), |(byte_idx, _)| byte_idx)
        } else {
            let mut idx = adj.min(text.len());
            while idx > 0 && !text.is_char_boundary(idx) {
                idx -= 1;
            }
            idx
        }
    }

    fn handle_input_mouse_down(
        &mut self,
        target: InputTarget,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.feed_selection = None;
        self.input_target = target;
        window.focus(&self.focus_handle);
        let text_owned = self
            .buffer_mut(target)
            .map(|b| b.as_str().to_owned())
            .unwrap_or_default();
        let idx = self.hit_test_input(target, event.position, &text_owned);
        if let Some(buf) = self.buffer_mut(target) {
            match event.click_count {
                0 | 1 => {
                    buf.set_cursor(idx, event.modifiers.shift);
                    self.dragging_input = Some(target);
                }
                2 => {
                    buf.select_word_at(idx);
                    self.dragging_input = None;
                }
                _ => {
                    buf.select_all();
                    self.dragging_input = None;
                }
            }
        }
        cx.notify();
    }

    fn hit_test_feed(&self, key: FeedTextKey, position: Point<Pixels>) -> Option<(usize, usize)> {
        let snap = self.feed_layouts.get(&key)?;
        let raw = match snap.layout.index_for_position(position) {
            Ok(i) | Err(i) => i,
        };
        let mut idx = raw.min(snap.text.len());
        while idx > 0 && !snap.text.is_char_boundary(idx) {
            idx -= 1;
        }
        Some((idx, snap.text.len()))
    }

    fn handle_feed_mouse_down(
        &mut self,
        key: FeedTextKey,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging_input = None;
        if let Some(buf) = self.active_buffer_mut() {
            buf.clear_selection();
        }
        window.focus(&self.focus_handle);
        let Some((idx, len)) = self.hit_test_feed(key, event.position) else {
            return;
        };
        match event.click_count {
            0 | 1 => {
                if event.modifiers.shift {
                    if let Some(prev) = self.feed_selection.filter(|s| s.key == key) {
                        self.feed_selection = Some(FeedTextSelection {
                            key,
                            anchor: prev.anchor,
                            focus: idx,
                            dragging: true,
                        });
                        cx.notify();
                        return;
                    }
                }
                self.feed_selection = Some(FeedTextSelection {
                    key,
                    anchor: idx,
                    focus: idx,
                    dragging: true,
                });
            }
            2 => {
                if let Some(snap) = self.feed_layouts.get(&key) {
                    let (start, end) = word_bounds_at(&snap.text, idx);
                    self.feed_selection = Some(FeedTextSelection {
                        key,
                        anchor: start,
                        focus: end,
                        dragging: false,
                    });
                }
            }
            _ => {
                self.feed_selection = Some(FeedTextSelection {
                    key,
                    anchor: 0,
                    focus: len,
                    dragging: false,
                });
            }
        }
        cx.notify();
    }

    fn selected_feed_text(&self) -> Option<String> {
        let sel = self.feed_selection?;
        let range = sel.range()?;
        let snap = self.feed_layouts.get(&sel.key)?;
        snap.text.get(range).map(str::to_owned)
    }

    fn handle_input_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.dragging_input {
            if event.pressed_button != Some(MouseButton::Left) {
                self.dragging_input = None;
                return;
            }
            let text_owned = self
                .buffer_mut(target)
                .map(|b| b.as_str().to_owned())
                .unwrap_or_default();
            let idx = self.hit_test_input(target, event.position, &text_owned);
            if let Some(buf) = self.buffer_mut(target) {
                buf.set_cursor(idx, true);
                cx.notify();
            }
            return;
        }
        if let Some(mut sel) = self.feed_selection {
            if !sel.dragging {
                return;
            }
            if event.pressed_button != Some(MouseButton::Left) {
                sel.dragging = false;
                self.feed_selection = if sel.anchor == sel.focus {
                    None
                } else {
                    Some(sel)
                };
                cx.notify();
                return;
            }
            if let Some((idx, _)) = self.hit_test_feed(sel.key, event.position) {
                if sel.focus != idx {
                    sel.focus = idx;
                    self.feed_selection = Some(sel);
                    cx.notify();
                }
            }
        }
    }

    fn set_workspace_root(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let canonical = dir.canonicalize().unwrap_or(dir);
        self.workspace_root = canonical.clone();
        self.workspace_prefs
            .record_and_save(&self.state.active_profile, &canonical);
        self.state.set_workspace_path_text(&canonical);
        self.state.attachments.clear();
        self.state.status_message = format!("Project folder: {}", canonical.display());
        cx.notify();
    }

    fn apply_workspace_path_input(&mut self, cx: &mut Context<Self>) {
        let raw = self.state.workspace_path_input.as_str().to_owned();
        match resolve_workspace_dir(&raw, &self.workspace_root) {
            Ok(dir) => {
                self.input_target = InputTarget::Composer;
                self.set_workspace_root(dir, cx);
            }
            Err(err) => {
                self.state.feed.push(FeedRow::Notice {
                    is_error: true,
                    text: err.clone(),
                });
                self.state.status_message = err;
                cx.notify();
            }
        }
    }

    fn pick_workspace_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select project working directory".into()),
        });
        let revision = self.client.view_revision();
        let workspace = self.workspace_root.clone();
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                if revision != view.client.view_revision() || workspace != view.workspace_root {
                    return;
                }
                match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(dir) = paths.into_iter().next() {
                        view.set_workspace_root(dir, cx);
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => {
                    let msg = format!(
                        "Folder picker unavailable ({err}); edit the Project Folder path or run /cd <path>."
                    );
                    view.state.feed.push(FeedRow::Notice {
                        is_error: true,
                        text: msg.clone(),
                    });
                    view.state.status_message = msg;
                    cx.notify();
                }
                Err(_) => {}
                }
            });
        })
        .detach();
    }

    fn pick_attach_files(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files to project & message".into()),
        });
        let revision = self.client.view_revision();
        let workspace = self.workspace_root.clone();
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                if revision != view.client.view_revision() || workspace != view.workspace_root {
                    return;
                }
                match result {
                Ok(Ok(Some(paths))) => {
                    view.attach_paths(&paths, cx);
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => {
                    let msg = format!(
                        "File picker unavailable ({err}); drag & drop files onto the window or run /attach <path>."
                    );
                    view.state.feed.push(FeedRow::Notice {
                        is_error: true,
                        text: msg.clone(),
                    });
                    view.state.status_message = msg;
                    cx.notify();
                }
                Err(_) => {}
                }
            });
        })
        .detach();
    }

    fn attach_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        // Dropping a single folder switches the active project workspace.
        if paths.len() == 1 && paths[0].is_dir() {
            self.set_workspace_root(paths[0].clone(), cx);
            return;
        }
        let mut added = 0usize;
        let mut copied = 0usize;
        for path in paths {
            match attach_path_to_workspace(path, &self.workspace_root) {
                Ok(att) => {
                    if att.imported {
                        copied += 1;
                    }
                    if self.state.add_attachment(att) {
                        added += 1;
                    }
                }
                Err(err) => {
                    self.state.feed.push(FeedRow::Notice {
                        is_error: true,
                        text: err.clone(),
                    });
                    self.state.status_message = err;
                }
            }
        }
        if added > 0 {
            self.state.status_message = if copied > 0 {
                format!("Attached {added} file(s) ({copied} copied into .hermes-attachments/)")
            } else {
                format!("Attached {added} workspace file(s)")
            };
        }
        cx.notify();
    }

    fn save_setup(&mut self, cx: &mut Context<Self>) {
        match self.setup.save(&self.state.active_profile) {
            Ok(msg) => {
                self.state.status_message = msg;
                self.client
                    .connect_and_sync(self.state.active_profile.clone());
            }
            Err(err) => {
                self.setup.notice = Some((true, err.clone()));
                self.state.status_message = err;
            }
        }
        cx.notify();
    }

    fn send_composer(&mut self, cx: &mut Context<Self>) {
        let text = self.state.composer_input.as_str().trim().to_owned();
        if (text.is_empty() && self.state.attachments.is_empty())
            || self.state.streaming
            || self.state.loading_session
        {
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
                        text: "Commands: /cd [dir], /attach [file], /setup, /new (/reset), /clear, /undo, /skills [search], /yolo, /<skill-name> [args]\nShortcuts: @file:path[:a-b], @folder:dir, @diff, @staged, @git:N, @url:https://..., !<shell cmd>, Ctrl+P palette, drag & drop files/folders".into(),
                    });
                    cx.notify();
                    return;
                }
                "cd" => {
                    if arg.is_empty() {
                        self.pick_workspace_folder(cx);
                    } else {
                        match resolve_workspace_dir(arg, &self.workspace_root) {
                            Ok(dir) => self.set_workspace_root(dir, cx),
                            Err(err) => {
                                self.state.feed.push(FeedRow::Notice {
                                    is_error: true,
                                    text: err.clone(),
                                });
                                self.state.status_message = err;
                                cx.notify();
                            }
                        }
                    }
                    return;
                }
                "attach" => {
                    if arg.is_empty() {
                        self.pick_attach_files(cx);
                    } else {
                        let expanded = if let Some(rest) =
                            arg.strip_prefix("~/").or_else(|| arg.strip_prefix("~\\"))
                        {
                            std::env::var_os("HOME")
                                .or_else(|| std::env::var_os("USERPROFILE"))
                                .map(|h| PathBuf::from(h).join(rest))
                                .unwrap_or_else(|| PathBuf::from(arg))
                        } else {
                            PathBuf::from(arg)
                        };
                        let path = if expanded.is_absolute() {
                            expanded
                        } else {
                            self.workspace_root.join(expanded)
                        };
                        self.attach_paths(&[path], cx);
                    }
                    return;
                }
                "setup" => {
                    self.setup = SetupDraft::load_from_profile(&self.state.active_profile);
                    self.setup.open = true;
                    self.input_target = InputTarget::Setup(SetupField::Model);
                    cx.notify();
                    return;
                }
                "new" | "reset" => {
                    self.new_chat();
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
                                self.load_session(sid);
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
                        "No installed skills found. Open ⚙ Setup to download bundled skills.".into()
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

        let attachments = std::mem::take(&mut self.state.attachments);
        let display_user = if text.is_empty() {
            format!(
                "[Attached: {}]",
                attachments
                    .iter()
                    .map(|a| a.rel_path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else if attachments.is_empty() {
            text
        } else {
            format!(
                "{text}\n\n[Attached: {}]",
                attachments
                    .iter()
                    .map(|a| a.rel_path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let base_query = if effective_query.is_empty() {
            "Please inspect the attached files.".to_owned()
        } else {
            effective_query
        };
        let final_query = format_prompt_with_attachments(&base_query, &attachments);

        self.state.feed.push(FeedRow::User(display_user));
        self.feed_follow_bottom = true;
        self.feed_scroll.scroll_to_bottom();
        self.state.streaming = true;
        self.state.status_message = "Starting turn...".into();
        self.client.start_turn(
            self.state.active_profile.clone(),
            self.state.selected_session_id.clone(),
            final_query,
            self.workspace_root.clone(),
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
            ApprovalScope::Once,
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
            ApprovalScope::Once,
        );
        self.input_target = InputTarget::Composer;
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        let primary = mods.control || mods.platform;

        if (mods.alt && key.eq_ignore_ascii_case("f4"))
            || (primary && key.eq_ignore_ascii_case("q"))
        {
            window.remove_window();
            cx.quit();
            return;
        }

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

        if primary && key.eq_ignore_ascii_case("a") {
            if let Some(buf) = self.active_buffer_mut() {
                buf.select_all();
                cx.notify();
            }
            return;
        }

        if primary && key.eq_ignore_ascii_case("c") {
            let selected = self
                .active_buffer_mut()
                .and_then(|buf| buf.selected_text().map(str::to_owned));
            if let Some(sel) = selected {
                cx.write_to_clipboard(ClipboardItem::new_string(sel));
                return;
            }
            if let Some(sel) = self.selected_feed_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(sel));
                self.state.status_message = "Copied selection to clipboard".into();
                cx.notify();
                return;
            }
            if self.state.streaming {
                if let Some(tid) = self.state.active_turn_id.clone() {
                    self.client
                        .cancel_turn(self.state.active_profile.clone(), tid);
                }
            }
            return;
        }

        if primary && key.eq_ignore_ascii_case("x") {
            let selected = self
                .active_buffer_mut()
                .and_then(|buf| buf.selected_text().map(str::to_owned));
            if let Some(sel) = selected {
                cx.write_to_clipboard(ClipboardItem::new_string(sel));
                if let Some(buf) = self.active_buffer_mut() {
                    buf.delete_selection();
                }
                if self.input_target == InputTarget::Search {
                    self.client.list_sessions(
                        self.state.active_profile.clone(),
                        self.state.search_input.as_str().to_owned(),
                    );
                }
                cx.notify();
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

        if key == "escape" && self.feed_selection.take().is_some() {
            cx.notify();
            return;
        }

        if key == "escape" && self.state.confirm_delete_session_id.take().is_some() {
            cx.notify();
            return;
        }

        match key {
            "left" => {
                if let Some(buf) = self.active_buffer_mut() {
                    buf.move_left(mods.shift);
                    cx.notify();
                }
            }
            "right" => {
                if let Some(buf) = self.active_buffer_mut() {
                    buf.move_right(mods.shift);
                    cx.notify();
                }
            }
            "home" => {
                if let Some(buf) = self.active_buffer_mut() {
                    buf.move_home(mods.shift);
                    cx.notify();
                }
            }
            "end" => {
                if let Some(buf) = self.active_buffer_mut() {
                    buf.move_end(mods.shift);
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
                InputTarget::WorkspacePath => {
                    self.apply_workspace_path_input(cx);
                }
                InputTarget::Clarify => {
                    self.submit_clarify(cx);
                }
                InputTarget::Setup(_) => {
                    self.save_setup(cx);
                }
            },
            "escape" => {
                if self.setup.open {
                    self.setup.open = false;
                    self.input_target = InputTarget::Composer;
                    cx.notify();
                } else if self.input_target == InputTarget::WorkspacePath {
                    self.state.set_workspace_path_text(&self.workspace_root);
                    self.input_target = InputTarget::Composer;
                    cx.notify();
                } else if self.state.pending_clarify.is_some() {
                    self.cancel_clarify(cx);
                } else if let Some(tid) = self.state.active_turn_id.clone() {
                    self.client
                        .cancel_turn(self.state.active_profile.clone(), tid);
                }
            }
            _ => {
                if let Some(ch) = text_from_keystroke(
                    key,
                    event.keystroke.key_char.as_deref(),
                    mods.control,
                    mods.alt,
                    mods.platform,
                ) {
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

/// Extract insertable text from a GPUI keystroke.
///
/// On Windows, `gpui 0.2.2` maps `VK_SPACE` via `parse_immutable` to `key = "space"` with
/// `key_char = None`, so `"space"` must be handled explicitly alongside `key_char`.
fn text_from_keystroke<'a>(
    key: &str,
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
    key_char.filter(|s| !s.is_empty())
}

#[allow(clippy::too_many_arguments)]
fn render_text_field_content(
    full_text: &str,
    cursor: usize,
    selection: Option<std::ops::Range<usize>>,
    focused: bool,
    masked: bool,
    placeholder: &str,
    text_main: Rgba,
    text_dim: Rgba,
    caret: Rgba,
    caret_h: f32,
) -> (gpui::AnyElement, Option<InputLayoutSnapshot>) {
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

#[allow(clippy::too_many_arguments)]
fn render_buffer_input(
    layouts: &mut HashMap<InputTarget, InputLayoutSnapshot>,
    target: InputTarget,
    buf: &TextBuffer,
    focused: bool,
    placeholder: &str,
    text_main: Rgba,
    text_dim: Rgba,
    caret: Rgba,
    caret_h: f32,
) -> gpui::AnyElement {
    let (el, snap) = render_text_field_content(
        buf.as_str(),
        buf.cursor(),
        buf.selection_range(),
        focused,
        false,
        placeholder,
        text_main,
        text_dim,
        caret,
        caret_h,
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

#[allow(clippy::too_many_arguments)]
fn render_masked_input(
    layouts: &mut HashMap<InputTarget, InputLayoutSnapshot>,
    target: InputTarget,
    buf: &TextBuffer,
    focused: bool,
    placeholder: &str,
    text_main: Rgba,
    text_dim: Rgba,
    caret: Rgba,
    caret_h: f32,
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
        &masked_text,
        masked_cursor,
        masked_sel,
        focused,
        true,
        placeholder,
        text_main,
        text_dim,
        caret,
        caret_h,
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

fn word_bounds_at(text: &str, idx: usize) -> (usize, usize) {
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

fn render_selectable_feed_text(
    feed_layouts: &mut HashMap<FeedTextKey, FeedLayoutSnapshot>,
    active_selection: Option<FeedTextSelection>,
    key: FeedTextKey,
    text: String,
    cx: &mut Context<HermesAppView>,
) -> gpui::AnyElement {
    let highlight = active_selection
        .filter(|sel| sel.key == key)
        .and_then(|sel| sel.range())
        .filter(|r| {
            r.end <= text.len() && text.is_char_boundary(r.start) && text.is_char_boundary(r.end)
        });
    let styled = if let Some(range) = highlight {
        StyledText::new(text.clone()).with_highlights([(
            range,
            HighlightStyle {
                color: Some(rgb(0xffffff).into()),
                background_color: Some(rgb(0x264f78).into()),
                ..Default::default()
            },
        )])
    } else {
        StyledText::new(text.clone())
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

fn render_markdown_blocks(
    row_idx: usize,
    text: &str,
    feed_layouts: &mut HashMap<FeedTextKey, FeedLayoutSnapshot>,
    active_selection: Option<FeedTextSelection>,
    cx: &mut Context<HermesAppView>,
) -> Vec<gpui::AnyElement> {
    let text_main = rgb(0xe6edf6);
    let text_dim = rgb(0x8e9bb0);
    let accent = rgb(0x4ea1ff);
    let border = rgb(0x2b3446);
    let bg_code = rgb(0x10141c);

    parse_blocks(text)
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
                    render_selectable_feed_text(feed_layouts, active_selection, key, text, cx),
                );
                if level <= 2 {
                    el.text_base().into_any_element()
                } else {
                    el.text_sm().into_any_element()
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
                    .child(render_selectable_feed_text(
                        feed_layouts,
                        active_selection,
                        key,
                        text,
                        cx,
                    ))
                    .into_any_element()
            }
            MdBlock::Bullet { text } => {
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
                    .text_sm()
                    .text_color(text_main)
                    .child(div().flex_shrink_0().text_color(accent).child("•"))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(render_selectable_feed_text(
                                feed_layouts,
                                active_selection,
                                key,
                                text,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
            MdBlock::Quote { text } => {
                let key = FeedTextKey {
                    row_idx,
                    block_idx,
                    sub_idx: 0,
                };
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .pl_3()
                    .border_l_2()
                    .border_color(accent)
                    .text_sm()
                    .text_color(text_dim)
                    .child(render_selectable_feed_text(
                        feed_layouts,
                        active_selection,
                        key,
                        text,
                        cx,
                    ))
                    .into_any_element()
            }
            MdBlock::Table {
                headers,
                alignments,
                rows,
            } => {
                let mut tsv_lines = Vec::with_capacity(rows.len() + 1);
                tsv_lines.push(headers.join("\t"));
                for r in &rows {
                    tsv_lines.push(r.join("\t"));
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
                            .child(render_selectable_feed_text(
                                feed_layouts,
                                active_selection,
                                key,
                                cell,
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
                                    .child(render_selectable_feed_text(
                                        feed_layouts,
                                        active_selection,
                                        key,
                                        cell,
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
                let copy_text = code.clone();
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
                            .text_xs()
                            .text_color(rgb(0xcbd5e1))
                            .child(render_selectable_feed_text(
                                feed_layouts,
                                active_selection,
                                key,
                                code,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn toggle_maximize_window(window: &Window) {
    use core::ffi::c_void;

    const SW_MAXIMIZE: i32 = 3;
    const SW_RESTORE: i32 = 9;

    #[link(name = "user32")]
    extern "system" {
        fn GetActiveWindow() -> *mut c_void;
        fn IsZoomed(hwnd: *mut c_void) -> i32;
        fn ShowWindowAsync(hwnd: *mut c_void, cmd_show: i32) -> i32;
    }

    let hwnd = unsafe { GetActiveWindow() };
    if !hwnd.is_null() {
        let cmd = if unsafe { IsZoomed(hwnd) } != 0 || window.is_maximized() {
            SW_RESTORE
        } else {
            SW_MAXIMIZE
        };
        unsafe {
            ShowWindowAsync(hwnd, cmd);
        }
    } else if !window.is_maximized() {
        window.zoom_window();
    }
}

impl Render for HermesAppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                            view.client.change_profile();
                            view.state.reset_for_profile(path.clone());
                            view.setup = SetupDraft::load_from_profile(&path);
                            view.workspace_prefs = WorkspacePrefs::load(&path);
                            view.workspace_root = view.workspace_prefs.resolve_initial();
                            view.state.set_workspace_path_text(&view.workspace_root);
                            view.input_target = if view.setup.open {
                                InputTarget::Setup(SetupField::Model)
                            } else {
                                InputTarget::Composer
                            };
                            view.client.connect_and_sync(path.clone());
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
                let sid_del = sess.id.clone();
                let selected = self.state.selected_session_id.as_deref() == Some(sid.as_str());
                let confirming_delete =
                    self.state.confirm_delete_session_id.as_deref() == Some(sid.as_str());
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
                    .w_full()
                    .min_w(px(0.0))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if selected { bg_user } else { bg_sidebar })
                    .border_1()
                    .border_color(if confirming_delete {
                        red
                    } else if selected {
                        accent
                    } else {
                        border
                    })
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_row()
                            .items_start()
                            .justify_between()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .text_sm()
                                    .text_color(text_main)
                                    .child(SharedString::from(title)),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .px_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_xs()
                                    .bg(if confirming_delete {
                                        red
                                    } else {
                                        gpui::rgba(0x00000000)
                                    })
                                    .text_color(if confirming_delete {
                                        rgb(0xffffff)
                                    } else {
                                        text_dim
                                    })
                                    .hover(move |s| {
                                        if confirming_delete {
                                            s.bg(rgb(0xd93830)).text_color(rgb(0xffffff))
                                        } else {
                                            s.bg(rgb(0x3a1820)).text_color(red)
                                        }
                                    })
                                    .child(if confirming_delete { "Del?" } else { "×" })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |view, _, _, cx| {
                                            cx.stop_propagation();
                                            if view.state.confirm_delete_session_id.as_deref()
                                                == Some(sid_del.as_str())
                                            {
                                                view.state.confirm_delete_session_id = None;
                                                view.state.status_message =
                                                    format!("Deleting session {sid_del}...");
                                                view.client.delete_session(
                                                    view.state.active_profile.clone(),
                                                    sid_del.clone(),
                                                );
                                            } else {
                                                view.state.confirm_delete_session_id =
                                                    Some(sid_del.clone());
                                            }
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .text_xs()
                            .text_color(text_dim)
                            .child(SharedString::from(subtitle)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, _, cx| {
                            view.state.confirm_delete_session_id = None;
                            view.load_session(sid.clone());
                            cx.notify();
                        }),
                    )
            })
            .collect();

        self.feed_layouts.clear();
        let active_feed_sel = self.feed_selection;
        let feed_rows = self.state.feed.clone();
        let feed_ui: Vec<_> = feed_rows
            .iter()
            .enumerate()
            .map(|(row_idx, row)| match row {
                FeedRow::User(text) => {
                    let key = FeedTextKey {
                        row_idx,
                        block_idx: 0,
                        sub_idx: 0,
                    };
                    div()
                        .w_full()
                        .flex_shrink_0()
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
                                .child(render_selectable_feed_text(
                                    &mut self.feed_layouts,
                                    active_feed_sel,
                                    key,
                                    text.clone(),
                                    cx,
                                )),
                        )
                        .into_any_element()
                }
                FeedRow::Assistant(text) => {
                    let copy_reply = text.clone();
                    let blocks = render_markdown_blocks(
                        row_idx,
                        text,
                        &mut self.feed_layouts,
                        active_feed_sel,
                        cx,
                    );
                    div()
                        .w_full()
                        .flex_shrink_0()
                        .flex()
                        .justify_start()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(780.0))
                                .min_w(px(0.0))
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .bg(bg_card)
                                .border_1()
                                .border_color(border)
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_row()
                                        .justify_between()
                                        .items_center()
                                        .child(div().text_xs().text_color(text_dim).child("Hermes"))
                                        .child(
                                            div()
                                                .px_2()
                                                .rounded_sm()
                                                .cursor_pointer()
                                                .bg(rgb(0x141923))
                                                .border_1()
                                                .border_color(border)
                                                .text_xs()
                                                .text_color(accent)
                                                .child("Copy")
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |view, _, _, cx| {
                                                        cx.stop_propagation();
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                copy_reply.clone(),
                                                            ),
                                                        );
                                                        view.state.status_message =
                                                            "Copied response to clipboard".into();
                                                        cx.notify();
                                                    }),
                                                ),
                                        ),
                                )
                                .children(blocks),
                        )
                        .into_any_element()
                }
                FeedRow::Tool {
                    name,
                    detail,
                    summary,
                    live_output,
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
                    let error_reason = if *state == ToolCardState::Failed {
                        summary.clone()
                    } else {
                        if let Some(s) = summary {
                            line.push_str(" · ");
                            line.push_str(s);
                        }
                        None
                    };
                    let preview_children: Vec<_> = if *state == ToolCardState::Running {
                        live_output
                            .iter()
                            .map(|l| {
                                div()
                                    .w_full()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_xs()
                                    .text_color(text_dim)
                                    .child(SharedString::from(format!("┆ {l}")))
                                    .into_any_element()
                            })
                            .collect()
                    } else if let Some(reason) = error_reason {
                        vec![div()
                            .w_full()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_xs()
                            .text_color(rgb(0xff9b9b))
                            .child(SharedString::from(format!("↳ {reason}")))
                            .into_any_element()]
                    } else {
                        Vec::new()
                    };
                    div()
                        .w_full()
                        .min_w(px(0.0))
                        .flex_shrink_0()
                        .flex()
                        .justify_start()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(780.0))
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(0x181e29))
                                .border_1()
                                .border_color(if *state == ToolCardState::Failed {
                                    red
                                } else {
                                    border
                                })
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .w_full()
                                        .min_w(px(0.0))
                                        .overflow_hidden()
                                        .text_xs()
                                        .text_color(col)
                                        .child(SharedString::from(line)),
                                )
                                .children(preview_children),
                        )
                        .into_any_element()
                }
                FeedRow::Notice { is_error, text } => {
                    let key = FeedTextKey {
                        row_idx,
                        block_idx: 0,
                        sub_idx: 0,
                    };
                    div()
                        .w_full()
                        .flex_shrink_0()
                        .flex()
                        .justify_start()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(780.0))
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(if *is_error { red } else { text_dim })
                                .child(render_selectable_feed_text(
                                    &mut self.feed_layouts,
                                    active_feed_sel,
                                    key,
                                    text.clone(),
                                    cx,
                                )),
                        )
                        .into_any_element()
                }
            })
            .collect();

        let approval_panel = self.state.pending_approval.clone().map(|pending| {
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
        });

        let clarify_panel = self.state.pending_clarify.clone().map(|clarify| {
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
                text_main,
                text_dim,
                accent,
                14.0,
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

        let setup_view = self.setup.open.then(|| {
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
                text_main,
                text_dim,
                accent,
                14.0,
            );
            let model_display = render_buffer_input(
                &mut self.input_layouts,
                InputTarget::Setup(SetupField::Model),
                &self.setup.model,
                self.input_target == InputTarget::Setup(SetupField::Model),
                "Model identifier (e.g. gpt-4.1, deepseek-chat)",
                text_main,
                text_dim,
                accent,
                14.0,
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
                text_main,
                text_dim,
                accent,
                14.0,
            );

            let web_env_name = self.setup.web_env_name();
            let web_key_display = web_env_name.map(|var| {
                render_masked_input(
                    &mut self.input_layouts,
                    InputTarget::Setup(SetupField::WebApiKey),
                    &self.setup.web_api_key,
                    self.input_target == InputTarget::Setup(SetupField::WebApiKey),
                    &format!("Optional {var} (leave blank to keep existing)"),
                    text_main,
                    text_dim,
                    accent,
                    14.0,
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
        });

        let reasoning_preview_ui = if self.state.streaming {
            let mut preview = self.state.live_reasoning.clone();
            let partial = self.state.live_reasoning_partial.trim();
            if !partial.is_empty() {
                preview.push(partial.to_owned());
            }
            if preview.len() > 3 {
                preview.drain(..preview.len() - 3);
            }
            if !preview.is_empty() {
                Some(
                    div()
                        .w_full()
                        .min_w(px(0.0))
                        .flex_shrink_0()
                        .flex()
                        .justify_start()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(780.0))
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(rgb(0x161b24))
                                .border_1()
                                .border_color(border)
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .w_full()
                                        .min_w(px(0.0))
                                        .text_xs()
                                        .text_color(accent)
                                        .child("Thinking…"),
                                )
                                .children(preview.into_iter().map(|line| {
                                    div()
                                        .w_full()
                                        .min_w(px(0.0))
                                        .overflow_hidden()
                                        .text_xs()
                                        .text_color(text_dim)
                                        .child(SharedString::from(format!("┆ {line}")))
                                        .into_any_element()
                                })),
                        )
                        .into_any_element(),
                )
            } else {
                None
            }
        } else {
            None
        };

        // Scrollable feed or Setup screen
        if self.feed_follow_bottom {
            self.feed_scroll.scroll_to_bottom();
        }
        let main_body = if let Some(setup_el) = setup_view {
            setup_el.into_any_element()
        } else {
            div()
                .id("chat-feed")
                .w_full()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .track_scroll(&self.feed_scroll)
                .on_scroll_wheel(cx.listener(|view, ev: &ScrollWheelEvent, _, _| {
                    let dy = ev.delta.pixel_delta(px(20.0)).y;
                    if dy > px(0.0) {
                        view.feed_follow_bottom = false;
                    } else if dy < px(0.0) {
                        let max_y = view.feed_scroll.max_offset().height;
                        let cur_y = view.feed_scroll.offset().y;
                        if max_y <= px(0.0) || cur_y + dy <= -max_y + px(36.0) {
                            view.feed_follow_bottom = true;
                        }
                    }
                }))
                .p_4()
                .flex()
                .flex_col()
                .gap_2()
                .children(feed_ui)
                .children(reasoning_preview_ui)
                .into_any_element()
        };

        let workspace_label = short_folder_label(&self.workspace_root);
        let workspace_path_display = render_buffer_input(
            &mut self.input_layouts,
            InputTarget::WorkspacePath,
            &self.state.workspace_path_input,
            self.input_target == InputTarget::WorkspacePath,
            "Project path (Enter to switch)...",
            text_main,
            text_dim,
            accent,
            14.0,
        );
        let current_ws_str = self.workspace_root.display().to_string();
        let recent_workspaces_ui: Vec<_> = self
            .workspace_prefs
            .recent
            .iter()
            .filter(|p| *p != &current_ws_str)
            .take(4)
            .map(|p| {
                let dir = PathBuf::from(p);
                let label = short_folder_label(&dir);
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(bg_card)
                    .border_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(text_dim)
                    .child(SharedString::from(format!("📁 {label}")))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, _, cx| {
                            view.set_workspace_root(dir.clone(), cx);
                        }),
                    )
            })
            .collect();

        let attachments_panel = (!self.state.attachments.is_empty()).then(|| {
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
        });

        let search_display = render_buffer_input(
            &mut self.input_layouts,
            InputTarget::Search,
            &self.state.search_input,
            self.input_target == InputTarget::Search,
            "Search sessions...",
            text_main,
            text_dim,
            accent,
            14.0,
        );
        let composer_display = render_buffer_input(
            &mut self.input_layouts,
            InputTarget::Composer,
            &self.state.composer_input,
            self.input_target == InputTarget::Composer,
            "Message Hermes... (@file/@diff, /help, !<cmd>, or drop/attach 📎 files)",
            text_main,
            text_dim,
            accent,
            16.0,
        );
        let header_session = self
            .state
            .selected_session_title
            .clone()
            .or_else(|| self.state.selected_session_id.clone())
            .unwrap_or_else(|| "New session".into());

        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .flex()
            .flex_row()
            // Sidebar
            .child(
                div()
                    .w(px(280.0))
                    .flex_shrink_0()
                    .h_full()
                    .min_h(px(0.0))
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
                                            view.new_chat();
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
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .justify_between()
                                    .items_center()
                                    .child(div().text_xs().text_color(text_dim).child(
                                        SharedString::from(format!(
                                            "Project: 📁 {workspace_label}"
                                        )),
                                    ))
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
                                            .text_color(accent)
                                            .child("Open...")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|view, _, _, cx| {
                                                    view.pick_workspace_folder(cx);
                                                }),
                                            ),
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
                                    .border_color(
                                        if self.input_target == InputTarget::WorkspacePath {
                                            accent
                                        } else {
                                            border
                                        },
                                    )
                                    .w_full()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_xs()
                                    .child(workspace_path_display)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, ev, window, cx| {
                                            view.handle_input_mouse_down(
                                                InputTarget::WorkspacePath,
                                                ev,
                                                window,
                                                cx,
                                            );
                                        }),
                                    ),
                            )
                            .when(!recent_workspaces_ui.is_empty(), |el| {
                                el.child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_1()
                                        .children(recent_workspaces_ui),
                                )
                            }),
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
                            .w_full()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_xs()
                            .child(search_display)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|view, ev, window, cx| {
                                    view.handle_input_mouse_down(
                                        InputTarget::Search,
                                        ev,
                                        window,
                                        cx,
                                    );
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("sessions-list")
                            .flex_1()
                            .min_h(px(0.0))
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
                    .min_w(px(0.0))
                    .h_full()
                    .min_h(px(0.0))
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
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
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
                                    .flex_shrink_0()
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
                                            .bg(if self.setup.open { accent } else { bg_card })
                                            .text_color(if self.setup.open {
                                                rgb(0x0b1017)
                                            } else {
                                                text_main
                                            })
                                            .text_xs()
                                            .child("⚙ Setup")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|view, _, window, cx| {
                                                    if view.setup.open {
                                                        view.setup.open = false;
                                                        view.input_target = InputTarget::Composer;
                                                    } else {
                                                        view.setup = SetupDraft::load_from_profile(
                                                            &view.state.active_profile,
                                                        );
                                                        view.setup.open = true;
                                                        view.input_target =
                                                            InputTarget::Setup(SetupField::Model);
                                                    }
                                                    window.focus(&view.focus_handle);
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
                                                    );
                                                    cx.notify();
                                                }),
                                            ),
                                    ),
                            ),
                    )
                    // Scrollable feed or in-app Setup screen
                    .child(main_body)
                    // Pending approval, clarify modal, `@` / `/` completion popup, or attached files bar
                    .children(approval_panel)
                    .children(clarify_panel)
                    .children(completion_panel)
                    .children(attachments_panel)
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
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(bg_card)
                                    .border_1()
                                    .border_color(border)
                                    .text_sm()
                                    .text_color(accent)
                                    .child("📎")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, _, _, cx| {
                                            view.pick_attach_files(cx);
                                        }),
                                    ),
                            )
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
                                    .min_w(px(0.0))
                                    .text_sm()
                                    .child(composer_display)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, ev, window, cx| {
                                            view.handle_input_mouse_down(
                                                InputTarget::Composer,
                                                ev,
                                                window,
                                                cx,
                                            );
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
            );

        let root = div()
            .id("hermes-gui-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_move(cx.listener(Self::handle_input_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    view.dragging_input = None;
                    if let Some(mut sel) = view.feed_selection {
                        sel.dragging = false;
                        view.feed_selection = if sel.anchor == sel.focus {
                            None
                        } else {
                            Some(sel)
                        };
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                view.attach_paths(paths.paths(), cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_app)
            .text_color(text_main);

        #[cfg(not(target_os = "windows"))]
        let _ = window;

        #[cfg(target_os = "windows")]
        let is_maximized = window.is_maximized();

        #[cfg(target_os = "windows")]
        let root = root.child(
            div()
                .w_full()
                .h(px(32.0))
                .bg(bg_sidebar)
                .border_b_1()
                .border_color(border)
                .flex()
                .flex_row()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .px_3()
                        .flex()
                        .items_center()
                        .text_xs()
                        .text_color(text_dim)
                        .window_control_area(WindowControlArea::Drag)
                        .child("Hermes"),
                )
                .child(
                    div()
                        .w(px(44.0))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(text_main)
                        .hover(|s| s.bg(bg_card))
                        .child("—")
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            cx.stop_propagation();
                            window.minimize_window();
                        }),
                )
                .child(
                    div()
                        .w(px(44.0))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(text_main)
                        .hover(|s| s.bg(bg_card))
                        .child(if is_maximized { "❐" } else { "□" })
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            cx.stop_propagation();
                            toggle_maximize_window(window);
                        }),
                )
                .child(
                    div()
                        .w(px(44.0))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(text_main)
                        .hover(|s| s.bg(rgb(0xc42b1c)).text_color(rgb(0xffffff)))
                        .child("×")
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            cx.stop_propagation();
                            window.remove_window();
                            cx.quit();
                        }),
                ),
        );

        root.child(body)
    }
}

fn main() -> std::process::ExitCode {
    if let Some(code) = hermes_cli::try_run_embedded_subcommand() {
        return code;
    }

    Application::new().run(|cx: &mut App| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1160.0), px(780.0)), cx);
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Hermes".into()),
                    appears_transparent: cfg!(target_os = "windows"),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                window.on_window_should_close(cx, |_, cx| {
                    cx.quit();
                    true
                });
                let view = cx.new(HermesAppView::new);
                let handle = view.read(cx).focus_handle.clone();
                window.focus(&handle);
                view
            },
        );
    });
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::text_from_keystroke;

    #[test]
    fn space_key_inserts_space_even_without_key_char() {
        // Windows GPUI 0.2.2 reports VK_SPACE as key="space", key_char=None.
        assert_eq!(
            text_from_keystroke("space", None, false, false, false),
            Some(" ")
        );
        // Linux/macOS may also provide key_char=Some(" "); still returns a single space.
        assert_eq!(
            text_from_keystroke("space", Some(" "), false, false, false),
            Some(" ")
        );
        // Regular printable and Unicode characters pass through.
        assert_eq!(
            text_from_keystroke("a", Some("a"), false, false, false),
            Some("a")
        );
        assert_eq!(
            text_from_keystroke("a", Some("ф"), false, false, false),
            Some("ф")
        );
        // Modified chords do not insert text.
        assert_eq!(text_from_keystroke("space", None, true, false, false), None);
        assert_eq!(
            text_from_keystroke("v", Some("v"), false, false, true),
            None
        );
    }
}
