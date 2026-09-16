//! Two-way mirroring between a jadav calendar and an external one.
//!
//! A *remote* is an external account (a Google account, another CalDAV
//! server); a *mirrored calendar* is a jadav calendar whose `provider` is
//! not `native` and whose contents are kept equal to one remote calendar.
//! Providers implement [`RemoteCalendar`]; the engine in this module is
//! provider-agnostic and runs one thread per remote account.
//!
//! ## Rules (a port of the `cal-puller` bridge this replaces)
//!
//! - **Push before pull**, so our own write's remote version is recorded
//!   before the next listing echoes it back.
//! - **Push only what a client or inbound mail wrote** (`Origin::Client` /
//!   `Origin::Inbound` rows with `rev > mirror_map.pushed_rev`); never
//!   anything the mirror itself applied.
//! - **Attendee-side writes push only the RSVP.** When the calendar's
//!   identity is merely an attendee, the sole change that reaches the
//!   remote is our own `PARTSTAT`; any other local edit is reverted to the
//!   remote copy (Google refuses non-organizer edits anyway).
//! - **Remote wins on conflict** (a `412`/`409`/version mismatch): the
//!   remote copy is re-fetched and applied locally as `Origin::Mirror`.
//! - **Echo suppression** in two layers: a listed member whose remote
//!   version equals what we last wrote is skipped; one whose version changed
//!   but whose [`Semantic`] fingerprint is equal only refreshes the version.
//! - **Provider-originated writes never schedule anything**: everything the
//!   engine stores carries `Origin::Mirror`, which the scheduling engine
//!   ignores by contract.
//! - **A full resync** (rejected token, Google `410`) re-lists the remote's
//!   window and deletes locally only mapped objects that were not listed
//!   *and* still fall inside that window — rows older than the window are
//!   left alone.

use crate::calendar::{self, EventTime, VEvent};
use crate::jadav::config::SendVia;
use crate::jadav::store::{ObjectRow, Origin, Precondition as StorePre, Store};
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------
// Remote-side types
// ---------------------------------------------------------------------

/// The remote's own identifier for an object (Google: the master event id;
/// CalDAV: the raw href as served).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemoteId(pub String);

/// An opaque version stamp for echo suppression and preconditions
/// (Google: master etag plus sorted exception etags; CalDAV: the quoted
/// ETag).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteVersion(pub String);

#[derive(Clone, Debug)]
pub struct RemoteObject {
    pub id: RemoteId,
    pub version: RemoteVersion,
    /// The `UID` as the remote has it.
    pub uid: String,
    /// One complete `VCALENDAR`: master plus `RECURRENCE-ID` overrides.
    pub ics: String,
    /// [`Semantic`] fingerprint of `ics`, precomputed by the provider so
    /// tests can assert equality across representations.
    pub fingerprint: String,
}

#[derive(Debug, Default)]
pub struct Changes {
    pub items: Vec<RemoteObject>,
    pub deleted: Vec<RemoteId>,
    pub next_token: Option<String>,
    /// `items` is a complete listing of the window (initial fill or forced
    /// resync), so anything mapped but unlisted inside the window is gone.
    pub full_resync: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    None,
    IfMatch(RemoteVersion),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// The remote can change just our own participation status.
    pub rsvp_only_writes: bool,
    /// The remote itself emails scheduling messages for writes we make.
    pub server_schedules: bool,
}

#[derive(Debug)]
pub enum RemoteError {
    /// Version precondition failed / the object already exists.
    Conflict(String),
    NotFound,
    /// The token is no longer valid; list again without one.
    FullResyncRequired,
    /// Credentials are gone for good (revoked refresh token); stop until a
    /// human re-authorises.
    NeedsReauth,
    /// The remote refused the write (e.g. non-organizer editing an event).
    Forbidden(String),
    /// Network / 5xx / rate limit after retries — try again next cycle.
    Transient(anyhow::Error),
    Other(anyhow::Error),
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::Conflict(m) => write!(f, "conflict: {}", m),
            RemoteError::NotFound => write!(f, "not found"),
            RemoteError::FullResyncRequired => write!(f, "sync token rejected"),
            RemoteError::NeedsReauth => {
                write!(f, "authorization revoked; re-run `jadav google auth`")
            }
            RemoteError::Forbidden(m) => write!(f, "forbidden: {}", m),
            RemoteError::Transient(e) => write!(f, "transient: {:#}", e),
            RemoteError::Other(e) => write!(f, "{:#}", e),
        }
    }
}

impl std::error::Error for RemoteError {}

impl From<anyhow::Error> for RemoteError {
    fn from(e: anyhow::Error) -> Self {
        RemoteError::Other(e)
    }
}

pub trait RemoteCalendar {
    /// Incremental changes since `token` (`None` = complete listing bounded
    /// by `window_start`).
    fn list_changes(
        &mut self,
        token: Option<&str>,
        window_start: Option<DateTime<Utc>>,
    ) -> Result<Changes, RemoteError>;
    fn fetch(&mut self, id: &RemoteId) -> Result<Option<RemoteObject>, RemoteError>;
    fn create(&mut self, doc: &VCalendarDoc) -> Result<RemoteObject, RemoteError>;
    fn update(
        &mut self,
        id: &RemoteId,
        doc: &VCalendarDoc,
        pre: Precondition,
    ) -> Result<RemoteObject, RemoteError>;
    /// Change only `identity`'s own `PARTSTAT` (on the master when
    /// `recurrence_id` is `None`, else on that instance).
    fn set_own_partstat(
        &mut self,
        id: &RemoteId,
        identity: &str,
        partstat: &str,
        recurrence_id: Option<&EventTime>,
        pre: Precondition,
    ) -> Result<RemoteObject, RemoteError>;
    fn delete(&mut self, id: &RemoteId, pre: Precondition) -> Result<(), RemoteError>;
    /// Find a remote copy by iCalendar `UID` (adoption of pre-existing
    /// objects when a local one is pushed for the first time).
    fn find_by_uid(&mut self, uid: &str) -> Result<Option<RemoteObject>, RemoteError>;
    fn capabilities(&self) -> Capabilities;
}

// ---------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------

/// A parsed calendar object: one master `VEVENT` plus its overrides.
#[derive(Clone, Debug)]
pub struct VCalendarDoc {
    pub ics: String,
    pub uid: String,
    pub master: VEvent,
    /// `(RECURRENCE-ID, override)` pairs in document order.
    pub overrides: Vec<(EventTime, VEvent)>,
}

impl VCalendarDoc {
    pub fn parse(ics: &str) -> Result<Self> {
        let events = calendar::parse_vevents(ics).map_err(|e| anyhow::anyhow!("{}", e))?;
        let master = events
            .iter()
            .find(|e| !calendar::is_recurrence_override(e))
            .or_else(|| events.first())
            .cloned()
            .context("calendar object has no VEVENT")?;
        let mut overrides = Vec::new();
        for ev in events
            .iter()
            .filter(|e| calendar::is_recurrence_override(e))
        {
            let raw = ev.raw.as_deref().unwrap_or("");
            if let Some(line) = calendar::raw_property_lines(raw, &["RECURRENCE-ID"]).first()
                && let Ok(rid) = calendar::parse_property_time(line)
            {
                overrides.push((rid, ev.clone()));
            }
        }
        Ok(Self {
            ics: ics.to_string(),
            uid: master.uid.clone(),
            master,
            overrides,
        })
    }
}

