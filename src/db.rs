use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::io::Read;
use std::path::PathBuf;

use crate::calendar::{EventStatus, EventTime, VEvent};
use crate::mail::{AttachmentData, Email, EmailContent, FolderInfo};
use chrono::{DateTime, Local, TimeZone, Utc};

const CURRENT_SCHEMA_VERSION: i32 = 6;

pub struct AttachmentMeta {
    pub id: i64,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: usize,
}

/// A row from the `calendars` table: one discovered/configured calendar
/// collection for an account.
pub struct CalendarRow {
    pub url: String,
    pub display_name: String,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
    pub color: Option<String>,
}

/// A row from the `calendar_events` table. `raw_ics` is the decompressed
/// original document (when known — always present for anything that came
/// from the server, absent only for a not-yet-uploaded local creation),
/// kept so edits can go through [`VEvent::to_ics`]'s patch path rather than
/// reconstructing a document from scratch.
#[derive(Clone)]
pub struct CalendarEventRow {
    pub id: i64,
    pub account: String,
    pub calendar_url: String,
    pub href: String,
    pub etag: Option<String>,
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub location: String,
    pub status: String,
    pub organizer: String,
    pub dtstart_utc: i64,
    pub dtend_utc: i64,
    pub all_day: bool,
    pub tzid: Option<String>,
    pub rrule: Option<String>,
    pub sequence: i64,
    pub raw_ics: Option<String>,
    pub local_status: String,
    pub local_error: Option<String>,
}

impl CalendarEventRow {
    /// Reconstruct a [`VEvent`] from this row: if `raw_ics` is present, it
    /// is re-parsed (giving back every unmodeled property, per
    /// `calendar`'s "preserve verbatim" design); otherwise a `VEvent` is
    /// built directly from the row's structured columns (used for
    /// not-yet-uploaded local creations, which have no raw document yet).
    pub fn to_vevent(&self) -> Result<VEvent> {
        if let Some(raw) = &self.raw_ics {
            let events = crate::calendar::parse_vevents(raw).map_err(|e| {
                anyhow::anyhow!(
                    "stored calendar_events row {} failed to re-parse: {}",
                    self.id,
                    e
                )
            })?;
            if let Some(event) = events.into_iter().find(|e| e.uid == self.uid) {
                return Ok(event);
            }
            // Fall through to the structured-column reconstruction if the
            // UID isn't found (shouldn't happen for data this crate wrote).
        }
        let dtstart = DateTime::<Utc>::from_timestamp(self.dtstart_utc, 0)
            .context("invalid stored dtstart_utc")?;
        let dtend = DateTime::<Utc>::from_timestamp(self.dtend_utc, 0)
            .context("invalid stored dtend_utc")?;
        Ok(VEvent {
            uid: self.uid.clone(),
            summary: self.summary.clone(),
            description: self.description.clone(),
            location: self.location.clone(),
            status: EventStatus::parse(&self.status),
            organizer: if self.organizer.is_empty() {
                None
            } else {
                Some(self.organizer.clone())
            },
            attendees: Vec::new(),
            dtstart: EventTime {
                utc: dtstart,
                tzid: self.tzid.clone(),
                all_day: self.all_day,
            },
            dtend: EventTime {
                utc: dtend,
                tzid: self.tzid.clone(),
                all_day: self.all_day,
            },
            rrule: self.rrule.clone(),
            exdates: Vec::new(),
            alarms: Vec::new(),
            sequence: self.sequence,
            dtstamp: None,
            raw: None,
        })
    }
}

pub struct MailDb {
    conn: Connection,
}

/// The default on-disk database path (`$XDG_DATA_HOME`/`jamail/mail.db`,
/// following `dirs::data_dir()`). `pub(crate)` so `daemon.rs` can pass it
/// explicitly to `caldav_server::spawn` rather than that module needing to
/// know this path-resolution logic itself.
pub(crate) fn db_path() -> Result<PathBuf> {
    let data_dir = dirs::data_dir().context("Could not determine data directory")?;
    let dir = data_dir.join("jamail");
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    Ok(dir.join("mail.db"))
}

fn compress(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, 3).unwrap_or_else(|_| data.to_vec())
}

fn decompress(data: &[u8]) -> Vec<u8> {
    let mut decoder = zstd::Decoder::new(data).unwrap();
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap_or_default();
    out
}

fn get_schema_version(conn: &Connection) -> i32 {
    // Check if schema_version table exists
    let has_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get::<_, i32>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !has_table {
        return 0;
    }

    conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap_or(0)
}

/// Full from-scratch schema creation, at [`CURRENT_SCHEMA_VERSION`]. Used
/// for a brand-new database and for any pre-v4 upgrade (schema shapes old
/// enough that "wipe and recreate" was already the accepted behavior
/// before calendar support existed — see [`upgrade_schema`] for the v4->v5
/// step, which is additive and does *not* go through this function,
/// specifically so existing mail caches survive picking up calendar
/// support).
fn create_schema(conn: &Connection) -> Result<()> {
    let sql = format!(
        "{}\n{}\n{}",
        MAIL_SCHEMA_SQL, CALENDAR_SCHEMA_SQL, SYNC_LOG_SCHEMA_SQL
    );
    conn.execute_batch(&sql)?;
    Ok(())
}

const MAIL_SCHEMA_SQL: &str = "DROP TABLE IF EXISTS emails_fts;
         DROP TABLE IF EXISTS attachments;
         DROP TABLE IF EXISTS emails;
         DROP TABLE IF EXISTS sync_state;
         DROP TABLE IF EXISTS folders;
         DROP TABLE IF EXISTS local_messages;
         DROP TABLE IF EXISTS calendar_sync_log;
         DROP TABLE IF EXISTS calendar_events;
         DROP TABLE IF EXISTS calendars;
         DROP TABLE IF EXISTS schema_version;

         CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version VALUES (6);

         CREATE TABLE folders (
             account   TEXT NOT NULL,
             name      TEXT NOT NULL,
             delimiter TEXT NOT NULL DEFAULT '/',
             updated   TEXT,
             PRIMARY KEY (account, name)
         );

         CREATE TABLE emails (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             account         TEXT NOT NULL,
             folder          TEXT NOT NULL,
             uid             INTEGER NOT NULL,
             from_addr       TEXT NOT NULL,
             to_addr         TEXT NOT NULL DEFAULT '',
             subject         TEXT NOT NULL,
             date_ts         INTEGER NOT NULL,
             date_rfc3339    TEXT NOT NULL,
             is_unread       INTEGER NOT NULL DEFAULT 1,
             preview         TEXT NOT NULL DEFAULT '',
             text_body_zstd  BLOB,
             html_body_zstd  BLOB,
             message_id      TEXT NOT NULL DEFAULT '',
             in_reply_to     TEXT NOT NULL DEFAULT '',
             refs            TEXT NOT NULL DEFAULT '',
             has_attachments INTEGER NOT NULL DEFAULT 0,
             raw_headers_zstd BLOB,
             UNIQUE(account, folder, uid)
         );

         CREATE INDEX idx_emails_acct_folder_date ON emails(account, folder, date_ts DESC);
         CREATE INDEX idx_emails_message_id ON emails(message_id);

         CREATE TABLE sync_state (
             account     TEXT NOT NULL,
             folder      TEXT NOT NULL,
             uidvalidity INTEGER NOT NULL DEFAULT 0,
             last_uid    INTEGER NOT NULL DEFAULT 0,
             last_sync   TEXT,
             PRIMARY KEY (account, folder)
         );

         CREATE VIRTUAL TABLE emails_fts USING fts5(
             from_addr, subject, body_text,
             tokenize='unicode61 remove_diacritics 2'
         );

         CREATE TABLE attachments (
             id           INTEGER PRIMARY KEY AUTOINCREMENT,
             email_id     INTEGER NOT NULL,
             filename     TEXT NOT NULL,
             mime_type    TEXT NOT NULL DEFAULT '',
             size_bytes   INTEGER NOT NULL DEFAULT 0,
             content_zstd BLOB NOT NULL
         );
         CREATE INDEX idx_attachments_email_id ON attachments(email_id);

         CREATE TABLE local_messages (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             account         TEXT NOT NULL,
             status          TEXT NOT NULL DEFAULT 'draft',
             from_addr       TEXT NOT NULL DEFAULT '',
             to_addr         TEXT NOT NULL DEFAULT '',
             cc              TEXT NOT NULL DEFAULT '',
             bcc             TEXT NOT NULL DEFAULT '',
             subject         TEXT NOT NULL DEFAULT '',
             body            TEXT NOT NULL DEFAULT '',
             in_reply_to     TEXT NOT NULL DEFAULT '',
             refs            TEXT NOT NULL DEFAULT '',
             created_at      TEXT NOT NULL,
             sent_at         TEXT,
             error           TEXT,
             upload_status   TEXT NOT NULL DEFAULT 'none',
             upload_error    TEXT
         );
         CREATE INDEX idx_local_messages_status ON local_messages(status, created_at DESC);";

/// `CREATE TABLE`/`CREATE INDEX` statements for calendar storage, shared
/// verbatim between [`create_schema`] (fresh database) and
/// [`upgrade_schema`]'s v4->v5 step (existing database, `IF NOT EXISTS` so
/// it's a no-op if somehow already applied).
const CALENDAR_SCHEMA_SQL: &str = "
         CREATE TABLE IF NOT EXISTS calendars (
             account      TEXT NOT NULL,
             url          TEXT NOT NULL,
             display_name TEXT NOT NULL DEFAULT '',
             ctag         TEXT,
             sync_token   TEXT,
             color        TEXT,
             PRIMARY KEY (account, url)
         );

         CREATE TABLE IF NOT EXISTS calendar_events (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             account         TEXT NOT NULL,
             calendar_url    TEXT NOT NULL,
             href            TEXT NOT NULL,
             etag            TEXT,
             uid             TEXT NOT NULL,
             summary         TEXT NOT NULL DEFAULT '',
             description     TEXT NOT NULL DEFAULT '',
             location        TEXT NOT NULL DEFAULT '',
             status          TEXT NOT NULL DEFAULT 'CONFIRMED',
             organizer       TEXT NOT NULL DEFAULT '',
             dtstart_utc     INTEGER NOT NULL,
             dtend_utc       INTEGER NOT NULL,
             all_day         INTEGER NOT NULL DEFAULT 0,
             tzid            TEXT,
             rrule           TEXT,
             sequence        INTEGER NOT NULL DEFAULT 0,
             raw_ics_zstd    BLOB,
             local_status    TEXT NOT NULL DEFAULT 'synced',
             local_error     TEXT,
             UNIQUE(account, calendar_url, href)
         );
         CREATE INDEX IF NOT EXISTS idx_calendar_events_time ON calendar_events(account, dtstart_utc);
         CREATE INDEX IF NOT EXISTS idx_calendar_events_uid ON calendar_events(uid);
         CREATE INDEX IF NOT EXISTS idx_calendar_events_local_status ON calendar_events(local_status);
";

