//! `jamaild`-internal per-account calendar sync thread, mirroring
//! [`crate::sync`]'s IMAP sync thread shape: a [`CalSyncControl`] (shared
//! `Arc` state a client/daemon can nudge) plus a loop function spawned
//! once per configured account that has `caldav` set.
//!
//! ## Sync strategy
//!
//! CalDAV has no push/IDLE equivalent this client implements, so this is a
//! poll loop (`poll_interval_secs`, from config; default 5 minutes — see
//! [`crate::config::CalDavConfig`]). Each cycle, per discovered calendar:
//!
//! 1. Drain any queued local writes first ([`CalSyncControl::write_queue`]
//!    — pending create/update/delete from `jacal`, delivered via
//!    `ipc::Request::EnqueueCalendarMutation`) so local edits reach the
//!    server before the next read-back would otherwise overwrite them.
//! 2. Try [`crate::caldav::CalDavClient::sync_calendar`] with the stored
//!    sync-token; on [`crate::caldav::SyncOutcome::FullResyncRequired`],
//!    fall back to [`crate::caldav::CalDavClient::list_all_events`] and
//!    reconcile deletions by diffing against what's locally cached for
//!    that calendar.
//! 3. Upsert changed events and delete removed ones in the local cache,
//!    then persist the new sync-token/ctag.
//!
//! ## Conflict handling
//!
//! A write that hits [`crate::caldav::CalDavError::Conflict`] (stale
//! `ETag`, or a UID collision on create) is *not* retried automatically
//! and does *not* silently overwrite the server: the local row is marked
//! [`crate::db::CAL_STATUS_CONFLICT`] with the server's error text, stays
//! visible in `jacal` (flagged), and requires the user to look at the
//! current server copy and re-edit/discard — the same "never silently
//! clobber" stance IMAP mark-seen/upload conflicts don't even have to
//! think about, but calendar writes very much do (two people editing the
//! same event, `jacal` open in two places, etc).

use crate::caldav::{CalDavClient, CalDavError, SyncOutcome};
use crate::calendar::EventEdits;
use crate::config::JamailAccount;
use crate::db::{self, MailDb};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A queued local calendar write, delivered from `jacal` via
/// `ipc::Request::EnqueueCalendarMutation` and drained by this account's
/// calendar sync thread on its next cycle (it already owns a
/// `CalDavClient`, mirroring how `sync::SyncControl::upload_queue` is
/// drained by the IMAP sync thread instead of being handled inline in
/// `jamaild`'s request handler).
///
/// Also used verbatim as the wire representation in the `jamaild`/`jacal`
/// IPC protocol's `EnqueueCalendarMutation` request — hence the
/// `Serialize`/`Deserialize` derives (same pattern as `sync::UploadKind`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CalMutation {
    Create { local_id: i64 },
    Update { local_id: i64, edits: EventEdits },
    Delete { local_id: i64 },
}

pub enum CalSyncEvent {
    Syncing(String),
    CalendarSynced(String, usize),
    Error(String),
    MutationApplied(i64),
    MutationError(i64, String),
    MutationConflict(i64, String),
}

pub struct CalSyncControl {
    pub shutdown: AtomicBool,
    pub force_sync: AtomicBool,
    pub write_queue: Mutex<Vec<CalMutation>>,
}

impl CalSyncControl {
    pub fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            force_sync: AtomicBool::new(false),
            write_queue: Mutex::new(Vec::new()),
        }
    }

    pub fn enqueue(&self, mutation: CalMutation) {
        if let Ok(mut q) = self.write_queue.lock() {
            q.push(mutation);
        }
    }
}

impl Default for CalSyncControl {
    fn default() -> Self {
        Self::new()
    }
}

pub fn spawn_calsync_thread(
    account: JamailAccount,
    account_name: String,
    control: Arc<CalSyncControl>,
    tx: Sender<CalSyncEvent>,
) -> Option<thread::JoinHandle<()>> {
    let caldav_cfg = account.caldav.clone()?;
    Some(thread::spawn(move || {
        calsync_loop(account_name, caldav_cfg, control, tx)
    }))
}

fn calsync_loop(
    account_name: String,
    cfg: crate::config::CalDavConfig,
    control: Arc<CalSyncControl>,
    tx: Sender<CalSyncEvent>,
) {
    loop {
        if control.shutdown.load(Ordering::Relaxed) {
            return;
        }

        let db = match MailDb::open() {
            Ok(db) => db,
            Err(e) => {
                if tx.send(CalSyncEvent::Error(format!("DB: {}", e))).is_err() {
                    return;
                }
                sleep_or_shutdown(&control, Duration::from_secs(60));
                continue;
            }
        };

        let password = match cfg.auth.resolve_password() {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.send(CalSyncEvent::Error(format!("auth: {}", e)));
                sleep_or_shutdown(&control, Duration::from_secs(60));
                continue;
            }
        };
        let client = match CalDavClient::new(&cfg.url, &cfg.login, &password) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(CalSyncEvent::Error(format!("{}", e)));
                sleep_or_shutdown(&control, Duration::from_secs(60));
                continue;
            }
        };

        run_one_cycle(&account_name, &cfg, &client, &db, &control, &tx);

        if control.shutdown.load(Ordering::Relaxed) {
            return;
        }
        let interval = Duration::from_secs(cfg.poll_interval_secs.max(30));
        sleep_or_shutdown(&control, interval);
    }
}

