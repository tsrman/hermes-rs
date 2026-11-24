"""Zero-dependency Python SDK wrapper for the Rust `hermes` CLI.

This module replaces programmatic `AIAgent.run_conversation` calls with an
out-of-process, versioned JSON Lines (`--events-json`, version 1) transport.
It isolates the subprocess environment so only the chosen profile and explicit
caller credentials reach `hermes`, streams typed events in real time, and
supports cooperative cancellation via SIGINT.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import threading
from typing import Callable, Iterator


PROTOCOL_VERSION = 1

# Minimal environment variables forwarded to the child `hermes` process.
# Unrelated caller secrets (OPENAI_API_KEY, another profile's HERMES_HOME, ...)
# are deliberately excluded unless passed explicitly via `api_key` or `extra_env`.
_SAFE_ENV_KEYS = (
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "SYSTEMROOT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
)


class HermesExtensionError(RuntimeError):
    """Base error raised by the Hermes Python SDK wrapper."""


class HermesBinaryNotFoundError(HermesExtensionError):
    """Raised when the configured `hermes` executable cannot be found."""


class HermesProtocolError(HermesExtensionError):
    """Raised when the child process violates the version-1 NDJSON protocol."""


class HermesTurnError(HermesExtensionError):
    """Raised when a turn fails with an error event or non-zero exit."""

    def __init__(self, message: str, exit_code: int = 1, result: TurnResult | None = None):
        super().__init__(message)
        self.exit_code = exit_code
        self.result = result


class HermesCancelledError(HermesExtensionError):
    """Raised when a turn is cancelled before completion."""

    def __init__(self, message: str = "turn cancelled", result: TurnResult | None = None):
        super().__init__(message)
        self.result = result


@dataclass(frozen=True)
class TurnEvent:
    """One versioned event emitted by `hermes chat --events-json`."""

    version: int
    event: str
    data: dict


@dataclass
class TurnResult:
    """Summary of a finished `hermes` turn."""

    text: str = ""
    session_id: str | None = None
    input_tokens: int = 0
    output_tokens: int = 0
    status: str = "error"
    exit_code: int = 1
    error: str | None = None
    warnings: list[str] = field(default_factory=list)
    events: list[TurnEvent] = field(default_factory=list)


class TurnHandle:
    """Handle for an in-flight `hermes` turn, allowing cancellation from another thread."""

    def __init__(self, process: subprocess.Popen[str]):
        self._process = process
        self._lock = threading.Lock()
        self._cancel_requested = False

    @property
    def pid(self) -> int:
        return self._process.pid

    def cancel(self, grace_seconds: float = 5.0) -> None:
        """Request cooperative cancellation (SIGINT on POSIX, terminate on Windows)."""
        with self._lock:
            if self._cancel_requested:
                return
            self._cancel_requested = True
            if self._process.poll() is not None:
                return
            try:
                if os.name == "posix":
                    self._process.send_signal(signal.SIGINT)
                else:
                    self._process.terminate()
            except ProcessLookupError:
                return
        try:
            self._process.wait(timeout=grace_seconds)
        except subprocess.TimeoutExpired:
            try:
                self._process.kill()
            except ProcessLookupError:
                pass


def _resolve_binary(binary: str | Path) -> str:
    candidate = str(binary)
    if os.sep in candidate or (os.altsep and os.altsep in candidate):
        path = Path(candidate)
        if not path.is_file() or not os.access(path, os.X_OK):
            raise HermesBinaryNotFoundError(
                f"hermes binary not found or not executable: {candidate}"
            )
        return str(path)
    resolved = shutil.which(candidate)
    if resolved is None:
        raise HermesBinaryNotFoundError(f"hermes binary not found on PATH: {candidate}")
    return resolved


def _parse_event_line(raw: str) -> TurnEvent:
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise HermesProtocolError(f"invalid JSON event line: {raw!r}") from exc
    if not isinstance(payload, dict):
        raise HermesProtocolError(f"event line must be a JSON object: {raw!r}")
    version = payload.get("version")
    event = payload.get("event")
    if version != PROTOCOL_VERSION or not isinstance(event, str) or not event:
        raise HermesProtocolError(
            f"unsupported event envelope (expected version {PROTOCOL_VERSION}): {payload!r}"
        )
    data = {k: v for k, v in payload.items() if k not in ("version", "event")}
    return TurnEvent(version=version, event=event, data=data)


class HermesClient:
    """Programmatic client wrapping the Rust `hermes` executable."""

    def __init__(
        self,
        *,
        binary: str | Path = "hsr",
        profile_home: str | Path | None = None,
        profile: str | None = None,
        workdir: str | Path | None = None,
        base_url: str | None = None,
        model: str | None = None,
        api_key: str | None = None,
        max_turns: int | None = None,
        yolo: bool = False,
        debug_log: str | Path | None = None,
        extra_env: dict[str, str] | None = None,
    ):
        if profile_home is not None and profile is not None:
            raise ValueError("specify either profile_home or profile, not both")
        self.binary = _resolve_binary(binary)
        self.profile_home = Path(profile_home) if profile_home is not None else None
        self.profile = profile
        self.workdir = Path(workdir) if workdir is not None else None
        self.base_url = base_url
        self.model = model
        self.api_key = api_key
        self.max_turns = max_turns
        self.yolo = yolo
        self.debug_log = Path(debug_log) if debug_log is not None else None
        self.extra_env = dict(extra_env or {})

    def _build_env(self) -> dict[str, str]:
        env: dict[str, str] = {}
        for key in _SAFE_ENV_KEYS:
            value = os.environ.get(key)
            if value is not None:
                env[key] = value
        if self.profile_home is not None:
            env["HERMES_HOME"] = str(self.profile_home)
        if self.api_key is not None:
            env["HERMES_API_KEY"] = self.api_key
        env.update(self.extra_env)
        return env

    def _build_command(
        self,
        query: str,
        *,
        save_session: bool = False,
        resume: str | None = None,
        model: str | None = None,
        max_turns: int | None = None,
    ) -> list[str]:
        if save_session and resume:
            raise ValueError("save_session and resume cannot both be set")
        command = [self.binary]
        if self.profile is not None:
            command.extend(["--profile", self.profile])
        command.extend(["chat", "--events-json", "-q", query])
        if save_session:
            command.append("--save-session")
        if resume:
            command.extend(["--resume", resume])
        chosen_model = model if model is not None else self.model
        if chosen_model:
            command.extend(["--model", chosen_model])
        if self.base_url:
            command.extend(["--base-url", self.base_url])
        chosen_max_turns = max_turns if max_turns is not None else self.max_turns
        if chosen_max_turns is not None:
            command.extend(["--max-turns", str(chosen_max_turns)])
        if self.yolo:
            command.append("--yolo")
        if self.debug_log is not None:
            command.extend(["--debug-log", str(self.debug_log)])
        return command

    def start_turn(
        self,
        query: str,
        *,
        save_session: bool = False,
        resume: str | None = None,
        model: str | None = None,
        max_turns: int | None = None,
    ) -> tuple[TurnHandle, Iterator[TurnEvent]]:
        """Spawn a turn and return `(handle, event_iterator)`."""
        command = self._build_command(
            query,
            save_session=save_session,
            resume=resume,
            model=model,
            max_turns=max_turns,
        )
        try:
            process = subprocess.Popen(
                command,
                cwd=str(self.workdir) if self.workdir is not None else None,
                env=self._build_env(),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                bufsize=1,
            )
        except FileNotFoundError as exc:
            raise HermesBinaryNotFoundError(
                f"hermes binary could not be executed: {self.binary}"
            ) from exc

        handle = TurnHandle(process)

        def _iter_events() -> Iterator[TurnEvent]:
            assert process.stdout is not None
            try:
                for raw in process.stdout:
                    line = raw.strip()
                    if not line:
                        continue
                    yield _parse_event_line(line)
            finally:
                if process.poll() is None:
                    handle.cancel()
                process.stdout.close()
                if process.stderr is not None:
                    process.stderr.close()
                process.wait()

        return handle, _iter_events()

    def run_turn(
        self,
        query: str,
        *,
        save_session: bool = False,
        resume: str | None = None,
        model: str | None = None,
        max_turns: int | None = None,
        on_event: Callable[[TurnEvent], None] | None = None,
        on_handle: Callable[[TurnHandle], None] | None = None,
        cancel_event: threading.Event | None = None,
        raise_on_error: bool = True,
    ) -> TurnResult:
        """Run one turn to completion, optionally streaming events to `on_event`."""
        handle, stream = self.start_turn(
            query,
            save_session=save_session,
            resume=resume,
            model=model,
            max_turns=max_turns,
        )
        if on_handle is not None:
            on_handle(handle)

        stop_watcher = threading.Event()
        watcher: threading.Thread | None = None
        if cancel_event is not None:
            def _watch() -> None:
                while not stop_watcher.is_set():
                    if cancel_event.wait(0.02):
                        handle.cancel()
                        return

            watcher = threading.Thread(target=_watch, daemon=True)
            watcher.start()

        result = TurnResult()
        chunks: list[str] = []
        saw_done = False
        try:
            for event in stream:
                result.events.append(event)
                if event.event == "text":
                    chunks.append(str(event.data.get("text", "")))
                elif event.event == "session":
                    result.session_id = str(event.data.get("id", "")) or None
                elif event.event == "usage":
                    result.input_tokens = int(event.data.get("input_tokens", 0))
                    result.output_tokens = int(event.data.get("output_tokens", 0))
                elif event.event == "warning":
                    result.warnings.append(str(event.data.get("message", "")))
                elif event.event == "error":
                    result.error = str(event.data.get("message", "turn failed"))
                elif event.event == "done":
                    saw_done = True
                    result.exit_code = int(event.data.get("exit_code", 1))
                    result.status = str(event.data.get("status", "error"))
                if on_event is not None:
                    on_event(event)
        finally:
            stop_watcher.set()
            if watcher is not None:
                watcher.join(timeout=1.0)

        result.text = "".join(chunks)
        if not saw_done:
            result.exit_code = handle._process.returncode if handle._process.returncode is not None else 1
            if result.exit_code == 130:
                result.status = "cancelled"
            elif result.exit_code == 0:
                raise HermesProtocolError("hermes exited without a terminal 'done' event")
            else:
                result.status = "error"

        if raise_on_error:
            if result.status == "cancelled":
                raise HermesCancelledError("turn cancelled", result=result)
            if result.status == "error":
                raise HermesTurnError(
                    result.error or f"turn failed with exit code {result.exit_code}",
                    exit_code=result.exit_code,
                    result=result,
                )
        return result
