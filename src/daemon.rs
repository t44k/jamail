//! The `jamaild` daemon: owns every configured account's IMAP sync loop,
//! the local cache writes, and desktop-notification delivery, and serves
//! the `jamail` client over the IPC protocol defined in [`crate::ipc`].
//!
//! ## Process model
//!
//! On start, the daemon spawns one [`crate::sync`] sync loop per configured
//! account (the same loop the pre-daemon-split `jamail` used to spawn for
//! whichever single account was currently active) plus one "event pump"
//! thread per account that drains that loop's `SyncEvent`s, decides whether
//! to fire a desktop notification ([`dispatch_sync_event`], using
//! [`crate::notify::should_notify`]), and broadcasts a tagged [`ipc::Event`]
//! to every connected client. Crucially, **all** accounts sync continuously
//! from daemon startup, independent of whether any client is connected or
//! which account/folder a connected client is currently viewing — this is
//! what makes notifications work under `systemd --user` with no `jamail`
//! TUI running at all, and is the one deliberate behavior change from the
//! pre-split single-account-at-a-time model (documented in `CLAUDE.md`).
//!
//! A [`crate::ipc::Request::SetCurrentFolder`] only ever changes which
//! folder an account's IMAP IDLE prioritizes for responsiveness; it never
//! starts or stops syncing an account.
//!
//! ## Connections
//!
//! Each accepted connection gets a reader "thread" (this module's
//! [`handle_client`], which also does the handshake and answers requests)
//! and a writer thread (drains a per-client channel fed by both direct
//! responses and broadcast events, so only one thread ever writes to a
//! given socket — required since IPC frames must not interleave).
//!
//! ## Shutdown
//!
//! See `ipc` module docs for the high-level lifecycle. Concretely: SIGTERM
//!/SIGINT set a process-global flag; a small watcher thread polls it (and
//! [`Daemon::shutdown_requested`], set by an explicit
//! [`crate::ipc::Request::Shutdown`]) and, once tripped, unlinks the socket
//! file and calls `std::process::exit(0)`. Every account's `SyncControl`
//! also gets its `shutdown` flag set first, so sync loops stop reconnecting
//! (though the process exit does not wait for them to notice — see
//! `sync::SyncControl` for why that's an acceptable, pre-existing pattern:
//! account switches already abandoned sync threads the same way).

use crate::config::{JamailAccount, JamailConfig};
use crate::ipc::{self, ClientHello, Request, Response, ServerHello, ServerMessage};
use crate::notify;
use crate::sync::{self, MarkSeenRequest, SyncControl, SyncEvent, UploadRequest};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// A notification sink: `(account_label, folder, count) -> ()`. Swappable
/// so tests can assert on notification decisions without spawning a real
/// `notify-send` process; production code uses [`default_notifier`].
pub type Notifier = Arc<dyn Fn(&str, &str, usize) + Send + Sync>;

/// The real notifier: fires a desktop notification via [`crate::notify`].
pub fn default_notifier() -> Notifier {
    Arc::new(|account: &str, folder: &str, count: usize| {
        notify::notify_new_mail(account, folder, count);
    })
}

struct AccountRuntime {
    control: Arc<SyncControl>,
}

/// Per-client outgoing queue capacity. A slow/stuck client only ever misses
/// events (broadcast uses `try_send` and drops on a full queue) rather than
/// blocking the account event pump that every other client shares.
const CLIENT_QUEUE_CAPACITY: usize = 256;

pub struct Daemon {
    accounts: HashMap<String, AccountRuntime>,
    clients: Mutex<HashMap<u64, mpsc::SyncSender<ServerMessage>>>,
    next_client_id: AtomicU64,
    shutdown_requested: AtomicBool,
}

impl Daemon {
    fn new(accounts: HashMap<String, AccountRuntime>) -> Arc<Self> {
        Arc::new(Self {
            accounts,
            clients: Mutex::new(HashMap::new()),
            next_client_id: AtomicU64::new(1),
            shutdown_requested: AtomicBool::new(false),
        })
    }