fn sleep_or_shutdown(control: &CalSyncControl, total: Duration) {
    // Sleep in short slices so Shutdown/ForceSync are noticed promptly
    // rather than only at the top of the next full poll interval.
    let step = Duration::from_millis(500);
    let mut waited = Duration::ZERO;
    while waited < total {
        if control.shutdown.load(Ordering::Relaxed) || control.force_sync.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(step);
        waited += step;
    }
}

fn run_one_cycle(
    account_name: &str,
    cfg: &crate::config::CalDavConfig,
    client: &CalDavClient,
    db: &MailDb,
    control: &CalSyncControl,
    tx: &Sender<CalSyncEvent>,
) {
    control.force_sync.store(false, Ordering::Relaxed);

    // 1. Drain queued local writes first.
    let writes: Vec<CalMutation> = control
        .write_queue
        .lock()
        .map(|mut q| q.drain(..).collect())
        .unwrap_or_default();
    for mutation in writes {
        apply_mutation(client, db, mutation, tx);
    }

    // 2. Discover calendars and sync each.
    let _ = tx.send(CalSyncEvent::Syncing(account_name.to_string()));
    let discovered = match client.discover_calendars() {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(CalSyncEvent::Error(format!("discover: {}", e)));
            return;
        }
    };

    let allowed: Option<&Vec<String>> = cfg.calendars.as_ref();
    let mut kept_urls = Vec::new();
    for cal in &discovered {
        if let Some(names) = allowed
            && !names.contains(&cal.display_name)
        {
            continue;
        }
        kept_urls.push(cal.url.clone());
        let _ = db.upsert_calendar(account_name, &cal.url, &cal.display_name);
        sync_one_calendar(account_name, &cal.url, client, db, tx);
    }
    let _ = db.prune_calendars_not_in(account_name, &kept_urls);
}

