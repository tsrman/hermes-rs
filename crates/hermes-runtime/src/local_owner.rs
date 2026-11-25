//! Shared cross-platform local session owner transport and reconnectable turn hub for CLI and GUI.
//!
//! - On Unix (`linux`, `macos`): binds a private Unix domain socket `<home>/rust-owner/owner.sock`
//!   (`0700` dir, `0600` socket, `peer_cred().uid()` verification, exclusive `owner.lock`).
//! - On Windows: binds a loopback TCP listener on `127.0.0.1:0` and writes a private
//!   `<home>/rust-owner/owner.addr` file carrying `{port, token}` (256-bit random token checked in
//!   constant time on every connection), guarded by an exclusive `owner.lock`.
//!
//! Protocol v1 preserves the exact framing used by `hsr chat` (`compat/test_owner.py`).
//! Protocol v2 adds multi-client GUI RPCs: session listing/search, transcript loading, profile
//! discovery, managed turns with monotonic event sequence replay, pending approval survival across
//! disconnects, and `SubscribeTurn { after_seq }` reconnection.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::hash::BuildHasher;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{broadcast, oneshot};
use tokio::time::timeout;

use crate::approval_policy::{ApprovalPolicy, SessionGrants};
use crate::file_tools::{ApprovalDecision, ApprovalRequest};
use crate::storage::{Session, SessionStore, SessionSummary as StorageSessionSummary};

pub const OWNER_DIRECTORY: &str = "rust-owner";
pub const OWNER_SOCKET_NAME: &str = "owner.sock";
pub const OWNER_ADDR_NAME: &str = "owner.addr";
pub const OWNER_LOCK_NAME: &str = "owner.lock";

pub const PROTOCOL_V1: u32 = 1;
pub const PROTOCOL_V2: u32 = 2;

pub const CONTROL_CANCEL: u8 = 1;
pub const CONTROL_APPROVAL_RESPONSE: u8 = 2;

pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REPLAY_EVENTS: usize = 4096;
const MAX_COMPLETED_TURNS: usize = 32;
const COMPLETED_TURN_TTL: Duration = Duration::from_secs(5 * 60);

// ── Cross-platform transport stream & listener ───────────────────────────────────────────────────

pub enum OwnerStream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
}

impl AsyncRead for OwnerStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for OwnerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(not(unix))]
#[derive(Deserialize, Serialize)]
struct LoopbackEndpointFile {
    port: u16,
    token: String,
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and touches no Rust memory.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn check_owner_directory(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0
            {
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

#[cfg(not(unix))]
fn check_owner_directory(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "local owner path is not a directory",
                ));
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn socket_exists(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == current_uid() => {
            Ok(true)
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local owner socket",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn check_unix_peer(stream: &tokio::net::UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() == current_uid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local owner peer has a different user ID",
        ))
    }
}

#[cfg(not(unix))]
fn random_auth_token() -> io::Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut raw = [0u8; 32];
    SystemRandom::new()
        .fill(&mut raw)
        .map_err(|_| io::Error::other("CSPRNG failed generating owner token"))?;
    let mut hex = String::with_capacity(64);
    for b in raw {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    Ok(hex)
}