/// `CREATE TABLE`/`CREATE INDEX` statements for the CalDAV *server*'s
/// append-only change log (see `caldav_server` module docs): every
/// create/update/delete a client makes against a jamaild-hosted calendar
/// appends one row here, keyed by an autoincrementing `id` that doubles as
/// the RFC 6578 sync-token/revision number. `deleted=1` rows are tombstones
/// (the corresponding `calendar_events` row is actually removed;
/// resurrection information — that a href *used to* exist — lives only
/// here, which is exactly what answering "what changed since token N"
/// needs for the deletion side of a sync-collection REPORT). This table is
/// unrelated to `calendar_events.local_status`'s `PENDING_*` values, which
/// track jamaild's own outbound queue when acting as a CalDAV *client*.
const SYNC_LOG_SCHEMA_SQL: &str = "
         CREATE TABLE IF NOT EXISTS calendar_sync_log (
             id           INTEGER PRIMARY KEY AUTOINCREMENT,
             account      TEXT NOT NULL,
             calendar_url TEXT NOT NULL,
             href         TEXT NOT NULL,
             deleted      INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_calendar_sync_log_lookup
             ON calendar_sync_log(account, calendar_url, id);
";

/// Additive v5->v6 upgrade: adds the CalDAV server's change-log table.
/// Same non-destructive rationale as the v4->v5 step above.
fn migrate_v5_to_v6_add_sync_log(conn: &Connection) -> Result<()> {
    conn.execute_batch(SYNC_LOG_SCHEMA_SQL)?;
    conn.execute("UPDATE schema_version SET version = 6", [])?;
    Ok(())
}

/// Status values for `calendar_events.local_status`: `SYNCED` reflects the
/// last-known server state exactly; the `PENDING_*` values are a local
/// write queued for the daemon's calendar sync thread to apply (mirroring
/// `local_messages`' Sent/Draft upload queue — see `sync::UploadRequest`);
/// `CONFLICT` means the last write attempt hit a `412`/`409` and needs
/// user attention (see `calsync` module docs).
/// Prefix of the `calendar_url` under which `caldav_server.rs` hosts an
/// account's own inbound calendar. It is a purely local namespace with no
/// remote counterpart, which is why remote-discovery-driven cleanup
/// (`prune_calendars_not_in`) must leave it alone.
pub const SERVER_CALENDAR_URL_PREFIX: &str = "server:";

/// Placeholder `calendar_events.href` for an event created locally and not
/// yet uploaded: there is no server-assigned path for it yet. Deliberately
/// not a valid URL or path — a request must never be sent to one.
pub const LOCAL_HREF_PREFIX: &str = "local:";

pub const CAL_STATUS_SYNCED: &str = "synced";
pub const CAL_STATUS_PENDING_CREATE: &str = "pending_create";
pub const CAL_STATUS_PENDING_UPDATE: &str = "pending_update";
pub const CAL_STATUS_PENDING_DELETE: &str = "pending_delete";
pub const CAL_STATUS_CONFLICT: &str = "conflict";

/// Additive v4->v5 upgrade: adds the calendar tables without touching (let
/// alone dropping) any existing mail table. This is the crux of "upgrading
/// jamail to a build with calendar support must not blindly wipe the
/// existing mail cache" — every mail-schema version bump before this one
/// used a full wipe-and-recreate (acceptable when the *shape* of the mail
/// tables themselves was changing), but calendar support only *adds*
/// tables, so there's no reason to lose cached mail over it.
fn migrate_v4_to_v5_add_calendar_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(CALENDAR_SCHEMA_SQL)?;
    conn.execute("UPDATE schema_version SET version = 5", [])?;
    Ok(())
}

/// Bring `conn` up to [`CURRENT_SCHEMA_VERSION`], preserving existing mail
/// data whenever an additive path is available (currently: exactly the
/// v4->v5 step). Anything older than v4 still goes through a full
/// [`create_schema`] wipe-and-recreate, matching this database's
/// established "the cache can always be rebuilt from the server" model for
/// mail-schema changes predating calendar support.
fn upgrade_schema(conn: &Connection) -> Result<()> {
    let mut version = get_schema_version(conn);
    if version < 4 {
        create_schema(conn)?;
        version = CURRENT_SCHEMA_VERSION;
    }
    if version == 4 {
        migrate_v4_to_v5_add_calendar_tables(conn)?;
        version = 5;
    }
    if version == 5 {
        migrate_v5_to_v6_add_sync_log(conn)?;
        version = 6;
    }
    debug_assert_eq!(
        version, CURRENT_SCHEMA_VERSION,
        "upgrade_schema must always land exactly on CURRENT_SCHEMA_VERSION"
    );
    Ok(())
}

/// Status values for `local_messages.status`.
pub const STATUS_DRAFT: &str = "draft";
pub const STATUS_SENDING: &str = "sending";
pub const STATUS_SEND_ERROR: &str = "send_error";
pub const STATUS_SENT: &str = "sent";

/// Status values for `local_messages.upload_status`.
pub const UPLOAD_NONE: &str = "none";
pub const UPLOAD_PENDING: &str = "pending";
pub const UPLOAD_UPLOADED: &str = "uploaded";
pub const UPLOAD_ERROR: &str = "error";

impl MailDb {
    pub fn open() -> Result<Self> {
        let path = db_path()?;
        Self::open_at(&path)
    }

    /// Open (creating/upgrading as needed) the database at an explicit
    /// path, rather than the default `dirs::data_dir()`-derived one. This
    /// is what lets `caldav_server` tests (and, in principle, any future
    /// `--db-path`-style override) point at an isolated file instead of
    /// the real on-disk cache — every request the CalDAV server handles
    /// opens its own connection via this same path (mirroring
    /// `sync::sync_loop`'s "open a fresh `MailDb` per iteration" pattern),
    /// so callers that need shared, persisted state across many opens
    /// (i.e. anything other than a `:memory:`-style throwaway) must pass a
    /// real file path here, not an in-memory one.
    pub(crate) fn open_at(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open database at {}", path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -4000;
             PRAGMA mmap_size = 0;",
        )?;

        upgrade_schema(&conn)?;

