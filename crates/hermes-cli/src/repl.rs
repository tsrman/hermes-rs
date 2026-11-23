//! The interactive conversation: read a line, run a turn, repeat.
//!
//! Each turn is an ordinary chat request. The first one saves a new session and the next ones
//! resume it, so history, compression and storage behave exactly as with `--resume`. A fixed
//! state scope keeps the working directory, background processes and Python kernel of the
//! conversation together across the turns.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hermes_runtime::turn::CompactionGuard;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Config, Editor};
use tokio::task::spawn_blocking;

use crate::approval;
use crate::chat::{self, ChatRequest, Event, ManualCompress};

type Lines = Editor<(), FileHistory>;

const PROMPT: &str = "› ";
const CONTINUATION: &str = "… ";
const HELP: &str = "Commands:\n  /help              show this list\n  /new               start a new conversation (the old one stays saved)\n  /sessions          list recent sessions\n  /resume <id>       continue another session\n  /model [name]      show or change the model for the next turns\n  /compress [topic]  compress the current session history now\n  /yolo              toggle approving file changes and commands without asking\n  /retry             send the last turn again when it was interrupted or failed\n  /exit, /quit       leave (Ctrl-D also works)\nA line ending in \\ continues on the next line. Ctrl-C stops a running turn.";

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
    let mut request = request;
    let mut session = request.resume().map(str::to_owned);
    let guard = Mutex::new(CompactionGuard::new());
    let mut code = 0;
    // A turn that was interrupted or failed is not saved, so `/retry` may send it again.
    let mut unsaved: Option<String> = None;
    let mut rerun: Option<String> = None;
    loop {
        let line = match rerun.take() {
            Some(line) => line,
            None => match next_input(&mut editor).await {
                Input::Line(line) => line.trim().to_owned(),
                Input::Interrupted => continue,
                Input::End => break,
            },
        };
        let line = line.as_str();
        if line.is_empty() {
            continue;
        }
        if let Some(command) = line.strip_prefix('/').filter(|_| !line.contains('\n')) {
            let (name, argument) = command
                .split_once(char::is_whitespace)
                .map_or((command, ""), |(name, rest)| (name, rest.trim()));
            match name {
                "exit" | "quit" => break,
                "help" => println!("{HELP}"),
                "new" => {
                    announce(&session);
                    session = None;
                    unsaved = None;
                    guard.lock().unwrap().reset();
                }
                "model" if argument.is_empty() => {
                    println!("{}", request.model().unwrap_or("(not set)"));
                }
                "model" => {
                    request.set_model(argument);
                    guard.lock().unwrap().reset();
                    println!("Model for the next turns: {argument}");
                }
                "compress" | "compact" => {
                    let Some(home) = home else {
                        eprintln!("Cannot compress: profile root is unavailable.");
                        continue;
                    };
                    let Some(id) = session.as_deref() else {
                        println!("Nothing to compress yet: start a conversation first.");
                        continue;
                    };
                    let focus = (!argument.is_empty()).then_some(argument);
                    match chat::compress_session(home, &request, id, focus, &guard).await {
                        Ok(ManualCompress::Applied { archived }) => {
                            println!("Compressed session {id}: archived {archived} message(s) into a summary.");
                        }
                        Ok(ManualCompress::Unchanged) => {
                            println!("Nothing to compress: conversation is short enough already.");
                        }
                        Ok(ManualCompress::Skipped) => {
                            eprintln!("Compression skipped: summary failed or did not shorten the conversation.");
                        }
                        Err(error) => eprintln!("Compression failed: {error}"),
                    }
                }
                "yolo" => {
                    if let Some(policy) = hermes_runtime::approval_policy::installed() {
                        let on = !policy.approves_all();
                        policy.set_approve_all(on);
                        println!(
                            "{}",
                            if on {
                                "yolo on: file changes and commands run without asking, except what approvals.deny names"
                            } else {
                                "yolo off: you will be asked again"
                            }
                        );
                    }
                }
                "sessions" => list_sessions(home),
                "resume" if argument.is_empty() => eprintln!("Usage: /resume <session id>"),
                "resume" => {
                    announce(&session);
                    session = Some(argument.to_owned());
                    unsaved = None;
                    guard.lock().unwrap().reset();
                    println!("The next turn continues session {argument}");
                }
                "retry" => match unsaved.take() {
                    Some(previous) => rerun = Some(previous),
                    None => eprintln!(
                        "Nothing to retry: the last turn finished and is saved. Ask again to add to it."
                    ),
                },
                other => eprintln!("Unknown command /{other}; /help lists the commands."),
            }
            continue;
        }
        let _ = editor.add_history_entry(line);
        let turn = request.clone().for_turn(line, &scope, session.as_deref());
        let mut saved = None;
        let mut usage = None;
        let mut output_open = true;
        let started = std::time::Instant::now();
        let outcome = tokio::select! {
            result = chat::execute_with_guard(
                home,
                turn,
                Some(&guard),
                |event| {
                    if let Event::Stderr(text) = &event {
                        if let Some(id) = text.strip_prefix("Session: ") {
                            saved = Some(id.to_owned());
                            return true;
                        }
                        if let Some(counts) = text.strip_prefix("Usage: ") {
                            usage = counts.split_once(' ')
                                .and_then(|(i, o)| Some((i.parse::<u64>().ok()?, o.parse::<u64>().ok()?)));
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
            None => {
                hermes_runtime::mcp::flush_cancellations().await;
                eprintln!("\ninterrupted (the turn is not saved; /retry sends it again)");
                unsaved = Some(line.to_owned());
            }
            Some(Ok(status)) => {
                code = status;
                unsaved = None;
                let seconds = started.elapsed().as_secs_f64();
                match usage {
                    Some((input, output)) if input + output > 0 => {
                        eprintln!("[{seconds:.1}s · {input} in / {output} out]");
                    }
                    _ => eprintln!("[{seconds:.1}s]"),
                }
            }
            Some(Err(error)) => {
                eprintln!("{error}");
                unsaved = Some(line.to_owned());
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

fn list_sessions(home: Option<&Path>) {
    let Some(home) = home else { return };
    let listed = hermes_runtime::storage::SessionStore::open_read_only(home)
        .and_then(|store| store.recent(10, None));
    match listed {
        Ok(sessions) if sessions.is_empty() => println!("No saved sessions yet."),
        Ok(sessions) => {
            for session in sessions {
                let text = session.title.or(session.preview).unwrap_or_default();
                println!(
                    "{}  {:>4} msgs  {}",
                    session.id,
                    session.message_count,
                    text.replace('\n', " ")
                );
            }
        }
        Err(_) => println!("No saved sessions yet."),
    }
}
