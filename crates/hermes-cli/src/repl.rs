//! The interactive conversation: read a line, run a turn, repeat.
//!
//! Each turn is an ordinary chat request. The first one saves a new session and the next ones
//! resume it, so history, compression and storage behave exactly as with `--resume`. A fixed
//! state scope keeps the working directory, background processes and Python kernel of the
//! conversation together across the turns.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Config, Editor};
use tokio::task::spawn_blocking;

use crate::approval;
use crate::chat::{self, ChatRequest, Event};

type Lines = Editor<(), FileHistory>;

const PROMPT: &str = "› ";
const CONTINUATION: &str = "… ";
const HELP: &str = "Commands:\n  /help          show this list\n  /new           start a new conversation (the old one stays saved)\n  /exit, /quit   leave (Ctrl-D also works)\nA line ending in \\ continues on the next line. Ctrl-C stops a running turn.";

enum Input {
    Line(String),
    Interrupted,
    End,
}

async fn read(editor: Lines, prompt: &'static str) -> (Lines, Result<String, ReadlineError>) {
    spawn_blocking(move || {
        let mut editor = editor;
        let line = editor.readline(prompt);
        (editor, line)
    })
    .await
    .expect("the line reader does not panic")
}

/// One logical input: physical lines joined while they end in a backslash.
async fn next_input(editor: &mut Lines) -> Input {
    let mut text = String::new();
    let mut prompt = PROMPT;
    loop {
        let (returned, line) = read(std::mem::replace(editor, placeholder()), prompt).await;
        *editor = returned;
        match line {
            Ok(line) => match line.strip_suffix('\\') {
                Some(head) => {
                    text.push_str(head);
                    text.push('\n');
                    prompt = CONTINUATION;
                }
                None => {
                    text.push_str(&line);
                    return Input::Line(text);
                }
            },
            Err(ReadlineError::Interrupted) => return Input::Interrupted,
            Err(_) => return Input::End,
        }
    }
}

fn placeholder() -> Lines {
    Editor::with_config(Config::default()).expect("a line editor can be created")
}

fn history_path(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|home| home.join(".hermes_history"))
}

pub async fn run(home: Option<&Path>, request: ChatRequest) -> Result<u8, String> {
    let tty = std::io::stdin().is_terminal();
    let mut editor: Lines = Editor::with_config(
        Config::builder()
            .auto_add_history(false)
            .history_ignore_dups(true)
            .map_err(|error| error.to_string())?
            .build(),
    )
    .map_err(|error| error.to_string())?;
    let history = history_path(home);
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }
    if tty {
        eprintln!("hermes: type a request, /help for commands, Ctrl-D to leave");
    }
    let scope = format!("repl-{}", std::process::id());
    let mut session = request.resume().map(str::to_owned);
    let mut code = 0;
    loop {
        let line = match next_input(&mut editor).await {
            Input::Line(line) => line,
            Input::Interrupted => continue,
            Input::End => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(command) = line.strip_prefix('/').filter(|_| !line.contains('\n')) {
            match command.trim() {
                "exit" | "quit" => break,
                "help" => println!("{HELP}"),
                "new" => {
                    announce(&session);
                    session = None;
                }
                other => eprintln!("Unknown command /{other}; /help lists the commands."),
            }
            continue;
        }
        let _ = editor.add_history_entry(line);
        let turn = request.clone().for_turn(line, &scope, session.as_deref());
        let mut saved = None;
        let mut output_open = true;
        let outcome = tokio::select! {
            result = chat::execute(
                home,
                turn,
                |event| {
                    if let Event::Stderr(text) = &event {
                        if let Some(id) = text.strip_prefix("Session: ") {
                            saved = Some(id.to_owned());
                            return true;
                        }
                    }
                    output_open = chat::display(event).is_ok();
                    output_open
                },
                approval::prompt,
            ) => Some(result),
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| format!("signal handler failed: {error}"))?;
                None
            }
        };
        match outcome {
            None => eprintln!("\ninterrupted"),
            Some(Ok(status)) => code = status,
            Some(Err(error)) => {
                eprintln!("{error}");
                if !output_open {
                    break;
                }
            }
        }
        if let Some(id) = saved {
            session = Some(id);
        }
    }
    if let Some(path) = &history {
        let _ = editor.save_history(path);
    }
    announce(&session);
    Ok(code)
}

fn announce(session: &Option<String>) {
    if let Some(id) = session {
        eprintln!("Session: {id}  (continue with: hermes chat --resume {id})");
    }
}
