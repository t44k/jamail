use crate::config::Account;
use crate::db::MailDb;
use crate::mail::MailClient;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

pub enum SyncEvent {
    Syncing,
    Progress(usize, usize),
    Complete(usize),
    Error(String),
}

/// Spawn a background thread that syncs mail, then waits for changes via IDLE
/// (or polls every 60s), and repeats. Reconnects automatically on error.
pub fn spawn_sync_thread(account: Account, account_name: String, tx: Sender<SyncEvent>) {
    thread::spawn(move || sync_loop(account, account_name, tx));
}

fn sync_loop(account: Account, account_name: String, tx: Sender<SyncEvent>) {
    loop {
        let db = match MailDb::open() {
            Ok(db) => db,
            Err(e) => {
                if tx.send(SyncEvent::Error(format!("DB: {}", e))).is_err() {
                    return;
                }
                thread::sleep(Duration::from_secs(60));
                continue;
            }
        };

        if tx.send(SyncEvent::Syncing).is_err() {
            return;
        }

        let mut client = match MailClient::connect(&account) {
            Ok(c) => c,
            Err(e) => {
                if tx.send(SyncEvent::Error(format!("{}", e))).is_err() {
                    return;
                }
                thread::sleep(Duration::from_secs(60));
                continue;
            }
        };

        // Inner loop: sync → wait for changes → repeat
        // Breaks on connection error to trigger reconnect
        loop {
            match client.sync_inbox(&db, &account_name, &|done, total| {
                let _ = tx.send(SyncEvent::Progress(done, total));
            }) {
                Ok(count) => {
                    if tx.send(SyncEvent::Complete(count)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(SyncEvent::Error(format!("{}", e)));
                    break;
                }
            }

            // Wait for new mail via IDLE, or timeout after 60s.
            // If IDLE is supported, the server pushes a notification immediately
            // when new mail arrives. Otherwise this just sleeps 60s.
            client.wait_for_changes(Duration::from_secs(60));
        }

        // Connection lost — wait before reconnecting
        thread::sleep(Duration::from_secs(5));
    }
}
