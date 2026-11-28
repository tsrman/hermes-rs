use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Color;

use super::render::*;
use super::selection::*;
use super::*;

use ratatui::backend::TestBackend;

fn sample_state() -> ReplState {
    let req = chat::prepare(
        None,
        chat::ChatArgs::for_test("http://127.0.0.1:9/v1", "gpt-5.4", "k"),
    )
    .unwrap();
    let mut st = ReplState::new(None, req, true);
    st.session = Some("sess-abcdef123456".into());
    st.record_usage(1200, 350);
    st.feed_entries
        .push(FeedEntry::User("Check Cargo.toml".into()));
    st.feed_entries.push(FeedEntry::Tool {
        name: "read_file".into(),
        detail: Some("Cargo.toml".into()),
        status: ToolStatus::Succeeded,
        summary: Some("22 lines".into()),
        elapsed_ms: Some(18),
    });
    st.feed_entries.push(FeedEntry::Assistant(
        "## Result\nFound **ratatui** in `Cargo.toml`.".into(),
    ));
    st
}

fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn renders_header_feed_tool_cards_and_composer_in_test_backend() {
    let state = sample_state();
    let mut app = TuiApp::new();
    app.input = "/co".into();
    app.cursor = 3;

    let backend = TestBackend::new(90, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();

    let text = buffer_text(&terminal);
    assert!(text.contains("HERMES"), "header missing HERMES:\n{text}");
    assert!(text.contains("gpt-5.4"), "header missing model:\n{text}");
    assert!(
        text.contains("1.2K/1.1M") && text.contains("[░░░░░░░░░░] 0%"),
        "header missing upstream-style context meter:\n{text}"
    );
    assert!(
        text.contains("read_file · Cargo.toml (22 lines · 18ms)"),
        "missing tool card with detail/status:\n{text}"
    );
    assert!(text.contains("Result"), "missing markdown heading:\n{text}");
    assert!(
        text.contains("Commands (Tab to complete)"),
        "missing slash palette:\n{text}"
    );
    assert!(
        text.contains("/compress"),
        "missing /compress in palette:\n{text}"
    );
}

#[test]
fn status_bar_renders_upstream_context_bar_tiers_thresholds_and_estimates() {
    let mut state = sample_state();
    let app = TuiApp::new();

    // 50% of gpt-5.4 (1.05M) -> 525K/1.1M, [█████░░░░░] 50%, Yellow
    state.record_context(525_000, false);
    state.compressions = 2;
    let mut wide = Terminal::new(TestBackend::new(100, 20)).unwrap();
    wide.draw(|f| render_frame(f, &app, &state)).unwrap();
    let wide_text = buffer_text(&wide);
    assert!(
        wide_text.contains("525K/1.1M")
            && wide_text.contains("[█████░░░░░] 50%")
            && wide_text.contains("🗜 2"),
        "wide header mismatch:\n{wide_text}"
    );
    assert_eq!(context_usage_style(49).fg, Some(Color::Green));
    assert_eq!(context_usage_style(50).fg, Some(Color::Yellow));
    assert_eq!(context_usage_style(80).fg, Some(Color::Yellow));
    assert_eq!(context_usage_style(81).fg, Some(Color::LightRed));
    assert_eq!(context_usage_style(95).fg, Some(Color::Red));

    // Estimated context (~) shown with tilde prefix; medium width omits token ratio but keeps bar.
    state.record_context(840_000, true);
    let mut medium = Terminal::new(TestBackend::new(64, 20)).unwrap();
    medium.draw(|f| render_frame(f, &app, &state)).unwrap();
    let medium_text = buffer_text(&medium);
    assert!(
        medium_text.contains("[████████░░] ~80%"),
        "medium header mismatch:\n{medium_text}"
    );

    // Narrow width keeps compact percentage.
    let mut narrow = Terminal::new(TestBackend::new(44, 20)).unwrap();
    narrow.draw(|f| render_frame(f, &app, &state)).unwrap();
    let narrow_text = buffer_text(&narrow);
    assert!(
        narrow_text.contains("~80%"),
        "narrow header mismatch:\n{narrow_text}"
    );
}

#[test]
fn renders_approval_and_clarify_modals_and_handles_keys() {
    let mut state = sample_state();
    let mut app = TuiApp::new();

    // 1. Approval modal
    let (tx, mut rx) = oneshot::channel();
    let req = ApprovalRequest {
        action: ApprovalAction::RunCommand,
        target: "/tmp/work".into(),
        content: "rm -rf build".into(),
        timeout_secs: 60,
    };
    open_modal(&mut app, req, tx);
    assert!(app.modal.is_some());

    let backend = TestBackend::new(90, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Approval Required"),
        "missing approval modal:\n{text}"
    );
    assert!(
        text.contains("rm -rf build"),
        "missing command preview:\n{text}"
    );

    handle_modal_key(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
    );
    assert!(app.modal.is_none());
    assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Approved)));

    // 2. Clarify modal
    let (tx2, mut rx2) = oneshot::channel();
    let clarify_req = ApprovalRequest {
        action: ApprovalAction::Clarify,
        target: "".into(),
        content: json!([{
            "qid": "q1",
            "question": "Which UI mode?",
            "choices": ["Inline", "TUI"],
            "multi_select": false
        }])
        .to_string(),
        timeout_secs: 60,
    };
    open_modal(&mut app, clarify_req, tx2);
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let text2 = buffer_text(&terminal);
    assert!(
        text2.contains("Clarification"),
        "missing clarify modal:\n{text2}"
    );
    assert!(
        text2.contains("Which UI mode?"),
        "missing question:\n{text2}"
    );

    // Press Down -> selects "TUI", then Enter -> submits
    handle_modal_key(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    );
    handle_modal_key(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(app.modal.is_none());
    match rx2.try_recv() {
        Ok(ApprovalDecision::Answered(json_str)) => {
            let parsed: Value = serde_json::from_str(&json_str).unwrap();
            assert_eq!(parsed["answers"]["q1"], "TUI");
            assert_eq!(parsed["outcome"], "submitted");
        }
        _ => panic!("expected ApprovalDecision::Answered"),
    }
}