fn sync_one_calendar(
    account_name: &str,
    calendar_url: &str,
    client: &CalDavClient,
    db: &MailDb,
    tx: &Sender<CalSyncEvent>,
) {
    let prior_token = db
        .get_calendar_sync_token(account_name, calendar_url)
        .ok()
        .flatten();
    let outcome = match client.sync_calendar(calendar_url, prior_token.as_deref()) {
        Ok(o) => o,
        Err(e) => {
            let _ = tx.send(CalSyncEvent::Error(format!("{}: {}", calendar_url, e)));
            return;
        }
    };

    let mut applied = 0usize;
    match outcome {
        SyncOutcome::Delta {
            token,
            changed,
            deleted,
        } => {
            for changed_event in changed {
                if apply_changed_event(account_name, calendar_url, client, db, &changed_event) {
                    applied += 1;
                }
            }
            for href in deleted {
                let _ = db.delete_calendar_event_by_href(account_name, calendar_url, &href);
                applied += 1;
            }
            let _ = db.set_calendar_sync_state(account_name, calendar_url, None, token.as_deref());
        }
        SyncOutcome::FullResyncRequired => match client.list_all_events(calendar_url) {
            Ok(all) => {
                let seen_hrefs: HashSet<String> = all.iter().map(|e| e.href.clone()).collect();
                for changed_event in &all {
                    if apply_changed_event(account_name, calendar_url, client, db, changed_event) {
                        applied += 1;
                    }
                }
                // Reconcile deletions: anything cached locally for this
                // calendar that the server no longer listed.
                if let Ok(cached) =
                    db.get_calendar_events_in_range(Some(account_name), i64::MIN, i64::MAX)
                {
                    for row in cached.iter().filter(|r| r.calendar_url == calendar_url) {
                        if !seen_hrefs.contains(&row.href)
                            && row.local_status == db::CAL_STATUS_SYNCED
                        {
                            let _ = db.delete_calendar_event_by_href(
                                account_name,
                                calendar_url,
                                &row.href,
                            );
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(CalSyncEvent::Error(format!(
                    "full resync of {} failed: {}",
                    calendar_url, e
                )));
                return;
            }
        },
    }
    let _ = tx.send(CalSyncEvent::CalendarSynced(
        calendar_url.to_string(),
        applied,
    ));
}

/// Upsert one changed event, fetching its body via `GET` first if the
/// server didn't inline `calendar-data` in the sync/list response (see
/// `caldav` module docs). Returns `true` if the row was successfully
/// applied.
fn apply_changed_event(
    account_name: &str,
    calendar_url: &str,
    client: &CalDavClient,
    db: &MailDb,
    changed: &crate::caldav::ChangedEvent,
) -> bool {
    let ics = match &changed.calendar_data {
        Some(ics) => ics.clone(),
        None => match client.get_event(&changed.href) {
            Ok((_, body)) => body,
            Err(_) => return false,
        },
    };
    let events = match crate::calendar::parse_vevents(&ics) {
        Ok(events) => events,
        Err(_) => return false, // explicit-error parse failures are skipped, not silently corrupted
    };
    let Some(event) = events.into_iter().next() else {
        return false;
    };
    db.upsert_calendar_event(
        account_name,
        calendar_url,
        &changed.href,
        changed.etag.as_deref(),
        &event,
        &ics,
    )
    .is_ok()
}

fn apply_mutation(
    client: &CalDavClient,
    db: &MailDb,
    mutation: CalMutation,
    tx: &Sender<CalSyncEvent>,
) {
    match mutation {
        CalMutation::Create { local_id } => match db.get_calendar_event_by_id(local_id) {
            Ok(Some(row)) => match row.to_vevent() {
                Ok(event) => {
                    let href = match crate::caldav::new_event_href(&row.calendar_url, &event.uid) {
                        Ok(h) => h,
                        Err(e) => {
                            let _ = tx.send(CalSyncEvent::MutationError(local_id, e.to_string()));
                            return;
                        }
                    };
                    let ics = event.to_new_ics(chrono::Utc::now());
                    match client.put_event(&href, &ics, None, true) {
                        Ok(etag) => {
                            let _ = db.mark_calendar_event_synced(local_id, &href, Some(&etag));
                            let _ = tx.send(CalSyncEvent::MutationApplied(local_id));
                        }
                        Err(CalDavError::Conflict(msg)) => {
                            let _ = db.mark_calendar_event_conflict(local_id, &msg);
                            let _ = tx.send(CalSyncEvent::MutationConflict(local_id, msg));
                        }
                        Err(e) => {
                            let _ = tx.send(CalSyncEvent::MutationError(local_id, e.to_string()));
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(CalSyncEvent::MutationError(local_id, e.to_string()));
                }
            },
            Ok(None) => {}
            Err(e) => {
                let _ = tx.send(CalSyncEvent::MutationError(local_id, e.to_string()));
            }
        },
        CalMutation::Update { local_id, edits: _ } => {
            // Edits are already applied to the local row by
            // `db::apply_local_calendar_edit` at edit time (see
            // `calapp`); here we just push the resulting raw ICS with the
            // row's current ETag as `If-Match`.
            match db.get_calendar_event_by_id(local_id) {
                Ok(Some(row)) if row.raw_ics.is_some() => {
                    let ics = row.raw_ics.clone().unwrap();
                    match client.put_event(&row.href, &ics, row.etag.as_deref(), false) {
                        Ok(etag) => {
                            let _ = db.mark_calendar_event_synced(local_id, &row.href, Some(&etag));
                            let _ = tx.send(CalSyncEvent::MutationApplied(local_id));
                        }
                        Err(CalDavError::Conflict(msg)) => {
                            let _ = db.mark_calendar_event_conflict(local_id, &msg);
                            let _ = tx.send(CalSyncEvent::MutationConflict(local_id, msg));
                        }
                        Err(e) => {
                            let _ = tx.send(CalSyncEvent::MutationError(local_id, e.to_string()));
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = tx.send(CalSyncEvent::MutationError(local_id, e.to_string()));
                }
            }
        }
        CalMutation::Delete { local_id } => match db.get_calendar_event_by_id(local_id) {
            Ok(Some(row)) => match client.delete_event(&row.href, row.etag.as_deref()) {
                Ok(()) => {
                    let _ = db.delete_calendar_event_row(local_id);
                    let _ = tx.send(CalSyncEvent::MutationApplied(local_id));
                }
                Err(CalDavError::Conflict(msg)) => {
                    let _ = db.mark_calendar_event_conflict(local_id, &msg);
                    let _ = tx.send(CalSyncEvent::MutationConflict(local_id, msg));
                }
                Err(e) => {
                    let _ = tx.send(CalSyncEvent::MutationError(local_id, e.to_string()));
                }
            },
            Ok(None) => {}
            Err(e) => {
                let _ = tx.send(CalSyncEvent::MutationError(local_id, e.to_string()));
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caldav::ChangedEvent;
    use crate::calendar::parse_vevents;

    #[test]
    fn apply_changed_event_upserts_when_calendar_data_is_inline() {
        let db = MailDb::open_in_memory().unwrap();
        let ics =
            "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240115T090000Z\r\nSUMMARY:Hi\r\nEND:VEVENT\r\n";
        let changed = ChangedEvent {
            href: "/cal/x.ics".to_string(),
            etag: Some("\"e1\"".to_string()),
            calendar_data: Some(ics.to_string()),
        };
        // No live server needed: calendar_data is inline, so this never
        // calls client.get_event(). Build a client pointed at a bogus
        // unreachable address purely to satisfy the function signature —
        // it must never be dialed.
        let client = CalDavClient::new("http://127.0.0.1:1", "u", "p").unwrap();
        let ok = apply_changed_event(
            "acct",
            "https://cal.example.com/dav/",
            &client,
            &db,
            &changed,
        );
        assert!(ok);
        let row = db
            .get_calendar_events_in_range(Some("acct"), 0, i64::MAX)
            .unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].uid, "x");
        assert_eq!(row[0].etag.as_deref(), Some("\"e1\""));
    }

    #[test]
    fn apply_changed_event_skips_unparseable_ics_without_panicking() {
        let db = MailDb::open_in_memory().unwrap();
        let changed = ChangedEvent {
            href: "/cal/bad.ics".to_string(),
            etag: None,
            calendar_data: Some("BEGIN:VEVENT\r\nEND:VEVENT\r\n".to_string()), // missing UID/DTSTART
        };
        let client = CalDavClient::new("http://127.0.0.1:1", "u", "p").unwrap();
        let ok = apply_changed_event(
            "acct",
            "https://cal.example.com/dav/",
            &client,
            &db,
            &changed,
        );
        assert!(!ok);
        let rows = db
            .get_calendar_events_in_range(Some("acct"), 0, i64::MAX)
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn calsync_control_enqueue_and_drain() {
        let control = CalSyncControl::new();
        control.enqueue(CalMutation::Delete { local_id: 5 });
        let drained: Vec<CalMutation> = control.write_queue.lock().unwrap().drain(..).collect();
        assert_eq!(drained.len(), 1);
        assert!(matches!(drained[0], CalMutation::Delete { local_id: 5 }));
    }

    #[test]
    fn spawn_calsync_thread_returns_none_when_account_has_no_caldav_config() {
        let account = crate::config::JamailAccount {
            default: false,
            email: "a@b.com".to_string(),
            display_name: None,
            imap: test_imap_config(),
            smtp: None,
            folders: None,
            show_unlisted_folders: false,
            senders: None,
            sent_folder: None,
            draft_folder: None,
            notify_folders: None,
            color: None,
            caldav: None,
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        let handle = spawn_calsync_thread(
            account,
            "acct".to_string(),
            Arc::new(CalSyncControl::new()),
            tx,
        );
        assert!(handle.is_none());
    }

    fn test_imap_config() -> crate::config::ImapConfig {
        crate::config::ImapConfig {
            host: "imap.example.com".to_string(),
            port: 993,
            login: "a@b.com".to_string(),
            auth: crate::config::AuthConfig {
                auth_type: "password".to_string(),
                value: "secret".to_string(),
            },
        }
    }

    #[test]
    fn apply_mutation_delete_of_nonexistent_row_is_a_harmless_no_op() {
        let db = MailDb::open_in_memory().unwrap();
        let client = CalDavClient::new("http://127.0.0.1:1", "u", "p").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        apply_mutation(&client, &db, CalMutation::Delete { local_id: 999 }, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn apply_mutation_create_is_a_conflict_when_server_unreachable_surfaces_as_error_not_panic() {
        // Use a real, currently-loopback-unreachable port so the PUT fails
        // fast with a connection error (not a hang) — verifies the error
        // path reports MutationError rather than panicking or silently
        // dropping the local row's state.
        let db = MailDb::open_in_memory().unwrap();
        let event = parse_vevents(
            "BEGIN:VEVENT\r\nUID:unreachable-1\r\nDTSTART:20240115T090000Z\r\nEND:VEVENT\r\n",
        )
        .unwrap()
        .remove(0);
        let local_id = db
            .insert_local_calendar_event("acct", "http://127.0.0.1:1/cal/", &event)
            .unwrap();
        let client = CalDavClient::new("http://127.0.0.1:1", "u", "p").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        apply_mutation(&client, &db, CalMutation::Create { local_id }, &tx);
        match rx.try_recv() {
            Ok(CalSyncEvent::MutationError(id, _)) => assert_eq!(id, local_id),
            other => panic!(
                "expected MutationError, got {:?}",
                other.err().map(|e| e.to_string())
            ),
        }
        // Row must remain pending_create (not silently marked synced).
        let row = db.get_calendar_event_by_id(local_id).unwrap().unwrap();
        assert_eq!(row.local_status, db::CAL_STATUS_PENDING_CREATE);
    }
}