/// How `identity` relates to an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityRole {
    Organizer,
    Attendee {
        partstat: Option<String>,
    },
    /// Neither organizer nor attendee — including events with no
    /// scheduling properties at all, which the identity implicitly owns.
    Uninvolved,
}

pub fn identity_role(master: &VEvent, identity: &str) -> IdentityRole {
    let me = identity.to_ascii_lowercase();
    if let Some(org) = &master.organizer
        && org.to_ascii_lowercase() == me
    {
        return IdentityRole::Organizer;
    }
    if let Some(a) = master
        .attendees
        .iter()
        .find(|a| a.email.to_ascii_lowercase() == me)
    {
        return IdentityRole::Attendee {
            partstat: a.partstat.clone(),
        };
    }
    IdentityRole::Uninvolved
}

/// Whether an object counts as "ours to edit" on the remote: we organize
/// it, or nobody does.
pub fn identity_may_edit(master: &VEvent, identity: &str) -> bool {
    match identity_role(master, identity) {
        IdentityRole::Organizer => true,
        IdentityRole::Attendee { .. } => false,
        IdentityRole::Uninvolved => master.organizer.is_none(),
    }
}

/// `(RECURRENCE-ID or None for the master, own PARTSTAT)` per component.
pub fn own_partstats(
    doc: &VCalendarDoc,
    identity: &str,
) -> Vec<(Option<EventTime>, Option<String>)> {
    let me = identity.to_ascii_lowercase();
    let mine = |ev: &VEvent| {
        ev.attendees
            .iter()
            .find(|a| a.email.to_ascii_lowercase() == me)
            .map(|a| {
                a.partstat
                    .clone()
                    .unwrap_or_else(|| "NEEDS-ACTION".to_string())
            })
    };
    let mut out = vec![(None, mine(&doc.master))];
    for (rid, ev) in &doc.overrides {
        out.push((Some(rid.clone()), mine(ev)));
    }
    out
}

// ---------------------------------------------------------------------
// Semantic fingerprint
// ---------------------------------------------------------------------

/// The parts of an event that mean something to a user, in a canonical
/// textual form, so two representations of the same event (Google JSON
/// rendered to ICS, a client's own ICS) compare equal. Excludes `UID`,
/// `SEQUENCE`, `DTSTAMP`, `CREATED`, `LAST-MODIFIED`, `ORGANIZER`,
/// attendee display names and every `X-` stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Semantic {
    pub lines: Vec<String>,
}

fn time_key(t: &EventTime) -> String {
    if t.all_day {
        format!("D:{}", t.utc.format("%Y-%m-%d"))
    } else {
        format!(
            "T:{}|{}",
            t.utc.format("%Y-%m-%dT%H:%M:%SZ"),
            t.tzid.as_deref().unwrap_or("-")
        )
    }
}

fn norm_text(s: &str) -> String {
    s.replace("\r\n", "\n").trim().to_string()
}

impl Semantic {
    fn component_lines(ev: &VEvent, include_recurrence: bool) -> Vec<String> {
        let raw = ev.raw.as_deref().unwrap_or("");
        let mut lines = vec![
            format!("summary={}", norm_text(&ev.summary)),
            format!("start={}", time_key(&ev.dtstart)),
            format!("end={}", time_key(&ev.dtend)),
            format!("description={}", norm_text(&ev.description)),
            format!("location={}", norm_text(&ev.location)),
            format!("status={}", ev.status.as_ics()),
        ];
        let transp = calendar::raw_property_lines(raw, &["TRANSP"])
            .first()
            .and_then(|l| {
                l.split_once(':')
                    .map(|(_, v)| v.trim().to_ascii_uppercase())
            })
            .unwrap_or_else(|| "OPAQUE".to_string());
        lines.push(format!("transp={}", transp));
        let class = calendar::raw_property_lines(raw, &["CLASS"])
            .first()
            .and_then(|l| {
                l.split_once(':')
                    .map(|(_, v)| v.trim().to_ascii_uppercase())
            })
            .unwrap_or_else(|| "DEFAULT".to_string());
        lines.push(format!("class={}", class));
        if include_recurrence {
            let mut rec: Vec<String> = Vec::new();
            if let Some(r) = &ev.rrule {
                rec.push(format!("RRULE:{}", r.trim().to_ascii_uppercase()));
            }
            let mut ex: Vec<String> = ev
                .exdates
                .iter()
                .map(|d| format!("EXDATE:{}", d.format("%Y-%m-%dT%H:%M:%SZ")))
                .collect();
            ex.sort();
            rec.extend(ex);
            for line in calendar::raw_property_lines(raw, &["RDATE"]) {
                rec.push(format!(
                    "RDATE:{}",
                    line.split_once(':').map(|(_, v)| v).unwrap_or("").trim()
                ));
            }
            lines.push(format!("recurrence={}", rec.join(";;")));
        }
        let mut att: Vec<String> = ev
            .attendees
            .iter()
            .map(|a| {
                format!(
                    "{};{};{}",
                    a.email.to_ascii_lowercase(),
                    a.partstat
                        .clone()
                        .unwrap_or_else(|| "NEEDS-ACTION".to_string())
                        .to_ascii_uppercase(),
                    if a.role
                        .as_deref()
                        .is_some_and(|r| r.eq_ignore_ascii_case("OPT-PARTICIPANT"))
                    {
                        1
                    } else {
                        0
                    }
                )
            })
            .collect();
        att.sort();
        lines.push(format!("attendees={}", att.join(",")));
        let mut reminders: Vec<String> = ev
            .alarms
            .iter()
            .filter_map(|al| match &al.trigger {
                calendar::AlarmTrigger::Relative {
                    offset,
                    related: calendar::AlarmRelated::Start,
                } => Some(format!(
                    "{}:{}",
                    al.action.to_ascii_uppercase(),
                    -offset.num_minutes()
                )),
                _ => None,
            })
            .collect();
        reminders.sort();
        lines.push(format!("reminders={}", reminders.join(",")));
        lines
    }

    pub fn from_doc(doc: &VCalendarDoc) -> Self {
        let mut lines = Self::component_lines(&doc.master, true);
        let mut overrides: Vec<(String, Vec<String>)> = doc
            .overrides
            .iter()
            .map(|(rid, ev)| (time_key(rid), Self::component_lines(ev, false)))
            .collect();
        overrides.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, comp) in overrides {
            lines.push(format!("override[{}]={}", key, comp.join("|")));
        }
        Self { lines }
    }

    pub fn from_ics(ics: &str) -> Result<Self> {
        Ok(Self::from_doc(&VCalendarDoc::parse(ics)?))
    }

    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.lines.join("\n").as_bytes());
        digest[..16].iter().map(|b| format!("{:02x}", b)).collect()
    }
}

/// Fingerprint of an ICS document, or of its raw bytes when it does not
/// parse (so unparsable objects still compare by content).
pub fn fingerprint_ics(ics: &str) -> String {
    match Semantic::from_ics(ics) {
        Ok(s) => s.fingerprint(),
        Err(_) => {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(ics.as_bytes());
            format!(
                "raw:{}",
                digest[..12]
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>()
            )
        }
    }
}

