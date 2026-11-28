//! `hsr-gui`: standalone GPUI desktop application for Hermes Agent backed by the shared
//! cross-platform local session owner (`hsr owner`, protocol v2).

mod clipboard_media;
mod diagram;
mod editor;
mod feed_render;
mod input_helpers;
mod markdown;
mod math_render;
mod owner_client;
mod panels;
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
    div, prelude::*, px, rgb, size, App, Application, Bounds, ClipboardEntry, ClipboardItem,
    Context, ExternalPaths, FocusHandle, FontWeight, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, PathPromptOptions, Pixels, Point, ScrollHandle, ScrollWheelEvent, SharedString,
    TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use hermes_runtime::config::profile_home;
use hermes_runtime::local_owner::ApprovalScope;

use crate::editor::TextBuffer;
use crate::feed_render::{
    render_markdown_blocks, render_selectable_feed_text, FeedLayoutSnapshot, FeedTextKey,
    FeedTextSelection,
};
use crate::input_helpers::{
    format_quote_insertion, format_running_elapsed, render_buffer_input, shortcut_letter,
    text_from_keystroke, word_bounds_at, InputLayoutSnapshot, InputTarget,
};
use crate::owner_client::{BackendUpdate, OwnerClient, ScopedUpdate};
use crate::panels::ThemeColors;
use crate::setup_state::{SetupDraft, SetupField};
use crate::state::{FeedRow, GuiState, PendingClarify, ToolCardState};
use crate::workspace::{
    attach_bytes_to_workspace, attach_path_to_workspace, format_prompt_with_attachments,
    format_size, resolve_workspace_dir, short_folder_label, WorkspacePrefs,
};