        Ok(Self { conn })
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory database")?;
        create_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Test-only: build a database with exactly the pre-calendar (v4) mail
    /// schema — no `calendars`/`calendar_events` tables at all — by reusing
    /// the real [`MAIL_SCHEMA_SQL`] (which, unlike [`create_schema`],
    /// already excludes the calendar tables) with its embedded version
    /// literal patched from 6 to 4. Used to verify
    /// [`migrate_v4_to_v5_add_calendar_tables`] against a faithful legacy
    /// database rather than a hand-maintained duplicate schema string that
    /// could drift from the real v4 shape.
    #[cfg(test)]
    pub(crate) fn open_in_memory_legacy_v4() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory database")?;
        let sql = MAIL_SCHEMA_SQL.replacen("VALUES (6)", "VALUES (4)", 1);
        conn.execute_batch(&sql)?;
        Ok(Self { conn })
    }

    /// Test-only: build a database with exactly the pre-sync-log (v5)
    /// schema — mail tables plus `calendars`/`calendar_events`, but no
    /// `calendar_sync_log` — to verify
    /// [`migrate_v5_to_v6_add_sync_log`] preserves existing calendar data.
    #[cfg(test)]
    pub(crate) fn open_in_memory_legacy_v5() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory database")?;
        let sql = format!("{}\n{}", MAIL_SCHEMA_SQL, CALENDAR_SCHEMA_SQL).replacen(
            "VALUES (6)",
            "VALUES (5)",
            1,
        );
        conn.execute_batch(&sql)?;
        Ok(Self { conn })
    }

    /// Test-only: run the real schema-upgrade path against this database's
    /// connection.
    #[cfg(test)]
    pub(crate) fn upgrade_for_test(&self) -> Result<()> {
        upgrade_schema(&self.conn)
    }

    #[cfg(test)]
    pub(crate) fn schema_version_for_test(&self) -> i32 {
        get_schema_version(&self.conn)
    }

    pub fn begin_tx(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    pub fn commit_tx(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback_tx(&self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }

    /// Insert a single email inside an already-open transaction.
    /// Caller is responsible for BEGIN/COMMIT/ROLLBACK.
    #[allow(clippy::too_many_arguments)]
    pub fn store_email_core(
        &self,
        account: &str,
        folder: &str,
        uid: u32,
        from: &str,
        to: &str,
        subject: &str,
        date: &DateTime<Local>,
        is_unread: bool,
        preview: &str,
        text_body: &str,
        html_body: Option<&str>,
        raw_headers: &str,
        message_id: &str,
        in_reply_to: &str,
        refs: &str,
        has_attachments: bool,
        attachments: &[AttachmentData],
    ) -> Result<i64> {
        let date_ts = date.timestamp();
        let date_rfc3339 = date.to_rfc3339();
        let text_compressed = if text_body.is_empty() {
            None
        } else {
            Some(compress(text_body.as_bytes()))
        };
        let html_compressed = html_body.map(|h| compress(h.as_bytes()));
        let headers_compressed = if raw_headers.is_empty() {
            None
        } else {
            Some(compress(raw_headers.as_bytes()))
        };

        // Delete old entry if exists (for REPLACE semantics)
        let old_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM emails WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, folder, uid],
                |row| row.get(0),
            )
            .ok();

        if let Some(old_id) = old_id {
            self.conn
                .execute("DELETE FROM emails_fts WHERE rowid = ?1", params![old_id])?;
            self.conn.execute(
                "DELETE FROM attachments WHERE email_id = ?1",
                params![old_id],
            )?;
            self.conn
                .execute("DELETE FROM emails WHERE id = ?1", params![old_id])?;
        }

        self.conn.execute(
            "INSERT INTO emails
                (account, folder, uid, from_addr, to_addr, subject, date_ts, date_rfc3339,
                 is_unread, preview, text_body_zstd, html_body_zstd, raw_headers_zstd,
                 message_id, in_reply_to, refs, has_attachments)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                account,
                folder,
                uid,
                from,
                to,
                subject,
                date_ts,
                date_rfc3339,
                is_unread as i32,
                preview,
                text_compressed,
                html_compressed,
                headers_compressed,
                message_id,
                in_reply_to,
                refs,
                has_attachments as i32,
            ],
        )?;

        let email_id = self.conn.last_insert_rowid();

        // Update FTS
        self.conn.execute(
            "INSERT INTO emails_fts (rowid, from_addr, subject, body_text) VALUES (?1, ?2, ?3, ?4)",
            params![email_id, from, subject, text_body],
        )?;

        // Store attachments
        for att in attachments {
            let compressed = compress(&att.data);
            self.conn.execute(
                "INSERT INTO attachments (email_id, filename, mime_type, size_bytes, content_zstd)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    email_id,
                    att.filename,
                    att.mime_type,
                    att.size_bytes as i64,
                    compressed
                ],
            )?;
        }

        Ok(email_id)
    }

    /// Store a single email, wrapped in its own transaction.
    #[allow(clippy::too_many_arguments, dead_code)]
    pub fn store_email(
        &self,
        account: &str,
        folder: &str,
        uid: u32,
        from: &str,
        to: &str,
        subject: &str,
        date: &DateTime<Local>,
        is_unread: bool,
        preview: &str,
        text_body: &str,
        html_body: Option<&str>,
        raw_headers: &str,
        message_id: &str,
        in_reply_to: &str,
        refs: &str,
        has_attachments: bool,
        attachments: &[AttachmentData],
    ) -> Result<i64> {
        self.begin_tx()?;
        let result = self.store_email_core(
            account,
            folder,
            uid,
            from,
            to,
            subject,
            date,
            is_unread,
            preview,
            text_body,
            html_body,
            raw_headers,
            message_id,
            in_reply_to,
            refs,
            has_attachments,
            attachments,
        );
        match result {
            Ok(id) => {
                self.commit_tx()?;
                Ok(id)
            }
            Err(e) => {
                self.rollback_tx();
                Err(e)
            }
        }
    }

    pub fn get_email_list(&self, account: &str, folder: &str) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, from_addr, subject, date_ts, date_rfc3339, is_unread, preview,
                    message_id, in_reply_to, refs, has_attachments, account
             FROM emails WHERE account = ?1 AND folder = ?2
             ORDER BY date_ts DESC",
        )?;

        let rows = stmt.query_map(params![account, folder], |row| {
            let id: i64 = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let from: String = row.get(2)?;
            let subject: String = row.get(3)?;
            let date_ts: i64 = row.get(4)?;
            let _date_rfc3339: String = row.get(5)?;
            let is_unread: bool = row.get::<_, i32>(6)? != 0;
            let preview: String = row.get(7)?;
            let message_id: String = row.get(8)?;
            let in_reply_to: String = row.get(9)?;
            let references: String = row.get(10)?;
            let has_attachments: bool = row.get::<_, i32>(11)? != 0;
            let acct: String = row.get(12)?;

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                id,
                uid,
                account: acct,
                from,
                subject,
                date,
                is_unread,
                preview,
                message_id,
                in_reply_to,
                references,
                has_attachments,
                status_label: None,
            })
        })?;

        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }

    pub fn get_email_content(&self, id: i64) -> Result<Option<EmailContent>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_addr, to_addr, subject, date_ts, text_body_zstd, html_body_zstd,
                    raw_headers_zstd
             FROM emails WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        let row = match rows.next()? {
            Some(r) => r,
            None => return Ok(None),
        };

        let from: String = row.get(0)?;
        let to: String = row.get(1)?;
        let subject: String = row.get(2)?;
        let date_ts: i64 = row.get(3)?;
        let text_zstd: Option<Vec<u8>> = row.get(4)?;
        let html_zstd: Option<Vec<u8>> = row.get(5)?;
        let headers_zstd: Option<Vec<u8>> = row.get(6)?;

        let date = Local
            .timestamp_opt(date_ts, 0)
            .single()
            .unwrap_or_else(Local::now);

        let text_body = text_zstd
            .map(|z| String::from_utf8_lossy(&decompress(&z)).to_string())
            .unwrap_or_default();

        let html_body = html_zstd.map(|z| String::from_utf8_lossy(&decompress(&z)).to_string());

        let raw_headers = headers_zstd
            .map(|z| String::from_utf8_lossy(&decompress(&z)).to_string())
            .unwrap_or_default();

        Ok(Some(EmailContent {
            from,
            to,
            subject,
            date,
            text_body,
            html_body,
            raw_headers,
        }))
    }

    pub fn get_global_inbox(&self) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, from_addr, subject, date_ts, date_rfc3339, is_unread, preview,
                    message_id, in_reply_to, refs, has_attachments, account
             FROM emails WHERE folder = 'INBOX'
             ORDER BY date_ts DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let from: String = row.get(2)?;
            let subject: String = row.get(3)?;
            let date_ts: i64 = row.get(4)?;
            let _date_rfc3339: String = row.get(5)?;
            let is_unread: bool = row.get::<_, i32>(6)? != 0;
            let preview: String = row.get(7)?;
            let message_id: String = row.get(8)?;
            let in_reply_to: String = row.get(9)?;
            let references: String = row.get(10)?;
            let has_attachments: bool = row.get::<_, i32>(11)? != 0;
            let acct: String = row.get(12)?;

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                id,
                uid,
                account: acct,
                from,
                subject,
                date,
                is_unread,
                preview,
                message_id,
                in_reply_to,
                references,
                has_attachments,
                status_label: None,
            })
        })?;

        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }

    pub fn mark_read(&self, id: i64) -> Result<()> {
        self.conn
            .execute("UPDATE emails SET is_unread = 0 WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn search_emails(&self, account: &str, folder: &str, query: &str) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.uid, e.from_addr, e.subject, e.date_ts, e.is_unread, e.preview,
                    e.message_id, e.in_reply_to, e.refs, e.has_attachments, e.account
             FROM emails_fts f
             JOIN emails e ON e.id = f.rowid
             WHERE emails_fts MATCH ?1
               AND e.account = ?2 AND e.folder = ?3
             ORDER BY e.date_ts DESC",
        )?;

        let rows = stmt.query_map(params![query, account, folder], |row| {
            let id: i64 = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let from: String = row.get(2)?;
            let subject: String = row.get(3)?;
            let date_ts: i64 = row.get(4)?;
            let is_unread: bool = row.get::<_, i32>(5)? != 0;
            let preview: String = row.get(6)?;
            let message_id: String = row.get(7)?;
            let in_reply_to: String = row.get(8)?;
            let references: String = row.get(9)?;
            let has_attachments: bool = row.get::<_, i32>(10)? != 0;
            let acct: String = row.get(11)?;

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                id,
                uid,
                account: acct,
                from,
                subject,
                date,
                is_unread,
                preview,
                message_id,
                in_reply_to,
                references,
                has_attachments,
                status_label: None,
            })
        })?;

        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }

    pub fn has_email(&self, account: &str, folder: &str, uid: u32) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM emails WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, folder, uid],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn get_sync_state(&self, account: &str, folder: &str) -> Result<Option<(u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT uidvalidity, last_uid FROM sync_state WHERE account = ?1 AND folder = ?2",
        )?;
        let mut rows = stmt.query(params![account, folder])?;
        match rows.next()? {
            Some(row) => {
                let uidvalidity: u32 = row.get(0)?;
                let last_uid: u32 = row.get(1)?;
                Ok(Some((uidvalidity, last_uid)))
            }
            None => Ok(None),
        }
    }

    pub fn set_sync_state(
        &self,
        account: &str,
        folder: &str,
        uidvalidity: u32,
        last_uid: u32,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_state (account, folder, uidvalidity, last_uid, last_sync)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            params![account, folder, uidvalidity, last_uid],
        )?;
        Ok(())
    }

    pub fn get_attachments_meta(&self, email_id: i64) -> Result<Vec<AttachmentMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, filename, mime_type, size_bytes
             FROM attachments WHERE email_id = ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![email_id], |row| {
            Ok(AttachmentMeta {
                id: row.get(0)?,
                filename: row.get(1)?,
                mime_type: row.get(2)?,
                size_bytes: row.get::<_, i64>(3)? as usize,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn get_attachment_data(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_zstd FROM attachments WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => {
                let compressed: Vec<u8> = row.get(0)?;
                Ok(Some(decompress(&compressed)))
            }
            None => Ok(None),
        }
    }

    pub fn clear_folder_emails(&self, account: &str, folder: &str) -> Result<()> {
        // Delete FTS entries and attachments for emails in this folder
        self.conn.execute(
            "DELETE FROM emails_fts WHERE rowid IN (SELECT id FROM emails WHERE account = ?1 AND folder = ?2)",
            params![account, folder],
        )?;
        self.conn.execute(
            "DELETE FROM attachments WHERE email_id IN (SELECT id FROM emails WHERE account = ?1 AND folder = ?2)",
            params![account, folder],
        )?;
        self.conn.execute(
            "DELETE FROM emails WHERE account = ?1 AND folder = ?2",
            params![account, folder],
        )?;
        self.conn.execute(
            "DELETE FROM sync_state WHERE account = ?1 AND folder = ?2",
            params![account, folder],
        )?;
        Ok(())
    }

    pub fn store_folders(&self, account: &str, folders: &[FolderInfo]) -> Result<()> {
        self.conn
            .execute("DELETE FROM folders WHERE account = ?1", params![account])?;
        for f in folders {
            self.conn.execute(
                "INSERT INTO folders (account, name, delimiter, updated)
                 VALUES (?1, ?2, ?3, datetime('now'))",
                params![account, f.name, f.delimiter],
            )?;
        }
        Ok(())
    }

    pub fn get_known_addresses(&self) -> Result<Vec<String>> {
        let mut addrs = std::collections::HashSet::new();

        let mut stmt = self.conn.prepare("SELECT DISTINCT from_addr FROM emails")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for addr in rows.flatten() {
            let trimmed = addr.trim().to_string();
            if !trimmed.is_empty() {
                addrs.insert(trimmed);
            }
        }

        let mut stmt = self.conn.prepare("SELECT DISTINCT to_addr FROM emails")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for to_field in rows.flatten() {
            for part in to_field.split(',') {
                let trimmed = part.trim().to_string();
                if !trimmed.is_empty() {
                    addrs.insert(trimmed);
                }
            }
        }

        let mut result: Vec<String> = addrs.into_iter().collect();
        result.sort_by_key(|a| a.to_lowercase());
        Ok(result)
    }

    pub fn get_all_uids(&self, account: &str, folder: &str) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid FROM emails WHERE account = ?1 AND folder = ?2")?;
        let rows = stmt.query_map(params![account, folder], |row| row.get(0))?;
        let mut uids = Vec::new();
        for row in rows {
            uids.push(row?);
        }
        Ok(uids)
    }

    pub fn update_flags(&self, account: &str, folder: &str, flags: &[(u32, bool)]) -> Result<bool> {
        let mut changed = false;
        for &(uid, is_unread) in flags {
            let rows = self.conn.execute(
                "UPDATE emails SET is_unread = ?1 WHERE account = ?2 AND folder = ?3 AND uid = ?4 AND is_unread != ?1",
                params![is_unread as i32, account, folder, uid],
            )?;
            if rows > 0 {
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Look up the (account, uid, folder) an email belongs to by its local
    /// `id`. Includes `account` (not just `uid`/`folder`) because `id` is a
    /// single autoincrement PK shared across every account (see CLAUDE.md
    /// "Emails have an autoincrement id... All UI lookups use id") — with
    /// `jamaild` now syncing every configured account concurrently, callers
    /// that route a request to a specific account's IMAP connection (e.g.
    /// mark-seen) need to know *which* account, not just assume "the
    /// current one" (wrong for the cross-account Global Inbox view).
    pub fn get_email_account_uid_and_folder(
        &self,
        id: i64,
    ) -> Result<Option<(String, u32, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT account, uid, folder FROM emails WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => {
                let account: String = row.get(0)?;
                let uid: u32 = row.get(1)?;
                let folder: String = row.get(2)?;
                Ok(Some((account, uid, folder)))
            }
            None => Ok(None),
        }
    }

    pub fn get_folders(&self, account: &str) -> Result<Vec<FolderInfo>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, delimiter FROM folders WHERE account = ?1 ORDER BY name")?;
        let rows = stmt.query_map(params![account], |row| {
            Ok(FolderInfo {
                name: row.get(0)?,
                delimiter: row.get(1)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    // ── Local messages (drafts/sent) ──────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn save_draft(
        &self,
        account: &str,
        from: &str,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
        in_reply_to: &str,
        refs: &str,
    ) -> Result<i64> {
        let now = Local::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO local_messages (account, status, from_addr, to_addr, cc, bcc, subject, body, in_reply_to, refs, created_at)
             VALUES (?1, 'draft', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![account, from, to, cc, bcc, subject, body, in_reply_to, refs, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_draft(
        &self,
        id: i64,
        from: &str,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET from_addr=?2, to_addr=?3, cc=?4, bcc=?5, subject=?6, body=?7
             WHERE id=?1 AND status='draft'",
            params![id, from, to, cc, bcc, subject, body],
        )?;
        Ok(())
    }

    /// Mark a draft as actively being sent (in-flight). Keeps it visible in
    /// the Drafts list — with a "Sending…" indicator — instead of it
    /// silently disappearing while the background send worker runs.
    pub fn mark_draft_sending(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET status=?2 WHERE id=?1",
            params![id, STATUS_SENDING],
        )?;
        Ok(())
    }

    /// Record a failed send attempt. The message stays in the Drafts list
    /// (not "sent") with the error text attached so it doesn't vanish.
    pub fn mark_draft_send_error(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET status=?2, error=?3 WHERE id=?1",
            params![id, STATUS_SEND_ERROR, error],
        )?;
        Ok(())
    }

    pub fn mark_draft_sent(&self, id: i64) -> Result<()> {
        let now = Local::now().to_rfc3339();
        self.conn.execute(
            "UPDATE local_messages SET status=?2, sent_at=?3, error=NULL WHERE id=?1",
            params![id, STATUS_SENT, now],
        )?;
        Ok(())
    }

    /// Mark a local message as queued for upload to a remote Sent/Drafts
    /// folder. Called right before an `UploadRequest` is handed to the sync
    /// thread.
    pub fn set_upload_pending(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET upload_status=?2, upload_error=NULL WHERE id=?1",
            params![id, UPLOAD_PENDING],
        )?;
        Ok(())
    }

    pub fn set_upload_success(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET upload_status=?2, upload_error=NULL WHERE id=?1",
            params![id, UPLOAD_UPLOADED],
        )?;
        Ok(())
    }

    pub fn set_upload_error(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET upload_status=?2, upload_error=?3 WHERE id=?1",
            params![id, UPLOAD_ERROR, error],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn delete_draft(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM local_messages WHERE id=?1 AND status IN ('draft','sending','send_error')",
            params![id],
        )?;
        Ok(())
    }

    /// Drafts include in-flight ("sending") and failed ("send_error") rows
    /// so an in-progress or errored send never silently disappears from the
    /// Drafts list.
    pub fn get_drafts(&self, account: &str) -> Result<Vec<LocalMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account, status, from_addr, to_addr, cc, bcc, subject, body,
                    in_reply_to, refs, created_at, sent_at, error, upload_status, upload_error
             FROM local_messages WHERE status IN ('draft','sending','send_error') AND account=?1
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![account], local_message_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn get_sent(&self, account: &str) -> Result<Vec<LocalMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account, status, from_addr, to_addr, cc, bcc, subject, body,
                    in_reply_to, refs, created_at, sent_at, error, upload_status, upload_error
             FROM local_messages WHERE status='sent' AND account=?1
             ORDER BY sent_at DESC",
        )?;
        let rows = stmt.query_map(params![account], local_message_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    #[allow(dead_code)]
    pub fn get_draft(&self, id: i64) -> Result<Option<LocalMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account, status, from_addr, to_addr, cc, bcc, subject, body,
                    in_reply_to, refs, created_at, sent_at, error, upload_status, upload_error
             FROM local_messages WHERE id=?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(local_message_from_row(row)?)),
            None => Ok(None),
        }
    }

    // -- Calendars --------------------------------------------------

    /// Insert or update a discovered/configured calendar's display name.
    /// Never touches `ctag`/`sync_token` — use
    /// [`set_calendar_sync_state`](Self::set_calendar_sync_state) for
    /// those, so a rediscovery pass (which only learns the display name
    /// again) can't accidentally reset sync progress.
    pub fn upsert_calendar(&self, account: &str, url: &str, display_name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO calendars (account, url, display_name) VALUES (?1, ?2, ?3)
             ON CONFLICT(account, url) DO UPDATE SET display_name = excluded.display_name",
            params![account, url, display_name],
        )?;
        Ok(())
    }

    pub fn get_calendars(&self, account: &str) -> Result<Vec<CalendarRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT url, display_name, ctag, sync_token, color FROM calendars
             WHERE account = ?1 ORDER BY display_name",
        )?;
        let rows = stmt.query_map(params![account], |row| {
            Ok(CalendarRow {
                url: row.get(0)?,
                display_name: row.get(1)?,
                ctag: row.get(2)?,
                sync_token: row.get(3)?,
                color: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_calendar_sync_token(&self, account: &str, url: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sync_token FROM calendars WHERE account = ?1 AND url = ?2",
                params![account, url],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn set_calendar_sync_state(
        &self,
        account: &str,
        url: &str,
        ctag: Option<&str>,
        sync_token: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE calendars SET ctag = ?1, sync_token = ?2 WHERE account = ?3 AND url = ?4",
            params![ctag, sync_token, account, url],
        )?;
        Ok(())
    }

    /// Remove calendars (and their events) for `account` whose URL is not
    /// in `keep_urls` — used after a discovery pass to drop calendars that
    /// disappeared server-side or fell out of a `calendars:` allowlist.
    ///
    /// Calendars in the [`SERVER_CALENDAR_URL_PREFIX`] namespace are never
    /// pruned: they are hosted by this daemon's own inbound CalDAV server
    /// and have no remote counterpart, so they can't appear in `keep_urls`
    /// (which is built purely from remote discovery) and would otherwise be
    /// wiped — along with every event in them — on the first sync cycle
    /// after startup.
    pub fn prune_calendars_not_in(&self, account: &str, keep_urls: &[String]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT url FROM calendars WHERE account = ?1")?;
        let existing: Vec<String> = stmt
            .query_map(params![account], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        for url in existing {
            if !keep_urls.contains(&url) && !url.starts_with(SERVER_CALENDAR_URL_PREFIX) {
                self.conn.execute(
                    "DELETE FROM calendar_events WHERE account = ?1 AND calendar_url = ?2",
                    params![account, url],
                )?;
                self.conn.execute(
                    "DELETE FROM calendars WHERE account = ?1 AND url = ?2",
                    params![account, url],
                )?;
            }
        }
        Ok(())
    }

    // -- Calendar events ----------------------------------------------

    /// Insert or replace a synced calendar event, keyed by
    /// `(account, calendar_url, href)`. Sets `local_status` to `synced`
    /// and clears any prior `local_error` — except for a row already marked
    /// [`CAL_STATUS_PENDING_DELETE`], which stays pending: the user has
    /// deleted it and the `DELETE` simply hasn't reached the server yet, so
    /// a read pass that still sees it server-side must not resurrect it.
    /// This is the "the server is now
    /// the source of truth for this row" path, used by `calsync` after a
    /// successful fetch. Compresses the raw ICS text the same way
    /// `emails.text_body_zstd` etc. are compressed.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_calendar_event(
        &self,
        account: &str,
        calendar_url: &str,
        href: &str,
        etag: Option<&str>,
        event: &VEvent,
        raw_ics: &str,
    ) -> Result<i64> {
        let organizer = event.organizer.clone().unwrap_or_default();
        let raw_compressed = compress(raw_ics.as_bytes());
        self.conn.execute(
            "INSERT INTO calendar_events (
                 account, calendar_url, href, etag, uid, summary, description, location,
                 status, organizer, dtstart_utc, dtend_utc, all_day, tzid, rrule, sequence,
                 raw_ics_zstd, local_status, local_error
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,NULL)
             ON CONFLICT(account, calendar_url, href) DO UPDATE SET
                 etag = excluded.etag,
                 uid = excluded.uid,
                 summary = excluded.summary,
                 description = excluded.description,
                 location = excluded.location,
                 status = excluded.status,
                 organizer = excluded.organizer,
                 dtstart_utc = excluded.dtstart_utc,
                 dtend_utc = excluded.dtend_utc,
                 all_day = excluded.all_day,
                 tzid = excluded.tzid,
                 rrule = excluded.rrule,
                 sequence = excluded.sequence,
                 raw_ics_zstd = excluded.raw_ics_zstd,
                 local_status = CASE
                     WHEN calendar_events.local_status = ?19 THEN ?19
                     ELSE excluded.local_status
                 END,
                 local_error = NULL",
            params![
                account,
                calendar_url,
                href,
                etag,
                event.uid,
                event.summary,
                event.description,
                event.location,
                event.status.as_ics(),
                organizer,
                event.dtstart.utc.timestamp(),
                event.dtend.utc.timestamp(),
                event.dtstart.all_day as i32,
                event.dtstart.tzid,
                event.rrule,
                event.sequence,
                raw_compressed,
                CAL_STATUS_SYNCED,
                CAL_STATUS_PENDING_DELETE,
            ],
        )?;
        self.conn.query_row(
            "SELECT id FROM calendar_events WHERE account = ?1 AND calendar_url = ?2 AND href = ?3",
            params![account, calendar_url, href],
            |row| row.get(0),
        ).context("reading back id of just-upserted calendar_events row")
    }

    /// Insert a brand-new, not-yet-uploaded local event. `href` is a
    /// synthetic placeholder (`local:{uid}`) until the daemon's calendar
    /// sync thread successfully creates it server-side and calls
    /// [`mark_calendar_event_synced`](Self::mark_calendar_event_synced)
    /// with the real href/etag.
    pub fn insert_local_calendar_event(
        &self,
        account: &str,
        calendar_url: &str,
        event: &VEvent,
    ) -> Result<i64> {
        let href = format!("{}{}", LOCAL_HREF_PREFIX, event.uid);
        let ics = event.to_new_ics(Utc::now());
        self.conn.execute(
            "INSERT INTO calendar_events (
                 account, calendar_url, href, etag, uid, summary, description, location,
                 status, organizer, dtstart_utc, dtend_utc, all_day, tzid, rrule, sequence,
                 raw_ics_zstd, local_status
             ) VALUES (?1,?2,?3,NULL,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                account,
                calendar_url,
                href,
                event.uid,
                event.summary,
                event.description,
                event.location,
                event.status.as_ics(),
                event.organizer.clone().unwrap_or_default(),
                event.dtstart.utc.timestamp(),
                event.dtend.utc.timestamp(),
                event.dtstart.all_day as i32,
                event.dtstart.tzid,
                event.rrule,
                event.sequence,
                compress(ics.as_bytes()),
                CAL_STATUS_PENDING_CREATE,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Apply a local edit to an already-synced (or already-pending) event
    /// and mark it `pending_update` (unless it's still `pending_create`,
    /// in which case it stays `pending_create` — there's nothing to
    /// "update" server-side yet). Rebuilds the stored raw ICS via
    /// [`VEvent::to_ics`] so unmodeled properties survive the edit.
    pub fn apply_local_calendar_edit(
        &self,
        id: i64,
        edits: &crate::calendar::EventEdits,
    ) -> Result<()> {
        let row = self
            .get_calendar_event_by_id(id)?
            .context("calendar event not found")?;
        let event = row.to_vevent()?;
        let new_ics = event
            .to_ics(edits, Utc::now())
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let updated = crate::calendar::parse_vevents(&new_ics)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .into_iter()
            .next()
            .context("re-parsing edited event produced no VEVENT")?;
        let next_status = if row.local_status == CAL_STATUS_PENDING_CREATE {
            CAL_STATUS_PENDING_CREATE
        } else {
            CAL_STATUS_PENDING_UPDATE
        };
        self.conn.execute(
            "UPDATE calendar_events SET
                 summary = ?1, description = ?2, location = ?3, status = ?4,
                 dtstart_utc = ?5, dtend_utc = ?6, all_day = ?7, tzid = ?8, rrule = ?9,
                 sequence = ?10, raw_ics_zstd = ?11, local_status = ?12, local_error = NULL
             WHERE id = ?13",
            params![
                updated.summary,
                updated.description,
                updated.location,
                updated.status.as_ics(),
                updated.dtstart.utc.timestamp(),
                updated.dtend.utc.timestamp(),
                updated.dtstart.all_day as i32,
                updated.dtstart.tzid,
                updated.rrule,
                updated.sequence,
                compress(new_ics.as_bytes()),
                next_status,
                id,
            ],
        )?;
        Ok(())
    }

    /// Mark an event for deletion: if it was only ever local
    /// (`pending_create`, never uploaded), delete the row outright since
    /// there's nothing server-side to reconcile; otherwise flag it
    /// `pending_delete` for the sync thread to `DELETE` remotely before
    /// removing the local row.
    ///
    /// Returns whether a remote `DELETE` is still owed — `false` means the
    /// row is already gone and the caller must **not** queue a mutation for
    /// `id`. That matters because SQLite reuses rowids: a queued
    /// `CalMutation::Delete { local_id }` naming a row that no longer
    /// exists can be resolved against a *different*, newly created event by
    /// the time the sync thread drains it.
    pub fn mark_calendar_event_pending_delete(&self, id: i64) -> Result<bool> {
        let row = self
            .get_calendar_event_by_id(id)?
            .context("calendar event not found")?;
        if row.local_status == CAL_STATUS_PENDING_CREATE {
            self.conn
                .execute("DELETE FROM calendar_events WHERE id = ?1", params![id])?;
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE calendar_events SET local_status = ?1, local_error = NULL WHERE id = ?2",
            params![CAL_STATUS_PENDING_DELETE, id],
        )?;
        Ok(true)
    }

    pub fn mark_calendar_event_synced(
        &self,
        id: i64,
        href: &str,
        etag: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE calendar_events SET href = ?1, etag = ?2, local_status = ?3, local_error = NULL
             WHERE id = ?4",
            params![href, etag, CAL_STATUS_SYNCED, id],
        )?;
        Ok(())
    }

    pub fn delete_calendar_event_row(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM calendar_events WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn mark_calendar_event_conflict(&self, id: i64, message: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE calendar_events SET local_status = ?1, local_error = ?2 WHERE id = ?3",
            params![CAL_STATUS_CONFLICT, message, id],
        )?;
        Ok(())
    }

    /// Delete by `(account, calendar_url, href)` — used when the sync
    /// thread learns a resource was deleted server-side (a sync-collection
    /// `404`, or one missing from a full listing).
    pub fn delete_calendar_event_by_href(
        &self,
        account: &str,
        calendar_url: &str,
        href: &str,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM calendar_events WHERE account = ?1 AND calendar_url = ?2 AND href = ?3",
            params![account, calendar_url, href],
        )?;
        Ok(())
    }

    pub fn get_calendar_event_by_id(&self, id: i64) -> Result<Option<CalendarEventRow>> {
        let mut stmt = self.conn.prepare(CALENDAR_EVENT_SELECT_BY_ID)?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(calendar_event_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Look up a row by its exact `(account, calendar_url, href)` key —
    /// used by `caldav_server` for GET/PUT/DELETE, where the request path
    /// itself is the href.
    pub fn get_calendar_event_by_href(
        &self,
        account: &str,
        calendar_url: &str,
        href: &str,
    ) -> Result<Option<CalendarEventRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "{} WHERE account = ?1 AND calendar_url = ?2 AND href = ?3",
            CALENDAR_EVENT_SELECT_BASE
        ))?;
        let mut rows = stmt.query(params![account, calendar_url, href])?;
        match rows.next()? {
            Some(row) => Ok(Some(calendar_event_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Events for `account` (or every account, when `None`) overlapping
    /// `[start_utc, end_utc)`, ordered by start time — the query backing
    /// jacal's day/3-day/week/month views.
    ///
    /// A recurring event (non-null `rrule`) is returned regardless of
    /// whether its *master* `dtstart_utc`/`dtend_utc` overlaps the window,
    /// since a later occurrence can fall inside the window even when the
    /// master doesn't — callers that care about individual occurrences
    /// (not just "is this recurring series relevant at all") must expand
    /// each returned recurring row with [`crate::calendar::expand_occurrences`]
    /// and filter to the same window themselves.
    ///
    /// Rows marked [`CAL_STATUS_PENDING_DELETE`] are excluded: the user has
    /// already deleted them and is only waiting on the server round trip,
    /// so they must disappear from the UI (and stop firing alarms)
    /// immediately rather than lingering until the next sync lands.
    pub fn get_calendar_events_in_range(
        &self,
        account: Option<&str>,
        start_utc: i64,
        end_utc: i64,
    ) -> Result<Vec<CalendarEventRow>> {
        let mut out = Vec::new();
        match account {
            Some(acct) => {
                let mut stmt = self.conn.prepare(&format!(
                    "{} WHERE account = ?1 AND local_status != ?4
                     AND (rrule IS NOT NULL OR (dtstart_utc < ?2 AND dtend_utc > ?3))
                     ORDER BY dtstart_utc",
                    CALENDAR_EVENT_SELECT_BASE
                ))?;
                let rows = stmt.query_map(
                    params![acct, end_utc, start_utc, CAL_STATUS_PENDING_DELETE],
                    calendar_event_from_row,
                )?;
                for r in rows {
                    out.push(r?);
                }
            }
            None => {
                let mut stmt = self.conn.prepare(&format!(
                    "{} WHERE local_status != ?3
                     AND (rrule IS NOT NULL OR (dtstart_utc < ?1 AND dtend_utc > ?2))
                     ORDER BY dtstart_utc",
                    CALENDAR_EVENT_SELECT_BASE
                ))?;
                let rows = stmt.query_map(
                    params![end_utc, start_utc, CAL_STATUS_PENDING_DELETE],
                    calendar_event_from_row,
                )?;
                for r in rows {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    /// Local writes queued for the sync thread to apply
    /// (`pending_create`/`pending_update`/`pending_delete`), for a given
    /// account.
    pub fn get_pending_calendar_writes(&self, account: &str) -> Result<Vec<CalendarEventRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "{} WHERE account = ?1 AND local_status IN (?2, ?3, ?4) ORDER BY id",
            CALENDAR_EVENT_SELECT_BASE
        ))?;
        let rows = stmt.query_map(
            params![
                account,
                CAL_STATUS_PENDING_CREATE,
                CAL_STATUS_PENDING_UPDATE,
                CAL_STATUS_PENDING_DELETE
            ],
            calendar_event_from_row,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // -- CalDAV server-side operations (caldav_server.rs) ----------------
    //
    // Everything below is for jamaild's *inbound* CalDAV server role
    // (serving a locally-hosted calendar to any CalDAV client), which is
    // authoritative for the data it holds — unlike the PENDING_* queue
    // above (jamaild acting as an outbound CalDAV *client*, queuing writes
    // for a remote server to accept), a server-side write applies directly
    // and is immediately `CAL_STATUS_SYNCED`. Every write also appends to
    // `calendar_sync_log`, which is what answers "what changed since
    // token N" for `Self::server_sync_changes`.

    fn append_sync_log(
        &self,
        account: &str,
        calendar_url: &str,
        href: &str,
        deleted: bool,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO calendar_sync_log (account, calendar_url, href, deleted) VALUES (?1,?2,?3,?4)",
            params![account, calendar_url, href, deleted as i32],
        )?;
        Ok(())
    }

    /// All events currently in `calendar_url` for `account` — the
    /// unfiltered listing behind a `calendar-query` REPORT (this server
    /// does not evaluate time-range/comp filters; see `caldav_server`
    /// module docs).
    pub fn server_list_events(
        &self,
        account: &str,
        calendar_url: &str,
    ) -> Result<Vec<CalendarEventRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "{} WHERE account = ?1 AND calendar_url = ?2 ORDER BY dtstart_utc",
            CALENDAR_EVENT_SELECT_BASE
        ))?;
        let rows = stmt.query_map(params![account, calendar_url], calendar_event_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Apply an inbound `PUT` (create-or-update) to a server-hosted
    /// calendar, honoring `If-Match`/`If-None-Match: *` preconditions
    /// exactly like a real CalDAV server must: a stale/absent precondition
    /// never silently overwrites another writer's change. `ics` must
    /// contain exactly one `VEVENT`. Returns `Ok(Err(PutError::..))` (not
    /// `Err`) for an expected, client-correctable failure (precondition
    /// failed, unparseable body) so callers can map it straight to an HTTP
    /// status without also having to distinguish it from a real I/O error.
    pub fn server_put_event(
        &self,
        account: &str,
        calendar_url: &str,
        href: &str,
        ics: &str,
        if_match: Option<&str>,
        if_none_match_star: bool,
    ) -> Result<std::result::Result<PutOutcome, PutError>> {
        let existing = self.get_calendar_event_by_href(account, calendar_url, href)?;

        if if_none_match_star && existing.is_some() {
            return Ok(Err(PutError::PreconditionFailed(
                "a resource already exists at this location".to_string(),
            )));
        }
        if let Some(want) = if_match {
            let current = existing.as_ref().and_then(|e| e.etag.clone());
            if current.as_deref() != Some(want) {
                return Ok(Err(PutError::PreconditionFailed(format!(
                    "ETag mismatch: client sent {}, current is {:?}",
                    want, current
                ))));
            }
        }

        let event = match crate::calendar::parse_vevents(ics) {
            Ok(events) => match events.into_iter().next() {
                Some(e) => e,
                None => {
                    return Ok(Err(PutError::InvalidBody(
                        "request body contained no VEVENT".to_string(),
                    )));
                }
            },
            Err(e) => return Ok(Err(PutError::InvalidBody(e.to_string()))),
        };

        let new_etag = compute_etag(ics);
        self.upsert_calendar_event(account, calendar_url, href, Some(&new_etag), &event, ics)?;
        self.append_sync_log(account, calendar_url, href, false)?;
        Ok(Ok(if existing.is_some() {
            PutOutcome::Updated(new_etag)
        } else {
            PutOutcome::Created(new_etag)
        }))
    }

    /// Apply an inbound `DELETE` to a server-hosted calendar, honoring
    /// `If-Match` the same way [`Self::server_put_event`] does.
    pub fn server_delete_event(
        &self,
        account: &str,
        calendar_url: &str,
        href: &str,
        if_match: Option<&str>,
    ) -> Result<std::result::Result<(), DeleteError>> {
        let Some(existing) = self.get_calendar_event_by_href(account, calendar_url, href)? else {
            return Ok(Err(DeleteError::NotFound));
        };
        if let Some(want) = if_match
            && existing.etag.as_deref() != Some(want)
        {
            return Ok(Err(DeleteError::PreconditionFailed(format!(
                "ETag mismatch: client sent {}, current is {:?}",
                want, existing.etag
            ))));
        }
        self.conn.execute(
            "DELETE FROM calendar_events WHERE account = ?1 AND calendar_url = ?2 AND href = ?3",
            params![account, calendar_url, href],
        )?;
        self.append_sync_log(account, calendar_url, href, true)?;
        Ok(Ok(()))
    }

    /// Finish a local write (`pending_create`/`pending_update`/
    /// `pending_delete`) to a calendar this daemon *hosts itself* — one in
    /// the [`SERVER_CALENDAR_URL_PREFIX`] namespace (see `caldav_server`).
    ///
    /// There is no remote server such a write could be pushed to; the local
    /// row already *is* the authoritative copy. Committing it therefore
    /// means clearing the pending status — giving a freshly created row
    /// `new_href` and a real ETag in place of its [`LOCAL_HREF_PREFIX`]
    /// placeholder — and appending the `calendar_sync_log` entry that tells
    /// CalDAV clients subscribed to this server that something changed.
    /// Without it the write would sit pending forever, queued for a CalDAV
    /// client that has no URL to send it to.
    pub fn server_commit_local_write(&self, id: i64, new_href: &str) -> Result<()> {
        let row = self
            .get_calendar_event_by_id(id)?
            .context("calendar event not found")?;
        if row.local_status == CAL_STATUS_PENDING_DELETE {
            self.conn
                .execute("DELETE FROM calendar_events WHERE id = ?1", params![id])?;
            self.append_sync_log(&row.account, &row.calendar_url, &row.href, true)?;
            return Ok(());
        }
        let ics = row
            .raw_ics
            .clone()
            .context("calendar event has no raw ICS to publish")?;
        let href = if row.href.starts_with(LOCAL_HREF_PREFIX) {
            new_href.to_string()
        } else {
            row.href.clone()
        };
        let etag = compute_etag(&ics);
        self.conn.execute(
            "UPDATE calendar_events
                SET href = ?1, etag = ?2, local_status = ?3, local_error = NULL
              WHERE id = ?4",
            params![href, etag, CAL_STATUS_SYNCED, id],
        )?;
        self.append_sync_log(&row.account, &row.calendar_url, &href, false)?;
        Ok(())
    }

    /// The current sync-token/CTag for `calendar_url` — the highest
    /// `calendar_sync_log.id` recorded for it, or `0` if it has never
    /// changed since being created.
    pub fn server_current_token(&self, account: &str, calendar_url: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM calendar_sync_log WHERE account = ?1 AND calendar_url = ?2",
            params![account, calendar_url],
            |row| row.get(0),
        )?)
    }

    /// Everything that changed in `calendar_url` since `since` (a prior
    /// [`Self::server_current_token`]/sync-token value; `0` for "from the
    /// beginning"), collapsed to one entry per `href` reflecting its
    /// latest state. Backs a `sync-collection` REPORT. Returns the entries
    /// plus the new token to hand back to the client.
    ///
    /// Relies on a SQLite-specific (not standard-SQL) guarantee: when a
    /// query's result has exactly one `MIN()`/`MAX()` aggregate alongside
    /// other bare columns, those bare columns are taken from the same row
    /// that produced the extreme value — see
    /// <https://www.sqlite.org/lang_select.html#bareagg>. That's what
    /// makes `deleted` below correctly reflect the *latest* logged state
    /// for each href, not an arbitrary one among duplicates.
    pub fn server_sync_changes(
        &self,
        account: &str,
        calendar_url: &str,
        since: i64,
    ) -> Result<(Vec<SyncLogEntry>, i64)> {
        let mut stmt = self.conn.prepare(
            "SELECT href, deleted, MAX(id) AS maxid FROM calendar_sync_log
             WHERE account = ?1 AND calendar_url = ?2 AND id > ?3
             GROUP BY href
             ORDER BY maxid",
        )?;
        let mut out = Vec::new();
        let mut max_id = since;
        let rows = stmt.query_map(params![account, calendar_url, since], |row| {
            let href: String = row.get(0)?;
            let deleted: i64 = row.get(1)?;
            let id: i64 = row.get(2)?;
            Ok((href, deleted != 0, id))
        })?;
        for r in rows {
            let (href, deleted, id) = r?;
            out.push(SyncLogEntry { href, deleted });
            if id > max_id {
                max_id = id;
            }
        }
        Ok((out, max_id))
    }
}

/// Outcome of a successful [`MailDb::server_put_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    Created(String),
    Updated(String),
}

/// An expected, client-correctable failure of
/// [`MailDb::server_put_event`] — maps directly to an HTTP status in
/// `caldav_server`, not a 500.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutError {
    PreconditionFailed(String),
    InvalidBody(String),
}

/// An expected, client-correctable failure of
/// [`MailDb::server_delete_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteError {
    NotFound,
    PreconditionFailed(String),
}

/// One row of [`MailDb::server_sync_changes`]'s result: `deleted` means
/// the resource at `href` no longer exists (a sync-collection response
/// represents this as a bare `404` for that href, not a fetchable body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncLogEntry {
    pub href: String,
    pub deleted: bool,
}

