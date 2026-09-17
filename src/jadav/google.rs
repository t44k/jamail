//! Google Calendar as a mirror remote, over the Calendar REST API.
//!
//! - **Auth**: OAuth 2.0 with a long-lived refresh token (obtained once via
//!   `jadav google auth <remote>` — a Desktop-app PKCE flow whose redirect
//!   the user pastes back — or imported from the previous bridge's state
//!   with `jadav google import-token`). The flow and the token endpoint
//!   itself live in [`crate::goauth`], shared with `jamaild`'s Google
//!   CalDAV support; [`GoogleAuth`] is the jadav-side wrapper that adds
//!   persistence — access tokens are cached until a minute before expiry
//!   and written to the store so a restart does not spend a refresh.
//!   `invalid_grant` marks the remote as needing re-auth; the mirror
//!   thread then stops touching it until a human acts.
//! - **Listing**: `events.list` with `syncToken` (incremental) or `timeMin`
//!   (initial/full), `showDeleted=true`, paging via `nextPageToken`; `410
//!   Gone` means the token expired → full resync. Never `singleEvents=true`:
//!   Google then returns recurring masters plus *exception* events
//!   (`recurringEventId` + `originalStartTime`), which this module folds
//!   back into one `VCALENDAR` per UID — live exceptions become
//!   `RECURRENCE-ID` overrides, cancelled ones become `EXDATE`s on the
//!   master.
//! - **Writes**: `events.insert`/`patch`/`delete` with `sendUpdates` derived
//!   from the calendar's `send_via` (`smtp` → `none`, because jadav emails
//!   the iTIP messages itself; `provider` → `all`). An RSVP is a `PATCH` of
//!   the full `attendees` array with our own `responseStatus` changed —
//!   Google replaces the array wholesale, so it has to be complete.
//! - **Mapping** (both directions) covers start/end with time zones and
//!   all-day dates, summary/description/location, status, transparency,
//!   visibility, sequence, organizer, attendees (CN, ROLE, PARTSTAT,
//!   CUTYPE=RESOURCE), the raw `recurrence[]` lines, `reminders.overrides`
//!   ↔ `VALARM`, conference links (read-only), `iCalUID` ↔ `UID`. Google
//!   event ids are stamped as `X-GOOGLE-EVENT-ID` for debugging.

use crate::calendar::{self, EventTime, VEvent};
use crate::goauth;
use crate::httpc::{self, HttpResponse, HttpUrl};
use crate::jadav::mirror::{
    Capabilities, Changes, Precondition, RemoteCalendar, RemoteError, RemoteId, RemoteObject,
    RemoteVersion, Semantic, VCalendarDoc, assemble_vcalendar,
};
use crate::jadav::store::{OauthTokenRow, Store};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const API: &str = "https://www.googleapis.com/calendar/v3";
pub use crate::goauth::{AUTH_URL, SCOPE_CALENDAR as SCOPE, TOKEN_URL};
pub const PRODID: &str = "-//jadav//google mirror//EN";
const TIMEOUT: Duration = Duration::from_secs(60);
const PAGE_SIZE: u32 = 250;

// ---------------------------------------------------------------------
// OAuth
// ---------------------------------------------------------------------

pub struct GoogleAuth {
    remote: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    access_token: Option<String>,
    expires_at: i64,
    store_path: Option<PathBuf>,
}

