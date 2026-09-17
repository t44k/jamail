use crate::config::JamailAccount;
use crate::db::MailDb;
use crate::mail::{ConnectionAbort, FolderInfo, MailClient};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

pub struct MarkSeenRequest {
    pub folder: String,
    pub uid: u32,
    /// How many times the server rejected this one; dropped after a few.
    pub attempts: u32,
}

/// A queued star/unstar (`\\Flagged`) for one message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagRequest {
    pub folder: String,
    pub uid: u32,
    pub flagged: bool,
    pub attempts: u32,
}

/// Which local-message flow an `UploadRequest` belongs to. Both share the
/// same upload machinery (APPEND to a configured IMAP folder); the kind is
/// only used to route the completion event back to the right UI state.
///
/// Also used as the wire representation in the `jamaild`/`jamail` IPC
/// protocol's `EnqueueUpload` request and `UploadComplete`/`UploadError`
/// events (see `ipc` module docs) — hence the `Serialize`/`Deserialize`
/// derives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Run a full pass now (jamail's `u`), cutting the IDLE period short.
    pub force_sync: AtomicBool,
    /// `(folder, uid)` the user read in jamail recently, with when: the flag
    /// sync must not flip these back to unread on a stale server answer
    /// while the `\Seen` store is still in flight (see
    /// `MailClient::sync_flags`). Entries expire after
    /// [`RECENTLY_SEEN_TTL`].
    recently_seen: Mutex<HashMap<(String, u32), std::time::Instant>>,
    /// Cuts the live IMAP connection from another thread — the daemon's
    /// stall watchdog and `ForceReconnect` use it (see `MailClient::abort_handle`).
    abort: Mutex<Option<ConnectionAbort>>,
    /// Unix seconds of the sync loop's last sign of life (every IDLE round,
    /// every fetch batch). The watchdog compares it with [`STALL_TIMEOUT`].
    last_progress: AtomicU64,
    /// Stars and unstars waiting to be stored on the server.
    pub flag_queue: Mutex<Vec<FlagRequest>>,
    /// `(folder, uid)` → the star the user set locally, with when: shields
    /// the local choice from a stale server answer the way `recently_seen`
    /// does for reads.
    recently_flagged: Mutex<HashMap<(String, u32), (bool, std::time::Instant)>>,
}

/// How long a locally read message is shielded from a stale "unseen".
pub const RECENTLY_SEEN_TTL: Duration = Duration::from_secs(600);
/// One IDLE round: short enough that a queued mark-seen, upload or `u`
/// request is acted on within seconds, long enough not to spam the server.
pub const IDLE_CHUNK: Duration = Duration::from_secs(10);
/// How long the daemon idles on the current folder before it polls every
/// other folder for new mail again.
pub const IDLE_PHASE: Duration = Duration::from_secs(60);
/// How often the flags (and pruning) of *every* folder are refreshed; the
/// current folder's are refreshed on every pass.
pub const FULL_FLAG_SYNC_INTERVAL: Duration = Duration::from_secs(300);
/// Give up on a mark-seen the server keeps rejecting after this many tries.
pub const MARK_SEEN_MAX_ATTEMPTS: u32 = 3;
/// A sync loop silent for this long is stuck on a dead connection (its
/// IDLE rounds are 10 s and every fetch batch reports); the daemon's
/// watchdog then cuts the socket so the blocked read fails and the loop
/// reconnects. Matches `mail::SESSION_READ_TIMEOUT`.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(300);

impl SyncControl {
    pub fn new(initial_folder: &str) -> Self {
        Self {
            current_folder: RwLock::new(initial_folder.to_string()),
            shutdown: AtomicBool::new(false),
            force_reconnect: AtomicBool::new(false),
            folder_filter: RwLock::new(vec![initial_folder.to_string()]),
            mark_seen_queue: Mutex::new(Vec::new()),
            upload_queue: Mutex::new(Vec::new()),
            force_sync: AtomicBool::new(false),
            recently_seen: Mutex::new(HashMap::new()),
            abort: Mutex::new(None),
            last_progress: AtomicU64::new(now_secs()),
            flag_queue: Mutex::new(Vec::new()),
            recently_flagged: Mutex::new(HashMap::new()),
        }
    }

