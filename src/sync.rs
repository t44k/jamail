use crate::config::JamailAccount;
use crate::db::MailDb;
use crate::mail::{FolderInfo, MailClient};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

pub struct MarkSeenRequest {
    pub folder: String,
    pub uid: u32,
}

/// Which local-message flow an `UploadRequest` belongs to. Both share the
/// same upload machinery (APPEND to a configured IMAP folder); the kind is
/// only used to route the completion event back to the right UI state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadKind {
    Sent,
    Draft,
}

/// A request to APPEND a raw message to a remote folder — used for
/// uploading a copy of a sent message, or a newly-saved draft, to the
/// account's configured `sent_folder`/`draft_folder`.
pub struct UploadRequest {
    pub kind: UploadKind,
    /// The `local_messages.id` this upload is for, so the result can be
    /// applied back to the right row.
    pub local_id: i64,
    pub folder: String,
    pub raw_message: Vec<u8>,
}

pub enum SyncEvent {
    Syncing(String),
    Progress(String, usize, usize),
    FolderComplete(String, usize),
    AllComplete,
    Error(String),
    FoldersLoaded(Vec<FolderInfo>),
    FlagsChanged(String),
    UploadComplete(UploadKind, i64),
    UploadError(UploadKind, i64, String),
}

pub struct SyncControl {
    pub current_folder: RwLock<String>,
    pub shutdown: AtomicBool,
    pub force_reconnect: AtomicBool,
    pub folder_filter: RwLock<Vec<String>>,
    pub mark_seen_queue: Mutex<Vec<MarkSeenRequest>>,
    pub upload_queue: Mutex<Vec<UploadRequest>>,
}

impl SyncControl {
    pub fn new(initial_folder: &str) -> Self {
        Self {
            current_folder: RwLock::new(initial_folder.to_string()),
            shutdown: AtomicBool::new(false),
            force_reconnect: AtomicBool::new(false),
            folder_filter: RwLock::new(vec![initial_folder.to_string()]),
            mark_seen_queue: Mutex::new(Vec::new()),
            upload_queue: Mutex::new(Vec::new()),
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
    let mut quick_reconnect = false;

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

                // Apply the account's configured folder order deterministically
                // (see `mail::order_folders`); when unconfigured, skip Gmail's
                // huge/duplicate special folders before falling back to the
                // default INBOX-first alphabetical order.
                let sync_source: Vec<FolderInfo> = if account.folders.is_none() {
                    let skip = ["[Gmail]/All Mail", "[Gmail]/Spam", "[Gmail]/Important"];
                    folders
                        .iter()
                        .filter(|f| !skip.contains(&f.name.as_str()))
                        .cloned()
                        .collect()
                } else {
                    folders.clone()
                };
                let filtered: Vec<String> = crate::mail::order_folders(
                    account.folders.as_deref(),
                    &sync_source,
                    account.show_unlisted_folders,
                )
                .into_iter()
                .map(|f| f.name)
                .collect();

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

                        // Sync flags from server for this folder
                        match client.sync_flags(&db, &account_name, folder) {
                            Ok(true) => {
                                let _ = tx.send(SyncEvent::FlagsChanged(folder.clone()));
                            }
                            Ok(false) => {}
                            Err(e) => {
                                let _ = tx
                                    .send(SyncEvent::Error(format!("Flag sync {}: {}", folder, e)));
                            }
                        }
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

            // Drain mark-seen queue and apply on server
            if let Ok(mut queue) = control.mark_seen_queue.lock() {
                let requests: Vec<MarkSeenRequest> = queue.drain(..).collect();
                drop(queue);

                if !requests.is_empty() {
                    let mut by_folder: HashMap<String, Vec<u32>> = HashMap::new();
                    for req in requests {
                        by_folder.entry(req.folder).or_default().push(req.uid);
                    }
                    for (folder, uids) in &by_folder {
                        if let Err(e) = client
                            .select_folder(folder)
                            .and_then(|_| client.mark_seen(uids))
                        {
                            let _ =
                                tx.send(SyncEvent::Error(format!("Mark seen {}: {}", folder, e)));
                        }
                    }
                }
            }

            // Drain pending Sent/Draft uploads and APPEND them on the server.
            if let Ok(mut queue) = control.upload_queue.lock() {
                let requests: Vec<UploadRequest> = queue.drain(..).collect();
                drop(queue);

                for req in requests {
                    let flags: &[imap::types::Flag<'_>] = match req.kind {
                        UploadKind::Sent => &[imap::types::Flag::Seen],
                        UploadKind::Draft => &[imap::types::Flag::Draft, imap::types::Flag::Seen],
                    };
                    match client.append_message(&req.folder, &req.raw_message, flags) {
                        Ok(()) => {
                            let _ = tx.send(SyncEvent::UploadComplete(req.kind, req.local_id));
                        }
                        Err(e) => {
                            let _ = tx.send(SyncEvent::UploadError(
                                req.kind,
                                req.local_id,
                                e.to_string(),
                            ));
                        }
                    }
                }
            }

            // Check force_reconnect before IDLE
            if control.force_reconnect.swap(false, Ordering::Relaxed) {
                let _ = tx.send(SyncEvent::Error("Reconnecting after wake".to_string()));
                quick_reconnect = true;
                break;
            }

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
            let before_idle = std::time::Instant::now();
            client.wait_for_changes(Duration::from_secs(60));

            // If IDLE returned after much longer than the timeout, sleep/wake likely occurred
            if before_idle.elapsed() > Duration::from_secs(90) {
                let _ = tx.send(SyncEvent::Error("Reconnecting after sleep".to_string()));
                quick_reconnect = true;
                break;
            }

            // Check force_reconnect after IDLE
            if control.force_reconnect.swap(false, Ordering::Relaxed) {
                let _ = tx.send(SyncEvent::Error("Reconnecting after wake".to_string()));
                quick_reconnect = true;
                break;
            }
        }

        // Connection lost — wait before reconnecting (skip delay after wake detection)
        if !quick_reconnect {
            thread::sleep(Duration::from_secs(5));
        }
        quick_reconnect = false;
    }
}