/// A deterministic, content-derived ETag: any change to `ics` changes the
/// ETag, and identical content produces the same one — both are correct
/// per HTTP's ETag semantics (RFC 7232 §2.3). Uses `DefaultHasher` (already
/// in `std`, not a cryptographic hash) since ETags need only be a good
/// change-detector here, not collision-resistant against an adversary.
fn compute_etag(ics: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    ics.hash(&mut hasher);
    format!("\"{:x}\"", hasher.finish())
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct LocalMessage {
    pub id: i64,
    pub account: String,
    pub status: String,
    pub from_addr: String,
    pub to_addr: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: String,
    pub refs: String,
    pub created_at: String,
    pub sent_at: Option<String>,
    pub error: Option<String>,
    pub upload_status: String,
    pub upload_error: Option<String>,
}

/// Human-readable in-flight/error state for a Drafts/Sent list row, or
/// `None` when there's nothing noteworthy to show beyond the plain
/// draft/sent marker already conveyed by the row's unread styling.
pub fn local_message_status_label(m: &LocalMessage) -> Option<String> {
    match m.status.as_str() {
        STATUS_SENDING => Some("Sending…".to_string()),
        STATUS_SEND_ERROR => Some(format!(
            "Send failed: {}",
            m.error.as_deref().unwrap_or("unknown error")
        )),
        STATUS_SENT => match m.upload_status.as_str() {
            UPLOAD_NONE => None,
            UPLOAD_PENDING => Some("Uploading to Sent…".to_string()),
            UPLOAD_UPLOADED => Some("Uploaded".to_string()),
            UPLOAD_ERROR => Some(format!(
                "Upload failed: {}",
                m.upload_error.as_deref().unwrap_or("unknown error")
            )),
            _ => None,
        },
        STATUS_DRAFT => match m.upload_status.as_str() {
            UPLOAD_NONE => None,
            UPLOAD_PENDING => Some("Uploading draft…".to_string()),
            UPLOAD_UPLOADED => Some("Draft uploaded".to_string()),
            UPLOAD_ERROR => Some(format!(
                "Draft upload failed: {}",
                m.upload_error.as_deref().unwrap_or("unknown error")
            )),
            _ => None,
        },
        _ => None,
    }
}

fn local_message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LocalMessage> {
    Ok(LocalMessage {
        id: row.get(0)?,
        account: row.get(1)?,
        status: row.get(2)?,
        from_addr: row.get(3)?,
        to_addr: row.get(4)?,
        cc: row.get(5)?,
        bcc: row.get(6)?,
        subject: row.get(7)?,
        body: row.get(8)?,
        in_reply_to: row.get(9)?,
        refs: row.get(10)?,
        created_at: row.get(11)?,
        sent_at: row.get(12)?,
        error: row.get(13)?,
        upload_status: row.get(14)?,
        upload_error: row.get(15)?,
    })
}

const CALENDAR_EVENT_SELECT_BASE: &str = "SELECT id, account, calendar_url, href, etag, uid, \
    summary, description, location, status, organizer, dtstart_utc, dtend_utc, all_day, tzid, \
    rrule, sequence, raw_ics_zstd, local_status, local_error FROM calendar_events";

const CALENDAR_EVENT_SELECT_BY_ID: &str = "SELECT id, account, calendar_url, href, etag, uid, \
    summary, description, location, status, organizer, dtstart_utc, dtend_utc, all_day, tzid, \
    rrule, sequence, raw_ics_zstd, local_status, local_error FROM calendar_events WHERE id = ?1";

fn calendar_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarEventRow> {
    let raw_ics_zstd: Option<Vec<u8>> = row.get(17)?;
    let raw_ics = raw_ics_zstd.map(|blob| String::from_utf8_lossy(&decompress(&blob)).into_owned());
    let all_day: i64 = row.get(13)?;
    Ok(CalendarEventRow {
        id: row.get(0)?,
        account: row.get(1)?,
        calendar_url: row.get(2)?,
        href: row.get(3)?,
        etag: row.get(4)?,
        uid: row.get(5)?,
        summary: row.get(6)?,
        description: row.get(7)?,
        location: row.get(8)?,
        status: row.get(9)?,
        organizer: row.get(10)?,
        dtstart_utc: row.get(11)?,
        dtend_utc: row.get(12)?,
        all_day: all_day != 0,
        tzid: row.get(14)?,
        rrule: row.get(15)?,
        sequence: row.get(16)?,
        raw_ics,
        local_status: row.get(18)?,
        local_error: row.get(19)?,
    })
}

#[cfg(test)]
mod local_message_tests {
    use super::*;

    fn msg(status: &str, upload_status: &str) -> LocalMessage {
        LocalMessage {
            id: 1,
            account: "personal".to_string(),
            status: status.to_string(),
            from_addr: "alice@example.com".to_string(),
            to_addr: "bob@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hi".to_string(),
            body: "Hello".to_string(),
            in_reply_to: String::new(),
            refs: String::new(),
            created_at: "2026-01-01T00:00:00+00:00".to_string(),
            sent_at: None,
            error: None,
            upload_status: upload_status.to_string(),
            upload_error: None,
        }
    }

    #[test]
    fn plain_draft_has_no_status_label() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_DRAFT, UPLOAD_NONE)),
            None
        );
    }

    #[test]
    fn sending_draft_shows_in_flight_label() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_SENDING, UPLOAD_NONE)),
            Some("Sending…".to_string())
        );
    }

    #[test]
    fn send_error_label_includes_error_text() {
        let mut m = msg(STATUS_SEND_ERROR, UPLOAD_NONE);
        m.error = Some("connection refused".to_string());
        assert_eq!(
            local_message_status_label(&m),
            Some("Send failed: connection refused".to_string())
        );
    }

    #[test]
    fn sent_with_no_upload_configured_has_no_label() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_SENT, UPLOAD_NONE)),
            None
        );
    }

    #[test]
    fn sent_upload_pending_shows_uploading_label() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_SENT, UPLOAD_PENDING)),
            Some("Uploading to Sent…".to_string())
        );
    }

    #[test]
    fn sent_upload_error_includes_error_text() {
        let mut m = msg(STATUS_SENT, UPLOAD_ERROR);
        m.upload_error = Some("folder does not exist".to_string());
        assert_eq!(
            local_message_status_label(&m),
            Some("Upload failed: folder does not exist".to_string())
        );
    }

    #[test]
    fn draft_upload_states_produce_distinct_labels() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_DRAFT, UPLOAD_PENDING)),
            Some("Uploading draft…".to_string())
        );
        assert_eq!(
            local_message_status_label(&msg(STATUS_DRAFT, UPLOAD_UPLOADED)),
            Some("Draft uploaded".to_string())
        );
    }

    #[test]
    fn drafts_query_includes_in_flight_and_errored_rows() {
        let db = MailDb::open_in_memory().unwrap();
        let id1 = db
            .save_draft("personal", "a@b.com", "c@d.com", "", "", "s1", "b1", "", "")
            .unwrap();
        let id2 = db
            .save_draft("personal", "a@b.com", "c@d.com", "", "", "s2", "b2", "", "")
            .unwrap();
        let id3 = db
            .save_draft("personal", "a@b.com", "c@d.com", "", "", "s3", "b3", "", "")
            .unwrap();
        db.mark_draft_sending(id2).unwrap();
        db.mark_draft_send_error(id3, "timeout").unwrap();

        let drafts = db.get_drafts("personal").unwrap();
        assert_eq!(drafts.len(), 3);

        db.mark_draft_sent(id1).unwrap();
        let drafts = db.get_drafts("personal").unwrap();
        assert_eq!(drafts.len(), 2);
        let sent = db.get_sent("personal").unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].id, id1);
    }

    #[test]
    fn upload_status_round_trips_through_db() {
        let db = MailDb::open_in_memory().unwrap();
        let id = db
            .save_draft("personal", "a@b.com", "c@d.com", "", "", "s", "b", "", "")
            .unwrap();
        db.mark_draft_sent(id).unwrap();
        db.set_upload_pending(id).unwrap();
        let sent = db.get_sent("personal").unwrap();
        assert_eq!(sent[0].upload_status, UPLOAD_PENDING);

        db.set_upload_error(id, "no such mailbox").unwrap();
        let sent = db.get_sent("personal").unwrap();
        assert_eq!(sent[0].upload_status, UPLOAD_ERROR);
        assert_eq!(sent[0].upload_error.as_deref(), Some("no such mailbox"));

        db.set_upload_success(id).unwrap();
        let sent = db.get_sent("personal").unwrap();
        assert_eq!(sent[0].upload_status, UPLOAD_UPLOADED);
        assert_eq!(sent[0].upload_error, None);
    }
}

