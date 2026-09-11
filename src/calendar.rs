//! iCalendar (RFC 5545) domain model: parsing `VEVENT` components out of an
//! `.ics`/`text/calendar` payload, and serializing edits back.
//!
//! ## Scope
//!
//! This is a standards-conscious *subset* parser/serializer, not a general
//! iCalendar library. It understands exactly the properties jamail's
//! calendar view needs: `UID`, `DTSTART`/`DTEND` (or `DURATION`),
//! `SUMMARY`, `DESCRIPTION`, `LOCATION`, `STATUS`, `ORGANIZER`/`ATTENDEE`,
//! `RRULE` (preserved, not expanded — see below), `SEQUENCE`, `DTSTAMP`,
//! and `VALARM` sub-components (`TRIGGER`/`ACTION`/`DESCRIPTION`).
//!
//! **Every other property is preserved verbatim, not dropped.** Rather than
//! reconstructing a `VEVENT` from scratch on every edit (which would
//! silently discard `EXDATE`/`RDATE`, `CATEGORIES`, `CLASS`, `GEO`,
//! `X-`-prefixed vendor extensions, and anything else this module doesn't
//! model), [`VEvent`] keeps the original raw component text and
//! [`VEvent::to_ics`] *patches* only the specific lines that changed,
//! leaving everything else byte-for-byte untouched. This is the safest
//! approach for a client that talks to servers/other clients whose data
//! this crate doesn't fully understand.
//!
//! ## Explicit, documented limitations
//!
//! - **Recurrence expansion is display-only and best-effort.**
//!   [`expand_occurrences`] uses the `rrule` crate to compute every
//!   occurrence of a recurring event (`RRULE`, honoring `EXDATE`) that
//!   overlaps a given window — this is what calendar-grid views call to
//!   show a repeating event on every date it actually falls on, not just
//!   its "master" `DTSTART`/`DTEND`. A raw `RRULE` value that the `rrule`
//!   crate's stricter parser/validator rejects (should be rare —
//!   [`validate_rrule`] already rejects the common mistakes at creation
//!   time, but a third-party server can still emit something this crate
//!   disagrees on) falls back to just the master occurrence, never an
//!   error or a guess. `RDATE` (ad-hoc extra occurrences beyond the rule)
//!   and `EXRULE` (a second rule describing exclusions, deprecated by RFC
//!   5545 and not supported by the `rrule` crate build used here) are
//!   still not modeled — both fall under "everything else is preserved
//!   verbatim" above, so they round-trip correctly but don't affect which
//!   occurrences are shown. **Editing or deleting a recurring event always
//!   acts on the single master `VEVENT`** — there is no "this occurrence
//!   only" vs. "this and following" vs. "all occurrences" distinction;
//!   every computed occurrence shown in the UI shares one underlying
//!   database row and CalDAV resource.
//! - **Only IANA-named timezones are understood.** A `DTSTART`/`DTEND`
//!   with `TZID=America/New_York` is resolved via the IANA database
//!   (`chrono-tz`). A `TZID` that isn't a recognized IANA name (e.g. a
//!   server that emits a custom `VTIMEZONE` with a made-up identifier and
//!   expects clients to read its offset rules from that block) is an
//!   explicit [`CalendarError::UnsupportedTimezone`] — this module does
//!   not parse `VTIMEZONE` blocks at all.
//! - **Floating (timezone-less) date-times are treated as the local
//!   machine timezone.** RFC 5545 leaves floating times to be interpreted
//!   in "whatever timezone the viewer is currently in"; since parsing here
//!   happens both in `jamaild` (server-side, headless) and `jacal`
//!   (client-side), the pragmatic choice is the host machine's local
//!   timezone (`chrono::Local`) for both, which is correct for the common
//!   case of a single-machine or same-timezone setup and is the
//!   documented limitation otherwise.
//! - **Multi-valued/quoted parameters beyond simple `NAME=value` pairs**
//!   (e.g. `MEMBER="mailto:a@x,mailto:b@x"` list values) are read as a
//!   single opaque string, not split into a list. Not needed for any field
//!   this module models structurally.

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rrule::{RRule, RRuleSet, Tz as RRuleTz, Unvalidated};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Errors for iCalendar constructs this module cannot safely interpret or
/// produce. Deliberately distinct from a bag-of-strings `anyhow::Error` so
/// callers (and tests) can match on *why* a document was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarError {
    MissingField(&'static str),
    InvalidDateTime(String),
    UnsupportedTimezone(String),
    InvalidDuration(String),
    Malformed(String),
}

impl fmt::Display for CalendarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalendarError::MissingField(name) => write!(f, "missing required field: {}", name),
            CalendarError::InvalidDateTime(v) => write!(f, "invalid date-time value: {}", v),
            CalendarError::UnsupportedTimezone(tz) => {
                write!(f, "unsupported timezone (not a known IANA name): {}", tz)
            }
            CalendarError::InvalidDuration(v) => write!(f, "invalid iCalendar duration: {}", v),
            CalendarError::Malformed(v) => write!(f, "malformed iCalendar data: {}", v),
        }
    }
}

impl std::error::Error for CalendarError {}

/// A parsed `DTSTART`/`DTEND`/absolute-`TRIGGER` value, timezone-aware.
///
/// The canonical instant is always stored as UTC (`utc`); `tzid` retains
/// the original zone name (when the source used one) purely so
/// [`VEvent::to_ics`] can round-trip it back into the same form rather than
/// flattening every timestamp to `Z`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventTime {
    pub utc: DateTime<Utc>,
    pub tzid: Option<String>,
    pub all_day: bool,
}

impl EventTime {
    pub fn utc(dt: DateTime<Utc>) -> Self {
        Self {
            utc: dt,
            tzid: None,
            all_day: false,
        }
    }

    pub fn all_day(date: NaiveDate) -> Self {
        Self {
            utc: Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()),
            tzid: None,
            all_day: true,
        }
    }

    pub fn with_tz(dt: DateTime<Utc>, tzid: impl Into<String>) -> Self {
        Self {
            utc: dt,
            tzid: Some(tzid.into()),
            all_day: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventStatus {
    Confirmed,
    Tentative,
    Cancelled,
}

impl EventStatus {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "TENTATIVE" => EventStatus::Tentative,
            "CANCELLED" => EventStatus::Cancelled,
            _ => EventStatus::Confirmed,
        }
    }

    pub fn as_ics(&self) -> &'static str {
        match self {
            EventStatus::Confirmed => "CONFIRMED",
            EventStatus::Tentative => "TENTATIVE",
            EventStatus::Cancelled => "CANCELLED",
        }
    }
}

impl fmt::Display for EventStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ics())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attendee {
    /// The `CAL-ADDRESS` value, with any leading `mailto:` stripped.
    pub email: String,
    pub name: Option<String>,
    pub role: Option<String>,
    pub partstat: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlarmRelated {
    Start,
    End,
}

/// A `VALARM`'s `TRIGGER`, in the two forms RFC 5545 §3.8.6.3 allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlarmTrigger {
    /// A signed duration relative to the event's start or end
    /// (`related`); negative means "before".
    Relative {
        offset: Duration,
        related: AlarmRelated,
    },
    Absolute(DateTime<Utc>),
}