pub(crate) struct HermesAppView {
    pub(crate) state: GuiState,
    pub(crate) setup: SetupDraft,
    pub(crate) workspace_prefs: WorkspacePrefs,
    pub(crate) workspace_root: PathBuf,
    pub(crate) client: OwnerClient,
    rx: Receiver<ScopedUpdate>,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) input_target: InputTarget,
    pub(crate) input_layouts: HashMap<InputTarget, InputLayoutSnapshot>,
    dragging_input: Option<InputTarget>,
    pub(crate) feed_layouts: HashMap<FeedTextKey, FeedLayoutSnapshot>,
    pub(crate) feed_selection: Option<FeedTextSelection>,
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
        cx.spawn(async move |this, cx| {
            let mut last_timer_tick = std::time::Instant::now();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                let keep_running = this
                    .update(cx, |view, cx| {
                        let updated = view.drain_updates();
                        let timer_tick = !view.state.active_tool_starts.is_empty()
                            && last_timer_tick.elapsed() >= Duration::from_secs(1);
                        if timer_tick {
                            last_timer_tick = std::time::Instant::now();
                        }
                        if updated || timer_tick {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !keep_running {
                    break;
                }
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

    pub(crate) fn handle_input_mouse_down(
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

    pub(crate) fn handle_feed_mouse_down(
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

    pub(crate) fn selected_feed_text(&self) -> Option<String> {
        let sel = self.feed_selection?;
        let range = sel.range()?;
        let snap = self.feed_layouts.get(&sel.key)?;
        snap.text.get(range).map(str::to_owned)
    }

    pub(crate) fn quote_text_into_composer(
        &mut self,
        fragment: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(chunk) = format_quote_insertion(self.state.composer_input.as_str(), fragment)
        else {
            return false;
        };
        self.state.composer_input.move_end(false);
        self.state.composer_input.insert_str(&chunk);
        self.feed_selection = None;
        self.input_target = InputTarget::Composer;
        self.state.palette_dismissed = false;
        self.state.palette_index = 0;
        self.state.status_message = "Quoted selection into reply".into();
        window.focus(&self.focus_handle);
        cx.notify();
        true
    }

    pub(crate) fn quote_selected_feed_text(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(selected) = self.selected_feed_text() else {
            return false;
        };
        self.quote_text_into_composer(&selected, window, cx)
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

    fn attach_clipboard_image(&mut self, ext: &str, bytes: &[u8], cx: &mut Context<Self>) {
        let base_name = format!("clipboard_image.{ext}");
        match attach_bytes_to_workspace(bytes, &base_name, &self.workspace_root) {
            Ok(att) => {
                let name = att.display_name.clone();
                let size = format_size(att.size_bytes);
                self.state.add_attachment(att);
                self.state.status_message = format!("Attached clipboard image ({name}, {size})");
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
    }

    pub(crate) fn save_setup(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn send_composer(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn submit_clarify(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn cancel_clarify(&mut self, cx: &mut Context<Self>) {
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
        let key_char = event.keystroke.key_char.as_deref();
        let mods = &event.keystroke.modifiers;
        let primary = mods.control || mods.platform;
        let primary_letter = if primary && !mods.alt {
            shortcut_letter(key, key_char)
        } else {
            None
        };

        if primary_letter == Some('q') && mods.shift {
            self.quote_selected_feed_text(window, cx);
            return;
        }

        if (mods.alt && key.eq_ignore_ascii_case("f4")) || primary_letter == Some('q') {
            window.remove_window();
            cx.quit();
            return;
        }

        if primary_letter == Some('v') {
            let item = cx.read_from_clipboard();

            // 1. In the Composer, attach clipboard images (GPUI ClipboardEntry::Image on all OSes).
            if self.input_target == InputTarget::Composer {
                if let Some(img) = item.as_ref().and_then(|clip| {
                    clip.entries().iter().find_map(|e| match e {
                        ClipboardEntry::Image(img) => Some(img),
                        _ => None,
                    })
                }) {
                    match clipboard_media::normalize_gpui_image(img.format(), img.bytes()) {
                        Ok((ext, bytes)) => self.attach_clipboard_image(ext, &bytes, cx),
                        Err(err) => {
                            self.state.feed.push(FeedRow::Notice {
                                is_error: true,
                                text: err.clone(),
                            });
                            self.state.status_message = err;
                            cx.notify();
                        }
                    }
                    return;
                }

                // 2. On Windows, Explorer Ctrl+C puts CF_HDROP (without Unicode text): attach those files.
                #[cfg(target_os = "windows")]
                if let Some(paths) = clipboard_media::read_windows_clipboard_files() {
                    self.attach_paths(&paths, cx);
                    return;
                }
            }

            // 3. Plain text paste works in every text input field.
            if let Some(text) = item.as_ref().and_then(ClipboardItem::text) {
                if !text.is_empty() {
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
                    return;
                }
            }

            // 4. On Windows, fall back to Win32 PNG / CF_DIBV5 / CF_DIB (Snipping Tool, Win+Shift+S,
            //    PrtScn, Paint, browsers) which gpui 0.2.2 does not decode.
            #[cfg(target_os = "windows")]
            if self.input_target == InputTarget::Composer {
                match clipboard_media::read_windows_clipboard_image() {
                    Ok(Some((ext, bytes))) => {
                        self.attach_clipboard_image(ext, &bytes, cx);
                    }
                    Ok(None) => {}
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

        if primary_letter == Some('u') {
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

        if primary_letter == Some('p') && self.input_target == InputTarget::Composer {
            if self.state.composer_input.is_empty() {
                self.state.composer_input.insert_char('/');
                self.state.palette_index = 0;
                self.state.palette_dismissed = false;
                cx.notify();
            }
            return;
        }

        if primary_letter == Some('a') {
            self.feed_selection = None;
            if let Some(buf) = self.active_buffer_mut() {
                buf.select_all();
                cx.notify();
            }
            return;
        }

        if primary_letter == Some('c') {
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

        if primary_letter == Some('x') {
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

/// Map a Ctrl/Cmd shortcut keystroke to its physical Latin letter across English and Russian
/// (ЙЦУКЕН) layouts as well as ASCII control codes emitted by some platforms.
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
        let theme = ThemeColors::dark();
        let ThemeColors {
            bg_app,
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
        } = theme;

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
                    let row_has_selection = active_feed_sel
                        .is_some_and(|sel| sel.key.row_idx == row_idx && sel.range().is_some());
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
                                                .flex()
                                                .flex_row()
                                                .gap_2()
                                                .when(row_has_selection, |el| {
                                                    el.child(
                                                        div()
                                                            .px_2()
                                                            .rounded_sm()
                                                            .cursor_pointer()
                                                            .bg(accent)
                                                            .text_xs()
                                                            .text_color(rgb(0x0b1017))
                                                            .font_weight(FontWeight::SEMIBOLD)
                                                            .child("❝ Quote")
                                                            .on_mouse_down(
                                                                MouseButton::Left,
                                                                cx.listener(
                                                                    |view, _, window, cx| {
                                                                        cx.stop_propagation();
                                                                        view.quote_selected_feed_text(
                                                                            window, cx,
                                                                        );
                                                                    },
                                                                ),
                                                            ),
                                                    )
                                                })
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
                                                                    "Copied response to clipboard"
                                                                        .into();
                                                                cx.notify();
                                                            }),
                                                        ),
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
                    if *state == ToolCardState::Running {
                        let elapsed = self
                            .state
                            .active_tool_starts
                            .get(name)
                            .map(|s| format_running_elapsed(s.elapsed()));
                        if let Some(dur) = elapsed {
                            line.push_str(&format!(" (running… · {dur})"));
                        } else {
                            line.push_str(" (running…)");
                        }
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

        let approval_panel = self.render_approval_panel(&theme, cx);
        let clarify_panel = self.render_clarify_panel(&theme, cx);
        let completion_panel = self.render_completion_panel(&theme, cx);
        let setup_view = self.render_setup_view(&theme, cx);
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
            theme.field_style(14.0),
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

        let selection_action_panel = self.render_selection_action_panel(&theme, cx);
        let attachments_panel = self.render_attachments_panel(&theme, cx);
        let search_display = render_buffer_input(
            &mut self.input_layouts,
            InputTarget::Search,
            &self.state.search_input,
            self.input_target == InputTarget::Search,
            "Search sessions...",
            theme.field_style(14.0),
        );
        let composer_display = render_buffer_input(
            &mut self.input_layouts,
            InputTarget::Composer,
            &self.state.composer_input,
            self.input_target == InputTarget::Composer,
            "Message Hermes... (@file/@diff, /help, !<cmd>, or drop/attach 📎 files)",
            theme.field_style(16.0),
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
                    // Pending approval, clarify modal, `@` / `/` completion popup, selection quote bar, or attached files bar
                    .children(approval_panel)
                    .children(clarify_panel)
                    .children(completion_panel)
                    .children(selection_action_panel)
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
                                    .id("composer-input-scroll")
                                    .flex_1()
                                    .min_h(px(38.0))
                                    .max_h(px(200.0))
                                    .overflow_y_scroll()
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
    use crate::editor::TextBuffer;
    use crate::feed_render::merge_highlights_with_selection;
    use crate::input_helpers::{
        format_quote_insertion, format_selection_preview, shortcut_letter, text_from_keystroke,
    };
    use crate::markdown::InlineStyle;

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
        // Single-char printable fallback when key_char is None.
        assert_eq!(
            text_from_keystroke("ф", None, false, false, false),
            Some("ф")
        );
        // Control characters and multi-char named keys are not inserted.
        assert_eq!(
            text_from_keystroke("enter", None, false, false, false),
            None
        );
        assert_eq!(
            text_from_keystroke("a", Some("\x01"), false, false, false),
            Some("a")
        );
        // Modified chords do not insert text.
        assert_eq!(text_from_keystroke("space", None, true, false, false), None);
        assert_eq!(
            text_from_keystroke("v", Some("v"), false, false, true),
            None
        );
    }

    #[test]
    fn shortcuts_work_on_latin_russian_layout_and_replace_selected_text() {
        assert_eq!(shortcut_letter("a", None), Some('a'));
        assert_eq!(shortcut_letter("A", None), Some('a'));
        assert_eq!(shortcut_letter("ф", None), Some('a'));
        assert_eq!(shortcut_letter("Ф", None), Some('a'));
        assert_eq!(shortcut_letter("с", None), Some('c'));
        assert_eq!(shortcut_letter("м", None), Some('v'));
        assert_eq!(shortcut_letter("ч", None), Some('x'));
        assert_eq!(shortcut_letter("unknown", Some("\x01")), Some('a'));

        let mut buf = TextBuffer::new();
        buf.insert_str("Старый текст запроса");
        buf.select_all();
        assert_eq!(buf.selected_text(), Some("Старый текст запроса"));
        // Pressing any printable key replaces the entire selection.
        let typed = text_from_keystroke("н", Some("Н"), false, false, false).unwrap();
        buf.insert_str(typed);
        assert_eq!(buf.as_str(), "Н");
        assert_eq!(buf.selected_text(), None);

        // Space also replaces an active selection.
        buf.select_all();
        let space = text_from_keystroke("space", None, false, false, false).unwrap();
        buf.insert_str(space);
        assert_eq!(buf.as_str(), " ");
    }

    #[test]
    fn merge_highlights_with_selection_emits_strictly_non_overlapping_sorted_runs() {
        let base = vec![(0..6, InlineStyle::Bold), (10..16, InlineStyle::Code)];
        let merged = merge_highlights_with_selection(20, &base, Some(3..13));
        assert!(!merged.is_empty());
        for pair in merged.windows(2) {
            assert!(pair[0].0.start < pair[0].0.end);
            assert!(pair[0].0.end <= pair[1].0.start);
        }
        // Selected overlap spans [3..6], [6..10], [10..13] all get selection background.
        let ranges: Vec<_> = merged.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..3, 3..6, 6..10, 10..13, 13..16]);
    }

    #[test]
    fn formats_quoted_selection_for_empty_and_existing_composer_text() {
        assert_eq!(format_quote_insertion("", "   \n  "), None);
        assert_eq!(
            format_quote_insertion("", "First line\n\nSecond line  ").as_deref(),
            Some("> First line\n>\n> Second line\n\n")
        );
        assert_eq!(
            format_quote_insertion("Вопрос по фрагменту:", "timeout = 30s").as_deref(),
            Some("\n\n> timeout = 30s\n\n")
        );
        assert_eq!(
            format_quote_insertion("> First quote\n\n", "Second quote").as_deref(),
            Some("> Second quote\n\n")
        );
        assert_eq!(
            format_selection_preview("  hello   world \n from chat ", 15),
            "hello world fro…"
        );
        assert_eq!(format_selection_preview("short text", 20), "short text");
    }
}