impl GoogleAuth {
    /// Load the refresh token (and any cached access token) for `remote`.
    pub fn from_store(
        store: &Store,
        store_path: &Path,
        remote: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<Self> {
        let row = store.get_oauth_token(remote)?.with_context(|| {
            format!(
                "remote {:?} has no OAuth token; run `jadav google auth {}` or `import-token`",
                remote, remote
            )
        })?;
        Ok(Self {
            remote: remote.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            refresh_token: row.refresh_token,
            access_token: row.access_token,
            expires_at: row.expires_at.unwrap_or(0),
            store_path: Some(store_path.to_path_buf()),
        })
    }

    pub fn new_unsaved(
        remote: &str,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Self {
        Self {
            remote: remote.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            refresh_token: refresh_token.to_string(),
            access_token: None,
            expires_at: 0,
            store_path: None,
        }
    }

    pub fn access_token(&mut self) -> Result<String, RemoteError> {
        let now = Utc::now().timestamp();
        if let Some(t) = &self.access_token
            && self.expires_at - 60 > now
        {
            return Ok(t.clone());
        }
        self.refresh()?;
        self.access_token
            .clone()
            .ok_or_else(|| RemoteError::Other(anyhow!("token endpoint returned no access token")))
    }

    pub fn invalidate(&mut self) {
        self.access_token = None;
        self.expires_at = 0;
    }

    fn persist(&self, needs_reauth: bool) {
        if let Some(path) = &self.store_path
            && let Ok(store) = Store::open(path)
        {
            let _ = store.put_oauth_token(
                &self.remote,
                &OauthTokenRow {
                    refresh_token: self.refresh_token.clone(),
                    access_token: self.access_token.clone(),
                    expires_at: Some(self.expires_at),
                    needs_reauth,
                },
            );
            if needs_reauth {
                let _ = store.set_remote_status(&self.remote, true, Some("refresh token revoked"));
            }
        }
    }

    fn refresh(&mut self) -> Result<(), RemoteError> {
        let grant = goauth::refresh_access_token(
            TOKEN_URL,
            &self.client_id,
            &self.client_secret,
            &self.refresh_token,
        )
        .map_err(|e| match e {
            // A revoked/expired refresh token is recorded, not retried:
            // the mirror thread stops on `NeedsReauth` until a human runs
            // `jadav google auth` again.
            goauth::TokenError::InvalidGrant => {
                self.invalidate();
                self.persist(true);
                RemoteError::NeedsReauth
            }
            goauth::TokenError::Transport(e) => RemoteError::Transient(e),
            goauth::TokenError::Other(e) => RemoteError::Other(e),
        })?;
        self.access_token = grant.access_token;
        self.expires_at = Utc::now().timestamp() + grant.expires_in.unwrap_or(3600);
        self.persist(false);
        Ok(())
    }
}

impl crate::caldav::TokenSource for GoogleAuth {
    fn token(&mut self) -> Result<String> {
        self.access_token().map_err(|e| anyhow!("{}", e))
    }
    fn invalidate(&mut self) {
        GoogleAuth::invalidate(self)
    }
}

// ---------------------------------------------------------------------
// REST client with retry
// ---------------------------------------------------------------------

pub struct GoogleClient {
    auth: Arc<Mutex<GoogleAuth>>,
    api_base: String,
    sleeper: Box<dyn FnMut(Duration) + Send>,
}

#[derive(Deserialize, Default)]
struct GErrorBody {
    #[serde(default)]
    error: GErrorInner,
}

#[derive(Deserialize, Default)]
struct GErrorInner {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<GErrorItem>,
}

#[derive(Deserialize, Default)]
struct GErrorItem {
    #[serde(default)]
    reason: String,
}

impl GoogleClient {
    pub fn new(auth: Arc<Mutex<GoogleAuth>>) -> Self {
        Self {
            auth,
            api_base: API.to_string(),
            sleeper: Box::new(std::thread::sleep),
        }
    }

    /// Point at a different API base (tests use a local mock server).
    pub fn with_api_base(mut self, base: &str) -> Self {
        self.api_base = base.trim_end_matches('/').to_string();
        self
    }

    pub fn with_sleeper(mut self, sleeper: Box<dyn FnMut(Duration) + Send>) -> Self {
        self.sleeper = sleeper;
        self
    }

    fn token(&self) -> Result<String, RemoteError> {
        let mut auth = self
            .auth
            .lock()
            .map_err(|_| RemoteError::Other(anyhow!("auth mutex poisoned")))?;
        auth.access_token()
    }

    fn invalidate(&self) {
        if let Ok(mut a) = self.auth.lock() {
            a.invalidate();
        }
    }

    /// One API call with the retry policy: 401 → refresh once; 429/5xx and
    /// rate-limit 403s → back off (`Retry-After` or 2/4/8 s); 410 →
    /// `FullResyncRequired`; 412 → `Conflict`; 404 → `NotFound`;
    /// other 403 → `Forbidden`; 400 → `Other` with Google's message.
    pub fn call(
        &mut self,
        method: &str,
        path_and_query: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<HttpResponse, RemoteError> {
        let url = HttpUrl::parse(&format!("{}{}", self.api_base, path_and_query))
            .map_err(RemoteError::Other)?;
        let body_bytes = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
        let mut retried_auth = false;
        let mut attempt = 0u32;
        loop {
            let token = self.token()?;
            let mut headers = vec![
                ("Authorization".to_string(), format!("Bearer {}", token)),
                ("Accept".to_string(), "application/json".to_string()),
            ];
            if body_bytes.is_some() {
                headers.push((
                    "Content-Type".to_string(),
                    "application/json; charset=utf-8".to_string(),
                ));
            }
            let resp = match httpc::send(method, &url, &headers, body_bytes.as_deref(), TIMEOUT) {
                Ok(r) => r,
                Err(e) => {
                    attempt += 1;
                    if attempt >= 4 {
                        return Err(RemoteError::Transient(e));
                    }
                    (self.sleeper)(Duration::from_secs(2u64.pow(attempt)));
                    continue;
                }
            };
            match resp.status {
                200..=299 => return Ok(resp),
                401 if !retried_auth => {
                    retried_auth = true;
                    self.invalidate();
                    continue;
                }
                401 => return Err(RemoteError::NeedsReauth),
                404 => return Err(RemoteError::NotFound),
                410 => return Err(RemoteError::FullResyncRequired),
                409 | 412 => return Err(RemoteError::Conflict(resp.body_str())),
                403 | 429 | 500 | 502 | 503 => {
                    let err: GErrorBody = resp.json().unwrap_or_default();
                    let reasons: Vec<&str> =
                        err.error.errors.iter().map(|e| e.reason.as_str()).collect();
                    let rate_limited = resp.status == 429
                        || reasons.iter().any(|r| {
                            matches!(
                                *r,
                                "rateLimitExceeded" | "userRateLimitExceeded" | "quotaExceeded"
                            )
                        });
                    if resp.status == 403 && !rate_limited {
                        return Err(RemoteError::Forbidden(if err.error.message.is_empty() {
                            resp.body_str()
                        } else {
                            err.error.message
                        }));
                    }
                    attempt += 1;
                    if attempt >= 4 {
                        return Err(RemoteError::Transient(anyhow!(
                            "HTTP {} after retries: {}",
                            resp.status,
                            err.error.message
                        )));
                    }
                    let wait = resp
                        .header("retry-after")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| Duration::from_secs(2u64.pow(attempt)));
                    (self.sleeper)(wait);
                }
                other => {
                    let err: GErrorBody = resp.json().unwrap_or_default();
                    return Err(RemoteError::Other(anyhow!(
                        "Google API HTTP {}: {}",
                        other,
                        if err.error.message.is_empty() {
                            resp.body_str().chars().take(300).collect()
                        } else {
                            err.error.message
                        }
                    )));
                }
            }
        }
    }

    pub fn call_json<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        path_and_query: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, RemoteError> {
        let resp = self.call(method, path_and_query, body)?;
        resp.json().map_err(RemoteError::Other)
    }
}

// ---------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GTime {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GPerson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(rename = "self", skip_serializing_if = "Option::is_none")]
    pub self_: Option<bool>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GAttendee {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organizer: Option<bool>,
    #[serde(rename = "self", skip_serializing_if = "Option::is_none")]
    pub self_: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GReminderOverride {
    pub method: String,
    pub minutes: i64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GReminders {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_default: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overrides: Option<Vec<GReminderOverride>>,
}

/// Give an event that relies on the calendar's default reminders
/// (`reminders.useDefault`, Google's normal case) those reminders as
/// explicit overrides, so the mirrored `VEVENT` carries the `VALARM`s the
/// Google UI shows. An event with its own overrides is left alone.
pub fn materialize_default_reminders(g: &mut GEvent, defaults: &[GReminderOverride]) {
    let uses_default = g
        .reminders
        .as_ref()
        .and_then(|r| r.use_default)
        .unwrap_or(true);
    if uses_default {
        g.reminders = Some(GReminders {
            use_default: Some(true),
            overrides: Some(defaults.to_vec()),
        });
    }
}

/// Whether two reminder lists mean the same thing (order-insensitive).
fn same_reminders(a: &[GReminderOverride], b: &[GReminderOverride]) -> bool {
    let key = |list: &[GReminderOverride]| {
        let mut v: Vec<(String, i64)> = list
            .iter()
            .map(|r| (r.method.to_ascii_lowercase(), r.minutes))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    key(a) == key(b)
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<GTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<GTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time_unspecified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recurring_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_start_time: Option<GTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transparency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(rename = "iCalUID", skip_serializing_if = "Option::is_none")]
    pub ical_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organizer: Option<GPerson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attendees: Option<Vec<GAttendee>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reminders: Option<GReminders>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hangout_link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conference_data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct GListResponse {
    pub items: Vec<GEvent>,
    pub next_page_token: Option<String>,
    pub next_sync_token: Option<String>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct GCalendarListEntry {
    pub id: String,
    pub summary: Option<String>,
    /// The user's own rename of a shared calendar; wins over `summary`.
    pub summary_override: Option<String>,
    pub description: Option<String>,
    pub primary: Option<bool>,
    pub access_role: Option<String>,
    pub background_color: Option<String>,
    pub time_zone: Option<String>,
    pub deleted: Option<bool>,
    pub hidden: Option<bool>,
    /// The calendar-wide reminders every event with
    /// `reminders.useDefault` gets — Google never puts them on the event.
    pub default_reminders: Vec<GReminderOverride>,
}

impl GCalendarListEntry {
    /// The name to show: the user's override, else the owner's summary,
    /// else the id.
    pub fn name(&self) -> &str {
        self.summary_override
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(self.summary.as_deref().filter(|s| !s.trim().is_empty()))
            .unwrap_or(&self.id)
    }

    /// Whether Google lets this account change events here.
    pub fn writable(&self) -> bool {
        matches!(self.access_role.as_deref(), Some("owner") | Some("writer"))
    }
}

/// The special "Birthdays" calendar Google derives from contacts.
pub const CONTACTS_CALENDAR_SUFFIX: &str = "#contacts@group.v.calendar.google.com";

#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct GCalendarList {
    pub items: Vec<GCalendarListEntry>,
}

// ---------------------------------------------------------------------
// Mapping: Google JSON -> iCalendar
// ---------------------------------------------------------------------

pub fn google_to_partstat(rs: &str) -> &'static str {
    match rs {
        "accepted" => "ACCEPTED",
        "declined" => "DECLINED",
        "tentative" => "TENTATIVE",
        _ => "NEEDS-ACTION",
    }
}

pub fn partstat_to_google(ps: &str) -> &'static str {
    match ps.to_ascii_uppercase().as_str() {
        "ACCEPTED" => "accepted",
        "DECLINED" => "declined",
        "TENTATIVE" => "tentative",
        _ => "needsAction",
    }
}

/// `sendUpdates` for a calendar's `send_via`.
pub fn send_updates_for(send_via: crate::jadav::config::SendVia) -> &'static str {
    match send_via {
        crate::jadav::config::SendVia::Smtp => "none",
        crate::jadav::config::SendVia::Provider => "all",
    }
}

pub fn gtime_to_event_time(t: &GTime) -> Result<EventTime> {
    if let Some(date) = &t.date {
        let d = chrono::NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
            .with_context(|| format!("bad Google date {:?}", date))?;
        return Ok(EventTime::all_day(d));
    }
    let dt_str = t
        .date_time
        .as_deref()
        .context("Google time without date or dateTime")?;
    let dt = DateTime::parse_from_rfc3339(dt_str.trim())
        .with_context(|| format!("bad Google dateTime {:?}", dt_str))?
        .with_timezone(&Utc);
    let tzid = t
        .time_zone
        .as_deref()
        .filter(|z| z.parse::<chrono_tz::Tz>().is_ok())
        .map(str::to_string);
    Ok(EventTime {
        utc: dt,
        tzid,
        all_day: false,
    })
}

pub fn event_time_to_gtime(t: &EventTime) -> GTime {
    if t.all_day {
        return GTime {
            date: Some(t.utc.format("%Y-%m-%d").to_string()),
            date_time: None,
            time_zone: None,
        };
    }
    if let Some(tzid) = &t.tzid
        && let Ok(tz) = tzid.parse::<chrono_tz::Tz>()
    {
        let local = tz.from_utc_datetime(&t.utc.naive_utc());
        return GTime {
            date: None,
            date_time: Some(local.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)),
            time_zone: Some(tzid.clone()),
        };
    }
    GTime {
        date: None,
        date_time: Some(t.utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        time_zone: Some("UTC".to_string()),
    }
}

fn ical_stamp(rfc3339: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(rfc3339.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ").to_string())
}

/// Render `name` in the same form as `master_start` (`VALUE=DATE`, `TZID=`
/// or UTC) — what `RECURRENCE-ID`/`EXDATE` must use to match the master.
fn render_in_master_form(name: &str, t: &EventTime, master_start: &EventTime) -> String {
    let shaped = EventTime {
        utc: t.utc,
        tzid: if master_start.all_day {
            None
        } else {
            master_start.tzid.clone().or(t.tzid.clone())
        },
        all_day: master_start.all_day,
    };
    calendar::render_time_property(name, &shaped)
}

/// Build one `VEVENT` block from a Google event. For overrides pass the
/// master's `DTSTART` so `RECURRENCE-ID` takes its form.
pub fn gevent_to_vevent_block(
    g: &GEvent,
    uid: &str,
    master_start: Option<&EventTime>,
    extra_exdates: &[EventTime],
) -> Result<String> {
    let mut lines: Vec<String> = vec!["BEGIN:VEVENT".to_string(), format!("UID:{}", uid)];
    let updated = g.updated.as_deref().and_then(ical_stamp);
    lines.push(format!(
        "DTSTAMP:{}",
        updated
            .clone()
            .unwrap_or_else(|| Utc::now().format("%Y%m%dT%H%M%SZ").to_string())
    ));
    if let Some(c) = g.created.as_deref().and_then(ical_stamp) {
        lines.push(format!("CREATED:{}", c));
    }
    if let Some(u) = updated {
        lines.push(format!("LAST-MODIFIED:{}", u));
    }
    let start = g
        .start
        .as_ref()
        .map(gtime_to_event_time)
        .transpose()?
        .context("event without start")?;
    let end = match (&g.end, g.end_time_unspecified) {
        (Some(e), _) => gtime_to_event_time(e)?,
        (None, _) => start.clone(),
    };
    if let (Some(ost), Some(ms)) = (&g.original_start_time, master_start) {
        let rid = gtime_to_event_time(ost)?;
        lines.push(render_in_master_form("RECURRENCE-ID", &rid, ms));
    }
    lines.push(calendar::render_time_property("DTSTART", &start));
    lines.push(calendar::render_time_property("DTEND", &end));
    if master_start.is_none()
        && let Some(rec) = &g.recurrence
    {
        for line in rec {
            let l = line.trim();
            if !l.is_empty() {
                lines.push(l.to_string());
            }
        }
        for ex in extra_exdates {
            lines.push(render_in_master_form("EXDATE", ex, &start));
        }
    }
    let summary = g
        .summary
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(no title)".to_string());
    lines.push(format!("SUMMARY:{}", calendar::escape_text(&summary)));
    if let Some(d) = &g.description
        && !d.is_empty()
    {
        lines.push(format!("DESCRIPTION:{}", calendar::escape_text(d)));
    }
    let conference = g.hangout_link.clone().or_else(|| {
        g.conference_data.as_ref().and_then(|cd| {
            cd.get("entryPoints")?
                .as_array()?
                .iter()
                .find(|ep| ep.get("entryPointType").and_then(|t| t.as_str()) == Some("video"))?
                .get("uri")?
                .as_str()
                .map(str::to_string)
        })
    });
    match (&g.location, &conference) {
        (Some(l), _) if !l.trim().is_empty() => {
            lines.push(format!("LOCATION:{}", calendar::escape_text(l)))
        }
        (_, Some(c)) => lines.push(format!("LOCATION:{}", calendar::escape_text(c))),
        _ => {}
    }
    if let Some(c) = &conference {
        lines.push(format!("X-GOOGLE-CONFERENCE:{}", c));
        lines.push(format!("URL:{}", c));
    }
    let status = match g.status.as_deref() {
        Some("tentative") => "TENTATIVE",
        Some("cancelled") => "CANCELLED",
        _ => "CONFIRMED",
    };
    lines.push(format!("STATUS:{}", status));
    if g.transparency.as_deref() == Some("transparent") {
        lines.push("TRANSP:TRANSPARENT".to_string());
    }
    match g.visibility.as_deref() {
        Some("public") => lines.push("CLASS:PUBLIC".to_string()),
        Some("private") => lines.push("CLASS:PRIVATE".to_string()),
        Some("confidential") => lines.push("CLASS:CONFIDENTIAL".to_string()),
        _ => {}
    }
    lines.push(format!("SEQUENCE:{}", g.sequence.unwrap_or(0)));
    if let Some(org) = &g.organizer
        && let Some(email) = &org.email
    {
        let cn = org
            .display_name
            .as_ref()
            .map(|n| format!(";CN={}", quote_param(n)))
            .unwrap_or_default();
        lines.push(format!("ORGANIZER{}:mailto:{}", cn, email));
    }
    for a in g.attendees.iter().flatten() {
        let Some(email) = &a.email else { continue };
        let mut params = String::new();
        if let Some(n) = &a.display_name {
            params.push_str(&format!(";CN={}", quote_param(n)));
        }
        if a.resource == Some(true) {
            params.push_str(";CUTYPE=RESOURCE");
        }
        params.push_str(if a.optional == Some(true) {
            ";ROLE=OPT-PARTICIPANT"
        } else {
            ";ROLE=REQ-PARTICIPANT"
        });
        params.push_str(&format!(
            ";PARTSTAT={}",
            google_to_partstat(a.response_status.as_deref().unwrap_or("needsAction"))
        ));
        if a.response_status.as_deref().unwrap_or("needsAction") == "needsAction" {
            params.push_str(";RSVP=TRUE");
        }
        lines.push(format!("ATTENDEE{}:mailto:{}", params, email));
    }
    if let Some(id) = &g.id {
        lines.push(format!("X-GOOGLE-EVENT-ID:{}", id));
    }
    if let Some(overrides) = g.reminders.as_ref().and_then(|r| r.overrides.as_ref()) {
        for o in overrides {
            let action = if o.method == "email" {
                "EMAIL"
            } else {
                "DISPLAY"
            };
            lines.push("BEGIN:VALARM".to_string());
            lines.push(format!("ACTION:{}", action));
            lines.push(format!("TRIGGER:-PT{}M", o.minutes.max(0)));
            lines.push("DESCRIPTION:Reminder".to_string());
            lines.push("END:VALARM".to_string());
        }
    }
    lines.push("END:VEVENT".to_string());
    Ok(lines.join("\r\n"))
}

fn quote_param(v: &str) -> String {
    let cleaned: String = v
        .chars()
        .filter(|c| *c != '"' && *c != '\r' && *c != '\n')
        .collect();
    if cleaned.contains([';', ':', ',']) {
        format!("\"{}\"", cleaned)
    } else {
        cleaned
    }
}

/// The composite version stamp: master etag plus each exception's id/etag.
pub fn composite_version(master: &GEvent, exceptions: &[GEvent]) -> String {
    let mut parts: Vec<String> = exceptions
        .iter()
        .map(|e| {
            format!(
                "{}={}",
                e.id.clone().unwrap_or_default(),
                e.etag.clone().unwrap_or_default()
            )
        })
        .collect();
    parts.sort();
    format!(
        "{}|{}",
        master.etag.clone().unwrap_or_default(),
        parts.join(",")
    )
}

/// Fold a master and its exception events into one `RemoteObject`.
pub fn gevent_group_to_object(master: &GEvent, exceptions: &[GEvent]) -> Result<RemoteObject> {
    let id = master.id.clone().context("Google event without id")?;
    let uid = master
        .ical_uid
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("{}@google.com", id));
    let master_start = master
        .start
        .as_ref()
        .map(gtime_to_event_time)
        .transpose()?
        .context("event without start")?;
    let mut exdates = Vec::new();
    let mut overrides = Vec::new();
    for ex in exceptions {
        let Some(ost) = &ex.original_start_time else {
            continue;
        };
        if ex.status.as_deref() == Some("cancelled") {
            exdates.push(gtime_to_event_time(ost)?);
        } else {
            overrides.push(gevent_to_vevent_block(ex, &uid, Some(&master_start), &[])?);
        }
    }
    let mut components = vec![gevent_to_vevent_block(master, &uid, None, &exdates)?];
    components.extend(overrides);
    let ics = assemble_vcalendar(PRODID, &components);
    let fingerprint = Semantic::from_ics(&ics)
        .map(|s| s.fingerprint())
        .unwrap_or_else(|_| crate::jadav::mirror::fingerprint_ics(&ics));
    Ok(RemoteObject {
        id: RemoteId(id),
        version: RemoteVersion(composite_version(master, exceptions)),
        uid,
        ics,
        fingerprint,
    })
}

// ---------------------------------------------------------------------
// Mapping: iCalendar -> Google JSON
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyMode {
    Full,
    /// Only fields an attendee may change (none besides the RSVP, which
    /// is patched separately): produces an empty body.
    RsvpOnly,
}

fn vevent_to_gevent(
    ev: &VEvent,
    include_recurrence: bool,
    defaults: &[GReminderOverride],
) -> GEvent {
    let raw = ev.raw.as_deref().unwrap_or("");
    let value_of = |name: &str| -> Option<String> {
        calendar::raw_property_lines(raw, &[name])
            .first()
            .and_then(|l| {
                l.split_once(':')
                    .map(|(_, v)| v.trim().to_ascii_uppercase())
            })
    };
    let mut g = GEvent {
        summary: Some(ev.summary.clone()),
        description: if ev.description.is_empty() {
            None
        } else {
            Some(ev.description.clone())
        },
        location: if ev.location.is_empty() {
            None
        } else {
            Some(ev.location.clone())
        },
        start: Some(event_time_to_gtime(&ev.dtstart)),
        end: Some(event_time_to_gtime(&ev.dtend)),
        status: Some(match ev.status {
            calendar::EventStatus::Confirmed => "confirmed".to_string(),
            calendar::EventStatus::Tentative => "tentative".to_string(),
            calendar::EventStatus::Cancelled => "cancelled".to_string(),
        }),
        sequence: Some(ev.sequence),
        ..Default::default()
    };
    g.transparency = Some(match value_of("TRANSP").as_deref() {
        Some("TRANSPARENT") => "transparent".to_string(),
        _ => "opaque".to_string(),
    });
    g.visibility = match value_of("CLASS").as_deref() {
        Some("PUBLIC") => Some("public".to_string()),
        Some("PRIVATE") => Some("private".to_string()),
        Some("CONFIDENTIAL") => Some("confidential".to_string()),
        _ => Some("default".to_string()),
    };
    if include_recurrence {
        let rec: Vec<String> = calendar::raw_property_lines(raw, &["RRULE", "EXDATE", "RDATE"]);
        g.recurrence = Some(rec);
    }
    if !ev.attendees.is_empty() {
        g.attendees = Some(
            ev.attendees
                .iter()
                .map(|a| GAttendee {
                    email: Some(a.email.clone()),
                    display_name: a.name.clone(),
                    response_status: Some(
                        partstat_to_google(a.partstat.as_deref().unwrap_or("NEEDS-ACTION"))
                            .to_string(),
                    ),
                    optional: if a
                        .role
                        .as_deref()
                        .is_some_and(|r| r.eq_ignore_ascii_case("OPT-PARTICIPANT"))
                    {
                        Some(true)
                    } else {
                        None
                    },
                    ..Default::default()
                })
                .collect(),
        );
    }
    let overrides: Vec<GReminderOverride> = ev
        .alarms
        .iter()
        .filter_map(|al| match &al.trigger {
            calendar::AlarmTrigger::Relative {
                offset,
                related: calendar::AlarmRelated::Start,
            } if offset.num_minutes() <= 0 => Some(GReminderOverride {
                method: if al.action.eq_ignore_ascii_case("EMAIL") {
                    "email".to_string()
                } else {
                    "popup".to_string()
                },
                minutes: (-offset.num_minutes()).min(40320),
            }),
            _ => None,
        })
        .take(5)
        .collect();
    // Alarms that are exactly the calendar's defaults go back as
    // `useDefault` — they came from there when the event was mirrored —
    // so a round trip never turns Google's defaults into per-event copies.
    g.reminders = Some(
        if overrides.is_empty() || same_reminders(&overrides, defaults) {
            GReminders {
                use_default: Some(true),
                overrides: None,
            }
        } else {
            GReminders {
                use_default: Some(false),
                overrides: Some(overrides),
            }
        },
    );
    g
}

/// The JSON body for an insert/patch of the master event.
pub fn doc_to_gevent_body(
    doc: &VCalendarDoc,
    mode: BodyMode,
    defaults: &[GReminderOverride],
) -> serde_json::Value {
    match mode {
        BodyMode::RsvpOnly => serde_json::json!({}),
        BodyMode::Full => {
            let mut g = vevent_to_gevent(&doc.master, true, defaults);
            g.ical_uid = Some(doc.uid.clone());
            serde_json::to_value(g).unwrap_or_else(|_| serde_json::json!({}))
        }
    }
}

// ---------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------

pub struct GoogleRest {
    client: GoogleClient,
    calendar_id: String,
    identity: String,
    send_updates: &'static str,
    skip_event_types: Vec<String>,
    /// The calendar's default reminders, fetched once per process on
    /// first use (`None` until then, or after a failed lookup).
    default_reminders: Option<Vec<GReminderOverride>>,
}

impl GoogleRest {
    pub fn new(
        client: GoogleClient,
        calendar_id: &str,
        identity: &str,
        send_via: crate::jadav::config::SendVia,
    ) -> Self {
        Self {
            client,
            calendar_id: calendar_id.to_string(),
            identity: identity.to_ascii_lowercase(),
            send_updates: send_updates_for(send_via),
            skip_event_types: default_skip_event_types(calendar_id),
            default_reminders: None,
        }
    }

    /// Use these as the calendar's default reminders instead of asking
    /// Google (tests, or a caller that already has the calendar list).
    pub fn with_default_reminders(mut self, defaults: Vec<GReminderOverride>) -> Self {
        self.default_reminders = Some(defaults);
        self
    }

    /// The calendar's default reminders, from `calendarList` on first use.
    /// A failed lookup yields none for this cycle and is retried next time.
    fn default_reminders(&mut self) -> Vec<GReminderOverride> {
        if let Some(d) = &self.default_reminders {
            return d.clone();
        }
        let path = format!(
            "/users/me/calendarList/{}",
            httpc::percent_encode_component(&self.calendar_id)
        );
        match self
            .client
            .call_json::<GCalendarListEntry>("GET", &path, None)
        {
            Ok(entry) => {
                self.default_reminders = Some(entry.default_reminders.clone());
                entry.default_reminders
            }
            Err(_) => Vec::new(),
        }
    }

    fn events_path(&self) -> String {
        format!(
            "/calendars/{}/events",
            httpc::percent_encode_component(&self.calendar_id)
        )
    }

    fn event_path(&self, id: &str) -> String {
        format!(
            "{}/{}",
            self.events_path(),
            httpc::percent_encode_component(id)
        )
    }

    fn list_page(&mut self, query: &[(&str, &str)]) -> Result<GListResponse, RemoteError> {
        let path = format!("{}?{}", self.events_path(), httpc::build_query(query));
        self.client.call_json("GET", &path, None)
    }

    /// Every event (master + exceptions, deleted included) sharing a UID.
    fn group_by_uid(&mut self, uid: &str) -> Result<Vec<GEvent>, RemoteError> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut q: Vec<(&str, &str)> = vec![
                ("iCalUID", uid),
                ("showDeleted", "true"),
                ("maxResults", "250"),
            ];
            if let Some(p) = &page {
                q.push(("pageToken", p));
            }
            let resp = self.list_page(&q)?;
            out.extend(resp.items);
            match resp.next_page_token {
                Some(t) => page = Some(t),
                None => break,
            }
        }
        Ok(out)
    }