#[cfg(test)]
mod calendar_migration_tests {
    use super::*;

    #[test]
    fn legacy_v4_database_has_no_calendar_tables_yet() {
        let db = MailDb::open_in_memory_legacy_v4().unwrap();
        assert_eq!(db.schema_version_for_test(), 4);
        let has_calendars: bool = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='calendars'",
                [],
                |row| row.get::<_, i32>(0),
            )
            .map(|c| c > 0)
            .unwrap();
        assert!(!has_calendars);
    }

    #[test]
    fn upgrading_v4_to_v5_preserves_existing_mail_data() {
        let db = MailDb::open_in_memory_legacy_v4().unwrap();

        // Seed real mail data at v4, before the calendar tables exist.
        db.begin_tx().unwrap();
        let email_id = db
            .store_email_core(
                "personal",
                "INBOX",
                42,
                "alice@example.com",
                "bob@example.com",
                "Hello",
                &Local::now(),
                true,
                "preview",
                "body text",
                None,
                "",
                "msg-1",
                "",
                "",
                false,
                &[],
            )
            .unwrap();
        db.commit_tx().unwrap();
        db.save_draft(
            "personal",
            "a@b.com",
            "",
            "",
            "",
            "draft subj",
            "draft body",
            "",
            "",
        )
        .unwrap();

        db.upgrade_for_test().unwrap();

        assert_eq!(db.schema_version_for_test(), 6);

        // Existing mail survived the upgrade untouched.
        let emails = db.get_email_list("personal", "INBOX").unwrap();
        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].id, email_id);
        assert_eq!(emails[0].subject, "Hello");
        let drafts = db.get_drafts("personal").unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].subject, "draft subj");

        // And the calendar tables now exist and are usable.
        db.upsert_calendar(
            "personal",
            "https://cal.example.com/dav/personal/",
            "Personal",
        )
        .unwrap();
        let calendars = db.get_calendars("personal").unwrap();
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].display_name, "Personal");
    }

    #[test]
    fn running_the_upgrade_twice_is_a_harmless_no_op() {
        let db = MailDb::open_in_memory_legacy_v4().unwrap();
        db.upgrade_for_test().unwrap();
        // Idempotency matters: jamaild and jamail both call MailDb::open()
        // independently and could race on first-run upgrade.
        db.upgrade_for_test().unwrap();
        assert_eq!(db.schema_version_for_test(), 6);
    }

    #[test]
    fn a_fresh_database_is_unaffected_by_upgrade() {
        let db = MailDb::open_in_memory().unwrap();
        assert_eq!(db.schema_version_for_test(), 6);
        db.upgrade_for_test().unwrap();
        assert_eq!(db.schema_version_for_test(), 6);
    }

    #[test]
    fn upgrading_v5_to_v6_preserves_existing_calendar_data() {
        let db = MailDb::open_in_memory_legacy_v5().unwrap();
        assert_eq!(db.schema_version_for_test(), 5);

        // Seed real calendar data at v5, before calendar_sync_log exists.
        db.upsert_calendar(
            "personal",
            "https://cal.example.com/dav/personal/",
            "Personal",
        )
        .unwrap();
        let event = crate::calendar::parse_vevents(
            "BEGIN:VEVENT\r\nUID:pre-v6\r\nDTSTART:20240115T090000Z\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n",
        )
        .unwrap()
        .remove(0);
        let event_id = db
            .upsert_calendar_event(
                "personal",
                "https://cal.example.com/dav/personal/",
                "/personal/pre-v6.ics",
                Some("\"v1\""),
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();

        db.upgrade_for_test().unwrap();
        assert_eq!(db.schema_version_for_test(), 6);

        // Existing calendar data survived the upgrade untouched.
        let calendars = db.get_calendars("personal").unwrap();
        assert_eq!(calendars.len(), 1);
        let row = db.get_calendar_event_by_id(event_id).unwrap().unwrap();
        assert_eq!(row.uid, "pre-v6");

        // And the new sync log table now exists and is usable.
        let (changes, token) = db
            .server_sync_changes("personal", "https://cal.example.com/dav/personal/", 0)
            .unwrap();
        assert!(changes.is_empty());
        assert_eq!(token, 0);
    }
}

