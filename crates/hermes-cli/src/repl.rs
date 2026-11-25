//! Interactive conversation orchestrator.
//!
//! Selects the active console UI mode (`inline`, `tui`, or automatic `plain` fallback for pipes,
//! `TERM=dumb`, and `NO_COLOR`) and allows seamless runtime switching via `/ui [inline|tui|plain]`
//! without losing the active session, model choice, compaction guard, or shell state.

use std::io::IsTerminal;
use std::path::Path;

use crate::chat::ChatRequest;
use crate::ui::{self, ActiveUi, ReplState, UiOutcome};

pub async fn run(home: Option<&Path>, request: ChatRequest) -> Result<u8, String> {
    let stdin_tty = std::io::stdin().is_terminal();
    let stdout_tty = std::io::stdout().is_terminal();
    let no_color = ui::env_no_color();
    let term = std::env::var("TERM").ok();

    let mut active = ui::resolve_ui_mode(
        request.ui_mode(),
        stdin_tty,
        stdout_tty,
        no_color,
        term.as_deref(),
    );
    let mut state = ReplState::new(home, request, !no_color);
    let mut startup_recap = if let Some(initial_id) = state.session.clone() {
        match ui::load_resumed_session(home, &mut state, &initial_id) {
            Ok(recap) => Some(recap),
            Err(err) => {
                state.feed_entries.push(ui::FeedEntry::Notice {
                    kind: ui::NoticeKind::Error,
                    text: format!("Could not resume session '{initial_id}': {err}"),
                });
                None
            }
        }
    } else {
        None
    };
    let mut first_entry = true;

    loop {
        let outcome = match active {
            ActiveUi::Plain | ActiveUi::Inline => {
                ui::inline::run(home, &mut state, active, first_entry, startup_recap.take()).await?
            }
            ActiveUi::Tui => {
                startup_recap = None;
                ui::tui::run(home, &mut state).await?
            }
        };
        first_entry = false;
        match outcome {
            UiOutcome::Exit => break,
            UiOutcome::Switch(next) => {
                active = next;
                if active == ActiveUi::Inline && state.color {
                    eprintln!("Switched to inline streaming mode.");
                } else if active == ActiveUi::Plain {
                    eprintln!("Switched to plain mode.");
                }
            }
        }
    }

    let styled = active == ActiveUi::Inline && state.color;
    ui::inline::announce(&state.session, styled);
    Ok(state.exit_code)
}