    /// Spawn a real IMAP sync thread + event pump for every configured
    /// account and return the daemon that manages them.
    pub fn start(accounts: &[(String, JamailAccount)], notifier: Notifier) -> Arc<Self> {
        let mut map = HashMap::new();
        let mut pumps = Vec::new();
        for (name, acct) in accounts {
            // Default IDLE target is INBOX until a client sends
            // SetCurrentFolder; every account still syncs its full
            // configured folder filter regardless (set by the sync loop
            // itself once it lists folders — see sync::sync_loop).
            let control = Arc::new(SyncControl::new("INBOX"));
            let (tx, rx) = mpsc::channel::<SyncEvent>();
            let _handle =
                sync::spawn_sync_thread(acct.clone(), name.clone(), Arc::clone(&control), tx);
            map.insert(name.clone(), AccountRuntime { control });
            pumps.push((name.clone(), acct.clone(), rx));
        }
        let daemon = Self::new(map);
        for (name, acct, rx) in pumps {
            spawn_pump(Arc::clone(&daemon), name, acct, rx, Arc::clone(&notifier));
        }
        daemon
    }

    fn broadcast(&self, msg: ServerMessage) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|_, tx| tx.try_send(msg.clone()).is_ok());
    }

    fn register_client(&self, tx: mpsc::SyncSender<ServerMessage>) -> u64 {
        let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().insert(id, tx);
        id
    }

    fn unregister_client(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
    }

    /// Set every account's shutdown flag and mark the daemon as shutting
    /// down. Does not itself terminate the process — see module docs.
    fn trigger_graceful_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        for rt in self.accounts.values() {
            rt.control.shutdown.store(true, Ordering::Relaxed);
        }
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }
}

fn spawn_pump(
    daemon: Arc<Daemon>,
    account: String,
    cfg: JamailAccount,
    rx: mpsc::Receiver<SyncEvent>,
    notifier: Notifier,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(ev) = rx.recv() {
            dispatch_sync_event(&account, &cfg, &ev, notifier.as_ref());
            daemon.broadcast(ServerMessage::Event(ipc::wire_event(&account, &ev)));
        }
    })
}

/// Decide whether `ev` should trigger a desktop notification for `account`
/// and, if so, call `notifier(account, folder, count)`. This is the entire
/// "notification ownership" decision, and it runs here — inside the
/// daemon's per-account event pump — never in the `jamail` client, and
/// never gated on any client being connected. Exposed standalone (rather
/// than inlined in [`spawn_pump`]'s loop) so it can be driven directly with
/// a synthetic event and a recording notifier in tests, without a real IMAP
/// connection or a real `notify-send` binary.
pub fn dispatch_sync_event(
    account: &str,
    account_cfg: &JamailAccount,
    ev: &SyncEvent,
    notifier: &dyn Fn(&str, &str, usize),
) {
    if let SyncEvent::FolderComplete(folder, count) = ev
        && *count > 0
        && notify::should_notify(account_cfg.notify_folders.as_deref(), folder)
    {
        notifier(account, folder, *count);
    }
}

fn handle_request(daemon: &Daemon, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::SetCurrentFolder { account, folder } => with_account(daemon, &account, |rt| {
            if let Ok(mut cf) = rt.control.current_folder.write() {
                *cf = folder;
            }
        }),
        Request::MarkSeen {
            account,
            folder,
            uid,
        } => with_account(daemon, &account, |rt| {
            if let Ok(mut q) = rt.control.mark_seen_queue.lock() {
                q.push(MarkSeenRequest { folder, uid });
            }
        }),
        Request::EnqueueUpload {
            account,
            kind,
            local_id,
            folder,
            raw_message_b64,
        } => {
            use base64::Engine;
            let raw_message =
                match base64::engine::general_purpose::STANDARD.decode(&raw_message_b64) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        return Response::Error {
                            message: format!("invalid base64 in raw_message_b64: {}", e),
                        };
                    }
                };
            with_account(daemon, &account, |rt| {
                if let Ok(mut q) = rt.control.upload_queue.lock() {
                    q.push(UploadRequest {
                        kind,
                        local_id,
                        folder,
                        raw_message,
                    });
                }
            })
        }
        Request::ForceReconnect { account } => with_account(daemon, &account, |rt| {
            rt.control.force_reconnect.store(true, Ordering::Relaxed);
        }),
        Request::Shutdown => {
            daemon.trigger_graceful_shutdown();
            Response::Ok
        }
    }
}