#[cfg(test)]
mod calendar_event_tests {
    use super::*;
    use crate::calendar::{EventEdits, EventStatus, EventTime, parse_vevents};
    use chrono::{TimeZone, Utc};

    const ACCOUNT: &str = "personal";
    const CAL_URL: &str = "https://cal.example.com/dav/personal/";

    fn sample_event(uid: &str) -> VEvent {
        parse_vevents(&format!(
            "BEGIN:VEVENT\r\nUID:{}\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n",
            uid
        ))
        .unwrap()
        .remove(0)
    }

    #[test]
    fn prune_never_removes_a_locally_hosted_calendar() {
        // `keep_urls` comes purely from remote discovery, so a `server:`
        // calendar can never appear in it — pruning on that basis would
        // wipe the inbound CalDAV server's own calendar, and every event
        // in it, on the first sync cycle after startup.
        let db = MailDb::open_in_memory().unwrap();
        db.upsert_calendar(ACCOUNT, CAL_URL, "Personal").unwrap();
        db.upsert_calendar(ACCOUNT, "server:personal", "Default")
            .unwrap();
        let event = sample_event("hosted");
        db.upsert_calendar_event(
            ACCOUNT,
            "server:personal",
            "/dav/personal/calendars/default/hosted.ics",
            Some("\"v1\""),
            &event,
            event.raw.as_ref().unwrap(),
        )
        .unwrap();

        db.prune_calendars_not_in(ACCOUNT, &[CAL_URL.to_string()])
            .unwrap();

        let urls: Vec<String> = db
            .get_calendars(ACCOUNT)
            .unwrap()
            .into_iter()
            .map(|c| c.url)
            .collect();
        assert!(urls.contains(&"server:personal".to_string()));
        assert_eq!(
            db.get_calendar_events_in_range(Some(ACCOUNT), 0, i64::MAX)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn server_commit_local_write_publishes_a_created_event_and_logs_it() {
        let db = MailDb::open_in_memory().unwrap();
        db.upsert_calendar(ACCOUNT, "server:personal", "Default")
            .unwrap();
        let event = sample_event("hosted-new");
        let id = db
            .insert_local_calendar_event(ACCOUNT, "server:personal", &event)
            .unwrap();
        let before = db.server_current_token(ACCOUNT, "server:personal").unwrap();

        db.server_commit_local_write(id, "/dav/personal/calendars/default/hosted-new.ics")
            .unwrap();

        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        assert_eq!(row.local_status, CAL_STATUS_SYNCED);
        assert_eq!(row.href, "/dav/personal/calendars/default/hosted-new.ics");
        assert!(row.etag.is_some());
        assert!(
            db.server_current_token(ACCOUNT, "server:personal").unwrap() > before,
            "subscribed CalDAV clients learn about the change from the sync log"
        );
    }

    #[test]
    fn server_commit_local_write_removes_a_deleted_event_and_logs_it() {
        let db = MailDb::open_in_memory().unwrap();
        db.upsert_calendar(ACCOUNT, "server:personal", "Default")
            .unwrap();
        let event = sample_event("hosted-gone");
        let id = db
            .upsert_calendar_event(
                ACCOUNT,
                "server:personal",
                "/dav/personal/calendars/default/hosted-gone.ics",
                Some("\"v1\""),
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();
        db.mark_calendar_event_pending_delete(id).unwrap();
        let before = db.server_current_token(ACCOUNT, "server:personal").unwrap();

        db.server_commit_local_write(id, "/unused.ics").unwrap();

        assert!(db.get_calendar_event_by_id(id).unwrap().is_none());
        assert!(db.server_current_token(ACCOUNT, "server:personal").unwrap() > before);
    }

    #[test]
    fn upsert_and_fetch_calendar_round_trips() {
        let db = MailDb::open_in_memory().unwrap();
        db.upsert_calendar(ACCOUNT, CAL_URL, "Personal").unwrap();
        db.set_calendar_sync_state(ACCOUNT, CAL_URL, Some("\"ctag-1\""), Some("token-1"))
            .unwrap();
        let calendars = db.get_calendars(ACCOUNT).unwrap();
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].ctag.as_deref(), Some("\"ctag-1\""));
        assert_eq!(
            db.get_calendar_sync_token(ACCOUNT, CAL_URL)
                .unwrap()
                .as_deref(),
            Some("token-1")
        );

        // Rediscovery (upsert_calendar again) must not reset sync state.
        db.upsert_calendar(ACCOUNT, CAL_URL, "Personal").unwrap();
        assert_eq!(
            db.get_calendar_sync_token(ACCOUNT, CAL_URL)
                .unwrap()
                .as_deref(),
            Some("token-1")
        );
    }

    #[test]
    fn prune_calendars_removes_dropped_calendar_and_its_events() {
        let db = MailDb::open_in_memory().unwrap();
        db.upsert_calendar(ACCOUNT, CAL_URL, "Personal").unwrap();
        db.upsert_calendar(ACCOUNT, "https://cal.example.com/dav/work/", "Work")
            .unwrap();
        let event = sample_event("keep-cal-check");
        db.upsert_calendar_event(
            ACCOUNT,
            CAL_URL,
            "/dav/personal/x.ics",
            Some("\"e1\""),
            &event,
            "BEGIN:VEVENT\r\nUID:keep-cal-check\r\nEND:VEVENT\r\n",
        )
        .unwrap();

        db.prune_calendars_not_in(ACCOUNT, &[CAL_URL.to_string()])
            .unwrap();

        let calendars = db.get_calendars(ACCOUNT).unwrap();
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].url, CAL_URL);
        // The kept calendar's events must survive the prune.
        let events = db
            .get_calendar_events_in_range(Some(ACCOUNT), 0, i64::MAX)
            .unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn upsert_calendar_event_inserts_then_updates_in_place() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("evt-1");
        let raw = event.raw.clone().unwrap();
        let id1 = db
            .upsert_calendar_event(
                ACCOUNT,
                CAL_URL,
                "/dav/personal/evt-1.ics",
                Some("\"v1\""),
                &event,
                &raw,
            )
            .unwrap();

