//! `jadav`'s own SQLite store.
//!
//! Deliberately separate from `mail.db` (`crate::db`): that file is a
//! *client-side cache* keyed by remote calendar URL with a single
//! `server:`-namespaced calendar per account, while jadav *is* the server —
//! it owns many calendars per principal, each with a provider (native, or
//! mirrored from Google / another CalDAV server), an owning email
//! identity, a write origin per object, an append-only change log that
//! backs RFC 6578 sync tokens, and (in later milestones) mirror bookkeeping,
//! an outbound iMIP queue and inbound-mail markers. Squeezing that into the
//! cache's tables would fight both designs; the patterns (WAL, zstd-packed
//! raw ICS, content-hash ETags, additive migrations) are copied instead.
//!
//! ## Model
//!
//! - One **principal** (the single Basic-auth login) with N identities.
//! - **calendars**, addressed by `slug` (the URL path segment). Config owns a
//!   calendar's identity/provider facts on every start
//!   ([`Store::reconcile_calendars`]); its presentation (`display_name`,
//!   `color`, `description`, `order`, `timezone`) is seeded from config
//!   once and then belongs to CalDAV clients via `PROPPATCH`.
//! - **objects**: one CalDAV resource = one `VCALENDAR` document per `UID`
//!   (master `VEVENT` plus any `RECURRENCE-ID` overrides), stored zstd-packed
//!   with index columns derived from the master. Every write records its
//!   [`Origin`]; only [`Origin::Client`] writes will ever trigger
//!   scheduling, and only non-`Mirror` writes are pushed to a remote.
//! - **change_log**: append-only, one row per write, `id` is the revision
//!   counter. A sync token is `urn:jadav:<slug>:<id>`; rows older than the
//!   retention window are compacted away, moving `sync_baseline_id` up so
//!   older tokens are refused with `valid-sync-token` (a full resync)
//!   rather than answered wrongly.
//!
//! ## Concurrency
//!
//! One connection per thread, opened per unit of work (an HTTP request, a
//! mirror cycle) — the same shape `crate::db::MailDb::open_at` uses. WAL plus
//! a 5 s `busy_timeout` lets the server, mirror and inbound threads write
//! concurrently without a process-wide lock. Every multi-statement write
//! runs under `BEGIN IMMEDIATE`.

use crate::calendar::{self, VEvent};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::path::Path;

pub use crate::jadav::config::{Provider, SendVia};

pub const CURRENT_SCHEMA_VERSION: i32 = 1;

/// Who performed a write. Stored on every object and change-log row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A CalDAV client (`PUT`/`DELETE`/`MKCALENDAR`…). The only origin the
    /// scheduling engine reacts to.
    Client,
    /// The mirror engine applying a remote change locally. Never pushed
    /// back out, never scheduled.
    Mirror,
    /// Applied from an inbound iTIP message (mail). Pushed to remotes,
    /// never scheduled.
    Inbound,
    /// jadav's own bookkeeping (e.g. `SCHEDULE-STATUS` write-back). Bumps
    /// the ETag so clients see it, but is neither pushed nor scheduled.
    System,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Client => "client",
            Origin::Mirror => "mirror",
            Origin::Inbound => "inbound",
            Origin::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "client" => Some(Origin::Client),
            "mirror" => Some(Origin::Mirror),
            "inbound" => Some(Origin::Inbound),
            "system" => Some(Origin::System),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarRow {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub color: Option<String>,
    pub order: i64,
    pub timezone: Option<String>,
    pub transparent: bool,
    pub identity: String,
    pub provider: Provider,
    pub remote_account: Option<String>,
    pub remote_calendar_id: Option<String>,
    pub two_way: bool,
    pub send_via: SendVia,
    pub is_default_for_identity: bool,
    pub sync_baseline_id: i64,
    pub created_at: i64,
}

impl CalendarRow {
    /// Whether CalDAV clients may write here: native calendars always,
    /// mirrored ones only when two-way. Mirror/inbound writes bypass this.
    pub fn writable(&self) -> bool {
        self.provider == Provider::Native || self.two_way
    }
}