struct TempDirGuard(std::path::PathBuf);
impl TempDirGuard {
    fn new(label: &str) -> Self {
        let unique = format!(
            "hsr-tui-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn up_arrow_recalls_persisted_and_submitted_history_without_palette_hijack() {
    let dir = TempDirGuard::new("history");
    std::fs::write(
        dir.path().join(".hsr_history"),
        "#V2\nfirst saved prompt\n/model\nsecond saved prompt\n",
    )
    .unwrap();

    let req = chat::prepare(
        Some(dir.path()),
        chat::ChatArgs::for_test("http://127.0.0.1:9/v1", "gpt-5.4", "k"),
    )
    .unwrap();
    let mut state = ReplState::new(Some(dir.path()), req, true);
    let mut app = TuiApp::new();
    let mut handle = None;
    let mut rx = None;

    // Type draft, then walk up through persisted history (including a /command entry).
    app.input = "draft text".into();
    app.cursor = app.input.len();

    handle_key_event(
        &mut app,
        &mut state,
        Some(dir.path()),
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "second saved prompt");

    // Second Up recalls `/model`; slash palette must stay hidden while browsing history.
    handle_key_event(
        &mut app,
        &mut state,
        Some(dir.path()),
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "/model");
    assert!(app.slash_palette().is_empty());

    // Third Up must still continue walking history instead of moving slash palette!
    handle_key_event(
        &mut app,
        &mut state,
        Some(dir.path()),
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "first saved prompt");

    // Key release events must be ignored.
    let release = KeyEvent {
        code: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: crossterm::event::KeyEventState::NONE,
    };
    handle_key_event(
        &mut app,
        &mut state,
        Some(dir.path()),
        release,
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "first saved prompt");

    // Walking back down restores the original draft.
    for _ in 0..3 {
        handle_key_event(
            &mut app,
            &mut state,
            Some(dir.path()),
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
    }
    assert_eq!(app.input, "draft text");
    assert!(app.history_pos.is_none());

    // Submitting a new prompt appends it to state.prompt_history and .hsr_history.
    let submitted = handle_key_event(
        &mut app,
        &mut state,
        Some(dir.path()),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(submitted, Some(TuiAction::Turn("draft text".into())));
    assert_eq!(
        state.prompt_history.last().map(String::as_str),
        Some("draft text")
    );
    let saved = std::fs::read_to_string(dir.path().join(".hsr_history")).unwrap();
    assert!(saved.contains("draft text"));
}

#[tokio::test]
async fn sessions_modal_renders_and_navigates_with_keys() {
    let mut state = sample_state();
    let mut app = TuiApp::new();
    app.modal = Some(ActiveModal::SessionPicker {
        sessions: vec![
            SessionSummary {
                id: "sess-newer-2222".into(),
                source: "cli".into(),
                model: Some("gpt-5.4".into()),
                title: Some("Deploy checklist".into()),
                started_at: 200.0,
                last_activity_at: Some(210.0),
                message_count: 6,
                preview: Some("second session hello".into()),
            },
            SessionSummary {
                id: "sess-older-1111".into(),
                source: "cli".into(),
                model: Some("gpt-5.4".into()),
                title: None,
                started_at: 100.0,
                last_activity_at: Some(110.0),
                message_count: 2,
                preview: Some("first session hello".into()),
            },
        ],
        cursor: 0,
        scroll: 0,
    });

    let backend = TestBackend::new(90, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let text = buffer_text(&terminal);
    assert!(text.contains("Sessions"), "missing modal title:\n{text}");
    assert!(
        text.contains("Deploy checklist") && text.contains("first session hello"),
        "missing session entries:\n{text}"
    );

    let mut handle = None;
    let mut rx = None;
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    match &app.modal {
        Some(ActiveModal::SessionPicker { cursor, .. }) => assert_eq!(*cursor, 1),
        _ => panic!("expected SessionPicker modal to remain open"),
    }

    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert!(app.modal.is_none());
}

#[test]
fn mouse_wheel_scrolls_conversation_feed_with_clamping_and_modals() {
    let mut state = sample_state();
    for i in 0..30 {
        state
            .feed_entries
            .push(FeedEntry::User(format!("Message line {i}")));
    }
    let mut app = TuiApp::new();
    let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let max_scroll = app.last_max_scroll.get();
    assert!(
        max_scroll > 10,
        "expected scrollable feed, got {max_scroll}"
    );

    let wheel = |kind| MouseEvent {
        kind,
        column: 10,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };

    // Scroll up moves away from bottom by 3 lines.
    assert!(handle_mouse_event(
        &mut app,
        wheel(MouseEventKind::ScrollUp)
    ));
    assert_eq!(app.scroll_from_bottom, 3);

    // Scrolling past top clamps cleanly at `max_scroll` (no overscroll debt).
    for _ in 0..100 {
        handle_mouse_event(&mut app, wheel(MouseEventKind::ScrollUp));
    }
    assert_eq!(app.scroll_from_bottom, max_scroll);
    assert!(!handle_mouse_event(
        &mut app,
        wheel(MouseEventKind::ScrollUp)
    ));

    // One notch down immediately moves back down by 3 lines.
    assert!(handle_mouse_event(
        &mut app,
        wheel(MouseEventKind::ScrollDown)
    ));
    assert_eq!(app.scroll_from_bottom, max_scroll.saturating_sub(3));

    // Mouse wheel also navigates SessionPicker modal.
    app.modal = Some(ActiveModal::SessionPicker {
        sessions: vec![
            SessionSummary {
                id: "s1".into(),
                source: "cli".into(),
                model: None,
                title: Some("One".into()),
                started_at: 2.0,
                last_activity_at: Some(2.0),
                message_count: 1,
                preview: None,
            },
            SessionSummary {
                id: "s2".into(),
                source: "cli".into(),
                model: None,
                title: Some("Two".into()),
                started_at: 1.0,
                last_activity_at: Some(1.0),
                message_count: 1,
                preview: None,
            },
        ],
        cursor: 0,
        scroll: 0,
    });
    assert!(handle_mouse_event(
        &mut app,
        wheel(MouseEventKind::ScrollDown)
    ));
    assert!(matches!(
        app.modal,
        Some(ActiveModal::SessionPicker { cursor: 1, .. })
    ));
    assert!(handle_mouse_event(
        &mut app,
        wheel(MouseEventKind::ScrollUp)
    ));
    assert!(matches!(
        app.modal,
        Some(ActiveModal::SessionPicker { cursor: 0, .. })
    ));
}

#[tokio::test]
async fn multiline_composer_renders_wrapped_lines_pasted_newlines_and_scrolls_to_cursor() {
    let mut state = sample_state();
    let mut app = TuiApp::new();
    let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();

    // 1. Long single line without `\n` wraps across multiple visible composer lines.
    let long_line = "AlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXi";
    app.insert_pasted(long_line);
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let rendered = buffer_text(&terminal);
    assert!(
        rendered.contains("AlphaBeta") && rendered.contains("LambdaMuNuXi"),
        "wrapped multiline input was clipped:\n{rendered}"
    );

    // 2. Pasting CRLF / CR text normalizes to `\n` and renders every line visibly.
    app.input.clear();
    app.cursor = 0;
    app.insert_pasted("FirstLine\r\nSecondLine\rThirdLine");
    assert_eq!(app.input, "FirstLine\nSecondLine\nThirdLine");
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let rendered_paste = buffer_text(&terminal);
    assert!(
        rendered_paste.contains("FirstLine")
            && rendered_paste.contains("SecondLine")
            && rendered_paste.contains("ThirdLine"),
        "pasted multiline lines missing:\n{rendered_paste}"
    );

    // 3. Trailing newline (e.g. after Alt+Enter / Ctrl+J) expands box immediately and moves cursor to row + 1.
    app.input = "LineOne\n".into();
    app.cursor = app.input.len();
    let layout = wrap_composer_lines(&app.input, app.cursor, None, 38);
    assert_eq!(layout.lines.len(), 2);
    assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));

    // 4. Input taller than max composer rows scrolls down so the active last line stays visible.
    app.input = (1..=12)
        .map(|i| format!("RowNumber{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.cursor = app.input.len();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let rendered_tall = buffer_text(&terminal);
    assert!(
        rendered_tall.contains("RowNumber12"),
        "composer failed to scroll to keep last line visible:\n{rendered_tall}"
    );

    // 5. Up/Down inside multiline input moves between lines before touching history.
    let mut handle = None;
    let mut rx = None;
    app.input = "LineA\nLineB".into();
    app.cursor = app.input.len();
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.cursor, 5); // end of "LineA"
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.cursor, app.input.len());
}

#[tokio::test]
async fn slash_palette_enter_executes_highlighted_command_and_tool_cards_show_status() {
    let mut state = sample_state();
    let mut app = TuiApp::new();
    let mut handle = None;
    let mut rx = None;

    // Typing `/se` matches `/sessions` uniquely; pressing Enter executes `/sessions`
    // instead of failing with "Unknown command /se".
    app.input = "/se".into();
    app.cursor = 3;
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert!(app.input.is_empty());
    assert!(
        !state.feed_entries.iter().any(
            |e| matches!(e, FeedEntry::Notice { text, .. } if text.contains("Unknown command"))
        ),
        "palette Enter should execute highlighted /sessions, not report Unknown command"
    );
    assert!(
        state.feed_entries.iter().any(
            |e| matches!(e, FeedEntry::Notice { text, .. } if text.contains("No saved sessions"))
        ),
        "expected /sessions output in feed"
    );

    // Typing `/` + Down selects `/new`, and Enter executes `/new` (clearing session).
    app.input = "/".into();
    app.cursor = 1;
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert!(state.session.is_none());

    // Running tool cards show live elapsed duration alongside `running…`.
    app.active_tools.insert(
        "terminal".into(),
        Instant::now()
            .checked_sub(Duration::from_secs(125))
            .unwrap_or_else(Instant::now),
    );
    state.feed_entries.push(FeedEntry::Tool {
        name: "terminal".into(),
        detail: Some("sleep 240".into()),
        status: ToolStatus::Running,
        summary: None,
        elapsed_ms: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(96, 20)).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let running_rendered = buffer_text(&terminal);
    assert!(
        running_rendered.contains("terminal · sleep 240 (running… · 2m 05s)"),
        "missing live elapsed time on running tool card:\n{running_rendered}"
    );
    app.active_tools.clear();
    state.feed_entries.pop();

    // Tool cards show command/code detail, exit code, duration, and error line on failure.
    state.feed_entries.push(FeedEntry::Tool {
        name: "terminal".into(),
        detail: Some("cargo test -p hermes-cli".into()),
        status: ToolStatus::Failed,
        summary: Some("exit 101 · 1 failed".into()),
        elapsed_ms: Some(12_900),
    });
    let mut terminal = Terminal::new(TestBackend::new(96, 20)).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let rendered = buffer_text(&terminal);
    assert!(
        rendered.contains("terminal · cargo test -p hermes-cli (exit 101 · 12.9s)"),
        "missing failed tool header with command & exit code:\n{rendered}"
    );
    assert!(
        rendered.contains("↳ 1 failed"),
        "missing indented failure detail line:\n{rendered}"
    );
    assert!(
        !rendered.contains("╭─") && !rendered.contains("─╮"),
        "tool line should not render half-box corners:\n{rendered}"
    );

    // Leading newlines from model responses after a tool call must not create blank gaps under `Hermes`.
    handle_turn_event(&mut app, &mut state, ChatEvent::Stdout("\n\n".into()));
    handle_turn_event(
        &mut app,
        &mut state,
        ChatEvent::Stdout("\nDirectory listing ready.".into()),
    );
    finish_turn(&mut app, &mut state, Ok(0));
    let mut terminal = Terminal::new(TestBackend::new(96, 20)).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let rendered2 = buffer_text(&terminal);
    let lines: Vec<&str> = rendered2.lines().collect();
    let hermes_row = lines
        .iter()
        .position(|l| l.contains("Hermes") && !l.contains("HERMES"))
        .expect("Hermes label present");
    assert!(
        lines[hermes_row + 1].contains("Directory listing ready."),
        "response text should start directly under Hermes without blank lines:\n{rendered2}"
    );
}

#[tokio::test]
async fn ctrl_v_russian_layout_and_shift_insert_paste_from_clipboard() {
    let mut state = sample_state();
    let mut app = TuiApp::new();
    let mut handle = None;
    let mut rx = None;

    // Ctrl+V inserts multiline clipboard text normalized to `\n`.
    {
        let _clip = clipboard::override_for_test(ClipboardRead::Text("line1\r\nline2".to_owned()));
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.input, "line1\nline2");
        assert_eq!(app.cursor, app.input.len());
    }

    // Russian keyboard layout Ctrl+М also pastes.
    {
        let _clip = clipboard::override_for_test(ClipboardRead::Text(" привет".to_owned()));
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Char('м'), KeyModifiers::CONTROL),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.input, "line1\nline2 привет");
    }

    // Shift+Insert pastes as well.
    {
        let _clip = clipboard::override_for_test(ClipboardRead::Text("!".to_owned()));
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.input, "line1\nline2 привет!");
    }