        let mut updated = event.clone();
        updated.summary = "Standup (moved)".to_string();
        let id2 = db
            .upsert_calendar_event(
                ACCOUNT,
                CAL_URL,
                "/dav/personal/evt-1.ics",
                Some("\"v2\""),
                &updated,
                &raw,
            )
            .unwrap();

        assert_eq!(
            id1, id2,
            "same href must update the same row, not insert a new one"
        );
        let row = db.get_calendar_event_by_id(id1).unwrap().unwrap();
        assert_eq!(row.summary, "Standup (moved)");
        assert_eq!(row.etag.as_deref(), Some("\"v2\""));
        assert_eq!(row.local_status, CAL_STATUS_SYNCED);
    }

    #[test]
    fn get_calendar_events_in_range_filters_by_overlap() {
        let db = MailDb::open_in_memory().unwrap();
        let jan15 = sample_event("in-range");
        db.upsert_calendar_event(
            ACCOUNT,
            CAL_URL,
            "/a.ics",
            None,
            &jan15,
            "BEGIN:VEVENT\r\nUID:in-range\r\nEND:VEVENT\r\n",
        )
        .unwrap();

        let mut later = sample_event("out-of-range");
        later.dtstart = EventTime::utc(Utc.with_ymd_and_hms(2024, 3, 1, 9, 0, 0).unwrap());
        later.dtend = EventTime::utc(Utc.with_ymd_and_hms(2024, 3, 1, 10, 0, 0).unwrap());
        db.upsert_calendar_event(
            ACCOUNT,
            CAL_URL,
            "/b.ics",
            None,
            &later,
            "BEGIN:VEVENT\r\nUID:out-of-range\r\nEND:VEVENT\r\n",
        )
        .unwrap();

        let range_start = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let range_end = Utc
            .with_ymd_and_hms(2024, 2, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let events = db
            .get_calendar_events_in_range(Some(ACCOUNT), range_start, range_end)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "in-range");
    }

    #[test]
    fn get_calendar_events_in_range_returns_recurring_rows_regardless_of_master_date() {
        // A recurring event whose own master DTSTART/DTEND is nowhere near
        // the queried window must still come back — its later occurrences
        // (computed by `calendar::expand_occurrences`/
        // `calapp::expand_recurring_events`, not this query) can still
        // fall inside it.
        let db = MailDb::open_in_memory().unwrap();
        let mut recurring = sample_event("recurring");
        recurring.dtstart = EventTime::utc(Utc.with_ymd_and_hms(2020, 1, 7, 9, 0, 0).unwrap());
        recurring.dtend = EventTime::utc(Utc.with_ymd_and_hms(2020, 1, 7, 9, 30, 0).unwrap());
        recurring.rrule = Some("FREQ=WEEKLY".to_string());
        db.upsert_calendar_event(
            ACCOUNT,
            CAL_URL,
            "/recurring.ics",
            None,
            &recurring,
            "BEGIN:VEVENT\r\nUID:recurring\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n",
        )
        .unwrap();

        let range_start = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let range_end = Utc
            .with_ymd_and_hms(2024, 2, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let events = db
            .get_calendar_events_in_range(Some(ACCOUNT), range_start, range_end)
            .unwrap();
        assert!(events.iter().any(|e| e.uid == "recurring"));
    }

    #[test]
    fn insert_local_calendar_event_is_pending_create_with_local_href() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("new-local-1");
        let id = db
            .insert_local_calendar_event(ACCOUNT, CAL_URL, &event)
            .unwrap();
        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        assert_eq!(row.local_status, CAL_STATUS_PENDING_CREATE);
        assert_eq!(row.href, "local:new-local-1");
        assert!(row.etag.is_none());
        assert!(row.raw_ics.is_some());
    }

    #[test]
    fn apply_local_edit_bumps_to_pending_update_and_preserves_uid() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("edit-me");
        let id = db
            .upsert_calendar_event(
                ACCOUNT,
                CAL_URL,
                "/edit-me.ics",
                Some("\"v1\""),
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();

        db.apply_local_calendar_edit(
            id,
            &EventEdits {
                summary: Some("New title".to_string()),
                status: Some(EventStatus::Tentative),
                ..Default::default()
            },
        )
        .unwrap();

        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        assert_eq!(row.summary, "New title");
        assert_eq!(row.status, "TENTATIVE");
        assert_eq!(row.local_status, CAL_STATUS_PENDING_UPDATE);
        assert_eq!(row.uid, "edit-me");
        // The etag is untouched by a local edit — only the sync thread
        // learns a new etag after successfully PUTting to the server.
        assert_eq!(row.etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn editing_a_still_pending_create_event_stays_pending_create() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("still-new");
        let id = db
            .insert_local_calendar_event(ACCOUNT, CAL_URL, &event)
            .unwrap();
        db.apply_local_calendar_edit(
            id,
            &EventEdits {
                summary: Some("Edited before first sync".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        assert_eq!(row.local_status, CAL_STATUS_PENDING_CREATE);
    }

    #[test]
    fn deleting_a_synced_event_marks_pending_delete_not_removed() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("to-delete");
        let id = db
            .upsert_calendar_event(
                ACCOUNT,
                CAL_URL,
                "/to-delete.ics",
                Some("\"v1\""),
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();
        assert!(
            db.mark_calendar_event_pending_delete(id).unwrap(),
            "a synced event still owes the server a DELETE"
        );
        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        assert_eq!(row.local_status, CAL_STATUS_PENDING_DELETE);
    }

    #[test]
    fn an_event_marked_pending_delete_stops_being_listed() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("vanishing");
        let id = db
            .upsert_calendar_event(
                ACCOUNT,
                CAL_URL,
                "/vanishing.ics",
                Some("\"v1\""),
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();
        assert_eq!(
            db.get_calendar_events_in_range(Some(ACCOUNT), 0, i64::MAX)
                .unwrap()
                .len(),
            1
        );
        db.mark_calendar_event_pending_delete(id).unwrap();
        assert!(
            db.get_calendar_events_in_range(Some(ACCOUNT), 0, i64::MAX)
                .unwrap()
                .is_empty(),
            "a deleted event must leave the UI at once, not linger until the DELETE lands"
        );
        assert!(
            db.get_calendar_events_in_range(None, 0, i64::MAX)
                .unwrap()
                .is_empty(),
            "the all-accounts query must filter it out too"
        );
    }

    #[test]
    fn a_read_pass_does_not_resurrect_an_event_pending_deletion() {
        // The DELETE hasn't reached the server yet, so a sync in between
        // still sees the event and upserts it. That must not clear the
        // pending_delete flag: the row would come back in the UI and the
        // queued delete would find nothing to do.
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("racing");
        let id = db
            .upsert_calendar_event(
                ACCOUNT,
                CAL_URL,
                "/racing.ics",
                Some("\"v1\""),
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();
        db.mark_calendar_event_pending_delete(id).unwrap();
        db.upsert_calendar_event(
            ACCOUNT,
            CAL_URL,
            "/racing.ics",
            Some("\"v2\""),
            &event,
            event.raw.as_ref().unwrap(),
        )
        .unwrap();
        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        assert_eq!(row.local_status, CAL_STATUS_PENDING_DELETE);
        // The fresh ETag is still picked up — the delete's If-Match wants it.
        assert_eq!(row.etag.as_deref(), Some("\"v2\""));
    }

    #[test]
    fn deleting_a_never_synced_local_event_removes_it_outright() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("never-synced");
        let id = db
            .insert_local_calendar_event(ACCOUNT, CAL_URL, &event)
            .unwrap();
        assert!(
            !db.mark_calendar_event_pending_delete(id).unwrap(),
            "a never-uploaded event owes the server nothing; queueing a \
             Delete for its freed id could hit a later event that reuses it"
        );
        assert!(db.get_calendar_event_by_id(id).unwrap().is_none());
    }

    #[test]
    fn mark_calendar_event_conflict_records_message_and_status() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("conflict-me");
        let id = db
            .upsert_calendar_event(
                ACCOUNT,
                CAL_URL,
                "/conflict-me.ics",
                Some("\"v1\""),
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();
        db.mark_calendar_event_conflict(id, "412 Precondition Failed")
            .unwrap();
        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        assert_eq!(row.local_status, CAL_STATUS_CONFLICT);
        assert_eq!(row.local_error.as_deref(), Some("412 Precondition Failed"));
    }

    #[test]
    fn mark_calendar_event_synced_updates_href_and_etag() {
        let db = MailDb::open_in_memory().unwrap();
        let event = sample_event("just-created");
        let id = db
            .insert_local_calendar_event(ACCOUNT, CAL_URL, &event)
            .unwrap();
        db.mark_calendar_event_synced(
            id,
            "/dav/personal/just-created.ics",
            Some("\"server-etag\""),
        )
        .unwrap();
        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        assert_eq!(row.href, "/dav/personal/just-created.ics");
        assert_eq!(row.etag.as_deref(), Some("\"server-etag\""));
        assert_eq!(row.local_status, CAL_STATUS_SYNCED);
    }

    #[test]
    fn delete_calendar_event_by_href_removes_only_that_row() {
        let db = MailDb::open_in_memory().unwrap();
        let a = sample_event("a");
        let b = sample_event("b");
        db.upsert_calendar_event(
            ACCOUNT,
            CAL_URL,
            "/a.ics",
            None,
            &a,
            "BEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT\r\n",
        )
        .unwrap();
        db.upsert_calendar_event(
            ACCOUNT,
            CAL_URL,
            "/b.ics",
            None,
            &b,
            "BEGIN:VEVENT\r\nUID:b\r\nEND:VEVENT\r\n",
        )
        .unwrap();
        db.delete_calendar_event_by_href(ACCOUNT, CAL_URL, "/a.ics")
            .unwrap();
        let events = db
            .get_calendar_events_in_range(Some(ACCOUNT), 0, i64::MAX)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "b");
    }

    #[test]
    fn get_pending_calendar_writes_returns_only_unsynced_rows() {
        let db = MailDb::open_in_memory().unwrap();
        let synced = sample_event("synced-1");
        db.upsert_calendar_event(
            ACCOUNT,
            CAL_URL,
            "/synced.ics",
            Some("\"v1\""),
            &synced,
            synced.raw.as_ref().unwrap(),
        )
        .unwrap();
        let pending = sample_event("pending-1");
        db.insert_local_calendar_event(ACCOUNT, CAL_URL, &pending)
            .unwrap();

        let writes = db.get_pending_calendar_writes(ACCOUNT).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].uid, "pending-1");
    }

    #[test]
    fn to_vevent_reparses_raw_ics_preserving_unmodeled_properties() {
        let db = MailDb::open_in_memory().unwrap();
        let raw = "BEGIN:VEVENT\r\nUID:has-extra\r\nDTSTART:20240115T090000Z\r\nSUMMARY:X\r\nX-CUSTOM:keep-me\r\nEND:VEVENT\r\n";
        let event = parse_vevents(raw).unwrap().remove(0);
        let id = db
            .upsert_calendar_event(ACCOUNT, CAL_URL, "/has-extra.ics", None, &event, raw)
            .unwrap();
        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        let reconstructed = row.to_vevent().unwrap();
        // Compare against what the parser itself produced for `raw` (its
        // block-join doesn't keep a trailing CRLF) rather than the literal
        // input, so this asserts true DB round-trip fidelity of whatever
        // `parse_vevents` normalizes to.
        assert_eq!(reconstructed.raw, event.raw);
        assert!(
            reconstructed
                .raw
                .as_deref()
                .unwrap()
                .contains("X-CUSTOM:keep-me")
        );
    }
}

