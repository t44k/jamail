//! IPC protocol between the `jamaild` daemon and the `jamail` foreground
//! client, plus the client-side connection manager.
//!
//! ## Transport
//!
//! A Unix domain stream socket, same machine only. The path is resolved by
//! [`resolve_socket_path`] in this order:
//!
//! 1. `$JAMAIL_SOCKET` environment variable, if set (absolute override, used
//!    by tests and anyone running multiple isolated instances).
//! 2. `accounts.daemon.socket_path` in `config.yaml`, if set.
//! 3. `$XDG_RUNTIME_DIR/jamail/jamaild.sock`.
//! 4. `/tmp/jamail-<uid>/jamaild.sock` if `XDG_RUNTIME_DIR` isn't set.
//!
//! The containing directory is created with `0700` permissions and the
//! socket file itself is `chmod`'d to `0600` right after `bind()` — only the
//! owning user can connect, matching the trust model of comparable
//! local-agent sockets (`ssh-agent`, `gpg-agent`).
//!
//! ## Framing
//!
//! Every message, in both directions, is a single frame:
//!
//! ```text
//! [4 bytes: u32 little-endian length N] [N bytes: JSON payload]
//! ```
//!
//! `N` must not exceed [`MAX_FRAME_BYTES`]; a larger value is treated as a
//! protocol error and the connection is closed (a defensive cap, not a
//! realistic limit — the largest payload is an `EnqueueUpload` request
//! carrying a base64'd raw outgoing message with attachments).
//!
//! JSON (rather than a binary format) was chosen deliberately: message
//! volume is tiny (a handful of control messages plus one event per
//! sync/notification tick), and human-readable frames make this protocol
//! trivial to inspect with `socat`/`nc` while debugging.
//!
//! ## Handshake and versioning
//!
//! Immediately after connecting, the client sends one [`ClientHello`] frame.
//! The daemon replies with exactly one [`ServerHello`] frame:
//!
//! - [`ServerHello::Ok`] — versions are compatible ([`is_compatible`]);
//!   the connection proceeds to normal request/event traffic.
//! - [`ServerHello::VersionMismatch`] — the daemon's [`PROTOCOL_VERSION`]
//!   is incompatible with the client's; the daemon closes the connection
//!   right after sending this frame. The client must not retry the same
//!   daemon binary — this is a fatal, non-transient error surfaced to the
//!   user (see [`ClientEvent::Fatal`]).
//!
//! [`is_compatible`] currently requires an exact match. The version is
//! bumped whenever a wire-incompatible change is made to [`Request`],
//! [`Response`], or [`Event`] (adding a new enum variant is *not*
//! wire-incompatible as long as both sides tolerate unknown variants
//! gracefully — today they don't need to, since both binaries are built
//! from the same source tree and shipped together, so any change to these
//! enums should bump the version out of caution).
//!
//! ## Steady state
//!
//! After the handshake, the client may send any number of [`Request`]
//! frames; the daemon answers each with exactly one [`Response`] frame, in
//! the order requests were sent on that connection (request/response pairs
//! are *not* tagged with a correlation id — every request this protocol
//! defines is either idempotent or fire-and-forget from the client's point
//! of view, and the one flow that needs a durable result — a Sent/Draft
//! upload outcome — is delivered later as an [`Event::UploadComplete`] /
//! [`Event::UploadError`], correlated by `local_id`, not as the immediate
//! `Response`). Concurrently, and in any interleaving, the daemon may push
//! any number of [`Event`] frames — one account's sync progress, another
//! account's completed folder, a third account's error — without being
//! asked. Both frame kinds share the same [`ServerMessage`] envelope so the
//! client can demultiplex them off one read loop.
//!
//! ## Error semantics
//!
//! - A [`Response::Error`] means the request was understood but could not
//!   be carried out (e.g. `account` names a account not present in the
//!   daemon's config) — the connection stays open.
//! - A framing violation (oversized length prefix, truncated frame, invalid
//!   JSON) is unrecoverable for that connection: the offending side logs it
//!   and closes the socket. The client's reconnect loop then takes over.
//! - Daemon-unavailable (socket missing, `ECONNREFUSED`) is not a protocol
//!   error — it is the client's cue to retry with backoff (see
//!   [`IpcClient`]) and, on first start, to consider auto-launching the
//!   daemon (see `daemon::try_autostart`).
//!
//! ## Lifecycle
//!
//! The daemon accepts any number of concurrent client connections (in
//! practice: zero or one `jamail` TUI at a time, but nothing prevents more).
//! [`Request::Shutdown`] asks the daemon to exit gracefully: it stops
//! accepting new connections, tells every account's sync loop to stop, and
//! exits after unlinking its socket file. The daemon also installs
//! `SIGTERM`/`SIGINT` handlers that trigger the same graceful path — this is
//! what `systemd --user stop` / `Ctrl+C` on a foreground `jamaild` use, and
//! is the primary shutdown path in practice; `Request::Shutdown` exists for
//! completeness, tests, and manual scripting.
//!
//! A `jamaild` that terminated uncleanly (e.g. `SIGKILL`) can leave a stale
//! socket file behind. The next `jamaild` start probes any pre-existing
//! socket file by attempting to connect to it: connection refused means
//! stale (remove and rebind), connection accepted means another daemon is
//! genuinely running (refuse to start — see `daemon::bind_or_recover`).
//!
//! ## Compatibility
//!
//! Both binaries are built from the same crate and are expected to be
//! installed/upgraded together (`cargo install --path .` installs both).
//! Nothing prevents running a stale `jamail` against a freshly-upgraded
//! `jamaild` (or vice versa) for a while, which is exactly what the version
//! handshake guards against: a mismatched pair fails fast with a clear
//! error instead of misbehaving on a misparsed frame.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Bumped whenever [`Request`]/[`Response`]/[`Event`]/[`ClientHello`]/
/// [`ServerHello`] change in a wire-incompatible way.
pub const PROTOCOL_VERSION: u32 = 1;

