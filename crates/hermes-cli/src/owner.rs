use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::chat::{self, ChatRequest, Event};

const DIRECTORY: &str = "rust-owner";
const SOCKET: &str = "owner.sock";
const VERSION: u32 = 1;
const CANCEL: u8 = 1;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_BUFFER: usize = 8 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize, Serialize)]
struct Start {
    version: u32,
    request: ChatRequest,
}

struct OwnerSocket {
    path: PathBuf,
    _lock: File,
}

impl Drop for OwnerSocket {
    fn drop(&mut self) {
        // The file lock remains held while removing the socket.
        let _ = fs::remove_file(&self.path);
    }
}

fn uid() -> u32 {
    // SAFETY: geteuid takes no arguments and does not read or modify Rust memory.
    unsafe { libc::geteuid() }
}

fn owner_directory(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "local owner directory must be private and owned by the current user",
                ));
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn socket_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local owner socket",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn check_peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() == uid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local owner peer has a different user ID",
        ))
    }
}

pub async fn connect(home: &Path) -> Result<Option<UnixStream>, String> {
    let directory = home.join(DIRECTORY);
    let path = directory.join(SOCKET);
    if !owner_directory(&directory).map_err(|error| error.to_string())?
        || !socket_exists(&path).map_err(|error| error.to_string())?
    {
        return Ok(None);
    }
    let stream = UnixStream::connect(&path)
        .await
        .map_err(|error| format!("cannot connect to local owner: {error}"))?;
    check_peer(&stream).map_err(|error| error.to_string())?;
    Ok(Some(stream))
}

pub async fn serve(home: &Path) -> Result<(), String> {
    fs::create_dir_all(home).map_err(|error| error.to_string())?;
    let directory = home.join(DIRECTORY);
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    if !owner_directory(&directory).map_err(|error| error.to_string())? {
        return Err("local owner directory is unavailable".into());
    }
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("owner.lock"))
        .map_err(|error| error.to_string())?;
    if !lock
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("invalid local owner lock file".into());
    }
    lock.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => "local owner is already running for this profile".into(),
        fs::TryLockError::Error(error) => error.to_string(),
    })?;
    let path = directory.join(SOCKET);
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    if socket_exists(&path).map_err(|error| error.to_string())? {
        fs::remove_file(&path).map_err(|error| error.to_string())?;
    }
    let listener = UnixListener::bind(&path).map_err(|error| error.to_string())?;
    let socket = OwnerSocket { path, _lock: lock };
    fs::set_permissions(&socket.path, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    eprintln!("Local session owner listening on {}", socket.path.display());
    let mut turns = JoinSet::new();
    let home = Arc::<Path>::from(home);
    let result = loop {
        tokio::select! {
            result = &mut interrupt => break result.map_err(|error| error.to_string()),
            _ = terminate.recv() => break Ok(()),
            Some(_) = turns.join_next(), if !turns.is_empty() => {},
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) if check_peer(&stream).is_ok() => {
                    let home = home.clone();
                    turns.spawn(async move { let _ = handle(stream, &home).await; });
                }
                Ok(_) => {}
                Err(error) => break Err(error.to_string()),
            }
        }
    };
    drop(listener);
    turns.shutdown().await;
    drop(socket);
    result
}

pub async fn forward(mut stream: UnixStream, request: ChatRequest) -> Result<u8, String> {
    let start = Start {
        version: VERSION,
        request,
    };
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let result = tokio::select! {
        result = timeout(IO_TIMEOUT, write_frame(&mut stream, &start)) => {
            result.map_err(|_| "local owner request timed out")?
        }
        signal = &mut interrupt => {
            signal.map_err(|error| format!("signal handler failed: {error}"))?;
            eprintln!("interrupted");
            return Ok(130);
        }
    };
    result.map_err(|error| format!("cannot send request to local owner: {error}"))?;
    let (mut reader, mut writer) = stream.into_split();
    let receive = async {
        loop {
            let event = read_frame::<Event>(&mut reader).await.map_err(|error| {
                format!("local owner disconnected before reporting the turn result: {error}")
            })?;
            if let Event::Exit(code) = event {
                return Ok(code);
            }
            chat::display(event).map_err(|error| error.to_string())?;
        }
    };
    tokio::pin!(receive);
    tokio::select! {
        result = &mut receive => result,
        signal = &mut interrupt => {
            signal.map_err(|error| format!("signal handler failed: {error}"))?;
            match timeout(IO_TIMEOUT, async {
                writer.write_u8(CANCEL).await.map_err(|error| error.to_string())?;
                receive.await
            }).await {
                Ok(result) => result,
                Err(_) => Err("local owner did not acknowledge cancellation".into()),
            }
        }
    }
}

async fn handle(mut stream: UnixStream, home: &Path) -> io::Result<()> {
    let Start { version, request } = timeout(IO_TIMEOUT, read_frame(&mut stream)).await??;
    if version != VERSION {
        write_frame(
            &mut stream,
            &Event::Stderr("incompatible local owner protocol".into()),
        )
        .await?;
        return write_frame(&mut stream, &Event::Exit(1)).await;
    }
    let (mut reader, mut writer) = stream.into_split();
    let (events, mut buffered) = mpsc::unbounded_channel();
    let queued = AtomicUsize::new(0);
    let stalled = Notify::new();
    let execute = async {
        let result = tokio::select! {
            biased;
            control = reader.read_u8() => match control {
                Ok(CANCEL) => {
                    let _ = events.send((Event::Stderr("interrupted".into()), 0));
                    Ok(130)
                }
                Ok(_) => Err("invalid local owner control byte".into()),
                Err(_) => return Ok(()),
            },
            _ = stalled.notified() => Err("local owner output buffer is full".into()),
            result = chat::execute(Some(home), request, |event| {
                // Bound memory by bytes so a burst of small SSE deltas can still be forwarded.
                let bytes = match &event {
                    Event::Stdout(text) | Event::Stderr(text) => text.capacity() + 64,
                    Event::Exit(_) => 64,
                };
                if queued.fetch_update(Relaxed, Relaxed, |total| {
                    total.checked_add(bytes).filter(|total| *total <= MAX_OUTPUT_BUFFER)
                }).is_err() || events.send((event, bytes)).is_err() {
                    stalled.notify_one();
                    return false;
                }
                true
            }) => result,
        };
        let code = match result {
            Ok(code) => code,
            Err(error) => {
                let _ = events.send((Event::Stderr(error), 0));
                1
            }
        };
        let _ = events.send((Event::Exit(code), 0));
        drop(events);
        Ok::<(), io::Error>(())
    };
    let output = async {
        while let Some((event, bytes)) = buffered.recv().await {
            timeout(IO_TIMEOUT, write_frame(&mut writer, &event)).await??;
            queued.fetch_sub(bytes, Relaxed);
        }
        Ok(())
    };
    tokio::try_join!(execute, output)?;
    Ok(())
}

async fn read_frame<T: DeserializeOwned>(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<T> {
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local owner frame length",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid local owner frame"))
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local owner frame is too large",
        ));
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await
}
