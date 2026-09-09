//! Real local smoke test for the `jamaild`/`jamail` daemon/client split.
//!
//! This starts the actual compiled `jamaild` binary (`env!("CARGO_BIN_EXE_jamaild")`),
//! talks to it over a real Unix domain socket using the same `jamail::ipc`
//! client code the `jamail` TUI uses internally, and confirms:
//!
//! - a real RPC round trip (handshake, `Ping`/`Pong`, `SetCurrentFolder`,
//!   `MarkSeen`, an unknown-account error, and graceful `Shutdown`);
//! - the daemon actually exits and removes its socket file after
//!   `Shutdown`;
//! - the real `jamail` binary's own startup path (its TTY guard, since this
//!   sandboxed test environment has no interactive terminal to hand it —
//!   see the limitation note below);
//! - the daemon's real notification-dispatch code path runs end-to-end.
//!
//! ## A deliberate scope limitation, and why
//!
//! Driving `jamaild` all the way to a real `SyncEvent::FolderComplete` (and
//! from there to a real desktop notification) would require a fake IMAP
//! server good enough to fool `mail::MailClient::connect`. That function
//! always connects in `imap`'s `AutoTls` mode: port 993 gets immediate TLS,
//! any other port requires a successful `STARTTLS` upgrade, and either way
//! the client validates the peer certificate. Satisfying that from a test
//! fixture would mean either weakening certificate validation in
//! production connection code (not something we're willing to do just to
//! make a test pass) or standing up a real trusted-CA TLS server in the
//! test harness, which is disproportionate to what this test needs to
//! prove. So the account configured below points at a closed local port:
//! `jamaild` starts, stays fully IPC-responsive, and that account's sync
//! loop harmlessly retries forever in the background — which is itself a
//! real, useful property to verify (the daemon does not wedge or become
//! unresponsive when IMAP is unreachable).
//!
//! The notification *decision* — `jamail::daemon::dispatch_sync_event`,
//! which is what "the daemon owns notification delivery" means in practice
//! — is exercised directly below with a synthetic `SyncEvent` and the real
//! default notifier (best-effort `notify-send`, silently a no-op if it
//! isn't installed, exactly like `notify::notify_new_mail`'s own existing
//! test). That's the real production function `jamaild`'s account event
//! pump calls in the field (see `daemon.rs`'s `spawn_pump`), just invoked
//! directly rather than by waiting on a real IMAP session. The precise
//! decision *logic* (fires iff configured and count>0, and does so with
//! zero clients connected — the crux of "the daemon owns this, not the
//! client") is asserted with a recording spy by `jamail::daemon`'s own unit
//! tests, in particular
//! `daemon::tests::dispatch_sync_event_fires_with_zero_clients_connected`.

use jamail::ipc;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct TestEnv {
    home: PathBuf,
    socket_path: PathBuf,
}

impl TestEnv {
    fn new(label: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "jamail-smoke-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        std::fs::create_dir_all(home.join(".config/jamail")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/jamail")).unwrap();
        let socket_path = base.join("jamaild.sock");

        // Minimal config: one account pointed at a local port nothing
        // listens on (see module docs for why), so the sync loop fails
        // fast and harmlessly instead of hanging or needing a real server.
        let config = "accounts:\n  test:\n    default: true\n    email: test@example.invalid\n    imap:\n      host: 127.0.0.1\n      port: 1\n      login: test@example.invalid\n      auth:\n        type: password\n        value: unused\n";
        std::fs::write(home.join(".config/jamail/config.yaml"), config).unwrap();

        Self { home, socket_path }
    }

    fn spawn_jamaild(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_jamaild"))
            .env("HOME", &self.home)
            .env("JAMAIL_SOCKET", &self.socket_path)
            .env_remove("XDG_RUNTIME_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn real jamaild binary")
    }
}

fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ipc::ping(path, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Kill a still-running child so a failing assertion earlier in a test
/// doesn't leak a real `jamaild` process.
/// Owns the spawned `jamaild` and kills it on drop — including on an early
/// return from a failed assertion — so a failing test never leaks a real
/// background process. `kill()`/`wait()` on an already-exited child is a
/// harmless no-op (the graceful-shutdown test explicitly waits for exit
/// before this drops, so this is a no-op there too).
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn real_jamaild_answers_rpc_over_a_real_unix_socket_and_shuts_down_cleanly() {
    let env = TestEnv::new("rpc");
    let mut child = KillOnDrop(env.spawn_jamaild());

    assert!(
        wait_for_socket(&env.socket_path, Duration::from_secs(10)),
        "real jamaild did not become reachable over its Unix socket in time"
    );

    // Full real handshake + a handful of real requests, using the exact
    // same jamail::ipc wire code the TUI client uses (not a reimplementation).
    let mut stream = std::os::unix::net::UnixStream::connect(&env.socket_path)
        .expect("connect to real jamaild socket");
    ipc::handshake(&mut stream).expect("protocol handshake with real jamaild");

    ipc::write_message(&mut stream, &ipc::Request::Ping).unwrap();
    let resp: ipc::ServerMessage = ipc::read_message(&mut stream).unwrap();
    assert!(matches!(
        resp,
        ipc::ServerMessage::Response(ipc::Response::Pong)
    ));

    ipc::write_message(
        &mut stream,
        &ipc::Request::SetCurrentFolder {
            account: "test".to_string(),
            folder: "INBOX".to_string(),
        },
    )
    .unwrap();
    let resp: ipc::ServerMessage = ipc::read_message(&mut stream).unwrap();
    assert!(matches!(
        resp,
        ipc::ServerMessage::Response(ipc::Response::Ok)
    ));

    ipc::write_message(
        &mut stream,
        &ipc::Request::MarkSeen {
            account: "test".to_string(),
            folder: "INBOX".to_string(),
            uid: 1,
        },
    )
    .unwrap();
    let resp: ipc::ServerMessage = ipc::read_message(&mut stream).unwrap();
    assert!(matches!(
        resp,
        ipc::ServerMessage::Response(ipc::Response::Ok)
    ));

    // Unknown account: a real Response::Error over the real socket, and the
    // connection must stay usable afterwards.
    ipc::write_message(
        &mut stream,
        &ipc::Request::SetCurrentFolder {
            account: "no-such-account".to_string(),
            folder: "INBOX".to_string(),
        },
    )
    .unwrap();
    let resp: ipc::ServerMessage = ipc::read_message(&mut stream).unwrap();
    assert!(matches!(
        resp,
        ipc::ServerMessage::Response(ipc::Response::Error { .. })
    ));

    // Graceful shutdown lifecycle: Shutdown -> Ok -> process actually exits
    // -> socket file actually removed. This is the real SIGTERM-equivalent
    // path `systemctl --user stop jamaild` relies on, triggered here via
    // the protocol instead of a signal for a deterministic test.
    ipc::write_message(&mut stream, &ipc::Request::Shutdown).unwrap();
    let resp: ipc::ServerMessage = ipc::read_message(&mut stream).unwrap();
    assert!(matches!(
        resp,
        ipc::ServerMessage::Response(ipc::Response::Ok)
    ));
    drop(stream);

    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.is_some(), "real jamaild did not exit after Shutdown");
    assert!(
        !env.socket_path.exists(),
        "real jamaild left its socket file behind after a graceful Shutdown"
    );
}

#[test]
fn real_jamail_binary_refuses_to_run_without_a_tty() {
    // No jamaild needed here — jamail's TTY guard runs before any daemon
    // connection is attempted, so this alone is a real (if narrow)
    // invocation of the real jamail binary's startup path in this
    // sandboxed, non-interactive test environment.
    let env = TestEnv::new("jamail-no-tty");
    let output = Command::new(env!("CARGO_BIN_EXE_jamail"))
        .env("HOME", &env.home)
        .env("JAMAIL_SOCKET", &env.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn real jamail binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("interactive terminal"),
        "unexpected stderr from real jamail binary: {}",
        stderr
    );
}

#[test]
fn daemon_notification_dispatch_runs_the_real_production_path() {
    use jamail::config::{AuthConfig, ImapConfig, JamailAccount};
    use jamail::sync::SyncEvent;

    let account_cfg = JamailAccount {
        default: false,
        email: "test@example.invalid".to_string(),
        display_name: None,
        imap: ImapConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            login: "test@example.invalid".to_string(),
            auth: AuthConfig {
                auth_type: "password".to_string(),
                value: "unused".to_string(),
            },
        },
        smtp: None,
        folders: None,
        show_unlisted_folders: false,
        senders: None,
        sent_folder: None,
        draft_folder: None,
        notify_folders: Some(vec!["INBOX".to_string()]),
        color: None,
    };

    // The exact function jamaild's per-account event pump calls in
    // production (see daemon.rs's spawn_pump), with the real default
    // notifier (best-effort notify-send — silently a no-op if it isn't
    // installed, same as notify::notify_new_mail's own test). Must not
    // panic either way.
    let notifier = jamail::daemon::default_notifier();
    jamail::daemon::dispatch_sync_event(
        "test",
        &account_cfg,
        &SyncEvent::FolderComplete("INBOX".to_string(), 2),
        notifier.as_ref(),
    );
}

#[test]
fn notification_ownership_stays_in_the_daemon_binary_not_the_client() {
    // Structural guardrail complementing the dynamic tests above: the
    // jamail client must never itself decide or fire a notification — that
    // decision belongs entirely to jamaild (see jamail::daemon::dispatch_sync_event).
    let client_src = include_str!("../src/bin/jamail.rs");
    assert!(
        !client_src.contains("notify::") && !client_src.contains("notify_new_mail"),
        "jamail (the client binary) must not reference the notify module — \
         notification delivery is owned exclusively by jamaild"
    );
}