impl AlarmTrigger {
    /// Resolve to the concrete instant the alarm should fire, given the
    /// owning event's start/end.
    pub fn fire_time(&self, dtstart: DateTime<Utc>, dtend: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            AlarmTrigger::Absolute(t) => *t,
            AlarmTrigger::Relative { offset, related } => {
                let anchor = match related {
                    AlarmRelated::Start => dtstart,
                    AlarmRelated::End => dtend,
                };
                anchor + *offset
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alarm {
    pub trigger: AlarmTrigger,
    pub action: String,
    pub description: Option<String>,
}

/// A parsed `VEVENT`, plus enough of the original raw text to patch on
/// edit (see module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VEvent {
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub location: String,
    pub status: EventStatus,
    pub organizer: Option<String>,
    pub attendees: Vec<Attendee>,
    pub dtstart: EventTime,
    pub dtend: EventTime,
    pub rrule: Option<String>,
    /// Instants excluded from `rrule`'s recurrence (RFC 5545 `EXDATE`) —
    /// e.g. "this week's occurrence was cancelled". Always empty for a
    /// non-recurring event. Matched against a computed occurrence's exact
    /// start instant by [`expand_occurrences`].
    pub exdates: Vec<DateTime<Utc>>,
    pub alarms: Vec<Alarm>,
    pub sequence: i64,
    pub dtstamp: Option<DateTime<Utc>>,
    /// The original `BEGIN:VEVENT`..`END:VEVENT` block text, unfolded to
    /// one property per line but otherwise verbatim. `None` for an event
    /// being newly created (not yet round-tripped through a server).
    pub raw: Option<String>,
}

impl VEvent {
    /// True if this event carries any recurrence rule. See module docs:
    /// recurrence is preserved, never expanded.
    pub fn is_recurring(&self) -> bool {
        self.rrule.is_some()
    }
}

/// Fields a user is allowed to change through jacal's edit UI. `None`
/// means "leave as-is". Anything not listed here (`ORGANIZER`, `ATTENDEE`,
/// `VALARM`, `RRULE`, and any property this module doesn't model at all)
/// is intentionally not editable and is always preserved verbatim.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventEdits {
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub status: Option<EventStatus>,
    pub dtstart: Option<EventTime>,
    pub dtend: Option<EventTime>,
    /// Only used when creating a brand-new event; ignored (not
    /// re-editable) once an event exists, per the module's "RRULE isn't
    /// editable after creation" scope decision. Accepted as a raw
    /// `RRULE` property value (e.g. `"FREQ=WEEKLY;COUNT=5"`), verified
    /// only for gross well-formedness (see [`validate_rrule`]).
    pub rrule: Option<String>,
}

// ---------------------------------------------------------------------
// Content-line parsing (RFC 5545 §3.1)
// ---------------------------------------------------------------------

struct ContentLine {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl ContentLine {
    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Undo RFC 5545 §3.1 line folding: a line starting with a single space or
/// tab is a continuation of the previous line. Tolerates bare `\n` as well
/// as `\r\n`.
fn unfold(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw_line in raw.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if (line.starts_with(' ') || line.starts_with('\t')) && !lines.is_empty() {
            let last = lines.last_mut().unwrap();
            last.push_str(&line[1..]);
        } else {
            lines.push(line.to_string());
        }
    }
    lines
}

fn parse_content_line(line: &str) -> Option<ContentLine> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] != b';' && bytes[i] != b':' {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let name = line[..i].to_ascii_uppercase();
    let mut params = Vec::new();
    while i < bytes.len() && bytes[i] == b';' {
        i += 1;
        let pstart = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let pname = line[pstart..i].to_ascii_uppercase();
        i += 1;
        let mut pval = String::new();
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let vstart = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            pval.push_str(&line[vstart..i.min(line.len())]);
            if i < bytes.len() {
                i += 1;
            }
        } else {
            let vstart = i;
            while i < bytes.len() && bytes[i] != b';' && bytes[i] != b':' {
                i += 1;
            }
            pval.push_str(&line[vstart..i]);
        }
        params.push((pname, pval));
    }
    if i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    i += 1;
    Some(ContentLine {
        name,
        params,
        value: line[i..].to_string(),
    })
}

fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(';') => out.push(';'),
                Some(',') => out.push(','),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------
// Duration parsing/formatting (RFC 5545 §3.3.6 — no year/month units)
// ---------------------------------------------------------------------

fn parse_ical_duration(s: &str) -> Result<Duration, CalendarError> {
    let bad = || CalendarError::InvalidDuration(s.to_string());
    let mut chars = s.chars().peekable();
    let mut sign: i64 = 1;
    match chars.peek() {
        Some('+') => {
            chars.next();
        }
        Some('-') => {
            sign = -1;
            chars.next();
        }
        _ => {}
    }
    if chars.next() != Some('P') {
        return Err(bad());
    }
    let mut total_seconds: i64 = 0;
    let mut in_time = false;
    let mut num = String::new();
    let mut saw_any = false;
    for c in chars {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            'W' if !in_time => {
                let n: i64 = num.parse().map_err(|_| bad())?;
                total_seconds += n * 7 * 86400;
                num.clear();
                saw_any = true;
            }
            'D' if !in_time => {
                let n: i64 = num.parse().map_err(|_| bad())?;
                total_seconds += n * 86400;
                num.clear();
                saw_any = true;
            }
            'H' if in_time => {
                let n: i64 = num.parse().map_err(|_| bad())?;
                total_seconds += n * 3600;
                num.clear();
                saw_any = true;
            }
            'M' if in_time => {
                let n: i64 = num.parse().map_err(|_| bad())?;
                total_seconds += n * 60;
                num.clear();
                saw_any = true;
            }
            'S' if in_time => {
                let n: i64 = num.parse().map_err(|_| bad())?;
                total_seconds += n;
                num.clear();
                saw_any = true;
            }
            _ => return Err(bad()),
        }
    }
    if !saw_any || !num.is_empty() {
        return Err(bad());
    }
    Ok(Duration::seconds(sign * total_seconds))
}

fn format_ical_duration(d: Duration) -> String {
    let neg = d.num_seconds() < 0;
    let mut secs = d.num_seconds().unsigned_abs();
    let days = secs / 86400;
    secs %= 86400;
    let hours = secs / 3600;
    secs %= 3600;
    let minutes = secs / 60;
    secs %= 60;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push('P');
    if days > 0 {
        out.push_str(&format!("{}D", days));
    }
    if hours > 0 || minutes > 0 || secs > 0 {
        out.push('T');
        if hours > 0 {
            out.push_str(&format!("{}H", hours));
        }
        if minutes > 0 {
            out.push_str(&format!("{}M", minutes));
        }
        if secs > 0 {
            out.push_str(&format!("{}S", secs));
        }
    } else if days == 0 {
        out.push_str("T0S");
    }
    out
}

// ---------------------------------------------------------------------
// Date-time parsing/formatting
// ---------------------------------------------------------------------

fn parse_naive_datetime(value: &str) -> Result<NaiveDateTime, CalendarError> {
    let v = value.trim_end_matches('Z');
    NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S")
        .map_err(|_| CalendarError::InvalidDateTime(value.to_string()))
}

fn parse_naive_date(value: &str) -> Result<NaiveDate, CalendarError> {
    NaiveDate::parse_from_str(value.trim(), "%Y%m%d")
        .map_err(|_| CalendarError::InvalidDateTime(value.to_string()))
}