fn with_account(daemon: &Daemon, account: &str, f: impl FnOnce(&AccountRuntime)) -> Response {
    match daemon.accounts.get(account) {
        Some(rt) => {
            f(rt);
            Response::Ok
        }
        None => Response::Error {
            message: format!("unknown account: {}", account),
        },
    }
}

/// Handle one accepted connection end-to-end: handshake, then request loop
/// until the peer disconnects or sends a framing-invalid message. Runs on
/// its own thread (spawned by [`run`]); spawns one additional writer thread
/// (see module docs on why reads/writes are split).
fn handle_client(mut stream: UnixStream, daemon: Arc<Daemon>) {
    let hello: ClientHello = match ipc::read_message(&mut stream) {
        Ok(h) => h,
        Err(_) => return,
    };

    if !ipc::is_compatible(hello.protocol_version, ipc::PROTOCOL_VERSION) {
        let _ = ipc::write_message(
            &mut stream,
            &ServerHello::VersionMismatch {
                server_protocol_version: ipc::PROTOCOL_VERSION,
            },
        );
        return; // daemon closes the connection on mismatch, per protocol.
    }
    if ipc::write_message(
        &mut stream,
        &ServerHello::Ok {
            protocol_version: ipc::PROTOCOL_VERSION,
            server_pid: std::process::id(),
        },
    )
    .is_err()
    {
        return;
    }

    let (client_tx, client_rx) = mpsc::sync_channel::<ServerMessage>(CLIENT_QUEUE_CAPACITY);
    let id = daemon.register_client(client_tx.clone());

    let writer_handle = match stream.try_clone() {
        Ok(mut writer_stream) => Some(thread::spawn(move || {
            while let Ok(msg) = client_rx.recv() {
                if ipc::write_message(&mut writer_stream, &msg).is_err() {
                    break;
                }
            }
        })),
        Err(_) => None,
    };

    while let Ok(req) = ipc::read_message::<_, Request>(&mut stream) {
        let resp = handle_request(&daemon, req);
        if client_tx.send(ServerMessage::Response(resp)).is_err() {
            break;
        }
    }

    daemon.unregister_client(id);
    // Drop this thread's own sender clone so the writer thread's `recv()`
    // observes every sender gone (the map's clone was just removed above)
    // and exits its loop, letting the join below actually complete.
    drop(client_tx);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    if let Some(h) = writer_handle {
        let _ = h.join();
    }
}

/// Bind `path`, recovering from a stale leftover socket file from a
/// previously-killed daemon: if binding fails with `AddrInUse`, probe by
/// connecting — a successful connect means another daemon is genuinely
/// live (refuse to start), a refused connect means the file is stale
/// (remove it and bind again).
fn bind_or_recover(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                anyhow::bail!(
                    "jamaild appears to already be running (socket {} is live)",
                    path.display()
                );
            }
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
            UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("binding {}", path.display())),
    }
}

fn set_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_termination_signal(_sig: libc::c_int) {
    // Async-signal-safe: only an atomic store.
    SIGNAL_RECEIVED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_termination_signal as *const () as usize,
        );
        libc::signal(
            libc::SIGINT,
            handle_termination_signal as *const () as usize,
        );
    }
}

/// Run the daemon: load config, bind the IPC socket, spawn every account's
/// sync loop + event pump, and accept client connections until told to
/// shut down (SIGTERM/SIGINT or a `Request::Shutdown`). Blocks until exit.
pub fn run() -> Result<()> {
    run_with_notifier(default_notifier())
}