/// Defensive cap on a single frame's payload size. Well above any real
/// message (the largest is a base64'd outgoing email with attachments).
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Name of the child socket file inside the resolved runtime directory.
const SOCKET_FILE_NAME: &str = "jamaild.sock";

/// Whether a client speaking `client_version` and a server speaking
/// `server_version` can talk to each other. Currently exact-match; kept as
/// its own function so the compatibility policy has one place to change
/// (e.g. to a range) without touching handshake plumbing.
pub fn is_compatible(client_version: u32, server_version: u32) -> bool {
    client_version == server_version
}

// ---------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------

/// Write one length-prefixed frame.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame of {} bytes exceeds MAX_FRAME_BYTES ({})",
                payload.len(),
                MAX_FRAME_BYTES
            ),
        ));
    }
    let len = payload.len() as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one length-prefixed frame. Returns `Err(UnexpectedEof)` if the peer
/// closed the connection cleanly between frames (the length prefix itself
/// couldn't be read), and `Err(InvalidData)` if the advertised length
/// exceeds [`MAX_FRAME_BYTES`] (a protocol violation, not EOF).
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "peer advertised frame of {} bytes, exceeds MAX_FRAME_BYTES ({})",
                len, MAX_FRAME_BYTES
            ),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Serialize `msg` as JSON and write it as one frame.
pub fn write_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    write_frame(w, &bytes)
}

/// Read one frame and deserialize it as JSON.
pub fn read_message<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let bytes = read_frame(r)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

// ---------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u32,
    /// Free-form identifier of the connecting client, for daemon-side logs
    /// (e.g. `"jamail-tui"`). Not used for any protocol decision.
    pub client_name: String,
    pub pid: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerHello {
    Ok {
        protocol_version: u32,
        server_pid: u32,
    },
    VersionMismatch {
        server_protocol_version: u32,
    },
}

// ---------------------------------------------------------------------
// Steady-state messages
// ---------------------------------------------------------------------

