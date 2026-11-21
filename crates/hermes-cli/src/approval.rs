use hermes_runtime::turn::{ApprovalDecision, ApprovalRequest};

pub fn available() -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && terminal::open().is_ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

pub async fn prompt(request: ApprovalRequest) -> ApprovalDecision {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::time::Duration;

        use hermes_runtime::turn::ApprovalAction;

        if matches!(request.action, ApprovalAction::Clarify) {
            // A limit of 0 waits for as long as the person takes.
            let asking = terminal::ask_questions(&request);
            let outcome = if request.timeout_secs == 0 {
                Ok(asking.await)
            } else {
                tokio::time::timeout(Duration::from_secs(u64::from(request.timeout_secs)), asking)
                    .await
            };
            return match outcome {
                Ok(Ok(decision)) => decision,
                Ok(Err(_)) => ApprovalDecision::Unavailable,
                Err(_) => ApprovalDecision::TimedOut,
            };
        }
        match tokio::time::timeout(
            Duration::from_secs(u64::from(request.timeout_secs)),
            terminal::ask(&request),
        )
        .await
        {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => ApprovalDecision::Unavailable,
            Err(_) => ApprovalDecision::TimedOut,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = request;
        ApprovalDecision::Unavailable
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod terminal {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    use tokio::io::unix::AsyncFd;

    use hermes_runtime::turn::ApprovalAction;

    use super::{ApprovalDecision, ApprovalRequest};

    pub(super) fn open() -> io::Result<File> {
        #[cfg(target_os = "linux")]
        let device = "/dev/tty";
        #[cfg(target_os = "macos")]
        let device = stdin_device()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_NOFOLLOW)
            .open(device)?;
        // SAFETY: these calls read the process group associated with a live
        // terminal descriptor and the group ID for this process.
        if unsafe { libc::tcgetpgrp(file.as_raw_fd()) } != unsafe { libc::getpgrp() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "terminal is not in foreground",
            ));
        }
        Ok(file)
    }

    #[cfg(target_os = "macos")]
    fn stdin_device() -> io::Result<std::path::PathBuf> {
        use std::ffi::{CStr, OsStr};
        use std::os::unix::ffi::OsStrExt;

        // Darwin rejects kqueue registration on /dev/tty, but supports it on
        // the underlying terminal device. Open it separately for O_NONBLOCK.
        let mut name = [0; libc::PATH_MAX as usize];
        // SAFETY: the live stdin descriptor is borrowed and the buffer is
        // writable for the supplied length.
        let error = unsafe {
            libc::ttyname_r(
                io::stdin().as_raw_fd(),
                name.as_mut_ptr().cast(),
                name.len(),
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        let path = CStr::from_bytes_until_nul(&name)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(OsStr::from_bytes(path.to_bytes()).into())
    }

    pub(super) async fn ask(request: &ApprovalRequest) -> io::Result<ApprovalDecision> {
        let tty = open()?;
        // SAFETY: tcflush clears pending terminal input on a live descriptor.
        if unsafe { libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let tty = AsyncFd::new(tty)?;
        let (action, preview) = match request.action {
            ApprovalAction::CreateFile => ("Create file", "New file content"),
            ApprovalAction::ReplaceFile => ("Replace file", "New file content"),
            ApprovalAction::RunCommand => ("Run command in", "Command"),
            ApprovalAction::ManageSkills => ("Change skills in", "Changes"),
            ApprovalAction::Clarify => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
        };
        let prompt = format!(
            "\n{action} {:?}\n{preview} (escaped)\n{:?}\nApprove once? [y/N] ",
            request.target, request.content
        );
        let mut remaining = prompt.as_bytes();
        while !remaining.is_empty() {
            let mut ready = tty.writable().await?;
            match ready.try_io(|fd| fd.get_ref().write(remaining)) {
                Ok(Ok(0)) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(Ok(written)) => remaining = &remaining[written..],
                Ok(Err(error)) => return Err(error),
                Err(_) => {}
            }
        }

        let mut answer = Vec::new();
        let mut bytes = [0; 128];
        loop {
            let mut ready = tty.readable().await?;
            match ready.try_io(|fd| fd.get_ref().read(&mut bytes)) {
                Ok(Ok(0)) => return Ok(ApprovalDecision::Denied),
                Ok(Ok(count)) => {
                    let received = &bytes[..count];
                    if let Some(end) = received.iter().position(|&byte| byte == b'\n') {
                        answer.extend_from_slice(&received[..end]);
                        let confirmed = std::str::from_utf8(&answer).ok().is_some_and(|answer| {
                            let answer = answer.trim();
                            answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
                        });
                        return Ok(if confirmed {
                            ApprovalDecision::Approved
                        } else {
                            ApprovalDecision::Denied
                        });
                    }
                    answer.extend_from_slice(received);
                    if answer.len() >= bytes.len() {
                        return Ok(ApprovalDecision::Denied);
                    }
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => {}
            }
        }
    }

    async fn write_all(tty: &AsyncFd<File>, text: &str) -> io::Result<()> {
        let mut remaining = text.as_bytes();
        while !remaining.is_empty() {
            let mut ready = tty.writable().await?;
            match ready.try_io(|fd| fd.get_ref().write(remaining)) {
                Ok(Ok(0)) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(Ok(written)) => remaining = &remaining[written..],
                Ok(Err(error)) => return Err(error),
                Err(_) => {}
            }
        }
        Ok(())
    }

    /// The next line typed at the terminal, or `None` at end of input.
    async fn read_line(tty: &AsyncFd<File>, pending: &mut Vec<u8>) -> io::Result<Option<String>> {
        const MAX_LINE: usize = 8192;
        let mut bytes = [0; 512];
        loop {
            if let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                return Ok(Some(
                    String::from_utf8_lossy(&line[..end])
                        .trim_end_matches('\r')
                        .to_owned(),
                ));
            }
            if pending.len() >= MAX_LINE {
                return Ok(Some(String::from_utf8_lossy(pending).into_owned()));
            }
            let mut ready = tty.readable().await?;
            match ready.try_io(|fd| fd.get_ref().read(&mut bytes)) {
                Ok(Ok(0)) => return Ok(None),
                Ok(Ok(count)) => pending.extend_from_slice(&bytes[..count]),
                Ok(Err(error)) => return Err(error),
                Err(_) => {}
            }
        }
    }

    /// Put the questions of a `clarify` call to the person and collect the answers.
    pub(super) async fn ask_questions(request: &ApprovalRequest) -> io::Result<ApprovalDecision> {
        use serde_json::{json, Map, Value};

        let questions: Vec<Value> = serde_json::from_str(&request.content)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        let tty = open()?;
        // SAFETY: tcflush clears pending terminal input on a live descriptor.
        if unsafe { libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let tty = AsyncFd::new(tty)?;
        let mut pending = Vec::new();
        let mut answers = Map::new();
        for question in &questions {
            let choices: Vec<String> = question["choices"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|c| c.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let multi = question["multi_select"].as_bool() == Some(true);
            let mut screen = format!(
                "\n{}\n",
                super::printable(question["question"].as_str().unwrap_or(""))
            );
            for (index, choice) in choices.iter().enumerate() {
                screen.push_str(&format!("  {}) {}\n", index + 1, super::printable(choice)));
            }
            screen.push_str(&match (choices.is_empty(), multi) {
                (true, _) => "Your answer (Enter to skip): ".to_owned(),
                (false, false) => "Number, or type your own answer (Enter to skip): ".to_owned(),
                (false, true) => {
                    "Numbers separated by commas, or type your own answer (Enter to skip): "
                        .to_owned()
                }
            });
            write_all(&tty, &screen).await?;
            let Some(line) = read_line(&tty, &mut pending).await? else {
                return Ok(ApprovalDecision::Denied);
            };
            let qid = question["qid"].as_str().unwrap_or_default().to_owned();
            answers.insert(
                qid,
                super::parse_answer(&line, &choices, multi).unwrap_or(Value::Null),
            );
        }
        let reply = json!({"answers": answers, "outcome": "submitted"});
        Ok(ApprovalDecision::Answered(reply.to_string()))
    }
}

/// Text for the terminal with control characters (escape sequences included) made harmless.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// An answer line as the value for the question: a choice by number, or the typed text.
/// `None` is a skipped question.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn parse_answer(line: &str, choices: &[String], multi: bool) -> Option<serde_json::Value> {
    use serde_json::Value;

    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let pick = |token: &str| -> Option<String> {
        let number: usize = token.trim().parse().ok()?;
        choices.get(number.checked_sub(1)?).cloned()
    };
    if multi {
        let tokens: Vec<&str> = line
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        let picked: Vec<Value> = tokens
            .iter()
            .map(|token| Value::String(pick(token).unwrap_or_else(|| (*token).to_owned())))
            .collect();
        return (!picked.is_empty()).then_some(Value::Array(picked));
    }
    Some(Value::String(pick(line).unwrap_or_else(|| line.to_owned())))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn answers_pick_choices_by_number_or_keep_typed_text() {
        let choices = vec!["A (Recommended)".to_owned(), "B".to_owned()];
        assert_eq!(parse_answer("2", &choices, false), Some(json!("B")));
        assert_eq!(
            parse_answer(" 1 ", &choices, false),
            Some(json!("A (Recommended)"))
        );
        assert_eq!(parse_answer("3", &choices, false), Some(json!("3")));
        assert_eq!(
            parse_answer("something else", &choices, false),
            Some(json!("something else"))
        );
        assert_eq!(parse_answer("", &choices, false), None);
        assert_eq!(
            parse_answer("1, 2,custom", &choices, true),
            Some(json!(["A (Recommended)", "B", "custom"]))
        );
        assert_eq!(parse_answer(" , ", &choices, true), None);
        assert_eq!(parse_answer("hello", &[], false), Some(json!("hello")));
    }

    #[test]
    fn control_characters_cannot_reach_the_terminal() {
        assert_eq!(
            printable("ok\u{1b}[31mred\u{7}"),
            "ok\u{fffd}[31mred\u{fffd}"
        );
        assert_eq!(printable("a\tb\nc"), "a\tb\nc");
    }
}
