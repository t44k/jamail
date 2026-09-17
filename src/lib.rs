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
//! - `jacal` (src/bin/jacal.rs) — the calendar terminal UI, a second client
//!   of `jamaild` over the same IPC protocol.
//! - `jadav` (src/bin/jadav.rs) — a standalone, self-hosted CalDAV server
//!   daemon (see [`jadav`]) meant to run on a server host rather than next
//!   to the TUIs; it shares this crate's iCalendar, HTTP and CalDAV code
//!   but has its own store and no IPC socket.
//!
//! See `ipc` and `daemon` module docs for the protocol/lifecycle details.

pub mod app;
pub mod calapp;
pub mod caldav;
pub mod caldav_server;
pub mod calendar;
pub mod calnotify;
pub mod calsync;
pub mod config;
pub mod daemon;
pub mod db;
pub mod goauth;
pub mod httpc;
pub mod ipc;
pub mod jadav;
pub mod mail;
pub mod notify;
pub mod smtp;
pub mod sync;
#[allow(dead_code)]
pub mod theme;
pub mod thread;
