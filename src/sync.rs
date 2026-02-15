use crate::config::JamailAccount;
use crate::db::MailDb;
use crate::mail::{FolderInfo, MailClient};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

pub enum SyncEvent {
    Syncing(String),
    Progress(String, usize, usize),
    FolderComplete(String, usize),
    AllComplete,
    Error(String),
    FoldersLoaded(Vec<FolderInfo>),
}

pub struct SyncControl {
    pub current_folder: RwLock<String>,
    pub shutdown: AtomicBool,
    pub folder_filter: RwLock<Vec<String>>,
}

impl SyncControl {
    pub fn new(initial_folder: &str) -> Self {
        Self {
            current_folder: RwLock::new(initial_folder.to_string()),
            shutdown: AtomicBool::new(false),
            folder_filter: RwLock::new(vec![initial_folder.to_string()]),
        }
    }
}

pub fn spawn_sync_thread(
    account: JamailAccount,
    account_name: String,
    control: Arc<SyncControl>,
    tx: Sender<SyncEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || sync_loop(account, account_name, control, tx))
}

fn sync_loop(
    account: JamailAccount,
    account_name: String,
    control: Arc<SyncControl>,
    tx: Sender<SyncEvent>,
) {
    loop {
        if control.shutdown.load(Ordering::Relaxed) {
            return;
        }

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

        if tx
            .send(SyncEvent::Syncing("connecting".to_string()))
            .is_err()
        {
            return;
        }

        let mut client = match MailClient::connect(&account) {
            Ok(c) => c,
            Err(e) => {
                if tx.send(SyncEvent::Error(format!("{}", e))).is_err() {
                    return;
                }
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        // Fetch folder list from IMAP
        match client.list_folders() {
            Ok(folders) => {
                let _ = db.store_folders(&account_name, &folders);

                // Apply folder filter from config, prioritizing INBOX first
                let filtered: Vec<String> = if let Some(ref filter) = account.folders {
                    // Use config ordering, keep only folders that exist on server
                    let server_names: std::collections::HashSet<&str> =
                        folders.iter().map(|f| f.name.as_str()).collect();
                    filter
                        .iter()
                        .filter(|f| server_names.contains(f.as_str()))
                        .cloned()
                        .collect()
                } else {
                    // No explicit filter: skip [Gmail]/All Mail and [Gmail]/Spam
                    // (they're huge and duplicate content from other folders)
                    let skip = ["[Gmail]/All Mail", "[Gmail]/Spam", "[Gmail]/Important"];
                    // Put INBOX first, then the rest alphabetically
                    let mut names: Vec<String> = folders
                        .iter()
                        .map(|f| f.name.clone())
                        .filter(|n| !skip.contains(&n.as_str()))
                        .collect();
                    names.sort();
                    if let Some(pos) = names.iter().position(|n| n == "INBOX") {
                        let inbox = names.remove(pos);
                        names.insert(0, inbox);
                    }
                    names
                };

                if let Ok(mut ff) = control.folder_filter.write() {
                    *ff = filtered;
                }

                let _ = tx.send(SyncEvent::FoldersLoaded(folders));
            }
            Err(e) => {
                let _ = tx.send(SyncEvent::Error(format!("List folders: {}", e)));
            }
        }

        // Inner loop: sync all folders → IDLE on current → repeat
        loop {
            if control.shutdown.load(Ordering::Relaxed) {
                return;
            }

            let folders_to_sync: Vec<String> = control
                .folder_filter
                .read()
                .map(|f| f.clone())
                .unwrap_or_default();

            let mut had_error = false;
            for folder in &folders_to_sync {
                if control.shutdown.load(Ordering::Relaxed) {
                    return;
                }

                let _ = tx.send(SyncEvent::Syncing(folder.clone()));

                match client.sync_folder(&db, &account_name, folder, &|done, total| {
                    let _ = tx.send(SyncEvent::Progress(folder.clone(), done, total));
                }) {
                    Ok(count) => {
                        let _ = tx.send(SyncEvent::FolderComplete(folder.clone(), count));
                    }
                    Err(e) => {
                        let _ = tx.send(SyncEvent::Error(format!("{}: {}", folder, e)));
                        had_error = true;
                        break;
                    }
                }
            }

            if had_error {
                break; // Reconnect
            }

            let _ = tx.send(SyncEvent::AllComplete);

            // SELECT the current folder for IDLE
            let current = control
                .current_folder
                .read()
                .map(|f| f.clone())
                .unwrap_or_else(|_| "INBOX".to_string());

            // Need to SELECT the folder before IDLE
            if client.select_folder(&current).is_err() {
                break; // Connection issue
            }

            // IDLE waits for new mail or 60s timeout
            client.wait_for_changes(Duration::from_secs(60));
        }

        // Connection lost — wait before reconnecting
        thread::sleep(Duration::from_secs(5));
    }
}