#[cfg(not(unix))]
fn verify_auth_token(expected: &str, got: &str) -> bool {
    let a = expected.as_bytes();
    let b = got.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub struct OwnerListener {
    endpoint_path: PathBuf,
    display_addr: String,
    _lock: File,
    #[cfg(unix)]
    unix: tokio::net::UnixListener,
    #[cfg(not(unix))]
    tcp: tokio::net::TcpListener,
    #[cfg(not(unix))]
    token: String,
}

impl Drop for OwnerListener {
    fn drop(&mut self) {
        // Lock stays held until drop finishes so another owner cannot race file removal.
        let _ = fs::remove_file(&self.endpoint_path);
    }
}

impl OwnerListener {
    pub fn display_address(&self) -> &str {
        &self.display_addr
    }

    pub async fn bind(home: &Path) -> Result<Self, String> {
        fs::create_dir_all(home).map_err(|e| e.to_string())?;
        let directory = home.join(OWNER_DIRECTORY);

        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
            match fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.to_string()),
            }
            if !check_owner_directory(&directory).map_err(|e| e.to_string())? {
                return Err("local owner directory is unavailable".into());
            }
            let lock = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(directory.join(OWNER_LOCK_NAME))
                .map_err(|e| e.to_string())?;
            if !lock.metadata().map_err(|e| e.to_string())?.is_file() {
                return Err("invalid local owner lock file".into());
            }
            lock.try_lock().map_err(|error| match error {
                fs::TryLockError::WouldBlock => {
                    "local owner is already running for this profile".into()
                }
                fs::TryLockError::Error(error) => error.to_string(),
            })?;
            let path = directory.join(OWNER_SOCKET_NAME);
            if socket_exists(&path).map_err(|e| e.to_string())? {
                fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
            let unix = tokio::net::UnixListener::bind(&path).map_err(|e| e.to_string())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
            let display_addr = path.display().to_string();
            Ok(Self {
                endpoint_path: path,
                display_addr,
                _lock: lock,
                unix,
            })
        }

        #[cfg(not(unix))]
        {
            fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let lock = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join(OWNER_LOCK_NAME))
                .map_err(|e| e.to_string())?;
            lock.try_lock().map_err(|error| match error {
                fs::TryLockError::WouldBlock => {
                    "local owner is already running for this profile".into()
                }
                fs::TryLockError::Error(error) => error.to_string(),
            })?;
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| format!("cannot bind Windows local owner port: {e}"))?;
            let port = tcp.local_addr().map_err(|e| e.to_string())?.port();
            let token = random_auth_token().map_err(|e| e.to_string())?;
            let addr_path = directory.join(OWNER_ADDR_NAME);
            let payload = serde_json::to_vec(&LoopbackEndpointFile {
                port,
                token: token.clone(),
            })
            .map_err(|e| e.to_string())?;
            fs::write(&addr_path, payload).map_err(|e| e.to_string())?;
            let display_addr = format!("127.0.0.1:{port} ({})", addr_path.display());
            Ok(Self {
                endpoint_path: addr_path,
                display_addr,
                _lock: lock,
                tcp,
                token,
            })
        }
    }

    pub async fn accept(&self) -> io::Result<OwnerStream> {
        #[cfg(unix)]
        loop {
            let (stream, _) = self.unix.accept().await?;
            if check_unix_peer(&stream).is_ok() {
                return Ok(OwnerStream::Unix(stream));
            }
        }

        #[cfg(not(unix))]
        loop {
            let (mut stream, _) = self.tcp.accept().await?;
            let auth: io::Result<String> = timeout(IO_TIMEOUT, read_frame(&mut stream))
                .await
                .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "auth timeout")));
            if let Ok(got) = auth {
                if verify_auth_token(&self.token, &got) {
                    return Ok(OwnerStream::Tcp(stream));
                }
            }
        }
    }
}

/// Connect to the profile's running local owner, returning `Ok(None)` when none is running.
pub async fn connect(home: &Path) -> Result<Option<OwnerStream>, String> {
    let directory = home.join(OWNER_DIRECTORY);
    if !check_owner_directory(&directory).map_err(|e| e.to_string())? {
        return Ok(None);
    }

    #[cfg(unix)]
    {
        let path = directory.join(OWNER_SOCKET_NAME);
        if !socket_exists(&path).map_err(|e| e.to_string())? {
            return Ok(None);
        }
        let stream = tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|error| format!("cannot connect to local owner: {error}"))?;
        check_unix_peer(&stream).map_err(|error| error.to_string())?;
        Ok(Some(OwnerStream::Unix(stream)))
    }

    #[cfg(not(unix))]
    {
        let path = directory.join(OWNER_ADDR_NAME);
        if !path.is_file() {
            return Ok(None);
        }
        let raw = fs::read(&path).map_err(|e| format!("cannot read local owner addr: {e}"))?;
        let endpoint: LoopbackEndpointFile = serde_json::from_slice(&raw)
            .map_err(|_| "corrupted local owner address file".to_owned())?;
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", endpoint.port))
            .await
            .map_err(|error| format!("cannot connect to local owner: {error}"))?;
        timeout(IO_TIMEOUT, write_frame(&mut stream, &endpoint.token))
            .await
            .map_err(|_| "local owner auth handshake timed out".to_owned())?
            .map_err(|error| format!("local owner auth failed: {error}"))?;
        Ok(Some(OwnerStream::Tcp(stream)))
    }
}