#[cfg(test)]
mod caldav_server_db_tests {
    use super::*;

    const ACCOUNT: &str = "personal";
    const CAL_URL: &str = "server:personal";

    fn ics(uid: &str, summary: &str) -> String {
        format!(
            "BEGIN:VEVENT\r\nUID:{}\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:{}\r\nEND:VEVENT\r\n",
            uid, summary
        )
    }

    #[test]
    fn put_new_event_creates_and_logs_a_change() {
        let db = MailDb::open_in_memory().unwrap();
        let body = ics("e1", "Standup");
        let outcome = db
            .server_put_event(ACCOUNT, CAL_URL, "/e1.ics", &body, None, false)
            .unwrap();
        match outcome {
            Ok(PutOutcome::Created(etag)) => assert!(!etag.is_empty()),
            other => panic!("expected Created, got {:?}", other),
        }
        let row = db
            .get_calendar_event_by_href(ACCOUNT, CAL_URL, "/e1.ics")
            .unwrap()
            .unwrap();
        assert_eq!(row.uid, "e1");
        assert_eq!(row.local_status, CAL_STATUS_SYNCED);

        let (changes, token) = db.server_sync_changes(ACCOUNT, CAL_URL, 0).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].href, "/e1.ics");
        assert!(!changes[0].deleted);
        assert_eq!(token, 1);
    }

    #[test]
    fn put_if_none_match_star_rejects_when_resource_already_exists() {
        let db = MailDb::open_in_memory().unwrap();
        let body = ics("e2", "First");
        db.server_put_event(ACCOUNT, CAL_URL, "/e2.ics", &body, None, false)
            .unwrap()
            .unwrap();
        let result = db
            .server_put_event(ACCOUNT, CAL_URL, "/e2.ics", &body, None, true)
            .unwrap();
        assert!(matches!(result, Err(PutError::PreconditionFailed(_))));
    }

    #[test]
    fn put_if_match_rejects_stale_etag_and_accepts_current_one() {
        let db = MailDb::open_in_memory().unwrap();
        let body1 = ics("e3", "V1");
        let created = db
            .server_put_event(ACCOUNT, CAL_URL, "/e3.ics", &body1, None, false)
            .unwrap()
            .unwrap();
        let etag1 = match created {
            PutOutcome::Created(e) => e,
            _ => panic!(),
        };

        let body2 = ics("e3", "V2");
        // Stale If-Match (using an etag that was never current) must fail.
        let stale = db
            .server_put_event(
                ACCOUNT,
                CAL_URL,
                "/e3.ics",
                &body2,
                Some("\"bogus\""),
                false,
            )
            .unwrap();
        assert!(matches!(stale, Err(PutError::PreconditionFailed(_))));

        // The correct current etag must succeed and produce a new one.
        let updated = db
            .server_put_event(ACCOUNT, CAL_URL, "/e3.ics", &body2, Some(&etag1), false)
            .unwrap()
            .unwrap();
        let etag2 = match updated {
            PutOutcome::Updated(e) => e,
            other => panic!("expected Updated, got {:?}", other),
        };
        assert_ne!(etag1, etag2, "content change must change the etag");

        let row = db
            .get_calendar_event_by_href(ACCOUNT, CAL_URL, "/e3.ics")
            .unwrap()
            .unwrap();
        assert_eq!(row.summary, "V2");
    }

    #[test]
    fn put_with_invalid_ics_body_is_a_client_error_not_a_panic() {
        let db = MailDb::open_in_memory().unwrap();
        let result = db
            .server_put_event(ACCOUNT, CAL_URL, "/bad.ics", "not an event", None, false)
            .unwrap();
        assert!(matches!(result, Err(PutError::InvalidBody(_))));
    }

    #[test]
    fn delete_requires_correct_if_match_and_is_idempotent_on_missing() {
        let db = MailDb::open_in_memory().unwrap();
        let body = ics("e4", "ToDelete");
        let etag = match db
            .server_put_event(ACCOUNT, CAL_URL, "/e4.ics", &body, None, false)
            .unwrap()
            .unwrap()
        {
            PutOutcome::Created(e) => e,
            _ => panic!(),
        };

        let wrong = db
            .server_delete_event(ACCOUNT, CAL_URL, "/e4.ics", Some("\"wrong\""))
            .unwrap();
        assert!(matches!(wrong, Err(DeleteError::PreconditionFailed(_))));

        let ok = db
            .server_delete_event(ACCOUNT, CAL_URL, "/e4.ics", Some(&etag))
            .unwrap();
        assert!(matches!(ok, Ok(())));
        assert!(
            db.get_calendar_event_by_href(ACCOUNT, CAL_URL, "/e4.ics")
                .unwrap()
                .is_none()
        );

        let missing = db
            .server_delete_event(ACCOUNT, CAL_URL, "/e4.ics", None)
            .unwrap();
        assert!(matches!(missing, Err(DeleteError::NotFound)));
    }

    #[test]
    fn sync_changes_collapses_create_then_delete_of_the_same_href_to_a_tombstone() {
        let db = MailDb::open_in_memory().unwrap();
        let body = ics("e5", "Transient");
        db.server_put_event(ACCOUNT, CAL_URL, "/e5.ics", &body, None, false)
            .unwrap()
            .unwrap();
        // No client has synced yet, so both the create and the delete
        // below happen "before" any sync-token the client holds.
        db.server_delete_event(ACCOUNT, CAL_URL, "/e5.ics", None)
            .unwrap()
            .unwrap();

        let (changes, _token) = db.server_sync_changes(ACCOUNT, CAL_URL, 0).unwrap();
        assert_eq!(changes.len(), 1, "one href, collapsed to its latest state");
        assert_eq!(changes[0].href, "/e5.ics");
        assert!(changes[0].deleted, "latest state for this href is deleted");
    }

    #[test]
    fn sync_changes_only_returns_entries_after_the_given_token() {
        let db = MailDb::open_in_memory().unwrap();
        db.server_put_event(ACCOUNT, CAL_URL, "/a.ics", &ics("a", "A"), None, false)
            .unwrap()
            .unwrap();
        let (_, token_after_a) = db.server_sync_changes(ACCOUNT, CAL_URL, 0).unwrap();

        db.server_put_event(ACCOUNT, CAL_URL, "/b.ics", &ics("b", "B"), None, false)
            .unwrap()
            .unwrap();

        let (changes_from_start, _) = db.server_sync_changes(ACCOUNT, CAL_URL, 0).unwrap();
        assert_eq!(changes_from_start.len(), 2);

        let (changes_since_a, _) = db
            .server_sync_changes(ACCOUNT, CAL_URL, token_after_a)
            .unwrap();
        assert_eq!(changes_since_a.len(), 1);
        assert_eq!(changes_since_a[0].href, "/b.ics");
    }

    #[test]
    fn server_list_events_returns_everything_in_the_calendar() {
        let db = MailDb::open_in_memory().unwrap();
        for uid in ["l1", "l2", "l3"] {
            db.server_put_event(
                ACCOUNT,
                CAL_URL,
                &format!("/{}.ics", uid),
                &ics(uid, uid),
                None,
                false,
            )
            .unwrap()
            .unwrap();
        }
        let events = db.server_list_events(ACCOUNT, CAL_URL).unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn compute_etag_changes_with_content_and_is_deterministic() {
        let a = compute_etag(&ics("x", "A"));
        let b = compute_etag(&ics("x", "B"));
        let a_again = compute_etag(&ics("x", "A"));
        assert_ne!(a, b);
        assert_eq!(a, a_again);
    }
}