/// Parse a `DTSTART`/`DTEND`/`RECURRENCE-ID`-shaped property's value given
/// its parameters. See module docs for the floating-time and
/// non-IANA-timezone limitations.
fn parse_event_time(line: &ContentLine) -> Result<EventTime, CalendarError> {
    parse_event_time_value(&line.value, line)
}

/// Parse one date/date-time value (`value`) using the `VALUE`/`TZID`
/// parameters carried on `line`. Split out from [`parse_event_time`] so
/// multi-value properties (`EXDATE`, `RDATE`) can parse each comma-separated
/// value with the same `VALUE=DATE`/`TZID`/floating-time/`Z`-suffix rules
/// DTSTART/DTEND use, without re-deriving them.
fn parse_event_time_value(value: &str, line: &ContentLine) -> Result<EventTime, CalendarError> {
    if line.param("VALUE") == Some("DATE") {
        return Ok(EventTime::all_day(parse_naive_date(value)?));
    }
    if value.ends_with('Z') {
        let naive = parse_naive_datetime(value)?;
        return Ok(EventTime::utc(Utc.from_utc_datetime(&naive)));
    }
    if let Some(tzid) = line.param("TZID") {
        let tz = chrono_tz::Tz::from_str(tzid)
            .map_err(|_| CalendarError::UnsupportedTimezone(tzid.to_string()))?;
        let naive = parse_naive_datetime(value)?;
        let resolved = match tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => dt,
            // DST "spring forward" gap or "fall back" ambiguity: pick the
            // earlier of the two candidates (or the nearest valid instant)
            // deterministically rather than erroring on ordinary calendar
            // data that happens to fall in a transition window.
            chrono::LocalResult::Ambiguous(a, _) => a,
            chrono::LocalResult::None => {
                return Err(CalendarError::InvalidDateTime(format!(
                    "{} does not exist in {} (DST transition)",
                    value, tzid
                )));
            }
        };
        return Ok(EventTime::with_tz(
            resolved.with_timezone(&Utc),
            tzid.to_string(),
        ));
    }
    // Floating time: no TZID, no trailing Z, not a DATE value.
    let naive = parse_naive_datetime(value)?;
    let local = match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt,
        chrono::LocalResult::Ambiguous(a, _) => a,
        chrono::LocalResult::None => {
            return Err(CalendarError::InvalidDateTime(format!(
                "{} does not exist in the local timezone (DST transition)",
                value
            )));
        }
    };
    Ok(EventTime::utc(local.with_timezone(&Utc)))
}

/// Parse an `EXDATE` content line, which may carry one or more
/// comma-separated date/date-time values (RFC 5545 §3.8.5.1).
fn parse_exdate_line(line: &ContentLine) -> Result<Vec<DateTime<Utc>>, CalendarError> {
    line.value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| parse_event_time_value(v, line).map(|t| t.utc))
        .collect()
}

/// Render `name` (e.g. `"DTSTART"`) plus `t` as a full iCalendar property
/// line (no trailing CRLF).
fn render_time_property(name: &str, t: &EventTime) -> String {
    if t.all_day {
        format!("{};VALUE=DATE:{}", name, t.utc.format("%Y%m%d"))
    } else if let Some(tzid) = &t.tzid {
        let tz = chrono_tz::Tz::from_str(tzid).unwrap_or(chrono_tz::UTC);
        let local = t.utc.with_timezone(&tz);
        format!("{};TZID={}:{}", name, tzid, local.format("%Y%m%dT%H%M%S"))
    } else {
        format!("{}:{}Z", name, t.utc.format("%Y%m%dT%H%M%S"))
    }
}

// ---------------------------------------------------------------------
// VEVENT parsing
// ---------------------------------------------------------------------

/// Parse every top-level `VEVENT` component out of an iCalendar document
/// (a bare `VEVENT` block, or a full `VCALENDAR` wrapping one or more of
/// them — `VTIMEZONE`/other sibling components are skipped, not
/// interpreted; see module docs).
pub fn parse_vevents(ics: &str) -> Result<Vec<VEvent>, CalendarError> {
    let lines = unfold(ics);
    let mut events = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].eq_ignore_ascii_case("BEGIN:VEVENT") {
            let start = i;
            let mut depth = 1;
            i += 1;
            while i < lines.len() && depth > 0 {
                if lines[i].len() >= 6 && lines[i][..6].eq_ignore_ascii_case("BEGIN:") {
                    depth += 1;
                } else if lines[i].len() >= 4 && lines[i][..4].eq_ignore_ascii_case("END:") {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                i += 1;
            }
            let end = i.min(lines.len().saturating_sub(1));
            let block = &lines[start..=end];
            let raw = block.join("\r\n");
            events.push(parse_single_vevent(block, raw)?);
        }
        i += 1;
    }
    Ok(events)
}

fn parse_single_vevent(lines: &[String], raw: String) -> Result<VEvent, CalendarError> {
    let mut uid: Option<String> = None;
    let mut summary = String::new();
    let mut description = String::new();
    let mut location = String::new();
    let mut status = EventStatus::Confirmed;
    let mut organizer: Option<String> = None;
    let mut attendees = Vec::new();
    let mut dtstart: Option<EventTime> = None;
    let mut dtend: Option<EventTime> = None;
    let mut duration: Option<Duration> = None;
    let mut rrule: Option<String> = None;
    let mut exdates: Vec<DateTime<Utc>> = Vec::new();
    let mut alarms = Vec::new();
    let mut sequence: i64 = 0;
    let mut dtstamp: Option<DateTime<Utc>> = None;

    let mut in_valarm = false;
    let mut alarm_trigger: Option<AlarmTrigger> = None;
    let mut alarm_action = String::new();
    let mut alarm_description: Option<String> = None;
    let mut depth = 0i32;

    for raw_line in &lines[1..lines.len().saturating_sub(1)] {
        if raw_line.len() >= 6 && raw_line[..6].eq_ignore_ascii_case("BEGIN:") {
            if raw_line[6..].eq_ignore_ascii_case("VALARM") {
                in_valarm = true;
                alarm_trigger = None;
                alarm_action.clear();
                alarm_description = None;
            }
            depth += 1;
            continue;
        }
        if raw_line.len() >= 4 && raw_line[..4].eq_ignore_ascii_case("END:") {
            if in_valarm && raw_line[4..].eq_ignore_ascii_case("VALARM") {
                if let Some(trigger) = alarm_trigger.take() {
                    alarms.push(Alarm {
                        trigger,
                        action: if alarm_action.is_empty() {
                            "DISPLAY".to_string()
                        } else {
                            alarm_action.clone()
                        },
                        description: alarm_description.take(),
                    });
                }
                in_valarm = false;
            }
            depth -= 1;
            continue;
        }
        if depth > 0 && !in_valarm {
            // Inside some other nested component we don't interpret
            // (shouldn't normally occur directly inside VEVENT other than
            // VALARM, but skip defensively rather than misparse).
            continue;
        }

        let Some(cl) = parse_content_line(raw_line) else {
            continue;
        };

        if in_valarm {
            match cl.name.as_str() {
                "TRIGGER" => {
                    alarm_trigger = Some(parse_trigger(&cl)?);
                }
                "ACTION" => alarm_action = cl.value.trim().to_string(),
                "DESCRIPTION" => alarm_description = Some(unescape_text(&cl.value)),
                _ => {}
            }
            continue;
        }

        match cl.name.as_str() {
            "UID" => uid = Some(cl.value.trim().to_string()),
            "SUMMARY" => summary = unescape_text(&cl.value),
            "DESCRIPTION" => description = unescape_text(&cl.value),
            "LOCATION" => location = unescape_text(&cl.value),
            "STATUS" => status = EventStatus::parse(&cl.value),
            "ORGANIZER" => organizer = Some(normalize_cal_address(&cl.value)),
            "ATTENDEE" => attendees.push(parse_attendee(&cl)),
            "DTSTART" => dtstart = Some(parse_event_time(&cl)?),
            "DTEND" => dtend = Some(parse_event_time(&cl)?),
            "DURATION" => duration = Some(parse_ical_duration(cl.value.trim())?),
            "RRULE" => rrule = Some(cl.value.trim().to_string()),
            "EXDATE" => exdates.extend(parse_exdate_line(&cl)?),
            "SEQUENCE" => sequence = cl.value.trim().parse().unwrap_or(0),
            "DTSTAMP" => {
                dtstamp = parse_event_time(&cl).ok().map(|t| t.utc);
            }
            _ => {}
        }
    }

    let uid = uid.ok_or(CalendarError::MissingField("UID"))?;
    let dtstart = dtstart.ok_or(CalendarError::MissingField("DTSTART"))?;

    let dtend = match (dtend, duration) {
        (Some(e), _) => e,
        (None, Some(d)) => EventTime {
            utc: dtstart.utc + d,
            tzid: dtstart.tzid.clone(),
            all_day: dtstart.all_day,
        },
        (None, None) if dtstart.all_day => EventTime {
            utc: dtstart.utc + Duration::days(1),
            tzid: None,
            all_day: true,
        },
        (None, None) => dtstart.clone(),
    };

    Ok(VEvent {
        uid,
        summary,
        description,
        location,
        status,
        organizer,
        attendees,
        dtstart,
        dtend,
        rrule,
        exdates,
        alarms,
        sequence,
        dtstamp,
        raw: Some(raw),
    })
}

