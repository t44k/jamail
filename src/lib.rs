//! jamail library crate: shared code for the two binaries.
//!
//! - `jamaild` (src/bin/jamaild.rs) — the background daemon. Owns the IMAP
//!   connections, the sync loops, the local SQLite cache writes, and
//!   desktop-notification delivery. Suitable for `systemd --user`.
//! - `jamail` (src/bin/jamail.rs) — the foreground terminal UI. Reads the
//!   same on-disk cache and talks to `jamaild` over a small versioned IPC
//!   protocol (see [`ipc`]) for anything that needs a live IMAP session
//!   (folder-of-interest hints, mark-seen, Sent/Draft uploads) and for the
//!   background sync-progress/event stream.
//!
//! See `ipc` and `daemon` module docs for the protocol/lifecycle details.

pub mod app;
pub mod config;
pub mod daemon;
pub mod db;
pub mod ipc;
pub mod mail;
pub mod notify;
pub mod smtp;
pub mod sync;
#[allow(dead_code)]
pub mod theme;
pub mod thread;