/// What config says a calendar should be. Built by `jadav::run` from
/// [`crate::jadav::config::CalendarConfig`] with identities resolved.
#[derive(Clone, Debug)]
pub struct CalendarSpec {
    pub slug: String,
    pub display_name: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub timezone: Option<String>,
    pub identity: String,
    pub provider: Provider,
    pub remote_account: Option<String>,
    pub remote_calendar_id: Option<String>,
    pub two_way: bool,
    pub send_via: SendVia,
    pub is_default_for_identity: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    pub calendar_slug: String,
    /// Last path segment, percent-decoded (e.g. `abc@example.com.ics`).
    pub href_name: String,
    pub uid: String,
    /// Quoted strong ETag.
    pub etag: String,
    pub summary: String,
    pub dtstart_utc: Option<i64>,
    pub dtend_utc: Option<i64>,
    pub all_day: bool,
    pub rrule: Option<String>,
    pub sequence: i64,
    pub organizer: Option<String>,
    pub schedule_tag: i64,
    pub origin: Origin,
    pub rev: i64,
    pub deleted_at: Option<i64>,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRow {
    pub meta: ObjectMeta,
    pub ics: String,
}

/// Everything a committed write produced, handed to the scheduling engine
/// and the mirror wake-up.
#[derive(Clone, Debug)]
pub struct WriteOutcome {
    pub calendar: CalendarRow,
    pub href_name: String,
    pub uid: String,
    pub old: Option<ObjectRow>,
    pub new: Option<ObjectRow>,
    pub origin: Origin,
    pub log_id: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PutError {
    PreconditionFailed {
        current_etag: Option<String>,
    },
    /// The UID already lives under another href in this calendar.
    UidConflict {
        other_href_name: String,
    },
    /// An update tried to change the resource's UID.
    UidChanged,
    /// The body is not a `VCALENDAR` object at all (415).
    NotVcalendar,
    /// Structurally a calendar, but not one this server can index (403
    /// `valid-calendar-data`).
    InvalidBody(String),
    /// A top-level component other than `VEVENT`/`VTIMEZONE` (403
    /// `supported-calendar-component`).
    UnsupportedComponent(String),
    /// A client write to a read-only (one-way mirrored) calendar.
    ReadOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteError {
    NotFound,
    PreconditionFailed { current_etag: String },
    ReadOnly,
}

/// HTTP conditional-request semantics, evaluated against the current
/// ETag (strong comparison; weak `W/` tags never match).
#[derive(Clone, Debug)]
pub enum Precondition<'a> {
    None,
    IfMatch(&'a [String]),
    IfMatchAny,
    IfNoneMatchAny,
    IfNoneMatch(&'a [String]),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEntry {
    pub id: i64,
    pub href_name: String,
    pub uid: String,
    pub deleted: bool,
    pub origin: Origin,
}

#[derive(Debug)]
pub struct InvalidSyncToken;

#[derive(Debug)]
pub struct CalendarExists;

/// `PROPPATCH`-able presentation properties. Outer `None` = leave as is;
/// inner `None` (for nullable columns) = remove.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropPatch {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub color: Option<Option<String>>,
    pub order: Option<i64>,
    pub timezone: Option<Option<String>>,
    pub transparent: Option<bool>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompactionReport {
    pub deleted_rows: usize,
    pub calendars_touched: usize,
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS principal (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    login        TEXT NOT NULL,
    identities   TEXT NOT NULL,
    updated_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS calendars (
    slug                    TEXT PRIMARY KEY,
    display_name            TEXT NOT NULL,
    description             TEXT NOT NULL DEFAULT '',
    color                   TEXT,
    \"order\"               INTEGER NOT NULL DEFAULT 0,
    timezone                TEXT,
    transparent             INTEGER NOT NULL DEFAULT 0,
    identity                TEXT NOT NULL,
    provider                TEXT NOT NULL CHECK (provider IN ('native','google','caldav')),
    remote_account          TEXT,
    remote_calendar_id      TEXT,
    two_way                 INTEGER NOT NULL DEFAULT 1,
    send_via                TEXT NOT NULL DEFAULT 'smtp' CHECK (send_via IN ('smtp','provider')),
    is_default_for_identity INTEGER NOT NULL DEFAULT 0,
    sync_baseline_id        INTEGER NOT NULL DEFAULT 0,
    remote_sync_token       TEXT,
    last_full_fill_at       INTEGER,
    window_start            INTEGER,
    mirror_pushed_log_id    INTEGER NOT NULL DEFAULT 0,
    created_at              INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS objects (
    calendar_slug   TEXT NOT NULL REFERENCES calendars(slug) ON DELETE CASCADE,
    href_name       TEXT NOT NULL,
    uid             TEXT NOT NULL,
    etag            TEXT NOT NULL,
    raw_ics_zstd    BLOB NOT NULL,
    component_type  TEXT NOT NULL DEFAULT 'VEVENT',
    summary         TEXT NOT NULL DEFAULT '',
    dtstart_utc     INTEGER,
    dtend_utc       INTEGER,
    all_day         INTEGER NOT NULL DEFAULT 0,
    rrule           TEXT,
    sequence        INTEGER NOT NULL DEFAULT 0,
    organizer       TEXT,
    my_partstat     TEXT,
    schedule_tag    INTEGER NOT NULL DEFAULT 1,
    origin          TEXT NOT NULL CHECK (origin IN ('client','mirror','inbound','system')),
    rev             INTEGER NOT NULL DEFAULT 1,
    deleted_at      INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (calendar_slug, href_name),
    UNIQUE (calendar_slug, uid)
);
CREATE INDEX IF NOT EXISTS idx_objects_time ON objects(calendar_slug, dtstart_utc);

CREATE TABLE IF NOT EXISTS change_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    calendar_slug TEXT NOT NULL,
    href_name     TEXT NOT NULL,
    uid           TEXT NOT NULL,
    deleted       INTEGER NOT NULL DEFAULT 0,
    origin        TEXT NOT NULL,
    rev           INTEGER NOT NULL,
    at            INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_change_log_cal ON change_log(calendar_slug, id);

-- Owned by later milestones (mirror engine, scheduling, inbound mail);
-- created now so their arrival is never a destructive migration.
CREATE TABLE IF NOT EXISTS mirror_map (
    calendar_slug  TEXT NOT NULL,
    uid            TEXT NOT NULL,
    remote_id      TEXT NOT NULL,
    remote_version TEXT,
    remote_uid     TEXT,
    fingerprint    TEXT,
    pushed_rev     INTEGER NOT NULL DEFAULT 0,
    last_error     TEXT,
    PRIMARY KEY (calendar_slug, uid)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_mirror_map_remote ON mirror_map(calendar_slug, remote_id);
CREATE TABLE IF NOT EXISTS mirror_skip (
    calendar_slug  TEXT NOT NULL,
    remote_id      TEXT NOT NULL,
    remote_version TEXT,
    reason         TEXT,
    PRIMARY KEY (calendar_slug, remote_id)
);
CREATE TABLE IF NOT EXISTS remotes_status (
    remote               TEXT PRIMARY KEY,
    needs_reauth         INTEGER NOT NULL DEFAULT 0,
    last_ok_at           INTEGER,
    last_error           TEXT,
    last_error_at        INTEGER,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS oauth_tokens (
    remote_account TEXT PRIMARY KEY,
    refresh_token  TEXT NOT NULL,
    access_token   TEXT,
    expires_at     INTEGER,
    needs_reauth   INTEGER NOT NULL DEFAULT 0,
    updated_at     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS outbound_queue (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at      INTEGER NOT NULL,
    calendar_slug   TEXT NOT NULL,
    href_name       TEXT NOT NULL,
    ical_uid        TEXT NOT NULL,
    recurrence_id   TEXT,
    method          TEXT NOT NULL,
    sender          TEXT NOT NULL,
    recipient       TEXT NOT NULL,
    message_id      TEXT NOT NULL,
    raw_message     BLOB NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    state           TEXT NOT NULL DEFAULT 'pending'
);
CREATE INDEX IF NOT EXISTS idx_outbound_due ON outbound_queue(state, next_attempt_at);
CREATE TABLE IF NOT EXISTS imap_cursor (
    folder      TEXT PRIMARY KEY,
    uidvalidity INTEGER NOT NULL,
    last_uid    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS imap_seen (
    folder      TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL,
    uid         INTEGER NOT NULL,
    seen_at     INTEGER NOT NULL,
    PRIMARY KEY (folder, uidvalidity, uid)
);
CREATE TABLE IF NOT EXISTS imip_processed (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id    TEXT NOT NULL,
    folder        TEXT,
    uid           INTEGER,
    method        TEXT,
    ical_uid      TEXT,
    recurrence_id TEXT,
    sequence      INTEGER,
    dtstamp       TEXT,
    identity      TEXT,
    calendar_slug TEXT,
    outcome       TEXT NOT NULL,
    at            INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_imip_processed_msgid ON imip_processed(message_id);
CREATE INDEX IF NOT EXISTS idx_imip_processed_uid ON imip_processed(ical_uid);
CREATE TABLE IF NOT EXISTS schedule_inbox (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    href_name          TEXT NOT NULL UNIQUE,
    identity           TEXT NOT NULL,
    ical_uid           TEXT,
    method             TEXT,
    etag               TEXT NOT NULL,
    raw_itip_zstd      BLOB NOT NULL,
    received_at        INTEGER NOT NULL
);
";

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn compress(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, 3).unwrap_or_else(|_| data.to_vec())
}

fn decompress(data: &[u8]) -> Vec<u8> {
    use std::io::Read;
    match zstd::Decoder::new(data) {
        Ok(mut decoder) => {
            let mut out = Vec::new();
            if decoder.read_to_end(&mut out).is_ok() {
                return out;
            }
            data.to_vec()
        }
        Err(_) => data.to_vec(),
    }
}

/// A quoted strong ETag: the first 128 bits of SHA-256 over the stored
/// bytes. Content-derived so identical bodies compare equal, and — unlike
/// `std`'s `DefaultHasher` — stable across Rust releases, so a toolchain
/// upgrade never silently forces every client into a full resync.
pub fn compute_etag(ics: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(ics.as_bytes());
    let mut hex = String::with_capacity(34);
    hex.push('"');
    for b in &digest[..16] {
        hex.push_str(&format!("{:02x}", b));
    }
    hex.push('"');
    hex
}

/// Format the opaque sync token for `slug` at change-log position `id`.
pub fn format_sync_token(slug: &str, id: i64) -> String {
    format!("urn:jadav:{}:{}", slug, id)
}

/// Parse a token produced by [`format_sync_token`] for this slug; a
/// token for another calendar or of another shape is `None`.
pub fn parse_sync_token(token: &str, slug: &str) -> Option<i64> {
    let rest = token.trim().strip_prefix("urn:jadav:")?;
    let (tok_slug, id) = rest.rsplit_once(':')?;
    if tok_slug != slug {
        return None;
    }
    id.parse::<i64>().ok().filter(|n| *n >= 0)
}

/// Result of structural validation of a PUT body.
struct Validated {
    uid: String,
    /// The series master, when it could be indexed (`None` for objects
    /// whose `TZID`s this crate cannot resolve — stored unindexed).
    master: Option<VEvent>,
}

/// Structural checks a CalDAV server must make before storing: the body is
/// a `VCALENDAR`, contains only `VEVENT`/`VTIMEZONE` components, at least
/// one `VEVENT`, and every `VEVENT` carries the same non-empty `UID`.
fn validate_object(ics: &str) -> Result<Validated, PutError> {
    let lines = calendar::unfold(ics);
    let first = lines.iter().find(|l| !l.trim().is_empty());
    match first {
        Some(l) if l.trim().eq_ignore_ascii_case("BEGIN:VCALENDAR") => {}
        _ => return Err(PutError::NotVcalendar),
    }
    let mut depth = 0i32;
    let mut vevents = 0usize;
    let mut uids: Vec<String> = Vec::new();
    let mut in_vevent_depth: Option<i32> = None;
    for line in &lines {
        let upper_start = line.get(..6).map(|s| s.to_ascii_uppercase());
        if upper_start.as_deref() == Some("BEGIN:") {
            let name = line[6..].trim().to_ascii_uppercase();
            depth += 1;
            if depth == 2 {
                match name.as_str() {
                    "VEVENT" => {
                        vevents += 1;
                        in_vevent_depth = Some(depth);
                    }
                    "VTIMEZONE" => {}
                    other => return Err(PutError::UnsupportedComponent(other.to_string())),
                }
            }
            continue;
        }
        if line.get(..4).map(|s| s.eq_ignore_ascii_case("END:")) == Some(true) {
            if in_vevent_depth == Some(depth) {
                in_vevent_depth = None;
            }
            depth -= 1;
            continue;
        }
        if in_vevent_depth == Some(depth)
            && let Some(cl) = calendar::parse_content_line(line)
            && cl.name == "UID"
        {
            uids.push(cl.value.trim().to_string());
        }
    }
    if vevents == 0 {
        return Err(PutError::InvalidBody(
            "calendar object contains no VEVENT".to_string(),
        ));
    }
    if uids.len() != vevents {
        return Err(PutError::InvalidBody(
            "every VEVENT must carry a UID".to_string(),
        ));
    }
    let uid = uids[0].clone();
    if uid.is_empty() {
        return Err(PutError::InvalidBody("empty UID".to_string()));
    }
    if uids.iter().any(|u| *u != uid) {
        return Err(PutError::InvalidBody(
            "all VEVENTs in one resource must share the same UID".to_string(),
        ));
    }
    let master = match calendar::parse_vevents(ics) {
        Ok(events) => events
            .iter()
            .find(|e| e.uid == uid && !calendar::is_recurrence_override(e))
            .or_else(|| events.iter().find(|e| e.uid == uid))
            .cloned(),
        Err(calendar::CalendarError::UnsupportedTimezone(_)) => None,
        Err(e) => return Err(PutError::InvalidBody(e.to_string())),
    };
    Ok(Validated { uid, master })
}

fn precondition_holds(pre: &Precondition<'_>, current: Option<&str>) -> Result<(), ()> {
    let matches_any = |list: &[String]| {
        current.is_some_and(|cur| {
            list.iter()
                .any(|t| t.trim() == cur && !t.trim_start().starts_with("W/"))
        })
    };
    match pre {
        Precondition::None => Ok(()),
        Precondition::IfMatchAny => {
            if current.is_some() {
                Ok(())
            } else {
                Err(())
            }
        }
        Precondition::IfMatch(list) => {
            if matches_any(list) {
                Ok(())
            } else {
                Err(())
            }
        }
        Precondition::IfNoneMatchAny => {
            if current.is_none() {
                Ok(())
            } else {
                Err(())
            }
        }
        Precondition::IfNoneMatch(list) => {
            if matches_any(list) {
                Err(())
            } else {
                Ok(())
            }
        }
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating/migrating as needed) the store at `path`, restricting
    /// the file to the owner. One `Store` per thread; open per request.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let existed = path.exists();
        let conn = Connection::open(path)
            .with_context(|| format!("opening jadav store at {}", path.display()))?;
        Self::configure(&conn)?;
        upgrade_schema(&conn)?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Store { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory().context("opening in-memory jadav store")?;
        Self::configure(&conn)?;
        upgrade_schema(&conn)?;
        Ok(Store { conn })
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             PRAGMA cache_size = -4000;
             PRAGMA mmap_size = 0;",
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Principal
    // ------------------------------------------------------------------

    pub fn set_principal(&self, login: &str, identities: &[String]) -> Result<()> {
        let json = serde_json::to_string(identities)?;
        self.conn.execute(
            "INSERT INTO principal (id, login, identities, updated_at) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET login = excluded.login,
                 identities = excluded.identities, updated_at = excluded.updated_at",
            params![login, json, now_ts()],
        )?;
        Ok(())
    }

    pub fn principal(&self) -> Result<Option<(String, Vec<String>)>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT login, identities FROM principal WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((login, json)) => {
                let ids: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
                Ok(Some((login, ids)))
            }
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // Calendars
    // ------------------------------------------------------------------

    /// Bring the `calendars` table in line with config: create missing
    /// calendars (seeding presentation props), overwrite the identity /
    /// provider / remote / two-way / send-via / default facts on existing
    /// ones, and leave presentation props alone. Calendars present only in
    /// the store are reported as warnings, never deleted.
    pub fn reconcile_calendars(&self, specs: &[CalendarSpec]) -> Result<Vec<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let now = now_ts();
        let baseline: i64 = change_log_high_water(&tx)?;
        let mut configured: HashSet<&str> = HashSet::new();
        for spec in specs {
            configured.insert(spec.slug.as_str());
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM calendars WHERE slug = ?1",
                    params![spec.slug],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_some() {
                tx.execute(
                    "UPDATE calendars SET identity = ?2, provider = ?3, remote_account = ?4,
                         remote_calendar_id = ?5, two_way = ?6, send_via = ?7,
                         is_default_for_identity = ?8
                     WHERE slug = ?1",
                    params![
                        spec.slug,
                        spec.identity,
                        spec.provider.as_str(),
                        spec.remote_account,
                        spec.remote_calendar_id,
                        spec.two_way as i32,
                        spec.send_via.as_str(),
                        spec.is_default_for_identity as i32,
                    ],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO calendars (slug, display_name, description, color, timezone,
                         identity, provider, remote_account, remote_calendar_id, two_way,
                         send_via, is_default_for_identity, sync_baseline_id, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        spec.slug,
                        spec.display_name,
                        spec.description.clone().unwrap_or_default(),
                        spec.color,
                        spec.timezone,
                        spec.identity,
                        spec.provider.as_str(),
                        spec.remote_account,
                        spec.remote_calendar_id,
                        spec.two_way as i32,
                        spec.send_via.as_str(),
                        spec.is_default_for_identity as i32,
                        baseline,
                        now,
                    ],
                )?;
            }
        }
        let mut warnings = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT slug, provider FROM calendars")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (slug, provider) = row?;
                if !configured.contains(slug.as_str()) {
                    warnings.push(format!(
                        "calendar {:?} ({}) exists in the store but not in config; left untouched",
                        slug, provider
                    ));
                }
            }
        }
        tx.commit()?;
        Ok(warnings)
    }

    fn calendar_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarRow> {
        let provider: String = r.get("provider")?;
        let send_via: String = r.get("send_via")?;
        Ok(CalendarRow {
            slug: r.get("slug")?,
            display_name: r.get("display_name")?,
            description: r.get("description")?,
            color: r.get("color")?,
            order: r.get("order")?,
            timezone: r.get("timezone")?,
            transparent: r.get::<_, i64>("transparent")? != 0,
            identity: r.get("identity")?,
            provider: Provider::parse(&provider).unwrap_or(Provider::Native),
            remote_account: r.get("remote_account")?,
            remote_calendar_id: r.get("remote_calendar_id")?,
            two_way: r.get::<_, i64>("two_way")? != 0,
            send_via: SendVia::parse(&send_via).unwrap_or(SendVia::Smtp),
            is_default_for_identity: r.get::<_, i64>("is_default_for_identity")? != 0,
            sync_baseline_id: r.get("sync_baseline_id")?,
            created_at: r.get("created_at")?,
        })
    }

    const CALENDAR_COLUMNS: &'static str = "slug, display_name, description, color, \"order\", timezone, transparent, identity, provider, remote_account, remote_calendar_id, two_way, send_via, is_default_for_identity, sync_baseline_id, created_at";

    pub fn list_calendars(&self) -> Result<Vec<CalendarRow>> {
        let sql = format!(
            "SELECT {} FROM calendars ORDER BY \"order\", display_name, slug",
            Self::CALENDAR_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], Self::calendar_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_calendar(&self, slug: &str) -> Result<Option<CalendarRow>> {
        let sql = format!(
            "SELECT {} FROM calendars WHERE slug = ?1",
            Self::CALENDAR_COLUMNS
        );
        Ok(self
            .conn
            .query_row(&sql, params![slug], Self::calendar_from_row)
            .optional()?)
    }

    /// The calendar invitations for `identity` land in when none holds the
    /// event yet: the flagged default, else the identity's first native
    /// calendar.
    pub fn default_calendar_for_identity(&self, identity: &str) -> Result<Option<CalendarRow>> {
        let all = self.list_calendars()?;
        let identity = identity.to_ascii_lowercase();
        Ok(all
            .iter()
            .find(|c| c.identity == identity && c.is_default_for_identity)
            .or_else(|| {
                all.iter()
                    .find(|c| c.identity == identity && c.provider == Provider::Native)
            })
            .cloned())
    }

    /// Create a native calendar (what `MKCALENDAR` does). The change-log
    /// baseline starts at the current global position so a token minted
    /// for a previous calendar of the same slug can never be accepted.
    pub fn create_calendar(
        &self,
        slug: &str,
        identity: &str,
        props: &PropPatch,
    ) -> Result<Result<CalendarRow, CalendarExists>> {
        let tx = self.conn.unchecked_transaction()?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM calendars WHERE slug = ?1",
                params![slug],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            return Ok(Err(CalendarExists));
        }
        let baseline: i64 = change_log_high_water(&tx)?;
        tx.execute(
            "INSERT INTO calendars (slug, display_name, description, color, \"order\", timezone,
                 transparent, identity, provider, two_way, send_via, is_default_for_identity,
                 sync_baseline_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'native', 1, 'smtp', 0, ?9, ?10)",
            params![
                slug,
                props
                    .display_name
                    .clone()
                    .unwrap_or_else(|| slug.to_string()),
                props.description.clone().unwrap_or_default(),
                props.color.clone().flatten(),
                props.order.unwrap_or(0),
                props.timezone.clone().flatten(),
                props.transparent.unwrap_or(false) as i32,
                identity.to_ascii_lowercase(),
                baseline,
                now_ts(),
            ],
        )?;
        tx.commit()?;
        Ok(Ok(self
            .get_calendar(slug)?
            .context("calendar vanished after create")?))
    }

    /// Delete a calendar and everything in it (objects cascade; its change
    /// log is dropped too). Returns whether it existed.
    pub fn delete_calendar(&self, slug: &str) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM change_log WHERE calendar_slug = ?1",
            params![slug],
        )?;
        tx.execute(
            "DELETE FROM mirror_map WHERE calendar_slug = ?1",
            params![slug],
        )?;
        tx.execute(
            "DELETE FROM mirror_skip WHERE calendar_slug = ?1",
            params![slug],
        )?;
        let n = tx.execute("DELETE FROM calendars WHERE slug = ?1", params![slug])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Apply presentation-property changes. Never touches the change log:
    /// `getctag`/`sync-token` describe the *members*, not the collection's
    /// own properties.
    pub fn update_calendar_props(&self, slug: &str, patch: &PropPatch) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        if let Some(v) = &patch.display_name {
            tx.execute(
                "UPDATE calendars SET display_name = ?2 WHERE slug = ?1",
                params![slug, v],
            )?;
        }
        if let Some(v) = &patch.description {
            tx.execute(
                "UPDATE calendars SET description = ?2 WHERE slug = ?1",
                params![slug, v],
            )?;
        }
        if let Some(v) = &patch.color {
            tx.execute(
                "UPDATE calendars SET color = ?2 WHERE slug = ?1",
                params![slug, v],
            )?;
        }
        if let Some(v) = patch.order {
            tx.execute(
                "UPDATE calendars SET \"order\" = ?2 WHERE slug = ?1",
                params![slug, v],
            )?;
        }
        if let Some(v) = &patch.timezone {
            tx.execute(
                "UPDATE calendars SET timezone = ?2 WHERE slug = ?1",
                params![slug, v],
            )?;
        }
        if let Some(v) = patch.transparent {
            tx.execute(
                "UPDATE calendars SET transparent = ?2 WHERE slug = ?1",
                params![slug, v as i32],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Change log / sync tokens
    // ------------------------------------------------------------------

    /// Current revision of a calendar's membership: `max(baseline,
    /// MAX(change_log.id))`. Monotonic across compaction, so it also serves
    /// as `getctag`.
    pub fn ctag(&self, slug: &str) -> Result<i64> {
        let (baseline, max_id): (i64, i64) = self.conn.query_row(
            "SELECT c.sync_baseline_id,
                    COALESCE((SELECT MAX(id) FROM change_log WHERE calendar_slug = c.slug), 0)
             FROM calendars c WHERE c.slug = ?1",
            params![slug],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(baseline.max(max_id))
    }

    pub fn sync_token(&self, slug: &str) -> Result<String> {
        Ok(format_sync_token(slug, self.ctag(slug)?))
    }

    /// Members changed since change-log position `since_id`. `0` is an
    /// initial sync (every live object, no tombstones). A position older
    /// than what the log still holds, or newer than the current head, is
    /// `Err(InvalidSyncToken)` so the client performs a full resync.
    pub fn changes_since(
        &self,
        slug: &str,
        since_id: i64,
    ) -> Result<Result<(Vec<ChangeEntry>, i64), InvalidSyncToken>> {
        let cal = self
            .get_calendar(slug)?
            .with_context(|| format!("no calendar {}", slug))?;
        let head = self.ctag(slug)?;
        if since_id == 0 {
            let mut stmt = self.conn.prepare(
                "SELECT href_name, uid, origin FROM objects
                 WHERE calendar_slug = ?1 AND deleted_at IS NULL ORDER BY href_name",
            )?;
            let rows = stmt.query_map(params![slug], |r| {
                Ok(ChangeEntry {
                    id: 0,
                    href_name: r.get(0)?,
                    uid: r.get(1)?,
                    deleted: false,
                    origin: Origin::parse(&r.get::<_, String>(2)?).unwrap_or(Origin::Client),
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            return Ok(Ok((out, head)));
        }
        if since_id < cal.sync_baseline_id || since_id > head {
            return Ok(Err(InvalidSyncToken));
        }
        // Bare-aggregate collapse: with GROUP BY href_name, the non-aggregate
        // columns come from the row holding MAX(id) (SQLite-documented
        // behaviour), i.e. each href's latest state since the token.
        let mut stmt = self.conn.prepare(
            "SELECT MAX(id) AS maxid, href_name, uid, deleted, origin FROM change_log
             WHERE calendar_slug = ?1 AND id > ?2
             GROUP BY href_name ORDER BY maxid",
        )?;
        let rows = stmt.query_map(params![slug, since_id], |r| {
            Ok(ChangeEntry {
                id: r.get(0)?,
                href_name: r.get(1)?,
                uid: r.get(2)?,
                deleted: r.get::<_, i64>(3)? != 0,
                origin: Origin::parse(&r.get::<_, String>(4)?).unwrap_or(Origin::Client),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(Ok((out, head)))
    }

    /// Drop change-log rows older than `retention_secs` (never past a
    /// mirrored calendar's push cursor), advancing each calendar's
    /// `sync_baseline_id` so tokens that predate the kept history are
    /// refused with `valid-sync-token` instead of answered incompletely.
    pub fn compact_change_log(&self, retention_secs: i64, now: i64) -> Result<CompactionReport> {
        let mut report = CompactionReport::default();
        let calendars = self.list_calendars()?;
        let tx = self.conn.unchecked_transaction()?;
        for cal in &calendars {
            let cutoff: Option<i64> = tx.query_row(
                "SELECT MAX(id) FROM change_log WHERE calendar_slug = ?1 AND at < ?2",
                params![cal.slug, now - retention_secs],
                |r| r.get(0),
            )?;
            let Some(mut cutoff) = cutoff else { continue };
            if cal.provider != Provider::Native {
                let pushed: i64 = tx.query_row(
                    "SELECT mirror_pushed_log_id FROM calendars WHERE slug = ?1",
                    params![cal.slug],
                    |r| r.get(0),
                )?;
                cutoff = cutoff.min(pushed);
            }
            if cutoff <= 0 {
                continue;
            }
            let n = tx.execute(
                "DELETE FROM change_log WHERE calendar_slug = ?1 AND id <= ?2",
                params![cal.slug, cutoff],
            )?;
            tx.execute(
                "UPDATE calendars SET sync_baseline_id = MAX(sync_baseline_id, ?2) WHERE slug = ?1",
                params![cal.slug, cutoff],
            )?;
            report.deleted_rows += n;
            report.calendars_touched += 1;
        }
        tx.commit()?;
        Ok(report)
    }

    // ------------------------------------------------------------------
    // Objects
    // ------------------------------------------------------------------

    const OBJECT_COLUMNS: &'static str = "calendar_slug, href_name, uid, etag, summary, dtstart_utc, dtend_utc, all_day, rrule, sequence, organizer, schedule_tag, origin, rev, deleted_at, updated_at, raw_ics_zstd";

    fn object_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ObjectRow> {
        let origin: String = r.get(12)?;
        let blob: Vec<u8> = r.get(16)?;
        Ok(ObjectRow {
            meta: ObjectMeta {
                calendar_slug: r.get(0)?,
                href_name: r.get(1)?,
                uid: r.get(2)?,
                etag: r.get(3)?,
                summary: r.get(4)?,
                dtstart_utc: r.get(5)?,
                dtend_utc: r.get(6)?,
                all_day: r.get::<_, i64>(7)? != 0,
                rrule: r.get(8)?,
                sequence: r.get(9)?,
                organizer: r.get(10)?,
                schedule_tag: r.get(11)?,
                origin: Origin::parse(&origin).unwrap_or(Origin::Client),
                rev: r.get(13)?,
                deleted_at: r.get(14)?,
                updated_at: r.get(15)?,
            },
            ics: String::from_utf8_lossy(&decompress(&blob)).into_owned(),
        })
    }

    fn query_objects(
        &self,
        where_sql: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<ObjectRow>> {
        let sql = format!(
            "SELECT {} FROM objects WHERE {} ORDER BY href_name",
            Self::OBJECT_COLUMNS,
            where_sql
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(args, Self::object_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Every live object of a calendar.
    pub fn list_objects(&self, slug: &str) -> Result<Vec<ObjectRow>> {
        self.query_objects("calendar_slug = ?1 AND deleted_at IS NULL", &[&slug])
    }

    /// Objects that may overlap `[start, end)`: timed ones by their index
    /// columns, plus every recurring master (expansion is the caller's
    /// job) and every unindexed object (unknown timezone — a superset is
    /// the only correct answer for those).
    pub fn list_objects_overlapping(
        &self,
        slug: &str,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Result<Vec<ObjectRow>> {
        let start = start.unwrap_or(i64::MIN);
        let end = end.unwrap_or(i64::MAX);
        self.query_objects(
            "calendar_slug = ?1 AND deleted_at IS NULL AND (
                 rrule IS NOT NULL OR dtstart_utc IS NULL
                 OR (dtstart_utc < ?3 AND COALESCE(dtend_utc, dtstart_utc + 1) > ?2))",
            &[&slug, &start, &end],
        )
    }

    pub fn get_object(&self, slug: &str, href_name: &str) -> Result<Option<ObjectRow>> {
        Ok(self
            .query_objects(
                "calendar_slug = ?1 AND href_name = ?2 AND deleted_at IS NULL",
                &[&slug, &href_name],
            )?
            .pop())
    }

    /// Like [`Self::get_object`] but also returns a soft-deleted row (a
    /// client delete on a mirrored calendar still awaiting its push).
    pub fn get_object_including_deleted(
        &self,
        slug: &str,
        href_name: &str,
    ) -> Result<Option<ObjectRow>> {
        Ok(self
            .query_objects(
                "calendar_slug = ?1 AND href_name = ?2",
                &[&slug, &href_name],
            )?
            .pop())
    }

    pub fn get_object_by_uid(&self, slug: &str, uid: &str) -> Result<Option<ObjectRow>> {
        Ok(self
            .query_objects(
                "calendar_slug = ?1 AND uid = ?2 AND deleted_at IS NULL",
                &[&slug, &uid],
            )?
            .pop())
    }

    /// Find which calendar holds `uid` (live objects only).
    pub fn find_object_by_uid(&self, uid: &str) -> Result<Option<ObjectRow>> {
        Ok(self
            .query_objects("uid = ?1 AND deleted_at IS NULL", &[&uid])?
            .pop())
    }

    /// Store (create or replace) a calendar object. Client-correctable
    /// failures come back as `Ok(Err(PutError))` so the server maps them
    /// straight to a status; `Err` is an I/O or logic failure.
    pub fn put_object(
        &self,
        slug: &str,
        href_name: &str,
        ics: &str,
        pre: Precondition<'_>,
        origin: Origin,
    ) -> Result<Result<WriteOutcome, PutError>> {
        let tx = self.conn.unchecked_transaction()?;
        let calendar = self
            .get_calendar(slug)?
            .with_context(|| format!("no calendar {}", slug))?;
        let existing = self.get_object_including_deleted(slug, href_name)?;
        let live_existing = existing.clone().filter(|o| o.meta.deleted_at.is_none());
        let current_etag = live_existing.as_ref().map(|o| o.meta.etag.as_str());
        if precondition_holds(&pre, current_etag).is_err() {
            return Ok(Err(PutError::PreconditionFailed {
                current_etag: current_etag.map(str::to_string),
            }));
        }
        if origin == Origin::Client && !calendar.writable() {
            return Ok(Err(PutError::ReadOnly));
        }
        let validated = match validate_object(ics) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        if let Some(old) = &existing
            && old.meta.uid != validated.uid
        {
            // A soft-deleted row may be reused for a new UID; a live one
            // may not change UID.
            if old.meta.deleted_at.is_none() {
                return Ok(Err(PutError::UidChanged));
            }
        }
        let other: Option<String> = tx
            .query_row(
                "SELECT href_name FROM objects WHERE calendar_slug = ?1 AND uid = ?2 AND href_name != ?3",
                params![slug, validated.uid, href_name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other_href) = other {
            let other_deleted: Option<i64> = tx
                .query_row(
                    "SELECT deleted_at FROM objects WHERE calendar_slug = ?1 AND href_name = ?2",
                    params![slug, other_href],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if other_deleted.is_none() {
                return Ok(Err(PutError::UidConflict {
                    other_href_name: other_href,
                }));
            }
            // The other href is a soft-deleted leftover: retire it so the
            // UID uniqueness constraint lets the new href through.
            tx.execute(
                "DELETE FROM objects WHERE calendar_slug = ?1 AND href_name = ?2",
                params![slug, other_href],
            )?;
        }

        let etag = compute_etag(ics);
        let now = now_ts();
        let rev = existing.as_ref().map(|o| o.meta.rev + 1).unwrap_or(1);
        let created_at = existing.as_ref().map(|o| o.meta.updated_at).unwrap_or(now);
        let master = validated.master.as_ref();
        let (summary, dtstart, dtend, all_day, rrule, sequence, organizer) = match master {
            Some(m) => (
                m.summary.clone(),
                Some(m.dtstart.utc.timestamp()),
                Some(m.dtend.utc.timestamp()),
                m.dtstart.all_day,
                m.rrule.clone(),
                m.sequence,
                m.organizer.clone(),
            ),
            None => (String::new(), None, None, false, None, 0, None),
        };
        tx.execute(
            "INSERT INTO objects (calendar_slug, href_name, uid, etag, raw_ics_zstd, component_type,
                 summary, dtstart_utc, dtend_utc, all_day, rrule, sequence, organizer,
                 schedule_tag, origin, rev, deleted_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'VEVENT', ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1, ?13, ?14, NULL, ?15, ?16)
             ON CONFLICT(calendar_slug, href_name) DO UPDATE SET
                 uid = excluded.uid, etag = excluded.etag, raw_ics_zstd = excluded.raw_ics_zstd,
                 summary = excluded.summary, dtstart_utc = excluded.dtstart_utc,
                 dtend_utc = excluded.dtend_utc, all_day = excluded.all_day, rrule = excluded.rrule,
                 sequence = excluded.sequence, organizer = excluded.organizer,
                 schedule_tag = CASE WHEN excluded.origin IN ('inbound','system')
                                     THEN objects.schedule_tag + 1 ELSE objects.schedule_tag END,
                 origin = excluded.origin, rev = excluded.rev, deleted_at = NULL,
                 updated_at = excluded.updated_at",
            params![
                slug,
                href_name,
                validated.uid,
                etag,
                compress(ics.as_bytes()),
                summary,
                dtstart,
                dtend,
                all_day as i32,
                rrule,
                sequence,
                organizer,
                origin.as_str(),
                rev,
                created_at,
                now,
            ],
        )?;
        tx.execute(
            "INSERT INTO change_log (calendar_slug, href_name, uid, deleted, origin, rev, at)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
            params![slug, href_name, validated.uid, origin.as_str(), rev, now],
        )?;
        let log_id = tx.last_insert_rowid();
        tx.commit()?;
        let new = self.get_object(slug, href_name)?;
        Ok(Ok(WriteOutcome {
            calendar,
            href_name: href_name.to_string(),
            uid: validated.uid,
            old: live_existing,
            new,
            origin,
            log_id,
        }))
    }

    /// Delete an object. A client delete on a *mirrored* calendar is a
    /// soft delete (the row stays, flagged, until the mirror has pushed
    /// it); everything else removes the row. Either way a tombstone is
    /// appended so CalDAV clients see the member disappear at once.
    pub fn delete_object(
        &self,
        slug: &str,
        href_name: &str,
        pre: Precondition<'_>,
        origin: Origin,
    ) -> Result<Result<WriteOutcome, DeleteError>> {
        let tx = self.conn.unchecked_transaction()?;
        let calendar = self
            .get_calendar(slug)?
            .with_context(|| format!("no calendar {}", slug))?;
        let Some(existing) = self.get_object(slug, href_name)? else {
            return Ok(Err(DeleteError::NotFound));
        };
        if precondition_holds(&pre, Some(&existing.meta.etag)).is_err() {
            return Ok(Err(DeleteError::PreconditionFailed {
                current_etag: existing.meta.etag.clone(),
            }));
        }
        if origin == Origin::Client && !calendar.writable() {
            return Ok(Err(DeleteError::ReadOnly));
        }
        let now = now_ts();
        let rev = existing.meta.rev + 1;
        let soft = origin == Origin::Client && calendar.provider != Provider::Native;
        if soft {
            tx.execute(
                "UPDATE objects SET deleted_at = ?3, origin = ?4, rev = ?5, updated_at = ?3
                 WHERE calendar_slug = ?1 AND href_name = ?2",
                params![slug, href_name, now, origin.as_str(), rev],
            )?;
        } else {
            tx.execute(
                "DELETE FROM objects WHERE calendar_slug = ?1 AND href_name = ?2",
                params![slug, href_name],
            )?;
        }
        tx.execute(
            "INSERT INTO change_log (calendar_slug, href_name, uid, deleted, origin, rev, at)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6)",
            params![
                slug,
                href_name,
                existing.meta.uid,
                origin.as_str(),
                rev,
                now
            ],
        )?;
        let log_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Ok(WriteOutcome {
            calendar,
            href_name: href_name.to_string(),
            uid: existing.meta.uid.clone(),
            old: Some(existing),
            new: None,
            origin,
            log_id,
        }))
    }

    /// Remove a soft-deleted row for good once its remote delete has been
    /// pushed (mirror engine).
    pub fn finalize_client_delete(&self, slug: &str, href_name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM objects WHERE calendar_slug = ?1 AND href_name = ?2 AND deleted_at IS NOT NULL",
            params![slug, href_name],
        )?;
        Ok(())
    }

    /// Objects the mirror engine still has to push: last written by a
    /// client or from mail, newer than what was pushed (soft-deleted rows
    /// included).
    pub fn list_objects_to_push(&self, slug: &str) -> Result<Vec<ObjectRow>> {
        let sql = format!(
            "SELECT {} FROM objects o WHERE o.calendar_slug = ?1
               AND o.origin IN ('client','inbound')
               AND o.rev > COALESCE((SELECT pushed_rev FROM mirror_map m
                                     WHERE m.calendar_slug = o.calendar_slug AND m.uid = o.uid), 0)
             ORDER BY o.updated_at",
            Self::OBJECT_COLUMNS
                .split(", ")
                .map(|c| format!("o.{}", c))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![slug], Self::object_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Schedule inbox (served empty until the scheduling milestone)
    // ------------------------------------------------------------------

    pub fn inbox_ctag(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM schedule_inbox", [], |r| {
                r.get(0)
            })?)
    }
}

/// The highest change-log id ever issued, including rows since deleted
/// (a calendar's log is dropped with the calendar). `AUTOINCREMENT` keeps
/// this in `sqlite_sequence`, so a recreated calendar can start its
/// baseline above every token it could ever have handed out before.
fn change_log_high_water(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'change_log'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn get_schema_version(conn: &Connection) -> Result<i32> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(0);
    }
    Ok(conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(0))
}

/// Create or additively upgrade the schema. Runs under `BEGIN IMMEDIATE`
/// so two first-openers cannot race.
fn upgrade_schema(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let version = get_schema_version(&tx)?;
    if version > CURRENT_SCHEMA_VERSION {
        bail!(
            "jadav store schema version {} is newer than this build supports ({})",
            version,
            CURRENT_SCHEMA_VERSION
        );
    }
    if version == 0 {
        tx.execute_batch(SCHEMA_SQL)?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![CURRENT_SCHEMA_VERSION],
        )?;
    }
    // Future versions: `if version < 2 { ...ALTER/CREATE IF NOT EXISTS...; UPDATE schema_version SET version = 2 }`
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn spec(slug: &str, provider: Provider, two_way: bool) -> CalendarSpec {
        CalendarSpec {
            slug: slug.to_string(),
            display_name: slug.to_uppercase(),
            description: None,
            color: Some("#112233".to_string()),
            timezone: None,
            identity: "alice@example.com".to_string(),
            provider,
            remote_account: if provider == Provider::Native {
                None
            } else {
                Some("acct".to_string())
            },
            remote_calendar_id: None,
            two_way,
            send_via: SendVia::Smtp,
            is_default_for_identity: slug == "personal",
        }
    }

    fn store() -> Store {
        let s = Store::open_in_memory().unwrap();
        s.set_principal("alice@example.com", &["alice@example.com".to_string()])
            .unwrap();
        s.reconcile_calendars(&[
            spec("personal", Provider::Native, true),
            spec("ro-mirror", Provider::Caldav, false),
            spec("rw-mirror", Provider::Google, true),
        ])
        .unwrap();
        s
    }

    fn ics(uid: &str, summary: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:{summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    #[test]
    fn compute_etag_is_quoted_stable_and_content_sensitive() {
        let a = compute_etag("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n");
        assert!(
            a.starts_with('"') && a.ends_with('"') && a.len() == 34,
            "{a}"
        );
        // Pinned vector (sha256 of the input, first 32 hex chars): any
        // drift here would force every client into a full resync.
        assert_eq!(a, "\"215efae54e34ac5657da47b49c8d1bd8\"");
        assert_eq!(a, compute_etag("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"));
        assert_ne!(
            a,
            compute_etag("BEGIN:VCALENDAR\r\nX:1\r\nEND:VCALENDAR\r\n")
        );
    }

    #[test]
    fn sync_token_round_trips_and_rejects_foreign_slugs() {
        let t = format_sync_token("personal", 42);
        assert_eq!(t, "urn:jadav:personal:42");
        assert_eq!(parse_sync_token(&t, "personal"), Some(42));
        assert_eq!(parse_sync_token(&t, "other"), None);
        assert_eq!(
            parse_sync_token("http://sabre.io/ns/sync/10", "personal"),
            None
        );
        assert_eq!(parse_sync_token("urn:jadav:personal:-1", "personal"), None);
        // A slug containing ':' still parses because the id is the last field.
        assert_eq!(parse_sync_token("urn:jadav:a:b:7", "a:b"), Some(7));
    }

    #[test]
    fn validate_object_rules() {
        assert_eq!(
            validate_object("BEGIN:VEVENT\r\nUID:x\r\nEND:VEVENT\r\n").err(),
            Some(PutError::NotVcalendar)
        );
        assert!(matches!(
            validate_object("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:x\r\nEND:VTODO\r\nEND:VCALENDAR\r\n"),
            Err(PutError::UnsupportedComponent(c)) if c == "VTODO"
        ));
        assert!(matches!(
            validate_object(
                "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:X\r\nEND:VTIMEZONE\r\nEND:VCALENDAR\r\n"
            ),
            Err(PutError::InvalidBody(_))
        ));
        assert!(matches!(
            validate_object(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nDTSTART:20240101T000000Z\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:b\r\nDTSTART:20240101T000000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            ),
            Err(PutError::InvalidBody(_))
        ));
        let ok = validate_object(&ics("u1", "S")).unwrap();
        assert_eq!(ok.uid, "u1");
        assert!(ok.master.is_some());
        // Unknown (Windows) TZID: accepted, but unindexed.
        let win = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w\r\nDTSTART;TZID=W. Europe Standard Time:20240115T090000\r\nDTEND;TZID=W. Europe Standard Time:20240115T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let v = validate_object(win).unwrap();
        assert_eq!(v.uid, "w");
        assert!(v.master.is_none());
        // Master + override sharing a UID is fine; the master is indexed.
        let series = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:s\r\nDTSTART:20240101T090000Z\r\nDTEND:20240101T100000Z\r\nRRULE:FREQ=DAILY\r\nSUMMARY:M\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:s\r\nRECURRENCE-ID:20240102T090000Z\r\nDTSTART:20240102T110000Z\r\nDTEND:20240102T120000Z\r\nSUMMARY:O\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let v = validate_object(series).unwrap();
        assert_eq!(v.master.unwrap().summary, "M");
    }

    #[test]
    fn reconcile_creates_updates_facts_and_preserves_presentation() {
        let s = store();
        let cals = s.list_calendars().unwrap();
        assert_eq!(cals.len(), 3);
        let p = s.get_calendar("personal").unwrap().unwrap();
        assert_eq!(p.color.as_deref(), Some("#112233"));
        assert!(p.is_default_for_identity);
        // A client recolours; config flips two_way; the colour must survive.
        s.update_calendar_props(
            "rw-mirror",
            &PropPatch {
                color: Some(Some("#ff0000".to_string())),
                display_name: Some("Renamed".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let warnings = s
            .reconcile_calendars(&[
                spec("personal", Provider::Native, true),
                spec("rw-mirror", Provider::Google, false),
            ])
            .unwrap();
        let m = s.get_calendar("rw-mirror").unwrap().unwrap();
        assert_eq!(m.color.as_deref(), Some("#ff0000"));
        assert_eq!(m.display_name, "Renamed");
        assert!(!m.two_way);
        assert!(!m.writable());
        assert!(
            warnings.iter().any(|w| w.contains("ro-mirror")),
            "{warnings:?}"
        );
        assert!(s.get_calendar("ro-mirror").unwrap().is_some());
    }

    #[test]
    fn put_get_update_delete_round_trip_with_preconditions_and_log() {
        let s = store();
        let out = s
            .put_object(
                "personal",
                "a.ics",
                &ics("a", "One"),
                Precondition::IfNoneMatchAny,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        assert!(out.old.is_none());
        let etag1 = out.new.as_ref().unwrap().meta.etag.clone();
        assert!(etag1.starts_with('"'));
        assert_eq!(out.new.as_ref().unwrap().meta.summary, "One");
        assert_eq!(out.new.as_ref().unwrap().meta.rev, 1);
        assert_eq!(s.ctag("personal").unwrap(), out.log_id);

        // Create-only precondition now fails.
        assert_eq!(
            s.put_object(
                "personal",
                "a.ics",
                &ics("a", "Two"),
                Precondition::IfNoneMatchAny,
                Origin::Client
            )
            .unwrap()
            .unwrap_err(),
            PutError::PreconditionFailed {
                current_etag: Some(etag1.clone())
            }
        );
        // Stale If-Match fails; weak tags never match.
        let stale = vec!["\"nope\"".to_string()];
        assert!(matches!(
            s.put_object(
                "personal",
                "a.ics",
                &ics("a", "Two"),
                Precondition::IfMatch(&stale),
                Origin::Client
            )
            .unwrap(),
            Err(PutError::PreconditionFailed { .. })
        ));
        let weak = vec![format!("W/{}", etag1)];
        assert!(matches!(
            s.put_object(
                "personal",
                "a.ics",
                &ics("a", "Two"),
                Precondition::IfMatch(&weak),
                Origin::Client
            )
            .unwrap(),
            Err(PutError::PreconditionFailed { .. })
        ));
        let good = vec![etag1.clone()];
        let out2 = s
            .put_object(
                "personal",
                "a.ics",
                &ics("a", "Two"),
                Precondition::IfMatch(&good),
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        assert_eq!(out2.old.unwrap().meta.summary, "One");
        assert_eq!(out2.new.as_ref().unwrap().meta.rev, 2);
        assert_ne!(out2.new.as_ref().unwrap().meta.etag, etag1);

        // UID change on an existing href is refused; same UID under another href too.
        assert_eq!(
            s.put_object(
                "personal",
                "a.ics",
                &ics("zzz", "X"),
                Precondition::None,
                Origin::Client
            )
            .unwrap()
            .unwrap_err(),
            PutError::UidChanged
        );
        assert_eq!(
            s.put_object(
                "personal",
                "b.ics",
                &ics("a", "X"),
                Precondition::None,
                Origin::Client
            )
            .unwrap()
            .unwrap_err(),
            PutError::UidConflict {
                other_href_name: "a.ics".to_string()
            }
        );

        // Delete with a stale tag fails, then succeeds; native delete is hard.
        assert!(matches!(
            s.delete_object(
                "personal",
                "a.ics",
                Precondition::IfMatch(&stale),
                Origin::Client
            )
            .unwrap(),
            Err(DeleteError::PreconditionFailed { .. })
        ));
        let del = s
            .delete_object(
                "personal",
                "a.ics",
                Precondition::IfMatchAny,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        assert!(del.new.is_none());
        assert!(s.get_object("personal", "a.ics").unwrap().is_none());
        assert!(
            s.get_object_including_deleted("personal", "a.ics")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            s.delete_object("personal", "a.ics", Precondition::None, Origin::Client)
                .unwrap()
                .unwrap_err(),
            DeleteError::NotFound
        );

        // The log saw create, update, delete.
        let (changes, head) = s.changes_since("personal", 0).unwrap().unwrap();
        assert!(
            changes.is_empty(),
            "initial listing has no live members: {changes:?}"
        );
        assert_eq!(head, del.log_id);
        let (changes, _) = s.changes_since("personal", out.log_id).unwrap().unwrap();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].deleted);
        assert_eq!(changes[0].href_name, "a.ics");
    }

    #[test]
    fn read_only_mirror_refuses_client_writes_but_not_mirror_writes() {
        let s = store();
        assert_eq!(
            s.put_object(
                "ro-mirror",
                "x.ics",
                &ics("x", "X"),
                Precondition::None,
                Origin::Client
            )
            .unwrap()
            .unwrap_err(),
            PutError::ReadOnly
        );
        s.put_object(
            "ro-mirror",
            "x.ics",
            &ics("x", "X"),
            Precondition::None,
            Origin::Mirror,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            s.delete_object("ro-mirror", "x.ics", Precondition::None, Origin::Client)
                .unwrap()
                .unwrap_err(),
            DeleteError::ReadOnly
        );
        s.delete_object("ro-mirror", "x.ics", Precondition::None, Origin::Mirror)
            .unwrap()
            .unwrap();
        assert!(s.get_object("ro-mirror", "x.ics").unwrap().is_none());
    }

    #[test]
    fn client_delete_on_a_mirrored_calendar_is_soft_and_pushable() {
        let s = store();
        s.put_object(
            "rw-mirror",
            "m.ics",
            &ics("m", "M"),
            Precondition::None,
            Origin::Mirror,
        )
        .unwrap()
        .unwrap();
        assert!(s.list_objects_to_push("rw-mirror").unwrap().is_empty());
        s.delete_object("rw-mirror", "m.ics", Precondition::None, Origin::Client)
            .unwrap()
            .unwrap();
        // Gone for clients, still there for the mirror engine.
        assert!(s.get_object("rw-mirror", "m.ics").unwrap().is_none());
        let pending = s.list_objects_to_push("rw-mirror").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].meta.deleted_at.is_some());
        let (changes, _) = s.changes_since("rw-mirror", 0).unwrap().unwrap();
        assert!(changes.is_empty());
        s.finalize_client_delete("rw-mirror", "m.ics").unwrap();
        assert!(
            s.get_object_including_deleted("rw-mirror", "m.ics")
                .unwrap()
                .is_none()
        );
        // Re-PUT under the same href with a new UID is allowed after the
        // soft-deleted row is gone.
        s.put_object(
            "rw-mirror",
            "m.ics",
            &ics("m2", "M2"),
            Precondition::None,
            Origin::Client,
        )
        .unwrap()
        .unwrap();
    }

    #[test]
    fn changes_since_collapses_per_href_and_validates_tokens() {
        let s = store();
        let a = s
            .put_object(
                "personal",
                "a.ics",
                &ics("a", "1"),
                Precondition::None,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        let _b = s
            .put_object(
                "personal",
                "b.ics",
                &ics("b", "1"),
                Precondition::None,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        let a2 = s
            .put_object(
                "personal",
                "a.ics",
                &ics("a", "2"),
                Precondition::None,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        let (changes, head) = s.changes_since("personal", a.log_id).unwrap().unwrap();
        assert_eq!(head, a2.log_id);
        let hrefs: Vec<&str> = changes.iter().map(|c| c.href_name.as_str()).collect();
        assert_eq!(hrefs, vec!["b.ics", "a.ics"]);
        assert!(changes.iter().all(|c| !c.deleted));
        // Up to date: no changes, same head.
        let (none, head2) = s.changes_since("personal", head).unwrap().unwrap();
        assert!(none.is_empty());
        assert_eq!(head2, head);
        // A token from the future is invalid.
        assert!(s.changes_since("personal", head + 5).unwrap().is_err());
        // Initial sync lists both live members.
        let (initial, _) = s.changes_since("personal", 0).unwrap().unwrap();
        assert_eq!(initial.len(), 2);
    }

    #[test]
    fn compaction_moves_the_baseline_and_keeps_ctag_monotonic() {
        let s = store();
        let a = s
            .put_object(
                "personal",
                "a.ics",
                &ics("a", "1"),
                Precondition::None,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        let b = s
            .put_object(
                "personal",
                "b.ics",
                &ics("b", "1"),
                Precondition::None,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        let c = s
            .put_object(
                "personal",
                "c.ics",
                &ics("c", "1"),
                Precondition::None,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        // Backdate the first two rows so they fall outside retention.
        s.conn
            .execute(
                "UPDATE change_log SET at = at - 1000 WHERE id <= ?1",
                params![b.log_id],
            )
            .unwrap();
        let before = s.ctag("personal").unwrap();
        let report = s.compact_change_log(500, now_ts()).unwrap();
        assert_eq!(report.deleted_rows, 2);
        let cal = s.get_calendar("personal").unwrap().unwrap();
        assert_eq!(cal.sync_baseline_id, b.log_id);
        assert_eq!(s.ctag("personal").unwrap(), before);
        // A token older than the kept history is invalid; at the baseline
        // and after it is fine; an initial sync (0) always works.
        assert!(s.changes_since("personal", a.log_id).unwrap().is_err());
        assert!(s.changes_since("personal", b.log_id).unwrap().is_ok());
        let (changes, _) = s.changes_since("personal", b.log_id).unwrap().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].id, c.log_id);
        assert!(s.changes_since("personal", 0).unwrap().is_ok());
        // Mirrored calendars never compact past the push cursor.
        let m = s
            .put_object(
                "rw-mirror",
                "m.ics",
                &ics("m", "1"),
                Precondition::None,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        s.conn
            .execute(
                "UPDATE change_log SET at = at - 1000 WHERE id = ?1",
                params![m.log_id],
            )
            .unwrap();
        let report = s.compact_change_log(500, now_ts()).unwrap();
        assert_eq!(report.deleted_rows, 0);
    }

    #[test]
    fn recreated_calendar_rejects_tokens_from_its_predecessor() {
        let s = store();
        let out = s
            .put_object(
                "personal",
                "a.ics",
                &ics("a", "1"),
                Precondition::None,
                Origin::Client,
            )
            .unwrap()
            .unwrap();
        let old_head = s.ctag("personal").unwrap();
        assert_eq!(old_head, out.log_id);
        assert!(s.delete_calendar("personal").unwrap());
        assert!(s.get_calendar("personal").unwrap().is_none());
        let created = s
            .create_calendar("personal", "alice@example.com", &PropPatch::default())
            .unwrap()
            .unwrap();
        assert_eq!(created.provider, Provider::Native);
        assert!(
            created.sync_baseline_id >= old_head,
            "{} < {}",
            created.sync_baseline_id,
            old_head
        );
        assert!(
            s.changes_since("personal", old_head).unwrap().is_err()
                || old_head == created.sync_baseline_id
        );
        // The new calendar's head is at or above every id the old one issued.
        assert!(s.ctag("personal").unwrap() >= old_head);
        assert!(matches!(
            s.create_calendar("personal", "alice@example.com", &PropPatch::default())
                .unwrap(),
            Err(CalendarExists)
        ));
    }

    #[test]
    fn overlapping_query_is_a_superset_including_recurring_and_unindexed() {
        let s = store();
        s.put_object(
            "personal",
            "a.ics",
            &ics("a", "Jan 15"),
            Precondition::None,
            Origin::Client,
        )
        .unwrap()
        .unwrap();
        let recurring = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:r\r\nDTSTART:20230101T090000Z\r\nDTEND:20230101T100000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:R\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        s.put_object(
            "personal",
            "r.ics",
            recurring,
            Precondition::None,
            Origin::Client,
        )
        .unwrap()
        .unwrap();
        let win = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w\r\nDTSTART;TZID=W. Europe Standard Time:20200101T090000\r\nDTEND;TZID=W. Europe Standard Time:20200101T100000\r\nSUMMARY:W\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        s.put_object("personal", "w.ics", win, Precondition::None, Origin::Client)
            .unwrap()
            .unwrap();
        let start = chrono::Utc
            .with_ymd_and_hms(2024, 1, 15, 0, 0, 0)
            .unwrap()
            .timestamp();
        let end = chrono::Utc
            .with_ymd_and_hms(2024, 1, 16, 0, 0, 0)
            .unwrap()
            .timestamp();
        let rows = s
            .list_objects_overlapping("personal", Some(start), Some(end))
            .unwrap();
        let hrefs: Vec<&str> = rows.iter().map(|r| r.meta.href_name.as_str()).collect();
        assert_eq!(hrefs, vec!["a.ics", "r.ics", "w.ics"]);
        let later = chrono::Utc
            .with_ymd_and_hms(2025, 1, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let rows = s
            .list_objects_overlapping("personal", Some(later), None)
            .unwrap();
        let hrefs: Vec<&str> = rows.iter().map(|r| r.meta.href_name.as_str()).collect();
        assert_eq!(hrefs, vec!["r.ics", "w.ics"]);
        assert!(
            s.get_object("personal", "w.ics")
                .unwrap()
                .unwrap()
                .meta
                .dtstart_utc
                .is_none()
        );
    }

    #[test]
    fn default_calendar_for_identity_prefers_the_flag_then_native() {
        let s = store();
        let d = s
            .default_calendar_for_identity("alice@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(d.slug, "personal");
        assert!(
            s.default_calendar_for_identity("nobody@example.com")
                .unwrap()
                .is_none()
        );
        assert_eq!(s.find_object_by_uid("nothing").unwrap(), None);
    }

    #[test]
    fn schema_version_is_recorded_and_reopen_is_idempotent() {
        let s = store();
        assert_eq!(get_schema_version(&s.conn).unwrap(), CURRENT_SCHEMA_VERSION);
        upgrade_schema(&s.conn).unwrap();
        assert_eq!(s.principal().unwrap().unwrap().0, "alice@example.com");
    }
}