/// A client-to-daemon request. See module docs for error semantics.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Health check; always answered with [`Response::Pong`]. Used by
    /// [`ping`] and by the client's autostart-probe before spawning a new
    /// `jamaild`.
    Ping,
    /// Tell the daemon which folder the user is currently looking at for
    /// `account`, so its IMAP IDLE prioritizes that folder. Every other
    /// configured account keeps syncing/idling in the background
    /// regardless — `jamaild` syncs all configured accounts continuously
    /// from the moment it starts, independent of any connected client, so
    /// notifications keep working with zero clients attached.
    SetCurrentFolder { account: String, folder: String },
    /// Queue a `\Seen` flag to be applied on the server next sync cycle.
    MarkSeen {
        account: String,
        folder: String,
        uid: u32,
    },
    /// Queue an APPEND of a raw outgoing message (a sent copy or a saved
    /// draft) to `folder`. `raw_message_b64` is standard base64 (RFC 4648)
    /// of the raw RFC 2822 bytes. The result arrives later as
    /// [`Event::UploadComplete`]/[`Event::UploadError`], correlated by
    /// `local_id`.
    EnqueueUpload {
        account: String,
        kind: crate::sync::UploadKind,
        local_id: i64,
        folder: String,
        raw_message_b64: String,
    },
    /// Force the named account's sync loop to drop and re-establish its
    /// IMAP connection on its next opportunity (used after the client
    /// detects its own process was suspended/resumed, e.g. laptop sleep —
    /// each account's loop also detects this independently via its own IDLE
    /// timing, so this is a latency optimization, not the only path).
    ForceReconnect { account: String },
    /// Ask the daemon to shut down gracefully (see module docs). Answered
    /// with [`Response::Ok`] before the daemon begins tearing down.
    Shutdown,
}

/// A daemon-to-client response to exactly one [`Request`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong,
    Error { message: String },
}

/// A daemon-to-client push notification about background sync/upload state,
/// tagged with the account it concerns (a single connection receives events
/// for *every* account the daemon manages, not just whichever one the
/// client last called [`Request::SetCurrentFolder`] for).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Syncing {
        account: String,
        folder: String,
    },
    Progress {
        account: String,
        folder: String,
        done: usize,
        total: usize,
    },
    FolderComplete {
        account: String,
        folder: String,
        count: usize,
    },
    AllComplete {
        account: String,
    },
    Error {
        account: String,
        message: String,
    },
    FoldersLoaded {
        account: String,
        folders: Vec<crate::mail::FolderInfo>,
    },
    FlagsChanged {
        account: String,
        folder: String,
    },
    UploadComplete {
        account: String,
        kind: crate::sync::UploadKind,
        local_id: i64,
    },
    UploadError {
        account: String,
        kind: crate::sync::UploadKind,
        local_id: i64,
        message: String,
    },
}

/// Envelope for every daemon-to-client frame after the handshake, so a
/// single read loop can demultiplex solicited responses from unsolicited
/// events.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    Response(Response),
    Event(Event),
}

/// Tag a daemon-internal [`crate::sync::SyncEvent`] with the account it
/// belongs to, producing the wire [`Event`]. Used by the daemon's
/// per-account event pump (`daemon::dispatch_sync_event`).
pub fn wire_event(account: &str, ev: &crate::sync::SyncEvent) -> Event {
    use crate::sync::SyncEvent;
    let account = account.to_string();
    match ev {
        SyncEvent::Syncing(folder) => Event::Syncing {
            account,
            folder: folder.clone(),
        },
        SyncEvent::Progress(folder, done, total) => Event::Progress {
            account,
            folder: folder.clone(),
            done: *done,
            total: *total,
        },
        SyncEvent::FolderComplete(folder, count) => Event::FolderComplete {
            account,
            folder: folder.clone(),
            count: *count,
        },
        SyncEvent::AllComplete => Event::AllComplete { account },
        SyncEvent::Error(message) => Event::Error {
            account,
            message: message.clone(),
        },
        SyncEvent::FoldersLoaded(folders) => Event::FoldersLoaded {
            account,
            folders: folders.clone(),
        },
        SyncEvent::FlagsChanged(folder) => Event::FlagsChanged {
            account,
            folder: folder.clone(),
        },
        SyncEvent::UploadComplete(kind, local_id) => Event::UploadComplete {
            account,
            kind: *kind,
            local_id: *local_id,
        },
        SyncEvent::UploadError(kind, local_id, message) => Event::UploadError {
            account,
            kind: *kind,
            local_id: *local_id,
            message: message.clone(),
        },
    }
}