fn parse_trigger(cl: &ContentLine) -> Result<AlarmTrigger, CalendarError> {
    let related = match cl.param("RELATED") {
        Some(r) if r.eq_ignore_ascii_case("END") => AlarmRelated::End,
        _ => AlarmRelated::Start,
    };
    if cl.param("VALUE") == Some("DATE-TIME") || cl.value.ends_with('Z') {
        let naive = parse_naive_datetime(&cl.value)?;
        return Ok(AlarmTrigger::Absolute(Utc.from_utc_datetime(&naive)));
    }
    let offset = parse_ical_duration(cl.value.trim())?;
    Ok(AlarmTrigger::Relative { offset, related })
}

fn normalize_cal_address(value: &str) -> String {
    value
        .trim()
        .strip_prefix("mailto:")
        .or_else(|| value.trim().strip_prefix("MAILTO:"))
        .unwrap_or(value.trim())
        .to_string()
}

fn parse_attendee(cl: &ContentLine) -> Attendee {
    Attendee {
        email: normalize_cal_address(&cl.value),
        name: cl.param("CN").map(|s| s.to_string()),
        role: cl.param("ROLE").map(|s| s.to_string()),
        partstat: cl.param("PARTSTAT").map(|s| s.to_string()),
    }
}

/// Gross well-formedness check for a user-typed `RRULE` value: must be
/// `KEY=VALUE` pairs separated by `;`, and must include a recognized
/// `FREQ`. Does not validate every RFC 5545 constraint (e.g. `BYDAY`
/// values) — just enough to reject obvious typos before they're sent to a
/// server.
pub fn validate_rrule(rrule: &str) -> Result<(), CalendarError> {
    let mut has_freq = false;
    for part in rrule.split(';') {
        let part = part.trim();
        if part.is_empty() {
            return Err(CalendarError::Malformed(format!(
                "empty RRULE part in: {}",
                rrule
            )));
        }
        let Some((k, v)) = part.split_once('=') else {
            return Err(CalendarError::Malformed(format!(
                "RRULE part missing '=': {}",
                part
            )));
        };
        if v.is_empty() {
            return Err(CalendarError::Malformed(format!(
                "RRULE part missing value: {}",
                part
            )));
        }
        if k.eq_ignore_ascii_case("FREQ") {
            if !matches!(
                v.to_ascii_uppercase().as_str(),
                "SECONDLY" | "MINUTELY" | "HOURLY" | "DAILY" | "WEEKLY" | "MONTHLY" | "YEARLY"
            ) {
                return Err(CalendarError::Malformed(format!(
                    "unrecognized RRULE FREQ: {}",
                    v
                )));
            }
            has_freq = true;
        }
    }
    if !has_freq {
        return Err(CalendarError::Malformed(format!(
            "RRULE missing FREQ: {}",
            rrule
        )));
    }
    Ok(())
}

/// A short, human-readable summary of a raw `RRULE` value for display
/// (e.g. `"Repeats weekly"`, `"Repeats daily, 5 times"`). Pure metadata
/// display — never used to compute occurrences (see module docs).
pub fn describe_rrule(rrule: &str) -> String {
    let mut freq = None;
    let mut interval = 1u32;
    let mut count = None;
    let mut until = None;
    for part in rrule.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            match k.to_ascii_uppercase().as_str() {
                "FREQ" => freq = Some(v.to_ascii_uppercase()),
                "INTERVAL" => interval = v.parse().unwrap_or(1),
                "COUNT" => count = v.parse::<u32>().ok(),
                "UNTIL" => until = Some(v.to_string()),
                _ => {}
            }
        }
    }
    let base = match (freq.as_deref(), interval) {
        (Some("DAILY"), 1) => "Repeats daily".to_string(),
        (Some("DAILY"), n) => format!("Repeats every {} days", n),
        (Some("WEEKLY"), 1) => "Repeats weekly".to_string(),
        (Some("WEEKLY"), n) => format!("Repeats every {} weeks", n),
        (Some("MONTHLY"), 1) => "Repeats monthly".to_string(),
        (Some("MONTHLY"), n) => format!("Repeats every {} months", n),
        (Some("YEARLY"), 1) => "Repeats yearly".to_string(),
        (Some("YEARLY"), n) => format!("Repeats every {} years", n),
        (Some(other), 1) => format!("Repeats ({})", other.to_lowercase()),
        (Some(other), n) => format!("Repeats every {} ({})", n, other.to_lowercase()),
        (None, _) => "Repeats".to_string(),
    };
    if let Some(c) = count {
        format!("{}, {} times", base, c)
    } else if let Some(u) = until {
        format!("{} until {}", base, u)
    } else {
        base
    }
}

// ---------------------------------------------------------------------
// Recurrence expansion (display-only — see module docs)
// ---------------------------------------------------------------------

/// Hard cap on occurrences computed per [`expand_occurrences`] call, so a
/// pathological rule (e.g. `FREQ=MINUTELY` over a year-long window) can't
/// blow up memory/CPU. Generous for anything a calendar grid could
/// meaningfully render — a daily event over 10 years is ~3650.
const MAX_EXPANDED_OCCURRENCES: u16 = 3660;