    // Unavailable clipboard reports a warning notice without modifying input.
    {
        let _clip = clipboard::override_for_test(ClipboardRead::Unavailable(
            "Clipboard unavailable".to_owned(),
        ));
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.input, "line1\nline2 привет!");
        assert!(state.feed_entries.iter().any(|e| matches!(
            e,
            FeedEntry::Notice {
                kind: NoticeKind::Warn,
                text
            } if text.contains("Clipboard unavailable")
        )));
    }

    // Ctrl+V inside Clarify modal appends single-line text into custom input.
    {
        let (tx, _rx_reply) = oneshot::channel();
        app.modal = Some(ActiveModal::Clarify {
            questions: vec![ClarifyQuestion {
                qid: "q1".into(),
                prompt: "Details?".into(),
                choices: vec![],
                multi_select: false,
            }],
            current: 0,
            cursor: 0,
            selected: BTreeSet::new(),
            custom: String::new(),
            answers: Map::new(),
            reply: Some(tx),
        });
        let _clip = clipboard::override_for_test(ClipboardRead::Text("foo\r\nbar".to_owned()));
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            &mut handle,
            &mut rx,
        )
        .await;
        match &app.modal {
            Some(ActiveModal::Clarify { custom, .. }) => assert_eq!(custom, "foo bar"),
            _ => panic!("expected Clarify modal to stay open"),
        }
    }
}