// ---------------------------------------------------------------------
// Socket path resolution
// ---------------------------------------------------------------------

/// Pure resolution logic, independent of actually reading process
/// environment/config, so it's fully unit-testable. See [`resolve_socket_path`]
/// for the real entry point.
pub fn resolve_socket_path_from(
    env_override: Option<&str>,
    config_socket_path: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    uid: u32,
) -> PathBuf {
    if let Some(p) = env_override.filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(p) = config_socket_path.filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    match xdg_runtime_dir.filter(|s| !s.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("jamail").join(SOCKET_FILE_NAME),
        None => PathBuf::from(format!("/tmp/jamail-{}", uid)).join(SOCKET_FILE_NAME),
    }
}

/// Resolve the real socket path for this process: `$JAMAIL_SOCKET` env var,
/// else `config.daemon.socket_path`, else `$XDG_RUNTIME_DIR/jamail/jamaild.sock`,
/// else `/tmp/jamail-<uid>/jamaild.sock`.
pub fn resolve_socket_path(config_socket_path: Option<&str>) -> PathBuf {
    let env_override = std::env::var("JAMAIL_SOCKET").ok();
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let uid = unsafe { libc::getuid() };
    resolve_socket_path_from(
        env_override.as_deref(),
        config_socket_path,
        xdg.as_deref(),
        uid,
    )
}

/// Ensure the socket's parent directory exists with `0700` permissions
/// (created if missing; tightened if it already existed looser).
pub fn ensure_socket_dir(socket_path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = socket_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

// ---------------------------------------------------------------------
// One-shot helpers
// ---------------------------------------------------------------------

/// Connect, handshake, send `Request::Ping`, and confirm `Response::Pong`
/// within `timeout`. Used to cheaply check daemon liveness (e.g. before
/// deciding whether to auto-spawn `jamaild`) without standing up a full
/// [`IpcClient`].
pub fn ping(socket_path: &Path, timeout: Duration) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    handshake(&mut stream)?;
    write_message(&mut stream, &Request::Ping)?;
    match read_message::<_, ServerMessage>(&mut stream)? {
        ServerMessage::Response(Response::Pong) => Ok(()),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected Pong, got {:?}", other),
        )),
    }
}

/// Perform the client side of the handshake on an already-connected stream.
/// Returns `Ok(())` on a compatible [`ServerHello::Ok`], or an error
/// (`ErrorKind::Unsupported` for a version mismatch — checked by callers
/// that need to distinguish fatal-vs-retryable) otherwise.
pub fn handshake<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    let hello = ClientHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "jamail".to_string(),
        pid: std::process::id(),
    };
    write_message(stream, &hello)?;
    match read_message::<_, ServerHello>(stream)? {
        ServerHello::Ok {
            protocol_version, ..
        } => {
            if is_compatible(PROTOCOL_VERSION, protocol_version) {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "protocol version mismatch: client {} vs server {}",
                        PROTOCOL_VERSION, protocol_version
                    ),
                ))
            }
        }
        ServerHello::VersionMismatch {
            server_protocol_version,
        } => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "protocol version mismatch: client {} vs server {}",
                PROTOCOL_VERSION, server_protocol_version
            ),
        )),
    }
}

// ---------------------------------------------------------------------
// Client-side persistent connection manager
// ---------------------------------------------------------------------

/// Events surfaced to the application from the persistent client connection,
/// combining protocol [`Event`]s with connection-lifecycle notices.
#[derive(Clone, Debug)]
pub enum ClientEvent {
    /// The daemon pushed a sync/upload event.
    Event(Event),
    /// (Re)connected and handshake succeeded.
    Connected,
    /// The connection dropped (or never connected); `reason` is
    /// human-readable. The manager keeps retrying with backoff — this is
    /// not fatal.
    Disconnected(String),
    /// An unrecoverable error (currently: protocol version mismatch). The
    /// manager thread has stopped retrying and exited; no further
    /// `ClientEvent`s will arrive.
    Fatal(String),
}