/// Compute every occurrence of `ev` that overlaps `[window_start,
/// window_end)`, as `(start, end)` UTC instant pairs, sorted ascending.
///
/// Works uniformly for both recurring and non-recurring events: a
/// non-recurring `ev` (no `RRULE`) yields at most one pair — its own
/// `dtstart`/`dtend` — present only if it overlaps the window. A
/// recurring `ev` yields one pair per occurrence in range, each with the
/// same duration as the master event and with `ev.exdates` excluded. See
/// the module docs for exactly what's *not* handled (`RDATE`, `EXRULE`,
/// and a malformed/unsupported `RRULE`, which falls back to just the
/// master occurrence).
pub fn expand_occurrences(
    ev: &VEvent,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let duration = ev.dtend.utc - ev.dtstart.utc;
    let master_overlaps = ev.dtstart.utc < window_end && ev.dtend.utc > window_start;

    let Some(rrule_str) = ev.rrule.as_deref() else {
        return if master_overlaps {
            vec![(ev.dtstart.utc, ev.dtend.utc)]
        } else {
            Vec::new()
        };
    };

    match expand_rrule_occurrences(rrule_str, ev, window_start, window_end, duration) {
        Some(mut occurrences) => {
            occurrences.sort();
            occurrences
        }
        // Couldn't parse/validate this RRULE under the `rrule` crate's
        // grammar: never silently drop the event, show what we know for
        // certain (its master occurrence) instead of guessing.
        None if master_overlaps => vec![(ev.dtstart.utc, ev.dtend.utc)],
        None => Vec::new(),
    }
}

fn expand_rrule_occurrences(
    rrule_str: &str,
    ev: &VEvent,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    duration: Duration,
) -> Option<Vec<(DateTime<Utc>, DateTime<Utc>)>> {
    let unvalidated: RRule<Unvalidated> = rrule_str.parse().ok()?;
    let dt_start = ev.dtstart.utc.with_timezone(&RRuleTz::UTC);
    let validated = unvalidated.validate(dt_start).ok()?;
    let mut set = RRuleSet::new(dt_start).rrule(validated);
    for ex in &ev.exdates {
        set = set.exdate(ex.with_timezone(&RRuleTz::UTC));
    }

    // Look back by one event-duration before the window so an occurrence
    // that *starts* before `window_start` but still overlaps it (e.g. an
    // event spanning midnight) isn't missed — `.after()` filters on
    // occurrence start, not overlap.
    let lookback = if duration > Duration::zero() {
        duration
    } else {
        Duration::zero()
    };
    let after = (window_start - lookback).with_timezone(&RRuleTz::UTC);
    let before = window_end.with_timezone(&RRuleTz::UTC);

    let result = set
        .after(after)
        .before(before)
        .all(MAX_EXPANDED_OCCURRENCES);
    Some(
        result
            .dates
            .into_iter()
            .map(|start| {
                let start_utc = start.with_timezone(&Utc);
                (start_utc, start_utc + duration)
            })
            .filter(|(s, e)| *s < window_end && *e > window_start)
            .collect(),
    )
}

// ---------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------

const PROPERTY_NAMES_TO_PATCH: &[&str] = &[
    "SUMMARY",
    "DESCRIPTION",
    "LOCATION",
    "STATUS",
    "DTSTART",
    "DTEND",
    "DURATION",
    "SEQUENCE",
    "DTSTAMP",
    "LAST-MODIFIED",
];

impl VEvent {
    /// Build a brand-new minimal `VCALENDAR`/`VEVENT` document from this
    /// event's structured fields (used when creating an event that has no
    /// `raw` original to patch).
    pub fn to_new_ics(&self, now: DateTime<Utc>) -> String {
        let mut lines = Vec::new();
        lines.push("BEGIN:VCALENDAR".to_string());
        lines.push("VERSION:2.0".to_string());
        lines.push("PRODID:-//jamail//jacal//EN".to_string());
        lines.push("BEGIN:VEVENT".to_string());
        lines.push(format!("UID:{}", self.uid));
        lines.push(format!("DTSTAMP:{}Z", now.format("%Y%m%dT%H%M%S")));
        lines.push(render_time_property("DTSTART", &self.dtstart));
        lines.push(render_time_property("DTEND", &self.dtend));
        lines.push(format!("SUMMARY:{}", escape_text(&self.summary)));
        if !self.description.is_empty() {
            lines.push(format!("DESCRIPTION:{}", escape_text(&self.description)));
        }
        if !self.location.is_empty() {
            lines.push(format!("LOCATION:{}", escape_text(&self.location)));
        }
        lines.push(format!("STATUS:{}", self.status.as_ics()));
        lines.push(format!("SEQUENCE:{}", self.sequence));
        if let Some(rrule) = &self.rrule {
            lines.push(format!("RRULE:{}", rrule));
        }
        if let Some(org) = &self.organizer {
            lines.push(format!("ORGANIZER:mailto:{}", org));
        }
        for a in &self.attendees {
            lines.push(render_attendee(a));
        }
        for alarm in &self.alarms {
            lines.push("BEGIN:VALARM".to_string());
            lines.push(format!("ACTION:{}", alarm.action));
            lines.push(render_trigger(&alarm.trigger));
            if let Some(d) = &alarm.description {
                lines.push(format!("DESCRIPTION:{}", escape_text(d)));
            }
            lines.push("END:VALARM".to_string());
        }
        lines.push("END:VEVENT".to_string());
        lines.push("END:VCALENDAR".to_string());
        lines.join("\r\n") + "\r\n"
    }

    /// Serialize this event, applying `edits` (if any) either by patching
    /// the original raw block (preserving every property this module
    /// doesn't model) or, if there's no original, by building a fresh
    /// document via [`Self::to_new_ics`]. Bumps `SEQUENCE` and refreshes
    /// `DTSTAMP`/`LAST-MODIFIED` whenever `edits` changes anything,
    /// matching RFC 5545 §3.8.7.4's guidance that `SEQUENCE` increases on
    /// each significant revision.
    pub fn to_ics(&self, edits: &EventEdits, now: DateTime<Utc>) -> Result<String, CalendarError> {
        let mut updated = self.clone();
        let mut changed = false;
        if let Some(v) = &edits.summary {
            updated.summary = v.clone();
            changed = true;
        }
        if let Some(v) = &edits.description {
            updated.description = v.clone();
            changed = true;
        }
        if let Some(v) = &edits.location {
            updated.location = v.clone();
            changed = true;
        }
        if let Some(v) = edits.status {
            updated.status = v;
            changed = true;
        }
        if let Some(v) = &edits.dtstart {
            updated.dtstart = v.clone();
            changed = true;
        }
        if let Some(v) = &edits.dtend {
            updated.dtend = v.clone();
            changed = true;
        }
        if let Some(v) = &edits.rrule {
            validate_rrule(v)?;
            updated.rrule = Some(v.clone());
            changed = true;
        }
        if changed {
            updated.sequence += 1;
            updated.dtstamp = Some(now);
        }

        match &self.raw {
            Some(raw) => patch_raw_vevent(raw, &updated, now),
            None => Ok(updated.to_new_ics(now)),
        }
    }
}