// ---------------------------------------------------------------------
// Push decisions (pure)
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorMapRow {
    pub calendar_slug: String,
    pub uid: String,
    pub remote_id: String,
    pub remote_version: Option<String>,
    pub remote_uid: Option<String>,
    pub fingerprint: Option<String>,
    pub pushed_rev: i64,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalState {
    pub rev: i64,
    pub origin: Origin,
    pub deleted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushAction {
    Nothing,
    /// New locally; `find_by_uid` first — a hit becomes an adoption.
    Create,
    UpdateFull,
    /// Attendee side: compare own PARTSTAT with the remote copy and push
    /// only that (or revert the local edit).
    RsvpIfChanged,
    DeleteRemote,
    /// Attendee side delete: decline first so the organizer learns of it,
    /// then remove the remote copy from our view.
    DeclineThenDelete,
    /// Never reached the remote: just drop the soft-deleted row.
    FinalizeLocalDelete,
}

pub fn decide_push(
    local: &LocalState,
    map: Option<&MirrorMapRow>,
    role: &IdentityRole,
    two_way: bool,
) -> PushAction {
    if !two_way {
        return PushAction::Nothing;
    }
    if matches!(local.origin, Origin::Mirror | Origin::System) {
        return PushAction::Nothing;
    }
    if let Some(m) = map
        && local.rev <= m.pushed_rev
    {
        return PushAction::Nothing;
    }
    let attendee = matches!(role, IdentityRole::Attendee { .. });
    match (local.deleted, map.is_some()) {
        (false, false) => PushAction::Create,
        (true, false) => PushAction::FinalizeLocalDelete,
        (false, true) if attendee => PushAction::RsvpIfChanged,
        (false, true) => PushAction::UpdateFull,
        (true, true) if attendee => PushAction::DeclineThenDelete,
        (true, true) => PushAction::DeleteRemote,
    }
}

// ---------------------------------------------------------------------
// The sync engine
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CalendarMirrorCfg {
    pub slug: String,
    pub identity: String,
    pub two_way: bool,
    pub send_via: SendVia,
    /// `0` = unbounded.
    pub history_days: u32,
    /// The remote calendar's id (Google) or collection URL / display name
    /// (CalDAV).
    pub remote_calendar: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub pulled: usize,
    pub pushed: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    pub reverted: usize,
    pub conflicts: usize,
    pub echoes: usize,
}

pub type Logger<'a> = &'a dyn Fn(&str);

fn window_start_for(history_days: u32) -> Option<DateTime<Utc>> {
    if history_days == 0 {
        None
    } else {
        Some(Utc::now() - chrono::Duration::days(i64::from(history_days)))
    }
}

/// One mirror pass for one calendar: push, then pull.
pub fn sync_calendar_once(
    store: &Store,
    remote: &mut dyn RemoteCalendar,
    cfg: &CalendarMirrorCfg,
    log: Logger<'_>,
) -> Result<SyncStats, RemoteError> {
    let mut stats = SyncStats::default();
    if cfg.two_way {
        push_pending(store, remote, cfg, log, &mut stats)?;
    }
    pull(store, remote, cfg, log, &mut stats)?;
    Ok(stats)
}

fn record_pushed(
    store: &Store,
    cfg: &CalendarMirrorCfg,
    local: &ObjectRow,
    remote_obj: &RemoteObject,
    stats: &mut SyncStats,
    log: Logger<'_>,
) -> Result<()> {
    let mut pushed_rev = local.meta.rev;
    // If the remote normalised the object, adopt its copy now so the two
    // sides agree in one step instead of flapping.
    let local_fp = fingerprint_ics(&local.ics);
    if remote_obj.fingerprint != local_fp
        && local.meta.deleted_at.is_none()
        && let Ok(Ok(out)) = store.put_object(
            &cfg.slug,
            &local.meta.href_name,
            &remote_obj.ics,
            StorePre::None,
            Origin::Mirror,
        )
        && let Some(new) = out.new
    {
        pushed_rev = new.meta.rev;
        log(&format!(
            "{}: {} normalised by the remote; local copy refreshed",
            cfg.slug, local.meta.uid
        ));
    }
    store.upsert_mirror_map(&MirrorMapRow {
        calendar_slug: cfg.slug.clone(),
        uid: local.meta.uid.clone(),
        remote_id: remote_obj.id.0.clone(),
        remote_version: Some(remote_obj.version.0.clone()),
        remote_uid: if remote_obj.uid != local.meta.uid {
            Some(remote_obj.uid.clone())
        } else {
            None
        },
        fingerprint: Some(remote_obj.fingerprint.clone()),
        pushed_rev,
        last_error: None,
    })?;
    stats.pushed += 1;
    Ok(())
}

fn apply_remote_locally(
    store: &Store,
    cfg: &CalendarMirrorCfg,
    href_name: &str,
    obj: &RemoteObject,
    local_uid: &str,
) -> Result<i64> {
    let out = store
        .put_object(
            &cfg.slug,
            href_name,
            &obj.ics,
            StorePre::None,
            Origin::Mirror,
        )?
        .map_err(|e| anyhow::anyhow!("storing remote copy of {}: {:?}", local_uid, e))?;
    let rev = out.new.as_ref().map(|n| n.meta.rev).unwrap_or(0);
    store.upsert_mirror_map(&MirrorMapRow {
        calendar_slug: cfg.slug.clone(),
        uid: local_uid.to_string(),
        remote_id: obj.id.0.clone(),
        remote_version: Some(obj.version.0.clone()),
        remote_uid: if obj.uid != local_uid {
            Some(obj.uid.clone())
        } else {
            None
        },
        fingerprint: Some(obj.fingerprint.clone()),
        pushed_rev: rev,
        last_error: None,
    })?;
    Ok(rev)
}

fn conflict_remote_wins(
    store: &Store,
    remote: &mut dyn RemoteCalendar,
    cfg: &CalendarMirrorCfg,
    local: &ObjectRow,
    remote_id: &RemoteId,
    stats: &mut SyncStats,
    log: Logger<'_>,
) -> Result<(), RemoteError> {
    stats.conflicts += 1;
    match remote.fetch(remote_id)? {
        Some(obj) => {
            apply_remote_locally(store, cfg, &local.meta.href_name, &obj, &local.meta.uid)?;
            log(&format!(
                "{}: conflict on {} ({}); remote copy kept",
                cfg.slug, local.meta.uid, local.meta.summary
            ));
        }
        None => {
            let _ = store.delete_object(
                &cfg.slug,
                &local.meta.href_name,
                StorePre::None,
                Origin::Mirror,
            )?;
            store.finalize_client_delete(&cfg.slug, &local.meta.href_name)?;
            store.delete_mirror_map(&cfg.slug, &local.meta.uid)?;
            log(&format!(
                "{}: conflict on {} and the remote copy is gone; removed locally",
                cfg.slug, local.meta.uid
            ));
        }
    }
    Ok(())
}

fn push_pending(
    store: &Store,
    remote: &mut dyn RemoteCalendar,
    cfg: &CalendarMirrorCfg,
    log: Logger<'_>,
    stats: &mut SyncStats,
) -> Result<(), RemoteError> {
    let pending = store.list_objects_to_push(&cfg.slug)?;
    for local in pending {
        let map = store.get_mirror_map(&cfg.slug, &local.meta.uid)?;
        let doc = match VCalendarDoc::parse(&local.ics) {
            Ok(d) => d,
            Err(e) => {
                log(&format!(
                    "{}: {} unparsable, not pushed: {}",
                    cfg.slug, local.meta.href_name, e
                ));
                continue;
            }
        };
        let role = identity_role(&doc.master, &cfg.identity);
        let state = LocalState {
            rev: local.meta.rev,
            origin: local.meta.origin,
            deleted: local.meta.deleted_at.is_some(),
        };
        let mut action = decide_push(&state, map.as_ref(), &role, cfg.two_way);
        // Adoption: a brand-new local object may already exist remotely.
        if action == PushAction::Create
            && let Some(existing) = remote.find_by_uid(&local.meta.uid)?
        {
            store.upsert_mirror_map(&MirrorMapRow {
                calendar_slug: cfg.slug.clone(),
                uid: local.meta.uid.clone(),
                remote_id: existing.id.0.clone(),
                remote_version: Some(existing.version.0.clone()),
                remote_uid: None,
                fingerprint: Some(existing.fingerprint.clone()),
                pushed_rev: 0,
                last_error: None,
            })?;
            action = if matches!(role, IdentityRole::Attendee { .. }) {
                PushAction::RsvpIfChanged
            } else {
                PushAction::UpdateFull
            };
        }
        let map = store.get_mirror_map(&cfg.slug, &local.meta.uid)?;
        let remote_id = map.as_ref().map(|m| RemoteId(m.remote_id.clone()));
        let pre = map
            .as_ref()
            .and_then(|m| m.remote_version.clone())
            .map(|v| Precondition::IfMatch(RemoteVersion(v)))
            .unwrap_or(Precondition::None);
        match action {
            PushAction::Nothing => {}
            PushAction::FinalizeLocalDelete => {
                store.finalize_client_delete(&cfg.slug, &local.meta.href_name)?;
            }
            PushAction::Create => match remote.create(&doc) {
                Ok(obj) => {
                    log(&format!(
                        "{}: created {} on the remote",
                        cfg.slug, local.meta.summary
                    ));
                    record_pushed(store, cfg, &local, &obj, stats, log)?;
                }
                Err(RemoteError::Conflict(m)) => {
                    log(&format!(
                        "{}: create of {} conflicted ({}); will adopt next pass",
                        cfg.slug, local.meta.uid, m
                    ));
                    stats.conflicts += 1;
                }
                Err(e) => return Err(e),
            },
            PushAction::UpdateFull => {
                let id = remote_id.clone().expect("map present for update");
                match remote.update(&id, &doc, pre.clone()) {
                    Ok(obj) => {
                        log(&format!(
                            "{}: updated {} on the remote",
                            cfg.slug, local.meta.summary
                        ));
                        record_pushed(store, cfg, &local, &obj, stats, log)?;
                    }
                    Err(RemoteError::Conflict(_)) => {
                        conflict_remote_wins(store, remote, cfg, &local, &id, stats, log)?;
                    }
                    Err(RemoteError::Forbidden(m)) => {
                        // Not actually ours to edit remotely: fall back to RSVP.
                        log(&format!(
                            "{}: remote refused a full update of {} ({}); trying RSVP only",
                            cfg.slug, local.meta.uid, m
                        ));
                        push_rsvp_only(store, remote, cfg, &local, &doc, &id, pre, stats, log)?;
                    }
                    Err(e) => return Err(e),
                }
            }
            PushAction::RsvpIfChanged => {
                let id = remote_id.clone().expect("map present for rsvp");
                push_rsvp_only(store, remote, cfg, &local, &doc, &id, pre, stats, log)?;
            }
            PushAction::DeleteRemote => {
                let id = remote_id.clone().expect("map present for delete");
                match remote.delete(&id, pre) {
                    Ok(()) | Err(RemoteError::NotFound) => {
                        store.finalize_client_delete(&cfg.slug, &local.meta.href_name)?;
                        store.delete_mirror_map(&cfg.slug, &local.meta.uid)?;
                        stats.deleted_remote += 1;
                        log(&format!(
                            "{}: deleted {} on the remote",
                            cfg.slug, local.meta.summary
                        ));
                    }
                    Err(RemoteError::Conflict(_)) => {
                        conflict_remote_wins(store, remote, cfg, &local, &id, stats, log)?;
                    }
                    Err(e) => return Err(e),
                }
            }
            PushAction::DeclineThenDelete => {
                let id = remote_id.clone().expect("map present for decline");
                let declined = remote.set_own_partstat(&id, &cfg.identity, "DECLINED", None, pre);
                match declined {
                    Ok(obj) => match remote.delete(&id, Precondition::IfMatch(obj.version.clone()))
                    {
                        Ok(()) | Err(RemoteError::NotFound) => {
                            store.finalize_client_delete(&cfg.slug, &local.meta.href_name)?;
                            store.delete_mirror_map(&cfg.slug, &local.meta.uid)?;
                            stats.deleted_remote += 1;
                            log(&format!(
                                "{}: declined and removed {}",
                                cfg.slug, local.meta.summary
                            ));
                        }
                        Err(RemoteError::Forbidden(_)) | Err(RemoteError::Conflict(_)) => {
                            // Keep the declined copy locally instead.
                            apply_remote_locally(
                                store,
                                cfg,
                                &local.meta.href_name,
                                &obj,
                                &local.meta.uid,
                            )?;
                            log(&format!(
                                "{}: declined {} (remote would not let us remove it)",
                                cfg.slug, local.meta.summary
                            ));
                        }
                        Err(e) => return Err(e),
                    },
                    Err(RemoteError::Conflict(_)) => {
                        conflict_remote_wins(store, remote, cfg, &local, &id, stats, log)?;
                    }
                    Err(RemoteError::NotFound) => {
                        store.finalize_client_delete(&cfg.slug, &local.meta.href_name)?;
                        store.delete_mirror_map(&cfg.slug, &local.meta.uid)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_rsvp_only(
    store: &Store,
    remote: &mut dyn RemoteCalendar,
    cfg: &CalendarMirrorCfg,
    local: &ObjectRow,
    doc: &VCalendarDoc,
    id: &RemoteId,
    pre: Precondition,
    stats: &mut SyncStats,
    log: Logger<'_>,
) -> Result<(), RemoteError> {
    let Some(remote_obj) = remote.fetch(id)? else {
        // Gone remotely: the remote wins.
        let _ = store.delete_object(
            &cfg.slug,
            &local.meta.href_name,
            StorePre::None,
            Origin::Mirror,
        )?;
        store.finalize_client_delete(&cfg.slug, &local.meta.href_name)?;
        store.delete_mirror_map(&cfg.slug, &local.meta.uid)?;
        stats.deleted_local += 1;
        return Ok(());
    };
    let remote_doc = VCalendarDoc::parse(&remote_obj.ics)?;
    let mine = own_partstats(doc, &cfg.identity);
    let theirs = own_partstats(&remote_doc, &cfg.identity);
    let mut latest = remote_obj.clone();
    let mut pushed_any = false;
    let mut pre = pre;
    for (rid, my_ps) in &mine {
        let Some(my_ps) = my_ps else { continue };
        let remote_ps = theirs
            .iter()
            .find(|(r, _)| match (r, rid) {
                (None, None) => true,
                (Some(a), Some(b)) => a.utc == b.utc && a.all_day == b.all_day,
                _ => false,
            })
            .and_then(|(_, p)| p.clone());
        if remote_ps.as_deref() == Some(my_ps.as_str()) {
            continue;
        }
        if my_ps.eq_ignore_ascii_case("NEEDS-ACTION") {
            continue;
        }
        match remote.set_own_partstat(id, &cfg.identity, my_ps, rid.as_ref(), pre.clone()) {
            Ok(obj) => {
                pre = Precondition::IfMatch(obj.version.clone());
                latest = obj;
                pushed_any = true;
                log(&format!(
                    "{}: RSVP {} -> {}{}",
                    cfg.slug,
                    my_ps,
                    local.meta.summary,
                    rid.as_ref()
                        .map(|r| format!(" ({})", r.utc.format("%Y-%m-%d")))
                        .unwrap_or_default()
                ));
            }
            Err(RemoteError::Conflict(_)) => {
                return conflict_remote_wins(store, remote, cfg, local, id, stats, log);
            }
            Err(e) => return Err(e),
        }
    }
    if pushed_any {
        record_pushed(store, cfg, local, &latest, stats, log)?;
    } else {
        // Nothing RSVP-shaped changed: attendee-side edits do not propagate,
        // so bring the local copy back in line with the remote.
        apply_remote_locally(
            store,
            cfg,
            &local.meta.href_name,
            &remote_obj,
            &local.meta.uid,
        )?;
        stats.reverted += 1;
        log(&format!(
            "{}: local edit on {} ignored (only your RSVP syncs on events you do not organise)",
            cfg.slug, local.meta.summary
        ));
    }
    Ok(())
}

fn local_href_for(uid: &str) -> String {
    format!("{}.ics", crate::caldav::sanitize_uid(uid))
}

fn pull(
    store: &Store,
    remote: &mut dyn RemoteCalendar,
    cfg: &CalendarMirrorCfg,
    log: Logger<'_>,
    stats: &mut SyncStats,
) -> Result<(), RemoteError> {
    let state = store.calendar_mirror_state(&cfg.slug)?;
    let window = window_start_for(cfg.history_days);
    let mut token = state.remote_sync_token.clone();
    let changes = match remote.list_changes(token.as_deref(), window) {
        Ok(c) => c,
        Err(RemoteError::FullResyncRequired) => {
            log(&format!(
                "{}: remote sync token rejected; full resync",
                cfg.slug
            ));
            token = None;
            let mut c = remote.list_changes(None, window)?;
            c.full_resync = true;
            c
        }
        Err(e) => return Err(e),
    };
    let full = changes.full_resync || token.is_none();
    let mut seen: Vec<String> = Vec::new();
    for item in &changes.items {
        seen.push(item.id.0.clone());
        let existing_map = store.find_mirror_map_by_remote_id(&cfg.slug, &item.id.0)?;
        let (local_uid, href_name) = match &existing_map {
            Some(m) => {
                let href = store
                    .get_object_including_deleted_by_uid(&cfg.slug, &m.uid)?
                    .map(|o| o.meta.href_name)
                    .unwrap_or_else(|| local_href_for(&m.uid));
                (m.uid.clone(), href)
            }
            None => match store.get_object_by_uid(&cfg.slug, &item.uid)? {
                Some(o) => (o.meta.uid.clone(), o.meta.href_name.clone()),
                None => (item.uid.clone(), local_href_for(&item.uid)),
            },
        };
        if let Some(m) = &existing_map {
            if m.remote_version.as_deref() == Some(item.version.0.as_str()) {
                stats.echoes += 1;
                continue;
            }
            if m.fingerprint.as_deref() == Some(item.fingerprint.as_str()) {
                let mut refreshed = m.clone();
                refreshed.remote_version = Some(item.version.0.clone());
                store.upsert_mirror_map(&refreshed)?;
                stats.echoes += 1;
                continue;
            }
        }
        if let Some(local) = store.get_object_including_deleted_by_uid(&cfg.slug, &local_uid)?
            && matches!(local.meta.origin, Origin::Client | Origin::Inbound)
            && existing_map
                .as_ref()
                .is_some_and(|m| local.meta.rev > m.pushed_rev)
        {
            stats.conflicts += 1;
            log(&format!(
                "{}: both sides changed {}; remote copy kept",
                cfg.slug, local.meta.summary
            ));
        }
        apply_remote_locally(store, cfg, &href_name, item, &local_uid)?;
        stats.pulled += 1;
    }
    for gone in &changes.deleted {
        if let Some(m) = store.find_mirror_map_by_remote_id(&cfg.slug, &gone.0)? {
            if let Some(o) = store.get_object_including_deleted_by_uid(&cfg.slug, &m.uid)? {
                let _ = store.delete_object(
                    &cfg.slug,
                    &o.meta.href_name,
                    StorePre::None,
                    Origin::Mirror,
                );
                store.finalize_client_delete(&cfg.slug, &o.meta.href_name)?;
            }
            store.delete_mirror_map(&cfg.slug, &m.uid)?;
            stats.deleted_local += 1;
        }
    }
    if full {
        // Anything mapped, unlisted and still inside the window is gone.
        let window_ts = window.map(|w| w.timestamp());
        for m in store.list_mirror_map(&cfg.slug)? {
            if seen.contains(&m.remote_id) {
                continue;
            }
            let Some(o) = store.get_object_including_deleted_by_uid(&cfg.slug, &m.uid)? else {
                store.delete_mirror_map(&cfg.slug, &m.uid)?;
                continue;
            };
            let inside_window = match window_ts {
                None => true,
                Some(w) => o.meta.rrule.is_some() || o.meta.dtend_utc.unwrap_or(i64::MAX) >= w,
            };
            if inside_window {
                let _ = store.delete_object(
                    &cfg.slug,
                    &o.meta.href_name,
                    StorePre::None,
                    Origin::Mirror,
                );
                store.finalize_client_delete(&cfg.slug, &o.meta.href_name)?;
                store.delete_mirror_map(&cfg.slug, &m.uid)?;
                stats.deleted_local += 1;
            }
        }
    }
    store.set_calendar_mirror_state(
        &cfg.slug,
        changes.next_token.as_deref().or(token.as_deref()),
        if full {
            Some(Utc::now().timestamp())
        } else {
            state.last_full_fill_at
        },
        window.map(|w| w.timestamp()),
    )?;
    // Everything up to the newest change-log row is now reflected on the
    // remote or came from it: the compaction floor may move.
    store.set_mirror_pushed_log_id(&cfg.slug, store.ctag(&cfg.slug)?)?;
    Ok(())
}

// ---------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------

pub struct MirrorControl {
    pub shutdown: AtomicBool,
    pub force: AtomicBool,
}

impl MirrorControl {
    pub fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            force: AtomicBool::new(false),
        }
    }
}

impl Default for MirrorControl {
    fn default() -> Self {
        Self::new()
    }
}

/// Wake-up routing from the server: a client write to a mirrored calendar
/// nudges that calendar's remote thread so the push happens in seconds.
#[derive(Default)]
pub struct MirrorHub {
    controls: HashMap<String, Arc<MirrorControl>>,
    calendar_to_remote: HashMap<String, String>,
}

impl MirrorHub {
    pub fn register(&mut self, remote: &str, control: Arc<MirrorControl>, slugs: &[String]) {
        self.controls.insert(remote.to_string(), control);
        for s in slugs {
            self.calendar_to_remote
                .insert(s.clone(), remote.to_string());
        }
    }

    pub fn wake_for_calendar(&self, slug: &str) {
        if let Some(remote) = self.calendar_to_remote.get(slug)
            && let Some(c) = self.controls.get(remote)
        {
            c.force.store(true, Ordering::Relaxed);
        }
    }

    pub fn shutdown_all(&self) {
        for c in self.controls.values() {
            c.shutdown.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_mirrored(&self, slug: &str) -> bool {
        self.calendar_to_remote.contains_key(slug)
    }
}

/// Builds a fresh provider for one calendar of a remote; called once per
/// thread start so credentials/clients live on the mirror thread.
pub type ProviderFactory =
    Box<dyn Fn(&CalendarMirrorCfg) -> Result<Box<dyn RemoteCalendar>> + Send>;

/// Finds the calendars a remote should mirror beyond the configured ones
/// (`mirror_all`): lists the remote's calendars, records new ones in the
/// store and returns the mirror config of every discovered calendar.
pub type Discoverer = Box<dyn FnMut(&Store) -> Result<Vec<CalendarMirrorCfg>> + Send>;

/// How often a remote's calendar list is re-read for new calendars.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(3600);

fn sleep_or_wake(control: &MirrorControl, total: Duration) {
    let step = Duration::from_millis(250);
    let mut waited = Duration::ZERO;
    while waited < total {
        if control.shutdown.load(Ordering::Relaxed) || control.force.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(step);
        waited += step;
    }
}

pub fn spawn_mirror_thread(
    remote_name: String,
    calendars: Vec<CalendarMirrorCfg>,
    store_path: PathBuf,
    poll_interval: Duration,
    make_provider: ProviderFactory,
    mut discover: Option<Discoverer>,
    control: Arc<MirrorControl>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let log = |msg: &str| eprintln!("jadav: mirror[{}] {}", remote_name, msg);
        let mut calendars = calendars;
        let mut providers: Vec<(CalendarMirrorCfg, Box<dyn RemoteCalendar>)> = Vec::new();
        let mut failures: u32 = 0;
        let mut last_discovery: Option<Instant> = None;
        loop {
            if control.shutdown.load(Ordering::Relaxed) {
                return;
            }
            control.force.store(false, Ordering::Relaxed);
            let store = match Store::open(&store_path) {
                Ok(s) => s,
                Err(e) => {
                    log(&format!("cannot open store: {:#}", e));
                    sleep_or_wake(&control, Duration::from_secs(30));
                    continue;
                }
            };
            if store.remote_needs_reauth(&remote_name).unwrap_or(false) {
                sleep_or_wake(&control, Duration::from_secs(60));
                continue;
            }
            if providers.is_empty() {
                for cfg in &calendars {
                    match make_provider(cfg) {
                        Ok(p) => providers.push((cfg.clone(), p)),
                        Err(e) => log(&format!("{}: cannot build provider: {:#}", cfg.slug, e)),
                    }
                }
            }
            // `mirror_all`: pick up calendars added on the remote since the
            // last look, hourly. A failed listing only delays discovery.
            if let Some(d) = discover.as_mut()
                && last_discovery.is_none_or(|t| t.elapsed() >= DISCOVERY_INTERVAL)
            {
                last_discovery = Some(Instant::now());
                match d(&store) {
                    Ok(found) => {
                        for cfg in found {
                            if calendars.iter().any(|c| c.slug == cfg.slug) {
                                continue;
                            }
                            log(&format!(
                                "discovered calendar {} ({}, {})",
                                cfg.slug,
                                cfg.remote_calendar,
                                if cfg.two_way {
                                    "read-write"
                                } else {
                                    "read-only"
                                }
                            ));
                            match make_provider(&cfg) {
                                Ok(p) => providers.push((cfg.clone(), p)),
                                Err(e) => {
                                    log(&format!("{}: cannot build provider: {:#}", cfg.slug, e))
                                }
                            }
                            calendars.push(cfg);
                        }
                    }
                    Err(e) => log(&format!("calendar discovery failed: {:#}", e)),
                }
            }
            let mut cycle_error: Option<String> = None;
            let mut reauth = false;
            for (cfg, provider) in providers.iter_mut() {
                // A bug in one calendar's sync must not take the whole remote
                // thread down: catch the panic, report it, keep polling.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    sync_calendar_once(&store, provider.as_mut(), cfg, &log)
                }));
                let outcome = match outcome {
                    Ok(o) => o,
                    Err(payload) => {
                        let msg = payload
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_else(|| "unknown panic".to_string());
                        Err(RemoteError::Other(anyhow::anyhow!(
                            "internal error (panic): {}",
                            msg
                        )))
                    }
                };
                match outcome {
                    Ok(stats) => {
                        if stats != SyncStats::default() {
                            log(&format!(
                                "{}: pulled {} pushed {} deleted(local {} remote {}) reverted {} conflicts {} echoes {}",
                                cfg.slug,
                                stats.pulled,
                                stats.pushed,
                                stats.deleted_local,
                                stats.deleted_remote,
                                stats.reverted,
                                stats.conflicts,
                                stats.echoes
                            ));
                        }
                    }
                    Err(RemoteError::NeedsReauth) => {
                        reauth = true;
                        cycle_error = Some("authorization revoked".to_string());
                        break;
                    }
                    Err(e) => {
                        cycle_error = Some(format!("{}: {}", cfg.slug, e));
                        break;
                    }
                }
            }
            match &cycle_error {
                None => {
                    failures = 0;
                    let _ = store.set_remote_status(&remote_name, false, None);
                }
                Some(msg) => {
                    failures = failures.saturating_add(1);
                    log(msg);
                    let _ = store.set_remote_status(&remote_name, reauth, Some(msg));
                    if reauth {
                        log("stopping until `jadav google auth` is run again");
                    }
                }
            }
            drop(store);
            let backoff = if failures == 0 {
                poll_interval
            } else {
                (poll_interval * 2u32.saturating_pow(failures.min(5))).min(Duration::from_secs(600))
            };
            sleep_or_wake(&control, backoff);
        }
    })
}

/// Human-readable status lines for `jadav status`.
pub fn status_lines(store: &Store) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for row in store.list_remote_status()? {
        let (remote, needs_reauth, last_ok_at, last_error, failures) = (
            row.remote,
            row.needs_reauth,
            row.last_ok_at,
            row.last_error,
            row.consecutive_failures,
        );
        let ok = last_ok_at
            .and_then(|t| DateTime::<Utc>::from_timestamp(t, 0))
            .map(|t| t.format("%Y-%m-%d %H:%M:%SZ").to_string())
            .unwrap_or_else(|| "never".to_string());
        out.push(format!(
            "remote {}: last ok {}{}{}",
            remote,
            ok,
            if needs_reauth { "; NEEDS RE-AUTH" } else { "" },
            last_error
                .map(|e| format!("; last error ({} in a row): {}", failures, e))
                .unwrap_or_default()
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Shared helper for providers: assembling a document from parts
// ---------------------------------------------------------------------

/// Assemble a complete `VCALENDAR` from component blocks, adding one
/// `VTIMEZONE` per referenced IANA `TZID` (generated from `chrono-tz`'s
/// rules for the years the event touches).
pub fn assemble_vcalendar(prodid: &str, components: &[String]) -> String {
    let mut tzids: Vec<String> = Vec::new();
    let mut years: Vec<i32> = Vec::new();
    for comp in components {
        for line in calendar::unfold(comp) {
            if let Some(cl) = calendar::parse_content_line(&line)
                && let Some(tz) = cl.param("TZID")
                && !tzids.iter().any(|t| t == tz)
            {
                tzids.push(tz.to_string());
            }
            if let Some(cl) = calendar::parse_content_line(&line)
                && matches!(cl.name.as_str(), "DTSTART" | "DTEND" | "RECURRENCE-ID")
                && let Some(y) = cl.value.get(..4).and_then(|s| s.parse::<i32>().ok())
            {
                years.push(y);
            }
        }
    }
    let (from, to) = match (years.iter().min(), years.iter().max()) {
        (Some(a), Some(b)) => (*a - 1, *b + 2),
        _ => (Utc::now().year() - 1, Utc::now().year() + 2),
    };
    let mut out = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n");
    out.push_str(&format!("PRODID:{}\r\n", prodid));
    out.push_str("CALSCALE:GREGORIAN\r\n");
    for tz in &tzids {
        if let Some(block) = render_vtimezone(tz, from, to) {
            out.push_str(&block);
        }
    }
    for comp in components {
        let trimmed = comp.trim_matches(|c| c == '\r' || c == '\n');
        out.push_str(trimmed);
        out.push_str("\r\n");
    }
    out.push_str("END:VCALENDAR\r\n");
    calendar::fold_ics(&out)
}

/// A `VTIMEZONE` component for an IANA zone covering `[from_year,
/// to_year]`, derived by scanning `chrono-tz` offsets: one `STANDARD` /
/// `DAYLIGHT` sub-component per transition (fixed-offset zones get a single
/// `STANDARD`). `None` for an unknown zone.
pub fn render_vtimezone(tzid: &str, from_year: i32, to_year: i32) -> Option<String> {
    use chrono::{NaiveDate, Offset, TimeZone};
    use chrono_tz::OffsetName;
    let tz: chrono_tz::Tz = tzid.parse().ok()?;
    let mut out = format!("BEGIN:VTIMEZONE\r\nTZID:{}\r\n", tzid);
    let start = NaiveDate::from_ymd_opt(from_year, 1, 1)?.and_hms_opt(0, 0, 0)?;
    let end = NaiveDate::from_ymd_opt(to_year, 12, 31)?.and_hms_opt(0, 0, 0)?;
    let offset_at = |t: chrono::NaiveDateTime| -> (i32, String) {
        let dt = tz.from_utc_datetime(&t);
        (
            dt.offset().fix().local_minus_utc(),
            dt.offset().abbreviation().unwrap_or("").to_string(),
        )
    };
    let fmt_off = |secs: i32| -> String {
        let sign = if secs < 0 { '-' } else { '+' };
        let a = secs.abs();
        format!("{}{:02}{:02}", sign, a / 3600, (a % 3600) / 60)
    };
    let mut transitions: Vec<(chrono::NaiveDateTime, i32, i32, String)> = Vec::new();
    let mut cursor = start;
    let (mut prev_off, _) = offset_at(cursor);
    let initial = prev_off;
    while cursor < end {
        let next = cursor + chrono::Duration::days(1);
        let (off, _) = offset_at(next);
        if off != prev_off {
            // Bisect the day to the exact second of the transition.
            let (mut lo, mut hi) = (cursor, next);
            while hi - lo > chrono::Duration::seconds(1) {
                let mid = lo + (hi - lo) / 2;
                if offset_at(mid).0 == prev_off {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let (_, abbr) = offset_at(hi);
            transitions.push((hi, prev_off, off, abbr));
            prev_off = off;
        }
        cursor = next;
    }
    if transitions.is_empty() {
        let (_, abbr) = offset_at(start);
        out.push_str(&format!(
            "BEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:{}\r\nTZOFFSETTO:{}\r\nTZNAME:{}\r\nEND:STANDARD\r\n",
            fmt_off(initial),
            fmt_off(initial),
            if abbr.is_empty() { tzid.to_string() } else { abbr }
        ));
    } else {
        for (at_utc, from, to, abbr) in transitions {
            // DTSTART is local time *before* the change (the old offset).
            let local = at_utc + chrono::Duration::seconds(i64::from(from));
            let kind = if to > from { "DAYLIGHT" } else { "STANDARD" };
            out.push_str(&format!(
                "BEGIN:{kind}\r\nDTSTART:{}\r\nTZOFFSETFROM:{}\r\nTZOFFSETTO:{}\r\nTZNAME:{}\r\nEND:{kind}\r\n",
                local.format("%Y%m%dT%H%M%S"),
                fmt_off(from),
                fmt_off(to),
                if abbr.is_empty() { tzid.to_string() } else { abbr }
            ));
        }
    }
    out.push_str("END:VTIMEZONE\r\n");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//x//EN\r\nBEGIN:VEVENT\r\nUID:s1\r\nDTSTART;TZID=Europe/Budapest:20240115T090000\r\nDTEND;TZID=Europe/Budapest:20240115T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO\r\nEXDATE;TZID=Europe/Budapest:20240129T090000\r\nSUMMARY:Weekly\r\nDESCRIPTION:Agenda\r\nORGANIZER;CN=Boss:mailto:boss@example.com\r\nATTENDEE;CN=Me;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@example.com\r\nATTENDEE;CN=Other;ROLE=OPT-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:other@example.com\r\nTRANSP:OPAQUE\r\nSEQUENCE:3\r\nDTSTAMP:20240101T000000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nDESCRIPTION:x\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:s1\r\nRECURRENCE-ID;TZID=Europe/Budapest:20240122T090000\r\nDTSTART;TZID=Europe/Budapest:20240122T110000\r\nDTEND;TZID=Europe/Budapest:20240122T120000\r\nSUMMARY:Weekly (moved)\r\nATTENDEE;CN=Me;PARTSTAT=TENTATIVE:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn vcalendar_doc_parses_master_and_overrides() {
        let doc = VCalendarDoc::parse(DOC).unwrap();
        assert_eq!(doc.uid, "s1");
        assert_eq!(doc.master.summary, "Weekly");
        assert_eq!(doc.overrides.len(), 1);
        assert_eq!(doc.overrides[0].1.summary, "Weekly (moved)");
        assert_eq!(doc.overrides[0].0.tzid.as_deref(), Some("Europe/Budapest"));
    }

    #[test]
    fn identity_roles_and_own_partstats() {
        let doc = VCalendarDoc::parse(DOC).unwrap();
        assert_eq!(
            identity_role(&doc.master, "BOSS@example.com"),
            IdentityRole::Organizer
        );
        assert_eq!(
            identity_role(&doc.master, "me@example.com"),
            IdentityRole::Attendee {
                partstat: Some("NEEDS-ACTION".to_string())
            }
        );
        assert_eq!(
            identity_role(&doc.master, "nobody@example.com"),
            IdentityRole::Uninvolved
        );
        assert!(!identity_may_edit(&doc.master, "nobody@example.com"));
        let ps = own_partstats(&doc, "me@example.com");
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].1.as_deref(), Some("NEEDS-ACTION"));
        assert_eq!(ps[1].1.as_deref(), Some("TENTATIVE"));
        assert!(ps[1].0.is_some());
    }

    #[test]
    fn fingerprint_ignores_noise_and_tracks_meaning() {
        let base = Semantic::from_ics(DOC).unwrap().fingerprint();
        let noisy = DOC
            .replace("SEQUENCE:3", "SEQUENCE:9")
            .replace("DTSTAMP:20240101T000000Z", "DTSTAMP:20250101T000000Z")
            .replace("CN=Boss", "CN=The Boss")
            .replace("CN=Me;", "CN=Myself;");
        assert_eq!(Semantic::from_ics(&noisy).unwrap().fingerprint(), base);
        let renamed = DOC.replace("SUMMARY:Weekly\r\n", "SUMMARY:Weekly2\r\n");
        assert_ne!(Semantic::from_ics(&renamed).unwrap().fingerprint(), base);
        let rsvp = DOC.replace(
            "PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me",
            "PARTSTAT=ACCEPTED:mailto:me",
        );
        assert_ne!(Semantic::from_ics(&rsvp).unwrap().fingerprint(), base);
        let reminder = DOC.replace("TRIGGER:-PT15M", "TRIGGER:-PT30M");
        assert_ne!(Semantic::from_ics(&reminder).unwrap().fingerprint(), base);
        // Attendee order and parameter order do not matter.
        let reordered = DOC.replace(
            "ATTENDEE;CN=Me;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@example.com\r\nATTENDEE;CN=Other;ROLE=OPT-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:other@example.com",
            "ATTENDEE;PARTSTAT=ACCEPTED;ROLE=OPT-PARTICIPANT;CN=Other:mailto:other@example.com\r\nATTENDEE;RSVP=TRUE;PARTSTAT=NEEDS-ACTION;CN=Me:mailto:me@example.com",
        );
        assert_eq!(Semantic::from_ics(&reordered).unwrap().fingerprint(), base);
        assert!(fingerprint_ics("garbage").starts_with("raw:"));
    }

    fn map(pushed_rev: i64) -> MirrorMapRow {
        MirrorMapRow {
            calendar_slug: "c".to_string(),
            uid: "u".to_string(),
            remote_id: "r".to_string(),
            remote_version: Some("v".to_string()),
            remote_uid: None,
            fingerprint: None,
            pushed_rev,
            last_error: None,
        }
    }

    #[test]
    fn decide_push_table() {
        let org = IdentityRole::Organizer;
        let att = IdentityRole::Attendee { partstat: None };
        let live = |rev, origin| LocalState {
            rev,
            origin,
            deleted: false,
        };
        let dead = |rev, origin| LocalState {
            rev,
            origin,
            deleted: true,
        };
        assert_eq!(
            decide_push(&live(3, Origin::Client), None, &org, false),
            PushAction::Nothing
        );
        assert_eq!(
            decide_push(&live(3, Origin::Mirror), None, &org, true),
            PushAction::Nothing
        );
        assert_eq!(
            decide_push(&live(3, Origin::System), Some(&map(1)), &org, true),
            PushAction::Nothing
        );
        assert_eq!(
            decide_push(&live(3, Origin::Client), Some(&map(3)), &org, true),
            PushAction::Nothing
        );
        assert_eq!(
            decide_push(&live(1, Origin::Client), None, &org, true),
            PushAction::Create
        );
        assert_eq!(
            decide_push(&dead(2, Origin::Client), None, &org, true),
            PushAction::FinalizeLocalDelete
        );
        assert_eq!(
            decide_push(&live(4, Origin::Client), Some(&map(3)), &org, true),
            PushAction::UpdateFull
        );
        assert_eq!(
            decide_push(
                &live(4, Origin::Inbound),
                Some(&map(3)),
                &IdentityRole::Uninvolved,
                true
            ),
            PushAction::UpdateFull
        );
        assert_eq!(
            decide_push(&live(4, Origin::Client), Some(&map(3)), &att, true),
            PushAction::RsvpIfChanged
        );
        assert_eq!(
            decide_push(&dead(4, Origin::Client), Some(&map(3)), &org, true),
            PushAction::DeleteRemote
        );
        assert_eq!(
            decide_push(&dead(4, Origin::Client), Some(&map(3)), &att, true),
            PushAction::DeclineThenDelete
        );
    }

    #[test]
    fn vtimezone_for_budapest_has_dst_transitions_and_fixed_zone_one_block() {
        let tz = render_vtimezone("Europe/Budapest", 2025, 2026).unwrap();
        assert!(tz.starts_with("BEGIN:VTIMEZONE\r\nTZID:Europe/Budapest\r\n"));
        assert!(tz.contains("BEGIN:DAYLIGHT"));
        assert!(tz.contains("TZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200"));
        assert!(tz.contains("BEGIN:STANDARD"));
        assert!(tz.contains("TZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100"));
        // 2025-03-30 02:00 local is the spring change.
        assert!(tz.contains("DTSTART:20250330T020000"), "{tz}");
        assert!(tz.matches("BEGIN:DAYLIGHT").count() >= 2);
        let utc = render_vtimezone("UTC", 2025, 2025).unwrap();
        assert_eq!(utc.matches("BEGIN:STANDARD").count(), 1);
        assert!(render_vtimezone("Mars/Olympus", 2025, 2025).is_none());
    }

    #[test]
    fn assemble_vcalendar_adds_one_vtimezone_per_tzid_and_parses() {
        let master = "BEGIN:VEVENT\r\nUID:a\r\nDTSTART;TZID=Europe/Budapest:20260115T090000\r\nDTEND;TZID=Europe/London:20260115T100000\r\nSUMMARY:x\r\nEND:VEVENT";
        let doc = assemble_vcalendar("-//jadav//test//EN", &[master.to_string()]);
        assert_eq!(doc.matches("BEGIN:VTIMEZONE").count(), 2);
        assert!(doc.contains("TZID:Europe/Budapest"));
        assert!(doc.contains("TZID:Europe/London"));
        let parsed = VCalendarDoc::parse(&doc).unwrap();
        assert_eq!(parsed.master.summary, "x");
        for line in doc.split("\r\n") {
            assert!(line.len() <= 75);
        }
    }

    #[test]
    fn mirror_hub_routes_wakeups() {
        let mut hub = MirrorHub::default();
        let c = Arc::new(MirrorControl::new());
        hub.register("g", Arc::clone(&c), &["g-a".to_string()]);
        assert!(hub.is_mirrored("g-a"));
        assert!(!hub.is_mirrored("personal"));
        hub.wake_for_calendar("g-a");
        assert!(c.force.load(Ordering::Relaxed));
        hub.shutdown_all();
        assert!(c.shutdown.load(Ordering::Relaxed));
    }
}