    fn build_from_group(&mut self, master: &GEvent) -> Result<RemoteObject, RemoteError> {
        let needs_group = master.recurrence.as_ref().is_some_and(|r| !r.is_empty());
        let exceptions: Vec<GEvent> = if needs_group {
            let uid = master.ical_uid.clone().unwrap_or_default();
            let group = if uid.is_empty() {
                Vec::new()
            } else {
                self.group_by_uid(&uid)?
            };
            group
                .into_iter()
                .filter(|e| e.recurring_event_id.as_deref() == master.id.as_deref())
                .collect()
        } else {
            Vec::new()
        };
        let defaults = self.default_reminders();
        let mut master = master.clone();
        materialize_default_reminders(&mut master, &defaults);
        let mut exceptions = exceptions;
        for ex in &mut exceptions {
            materialize_default_reminders(ex, &defaults);
        }
        gevent_group_to_object(&master, &exceptions).map_err(RemoteError::Other)
    }

    fn fetch_event(&mut self, id: &str) -> Result<Option<GEvent>, RemoteError> {
        match self
            .client
            .call_json::<GEvent>("GET", &self.event_path(id), None)
        {
            Ok(g) => Ok(Some(g)),
            Err(RemoteError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn master_etag(pre: &Precondition) -> Option<String> {
        match pre {
            Precondition::None => None,
            Precondition::IfMatch(v) => {
                Some(v.0.split('|').next().unwrap_or("").to_string()).filter(|s| !s.is_empty())
            }
        }
    }

    fn instance_id_for(
        &mut self,
        master_id: &str,
        rid: &EventTime,
    ) -> Result<Option<String>, RemoteError> {
        let original = rid.utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let path = format!(
            "{}/instances?{}",
            self.event_path(master_id),
            httpc::build_query(&[
                ("originalStart", original.as_str()),
                ("showDeleted", "true")
            ])
        );
        let resp: GListResponse = self.client.call_json("GET", &path, None)?;
        Ok(resp.items.into_iter().next().and_then(|e| e.id))
    }

    fn patch_overrides(&mut self, master_id: &str, doc: &VCalendarDoc) -> Result<(), RemoteError> {
        let defaults = self.default_reminders();
        for (rid, ev) in &doc.overrides {
            let Some(instance_id) = self.instance_id_for(master_id, rid)? else {
                continue;
            };
            let mut body = vevent_to_gevent(ev, false, &defaults);
            body.recurrence = None;
            let value = serde_json::to_value(body).unwrap_or_default();
            let path = format!(
                "{}?{}",
                self.event_path(&instance_id),
                httpc::build_query(&[("sendUpdates", self.send_updates)])
            );
            self.client.call("PATCH", &path, Some(&value))?;
        }
        Ok(())
    }
}

impl RemoteCalendar for GoogleRest {
    fn list_changes(
        &mut self,
        token: Option<&str>,
        window_start: Option<DateTime<Utc>>,
    ) -> Result<Changes, RemoteError> {
        let mut items: Vec<GEvent> = Vec::new();
        let mut page: Option<String> = None;
        let mut next_sync: Option<String> = None;
        let page_size = PAGE_SIZE.to_string();
        let time_min = window_start.map(|w| w.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        loop {
            let mut q: Vec<(&str, &str)> =
                vec![("maxResults", page_size.as_str()), ("showDeleted", "true")];
            match token {
                Some(t) => q.push(("syncToken", t)),
                None => {
                    if let Some(tm) = &time_min {
                        q.push(("timeMin", tm.as_str()));
                    }
                }
            }
            if let Some(p) = &page {
                q.push(("pageToken", p));
            }
            let resp = self.list_page(&q)?;
            items.extend(resp.items);
            next_sync = resp.next_sync_token.or(next_sync);
            match resp.next_page_token {
                Some(t) => page = Some(t),
                None => break,
            }
        }
        let skip = self.skip_event_types.clone();
        let mut masters: HashMap<String, GEvent> = HashMap::new();
        let mut exceptions_seen: HashMap<String, Vec<GEvent>> = HashMap::new();
        let mut deleted: Vec<RemoteId> = Vec::new();
        for g in items {
            if g.event_type
                .as_deref()
                .is_some_and(|t| skip.iter().any(|s| s == t))
            {
                continue;
            }
            match (&g.recurring_event_id, &g.id) {
                (Some(master_id), _) => exceptions_seen
                    .entry(master_id.clone())
                    .or_default()
                    .push(g),
                (None, Some(id)) => {
                    if g.status.as_deref() == Some("cancelled") {
                        deleted.push(RemoteId(id.clone()));
                    } else {
                        masters.insert(id.clone(), g);
                    }
                }
                (None, None) => {}
            }
        }
        // Exceptions whose master did not appear in this delta: fetch it.
        let missing: Vec<String> = exceptions_seen
            .keys()
            .filter(|k| !masters.contains_key(*k))
            .cloned()
            .collect();
        for master_id in missing {
            if deleted.iter().any(|d| d.0 == master_id) {
                continue;
            }
            if let Some(m) = self.fetch_event(&master_id)? {
                if m.status.as_deref() == Some("cancelled") {
                    deleted.push(RemoteId(master_id));
                } else {
                    masters.insert(master_id, m);
                }
            }
        }
        let mut out = Vec::new();
        let mut ids: Vec<String> = masters.keys().cloned().collect();
        ids.sort();
        for id in ids {
            let master = &masters[&id];
            match self.build_from_group(master) {
                Ok(obj) => out.push(obj),
                Err(RemoteError::Other(e)) => {
                    eprintln!("jadav: google: skipping event {}: {:#}", id, e);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Changes {
            items: out,
            deleted,
            next_token: next_sync,
            full_resync: token.is_none(),
        })
    }

    fn fetch(&mut self, id: &RemoteId) -> Result<Option<RemoteObject>, RemoteError> {
        match self.fetch_event(&id.0)? {
            None => Ok(None),
            Some(g) if g.status.as_deref() == Some("cancelled") => Ok(None),
            Some(g) => Ok(Some(self.build_from_group(&g)?)),
        }
    }

    fn create(&mut self, doc: &VCalendarDoc) -> Result<RemoteObject, RemoteError> {
        let defaults = self.default_reminders();
        let body = doc_to_gevent_body(doc, BodyMode::Full, &defaults);
        let path = format!(
            "{}?{}",
            self.events_path(),
            httpc::build_query(&[("sendUpdates", self.send_updates)])
        );
        let created: GEvent = self.client.call_json("POST", &path, Some(&body))?;
        let id = created
            .id
            .clone()
            .ok_or_else(|| RemoteError::Other(anyhow!("insert returned no id")))?;
        if !doc.overrides.is_empty() {
            self.patch_overrides(&id, doc)?;
        }
        self.fetch(&RemoteId(id.clone()))?
            .ok_or_else(|| RemoteError::Other(anyhow!("created event {} vanished", id)))
    }

    fn update(
        &mut self,
        id: &RemoteId,
        doc: &VCalendarDoc,
        pre: Precondition,
    ) -> Result<RemoteObject, RemoteError> {
        let defaults = self.default_reminders();
        let body = doc_to_gevent_body(doc, BodyMode::Full, &defaults);
        let path = format!(
            "{}?{}",
            self.event_path(&id.0),
            httpc::build_query(&[("sendUpdates", self.send_updates)])
        );
        // Google honours If-Match on PATCH via the etag; without one we
        // simply overwrite (the engine only calls without a version for
        // freshly adopted objects).
        if let Some(etag) = Self::master_etag(&pre) {
            let current = self.fetch_event(&id.0)?;
            if let Some(cur) = current
                && cur.etag.as_deref() != Some(etag.as_str())
            {
                return Err(RemoteError::Conflict("etag changed".to_string()));
            }
        }
        self.client.call("PATCH", &path, Some(&body))?;
        if !doc.overrides.is_empty() {
            self.patch_overrides(&id.0, doc)?;
        }
        self.fetch(id)?.ok_or(RemoteError::NotFound)
    }

    fn set_own_partstat(
        &mut self,
        id: &RemoteId,
        identity: &str,
        partstat: &str,
        recurrence_id: Option<&EventTime>,
        pre: Precondition,
    ) -> Result<RemoteObject, RemoteError> {
        let target_id = match recurrence_id {
            None => id.0.clone(),
            Some(rid) => self
                .instance_id_for(&id.0, rid)?
                .ok_or_else(|| RemoteError::Other(anyhow!("no instance at {}", rid.utc)))?,
        };
        let Some(current) = self.fetch_event(&target_id)? else {
            return Err(RemoteError::NotFound);
        };
        if recurrence_id.is_none()
            && let Some(etag) = Self::master_etag(&pre)
            && current.etag.as_deref() != Some(etag.as_str())
        {
            return Err(RemoteError::Conflict("etag changed".to_string()));
        }
        let me = identity.to_ascii_lowercase();
        let mut attendees = current.attendees.clone().unwrap_or_default();
        let mut found = false;
        for a in attendees.iter_mut() {
            let email = a.email.as_deref().map(|e| e.to_ascii_lowercase());
            let is_me = a.self_ == Some(true)
                || email.as_deref() == Some(me.as_str())
                || email.as_deref() == Some(self.identity.as_str());
            if is_me {
                a.response_status = Some(partstat_to_google(partstat).to_string());
                found = true;
            }
        }
        if !found {
            return Err(RemoteError::Forbidden(format!(
                "{} is not an attendee",
                identity
            )));
        }
        let body = serde_json::json!({ "attendees": attendees });
        let path = format!(
            "{}?{}",
            self.event_path(&target_id),
            httpc::build_query(&[("sendUpdates", self.send_updates)])
        );
        self.client.call("PATCH", &path, Some(&body))?;
        self.fetch(id)?.ok_or(RemoteError::NotFound)
    }

    fn delete(&mut self, id: &RemoteId, pre: Precondition) -> Result<(), RemoteError> {
        if let Some(etag) = Self::master_etag(&pre)
            && let Some(cur) = self.fetch_event(&id.0)?
            && cur.etag.as_deref() != Some(etag.as_str())
        {
            return Err(RemoteError::Conflict("etag changed".to_string()));
        }
        let path = format!(
            "{}?{}",
            self.event_path(&id.0),
            httpc::build_query(&[("sendUpdates", self.send_updates)])
        );
        match self.client.call("DELETE", &path, None) {
            Ok(_) | Err(RemoteError::NotFound) | Err(RemoteError::FullResyncRequired) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn find_by_uid(&mut self, uid: &str) -> Result<Option<RemoteObject>, RemoteError> {
        let group = self.group_by_uid(uid)?;
        let master = group
            .iter()
            .find(|g| g.recurring_event_id.is_none() && g.status.as_deref() != Some("cancelled"))
            .cloned();
        match master {
            Some(m) => Ok(Some(self.build_from_group(&m)?)),
            None => Ok(None),
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            rsvp_only_writes: true,
            server_schedules: self.send_updates != "none",
        }
    }
}

// ---------------------------------------------------------------------
// CLI helpers
// ---------------------------------------------------------------------

/// Import a refresh token from the previous bridge's `/state/<name>.json`
/// (top-level `refresh_token`), then prove it works with one refresh.
pub fn import_token(
    store: &Store,
    store_path: &Path,
    remote: &str,
    client_id: &str,
    client_secret: &str,
    json_path: &Path,
) -> Result<()> {
    let text = std::fs::read_to_string(json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).context("parsing state JSON")?;
    let token = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .or_else(|| {
            value.as_object().and_then(|o| {
                o.values()
                    .find_map(|v| v.get("refresh_token").and_then(|t| t.as_str()))
            })
        })
        .context("no `refresh_token` key in that file")?;
    store.put_oauth_token(
        remote,
        &OauthTokenRow {
            refresh_token: token.to_string(),
            access_token: None,
            expires_at: None,
            needs_reauth: false,
        },
    )?;
    let mut auth = GoogleAuth::from_store(store, store_path, remote, client_id, client_secret)?;
    auth.access_token()
        .map_err(|e| anyhow!("token refresh failed: {}", e))?;
    println!(
        "imported refresh token for remote {} (refresh succeeded)",
        remote
    );
    Ok(())
}

/// Interactive: run [`goauth::authorize_interactive`]'s browser round trip
/// and store the refresh token it yields against `remote`.
pub fn auth_interactive(
    store: &Store,
    remote: &str,
    client_id: &str,
    client_secret: &str,
    login_hint: &str,
) -> Result<()> {
    let grant = goauth::authorize_interactive(client_id, client_secret, login_hint)?;
    let refresh = grant
        .refresh_token
        .context("Google returned no refresh token")?;
    store.put_oauth_token(
        remote,
        &OauthTokenRow {
            refresh_token: refresh,
            access_token: grant.access_token,
            expires_at: grant.expires_in.map(|s| Utc::now().timestamp() + s),
            needs_reauth: false,
        },
    )?;
    println!(
        "authorised remote {} (scope: {})",
        remote,
        grant.scope.unwrap_or_else(|| SCOPE.to_string())
    );
    Ok(())
}

/// Event types a mirror leaves out: Google's own status-like events
/// (working location, out of office, focus time) everywhere, and birthday
/// events too — except on the contacts "Birthdays" calendar, where they
/// are the whole content.
pub fn default_skip_event_types(calendar_id: &str) -> Vec<String> {
    let mut v = vec![
        "workingLocation".to_string(),
        "outOfOffice".to_string(),
        "focusTime".to_string(),
    ];
    if !calendar_id.ends_with(CONTACTS_CALENDAR_SUFFIX) {
        v.push("birthday".to_string());
    }
    v
}

pub fn list_calendars(client: &mut GoogleClient) -> Result<Vec<GCalendarListEntry>> {
    let list: GCalendarList = client
        .call_json("GET", "/users/me/calendarList?maxResults=250", None)
        .map_err(|e| anyhow!("{}", e))?;
    Ok(list.items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(json: &str) -> GEvent {
        serde_json::from_str(json).unwrap()
    }

    const MASTER: &str = r#"{
      "id": "abc123", "etag": "\"e1\"", "status": "confirmed", "summary": "Weekly sync",
      "description": "Agenda & notes", "location": "Room 1",
      "start": {"dateTime": "2024-01-15T09:00:00+01:00", "timeZone": "Europe/Budapest"},
      "end": {"dateTime": "2024-01-15T10:00:00+01:00", "timeZone": "Europe/Budapest"},
      "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO"],
      "iCalUID": "abc123@google.com", "sequence": 2, "transparency": "opaque", "visibility": "private",
      "organizer": {"email": "boss@example.com", "displayName": "Boss"},
      "attendees": [
        {"email": "boss@example.com", "displayName": "Boss", "responseStatus": "accepted", "organizer": true},
        {"email": "me@example.com", "displayName": "Me", "responseStatus": "needsAction", "self": true},
        {"email": "room@resource.calendar.google.com", "resource": true, "responseStatus": "accepted"},
        {"email": "opt@example.com", "optional": true, "responseStatus": "tentative"}
      ],
      "reminders": {"useDefault": false, "overrides": [{"method": "popup", "minutes": 15}, {"method": "email", "minutes": 60}]},
      "hangoutLink": "https://meet.google.com/abc-defg-hij",
      "created": "2024-01-01T00:00:00.000Z", "updated": "2024-01-10T12:00:00.000Z"
    }"#;

    const LIVE_EXCEPTION: &str = r#"{
      "id": "abc123_20240122T080000Z", "etag": "\"e2\"", "status": "confirmed", "summary": "Weekly sync (moved)",
      "recurringEventId": "abc123",
      "originalStartTime": {"dateTime": "2024-01-22T09:00:00+01:00", "timeZone": "Europe/Budapest"},
      "start": {"dateTime": "2024-01-22T11:00:00+01:00", "timeZone": "Europe/Budapest"},
      "end": {"dateTime": "2024-01-22T12:00:00+01:00", "timeZone": "Europe/Budapest"},
      "iCalUID": "abc123@google.com", "sequence": 3
    }"#;

    const CANCELLED_EXCEPTION: &str = r#"{
      "id": "abc123_20240129T080000Z", "etag": "\"e3\"", "status": "cancelled",
      "recurringEventId": "abc123",
      "originalStartTime": {"dateTime": "2024-01-29T09:00:00+01:00", "timeZone": "Europe/Budapest"}
    }"#;

    #[test]
    fn gtime_round_trips_timed_all_day_and_utc() {
        let t = gtime_to_event_time(&GTime {
            date: None,
            date_time: Some("2024-01-15T09:00:00+01:00".into()),
            time_zone: Some("Europe/Budapest".into()),
        })
        .unwrap();
        assert_eq!(t.utc, Utc.with_ymd_and_hms(2024, 1, 15, 8, 0, 0).unwrap());
        assert_eq!(t.tzid.as_deref(), Some("Europe/Budapest"));
        let back = event_time_to_gtime(&t);
        assert_eq!(back.date_time.as_deref(), Some("2024-01-15T09:00:00+01:00"));
        assert_eq!(back.time_zone.as_deref(), Some("Europe/Budapest"));
        let d = gtime_to_event_time(&GTime {
            date: Some("2024-01-15".into()),
            date_time: None,
            time_zone: None,
        })
        .unwrap();
        assert!(d.all_day);
        assert_eq!(event_time_to_gtime(&d).date.as_deref(), Some("2024-01-15"));
        let unknown = gtime_to_event_time(&GTime {
            date: None,
            date_time: Some("2024-01-15T08:00:00Z".into()),
            time_zone: Some("GMT+02:00".into()),
        })
        .unwrap();
        assert!(unknown.tzid.is_none());
        assert_eq!(
            event_time_to_gtime(&unknown).time_zone.as_deref(),
            Some("UTC")
        );
    }

    #[test]
    fn group_assembles_master_override_and_exdate() {
        let master = g(MASTER);
        let exceptions = vec![g(LIVE_EXCEPTION), g(CANCELLED_EXCEPTION)];
        let obj = gevent_group_to_object(&master, &exceptions).unwrap();
        assert_eq!(obj.id.0, "abc123");
        assert_eq!(obj.uid, "abc123@google.com");
        assert_eq!(
            obj.version.0,
            "\"e1\"|abc123_20240122T080000Z=\"e2\",abc123_20240129T080000Z=\"e3\""
        );
        // The stored document is folded at 75 octets; assert on the unfolded
        // content lines.
        let unfolded = calendar::unfold(&obj.ics).join("\r\n");
        let ics = &unfolded;
        assert_eq!(ics.matches("BEGIN:VEVENT").count(), 2, "{ics}");
        assert!(ics.contains("BEGIN:VTIMEZONE"));
        assert!(ics.contains("DTSTART;TZID=Europe/Budapest:20240115T090000"));
        assert!(ics.contains("RRULE:FREQ=WEEKLY;BYDAY=MO"));
        assert!(ics.contains("EXDATE;TZID=Europe/Budapest:20240129T090000"));
        assert!(ics.contains("RECURRENCE-ID;TZID=Europe/Budapest:20240122T090000"));
        assert!(ics.contains("SUMMARY:Weekly sync (moved)"));
        assert!(ics.contains("DESCRIPTION:Agenda & notes"));
        assert!(ics.contains("ORGANIZER;CN=Boss:mailto:boss@example.com"));
        assert!(ics.contains("ATTENDEE;CN=Me;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@example.com"));
        assert!(ics.contains("CUTYPE=RESOURCE"));
        assert!(ics.contains("ROLE=OPT-PARTICIPANT;PARTSTAT=TENTATIVE:mailto:opt@example.com"));
        assert!(ics.contains("CLASS:PRIVATE"));
        assert!(ics.contains("SEQUENCE:2"));
        assert!(ics.contains("X-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij"));
        assert!(ics.contains("TRIGGER:-PT15M"));
        assert!(ics.contains("ACTION:EMAIL"));
        assert!(ics.contains("X-GOOGLE-EVENT-ID:abc123"));
        // It parses back as one master with one override.
        let doc = VCalendarDoc::parse(&obj.ics).unwrap();
        assert_eq!(doc.overrides.len(), 1);
        assert_eq!(doc.master.exdates.len(), 1);
        assert_eq!(doc.master.attendees.len(), 4);
        for line in obj.ics.split("\r\n") {
            assert!(line.len() <= 75, "{line}");
        }
    }

    #[test]
    fn ics_to_google_body_round_trips_semantics() {
        let master = g(MASTER);
        let obj = gevent_group_to_object(&master, &[]).unwrap();
        let doc = VCalendarDoc::parse(&obj.ics).unwrap();
        let body = doc_to_gevent_body(&doc, BodyMode::Full, &[]);
        assert_eq!(body["summary"], "Weekly sync");
        assert_eq!(body["description"], "Agenda & notes");
        assert_eq!(body["start"]["dateTime"], "2024-01-15T09:00:00+01:00");
        assert_eq!(body["start"]["timeZone"], "Europe/Budapest");
        assert_eq!(body["recurrence"][0], "RRULE:FREQ=WEEKLY;BYDAY=MO");
        assert_eq!(body["visibility"], "private");
        assert_eq!(body["transparency"], "opaque");
        assert_eq!(body["sequence"], 2);
        assert_eq!(body["iCalUID"], "abc123@google.com");
        let attendees = body["attendees"].as_array().unwrap();
        assert_eq!(attendees.len(), 4);
        let me = attendees
            .iter()
            .find(|a| a["email"] == "me@example.com")
            .unwrap();
        assert_eq!(me["responseStatus"], "needsAction");
        let opt = attendees
            .iter()
            .find(|a| a["email"] == "opt@example.com")
            .unwrap();
        assert_eq!(opt["optional"], true);
        assert_eq!(body["reminders"]["useDefault"], false);
        assert_eq!(body["reminders"]["overrides"].as_array().unwrap().len(), 2);
        // Google-derived and client-written documents fingerprint equal.
        let rendered = Semantic::from_ics(&obj.ics).unwrap().fingerprint();
        let reparsed = Semantic::from_ics(&doc.ics).unwrap().fingerprint();
        assert_eq!(rendered, reparsed);
        assert_eq!(
            doc_to_gevent_body(&doc, BodyMode::RsvpOnly, &[]),
            serde_json::json!({})
        );
    }

    #[test]
    fn partstat_and_send_updates_mappings() {
        assert_eq!(google_to_partstat("accepted"), "ACCEPTED");
        assert_eq!(google_to_partstat("whatever"), "NEEDS-ACTION");
        assert_eq!(partstat_to_google("tentative"), "tentative");
        assert_eq!(partstat_to_google("NEEDS-ACTION"), "needsAction");
        assert_eq!(
            send_updates_for(crate::jadav::config::SendVia::Smtp),
            "none"
        );
        assert_eq!(
            send_updates_for(crate::jadav::config::SendVia::Provider),
            "all"
        );
    }

    #[test]
    fn gevent_serialisation_skips_absent_fields() {
        let body = serde_json::to_value(GEvent {
            summary: Some("x".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(body, serde_json::json!({"summary": "x"}));
        let a: GAttendee = serde_json::from_str(r#"{"email":"a@b","self":true}"#).unwrap();
        assert_eq!(a.self_, Some(true));
    }

    #[test]
    fn birthday_events_are_kept_only_on_the_contacts_calendar() {
        let normal = default_skip_event_types("w@example.com");
        assert!(normal.iter().any(|t| t == "birthday"));
        assert!(normal.iter().any(|t| t == "workingLocation"));
        let contacts = default_skip_event_types("addressbook#contacts@group.v.calendar.google.com");
        assert!(!contacts.iter().any(|t| t == "birthday"));
        assert!(contacts.iter().any(|t| t == "workingLocation"));
        let e = GCalendarListEntry {
            id: "x".to_string(),
            summary: Some("Owner name".to_string()),
            summary_override: Some("My name".to_string()),
            access_role: Some("writer".to_string()),
            ..Default::default()
        };
        assert_eq!(e.name(), "My name");
        assert!(e.writable());
    }

    #[test]
    fn default_reminders_become_alarms_on_pull_and_use_default_again_on_push() {
        let defaults = vec![GReminderOverride {
            method: "popup".to_string(),
            minutes: 10,
        }];
        // An event on the calendar's defaults gets them as explicit alarms.
        let mut on_defaults = g(
            r#"{"id": "d1", "iCalUID": "d1@google.com", "summary": "Standup",
            "start": {"dateTime": "2026-09-15T09:30:00+02:00", "timeZone": "Europe/Budapest"},
            "end": {"dateTime": "2026-09-15T09:45:00+02:00", "timeZone": "Europe/Budapest"},
            "reminders": {"useDefault": true}}"#,
        );
        materialize_default_reminders(&mut on_defaults, &defaults);
        let obj = gevent_group_to_object(&on_defaults, &[]).unwrap();
        assert!(obj.ics.contains("BEGIN:VALARM"), "{}", obj.ics);
        assert!(obj.ics.contains("TRIGGER:-PT10M"));
        // Its own overrides win over the defaults.
        let mut own = g(
            r#"{"id": "d2", "iCalUID": "d2@google.com", "summary": "Own",
            "start": {"dateTime": "2026-09-15T09:30:00+02:00"}, "end": {"dateTime": "2026-09-15T09:45:00+02:00"},
            "reminders": {"useDefault": false, "overrides": [{"method": "email", "minutes": 60}]}}"#,
        );
        materialize_default_reminders(&mut own, &defaults);
        assert_eq!(own.reminders.unwrap().overrides.unwrap()[0].minutes, 60);
        // No reminders object at all counts as "use the defaults".
        let mut bare = g(r#"{"id": "d3", "summary": "Bare",
            "start": {"dateTime": "2026-09-15T09:30:00+02:00"}, "end": {"dateTime": "2026-09-15T09:45:00+02:00"}}"#);
        materialize_default_reminders(&mut bare, &defaults);
        assert_eq!(bare.reminders.unwrap().overrides.unwrap().len(), 1);

        // Pushing the mirrored copy back says "use defaults", not a copy
        // of them; a different alarm set goes as overrides.
        let doc = VCalendarDoc::parse(&obj.ics).unwrap();
        let body = doc_to_gevent_body(&doc, BodyMode::Full, &defaults);
        assert_eq!(body["reminders"]["useDefault"], true);
        assert!(body["reminders"].get("overrides").is_none());
        let body = doc_to_gevent_body(&doc, BodyMode::Full, &[]);
        assert_eq!(body["reminders"]["useDefault"], false);
        assert_eq!(body["reminders"]["overrides"][0]["minutes"], 10);
        assert!(same_reminders(
            &[
                GReminderOverride {
                    method: "popup".into(),
                    minutes: 10
                },
                GReminderOverride {
                    method: "email".into(),
                    minutes: 60
                }
            ],
            &[
                GReminderOverride {
                    method: "EMAIL".into(),
                    minutes: 60
                },
                GReminderOverride {
                    method: "popup".into(),
                    minutes: 10
                }
            ]
        ));
    }
}
