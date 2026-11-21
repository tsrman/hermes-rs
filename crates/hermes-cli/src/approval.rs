use hermes_runtime::turn::{ApprovalDecision, ApprovalRequest};

pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && terminal::open().is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub async fn prompt(request: ApprovalRequest) -> ApprovalDecision {
    #[cfg(target_os = "linux")]
    {
        use std::time::Duration;
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = request;
        ApprovalDecision::Unavailable
    }
}

#[cfg(target_os = "linux")]
mod terminal {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    use tokio::io::unix::AsyncFd;

    use hermes_runtime::turn::ApprovalAction;

    use super::{ApprovalDecision, ApprovalRequest};

    pub(super) fn open() -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/dev/tty")?;
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
}