    /// Note that the sync loop is alive.
    pub fn touch_progress(&self) {
        self.last_progress.store(now_secs(), Ordering::Relaxed);
    }

    /// Seconds since the sync loop last reported progress.
    pub fn seconds_since_progress(&self) -> u64 {
        now_secs().saturating_sub(self.last_progress.load(Ordering::Relaxed))
    }

    pub(crate) fn set_abort(&self, handle: Option<ConnectionAbort>) {
        if let Ok(mut slot) = self.abort.lock() {
            *slot = handle;
        }
    }

    /// Cut the live IMAP connection so a read the sync thread is blocked in
    /// fails and the loop reconnects. `false` when there is no connection
    /// (or no handle on it) to cut.
    pub fn abort_connection(&self) -> bool {
        match self.abort.lock() {
            Ok(slot) => match slot.as_ref() {
                Some(h) => {
                    h.abort();
                    true
                }
                None => false,
            },
            Err(_) => false,
        }
    }

    /// Queue a `\Seen` store for `(folder, uid)` and shield it from a
    /// stale server answer meanwhile.
    pub fn enqueue_mark_seen(&self, folder: String, uid: u32) {
        if let Ok(mut seen) = self.recently_seen.lock() {
            let now = std::time::Instant::now();
            seen.retain(|_, at| now.duration_since(*at) < RECENTLY_SEEN_TTL);
            seen.insert((folder.clone(), uid), now);
        }
        if let Ok(mut q) = self.mark_seen_queue.lock() {
            q.push(MarkSeenRequest {
                folder,
                uid,
                attempts: 0,
            });
        }
    }

    /// Queue a star/unstar for `(folder, uid)` and remember the choice so a
    /// stale server answer cannot undo it meanwhile.
    pub fn enqueue_set_flagged(&self, folder: String, uid: u32, flagged: bool) {
        if let Ok(mut recent) = self.recently_flagged.lock() {
            let now = std::time::Instant::now();
            recent.retain(|_, (_, at)| now.duration_since(*at) < RECENTLY_SEEN_TTL);
            recent.insert((folder.clone(), uid), (flagged, now));
        }
        if let Ok(mut q) = self.flag_queue.lock() {
            // A newer choice for the same message replaces an older queued one.
            q.retain(|r| !(r.folder == folder && r.uid == uid));
            q.push(FlagRequest {
                folder,
                uid,
                flagged,
                attempts: 0,
            });
        }
    }

    /// The star the user set on `(folder, uid)` within [`RECENTLY_SEEN_TTL`],
    /// if any — it overrides the server's answer in the flag sync.
    pub fn recent_flag_override(&self, folder: &str, uid: u32) -> Option<bool> {
        self.recently_flagged.lock().ok().and_then(|recent| {
            recent
                .get(&(folder.to_string(), uid))
                .filter(|(_, at)| at.elapsed() < RECENTLY_SEEN_TTL)
                .map(|(flagged, _)| *flagged)
        })
    }

    /// Whether the user read `(folder, uid)` in jamail within
    /// [`RECENTLY_SEEN_TTL`].
    pub fn recently_seen(&self, folder: &str, uid: u32) -> bool {
        self.recently_seen
            .lock()
            .map(|seen| {
                seen.get(&(folder.to_string(), uid))
                    .is_some_and(|at| at.elapsed() < RECENTLY_SEEN_TTL)
            })
            .unwrap_or(false)
    }

    fn has_pending_actions(&self) -> bool {
        let non_empty =
            |q: &Mutex<Vec<MarkSeenRequest>>| q.lock().map(|q| !q.is_empty()).unwrap_or(false);
        non_empty(&self.mark_seen_queue)
            || self
                .flag_queue
                .lock()
                .map(|q| !q.is_empty())
                .unwrap_or(false)
            || self
                .upload_queue
                .lock()
                .map(|q| !q.is_empty())
                .unwrap_or(false)
    }
}