#[tokio::test]
async fn mouse_drag_selection_copies_soft_wrapped_text_survives_move_and_handles_copy_commands() {
    let mut state = sample_state();
    state.feed_entries.push(FeedEntry::User("Question".into()));
    state.feed_entries.push(FeedEntry::Assistant(
        "Hello world from Hermes TUI selection\n```rust\nlet answer = 42;\n```".into(),
    ));
    let mut app = TuiApp::new();
    // Narrow 28-col viewport forces soft wrapping of "Hello world from Hermes TUI selection".
    let mut terminal = Terminal::new(TestBackend::new(28, 20)).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();

    let (inner, screen_y_start, screen_y_next) = {
        let snap = app.feed_snapshot.borrow();
        let inner = snap.inner_area;
        let hello_row_idx = snap
            .lines
            .iter()
            .position(|l| l.plain.starts_with("Hello world"))
            .expect("rendered assistant line");
        assert!(
            snap.lines[hello_row_idx + 1].soft_wrapped,
            "expected continuation row to be soft-wrapped: {:?}",
            snap.lines.iter().map(|l| &l.plain).collect::<Vec<_>>()
        );
        let screen_y_start = inner.y + (hello_row_idx - snap.scroll_y) as u16;
        (inner, screen_y_start, screen_y_start + 1)
    };

    let clip = clipboard::capture_writes_for_test();
    let mouse = |kind, col, row| MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // 1. Left drag across the soft-wrapped sentence auto-copies cleanly without fake `\n`.
    assert!(handle_mouse_event(
        &mut app,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            inner.x,
            screen_y_start
        ),
    ));
    assert!(handle_mouse_event(
        &mut app,
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            inner.x + 25,
            screen_y_next,
        ),
    ));
    assert!(handle_mouse_event(
        &mut app,
        mouse(
            MouseEventKind::Up(MouseButton::Left),
            inner.x + 25,
            screen_y_next,
        ),
    ));
    assert!(
        app.selection.is_some(),
        "selection stays highlighted after release"
    );
    assert_eq!(
        clip.writes().last().map(String::as_str),
        Some("Hello world from Hermes TUI selection")
    );

    // 2. Moving the mouse afterwards must NOT clear the selection or trigger redraw.
    assert!(!handle_mouse_event(
        &mut app,
        mouse(MouseEventKind::Moved, inner.x + 2, screen_y_start),
    ));
    assert!(app.selection.is_some());

    // 3. Rendered frame highlights the selected cells in blue/white.
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let highlighted_cell = terminal
        .backend()
        .buffer()
        .cell((inner.x, screen_y_start))
        .unwrap();
    assert_eq!(highlighted_cell.bg, Color::Blue);
    assert_eq!(highlighted_cell.fg, Color::White);

    // 4. Ctrl+C (and Russian Ctrl+С) copies active selection and clears it without touching composer.
    app.input = "draft text".into();
    app.cursor = app.input.len();
    let mut handle = None;
    let mut rx = None;
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('с'), KeyModifiers::CONTROL),
        &mut handle,
        &mut rx,
    )
    .await;
    assert!(app.selection.is_none());
    assert_eq!(app.input, "draft text");
    assert_eq!(clip.writes().len(), 2);

    // 5. Right-click copies active selection and clears it.
    assert!(handle_mouse_event(
        &mut app,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            inner.x,
            screen_y_start
        ),
    ));
    assert!(handle_mouse_event(
        &mut app,
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            inner.x + 4,
            screen_y_start
        ),
    ));
    assert!(handle_mouse_event(
        &mut app,
        mouse(
            MouseEventKind::Up(MouseButton::Left),
            inner.x + 4,
            screen_y_start
        ),
    ));
    assert_eq!(clip.writes().last().map(String::as_str), Some("Hello"));
    assert!(handle_mouse_event(
        &mut app,
        mouse(
            MouseEventKind::Down(MouseButton::Right),
            inner.x + 2,
            screen_y_start
        ),
    ));
    assert!(app.selection.is_none());
    assert_eq!(clip.writes().last().map(String::as_str), Some("Hello"));

    // 6. Alt+M toggles mouse capture mode cleanly.
    assert!(app.mouse_capture);
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT),
        &mut handle,
        &mut rx,
    )
    .await;
    assert!(!app.mouse_capture);

    // 7. `/copy`, `/copy code`, and `/copy all` write clean clipboard payloads.
    handle_slash_command(&mut app, &mut state, None, "copy").await;
    assert!(clip
        .writes()
        .last()
        .unwrap()
        .contains("Hello world from Hermes TUI selection"));
    handle_slash_command(&mut app, &mut state, None, "copy code").await;
    assert_eq!(
        clip.writes().last().map(String::as_str),
        Some("let answer = 42;")
    );
    handle_slash_command(&mut app, &mut state, None, "copy all").await;
    let all = clip.writes().last().cloned().unwrap();
    assert!(all.contains("You:\nQuestion") && all.contains("Hermes:\nHello world"));
}