// ── Length-prefixed JSON framing ─────────────────────────────────────────────────────────────────

pub async fn read_frame<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> io::Result<T> {
    let bytes = read_frame_bytes(reader).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid local owner frame"))
}

pub async fn read_frame_bytes(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local owner frame length",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub async fn write_frame(
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

// ── Protocol v2 (GUI & multi-client reconnectable session RPCs) ──────────────────────────────────

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct V2Envelope {
    pub version: u32,
    pub command: GuiCommand,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    #[default]
    Once,
    Session,
    Always,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuiCommand {
    Ping,
    ListProfiles,
    ListSessions {
        limit: usize,
        query: Option<String>,
    },
    LoadSession {
        session_id: String,
    },
    DeleteSession {
        session_id: String,
    },
    ActiveTurns,
    StartTurn {
        session_id: Option<String>,
        query: String,
        model: Option<String>,
        cwd: Option<String>,
        yolo: bool,
    },
    SubscribeTurn {
        turn_id: String,
        after_seq: u64,
    },
    CancelTurn {
        turn_id: String,
    },
    RespondApproval {
        turn_id: String,
        approval_id: u64,
        decision: ApprovalDecision,
        #[serde(default)]
        scope: ApprovalScope,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProfileEntry {
    pub name: String,
    pub path: String,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub message_count: usize,
    pub started_at: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TranscriptItem {
    pub role: String,
    pub content: String,
    pub tool_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActiveTurnSummary {
    pub turn_id: String,
    pub session_id: Option<String>,
    pub query: String,
    pub latest_seq: u64,
    pub finished: bool,
    pub has_pending_approval: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SequencedTurnEvent {
    pub seq: u64,
    pub event: TurnStreamEvent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnStreamEvent {
    /// The requested cursor predates retained events. Reload the persisted transcript on Done.
    /// Includes the outstanding approval. A terminal event follows for completed turns.
    ReplayGap {
        session_id: Option<String>,
        pending_approval: Option<(u64, ApprovalRequest)>,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    Status {
        text: String,
    },
    SessionAssigned {
        session_id: String,
    },
    ToolStarted {
        name: String,
        detail: Option<String>,
    },
    ToolOutput {
        name: String,
        text: String,
    },
    ToolFinished {
        name: String,
        succeeded: bool,
        summary: Option<String>,
    },
    ApprovalRequired {
        approval_id: u64,
        request: ApprovalRequest,
    },
    ApprovalResolved {
        approval_id: u64,
    },
    Error {
        message: String,
    },
    Done {
        exit_code: u8,
        cancelled: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuiReply {
    Pong {
        profile_home: String,
    },
    Profiles {
        profiles: Vec<ProfileEntry>,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    Transcript {
        session: SessionSummary,
        messages: Vec<TranscriptItem>,
        active_turn_id: Option<String>,
    },
    ActiveTurns {
        turns: Vec<ActiveTurnSummary>,
    },
    TurnStarted {
        turn_id: String,
    },
    TurnEvent(SequencedTurnEvent),
    Ack,
    Error {
        message: String,
    },
}

// ── Managed Turn Hub (replay buffer + reconnect + persistent pending approval) ───────────────────

struct ManagedTurn {
    turn_id: String,
    session_id: Option<String>,
    query: String,
    policy: Option<Arc<ApprovalPolicy>>,
    events: VecDeque<SequencedTurnEvent>,
    next_seq: u64,
    finished: bool,
    pending_approval: Option<(u64, ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
    next_approval_id: u64,
    cancel_tx: Option<oneshot::Sender<()>>,
    tx: broadcast::Sender<SequencedTurnEvent>,
}

impl ManagedTurn {
    fn publish(&mut self, event: TurnStreamEvent) {
        self.next_seq += 1;
        let event = SequencedTurnEvent {
            seq: self.next_seq,
            event,
        };
        if self.events.len() == MAX_REPLAY_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event.clone());
        let _ = self.tx.send(event);
    }
}

#[derive(Default)]
struct TurnRegistry {
    turns: HashMap<String, Arc<Mutex<ManagedTurn>>>,
    session_grants: HashMap<String, SessionGrants>,
    completed: VecDeque<(Instant, String)>,
    next_id: u64,
}

impl TurnRegistry {
    fn prune(&mut self, now: Instant) {
        while let Some((finished_at, _)) = self.completed.front() {
            if self.completed.len() <= MAX_COMPLETED_TURNS
                && now.duration_since(*finished_at) < COMPLETED_TURN_TTL
            {
                break;
            }
            let (_, id) = self.completed.pop_front().unwrap();
            self.turns.remove(&id);
        }
    }
}

/// Multi-client turn hub held by the local owner process so GUI clients can disconnect and
/// reconnect without losing streaming output, active turns, or pending approval prompts.
#[derive(Clone, Default)]
pub struct TurnHub {
    registry: Arc<Mutex<TurnRegistry>>,
}

pub struct TurnHandle {
    emitter: TurnEmitter,
    cancel_rx: oneshot::Receiver<()>,
}

#[derive(Clone)]
pub struct TurnEmitter {
    hub: TurnHub,
    turn: Arc<Mutex<ManagedTurn>>,
}

impl TurnHandle {
    pub fn turn_id(&self) -> String {
        self.emitter.turn_id()
    }

    pub fn split(self) -> (TurnEmitter, oneshot::Receiver<()>) {
        (self.emitter, self.cancel_rx)
    }

    pub fn emit(&self, event: TurnStreamEvent) {
        self.emitter.emit(event);
    }

    pub fn request_approval(
        &self,
        request: ApprovalRequest,
    ) -> oneshot::Receiver<ApprovalDecision> {
        self.emitter.request_approval(request)
    }
}

impl TurnEmitter {
    pub fn turn_id(&self) -> String {
        self.turn.lock().unwrap().turn_id.clone()
    }

    pub fn emit(&self, event: TurnStreamEvent) {
        let mut guard = self.turn.lock().unwrap();
        if guard.finished {
            return;
        }
        if let TurnStreamEvent::SessionAssigned { ref session_id } = event {
            guard.session_id = Some(session_id.clone());
        }
        let finished = matches!(event, TurnStreamEvent::Done { .. });
        if finished {
            guard.finished = true;
            guard.cancel_tx = None;
            guard.pending_approval = None;
        }
        guard.publish(event);
        if finished {
            let turn_id = guard.turn_id.clone();
            let sid_and_grants = guard.session_id.clone().and_then(|sid| {
                let grants = guard.policy.as_ref()?.snapshot_session_grants();
                (!grants.is_empty()).then_some((sid, grants))
            });
            drop(guard);
            let mut registry = self.hub.registry.lock().unwrap();
            if let Some((sid, grants)) = sid_and_grants {
                registry
                    .session_grants
                    .entry(sid)
                    .or_default()
                    .merge_from(&grants);
            }
            let now = Instant::now();
            registry.completed.push_back((now, turn_id));
            registry.prune(now);
        }
    }

    pub fn request_approval(
        &self,
        request: ApprovalRequest,
    ) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.turn.lock().unwrap();
        if guard.finished {
            return rx;
        }
        guard.next_approval_id += 1;
        let approval_id = guard.next_approval_id;
        guard.pending_approval = Some((approval_id, request.clone(), tx));
        guard.publish(TurnStreamEvent::ApprovalRequired {
            approval_id,
            request,
        });
        rx
    }
}

impl TurnHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_turn(&self, session_id: Option<String>, query: String) -> TurnHandle {
        self.create_turn_with_policy(session_id, query, None)
    }

    pub fn create_turn_with_policy(
        &self,
        session_id: Option<String>,
        query: String,
        policy: Option<Arc<ApprovalPolicy>>,
    ) -> TurnHandle {
        let mut registry = self.registry();
        registry.next_id += 1;
        // Each new owner has a different map hash seed, so stale subscriptions cannot
        // attach to an unrelated turn after an owner restart.
        let turn_id = format!(
            "turn-{:016x}-{}",
            registry.turns.hasher().hash_one(0u64),
            registry.next_id
        );
        let (tx, _) = broadcast::channel(256);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let turn = Arc::new(Mutex::new(ManagedTurn {
            turn_id: turn_id.clone(),
            session_id,
            query,
            policy,
            events: VecDeque::new(),
            next_seq: 0,
            finished: false,
            pending_approval: None,
            next_approval_id: 0,
            cancel_tx: Some(cancel_tx),
            tx,
        }));
        registry.turns.insert(turn_id, Arc::clone(&turn));
        TurnHandle {
            emitter: TurnEmitter {
                turn,
                hub: self.clone(),
            },
            cancel_rx,
        }
    }

    pub fn session_grants(&self, session_id: &str) -> Option<SessionGrants> {
        self.registry().session_grants.get(session_id).cloned()
    }

    pub fn forget_session_grants(&self, session_id: &str) {
        self.registry().session_grants.remove(session_id);
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, TurnRegistry> {
        let mut registry = self.registry.lock().unwrap();
        registry.prune(Instant::now());
        registry
    }

    pub fn active_turns(&self) -> Vec<ActiveTurnSummary> {
        let registry = self.registry();
        let guard = &registry.turns;
        let mut out: Vec<ActiveTurnSummary> = guard
            .values()
            .map(|t| {
                let st = t.lock().unwrap();
                ActiveTurnSummary {
                    turn_id: st.turn_id.clone(),
                    session_id: st.session_id.clone(),
                    query: st.query.clone(),
                    latest_seq: st.next_seq,
                    finished: st.finished,
                    has_pending_approval: st.pending_approval.is_some(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.turn_id.cmp(&b.turn_id));
        out
    }

    pub fn active_turn_for_session(&self, session_id: &str) -> Option<String> {
        let registry = self.registry();
        let guard = &registry.turns;
        guard.values().find_map(|t| {
            let st = t.lock().unwrap();
            (!st.finished && st.session_id.as_deref() == Some(session_id))
                .then(|| st.turn_id.clone())
        })
    }

    /// Return `(missed_events, live_receiver, already_finished)` for reconnecting subscribers.
    pub fn subscribe(
        &self,
        turn_id: &str,
        after_seq: u64,
    ) -> Result<
        (
            Vec<SequencedTurnEvent>,
            broadcast::Receiver<SequencedTurnEvent>,
            bool,
        ),
        String,
    > {
        let turn = self
            .registry()
            .turns
            .get(turn_id)
            .cloned()
            .ok_or_else(|| format!("Unknown turn_id '{turn_id}'"))?;
        let st = turn.lock().unwrap();
        if after_seq > st.next_seq {
            return Err(format!("Invalid cursor {after_seq} for turn '{turn_id}'"));
        }
        let missed = if st
            .events
            .front()
            .is_some_and(|first| first.seq.saturating_sub(1) > after_seq)
        {
            let mut events = vec![SequencedTurnEvent {
                seq: st.next_seq - u64::from(st.finished),
                event: TurnStreamEvent::ReplayGap {
                    session_id: st.session_id.clone(),
                    pending_approval: st
                        .pending_approval
                        .as_ref()
                        .map(|(id, req, _)| (*id, req.clone())),
                },
            }];
            if st.finished {
                events.push(
                    st.events
                        .back()
                        .expect("completed turn has a Done event")
                        .clone(),
                );
            }
            events
        } else {
            st.events
                .iter()
                .filter(|ev| ev.seq > after_seq)
                .cloned()
                .collect()
        };
        let rx = st.tx.subscribe();
        let finished = st.finished;
        Ok((missed, rx, finished))
    }

    pub fn cancel(&self, turn_id: &str) -> Result<(), String> {
        let turn = self
            .registry()
            .turns
            .get(turn_id)
            .cloned()
            .ok_or_else(|| format!("Unknown turn_id '{turn_id}'"))?;
        let mut st = turn.lock().unwrap();
        if let Some(tx) = st.cancel_tx.take() {
            let _ = tx.send(());
        }
        Ok(())
    }

    pub fn respond_approval(
        &self,
        turn_id: &str,
        approval_id: u64,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    ) -> Result<Option<String>, String> {
        let turn = self
            .registry()
            .turns
            .get(turn_id)
            .cloned()
            .ok_or_else(|| format!("Unknown turn_id '{turn_id}'"))?;
        let (request, sender, policy, session_id) = {
            let mut st = turn.lock().unwrap();
            let matched = st
                .pending_approval
                .as_ref()
                .is_some_and(|(id, _, _)| *id == approval_id);
            if !matched {
                return Err(format!(
                    "No pending approval #{approval_id} for turn '{turn_id}'"
                ));
            }
            let (_, request, sender) = st.pending_approval.take().expect("matched approval");
            st.publish(TurnStreamEvent::ApprovalResolved { approval_id });
            (request, sender, st.policy.clone(), st.session_id.clone())
        };
        let mut persist_warning = None;
        if matches!(decision, ApprovalDecision::Approved) {
            if let Some(policy) = policy {
                match scope {
                    ApprovalScope::Once => {}
                    ApprovalScope::Session => {
                        policy.remember_session(&request);
                    }
                    ApprovalScope::Always => {
                        let (_, err) = policy.remember_always(&request);
                        persist_warning = err;
                    }
                }
                if !matches!(scope, ApprovalScope::Once) {
                    if let Some(sid) = session_id {
                        let snapshot = policy.snapshot_session_grants();
                        self.registry()
                            .session_grants
                            .entry(sid)
                            .or_default()
                            .merge_from(&snapshot);
                    }
                }
            }
        }
        let _ = sender.send(decision);
        Ok(persist_warning)
    }
}

// ── Profile discovery & session store queries for GUI / owner v2 ─────────────────────────────────

pub fn discover_profiles(active_home: &Path) -> Vec<ProfileEntry> {
    let mut entries: Vec<ProfileEntry> = Vec::new();
    let active_canon = fs::canonicalize(active_home).unwrap_or_else(|_| active_home.to_path_buf());
    let mut seen: Vec<PathBuf> = Vec::new();

    let mut push_path = |name: String, path: PathBuf| {
        let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.contains(&canon) {
            return;
        }
        seen.push(canon.clone());
        entries.push(ProfileEntry {
            name,
            path: path.display().to_string(),
            active: canon == active_canon,
        });
    };

    push_path(
        active_home
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_owned(),
        active_home.to_path_buf(),
    );

    if let Some(user_home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    {
        for base_name in [".hsr", ".hermes"] {
            let base = user_home.join(base_name);
            if base.is_dir() {
                push_path(base_name.to_owned(), base.clone());
            }
            let profiles_dir = base.join("profiles");
            if let Ok(read_dir) = fs::read_dir(&profiles_dir) {
                let mut children: Vec<PathBuf> = read_dir
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                children.sort();
                for child in children {
                    if let Some(name) = child.file_name().and_then(|n| n.to_str()) {
                        push_path(format!("{base_name}/{name}"), child);
                    }
                }
            }
        }
    }
    entries
}

fn storage_summary_to_wire(rec: StorageSessionSummary) -> SessionSummary {
    SessionSummary {
        id: rec.id,
        title: rec.title.or(rec.preview),
        model: rec.model,
        message_count: rec.message_count.max(0) as usize,
        started_at: rec.started_at,
    }
}

fn session_to_summary(rec: Session, message_count: usize) -> SessionSummary {
    SessionSummary {
        id: rec.id,
        title: rec.title,
        model: rec.model,
        message_count,
        started_at: rec.started_at,
    }
}

pub fn list_profile_sessions(
    home: &Path,
    limit: usize,
    query: Option<&str>,
) -> Result<Vec<SessionSummary>, String> {
    let store = match SessionStore::open_read_only(home) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let capped = limit.clamp(1, 200) as u32;
    let q = query.map(str::trim).filter(|s| !s.is_empty());
    let records = if let Some(search) = q {
        let hit_ids: Vec<String> = store
            .search(search, capped)
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.session_id)
            .collect();
        let all = store.recent(200, None).map_err(|e| e.to_string())?;
        let lower = search.to_ascii_lowercase();
        all.into_iter()
            .filter(|s| {
                hit_ids.contains(&s.id)
                    || s.id.to_ascii_lowercase().contains(&lower)
                    || s.title
                        .as_deref()
                        .is_some_and(|t| t.to_ascii_lowercase().contains(&lower))
                    || s.preview
                        .as_deref()
                        .is_some_and(|p| p.to_ascii_lowercase().contains(&lower))
            })
            .take(capped as usize)
            .collect()
    } else {
        store.recent(capped, None).map_err(|e| e.to_string())?
    };
    Ok(records.into_iter().map(storage_summary_to_wire).collect())
}

pub fn load_profile_transcript(
    home: &Path,
    session_id: &str,
) -> Result<(SessionSummary, Vec<TranscriptItem>), String> {
    let store = SessionStore::open_read_only(home).map_err(|e| e.to_string())?;
    let transcript = store
        .resume_display_transcript(session_id)
        .map_err(|e| e.to_string())?;
    let msg_count = transcript.messages.len();
    let summary = session_to_summary(transcript.session, msg_count);
    let items = transcript
        .messages
        .into_iter()
        .filter(|m| m.role == "user" || m.role == "assistant" || m.role == "tool")
        .map(|m| {
            let text = match &m.content {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            TranscriptItem {
                role: m.role,
                content: text,
                tool_name: m.tool_name,
            }
        })
        .collect();
    Ok((summary, items))
}

/// Convenience v2 single request-response client call (for non-streaming RPCs).
pub async fn send_v2_command(home: &Path, command: GuiCommand) -> Result<GuiReply, String> {
    let Some(mut stream) = connect(home).await? else {
        return Err("Local session owner is not running for this profile.".into());
    };
    let env = V2Envelope {
        version: PROTOCOL_V2,
        command,
    };
    timeout(IO_TIMEOUT, write_frame(&mut stream, &env))
        .await
        .map_err(|_| "local owner request timed out".to_owned())?
        .map_err(|e| format!("local owner write failed: {e}"))?;
    timeout(IO_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| "local owner reply timed out".to_owned())?
        .map_err(|e| format!("local owner read failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_tools::ApprovalAction;

    #[test]
    fn completed_history_is_bounded_and_keeps_running_turns() {
        let hub = TurnHub::new();
        let active = hub.create_turn(None, "active".into());
        let first = hub.create_turn(None, "first".into());
        let finish = |h: &TurnHandle| {
            h.emit(TurnStreamEvent::Done {
                exit_code: 0,
                cancelled: false,
            })
        };
        finish(&first);
        for _ in 0..MAX_COMPLETED_TURNS {
            finish(&hub.create_turn(None, String::new()));
        }
        assert!(hub.subscribe(&first.turn_id(), 0).is_err());
        assert!(hub.subscribe(&active.turn_id(), 0).is_ok());
    }

    #[test]
    fn completed_turn_expires_at_the_reconnect_deadline() {
        let hub = TurnHub::new();
        let turn = hub.create_turn(None, String::new());
        turn.emit(TurnStreamEvent::Done {
            exit_code: 0,
            cancelled: false,
        });
        assert!(hub.subscribe(&turn.turn_id(), 0).is_ok());
        hub.registry
            .lock()
            .unwrap()
            .completed
            .front_mut()
            .unwrap()
            .0 -= COMPLETED_TURN_TTL;
        assert!(hub.subscribe(&turn.turn_id(), 0).is_err());
    }

    #[test]
    fn truncated_replay_returns_session_and_pending_approval_then_terminal_event() {
        let hub = TurnHub::new();
        let turn = hub.create_turn(Some("saved".into()), String::new());
        let _decision = turn.request_approval(ApprovalRequest {
            target: "file".into(),
            content: "diff".into(),
            action: ApprovalAction::ReplaceFile,
            timeout_secs: 30,
        });
        for _ in 0..MAX_REPLAY_EVENTS {
            turn.emit(TurnStreamEvent::TextDelta { text: "x".into() });
        }
        let (events, _, finished) = hub.subscribe(&turn.turn_id(), 0).unwrap();
        assert!(!finished);
        assert!(
            matches!(events.as_slice(), [SequencedTurnEvent { event: TurnStreamEvent::ReplayGap {
            session_id: Some(id), pending_approval: Some((1, _)) }, .. }] if id == "saved")
        );
        let checkpoint = events[0].seq;
        turn.emit(TurnStreamEvent::Done {
            exit_code: 0,
            cancelled: false,
        });
        let (events, _, finished) = hub.subscribe(&turn.turn_id(), 0).unwrap();
        assert!(finished);
        assert!(matches!(
            events[0].event,
            TurnStreamEvent::ReplayGap {
                pending_approval: None,
                ..
            }
        ));
        assert_eq!(events[0].seq, checkpoint);
        assert!(matches!(events[1].event, TurnStreamEvent::Done { .. }));
        assert_eq!(
            hub.subscribe(&turn.turn_id(), checkpoint).unwrap().0.len(),
            1
        );
    }

    #[tokio::test]
    async fn turn_hub_replays_events_and_resolves_approval_after_reconnect() {
        let hub = TurnHub::new();
        let handle = hub.create_turn(Some("sess-1".into()), "fix bug".into());
        let turn_id = handle.turn_id();

        handle.emit(TurnStreamEvent::TextDelta {
            text: "Checking ".into(),
        });
        let approval_rx = handle.request_approval(ApprovalRequest {
            target: "main.rs".into(),
            content: "diff".into(),
            action: ApprovalAction::ReplaceFile,
            timeout_secs: 30,
        });

        // Client 1 disconnects; Client 2 reconnects and inspects active turns + replays from seq 0
        let active = hub.active_turns();
        assert_eq!(active.len(), 1);
        assert!(active[0].has_pending_approval);
        assert_eq!(
            hub.active_turn_for_session("sess-1").as_deref(),
            Some(turn_id.as_str())
        );

        let (missed, mut live_rx, finished) = hub.subscribe(&turn_id, 0).unwrap();
        assert!(!finished);
        assert_eq!(missed.len(), 2);
        let approval_id = match &missed[1].event {
            TurnStreamEvent::ApprovalRequired { approval_id, .. } => *approval_id,
            other => panic!("expected ApprovalRequired, got {other:?}"),
        };

        // Reconnected client approves the pending prompt
        hub.respond_approval(
            &turn_id,
            approval_id,
            ApprovalDecision::Approved,
            ApprovalScope::Once,
        )
        .unwrap();
        assert_eq!(approval_rx.await.unwrap(), ApprovalDecision::Approved);

        handle.emit(TurnStreamEvent::Done {
            exit_code: 0,
            cancelled: false,
        });

        let ev1 = live_rx.recv().await.unwrap();
        assert!(matches!(
            ev1.event,
            TurnStreamEvent::ApprovalResolved { .. }
        ));
        let ev2 = live_rx.recv().await.unwrap();
        assert!(matches!(
            ev2.event,
            TurnStreamEvent::Done {
                exit_code: 0,
                cancelled: false
            }
        ));
    }

    #[tokio::test]
    async fn owner_listener_isolates_profiles_and_roundtrips_frames() {
        let p1 = tempfile::tempdir().unwrap();
        let p2 = tempfile::tempdir().unwrap();

        let listener1 = OwnerListener::bind(p1.path()).await.unwrap();
        assert!(connect(p2.path()).await.unwrap().is_none());

        let srv = tokio::spawn(async move {
            let mut stream = listener1.accept().await.unwrap();
            let env: V2Envelope = read_frame(&mut stream).await.unwrap();
            assert_eq!(env.version, PROTOCOL_V2);
            write_frame(
                &mut stream,
                &GuiReply::Pong {
                    profile_home: "p1".into(),
                },
            )
            .await
            .unwrap();
        });

        let reply = send_v2_command(p1.path(), GuiCommand::Ping).await.unwrap();
        assert!(matches!(reply, GuiReply::Pong { profile_home } if profile_home == "p1"));
        srv.await.unwrap();
    }
}