/// Apply everything jamail queued — `\Seen` stores and Sent/Draft
/// uploads — on the live connection. A mark-seen the server rejects goes
/// back on the queue for another try (up to [`MARK_SEEN_MAX_ATTEMPTS`]);
/// an upload's outcome is reported once. `Err` means the connection itself
/// failed and the caller should reconnect (the queues keep their items).
fn apply_pending_actions(
    client: &mut MailClient,
    control: &SyncControl,
    tx: &Sender<SyncEvent>,
) -> Result<(), ()> {
    let requests: Vec<MarkSeenRequest> = control
        .mark_seen_queue
        .lock()
        .map(|mut q| q.drain(..).collect())
        .unwrap_or_default();
    if !requests.is_empty() {
        let mut by_folder: HashMap<String, Vec<MarkSeenRequest>> = HashMap::new();
        for req in requests {
            by_folder.entry(req.folder.clone()).or_default().push(req);
        }
        for (folder, reqs) in by_folder {
            let uids: Vec<u32> = reqs.iter().map(|r| r.uid).collect();
            if let Err(e) = client
                .select_folder(&folder)
                .and_then(|_| client.mark_seen(&uids))
            {
                let _ = tx.send(SyncEvent::Error(format!("Mark seen {}: {}", folder, e)));
                let mut gave_up = false;
                if let Ok(mut q) = control.mark_seen_queue.lock() {
                    for mut r in reqs {
                        r.attempts += 1;
                        if r.attempts < MARK_SEEN_MAX_ATTEMPTS {
                            q.push(r);
                        } else {
                            gave_up = true;
                        }
                    }
                }
                if gave_up {
                    let _ = tx.send(SyncEvent::Error(format!(
                        "Mark seen {}: giving up after {} attempts",
                        folder, MARK_SEEN_MAX_ATTEMPTS
                    )));
                }
                // A rejected STORE usually means the connection is gone;
                // let the caller reconnect and retry the rest.
                return Err(());
            }
        }
    }

    let stars: Vec<FlagRequest> = control
        .flag_queue
        .lock()
        .map(|mut q| q.drain(..).collect())
        .unwrap_or_default();
    if !stars.is_empty() {
        let mut groups: HashMap<(String, bool), Vec<FlagRequest>> = HashMap::new();
        for req in stars {
            groups
                .entry((req.folder.clone(), req.flagged))
                .or_default()
                .push(req);
        }
        for ((folder, flagged), reqs) in groups {
            let uids: Vec<u32> = reqs.iter().map(|r| r.uid).collect();
            if let Err(e) = client
                .select_folder(&folder)
                .and_then(|_| client.set_flagged(&uids, flagged))
            {
                let _ = tx.send(SyncEvent::Error(format!("Star {}: {}", folder, e)));
                if let Ok(mut q) = control.flag_queue.lock() {
                    for mut r in reqs {
                        r.attempts += 1;
                        if r.attempts < MARK_SEEN_MAX_ATTEMPTS {
                            q.push(r);
                        }
                    }
                }
                return Err(());
            }
        }
    }

    let uploads: Vec<UploadRequest> = control
        .upload_queue
        .lock()
        .map(|mut q| q.drain(..).collect())
        .unwrap_or_default();
    for req in uploads {
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
    Ok(())
}

pub fn spawn_sync_thread(
    account: JamailAccount,
    account_name: String,
    control: Arc<SyncControl>,
    tx: Sender<SyncEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || sync_loop(account, account_name, control, tx))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sync_loop(
    account: JamailAccount,
    account_name: String,
    control: Arc<SyncControl>,
    tx: Sender<SyncEvent>,
) {
    let mut quick_reconnect = false;
    // Consecutive failed connection attempts: back off 5 s → 60 s so an
    // outage is not hammered, reset by any successful connection.
    let mut connect_failures: u32 = 0;

    loop {
        if control.shutdown.load(Ordering::Relaxed) {
            return;
        }
        control.set_abort(None);
        control.touch_progress();

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
            Ok(c) => {
                connect_failures = 0;
                control.set_abort(c.abort_handle());
                control.touch_progress();
                c
            }
            Err(e) => {
                if tx.send(SyncEvent::Error(format!("{}", e))).is_err() {
                    return;
                }
                let delay = 5u64.saturating_mul(1u64 << connect_failures.min(4)).min(60);
                connect_failures = connect_failures.saturating_add(1);
                thread::sleep(Duration::from_secs(delay));
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

        // Inner loop: apply queued actions → sync folders → IDLE on the
        // current folder in short rounds → repeat. Every round of IDLE ends
        // within IDLE_CHUNK, so a mark-seen, an upload or a `u` request is
        // acted on within seconds; new mail in the current folder ends the
        // round at once; every IDLE_PHASE the other folders are polled too.
        let mut last_full_flags = std::time::Instant::now() - FULL_FLAG_SYNC_INTERVAL;
        'connected: loop {
            if control.shutdown.load(Ordering::Relaxed) {
                return;
            }
            control.touch_progress();

            // Pending user actions first: a message read in jamail must be
            // \Seen on the server before this pass reads the flags back.
            if apply_pending_actions(&mut client, &control, &tx).is_err() {
                quick_reconnect = true;
                break 'connected;
            }

            let folders_to_sync: Vec<String> = control
                .folder_filter
                .read()
                .map(|f| f.clone())
                .unwrap_or_default();
            let current = control
                .current_folder
                .read()
                .map(|f| f.clone())
                .unwrap_or_else(|_| "INBOX".to_string());
            let full_flags = last_full_flags.elapsed() >= FULL_FLAG_SYNC_INTERVAL;

            let mut had_error = false;
            for folder in &folders_to_sync {
                if control.shutdown.load(Ordering::Relaxed) {
                    return;
                }

                let _ = tx.send(SyncEvent::Syncing(folder.clone()));

                match client.sync_folder(&db, &account_name, folder, &|done, total| {
                    control.touch_progress();
                    let _ = tx.send(SyncEvent::Progress(folder.clone(), done, total));
                }) {
                    Ok(count) => {
                        let _ = tx.send(SyncEvent::FolderComplete(folder.clone(), count));

                        // Flags (and pruning of moved-away mail): the folder
                        // on screen every pass, every folder every
                        // FULL_FLAG_SYNC_INTERVAL.
                        if full_flags || *folder == current {
                            let shield = |uid: u32| control.recently_seen(folder, uid);
                            let star = |uid: u32| control.recent_flag_override(folder, uid);
                            match client.sync_flags(&db, &account_name, folder, &shield, &star) {
                                Ok(true) => {
                                    let _ = tx.send(SyncEvent::FlagsChanged(folder.clone()));
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    let _ = tx.send(SyncEvent::Error(format!(
                                        "Flag sync {}: {}",
                                        folder, e
                                    )));
                                }
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
            if full_flags {
                last_full_flags = std::time::Instant::now();
            }

            if had_error {
                break 'connected; // Reconnect
            }

            let _ = tx.send(SyncEvent::AllComplete);

            if control.force_reconnect.swap(false, Ordering::Relaxed) {
                let _ = tx.send(SyncEvent::Error("Reconnecting after wake".to_string()));
                quick_reconnect = true;
                break 'connected;
            }

            // IDLE phase on the current folder, in short rounds.
            if client.select_folder(&current).is_err() {
                break 'connected; // Connection issue
            }
            let phase_start = std::time::Instant::now();
            loop {
                if control.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if control.force_sync.swap(false, Ordering::Relaxed) {
                    break; // full pass now
                }
                if control.force_reconnect.swap(false, Ordering::Relaxed) {
                    let _ = tx.send(SyncEvent::Error("Reconnecting after wake".to_string()));
                    quick_reconnect = true;
                    break 'connected;
                }
                if control.has_pending_actions()
                    && (apply_pending_actions(&mut client, &control, &tx).is_err()
                        || client.select_folder(&current).is_err())
                {
                    quick_reconnect = true;
                    break 'connected;
                }

                control.touch_progress();
                let before_idle = std::time::Instant::now();
                let outcome = client.wait_for_changes(IDLE_CHUNK);
                // Far longer than asked: the machine slept; the socket is
                // probably stale even if it looks alive.
                if before_idle.elapsed() > IDLE_CHUNK + Duration::from_secs(30) {
                    let _ = tx.send(SyncEvent::Error("Reconnecting after sleep".to_string()));
                    quick_reconnect = true;
                    break 'connected;
                }
                match outcome {
                    Err(e) => {
                        let _ = tx.send(SyncEvent::Error(format!("IDLE: {}; reconnecting", e)));
                        quick_reconnect = true;
                        break 'connected;
                    }
                    // New mail (or a change) in the folder on screen: fetch
                    // it now, along with everything else that is due.
                    Ok(imap::extensions::idle::WaitOutcome::MailboxChanged) => break,
                    Ok(imap::extensions::idle::WaitOutcome::TimedOut) => {}
                }
                if phase_start.elapsed() >= IDLE_PHASE {
                    break; // periodic poll of every folder
                }
            }
        }

        control.set_abort(None);
        // Connection lost — wait before reconnecting (skip delay after wake detection)
        if !quick_reconnect {
            thread::sleep(Duration::from_secs(5));
        }
        quick_reconnect = false;
    }
}

#[cfg(test)]
mod pending_action_tests {
    use super::*;

    #[test]
    fn a_locally_read_message_is_queued_and_shielded_then_expires_by_ttl() {
        let control = SyncControl::new("INBOX");
        assert!(!control.recently_seen("INBOX", 7));
        control.enqueue_mark_seen("INBOX".to_string(), 7);
        assert!(control.recently_seen("INBOX", 7));
        assert!(!control.recently_seen("Archive", 7), "shield is per folder");
        assert!(control.has_pending_actions());
        let queued: Vec<MarkSeenRequest> = control
            .mark_seen_queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            (queued[0].folder.as_str(), queued[0].uid, queued[0].attempts),
            ("INBOX", 7, 0)
        );
        assert!(!control.has_pending_actions());
        // Force an expired entry: shielding must not outlive the TTL.
        control.recently_seen.lock().unwrap().insert(
            ("INBOX".to_string(), 8),
            std::time::Instant::now() - RECENTLY_SEEN_TTL,
        );
        assert!(!control.recently_seen("INBOX", 8));
        assert!(
            control.recently_seen("INBOX", 7),
            "fresh entry still shielded"
        );
        // The next enqueue prunes the expired one.
        control.enqueue_mark_seen("INBOX".to_string(), 9);
        assert!(
            !control
                .recently_seen
                .lock()
                .unwrap()
                .contains_key(&("INBOX".to_string(), 8))
        );
    }
}

#[cfg(test)]
mod abort_tests {
    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn abort_connection_unblocks_a_read_and_progress_is_tracked() {
        let control = SyncControl::new("INBOX");
        assert!(!control.abort_connection(), "nothing to cut yet");
        assert!(control.seconds_since_progress() < 5);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (_server_side, _) = listener.accept().unwrap();
        control.set_abort(Some(ConnectionAbort(client.try_clone().unwrap())));

        // A reader blocked on the socket, like the sync thread inside IDLE
        // on a dead connection…
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 1];
            let mut c = client;
            c.read(&mut buf)
        });
        std::thread::sleep(Duration::from_millis(100));
        // …returns as soon as the watchdog cuts the connection.
        assert!(control.abort_connection());
        let outcome = reader.join().unwrap();
        assert!(
            matches!(outcome, Ok(0)) || outcome.is_err(),
            "read should end: {outcome:?}"
        );

        control
            .last_progress
            .store(now_secs() - STALL_TIMEOUT.as_secs() - 1, Ordering::Relaxed);
        assert!(control.seconds_since_progress() > STALL_TIMEOUT.as_secs());
        control.touch_progress();
        assert!(control.seconds_since_progress() < 5);
        control.set_abort(None);
        assert!(!control.abort_connection());
    }
}

#[cfg(test)]
mod star_queue_tests {
    use super::*;

    #[test]
    fn stars_are_queued_once_per_message_and_shield_the_local_choice() {
        let control = SyncControl::new("INBOX");
        assert_eq!(control.recent_flag_override("INBOX", 5), None);
        control.enqueue_set_flagged("INBOX".to_string(), 5, true);
        control.enqueue_set_flagged("INBOX".to_string(), 5, false);
        control.enqueue_set_flagged("INBOX".to_string(), 6, true);
        assert_eq!(control.recent_flag_override("INBOX", 5), Some(false));
        assert_eq!(control.recent_flag_override("INBOX", 6), Some(true));
        assert_eq!(control.recent_flag_override("Archive", 6), None);
        assert!(control.has_pending_actions());
        let queued: Vec<FlagRequest> = control
            .flag_queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap();
        assert_eq!(queued.len(), 2, "the newer choice replaced the older one");
        assert_eq!(
            queued.iter().find(|r| r.uid == 5).map(|r| r.flagged),
            Some(false)
        );
        assert!(!control.has_pending_actions());
    }
}