/// Manages one persistent connection to `jamaild`, reconnecting with
/// backoff on failure. Requests are enqueued via [`IpcClient::send`]
/// (non-blocking, safe to call while disconnected — they're delivered once
/// reconnected); events (and connection-lifecycle notices) are drained via
/// [`IpcClient::event_rx`], mirroring how `sync::SyncEvent` was drained from
/// an `mpsc::Receiver` before the daemon split.
pub struct IpcClient {
    request_tx: mpsc::SyncSender<Request>,
    pub event_rx: mpsc::Receiver<ClientEvent>,
}

/// Bound on queued-but-not-yet-sent requests while disconnected. Far above
/// any realistic backlog (mark-seen/upload requests are infrequent,
/// human-paced actions) — this is a safety valve, not a real limit.
const REQUEST_QUEUE_CAPACITY: usize = 4096;

impl IpcClient {
    /// Spawn the connection-manager thread with the real reconnect backoff.
    pub fn spawn(socket_path: PathBuf) -> Self {
        Self::spawn_with_backoff(socket_path, Duration::from_secs(1), Duration::from_secs(5))
    }

    /// Spawn with an explicit backoff range (tests use short delays so they
    /// don't spend wall-clock time waiting on production timing).
    pub fn spawn_with_backoff(
        socket_path: PathBuf,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel(REQUEST_QUEUE_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();
        thread::spawn(move || {
            connection_manager_loop(
                socket_path,
                request_rx,
                event_tx,
                initial_backoff,
                max_backoff,
            );
        });
        Self {
            request_tx,
            event_rx,
        }
    }

    /// Enqueue a request to be sent once connected. Drops the request
    /// (silently — there is no meaningful recovery) only if the internal
    /// queue is completely full, which would mean `jamaild` has been
    /// unreachable for a very long time.
    pub fn send(&self, req: Request) {
        let _ = self.request_tx.try_send(req);
    }
}

fn connection_manager_loop(
    socket_path: PathBuf,
    request_rx: mpsc::Receiver<Request>,
    event_tx: mpsc::Sender<ClientEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
) {
    let mut backoff = initial_backoff;
    loop {
        match UnixStream::connect(&socket_path) {
            Ok(mut stream) => match handshake(&mut stream) {
                Ok(()) => {
                    backoff = initial_backoff;
                    let _ = event_tx.send(ClientEvent::Connected);
                    run_connection(&mut stream, &request_rx, &event_tx);
                    let _ = event_tx.send(ClientEvent::Disconnected(
                        "connection to jamaild lost".to_string(),
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                    let _ = event_tx.send(ClientEvent::Fatal(e.to_string()));
                    return; // fatal: stop retrying
                }
                Err(e) => {
                    let _ = event_tx.send(ClientEvent::Disconnected(format!("handshake: {}", e)));
                }
            },
            Err(e) => {
                let _ = event_tx.send(ClientEvent::Disconnected(format!(
                    "connecting to jamaild: {}",
                    e
                )));
            }
        }
        thread::sleep(backoff);
        backoff = std::cmp::min(backoff * 2, max_backoff);
    }
}

/// Run one live connection: a reader thread forwards `ServerMessage` frames
/// as `ClientEvent`s while this (calling) thread drains `request_rx` and
/// writes `Request` frames. Returns once either direction hits an error
/// (peer gone), at which point the caller reconnects.
fn run_connection(
    stream: &mut UnixStream,
    request_rx: &mpsc::Receiver<Request>,
    event_tx: &mpsc::Sender<ClientEvent>,
) {
    let mut reader = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let reader_event_tx = event_tx.clone();
    let (dead_tx, dead_rx) = mpsc::channel::<()>();
    let reader_handle = thread::spawn(move || {
        loop {
            match read_message::<_, ServerMessage>(&mut reader) {
                Ok(ServerMessage::Event(ev)) => {
                    if reader_event_tx.send(ClientEvent::Event(ev)).is_err() {
                        break;
                    }
                }
                Ok(ServerMessage::Response(_)) => {
                    // Responses are informational only in this protocol
                    // (see module docs) — nothing to correlate them to.
                }
                Err(_) => break,
            }
        }
        let _ = dead_tx.send(());
    });

    // Writer loop: forward queued requests until the reader signals the
    // connection died, or a write fails outright.
    loop {
        if dead_rx.try_recv().is_ok() {
            break;
        }
        match request_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(req) => {
                if write_message(stream, &req).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = reader_handle.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_preserves_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello world").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let out = read_frame(&mut cursor).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn frame_roundtrip_handles_empty_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let out = read_frame(&mut cursor).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn write_frame_rejects_oversized_payload() {
        let mut buf = Vec::new();
        let huge = vec![0u8; MAX_FRAME_BYTES as usize + 1];
        let err = write_frame(&mut buf, &huge).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn read_frame_rejects_oversized_length_prefix() {
        let mut buf = Vec::new();
        // Craft a length prefix bigger than MAX_FRAME_BYTES without
        // actually allocating/writing that many bytes.
        let bogus_len = MAX_FRAME_BYTES + 1;
        buf.extend_from_slice(&bogus_len.to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_frame_reports_eof_on_truncated_stream() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn message_roundtrip_request() {
        let req = Request::SetCurrentFolder {
            account: "work".to_string(),
            folder: "INBOX".to_string(),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let out: Request = read_message(&mut cursor).unwrap();
        match out {
            Request::SetCurrentFolder { account, folder } => {
                assert_eq!(account, "work");
                assert_eq!(folder, "INBOX");
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn message_roundtrip_enqueue_upload_preserves_binary_payload_via_base64() {
        use base64::Engine;
        let raw = vec![0u8, 1, 2, 255, 254, 10, 13, 0];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let req = Request::EnqueueUpload {
            account: "personal".to_string(),
            kind: crate::sync::UploadKind::Draft,
            local_id: 42,
            folder: "Drafts".to_string(),
            raw_message_b64: b64,
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let out: Request = read_message(&mut cursor).unwrap();
        match out {
            Request::EnqueueUpload {
                raw_message_b64,
                local_id,
                ..
            } => {
                assert_eq!(local_id, 42);
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&raw_message_b64)
                    .unwrap();
                assert_eq!(decoded, raw);
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn message_roundtrip_server_message_event() {
        let msg = ServerMessage::Event(Event::FolderComplete {
            account: "work".to_string(),
            folder: "INBOX".to_string(),
            count: 3,
        });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let out: ServerMessage = read_message(&mut cursor).unwrap();
        match out {
            ServerMessage::Event(Event::FolderComplete {
                account,
                folder,
                count,
            }) => {
                assert_eq!(account, "work");
                assert_eq!(folder, "INBOX");
                assert_eq!(count, 3);
            }
            other => panic!("unexpected message: {:?}", other),
        }
    }

    #[test]
    fn wire_event_tags_every_variant_with_account() {
        use crate::sync::{SyncEvent, UploadKind};
        let cases: Vec<SyncEvent> = vec![
            SyncEvent::Syncing("INBOX".to_string()),
            SyncEvent::Progress("INBOX".to_string(), 1, 2),
            SyncEvent::FolderComplete("INBOX".to_string(), 5),
            SyncEvent::AllComplete,
            SyncEvent::Error("boom".to_string()),
            SyncEvent::FoldersLoaded(vec![]),
            SyncEvent::FlagsChanged("INBOX".to_string()),
            SyncEvent::UploadComplete(UploadKind::Sent, 7),
            SyncEvent::UploadError(UploadKind::Draft, 8, "nope".to_string()),
        ];
        for ev in cases {
            let wire = wire_event("acct1", &ev);
            let account = match &wire {
                Event::Syncing { account, .. } => account,
                Event::Progress { account, .. } => account,
                Event::FolderComplete { account, .. } => account,
                Event::AllComplete { account } => account,
                Event::Error { account, .. } => account,
                Event::FoldersLoaded { account, .. } => account,
                Event::FlagsChanged { account, .. } => account,
                Event::UploadComplete { account, .. } => account,
                Event::UploadError { account, .. } => account,
            };
            assert_eq!(account, "acct1");
        }
    }

    #[test]
    fn is_compatible_requires_exact_match() {
        assert!(is_compatible(1, 1));
        assert!(!is_compatible(1, 2));
        assert!(!is_compatible(2, 1));
    }

    #[test]
    fn resolve_socket_path_prefers_env_override() {
        let p = resolve_socket_path_from(
            Some("/custom/sock"),
            Some("/config/sock"),
            Some("/run/user/1000"),
            1000,
        );
        assert_eq!(p, PathBuf::from("/custom/sock"));
    }

    #[test]
    fn resolve_socket_path_falls_back_to_config_then_xdg_then_tmp() {
        assert_eq!(
            resolve_socket_path_from(None, Some("/config/sock"), Some("/run/user/1000"), 1000),
            PathBuf::from("/config/sock")
        );
        assert_eq!(
            resolve_socket_path_from(None, None, Some("/run/user/1000"), 1000),
            PathBuf::from("/run/user/1000/jamail/jamaild.sock")
        );
        assert_eq!(
            resolve_socket_path_from(None, None, None, 1000),
            PathBuf::from("/tmp/jamail-1000/jamaild.sock")
        );
    }

    #[test]
    fn resolve_socket_path_treats_empty_strings_as_unset() {
        // A blank env var (e.g. `JAMAIL_SOCKET=` in a launched-from-systemd
        // environment) must not be treated as an explicit empty-path override.
        assert_eq!(
            resolve_socket_path_from(Some(""), Some(""), Some("/run/user/1000"), 1000),
            PathBuf::from("/run/user/1000/jamail/jamaild.sock")
        );
    }

    #[test]
    fn ping_against_nothing_listening_fails_promptly() {
        let dir = std::env::temp_dir().join(format!("jamail-ipc-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("no-daemon-here.sock");
        let start = std::time::Instant::now();
        let result = ping(&path, Duration::from_millis(200));
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "ping against a missing socket must fail promptly, took {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ipc_client_reconnects_once_daemon_becomes_available() {
        let dir = std::env::temp_dir().join(format!(
            "jamail-ipc-reconnect-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jamaild.sock");

        // Nothing listening yet: the client should report Disconnected
        // (not panic, not block forever) using a short test backoff.
        let client = IpcClient::spawn_with_backoff(
            path.clone(),
            Duration::from_millis(20),
            Duration::from_millis(50),
        );
        let mut saw_disconnected = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(ClientEvent::Disconnected(_)) =
                client.event_rx.recv_timeout(Duration::from_millis(100))
            {
                saw_disconnected = true;
                break;
            }
        }
        assert!(saw_disconnected, "expected at least one Disconnected event");

        // Now start a minimal listener that only does the handshake, and
        // confirm the client reports Connected shortly after.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let accept_handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _hello: ClientHello = read_message(&mut stream).unwrap();
                write_message(
                    &mut stream,
                    &ServerHello::Ok {
                        protocol_version: PROTOCOL_VERSION,
                        server_pid: std::process::id(),
                    },
                )
                .unwrap();
                // Keep the connection open briefly so the client observes
                // Connected before this thread exits and drops the stream.
                thread::sleep(Duration::from_millis(300));
            }
        });

        let mut saw_connected = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(ClientEvent::Connected) =
                client.event_rx.recv_timeout(Duration::from_millis(100))
            {
                saw_connected = true;
                break;
            }
        }
        assert!(
            saw_connected,
            "expected a Connected event after listener started"
        );

        let _ = accept_handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ipc_client_reports_fatal_on_version_mismatch_and_stops_retrying() {
        let dir = std::env::temp_dir().join(format!(
            "jamail-ipc-mismatch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jamaild.sock");

        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let accept_handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _hello: ClientHello = read_message(&mut stream).unwrap();
                write_message(
                    &mut stream,
                    &ServerHello::VersionMismatch {
                        server_protocol_version: PROTOCOL_VERSION + 1,
                    },
                )
                .unwrap();
            }
        });

        let client = IpcClient::spawn_with_backoff(
            path.clone(),
            Duration::from_millis(20),
            Duration::from_millis(50),
        );
        let mut saw_fatal = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match client.event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ClientEvent::Fatal(_)) => {
                    saw_fatal = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        assert!(saw_fatal, "expected a Fatal event on version mismatch");

        // No further events should show up (no reconnect loop still running).
        let extra = client.event_rx.recv_timeout(Duration::from_millis(300));
        assert!(
            extra.is_err(),
            "manager thread must stop after a fatal version mismatch, got {:?}",
            extra
        );

        let _ = accept_handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