fn render_attendee(a: &Attendee) -> String {
    let mut params = String::new();
    if let Some(cn) = &a.name {
        params.push_str(&format!(";CN={}", cn));
    }
    if let Some(role) = &a.role {
        params.push_str(&format!(";ROLE={}", role));
    }
    if let Some(partstat) = &a.partstat {
        params.push_str(&format!(";PARTSTAT={}", partstat));
    }
    format!("ATTENDEE{}:mailto:{}", params, a.email)
}

fn render_trigger(t: &AlarmTrigger) -> String {
    match t {
        AlarmTrigger::Absolute(dt) => {
            format!("TRIGGER;VALUE=DATE-TIME:{}Z", dt.format("%Y%m%dT%H%M%S"))
        }
        AlarmTrigger::Relative { offset, related } => {
            let related_param = match related {
                AlarmRelated::Start => "",
                AlarmRelated::End => ";RELATED=END",
            };
            format!("TRIGGER{}:{}", related_param, format_ical_duration(*offset))
        }
    }
}

/// Patch a raw `VEVENT` block: drop existing lines for any property in
/// [`PROPERTY_NAMES_TO_PATCH`] and re-insert the current values from
/// `updated`, leaving every other line (including ones for properties this
/// module doesn't model at all) untouched.
fn patch_raw_vevent(
    raw: &str,
    updated: &VEvent,
    now: DateTime<Utc>,
) -> Result<String, CalendarError> {
    let lines = unfold(raw);
    if lines.is_empty() || !lines[0].eq_ignore_ascii_case("BEGIN:VEVENT") {
        return Err(CalendarError::Malformed(
            "raw event text does not start with BEGIN:VEVENT".to_string(),
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut depth = 0i32;
    let mut inserted_replacements = false;
    for line in &lines {
        if line.len() >= 6 && line[..6].eq_ignore_ascii_case("BEGIN:") {
            if depth > 0 {
                out.push(line.clone());
            }
            depth += 1;
            if depth == 1 {
                out.push(line.clone());
            }
            continue;
        }
        if line.len() >= 4 && line[..4].eq_ignore_ascii_case("END:") {
            depth -= 1;
            if depth == 0 {
                if !inserted_replacements {
                    out.extend(replacement_lines(updated, now));
                    inserted_replacements = true;
                }
                out.push(line.clone());
                continue;
            }
            out.push(line.clone());
            continue;
        }
        if depth == 1 {
            // Top-level VEVENT property: drop it if it's one we patch —
            // the replacement set is inserted once, right before END.
            if let Some(cl) = parse_content_line(line)
                && PROPERTY_NAMES_TO_PATCH.contains(&cl.name.as_str())
            {
                continue;
            }
        }
        out.push(line.clone());
    }
    Ok(out.join("\r\n") + "\r\n")
}

fn replacement_lines(updated: &VEvent, now: DateTime<Utc>) -> Vec<String> {
    let mut lines = vec![
        render_time_property("DTSTART", &updated.dtstart),
        render_time_property("DTEND", &updated.dtend),
        format!("SUMMARY:{}", escape_text(&updated.summary)),
    ];
    if !updated.description.is_empty() {
        lines.push(format!("DESCRIPTION:{}", escape_text(&updated.description)));
    }
    if !updated.location.is_empty() {
        lines.push(format!("LOCATION:{}", escape_text(&updated.location)));
    }
    lines.push(format!("STATUS:{}", updated.status.as_ics()));
    lines.push(format!("SEQUENCE:{}", updated.sequence));
    lines.push(format!(
        "DTSTAMP:{}Z",
        updated.dtstamp.unwrap_or(now).format("%Y%m%dT%H%M%S")
    ));
    lines.push(format!("LAST-MODIFIED:{}Z", now.format("%Y%m%dT%H%M%S")));
    if let Some(rrule) = &updated.rrule {
        lines.push(format!("RRULE:{}", rrule));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn parses_minimal_vevent_with_utc_times() {
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:abc-123\r\n\
DTSTART:20240115T090000Z\r\n\
DTEND:20240115T100000Z\r\n\
SUMMARY:Team sync\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_vevents(ics).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.uid, "abc-123");
        assert_eq!(e.summary, "Team sync");
        assert_eq!(e.dtstart.utc, utc(2024, 1, 15, 9, 0, 0));
        assert_eq!(e.dtend.utc, utc(2024, 1, 15, 10, 0, 0));
        assert!(e.dtstart.tzid.is_none());
        assert!(!e.dtstart.all_day);
        assert_eq!(e.status, EventStatus::Confirmed);
    }

    #[test]
    fn missing_uid_is_an_explicit_error() {
        let ics = "BEGIN:VEVENT\r\nDTSTART:20240115T090000Z\r\nEND:VEVENT\r\n";
        let err = parse_vevents(ics).unwrap_err();
        assert_eq!(err, CalendarError::MissingField("UID"));
    }

    #[test]
    fn missing_dtstart_is_an_explicit_error() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nEND:VEVENT\r\n";
        let err = parse_vevents(ics).unwrap_err();
        assert_eq!(err, CalendarError::MissingField("DTSTART"));
    }

    #[test]
    fn parses_named_iana_timezone_and_normalizes_to_utc() {
        // 09:00 America/New_York in January (EST, UTC-5) is 14:00 UTC.
        let ics = "BEGIN:VEVENT\r\n\
UID:tz-1\r\n\
DTSTART;TZID=America/New_York:20240115T090000\r\n\
DTEND;TZID=America/New_York:20240115T100000\r\n\
SUMMARY:Tz test\r\n\
END:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert_eq!(e.dtstart.utc, utc(2024, 1, 15, 14, 0, 0));
        assert_eq!(e.dtstart.tzid.as_deref(), Some("America/New_York"));
    }

    #[test]
    fn unrecognized_tzid_is_an_explicit_unsupported_timezone_error() {
        let ics =
            "BEGIN:VEVENT\r\nUID:x\r\nDTSTART;TZID=Bogus/Zone:20240115T090000\r\nEND:VEVENT\r\n";
        let err = parse_vevents(ics).unwrap_err();
        assert_eq!(
            err,
            CalendarError::UnsupportedTimezone("Bogus/Zone".to_string())
        );
    }

    #[test]
    fn all_day_event_value_date_has_one_day_default_duration() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20240301\r\nSUMMARY:Day off\r\nEND:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert!(e.dtstart.all_day);
        assert_eq!(e.dtstart.utc, utc(2024, 3, 1, 0, 0, 0));
        assert_eq!(e.dtend.utc, utc(2024, 3, 2, 0, 0, 0));
        assert!(e.dtend.all_day);
    }

    #[test]
    fn duration_property_computes_dtend() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240115T090000Z\r\nDURATION:PT1H30M\r\nEND:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert_eq!(e.dtend.utc, utc(2024, 1, 15, 10, 30, 0));
    }

    #[test]
    fn parses_organizer_and_multiple_attendees_with_params() {
        let ics = "BEGIN:VEVENT\r\n\
UID:x\r\n\
DTSTART:20240115T090000Z\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;CN=Bob;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:bob@example.com\r\n\
ATTENDEE;CN=Carol;PARTSTAT=NEEDS-ACTION:mailto:carol@example.com\r\n\
END:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert_eq!(e.organizer.as_deref(), Some("alice@example.com"));
        assert_eq!(e.attendees.len(), 2);
        assert_eq!(e.attendees[0].email, "bob@example.com");
        assert_eq!(e.attendees[0].role.as_deref(), Some("REQ-PARTICIPANT"));
        assert_eq!(e.attendees[1].partstat.as_deref(), Some("NEEDS-ACTION"));
    }

    #[test]
    fn preserves_rrule_verbatim_and_describes_it() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240101T090000Z\r\nRRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=5\r\nEND:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert_eq!(e.rrule.as_deref(), Some("FREQ=WEEKLY;INTERVAL=2;COUNT=5"));
        assert!(e.is_recurring());
        assert_eq!(
            describe_rrule(e.rrule.as_ref().unwrap()),
            "Repeats every 2 weeks, 5 times"
        );
    }

    #[test]
    fn parses_multiple_exdate_values_across_multiple_lines() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240101T090000Z\r\nRRULE:FREQ=DAILY;COUNT=10\r\nEXDATE:20240103T090000Z,20240104T090000Z\r\nEXDATE:20240106T090000Z\r\nEND:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert_eq!(
            e.exdates,
            vec![
                utc(2024, 1, 3, 9, 0, 0),
                utc(2024, 1, 4, 9, 0, 0),
                utc(2024, 1, 6, 9, 0, 0),
            ]
        );
    }

    #[test]
    fn parses_exdate_with_value_date_and_tzid() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20240101\r\nRRULE:FREQ=DAILY;COUNT=5\r\nEXDATE;VALUE=DATE:20240103\r\nEND:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert_eq!(e.exdates, vec![utc(2024, 1, 3, 0, 0, 0)]);
    }

    #[test]
    fn validate_rrule_rejects_missing_freq_and_garbage() {
        assert!(validate_rrule("INTERVAL=2").is_err());
        assert!(validate_rrule("FREQ=BOGUS").is_err());
        assert!(validate_rrule("").is_err());
        assert!(validate_rrule("FREQ=WEEKLY").is_ok());
    }

    #[test]
    fn parses_valarm_relative_trigger_before_start() {
        let ics = "BEGIN:VEVENT\r\n\
UID:x\r\n\
DTSTART:20240115T090000Z\r\n\
DTEND:20240115T100000Z\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
TRIGGER:-PT15M\r\n\
DESCRIPTION:Reminder\r\n\
END:VALARM\r\n\
END:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert_eq!(e.alarms.len(), 1);
        let fire = e.alarms[0].trigger.fire_time(e.dtstart.utc, e.dtend.utc);
        assert_eq!(fire, utc(2024, 1, 15, 8, 45, 0));
    }

    #[test]
    fn parses_valarm_trigger_related_to_end() {
        let ics = "BEGIN:VEVENT\r\n\
UID:x\r\n\
DTSTART:20240115T090000Z\r\n\
DTEND:20240115T100000Z\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
TRIGGER;RELATED=END:PT5M\r\n\
END:VALARM\r\n\
END:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        let fire = e.alarms[0].trigger.fire_time(e.dtstart.utc, e.dtend.utc);
        assert_eq!(fire, utc(2024, 1, 15, 10, 5, 0));
    }

    #[test]
    fn parses_absolute_valarm_trigger() {
        let ics = "BEGIN:VEVENT\r\n\
UID:x\r\n\
DTSTART:20240115T090000Z\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
TRIGGER;VALUE=DATE-TIME:20240115T083000Z\r\n\
END:VALARM\r\n\
END:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        match &e.alarms[0].trigger {
            AlarmTrigger::Absolute(t) => assert_eq!(*t, utc(2024, 1, 15, 8, 30, 0)),
            other => panic!("expected Absolute trigger, got {:?}", other),
        }
    }

    #[test]
    fn invalid_duration_syntax_is_an_explicit_error() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240115T090000Z\r\nDURATION:garbage\r\nEND:VEVENT\r\n";
        let err = parse_vevents(ics).unwrap_err();
        assert!(matches!(err, CalendarError::InvalidDuration(_)));
    }

    #[test]
    fn text_escaping_round_trips_through_summary() {
        let original = "Line one\nLine two; with, punctuation \\ backslash";
        let escaped = escape_text(original);
        assert_eq!(unescape_text(&escaped), original);
    }

    #[test]
    fn to_new_ics_round_trips_through_parser() {
        let ev = VEvent {
            uid: "new-1".to_string(),
            summary: "Standup".to_string(),
            description: "Daily, sync".to_string(),
            location: "Room A".to_string(),
            status: EventStatus::Tentative,
            organizer: None,
            attendees: vec![],
            dtstart: EventTime::utc(utc(2024, 2, 1, 9, 0, 0)),
            dtend: EventTime::utc(utc(2024, 2, 1, 9, 15, 0)),
            rrule: None,
            exdates: vec![],
            alarms: vec![],
            sequence: 0,
            dtstamp: None,
            raw: None,
        };
        let ics = ev.to_new_ics(utc(2024, 2, 1, 0, 0, 0));
        let parsed = parse_vevents(&ics).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].uid, "new-1");
        assert_eq!(parsed[0].summary, "Standup");
        assert_eq!(parsed[0].description, "Daily, sync");
        assert_eq!(parsed[0].status, EventStatus::Tentative);
    }

    #[test]
    fn edit_patches_raw_block_and_preserves_unmodeled_properties() {
        let ics = "BEGIN:VEVENT\r\n\
UID:keep-me\r\n\
DTSTART:20240115T090000Z\r\n\
DTEND:20240115T100000Z\r\n\
SUMMARY:Old title\r\n\
CATEGORIES:WORK,IMPORTANT\r\n\
X-CUSTOM-PROP:do-not-drop-me\r\n\
SEQUENCE:2\r\n\
END:VEVENT\r\n";
        let event = &parse_vevents(ics).unwrap()[0];
        let edits = EventEdits {
            summary: Some("New title".to_string()),
            ..Default::default()
        };
        let patched = event.to_ics(&edits, utc(2024, 1, 16, 0, 0, 0)).unwrap();
        assert!(patched.contains("SUMMARY:New title"));
        assert!(!patched.contains("Old title"));
        assert!(patched.contains("CATEGORIES:WORK,IMPORTANT"));
        assert!(patched.contains("X-CUSTOM-PROP:do-not-drop-me"));
        assert!(patched.contains("SEQUENCE:3"));
        assert!(patched.contains("UID:keep-me"));

        // Re-parsing the patched text must still work and reflect the edit.
        let reparsed = &parse_vevents(&patched).unwrap()[0];
        assert_eq!(reparsed.summary, "New title");
        assert_eq!(reparsed.sequence, 3);
    }

    #[test]
    fn edit_with_no_changes_does_not_bump_sequence() {
        let ics =
            "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240115T090000Z\r\nSEQUENCE:1\r\nEND:VEVENT\r\n";
        let event = &parse_vevents(ics).unwrap()[0];
        let patched = event
            .to_ics(&EventEdits::default(), utc(2024, 1, 16, 0, 0, 0))
            .unwrap();
        assert!(patched.contains("SEQUENCE:1"));
    }

    #[test]
    fn floating_time_without_tzid_or_z_uses_local_timezone() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240115T090000\r\nEND:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        // Just assert it parses to *some* concrete UTC instant without
        // erroring — the exact offset depends on the test machine's local
        // timezone, which is the documented behavior.
        assert!(e.dtstart.tzid.is_none());
        assert!(!e.dtstart.all_day);
    }

    #[test]
    fn cancelled_status_round_trips() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240115T090000Z\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n";
        let e = &parse_vevents(ics).unwrap()[0];
        assert_eq!(e.status, EventStatus::Cancelled);
    }

    #[test]
    fn multiple_vevents_in_one_document_all_parse() {
        let ics = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\nUID:one\r\nDTSTART:20240115T090000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:two\r\nDTSTART:20240116T090000Z\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_vevents(ics).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].uid, "one");
        assert_eq!(events[1].uid, "two");
    }

    #[test]
    fn ical_duration_format_round_trips() {
        for s in [
            "PT15M",
            "-PT15M",
            "P1DT2H3M4S",
            "PT0S",
            "P2W".replacen("2W", "14D", 1).as_str(),
        ] {
            let d = parse_ical_duration(s).unwrap();
            let rendered = format_ical_duration(d);
            let reparsed = parse_ical_duration(&rendered).unwrap();
            assert_eq!(d, reparsed, "duration {} -> {} -> mismatch", s, rendered);
        }
    }

    #[test]
    fn weeks_in_duration_convert_to_seconds_correctly() {
        let d = parse_ical_duration("P2W").unwrap();
        assert_eq!(d, Duration::days(14));
    }

    fn recurring_event(dtstart: DateTime<Utc>, dtend: DateTime<Utc>, rrule: &str) -> VEvent {
        VEvent {
            uid: "recurring-1".to_string(),
            summary: "Standup".to_string(),
            description: String::new(),
            location: String::new(),
            status: EventStatus::Confirmed,
            organizer: None,
            attendees: vec![],
            dtstart: EventTime::utc(dtstart),
            dtend: EventTime::utc(dtend),
            rrule: Some(rrule.to_string()),
            exdates: vec![],
            alarms: vec![],
            sequence: 0,
            dtstamp: None,
            raw: None,
        }
    }

    #[test]
    fn expand_occurrences_returns_single_pair_for_non_recurring_event() {
        let ev = recurring_event(
            utc(2024, 2, 1, 9, 0, 0),
            utc(2024, 2, 1, 9, 30, 0),
            "FREQ=DAILY;COUNT=1",
        );
        let mut ev = ev;
        ev.rrule = None;
        let occurrences =
            expand_occurrences(&ev, utc(2024, 1, 1, 0, 0, 0), utc(2024, 12, 31, 0, 0, 0));
        assert_eq!(
            occurrences,
            vec![(utc(2024, 2, 1, 9, 0, 0), utc(2024, 2, 1, 9, 30, 0))]
        );

        // Outside the window: not returned.
        let none = expand_occurrences(&ev, utc(2025, 1, 1, 0, 0, 0), utc(2025, 2, 1, 0, 0, 0));
        assert!(none.is_empty());
    }

    #[test]
    fn expand_occurrences_expands_weekly_rrule_across_window() {
        // Every Tuesday, starting Tue 2024-01-02, 09:00-09:30 UTC.
        let ev = recurring_event(
            utc(2024, 1, 2, 9, 0, 0),
            utc(2024, 1, 2, 9, 30, 0),
            "FREQ=WEEKLY;BYDAY=TU",
        );
        let occurrences =
            expand_occurrences(&ev, utc(2024, 1, 1, 0, 0, 0), utc(2024, 2, 1, 0, 0, 0));
        // Tuesdays in January 2024: 2, 9, 16, 23, 30.
        assert_eq!(occurrences.len(), 5);
        assert_eq!(occurrences[0].0, utc(2024, 1, 2, 9, 0, 0));
        assert_eq!(occurrences[4].0, utc(2024, 1, 30, 9, 0, 0));
        for (s, e) in &occurrences {
            assert_eq!(*e - *s, Duration::minutes(30));
        }
    }

    #[test]
    fn expand_occurrences_honors_ordinal_byday_last_wednesday() {
        // Real-world pattern from user data: last Wednesday of every month.
        let ev = recurring_event(
            utc(2024, 1, 31, 18, 0, 0),
            utc(2024, 1, 31, 19, 0, 0),
            "FREQ=MONTHLY;BYDAY=-1WE",
        );
        let occurrences =
            expand_occurrences(&ev, utc(2024, 1, 1, 0, 0, 0), utc(2024, 4, 1, 0, 0, 0));
        // Last Wednesdays: Jan 31, Feb 28, Mar 27, 2024.
        assert_eq!(
            occurrences.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![
                utc(2024, 1, 31, 18, 0, 0),
                utc(2024, 2, 28, 18, 0, 0),
                utc(2024, 3, 27, 18, 0, 0),
            ]
        );
    }

    #[test]
    fn expand_occurrences_respects_until_and_count() {
        let ev = recurring_event(
            utc(2024, 1, 1, 9, 0, 0),
            utc(2024, 1, 1, 9, 15, 0),
            "FREQ=DAILY;COUNT=3",
        );
        let occurrences =
            expand_occurrences(&ev, utc(2024, 1, 1, 0, 0, 0), utc(2024, 12, 31, 0, 0, 0));
        assert_eq!(occurrences.len(), 3);
        assert_eq!(occurrences[2].0, utc(2024, 1, 3, 9, 0, 0));
    }

    #[test]
    fn expand_occurrences_excludes_exdates() {
        let mut ev = recurring_event(
            utc(2024, 1, 1, 9, 0, 0),
            utc(2024, 1, 1, 9, 15, 0),
            "FREQ=DAILY;COUNT=5",
        );
        ev.exdates = vec![utc(2024, 1, 3, 9, 0, 0)];
        let occurrences =
            expand_occurrences(&ev, utc(2024, 1, 1, 0, 0, 0), utc(2024, 12, 31, 0, 0, 0));
        assert_eq!(occurrences.len(), 4);
        assert!(
            occurrences
                .iter()
                .all(|(s, _)| *s != utc(2024, 1, 3, 9, 0, 0))
        );
    }

    #[test]
    fn expand_occurrences_includes_event_spanning_window_start() {
        // Weekly event starting before the window but overlapping into it.
        let ev = recurring_event(
            utc(2023, 12, 26, 23, 0, 0),
            utc(2023, 12, 27, 1, 0, 0),
            "FREQ=WEEKLY",
        );
        let occurrences =
            expand_occurrences(&ev, utc(2024, 1, 1, 0, 0, 0), utc(2024, 1, 3, 0, 0, 0));
        // The occurrence starting 2024-01-02 23:00 overlaps into Jan 3.
        assert!(
            occurrences
                .iter()
                .any(|(s, _)| *s == utc(2024, 1, 2, 23, 0, 0))
        );
    }

    #[test]
    fn expand_occurrences_falls_back_to_master_for_unparseable_rrule() {
        let ev = recurring_event(
            utc(2024, 1, 1, 9, 0, 0),
            utc(2024, 1, 1, 9, 15, 0),
            "NOT-A-VALID-RRULE",
        );
        let occurrences =
            expand_occurrences(&ev, utc(2024, 1, 1, 0, 0, 0), utc(2024, 1, 2, 0, 0, 0));
        assert_eq!(
            occurrences,
            vec![(utc(2024, 1, 1, 9, 0, 0), utc(2024, 1, 1, 9, 15, 0))]
        );
    }
}