#[test]
fn renders_live_reasoning_and_tool_output_preview_lines_and_clears_when_done() {
    let mut state = sample_state();
    state.feed_entries.clear();
    let mut app = TuiApp::new();
    app.turn_started = Some(Instant::now());

    // 1. Live reasoning preview shows last 3 lines under Thinking…
    handle_turn_event(
        &mut app,
        &mut state,
        ChatEvent::Stderr("Reasoning: \"thought 1\\nthought 2\\nthought 3\\nthought 4\"".into()),
    );
    let backend = TestBackend::new(90, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let screen1 = buffer_text(&terminal);
    assert!(screen1.contains("Thinking…"), "{screen1}");
    assert!(!screen1.contains("thought 1"), "{screen1}");
    assert!(screen1.contains("thought 2"), "{screen1}");
    assert!(screen1.contains("thought 3"), "{screen1}");
    assert!(screen1.contains("thought 4"), "{screen1}");

    // 2. Live tool output preview shows last 3 lines under running tool card
    handle_turn_event(
        &mut app,
        &mut state,
        ChatEvent::Stderr("Tool started: \"terminal\"\tpython3 run.py".into()),
    );
    handle_turn_event(
        &mut app,
        &mut state,
        ChatEvent::Stderr("Tool output: \"terminal\"\t\"row 1\\nrow 2\\nrow 3\\nrow 4\\n\"".into()),
    );
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let screen2 = buffer_text(&terminal);
    assert!(screen2.contains("terminal"), "{screen2}");
    assert!(!screen2.contains("row 1"), "{screen2}");
    assert!(screen2.contains("row 2"), "{screen2}");
    assert!(screen2.contains("row 3"), "{screen2}");
    assert!(screen2.contains("row 4"), "{screen2}");

    // 3. Finishing the tool clears its transient output lines cleanly.
    handle_turn_event(
        &mut app,
        &mut state,
        ChatEvent::Stderr("Tool completed: \"terminal\"\texit 0".into()),
    );
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let screen3 = buffer_text(&terminal);
    assert!(!screen3.contains("row 4"), "{screen3}");
    assert!(screen3.contains("exit 0"), "{screen3}");
}

#[tokio::test]
async fn compress_clears_input_immediately_and_approval_modal_places_options_above_script() {
    let dir = std::env::temp_dir().join(format!("hsr-tui-compress-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut state = sample_state();
    state.session = Some("sess-123".into());
    let mut app = TuiApp::new();
    app.input = "/compress focus on errors".into();
    app.cursor = app.input.len();
    let mut handle = None;
    let mut rx = None;

    let action = handle_key_event(
        &mut app,
        &mut state,
        Some(&dir),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        app.input.is_empty(),
        "composer input must clear immediately on /compress Enter"
    );
    assert_eq!(
        action,
        Some(TuiAction::Compress {
            session_id: "sess-123".into(),
            focus: Some("focus on errors".into()),
        })
    );

    // Approval modal must render action options ABOVE the script body, even for long scripts.
    let long_script: String = (1..=40)
        .map(|i| format!("print('script_line_{i}')"))
        .collect::<Vec<_>>()
        .join("\n");
    let (reply_tx, _reply_rx) = oneshot::channel();
    app.modal = Some(ActiveModal::Approval {
        request: ApprovalRequest {
            target: "/tmp/work".into(),
            content: long_script,
            action: ApprovalAction::RunCode,
            timeout_secs: 60,
        },
        reason: None,
        reply: Some(reply_tx),
        scroll: 0,
    });
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let screen = buffer_text(&terminal);
    let options_pos = screen
        .find("[y] Approve once")
        .expect("approval options must be visible without scrolling");
    let script_pos = screen
        .find("script_line_1")
        .expect("script body must be visible");
    assert!(
        options_pos < script_pos,
        "approval options must appear above script text:\n{screen}"
    );
}

#[test]
fn palette_scrolls_viewport_to_keep_selected_item_visible() {
    let palette: Vec<PaletteEntry> = (1..=15)
        .map(|i| PaletteEntry {
            replacement: format!("/cmd{i:02}"),
            usage: format!("/cmd{i:02}"),
            description: format!("Command {i}"),
            at_start: None,
        })
        .collect();
    let backend = TestBackend::new(80, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 80, 14);
    terminal
        .draw(|f| render_palette(f, area, " Commands ", &palette, 10))
        .unwrap();
    let screen = buffer_text(&terminal);
    assert!(
        screen.contains("/cmd11"),
        "selected item must be visible:\n{screen}"
    );
    assert!(
        screen.contains("(11/15)"),
        "counter must reflect selection:\n{screen}"
    );
    assert!(
        !screen.contains("/cmd01"),
        "top items should scroll out:\n{screen}"
    );
}

#[tokio::test]
async fn ctrl_a_selects_highlights_and_supports_editor_shortcuts_including_russian_and_putty() {
    let _writes = clipboard::capture_writes_for_test();
    let mut app = TuiApp::new();
    let mut state = sample_state();
    let mut handle = None;
    let mut rx = None;

    for ch in "Привет мир test".chars() {
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
    }
    assert_eq!(app.input, "Привет мир test");

    // Ctrl+A selects the entire composer text and highlights it in blue.
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.selected_input_text(), Some("Привет мир test"));

    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
    let buf = terminal.backend().buffer();
    let has_blue_cell = (0..60).any(|x| {
        (0..12).any(|y| {
            let cell = &buf[(x, y)];
            cell.symbol() == "П" && cell.bg == Color::Blue
        })
    });
    assert!(has_blue_cell, "selected composer text must be highlighted");

    // Ctrl+C while composer text is selected copies it without wiping the input.
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "Привет мир test");
    assert_eq!(
        _writes.writes().last().map(String::as_str),
        Some("Привет мир test")
    );

    // Typing any character replaces the active selection.
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "X");
    assert!(app.input_selection().is_none());

    // Russian layout Ctrl+Ф selects all, and Ctrl+Ч cuts it to clipboard.
    app.insert_pasted("alpha beta gamma");
    assert_eq!(app.input, "Xalpha beta gamma");
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('ф'), KeyModifiers::CONTROL),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.selected_input_text(), Some("Xalpha beta gamma"));
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('ч'), KeyModifiers::CONTROL),
        &mut handle,
        &mut rx,
    )
    .await;
    assert!(app.input.is_empty());
    assert_eq!(
        _writes.writes().last().map(String::as_str),
        Some("Xalpha beta gamma")
    );

    // Word navigation, Shift selection, Ctrl+Backspace, and PuTTY Ctrl+H.
    app.insert_pasted("one two three");
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.selected_input_text(), Some("three"));
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "one two ");
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "one two");
    handle_key_event(
        &mut app,
        &mut state,
        None,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        &mut handle,
        &mut rx,
    )
    .await;
    assert_eq!(app.input, "one ");
}