/// Same as [`run`] but with an injectable notifier — used by tests that
/// want the real accept-loop/signal-handling wiring without spawning real
/// notification processes. Production use is always [`run`].
pub fn run_with_notifier(notifier: Notifier) -> Result<()> {
    let config = JamailConfig::load().context("Failed to load jamail config")?;
    let socket_path = ipc::resolve_socket_path(
        config
            .daemon
            .as_ref()
            .and_then(|d| d.socket_path.as_deref()),
    );
    ipc::ensure_socket_dir(&socket_path)
        .with_context(|| format!("preparing directory for {}", socket_path.display()))?;
    let listener = bind_or_recover(&socket_path)?;
    set_socket_permissions(&socket_path)
        .with_context(|| format!("setting permissions on {}", socket_path.display()))?;

    install_signal_handlers();

    let accounts: Vec<(String, JamailAccount)> = config.accounts.into_iter().collect();
    let daemon = Daemon::start(&accounts, notifier);

    {
        let daemon = Arc::clone(&daemon);
        let socket_path = socket_path.clone();
        thread::spawn(move || {
            loop {
                if SIGNAL_RECEIVED.load(Ordering::SeqCst) || daemon.shutdown_requested() {
                    let _ = std::fs::remove_file(&socket_path);
                    std::process::exit(0);
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                thread::spawn(move || handle_client(stream, daemon));
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

/// Best-effort: spawn a detached `jamaild` if one isn't already reachable
/// at `socket_path`. Lets a bare `jamail` invocation "just work" without
/// requiring the user to have set up the systemd --user unit first (that
/// remains the recommended always-on setup — see `contrib/systemd` — this
/// is the convenience fallback). Never blocks waiting for the daemon to
/// finish starting: the caller's `ipc::IpcClient` reconnect loop picks up
/// the socket once it exists.
pub fn try_autostart(socket_path: &Path) {
    if ipc::ping(socket_path, Duration::from_millis(250)).is_ok() {
        return; // already running
    }
    let jamaild_path = locate_jamaild();
    // `setsid` detaches jamaild into its own session so it outlives this
    // process (matters if `jamail` exits, or the terminal it ran in
    // closes) — falls back to a plain spawn (still works, just without the
    // detach) if `setsid` isn't installed, which would be unusual on any
    // Linux system (it ships in util-linux).
    let cmd = format!(
        "setsid {} >/dev/null 2>&1 </dev/null &",
        shell_quote(&jamaild_path)
    );
    let _ = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn locate_jamaild() -> String {
    // Prefer a `jamaild` next to the currently-running binary — covers
    // `cargo install --path .`, which installs both binaries to the same
    // directory. Falls back to a bare `jamaild`, resolved via the shell's
    // PATH.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("jamaild");
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    "jamaild".to_string()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, ImapConfig};
    use std::sync::Mutex as StdMutex;

    fn test_account(notify_folders: Option<Vec<String>>) -> JamailAccount {
        JamailAccount {
            default: false,
            email: "alice@example.com".to_string(),
            display_name: None,
            imap: ImapConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                login: "alice@example.com".to_string(),
                auth: AuthConfig {
                    auth_type: "password".to_string(),
                    value: "secret".to_string(),
                },
            },
            smtp: None,
            folders: None,
            show_unlisted_folders: false,
            senders: None,
            sent_folder: None,
            draft_folder: None,
            notify_folders,
            color: None,
        }
    }

    // -- dispatch_sync_event: notification-ownership decision logic -----

    #[test]
    fn dispatch_sync_event_notifies_when_configured_and_count_positive() {
        let cfg = test_account(Some(vec!["INBOX".to_string()]));
        let calls: Arc<StdMutex<Vec<(String, String, usize)>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let calls2 = Arc::clone(&calls);
        let notifier = move |a: &str, f: &str, c: usize| {
            calls2
                .lock()
                .unwrap()
                .push((a.to_string(), f.to_string(), c));
        };
        dispatch_sync_event(
            "personal",
            &cfg,
            &SyncEvent::FolderComplete("INBOX".to_string(), 3),
            &notifier,
        );
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0],
            ("personal".to_string(), "INBOX".to_string(), 3)
        );
    }

    #[test]
    fn dispatch_sync_event_does_not_notify_when_folder_unconfigured() {
        let cfg = test_account(Some(vec!["Important".to_string()]));
        let called = Arc::new(StdMutex::new(false));
        let called2 = Arc::clone(&called);
        let notifier = move |_: &str, _: &str, _: usize| {
            *called2.lock().unwrap() = true;
        };
        dispatch_sync_event(
            "personal",
            &cfg,
            &SyncEvent::FolderComplete("INBOX".to_string(), 3),
            &notifier,
        );
        assert!(!*called.lock().unwrap());
    }

    #[test]
    fn dispatch_sync_event_does_not_notify_when_notify_folders_unset() {
        let cfg = test_account(None);
        let called = Arc::new(StdMutex::new(false));
        let called2 = Arc::clone(&called);
        let notifier = move |_: &str, _: &str, _: usize| {
            *called2.lock().unwrap() = true;
        };
        dispatch_sync_event(
            "personal",
            &cfg,
            &SyncEvent::FolderComplete("INBOX".to_string(), 1),
            &notifier,
        );
        assert!(!*called.lock().unwrap());
    }

    #[test]
    fn dispatch_sync_event_does_not_notify_when_count_is_zero() {
        let cfg = test_account(Some(vec!["INBOX".to_string()]));
        let called = Arc::new(StdMutex::new(false));
        let called2 = Arc::clone(&called);
        let notifier = move |_: &str, _: &str, _: usize| {
            *called2.lock().unwrap() = true;
        };
        dispatch_sync_event(
            "personal",
            &cfg,
            &SyncEvent::FolderComplete("INBOX".to_string(), 0),
            &notifier,
        );
        assert!(!*called.lock().unwrap());
    }

    #[test]
    fn dispatch_sync_event_ignores_non_folder_complete_events() {
        let cfg = test_account(Some(vec!["INBOX".to_string()]));
        let called = Arc::new(StdMutex::new(false));
        let called2 = Arc::clone(&called);
        let notifier = move |_: &str, _: &str, _: usize| {
            *called2.lock().unwrap() = true;
        };
        for ev in [
            SyncEvent::Syncing("INBOX".to_string()),
            SyncEvent::AllComplete,
            SyncEvent::Error("boom".to_string()),
        ] {
            dispatch_sync_event("personal", &cfg, &ev, &notifier);
        }
        assert!(!*called.lock().unwrap());
    }

    #[test]
    fn dispatch_sync_event_fires_with_zero_clients_connected() {
        // The crux of "the daemon owns notification delivery": this proves
        // the decision doesn't depend on any client being attached at all —
        // dispatch_sync_event takes no client/connection state whatsoever.
        let cfg = test_account(Some(vec!["INBOX".to_string()]));
        let called = Arc::new(StdMutex::new(false));
        let called2 = Arc::clone(&called);
        let notifier = move |_: &str, _: &str, _: usize| {
            *called2.lock().unwrap() = true;
        };
        dispatch_sync_event(
            "personal",
            &cfg,
            &SyncEvent::FolderComplete("INBOX".to_string(), 1),
            &notifier,
        );
        assert!(*called.lock().unwrap());
    }

    // -- handle_client: client-server interaction over a real socketpair -

    fn test_daemon_with_account(
        name: &str,
        cfg: JamailAccount,
    ) -> (Arc<Daemon>, mpsc::Sender<SyncEvent>) {
        let control = Arc::new(SyncControl::new("INBOX"));
        let mut map = HashMap::new();
        map.insert(
            name.to_string(),
            AccountRuntime {
                control: Arc::clone(&control),
            },
        );
        let daemon = Daemon::new(map);
        let (tx, rx) = mpsc::channel();
        spawn_pump(
            Arc::clone(&daemon),
            name.to_string(),
            cfg,
            rx,
            Arc::new(|_, _, _| {}),
        );
        (daemon, tx)
    }

    fn connect_pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    fn client_hello() -> ClientHello {
        ClientHello {
            protocol_version: ipc::PROTOCOL_VERSION,
            client_name: "test-client".to_string(),
            pid: std::process::id(),
        }
    }

    #[test]
    fn handshake_and_ping_roundtrip() {
        let (daemon, _tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        ipc::write_message(&mut client, &client_hello()).unwrap();
        let hello: ServerHello = ipc::read_message(&mut client).unwrap();
        assert!(matches!(hello, ServerHello::Ok { .. }));

        ipc::write_message(&mut client, &Request::Ping).unwrap();
        let resp: ServerMessage = ipc::read_message(&mut client).unwrap();
        assert!(matches!(resp, ServerMessage::Response(Response::Pong)));

        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn version_mismatch_is_rejected_and_connection_closed() {
        let (daemon, _tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        let mut bad_hello = client_hello();
        bad_hello.protocol_version = ipc::PROTOCOL_VERSION + 1;
        ipc::write_message(&mut client, &bad_hello).unwrap();
        let hello: ServerHello = ipc::read_message(&mut client).unwrap();
        match hello {
            ServerHello::VersionMismatch {
                server_protocol_version,
            } => assert_eq!(server_protocol_version, ipc::PROTOCOL_VERSION),
            other => panic!("expected VersionMismatch, got {:?}", other),
        }

        // The daemon must not proceed to request handling after a mismatch:
        // sending a request now and expecting a response should time out /
        // fail because the daemon side already returned from handle_client.
        handle.join().unwrap();
    }

    #[test]
    fn set_current_folder_updates_the_right_account() {
        let (daemon, _tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        ipc::write_message(&mut client, &client_hello()).unwrap();
        let _: ServerHello = ipc::read_message(&mut client).unwrap();

        ipc::write_message(
            &mut client,
            &Request::SetCurrentFolder {
                account: "acct1".to_string(),
                folder: "Archive".to_string(),
            },
        )
        .unwrap();
        let resp: ServerMessage = ipc::read_message(&mut client).unwrap();
        assert!(matches!(resp, ServerMessage::Response(Response::Ok)));

        assert_eq!(
            *daemon.accounts["acct1"]
                .control
                .current_folder
                .read()
                .unwrap(),
            "Archive"
        );

        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn mark_seen_enqueues_on_the_right_account() {
        let (daemon, _tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        ipc::write_message(&mut client, &client_hello()).unwrap();
        let _: ServerHello = ipc::read_message(&mut client).unwrap();

        ipc::write_message(
            &mut client,
            &Request::MarkSeen {
                account: "acct1".to_string(),
                folder: "INBOX".to_string(),
                uid: 42,
            },
        )
        .unwrap();
        let resp: ServerMessage = ipc::read_message(&mut client).unwrap();
        assert!(matches!(resp, ServerMessage::Response(Response::Ok)));

        let queue = daemon.accounts["acct1"]
            .control
            .mark_seen_queue
            .lock()
            .unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].folder, "INBOX");
        assert_eq!(queue[0].uid, 42);

        drop(queue);
        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn enqueue_upload_decodes_base64_and_enqueues() {
        use base64::Engine;
        let (daemon, _tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        ipc::write_message(&mut client, &client_hello()).unwrap();
        let _: ServerHello = ipc::read_message(&mut client).unwrap();

        let raw = b"From: a@b.com\r\n\r\nhello".to_vec();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        ipc::write_message(
            &mut client,
            &Request::EnqueueUpload {
                account: "acct1".to_string(),
                kind: sync::UploadKind::Draft,
                local_id: 7,
                folder: "Drafts".to_string(),
                raw_message_b64: b64,
            },
        )
        .unwrap();
        let resp: ServerMessage = ipc::read_message(&mut client).unwrap();
        assert!(matches!(resp, ServerMessage::Response(Response::Ok)));

        let queue = daemon.accounts["acct1"]
            .control
            .upload_queue
            .lock()
            .unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].local_id, 7);
        assert_eq!(queue[0].folder, "Drafts");
        assert_eq!(queue[0].raw_message, raw);
        assert_eq!(queue[0].kind, sync::UploadKind::Draft);

        drop(queue);
        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn enqueue_upload_with_invalid_base64_returns_error_response() {
        let (daemon, _tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        ipc::write_message(&mut client, &client_hello()).unwrap();
        let _: ServerHello = ipc::read_message(&mut client).unwrap();

        ipc::write_message(
            &mut client,
            &Request::EnqueueUpload {
                account: "acct1".to_string(),
                kind: sync::UploadKind::Sent,
                local_id: 1,
                folder: "Sent".to_string(),
                raw_message_b64: "not valid base64 !!!".to_string(),
            },
        )
        .unwrap();
        let resp: ServerMessage = ipc::read_message(&mut client).unwrap();
        assert!(matches!(
            resp,
            ServerMessage::Response(Response::Error { .. })
        ));

        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn unknown_account_returns_error_response_and_connection_stays_open() {
        let (daemon, _tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        ipc::write_message(&mut client, &client_hello()).unwrap();
        let _: ServerHello = ipc::read_message(&mut client).unwrap();

        ipc::write_message(
            &mut client,
            &Request::SetCurrentFolder {
                account: "no-such-account".to_string(),
                folder: "INBOX".to_string(),
            },
        )
        .unwrap();
        let resp: ServerMessage = ipc::read_message(&mut client).unwrap();
        assert!(matches!(
            resp,
            ServerMessage::Response(Response::Error { .. })
        ));

        // Connection must still be usable afterwards.
        ipc::write_message(&mut client, &Request::Ping).unwrap();
        let resp2: ServerMessage = ipc::read_message(&mut client).unwrap();
        assert!(matches!(resp2, ServerMessage::Response(Response::Pong)));

        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn shutdown_request_sets_daemon_and_every_account_shutdown_flag() {
        let (daemon, _tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        ipc::write_message(&mut client, &client_hello()).unwrap();
        let _: ServerHello = ipc::read_message(&mut client).unwrap();

        ipc::write_message(&mut client, &Request::Shutdown).unwrap();
        let resp: ServerMessage = ipc::read_message(&mut client).unwrap();
        assert!(matches!(resp, ServerMessage::Response(Response::Ok)));

        assert!(daemon.shutdown_requested());
        assert!(
            daemon.accounts["acct1"]
                .control
                .shutdown
                .load(Ordering::Relaxed)
        );

        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn events_are_broadcast_to_connected_clients() {
        let (daemon, tx) = test_daemon_with_account("acct1", test_account(None));
        let (mut client, server) = connect_pair();
        let d = Arc::clone(&daemon);
        let handle = thread::spawn(move || handle_client(server, d));

        ipc::write_message(&mut client, &client_hello()).unwrap();
        let _: ServerHello = ipc::read_message(&mut client).unwrap();

        // Drive the (fake, no real IMAP) account's sync-event channel by
        // hand, exactly like a real sync loop would, and confirm the
        // client receives the tagged wire event through the real
        // handle_client broadcast path.
        tx.send(SyncEvent::FolderComplete("INBOX".to_string(), 4))
            .unwrap();

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let msg: ServerMessage = ipc::read_message(&mut client).unwrap();
        match msg {
            ServerMessage::Event(ipc::Event::FolderComplete {
                account,
                folder,
                count,
            }) => {
                assert_eq!(account, "acct1");
                assert_eq!(folder, "INBOX");
                assert_eq!(count, 4);
            }
            other => panic!("unexpected message: {:?}", other),
        }

        drop(tx);
        drop(client);
        handle.join().unwrap();
    }

    // -- bind_or_recover: lifecycle / stale-socket handling --------------

    fn temp_socket_path(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jamail-daemon-test-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("jamaild.sock")
    }

    #[test]
    fn bind_or_recover_refuses_to_start_when_another_daemon_is_live() {
        let path = temp_socket_path("live");
        let _first = UnixListener::bind(&path).unwrap();
        let err = bind_or_recover(&path).unwrap_err();
        assert!(err.to_string().contains("already"));
    }

    #[test]
    fn bind_or_recover_cleans_up_a_stale_socket_file() {
        let path = temp_socket_path("stale");
        {
            let listener = UnixListener::bind(&path).unwrap();
            drop(listener); // leaves the socket *file* behind, nothing listening
        }
        assert!(path.exists());
        let listener = bind_or_recover(&path).expect("should recover from stale socket");
        drop(listener);
    }

    #[test]
    fn bind_or_recover_binds_cleanly_when_no_file_exists() {
        let dir = std::env::temp_dir().join(format!(
            "jamail-daemon-test-fresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jamaild.sock");
        assert!(!path.exists());
        let listener = bind_or_recover(&path).unwrap();
        drop(listener);
    }
}
