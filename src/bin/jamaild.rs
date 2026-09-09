//! `jamaild` — the jamail background daemon.
//!
//! Owns every configured account's IMAP connection/sync loop, writes to the
//! shared local cache, and delivers desktop notifications. Talks to the
//! `jamail` foreground client over the IPC protocol documented in
//! `jamail::ipc`. See that module and `jamail::daemon` for the full
//! protocol/lifecycle/compatibility contract.
//!
//! Designed to run under `systemd --user` (see `contrib/systemd/jamaild.service`)
//! but works identically started directly from a shell — `jamail` will even
//! auto-spawn one if none is reachable (see `jamail::daemon::try_autostart`).
//!
//! Exits non-zero with a message on stderr if it can't load config, can't
//! bind its socket, or another `jamaild` is already running.

fn main() {
    if let Err(err) = jamail::daemon::run() {
        eprintln!("jamaild: {:?}", err);
        std::process::exit(1);
    }
}
