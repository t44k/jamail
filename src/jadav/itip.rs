//! The iTIP (RFC 5546) broker: pure rules that turn a committed client
//! write into the scheduling messages it implies, and that apply received
//! messages to stored objects. Transport (iMIP mail) lives in `imip`.
//!
//! ## Outbound (`process_client_change`)
//!
//! Only `Origin::Client` writes schedule anything — a mirror or inbound
//! write *is* the other side's message. Role is decided from the object:
//! organizer side when the `ORGANIZER` is one of our identities, attendee
//! side when one of our identities is an `ATTENDEE`.
//!
//! Organizer side, per recipient (every attendee except ourselves,
//! `SCHEDULE-AGENT=CLIENT|NONE` lines, rooms/resources and non-`mailto:`
//! addresses): a new object → `REQUEST`; a newly added attendee → `REQUEST`;
//! an existing attendee → `REQUEST` only on a *significant* change
//! (`DTSTART DTEND DURATION RRULE RDATE EXDATE STATUS SUMMARY LOCATION` by
//! default — not `DESCRIPTION`, so a typo fix does not re-invite everyone)
//! or when the client bumped `SEQUENCE`; a removed attendee, a
//! `STATUS:CANCELLED` or a `DELETE` → `CANCEL`. `SEQUENCE` is bumped in the
//! stored copy whenever a significant update goes out and the client did
//! not bump it itself.
//!
//! Attendee side: our own `PARTSTAT` changing to `ACCEPTED`/`TENTATIVE`/
//! `DECLINED` → `REPLY` (per component, so a single-instance answer carries
//! only that override); a `DELETE` of an object we merely attend → `REPLY`
//! `DECLINED` (RFC 6638 §3.2.2.2) unless it was already declined or
//! cancelled; anything else → nothing. Attendee-side writes never touch
//! `SEQUENCE`.
//!
//! Messages are built by *filtering raw lines*, never by re-rendering the
//! model: an Outlook `TZID`, an Apple `X-` parameter or a quoted `CN`
//! survives untouched. `SCHEDULE-*` parameters and `VALARM`s (private to a
//! recipient) are stripped from what goes out; `SCHEDULE-STATUS=1.0`
//! (pending) is stamped on each recipient's line in the copy we store, to
//! be upgraded by the delivery worker.
//!
//! ## Inbound (`apply_inbound`)
//!
//! `REQUEST`/`CANCEL` for a *mirrored* calendar are not applied while its
//! mirror is running (the provider is authoritative and the mirror brings
//! the event) — but they *are* applied when that mirror is stopped
//! (`InboundContext::mirror_offline`), since the mail is then the only
//! thing still arriving. For native calendars they always create/update
//! by `UID` with `SEQUENCE` then `DTSTAMP`
//! ordering, preserving our real `PARTSTAT` on a same-`SEQUENCE` refresh,
//! and a `CANCEL` sets `STATUS:CANCELLED` — never a delete, which a client
//! would see as us declining. `REPLY` patches that attendee's `PARTSTAT` on
//! an object we organise (mirrored or not), leaving `SEQUENCE` alone.

use crate::calendar::{
    self, EventStatus, EventTime, ICalendarDocument, VEvent, cal_address_email, line_has_param,
    parse_content_line, rewrite_block_lines, set_param_on_line, split_content_line,
};
use crate::jadav::store::{CalendarRow, Origin, Provider};
use chrono::{DateTime, Utc};

pub const DEFAULT_SIGNIFICANT_PROPERTIES: &[&str] = &[
    "DTSTART", "DTEND", "DURATION", "RRULE", "RDATE", "EXDATE", "STATUS", "SUMMARY", "LOCATION",
];

pub const DEFAULT_PRODID: &str = "-//jamail//jadav//EN";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItipMethod {
    Request,
    Cancel,
    Reply,
}

impl ItipMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            ItipMethod::Request => "REQUEST",
            ItipMethod::Cancel => "CANCEL",
            ItipMethod::Reply => "REPLY",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageKind {
    Invitation,
    UpdatedInvitation,
    Cancellation,
    Reply { partstat: String },
}

/// One message to one recipient.
#[derive(Clone, Debug)]
pub struct ItipMessage {
    pub method: ItipMethod,
    pub kind: MessageKind,
    /// Our identity the message is sent as (bare address).
    pub sender: String,
    pub recipient: String,
    pub recipient_cn: Option<String>,
    pub uid: String,
    pub sequence: i64,
    /// `Some` for an instance-only message.
    pub recurrence_id: Option<EventTime>,
    pub summary: String,
    pub dtstart: EventTime,
    pub dtend: EventTime,
    pub location: String,
    pub description: String,
    pub organizer: String,
    /// The METHOD-tagged, folded `VCALENDAR` to attach.
    pub ics: String,
}

pub struct SchedulingContext<'a> {
    /// The owning identity of the calendar the object lives in.
    pub calendar_identity: &'a str,
    /// Every identity of the principal (lower-cased).
    pub all_identities: &'a [String],
    pub significant_properties: &'a [String],
    pub prodid: &'a str,
    pub now: DateTime<Utc>,
}

/// A committed (or about-to-be-committed) write.
pub struct ObjectChange {
    pub old: Option<ICalendarDocument>,
    pub new: Option<ICalendarDocument>,
    pub origin: Origin,
}

#[derive(Default, Debug)]
pub struct BrokerOutput {
    pub messages: Vec<ItipMessage>,
    /// The document to store instead of `change.new` (bumped `SEQUENCE`,
    /// `SCHEDULE-STATUS` stamps). `None` = store as received.
    pub rewritten_new: Option<ICalendarDocument>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    Organizer(String),
    Attendee(String),
    None,
}

fn is_identity(addr: &str, identities: &[String]) -> bool {
    let a = addr.to_ascii_lowercase();
    identities.iter().any(|i| i.eq_ignore_ascii_case(&a))
}

/// Which of our identities this object concerns, and how.
pub fn role_of(doc: &ICalendarDocument, ctx: &SchedulingContext<'_>) -> Role {
    let Some(master) = doc.master() else {
        return Role::None;
    };
    if let Some(org) = &master.organizer
        && is_identity(org, ctx.all_identities)
    {
        return Role::Organizer(org.to_ascii_lowercase());
    }
    let attendee_ids: Vec<String> = doc
        .events
        .iter()
        .flat_map(|e| e.attendees.iter().map(|a| a.email.to_ascii_lowercase()))
        .collect();
    if attendee_ids
        .iter()
        .any(|a| a == &ctx.calendar_identity.to_ascii_lowercase())
    {
        return Role::Attendee(ctx.calendar_identity.to_ascii_lowercase());
    }
    for id in ctx.all_identities {
        if attendee_ids.iter().any(|a| a == &id.to_ascii_lowercase()) {
            return Role::Attendee(id.to_ascii_lowercase());
        }
    }
    Role::None
}

/// Raw `ATTENDEE`/`ORGANIZER` line for `email` in a component.
fn find_line(ev: &VEvent, name: &str, email: &str) -> Option<String> {
    let raw = ev.raw.as_deref()?;
    let me = email.to_ascii_lowercase();
    calendar::raw_property_lines(raw, &[name])
        .into_iter()
        .find(|l| split_content_line(l).is_some_and(|(_, _, v)| cal_address_email(&v) == me))
}

fn schedule_agent_is_client(line: &str) -> bool {
    split_content_line(line).is_some_and(|(_, params, _)| {
        params.iter().any(|p| {
            p.split_once('=').is_some_and(|(k, v)| {
                k.eq_ignore_ascii_case("SCHEDULE-AGENT")
                    && (v.eq_ignore_ascii_case("CLIENT") || v.eq_ignore_ascii_case("NONE"))
            })
        })
    })
}

fn cutype_is_resource(line: &str) -> bool {
    split_content_line(line).is_some_and(|(_, params, _)| {
        params.iter().any(|p| {
            p.split_once('=').is_some_and(|(k, v)| {
                k.eq_ignore_ascii_case("CUTYPE")
                    && (v.eq_ignore_ascii_case("ROOM") || v.eq_ignore_ascii_case("RESOURCE"))
            })
        })
    })
}

fn cn_of(line: &str) -> Option<String> {
    split_content_line(line).and_then(|(_, params, _)| {
        params.iter().find_map(|p| {
            p.split_once('=')
                .filter(|(k, _)| k.eq_ignore_ascii_case("CN"))
                .map(|(_, v)| v.trim_matches('"').to_string())
        })
    })
}

fn force_send(line: &str, method: &str) -> bool {
    split_content_line(line).is_some_and(|(_, params, _)| {
        params.iter().any(|p| {
            p.split_once('=').is_some_and(|(k, v)| {
                k.eq_ignore_ascii_case("SCHEDULE-FORCE-SEND") && v.eq_ignore_ascii_case(method)
            })
        })
    })
}

/// Attendees of a document that should receive scheduling messages from
/// organizer `sender`: `(email, cn, present-in-master)` — deduplicated
/// across components.
fn recipients(doc: &ICalendarDocument, sender: &str) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for ev in &doc.events {
        let raw = ev.raw.as_deref().unwrap_or("");
        for line in calendar::raw_property_lines(raw, &["ATTENDEE"]) {
            let Some((_, _, value)) = split_content_line(&line) else {
                continue;
            };
            let lowered = value.trim().to_ascii_lowercase();
            if !lowered.starts_with("mailto:") {
                continue;
            }
            let email = cal_address_email(&value);
            if email.is_empty() || email == sender.to_ascii_lowercase() {
                continue;
            }
            if schedule_agent_is_client(&line) || cutype_is_resource(&line) {
                continue;
            }
            if !out.iter().any(|(e, _)| *e == email) {
                out.push((email, cn_of(&line)));
            }
        }
    }
    out
}

fn rid_key(rid: Option<&EventTime>) -> String {
    match rid {
        None => "master".to_string(),
        Some(t) => format!("{}|{}", t.utc.timestamp(), t.all_day),
    }
}

/// Values of a property in a component (raw, upper-cased name match),
/// with date properties normalised to their UTC instant.
fn prop_values(ev: &VEvent, name: &str) -> Vec<String> {
    let raw = ev.raw.as_deref().unwrap_or("");
    calendar::raw_property_lines(raw, &[name])
        .into_iter()
        .filter_map(|l| {
            let cl = parse_content_line(&l)?;
            let normalised = match name {
                "DTSTART" | "DTEND" | "RECURRENCE-ID" | "EXDATE" | "RDATE" => cl
                    .value
                    .split(',')
                    .map(|v| {
                        calendar::parse_property_time(&format!(
                            "{}{}:{}",
                            cl.name,
                            cl.params
                                .iter()
                                .map(|(k, v)| format!(";{}={}", k, v))
                                .collect::<String>(),
                            v
                        ))
                        .map(|t| format!("{}|{}", t.utc.timestamp(), t.all_day))
                        .unwrap_or_else(|_| v.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                "SUMMARY" | "LOCATION" | "DESCRIPTION" => calendar::unescape_text(cl.value.trim()),
                _ => cl.value.trim().to_ascii_uppercase(),
            };
            Some(normalised)
        })
        .collect()
}

/// Whether anything an attendee needs to know changed between two
/// versions: compared per component (by `RECURRENCE-ID`), an override
/// added or removed counts.
pub fn is_significant_change(
    old: &ICalendarDocument,
    new: &ICalendarDocument,
    props: &[String],
) -> bool {
    let key = |e: &VEvent| rid_key(e.recurrence_id.as_ref());
    let old_keys: Vec<String> = old.events.iter().map(key).collect();
    let new_keys: Vec<String> = new.events.iter().map(key).collect();
    if old_keys.len() != new_keys.len() || old_keys.iter().any(|k| !new_keys.contains(k)) {
        return true;
    }
    for ne in &new.events {
        let k = key(ne);
        let Some(oe) = old.events.iter().find(|e| key(e) == k) else {
            return true;
        };
        for p in props {
            let name = p.to_ascii_uppercase();
            let mut a = prop_values(oe, &name);
            let mut b = prop_values(ne, &name);
            a.sort();
            b.sort();
            if a != b {
                return true;
            }
        }
    }
    false
}

/// A component reduced to what a message needs: keep `keep` properties,
/// only `keep_attendee`'s `ATTENDEE` line (or all attendees when `None`),
/// drop `VALARM`s and `SCHEDULE-*` parameters, then `add` extra lines.
fn filter_component(
    raw: &str,
    keep: Option<&[&str]>,
    keep_attendee: Option<&str>,
    add: &[String],
) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut skipping_alarm = 0i32;
    let mut depth = 0i32;
    let lines = calendar::unfold(raw);
    let end_index = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate() {
        if calendar::starts_with_ignore_case(line, "BEGIN:VALARM") {
            skipping_alarm += 1;
            continue;
        }
        if skipping_alarm > 0 {
            if calendar::starts_with_ignore_case(line, "END:VALARM") {
                skipping_alarm -= 1;
            }
            continue;
        }
        if calendar::starts_with_ignore_case(line, "BEGIN:") {
            depth += 1;
            out.push(line.clone());
            continue;
        }
        if calendar::starts_with_ignore_case(line, "END:") {
            if i == end_index {
                for a in add {
                    out.push(a.clone());
                }
            }
            depth -= 1;
            out.push(line.clone());
            continue;
        }
        if depth != 1 {
            out.push(line.clone());
            continue;
        }
        let Some(cl) = parse_content_line(line) else {
            continue;
        };
        if let Some(k) = keep
            && !k.iter().any(|n| n.eq_ignore_ascii_case(&cl.name))
        {
            continue;
        }
        if cl.name == "ATTENDEE"
            && let Some(who) = keep_attendee
            && cal_address_email(&cl.value) != who.to_ascii_lowercase()
        {
            continue;
        }
        if cl.name == "ATTENDEE" || cl.name == "ORGANIZER" {
            let mut l = line.clone();
            for p in ["SCHEDULE-STATUS", "SCHEDULE-AGENT", "SCHEDULE-FORCE-SEND"] {
                if line_has_param(&l, p) {
                    l = set_param_on_line(&l, p, None);
                }
            }
            out.push(l);
            continue;
        }
        out.push(line.clone());
    }
    out.join("\r\n")
}

fn message_doc(
    source: &ICalendarDocument,
    method: ItipMethod,
    components: Vec<String>,
    prodid: &str,
) -> String {
    // Carry the source's VTIMEZONEs and generate any the components
    // reference but the source lacked (a recipient's Outlook wants them).
    let mut timezones = source.timezones.clone();
    let mut years: Vec<i32> = Vec::new();
    let mut tzids: Vec<String> = Vec::new();
    for comp in &components {
        for line in calendar::unfold(comp) {
            if let Some(cl) = parse_content_line(&line) {
                if let Some(tz) = cl.param("TZID")
                    && !tzids.iter().any(|t| t == tz)
                {
                    tzids.push(tz.to_string());
                }
                if matches!(cl.name.as_str(), "DTSTART" | "DTEND" | "RECURRENCE-ID")
                    && let Some(y) = cl.value.get(..4).and_then(|v| v.parse::<i32>().ok())
                {
                    years.push(y);
                }
            }
        }
    }
    let (from, to) = match (years.iter().min(), years.iter().max()) {
        (Some(a), Some(b)) => (*a - 1, *b + 2),
        _ => (2020, 2030),
    };
    for tz in tzids {
        let present = timezones.iter().any(|block| {
            calendar::unfold(block).iter().any(|l| {
                parse_content_line(l).is_some_and(|cl| cl.name == "TZID" && cl.value.trim() == tz)
            })
        });
        if !present && let Some(block) = crate::jadav::mirror::render_vtimezone(&tz, from, to) {
            timezones.push(block.trim_end_matches(['\r', '\n']).to_string());
        }
    }
    let mut doc = ICalendarDocument {
        method: Some(method.as_str().to_string()),
        prodid: Some(prodid.to_string()),
        other_properties: vec!["CALSCALE:GREGORIAN".to_string()],
        timezones,
        events: Vec::new(),
        other_components: Vec::new(),
    };
    // Components are pre-rendered raw blocks; parse them back so `to_ics`
    // has `raw` to emit (they never fail: they were valid to begin with).
    let joined = format!(
        "BEGIN:VCALENDAR\r\n{}\r\n{}\r\nEND:VCALENDAR\r\n",
        doc.timezones.join("\r\n"),
        components.join("\r\n")
    );
    if let Ok(parsed) = calendar::parse_document(&joined) {
        doc.events = parsed.events;
    }
    doc.to_ics()
}

fn stamp(now: DateTime<Utc>) -> String {
    format!("DTSTAMP:{}", now.format("%Y%m%dT%H%M%SZ"))
}

#[allow(clippy::too_many_arguments)]
fn build_message(
    method: ItipMethod,
    kind: MessageKind,
    sender: &str,
    recipient: &(String, Option<String>),
    doc: &ICalendarDocument,
    ev: &VEvent,
    sequence: i64,
    recurrence_id: Option<EventTime>,
    ics: String,
) -> ItipMessage {
    ItipMessage {
        method,
        kind,
        sender: sender.to_string(),
        recipient: recipient.0.clone(),
        recipient_cn: recipient.1.clone(),
        uid: ev.uid.clone(),
        sequence,
        recurrence_id,
        summary: ev.summary.clone(),
        dtstart: ev.dtstart.clone(),
        dtend: ev.dtend.clone(),
        location: ev.location.clone(),
        description: ev.description.clone(),
        organizer: ev.organizer.clone().unwrap_or_default(),
        ics: {
            let _ = doc;
            ics
        },
    }
}

/// `METHOD:REQUEST` for one attendee: the master plus every override the
/// attendee is on (or only the overrides they are on, when absent from the
/// master), `SCHEDULE-*` stripped, alarms removed, `DTSTAMP` refreshed.
pub fn build_request_ics(
    doc: &ICalendarDocument,
    attendee: &str,
    sequence: i64,
    ctx: &SchedulingContext<'_>,
) -> (String, bool) {
    let in_master = doc.master().is_some_and(|m| {
        m.attendees
            .iter()
            .any(|a| a.email.eq_ignore_ascii_case(attendee))
    });
    let mut components = Vec::new();
    let mut instance_only = !in_master;
    for ev in &doc.events {
        let is_master = !calendar::is_recurrence_override(ev);
        let on_it = ev
            .attendees
            .iter()
            .any(|a| a.email.eq_ignore_ascii_case(attendee));
        if (is_master && in_master) || (!is_master && on_it) {
            let raw = ev.raw.clone().unwrap_or_default();
            let raw = calendar::replace_block_property(
                &raw,
                "SEQUENCE",
                Some(&format!("SEQUENCE:{}", sequence)),
            );
            let raw = calendar::replace_block_property(&raw, "DTSTAMP", Some(&stamp(ctx.now)));
            components.push(filter_component(&raw, None, None, &[]));
        }
    }
    if components.is_empty() {
        instance_only = false;
    }
    (
        message_doc(doc, ItipMethod::Request, components, ctx.prodid),
        instance_only,
    )
}

/// `METHOD:CANCEL` for one attendee.
pub fn build_cancel_ics(
    doc: &ICalendarDocument,
    attendee: &str,
    sequence: i64,
    only_rid: Option<&EventTime>,
    ctx: &SchedulingContext<'_>,
) -> String {
    const KEEP: &[&str] = &[
        "UID",
        "SEQUENCE",
        "DTSTAMP",
        "ORGANIZER",
        "ATTENDEE",
        "DTSTART",
        "DTEND",
        "DURATION",
        "SUMMARY",
        "RECURRENCE-ID",
        "STATUS",
    ];
    let mut components = Vec::new();
    for ev in &doc.events {
        if let Some(rid) = only_rid {
            let matches = ev
                .recurrence_id
                .as_ref()
                .is_some_and(|r| r.utc == rid.utc && r.all_day == rid.all_day);
            if !matches {
                continue;
            }
        }
        let raw = ev.raw.clone().unwrap_or_default();
        let raw = calendar::replace_block_property(
            &raw,
            "SEQUENCE",
            Some(&format!("SEQUENCE:{}", sequence)),
        );
        let raw = calendar::replace_block_property(&raw, "DTSTAMP", Some(&stamp(ctx.now)));
        let raw = calendar::replace_block_property(&raw, "STATUS", Some("STATUS:CANCELLED"));
        components.push(filter_component(&raw, Some(KEEP), Some(attendee), &[]));
    }
    message_doc(doc, ItipMethod::Cancel, components, ctx.prodid)
}

/// `METHOD:REPLY` from `identity` for the given components (`None` =
/// master) with `partstat` on our line.
pub fn build_reply_ics(
    doc: &ICalendarDocument,
    identity: &str,
    components: &[Option<EventTime>],
    partstat: &str,
    ctx: &SchedulingContext<'_>,
) -> String {
    const KEEP: &[&str] = &[
        "UID",
        "SEQUENCE",
        "DTSTAMP",
        "ORGANIZER",
        "ATTENDEE",
        "DTSTART",
        "DTEND",
        "DURATION",
        "SUMMARY",
        "RECURRENCE-ID",
    ];
    let mut out = Vec::new();
    for want in components {
        let Some(idx) = doc.component_index(want.as_ref()) else {
            continue;
        };
        let ev = &doc.events[idx];
        let raw = ev.raw.clone().unwrap_or_default();
        let raw = calendar::replace_block_property(&raw, "DTSTAMP", Some(&stamp(ctx.now)));
        let me = identity.to_ascii_lowercase();
        let (raw, _) = rewrite_block_lines(&raw, |name, line| {
            if name != "ATTENDEE" {
                return None;
            }
            let (_, _, v) = split_content_line(line)?;
            if cal_address_email(&v) != me {
                return None;
            }
            let l = set_param_on_line(line, "PARTSTAT", Some(&partstat.to_ascii_uppercase()));
            Some(set_param_on_line(&l, "RSVP", None))
        });
        out.push(filter_component(
            &raw,
            Some(KEEP),
            Some(identity),
            &["REQUEST-STATUS:2.0;Success".to_string()],
        ));
    }
    message_doc(doc, ItipMethod::Reply, out, ctx.prodid)
}

fn own_partstat(ev: &VEvent, identity: &str) -> Option<String> {
    ev.attendees
        .iter()
        .find(|a| a.email.eq_ignore_ascii_case(identity))
        .map(|a| {
            a.partstat
                .clone()
                .unwrap_or_else(|| "NEEDS-ACTION".to_string())
                .to_ascii_uppercase()
        })
}

fn is_cancelled(doc: &ICalendarDocument) -> bool {
    doc.master()
        .is_some_and(|m| m.status == EventStatus::Cancelled)
}

/// The broker: what a client write means in scheduling terms.
pub fn process_client_change(change: &ObjectChange, ctx: &SchedulingContext<'_>) -> BrokerOutput {
    let mut out = BrokerOutput::default();
    if change.origin != Origin::Client {
        return out;
    }
    let doc_for_role = change.new.as_ref().or(change.old.as_ref());
    let Some(role_doc) = doc_for_role else {
        return out;
    };
    match role_of(role_doc, ctx) {
        Role::None => out,
        Role::Organizer(sender) => {
            organizer_side(change, &sender, ctx, &mut out);
            out
        }
        Role::Attendee(identity) => {
            attendee_side(change, &identity, ctx, &mut out);
            out
        }
    }
}

fn organizer_side(
    change: &ObjectChange,
    sender: &str,
    ctx: &SchedulingContext<'_>,
    out: &mut BrokerOutput,
) {
    match (&change.old, &change.new) {
        (None, None) => {}
        (Some(old), None) => {
            if is_cancelled(old) {
                return;
            }
            let Some(master) = old.master() else { return };
            let seq = master.sequence + 1;
            for r in recipients(old, sender) {
                let ics = build_cancel_ics(old, &r.0, seq, None, ctx);
                out.messages.push(build_message(
                    ItipMethod::Cancel,
                    MessageKind::Cancellation,
                    sender,
                    &r,
                    old,
                    master,
                    seq,
                    None,
                    ics,
                ));
            }
        }
        (None, Some(new)) => {
            let Some(master) = new.master() else { return };
            if is_cancelled(new) {
                return;
            }
            let mut stamped = new.clone();
            let seq = master.sequence;
            for r in recipients(new, sender) {
                let (ics, instance_only) = build_request_ics(new, &r.0, seq, ctx);
                let rid = if instance_only {
                    first_override_rid(new, &r.0)
                } else {
                    None
                };
                out.messages.push(build_message(
                    ItipMethod::Request,
                    MessageKind::Invitation,
                    sender,
                    &r,
                    new,
                    master,
                    seq,
                    rid,
                    ics,
                ));
                let _ = stamped.set_schedule_status(&r.0, "1.0", None);
            }
            if !out.messages.is_empty() {
                out.rewritten_new = Some(stamped);
            }
        }
        (Some(old), Some(new)) => {
            let Some(new_master) = new.master() else {
                return;
            };
            let old_seq = old.master().map(|m| m.sequence).unwrap_or(0);
            let client_bumped = new_master.sequence > old_seq;
            let significant = is_significant_change(old, new, ctx.significant_properties);
            let became_cancelled = is_cancelled(new) && !is_cancelled(old);
            let old_recipients = recipients(old, sender);
            let new_recipients = recipients(new, sender);
            let mut stamped = new.clone();
            let mut seq = new_master.sequence;
            let needs_bump = (significant || became_cancelled) && !client_bumped;
            if needs_bump {
                seq = old_seq + 1;
                let _ = stamped.set_sequence(seq);
            }
            // Removed attendees (and everyone, on cancellation) get CANCEL.
            for r in &old_recipients {
                let still_there = new_recipients.iter().any(|n| n.0 == r.0);
                if became_cancelled || !still_there {
                    let ics = build_cancel_ics(
                        if became_cancelled { new } else { old },
                        &r.0,
                        seq.max(old_seq),
                        None,
                        ctx,
                    );
                    let master = if became_cancelled {
                        new_master
                    } else {
                        old.master().unwrap_or(new_master)
                    };
                    out.messages.push(build_message(
                        ItipMethod::Cancel,
                        MessageKind::Cancellation,
                        sender,
                        r,
                        new,
                        master,
                        seq.max(old_seq),
                        None,
                        ics,
                    ));
                }
            }
            if became_cancelled {
                out.rewritten_new = Some(stamped);
                return;
            }
            for r in &new_recipients {
                let was_there = old_recipients.iter().any(|o| o.0 == r.0);
                let forced = find_line(new_master, "ATTENDEE", &r.0)
                    .is_some_and(|l| force_send(&l, "REQUEST"));
                let (kind, send) = if !was_there {
                    (MessageKind::Invitation, true)
                } else if significant || client_bumped || forced {
                    (MessageKind::UpdatedInvitation, true)
                } else {
                    (MessageKind::Invitation, false)
                };
                if !send {
                    continue;
                }
                let (ics, instance_only) = build_request_ics(new, &r.0, seq, ctx);
                let rid = if instance_only {
                    first_override_rid(new, &r.0)
                } else {
                    None
                };
                out.messages.push(build_message(
                    ItipMethod::Request,
                    kind,
                    sender,
                    r,
                    new,
                    new_master,
                    seq,
                    rid,
                    ics,
                ));
                let _ = stamped.set_schedule_status(&r.0, "1.0", None);
            }
            if !out.messages.is_empty() || needs_bump {
                out.rewritten_new = Some(stamped);
            }
        }
    }
}

fn first_override_rid(doc: &ICalendarDocument, attendee: &str) -> Option<EventTime> {
    doc.overrides()
        .find(|e| {
            e.attendees
                .iter()
                .any(|a| a.email.eq_ignore_ascii_case(attendee))
        })
        .and_then(|e| e.recurrence_id.clone())
}

fn attendee_side(
    change: &ObjectChange,
    identity: &str,
    ctx: &SchedulingContext<'_>,
    out: &mut BrokerOutput,
) {
    let agent_is_client = |doc: &ICalendarDocument| {
        doc.master().is_some_and(|m| {
            find_line(m, "ATTENDEE", identity).is_some_and(|l| schedule_agent_is_client(&l))
                || m.organizer
                    .as_deref()
                    .and_then(|o| find_line(m, "ORGANIZER", o))
                    .is_some_and(|l| schedule_agent_is_client(&l))
        })
    };
    match (&change.old, &change.new) {
        (Some(old), None) => {
            if is_cancelled(old) || agent_is_client(old) {
                return;
            }
            let Some(master) = old.master() else { return };
            if own_partstat(master, identity).as_deref() == Some("DECLINED") {
                return;
            }
            let Some(org) = master.organizer.clone() else {
                return;
            };
            let ics = build_reply_ics(old, identity, &[None], "DECLINED", ctx);
            out.messages.push(build_message(
                ItipMethod::Reply,
                MessageKind::Reply {
                    partstat: "DECLINED".to_string(),
                },
                identity,
                &(org.to_ascii_lowercase(), None),
                old,
                master,
                master.sequence,
                None,
                ics,
            ));
        }
        (Some(old), Some(new)) => {
            if agent_is_client(new) {
                return;
            }
            let Some(master) = new.master() else { return };
            let Some(org) = master.organizer.clone() else {
                return;
            };
            // Which components changed our PARTSTAT to a real answer?
            let mut changed: Vec<(Option<EventTime>, String)> = Vec::new();
            for ev in &new.events {
                let new_ps = own_partstat(ev, identity);
                let Some(new_ps) = new_ps else { continue };
                if !matches!(new_ps.as_str(), "ACCEPTED" | "TENTATIVE" | "DECLINED") {
                    continue;
                }
                let old_ps = old
                    .component_index(ev.recurrence_id.as_ref())
                    .and_then(|i| own_partstat(&old.events[i], identity));
                let forced =
                    find_line(ev, "ATTENDEE", identity).is_some_and(|l| force_send(&l, "REPLY"));
                if old_ps.as_deref() != Some(new_ps.as_str()) || forced {
                    changed.push((ev.recurrence_id.clone(), new_ps));
                }
            }
            if changed.is_empty() {
                return;
            }
            // One REPLY per distinct partstat value (usually one).
            let mut stamped = new.clone();
            let mut partstats: Vec<String> = changed.iter().map(|(_, p)| p.clone()).collect();
            partstats.sort();
            partstats.dedup();
            for ps in partstats {
                let comps: Vec<Option<EventTime>> = changed
                    .iter()
                    .filter(|(_, p)| *p == ps)
                    .map(|(r, _)| r.clone())
                    .collect();
                let ics = build_reply_ics(new, identity, &comps, &ps, ctx);
                let rid = if comps.len() == 1 {
                    comps[0].clone()
                } else {
                    None
                };
                out.messages.push(build_message(
                    ItipMethod::Reply,
                    MessageKind::Reply {
                        partstat: ps.clone(),
                    },
                    identity,
                    &(org.to_ascii_lowercase(), None),
                    new,
                    master,
                    master.sequence,
                    rid,
                    ics,
                ));
            }
            let _ = stamped.set_schedule_status(&org, "1.0", None);
            out.rewritten_new = Some(stamped);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------
// Inbound
// ---------------------------------------------------------------------

pub struct InboundContext<'a> {
    pub calendar: &'a CalendarRow,
    /// The identity the message was routed to.
    pub identity: &'a str,
    pub all_identities: &'a [String],
    pub stored: Option<&'a ICalendarDocument>,
    /// Also copy `REQUEST`/`CANCEL` into the schedule inbox for mirrored
    /// calendars (default off: iOS may then re-file the event elsewhere).
    pub inbox_for_mirrored: bool,
    /// The remote behind this mirrored calendar is not syncing (its OAuth
    /// authorization was revoked), so nothing is bringing the provider's
    /// changes in. Lifts the mirrored-calendar skip below — see
    /// [`apply_inbound`].
    pub mirror_offline: bool,
    pub now: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboundOutcome {
    Created,
    Updated,
    InstanceMerged,
    CancelledWhole,
    CancelledInstance,
    ReplyApplied { attendee: String, partstat: String },
    IgnoredStale,
    IgnoredDuplicate,
    IgnoredUnknownUid,
    IgnoredUnknownAttendee,
    IgnoredMethod(String),
    NotAppliedMirrored,
}

impl InboundOutcome {
    pub fn as_str(&self) -> String {
        match self {
            InboundOutcome::Created => "created".into(),
            InboundOutcome::Updated => "updated".into(),
            InboundOutcome::InstanceMerged => "instance_merged".into(),
            InboundOutcome::CancelledWhole => "cancelled".into(),
            InboundOutcome::CancelledInstance => "cancelled_instance".into(),
            InboundOutcome::ReplyApplied { attendee, partstat } => {
                format!("reply_applied:{}={}", attendee, partstat)
            }
            InboundOutcome::IgnoredStale => "ignored_stale".into(),
            InboundOutcome::IgnoredDuplicate => "ignored_duplicate".into(),
            InboundOutcome::IgnoredUnknownUid => "ignored_unknown_uid".into(),
            InboundOutcome::IgnoredUnknownAttendee => "ignored_unknown_attendee".into(),
            InboundOutcome::IgnoredMethod(m) => format!("ignored_method:{}", m),
            InboundOutcome::NotAppliedMirrored => "not_applied_mirrored".into(),
        }
    }
}

#[derive(Debug)]
pub struct InboundDecision {
    pub outcome: InboundOutcome,
    /// The document to store (committed with `Origin::Inbound`).
    pub write: Option<ICalendarDocument>,
    /// Also file a copy in the schedule inbox.
    pub inbox_copy: bool,
}

fn no_write(outcome: InboundOutcome) -> InboundDecision {
    InboundDecision {
        outcome,
        write: None,
        inbox_copy: false,
    }
}

fn strip_method(doc: &ICalendarDocument) -> ICalendarDocument {
    let mut d = doc.clone();
    d.method = None;
    d
}

/// Replace or append the override with `rid` in `target` with the block
/// from `source`.
fn merge_override(target: &mut ICalendarDocument, source_ev: &VEvent) {
    let rid = source_ev.recurrence_id.as_ref();
    match target.component_index(rid) {
        Some(i) if rid.is_some() => target.events[i] = source_ev.clone(),
        _ => target.events.push(source_ev.clone()),
    }
}

/// Apply a received iTIP message to the stored object (if any).
pub fn apply_inbound(incoming: &ICalendarDocument, ctx: &InboundContext<'_>) -> InboundDecision {
    let method = incoming.method.clone().unwrap_or_default();
    let Some(in_master) = incoming.master() else {
        return no_write(InboundOutcome::IgnoredMethod(method));
    };
    let mirrored = ctx.calendar.provider != Provider::Native;
    // A mirrored calendar ignores REQUEST/CANCEL because the provider is
    // authoritative and the mirror brings them — true only while the
    // mirror actually runs. With its remote stopped (a revoked OAuth
    // token), this message is the *only* live signal, and dropping it
    // leaves the calendar silently wrong until a human notices; a
    // cancelled meeting in particular keeps showing as if it were on.
    // Applying it converges on what the provider already holds, so the
    // mirror finds nothing to push back when it resumes, and remote-wins
    // reconciliation settles any difference.
    let skip_as_mirrored = mirrored && !ctx.mirror_offline;
    let native_inbox = !mirrored || ctx.inbox_for_mirrored;
    match method.as_str() {
        "REQUEST" => {
            if skip_as_mirrored {
                return no_write(InboundOutcome::NotAppliedMirrored);
            }
            let incoming_has_master = incoming
                .events
                .iter()
                .any(|e| !calendar::is_recurrence_override(e));
            let Some(stored) = ctx.stored else {
                return InboundDecision {
                    outcome: InboundOutcome::Created,
                    write: Some(strip_method(incoming)),
                    inbox_copy: native_inbox,
                };
            };
            let stored_master = stored.master();
            let stored_seq = stored_master.map(|m| m.sequence).unwrap_or(0);
            if !incoming_has_master {
                // Instance-only update: merge overrides into the stored series.
                let mut merged = stored.clone();
                for ev in incoming.overrides() {
                    merge_override(&mut merged, ev);
                }
                return InboundDecision {
                    outcome: InboundOutcome::InstanceMerged,
                    write: Some(merged),
                    inbox_copy: native_inbox,
                };
            }
            if in_master.sequence > stored_seq {
                return InboundDecision {
                    outcome: InboundOutcome::Updated,
                    write: Some(strip_method(incoming)),
                    inbox_copy: native_inbox,
                };
            }
            if in_master.sequence < stored_seq {
                return no_write(InboundOutcome::IgnoredStale);
            }
            let newer = match (in_master.dtstamp, stored_master.and_then(|m| m.dtstamp)) {
                (Some(a), Some(b)) => a > b,
                (Some(_), None) => true,
                _ => false,
            };
            if !newer {
                return no_write(InboundOutcome::IgnoredDuplicate);
            }
            // Same sequence, fresher copy: take it but keep our real answer.
            let mut doc = strip_method(incoming);
            for (i, ev) in stored.events.iter().enumerate() {
                let Some(ps) = own_partstat(ev, ctx.identity) else {
                    continue;
                };
                if matches!(ps.as_str(), "ACCEPTED" | "TENTATIVE" | "DECLINED") {
                    let rid = stored.events[i].recurrence_id.clone();
                    let _ = doc.set_attendee_partstat(ctx.identity, &ps, rid.as_ref());
                }
            }
            InboundDecision {
                outcome: InboundOutcome::Updated,
                write: Some(doc),
                inbox_copy: native_inbox,
            }
        }
        "CANCEL" => {
            if skip_as_mirrored {
                return no_write(InboundOutcome::NotAppliedMirrored);
            }
            let Some(stored) = ctx.stored else {
                return no_write(InboundOutcome::IgnoredUnknownUid);
            };
            let stored_seq = stored.master().map(|m| m.sequence).unwrap_or(0);
            if in_master.sequence < stored_seq {
                return no_write(InboundOutcome::IgnoredStale);
            }
            let whole = incoming
                .events
                .iter()
                .any(|e| !calendar::is_recurrence_override(e));
            let mut doc = stored.clone();
            if whole {
                let _ = doc.set_status(
                    EventStatus::Cancelled,
                    Some(in_master.sequence.max(stored_seq)),
                );
                let _ = doc.touch(ctx.now);
                return InboundDecision {
                    outcome: InboundOutcome::CancelledWhole,
                    write: Some(doc),
                    inbox_copy: native_inbox,
                };
            }
            for ev in incoming.overrides() {
                let rid = ev.recurrence_id.as_ref();
                match doc.component_index(rid) {
                    Some(i) if rid.is_some() => {
                        let raw = doc.events[i].raw.clone().unwrap_or_default();
                        let raw = calendar::replace_block_property(
                            &raw,
                            "STATUS",
                            Some("STATUS:CANCELLED"),
                        );
                        if let Ok(parsed) = calendar::parse_document(&format!(
                            "BEGIN:VCALENDAR\r\n{}\r\n{}\r\nEND:VCALENDAR\r\n",
                            doc.timezones.join("\r\n"),
                            raw
                        )) && let Some(ev2) = parsed.events.into_iter().next()
                        {
                            doc.events[i] = ev2;
                        }
                    }
                    _ => {
                        // No override stored: exclude the instance from the master.
                        if let (Some(rid), Some(mi)) = (rid, doc.component_index(None)) {
                            let master = &doc.events[mi];
                            let shaped = EventTime {
                                utc: rid.utc,
                                tzid: if master.dtstart.all_day {
                                    None
                                } else {
                                    master.dtstart.tzid.clone()
                                },
                                all_day: master.dtstart.all_day,
                            };
                            let exdate = calendar::render_time_property("EXDATE", &shaped);
                            let raw = master.raw.clone().unwrap_or_default();
                            let mut lines = calendar::unfold(&raw);
                            if let Some(pos) = lines
                                .iter()
                                .rposition(|l| calendar::starts_with_ignore_case(l, "END:VEVENT"))
                            {
                                lines.insert(pos, exdate);
                            }
                            if let Ok(parsed) = calendar::parse_document(&format!(
                                "BEGIN:VCALENDAR\r\n{}\r\n{}\r\nEND:VCALENDAR\r\n",
                                doc.timezones.join("\r\n"),
                                lines.join("\r\n")
                            )) && let Some(ev2) = parsed.events.into_iter().next()
                            {
                                doc.events[mi] = ev2;
                            }
                        }
                    }
                }
            }
            let _ = doc.touch(ctx.now);
            InboundDecision {
                outcome: InboundOutcome::CancelledInstance,
                write: Some(doc),
                inbox_copy: false,
            }
        }
        "REPLY" => {
            let Some(stored) = ctx.stored else {
                return no_write(InboundOutcome::IgnoredUnknownUid);
            };
            let organizer_is_us = stored
                .master()
                .and_then(|m| m.organizer.clone())
                .is_some_and(|o| is_identity(&o, ctx.all_identities));
            if !organizer_is_us {
                return no_write(InboundOutcome::IgnoredUnknownUid);
            }
            let stored_seq = stored.master().map(|m| m.sequence).unwrap_or(0);
            if in_master.sequence < stored_seq {
                return no_write(InboundOutcome::IgnoredStale);
            }
            let mut doc = stored.clone();
            let mut applied: Option<(String, String)> = None;
            let mut any_known = false;
            let mut all_equal = true;
            for ev in &incoming.events {
                for a in &ev.attendees {
                    let Some(ps) = a.partstat.clone() else {
                        continue;
                    };
                    let rid = ev.recurrence_id.as_ref();
                    let Some(idx) = doc.component_index(rid) else {
                        continue;
                    };
                    let known = doc.events[idx]
                        .attendees
                        .iter()
                        .any(|x| x.email.eq_ignore_ascii_case(&a.email));
                    if !known {
                        continue;
                    }
                    any_known = true;
                    let current = doc.events[idx]
                        .attendees
                        .iter()
                        .find(|x| x.email.eq_ignore_ascii_case(&a.email))
                        .and_then(|x| x.partstat.clone())
                        .unwrap_or_else(|| "NEEDS-ACTION".to_string());
                    if current.eq_ignore_ascii_case(&ps) {
                        continue;
                    }
                    all_equal = false;
                    if doc
                        .set_attendee_partstat(&a.email, &ps, rid)
                        .unwrap_or(false)
                    {
                        applied = Some((a.email.to_ascii_lowercase(), ps.to_ascii_uppercase()));
                    }
                }
            }
            if !any_known {
                return no_write(InboundOutcome::IgnoredUnknownAttendee);
            }
            if all_equal {
                return no_write(InboundOutcome::IgnoredDuplicate);
            }
            let _ = doc.touch(ctx.now);
            let (attendee, partstat) = applied.unwrap_or_default();
            InboundDecision {
                outcome: InboundOutcome::ReplyApplied { attendee, partstat },
                write: Some(doc),
                inbox_copy: false,
            }
        }
        other => no_write(InboundOutcome::IgnoredMethod(other.to_string())),
    }
}

/// Answer a `POST` to the schedule outbox: this server does no free-busy
/// lookups, so every recipient gets `3.7;Invalid calendar user` for a
/// `VFREEBUSY` request and `5.3;No scheduling support for user` otherwise.
pub fn handle_outbox_post(body: &str) -> (u16, String) {
    let doc = calendar::parse_document(body).ok();
    let is_freebusy = body.to_ascii_uppercase().contains("BEGIN:VFREEBUSY");
    let mut recipients: Vec<String> = Vec::new();
    for line in calendar::unfold(body) {
        if let Some(cl) = parse_content_line(&line)
            && cl.name == "ATTENDEE"
        {
            recipients.push(cl.value.trim().to_string());
        }
    }
    let _ = doc;
    let status = if is_freebusy {
        "3.7;Invalid calendar user"
    } else {
        "5.3;No scheduling support for user"
    };
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><C:schedule-response xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">",
    );
    for r in recipients {
        xml.push_str(&format!(
            "<C:response><C:recipient><D:href>{}</D:href></C:recipient><C:request-status>{}</C:request-status></C:response>",
            crate::jadav::xml::escape_text(&r),
            status
        ));
    }
    xml.push_str("</C:schedule-response>");
    (200, xml)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::parse_document;
    use crate::jadav::store::SendVia;

    const ORG: &str = "boss@example.com";
    const ME: &str = "me@example.com";

    fn ctx<'a>(identity: &'a str, ids: &'a [String], props: &'a [String]) -> SchedulingContext<'a> {
        SchedulingContext {
            calendar_identity: identity,
            all_identities: ids,
            significant_properties: props,
            prodid: DEFAULT_PRODID,
            now: Utc.with_ymd_and_hms(2024, 2, 1, 12, 0, 0).unwrap(),
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn props() -> Vec<String> {
        DEFAULT_SIGNIFICANT_PROPERTIES
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn doc(body: &str) -> ICalendarDocument {
        parse_document(&format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\n{}END:VCALENDAR\r\n",
            body
        ))
        .unwrap()
    }

    fn event(organizer: &str, attendees: &[(&str, &str)], extra: &str) -> String {
        let mut s = format!(
            "BEGIN:VEVENT\r\nUID:e1\r\nDTSTART;TZID=Europe/Budapest:20240315T100000\r\nDTEND;TZID=Europe/Budapest:20240315T110000\r\nSUMMARY:Planning\r\nDESCRIPTION:Agenda\r\nSEQUENCE:0\r\nDTSTAMP:20240101T000000Z\r\nORGANIZER;CN=Boss:mailto:{organizer}\r\n"
        );
        for (email, ps) in attendees {
            s.push_str(&format!(
                "ATTENDEE;CN=X;PARTSTAT={ps};RSVP=TRUE:mailto:{email}\r\n"
            ));
        }
        s.push_str(extra);
        s.push_str(
            "BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\nEND:VEVENT\r\n",
        );
        s
    }

    use chrono::TimeZone;

    #[test]
    fn organizer_create_sends_request_to_each_attendee_and_stamps_pending() {
        let all = ids(&[ORG]);
        let p = props();
        let c = ctx(ORG, &all, &p);
        let new = doc(&event(
            ORG,
            &[
                (ME, "NEEDS-ACTION"),
                ("other@example.com", "NEEDS-ACTION"),
                (ORG, "ACCEPTED"),
            ],
            "ATTENDEE;CUTYPE=ROOM;PARTSTAT=ACCEPTED:mailto:room@example.com\r\nATTENDEE;SCHEDULE-AGENT=CLIENT;PARTSTAT=NEEDS-ACTION:mailto:client@example.com\r\n",
        ));
        let out = process_client_change(
            &ObjectChange {
                old: None,
                new: Some(new),
                origin: Origin::Client,
            },
            &c,
        );
        let mut to: Vec<&str> = out.messages.iter().map(|m| m.recipient.as_str()).collect();
        to.sort();
        assert_eq!(to, vec![ME, "other@example.com"]);
        let m = &out.messages[0];
        assert_eq!(m.method, ItipMethod::Request);
        assert_eq!(m.kind, MessageKind::Invitation);
        assert_eq!(m.sender, ORG);
        assert!(m.ics.contains("METHOD:REQUEST"));
        assert!(!m.ics.contains("VALARM"));
        assert!(!m.ics.contains("SCHEDULE-AGENT"));
        assert!(m.ics.contains("DTSTAMP:20240201T120000Z"));
        assert!(
            m.ics.contains("BEGIN:VTIMEZONE\r\nTZID:Europe/Budapest"),
            "a VTIMEZONE is generated for the referenced zone: {}",
            m.ics
        );
        let stamped = out.rewritten_new.unwrap();
        let raw = stamped.events[0].raw.clone().unwrap();
        assert!(
            raw.contains("SCHEDULE-STATUS=1.0:mailto:me@example.com"),
            "{raw}"
        );
        assert!(raw.contains("SCHEDULE-STATUS=1.0:mailto:other@example.com"));
        assert!(!raw.contains("SCHEDULE-STATUS=1.0:mailto:room"));
        assert_eq!(
            stamped.events[0].sequence, 0,
            "creation keeps the client's sequence"
        );
    }

    #[test]
    fn organizer_update_rules() {
        let all = ids(&[ORG]);
        let p = props();
        let c = ctx(ORG, &all, &p);
        let old = doc(&event(
            ORG,
            &[(ME, "ACCEPTED"), ("other@example.com", "NEEDS-ACTION")],
            "",
        ));
        // Description-only change: nothing.
        let desc = doc(&event(
            ORG,
            &[(ME, "ACCEPTED"), ("other@example.com", "NEEDS-ACTION")],
            "",
        )
        .replace("DESCRIPTION:Agenda", "DESCRIPTION:Agenda v2"));
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: Some(desc),
                origin: Origin::Client,
            },
            &c,
        );
        assert!(out.messages.is_empty());
        assert!(out.rewritten_new.is_none());
        // Time change: UpdatedInvitation to all, SEQUENCE bumped to 1.
        let moved = doc(&event(
            ORG,
            &[(ME, "ACCEPTED"), ("other@example.com", "NEEDS-ACTION")],
            "",
        )
        .replace("T100000", "T140000"));
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: Some(moved),
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 2);
        assert!(
            out.messages
                .iter()
                .all(|m| m.kind == MessageKind::UpdatedInvitation && m.sequence == 1)
        );
        assert!(out.messages[0].ics.contains("SEQUENCE:1"));
        assert_eq!(out.rewritten_new.as_ref().unwrap().events[0].sequence, 1);
        // Client bumped SEQUENCE itself: request, no double bump.
        let bumped = doc(&event(
            ORG,
            &[(ME, "ACCEPTED"), ("other@example.com", "NEEDS-ACTION")],
            "",
        )
        .replace("SEQUENCE:0", "SEQUENCE:5"));
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: Some(bumped),
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 2);
        assert!(out.messages.iter().all(|m| m.sequence == 5));
        // Attendee added: invitation only to the new one.
        let added = doc(&event(
            ORG,
            &[
                (ME, "ACCEPTED"),
                ("other@example.com", "NEEDS-ACTION"),
                ("new@example.com", "NEEDS-ACTION"),
            ],
            "",
        ));
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: Some(added),
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].recipient, "new@example.com");
        assert_eq!(out.messages[0].kind, MessageKind::Invitation);
        // Attendee removed: CANCEL to them only.
        let removed = doc(&event(ORG, &[(ME, "ACCEPTED")], ""));
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: Some(removed),
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].method, ItipMethod::Cancel);
        assert_eq!(out.messages[0].recipient, "other@example.com");
        assert!(out.messages[0].ics.contains("STATUS:CANCELLED"));
        assert!(out.messages[0].ics.contains("mailto:other@example.com"));
        assert!(
            !out.messages[0].ics.contains("mailto:me@example.com"),
            "cancel carries only the recipient"
        );
        // STATUS:CANCELLED: cancel to all, bumped.
        let cancelled = doc(&event(
            ORG,
            &[(ME, "ACCEPTED"), ("other@example.com", "NEEDS-ACTION")],
            "STATUS:CANCELLED\r\n",
        ));
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: Some(cancelled),
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 2);
        assert!(
            out.messages
                .iter()
                .all(|m| m.method == ItipMethod::Cancel && m.sequence == 1)
        );
        // Delete: cancel to all with old+1; delete of a cancelled one: nothing.
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: None,
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 2);
        assert!(
            out.messages
                .iter()
                .all(|m| m.method == ItipMethod::Cancel && m.sequence == 1)
        );
        let already = doc(&event(ORG, &[(ME, "ACCEPTED")], "STATUS:CANCELLED\r\n"));
        let out = process_client_change(
            &ObjectChange {
                old: Some(already),
                new: None,
                origin: Origin::Client,
            },
            &c,
        );
        assert!(out.messages.is_empty());
        // Non-client origins never schedule.
        let out = process_client_change(
            &ObjectChange {
                old: None,
                new: Some(old.clone()),
                origin: Origin::Mirror,
            },
            &c,
        );
        assert!(out.messages.is_empty());
    }

    #[test]
    fn attendee_side_rules() {
        let all = ids(&[ME, "alt@example.com"]);
        let p = props();
        let c = ctx(ME, &all, &p);
        let old = doc(&event(
            ORG,
            &[(ME, "NEEDS-ACTION"), ("other@example.com", "ACCEPTED")],
            "",
        ));
        let accepted = doc(&event(
            ORG,
            &[(ME, "ACCEPTED"), ("other@example.com", "ACCEPTED")],
            "",
        ));
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: Some(accepted),
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 1);
        let m = &out.messages[0];
        assert_eq!(m.method, ItipMethod::Reply);
        assert_eq!(
            m.kind,
            MessageKind::Reply {
                partstat: "ACCEPTED".into()
            }
        );
        assert_eq!(m.sender, ME);
        assert_eq!(m.recipient, ORG);
        assert_eq!(m.sequence, 0);
        assert!(m.ics.contains("METHOD:REPLY"));
        assert!(m.ics.contains("PARTSTAT=ACCEPTED:mailto:me@example.com"));
        assert!(
            !m.ics.contains("other@example.com"),
            "reply carries only our line"
        );
        assert!(!m.ics.contains("RSVP=TRUE"));
        assert!(m.ics.contains("REQUEST-STATUS:2.0;Success"));
        assert!(!m.ics.contains("DESCRIPTION:"));
        let stamped = out.rewritten_new.unwrap();
        assert!(
            stamped.events[0]
                .raw
                .as_ref()
                .unwrap()
                .contains("ORGANIZER;CN=Boss;SCHEDULE-STATUS=1.0:mailto:boss@example.com")
        );
        assert_eq!(stamped.events[0].sequence, 0);
        // NEEDS-ACTION or an unrelated edit: nothing.
        let renamed = doc(&event(
            ORG,
            &[(ME, "NEEDS-ACTION"), ("other@example.com", "ACCEPTED")],
            "",
        )
        .replace("SUMMARY:Planning", "SUMMARY:Mine"));
        assert!(
            process_client_change(
                &ObjectChange {
                    old: Some(old.clone()),
                    new: Some(renamed),
                    origin: Origin::Client
                },
                &c
            )
            .messages
            .is_empty()
        );
        // Delete: DECLINED reply; delete when already declined: nothing.
        let out = process_client_change(
            &ObjectChange {
                old: Some(old.clone()),
                new: None,
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 1);
        assert_eq!(
            out.messages[0].kind,
            MessageKind::Reply {
                partstat: "DECLINED".into()
            }
        );
        assert!(
            out.messages[0]
                .ics
                .contains("PARTSTAT=DECLINED:mailto:me@example.com")
        );
        let declined = doc(&event(ORG, &[(ME, "DECLINED")], ""));
        assert!(
            process_client_change(
                &ObjectChange {
                    old: Some(declined),
                    new: None,
                    origin: Origin::Client
                },
                &c
            )
            .messages
            .is_empty()
        );
        // SCHEDULE-AGENT=CLIENT on our line silences everything.
        let client_old = doc(&event(
            ORG,
            &[],
            "ATTENDEE;SCHEDULE-AGENT=CLIENT;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\n",
        ));
        let client_new = doc(&event(
            ORG,
            &[],
            "ATTENDEE;SCHEDULE-AGENT=CLIENT;PARTSTAT=ACCEPTED:mailto:me@example.com\r\n",
        ));
        assert!(
            process_client_change(
                &ObjectChange {
                    old: Some(client_old),
                    new: Some(client_new),
                    origin: Origin::Client
                },
                &c
            )
            .messages
            .is_empty()
        );
    }

    #[test]
    fn per_instance_reply_carries_only_that_override() {
        let all = ids(&[ME]);
        let p = props();
        let c = ctx(ME, &all, &p);
        let series = |ps_master: &str, ps_override: &str| {
            doc(&format!(
                "BEGIN:VEVENT\r\nUID:s\r\nDTSTART:20240301T090000Z\r\nDTEND:20240301T100000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:W\r\nORGANIZER:mailto:{ORG}\r\nATTENDEE;PARTSTAT={ps_master}:mailto:{ME}\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:s\r\nRECURRENCE-ID:20240308T090000Z\r\nDTSTART:20240308T090000Z\r\nDTEND:20240308T100000Z\r\nSUMMARY:W\r\nORGANIZER:mailto:{ORG}\r\nATTENDEE;PARTSTAT={ps_override}:mailto:{ME}\r\nEND:VEVENT\r\n"
            ))
        };
        let out = process_client_change(
            &ObjectChange {
                old: Some(series("ACCEPTED", "ACCEPTED")),
                new: Some(series("ACCEPTED", "DECLINED")),
                origin: Origin::Client,
            },
            &c,
        );
        assert_eq!(out.messages.len(), 1);
        let m = &out.messages[0];
        assert!(m.recurrence_id.is_some());
        assert_eq!(m.ics.matches("BEGIN:VEVENT").count(), 1);
        assert!(m.ics.contains("RECURRENCE-ID:20240308T090000Z"));
        assert!(!m.ics.contains("RRULE"));
    }

    fn cal(provider: Provider) -> CalendarRow {
        CalendarRow {
            slug: "personal".into(),
            display_name: "Personal".into(),
            description: String::new(),
            color: None,
            order: 0,
            timezone: None,
            transparent: false,
            identity: ME.into(),
            provider,
            remote_account: None,
            remote_calendar_id: None,
            two_way: true,
            send_via: SendVia::Smtp,
            is_default_for_identity: true,
            sync_baseline_id: 0,
            created_at: 0,
        }
    }

    fn inbound<'a>(
        calendar: &'a CalendarRow,
        stored: Option<&'a ICalendarDocument>,
        ids: &'a [String],
    ) -> InboundContext<'a> {
        InboundContext {
            calendar,
            identity: ME,
            all_identities: ids,
            stored,
            inbox_for_mirrored: false,
            mirror_offline: false,
            now: Utc.with_ymd_and_hms(2024, 2, 1, 12, 0, 0).unwrap(),
        }
    }

    /// [`inbound`] with the calendar's mirror stopped.
    fn inbound_offline<'a>(
        calendar: &'a CalendarRow,
        stored: Option<&'a ICalendarDocument>,
        ids: &'a [String],
    ) -> InboundContext<'a> {
        InboundContext {
            mirror_offline: true,
            ..inbound(calendar, stored, ids)
        }
    }

    fn with_method(method: &str, body: &str) -> ICalendarDocument {
        parse_document(&format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//g//EN\r\nMETHOD:{method}\r\n{body}END:VCALENDAR\r\n")).unwrap()
    }

    #[test]
    fn inbound_request_create_update_stale_and_partstat_preservation() {
        let native = cal(Provider::Native);
        let all = ids(&[ME]);
        let req = with_method("REQUEST", &event(ORG, &[(ME, "NEEDS-ACTION")], ""));
        let d = apply_inbound(&req, &inbound(&native, None, &all));
        assert_eq!(d.outcome, InboundOutcome::Created);
        assert!(d.inbox_copy);
        let stored = d.write.unwrap();
        assert!(stored.method.is_none());
        // We accept locally; a same-sequence refresh with a newer DTSTAMP keeps ACCEPTED.
        let mut accepted = stored.clone();
        accepted
            .set_attendee_partstat(ME, "ACCEPTED", None)
            .unwrap();
        let refresh = with_method(
            "REQUEST",
            &event(ORG, &[(ME, "NEEDS-ACTION")], "")
                .replace("DTSTAMP:20240101T000000Z", "DTSTAMP:20240120T000000Z")
                .replace("SUMMARY:Planning", "SUMMARY:Planning (room changed)"),
        );
        let d = apply_inbound(&refresh, &inbound(&native, Some(&accepted), &all));
        assert_eq!(d.outcome, InboundOutcome::Updated);
        let w = d.write.unwrap();
        assert_eq!(w.events[0].summary, "Planning (room changed)");
        assert_eq!(
            w.events[0].attendees[0].partstat.as_deref(),
            Some("ACCEPTED")
        );
        // Same sequence, older dtstamp: duplicate. Lower sequence: stale. Higher: replaces.
        let dup = with_method("REQUEST", &event(ORG, &[(ME, "NEEDS-ACTION")], ""));
        assert_eq!(
            apply_inbound(&dup, &inbound(&native, Some(&accepted), &all)).outcome,
            InboundOutcome::IgnoredDuplicate
        );
        let mut newer = accepted.clone();
        newer.set_sequence(3).unwrap();
        assert_eq!(
            apply_inbound(&dup, &inbound(&native, Some(&newer), &all)).outcome,
            InboundOutcome::IgnoredStale
        );
        let bumped = with_method(
            "REQUEST",
            &event(ORG, &[(ME, "NEEDS-ACTION")], "").replace("SEQUENCE:0", "SEQUENCE:4"),
        );
        let d = apply_inbound(&bumped, &inbound(&native, Some(&newer), &all));
        assert_eq!(d.outcome, InboundOutcome::Updated);
        assert_eq!(
            d.write.unwrap().events[0].attendees[0].partstat.as_deref(),
            Some("NEEDS-ACTION"),
            "organizer decides after a re-invite"
        );
        // Mirrored calendars never take REQUEST/CANCEL.
        let mirrored = cal(Provider::Google);
        assert_eq!(
            apply_inbound(&req, &inbound(&mirrored, None, &all)).outcome,
            InboundOutcome::NotAppliedMirrored
        );
        // ...unless that mirror is stopped, when the mail is the only
        // thing still arriving for the calendar.
        let d = apply_inbound(&req, &inbound_offline(&mirrored, None, &all));
        assert_eq!(d.outcome, InboundOutcome::Created);
        assert!(d.write.is_some());
        assert_eq!(
            apply_inbound(
                &with_method("PUBLISH", &event(ORG, &[], "")),
                &inbound(&native, None, &all)
            )
            .outcome,
            InboundOutcome::IgnoredMethod("PUBLISH".into())
        );
    }

    #[test]
    fn a_cancel_for_a_mirrored_calendar_is_applied_once_its_mirror_stops() {
        // The failure this exists for: an organizer cancels a meeting, the
        // CANCEL mail arrives, and the calendar's Google mirror is stopped
        // on a revoked token — dropping it leaves the meeting showing as
        // if it were still on, on the day it was supposed to happen.
        let mirrored = cal(Provider::Google);
        let all = ids(&[ME]);
        let stored = doc(&format!(
            "BEGIN:VEVENT\r\nUID:s\r\nDTSTART;TZID=Europe/Budapest:20240301T090000\r\nDTEND;TZID=Europe/Budapest:20240301T100000\r\nSUMMARY:W\r\nSEQUENCE:1\r\nORGANIZER:mailto:{ORG}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:{ME}\r\nEND:VEVENT\r\n"
        ));
        let cancel = with_method(
            "CANCEL",
            &format!(
                "BEGIN:VEVENT\r\nUID:s\r\nDTSTART;TZID=Europe/Budapest:20240301T090000\r\nSUMMARY:W\r\nSEQUENCE:2\r\nORGANIZER:mailto:{ORG}\r\nATTENDEE:mailto:{ME}\r\nEND:VEVENT\r\n"
            ),
        );
        assert_eq!(
            apply_inbound(&cancel, &inbound(&mirrored, Some(&stored), &all)).outcome,
            InboundOutcome::NotAppliedMirrored,
            "a running mirror still owns the calendar"
        );
        let d = apply_inbound(&cancel, &inbound_offline(&mirrored, Some(&stored), &all));
        assert_eq!(d.outcome, InboundOutcome::CancelledWhole);
        // Cancelled, never deleted — a delete would read as us declining.
        let written = d.write.expect("a write");
        assert_eq!(written.master().unwrap().status.as_ics(), "CANCELLED");
    }

    #[test]
    fn inbound_cancel_whole_and_instance() {
        let native = cal(Provider::Native);
        let all = ids(&[ME]);
        let stored = doc(&format!(
            "BEGIN:VEVENT\r\nUID:s\r\nDTSTART;TZID=Europe/Budapest:20240301T090000\r\nDTEND;TZID=Europe/Budapest:20240301T100000\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:W\r\nSEQUENCE:1\r\nORGANIZER:mailto:{ORG}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:{ME}\r\nEND:VEVENT\r\n"
        ));
        let whole = with_method(
            "CANCEL",
            &format!(
                "BEGIN:VEVENT\r\nUID:s\r\nDTSTART:20240301T080000Z\r\nSEQUENCE:2\r\nSTATUS:CANCELLED\r\nORGANIZER:mailto:{ORG}\r\nATTENDEE:mailto:{ME}\r\nEND:VEVENT\r\n"
            ),
        );
        let d = apply_inbound(&whole, &inbound(&native, Some(&stored), &all));
        assert_eq!(d.outcome, InboundOutcome::CancelledWhole);
        let w = d.write.unwrap();
        assert_eq!(w.events[0].status, EventStatus::Cancelled);
        assert_eq!(w.events[0].sequence, 2);
        let instance = with_method(
            "CANCEL",
            &format!(
                "BEGIN:VEVENT\r\nUID:s\r\nRECURRENCE-ID:20240308T080000Z\r\nDTSTART:20240308T080000Z\r\nSEQUENCE:1\r\nSTATUS:CANCELLED\r\nORGANIZER:mailto:{ORG}\r\nATTENDEE:mailto:{ME}\r\nEND:VEVENT\r\n"
            ),
        );
        let d = apply_inbound(&instance, &inbound(&native, Some(&stored), &all));
        assert_eq!(d.outcome, InboundOutcome::CancelledInstance);
        let w = d.write.unwrap();
        let raw = w.events[0].raw.clone().unwrap();
        assert!(
            raw.contains("EXDATE;TZID=Europe/Budapest:20240308T090000"),
            "{raw}"
        );
        assert_eq!(w.events[0].exdates.len(), 1);
        assert_eq!(
            apply_inbound(&whole, &inbound(&native, None, &all)).outcome,
            InboundOutcome::IgnoredUnknownUid
        );
        let stale = with_method(
            "CANCEL",
            &format!(
                "BEGIN:VEVENT\r\nUID:s\r\nDTSTART:20240301T080000Z\r\nSEQUENCE:0\r\nORGANIZER:mailto:{ORG}\r\nEND:VEVENT\r\n"
            ),
        );
        assert_eq!(
            apply_inbound(&stale, &inbound(&native, Some(&stored), &all)).outcome,
            InboundOutcome::IgnoredStale
        );
    }

    #[test]
    fn inbound_reply_patches_only_that_attendee() {
        let native = cal(Provider::Native);
        let all = ids(&[ME]);
        // We organise; bob replies.
        let stored = doc(&event(
            ME,
            &[
                ("bob@example.com", "NEEDS-ACTION"),
                ("carol@example.com", "NEEDS-ACTION"),
            ],
            "",
        ));
        let reply = with_method(
            "REPLY",
            &format!(
                "BEGIN:VEVENT\r\nUID:e1\r\nDTSTART;TZID=Europe/Budapest:20240315T100000\r\nSEQUENCE:0\r\nORGANIZER:mailto:{ME}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:BOB@example.com\r\nEND:VEVENT\r\n"
            ),
        );
        let d = apply_inbound(&reply, &inbound(&native, Some(&stored), &all));
        assert_eq!(
            d.outcome,
            InboundOutcome::ReplyApplied {
                attendee: "bob@example.com".into(),
                partstat: "ACCEPTED".into()
            }
        );
        let w = d.write.unwrap();
        let bob = w.events[0]
            .attendees
            .iter()
            .find(|a| a.email == "bob@example.com")
            .unwrap();
        assert_eq!(bob.partstat.as_deref(), Some("ACCEPTED"));
        let carol = w.events[0]
            .attendees
            .iter()
            .find(|a| a.email == "carol@example.com")
            .unwrap();
        assert_eq!(carol.partstat.as_deref(), Some("NEEDS-ACTION"));
        assert_eq!(w.events[0].sequence, 0);
        // Same reply again: duplicate. Unknown attendee: ignored. Not our event: ignored.
        assert_eq!(
            apply_inbound(&reply, &inbound(&native, Some(&w), &all)).outcome,
            InboundOutcome::IgnoredDuplicate
        );
        let stranger = with_method(
            "REPLY",
            &format!(
                "BEGIN:VEVENT\r\nUID:e1\r\nDTSTART:20240315T090000Z\r\nSEQUENCE:0\r\nORGANIZER:mailto:{ME}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:zed@example.com\r\nEND:VEVENT\r\n"
            ),
        );
        assert_eq!(
            apply_inbound(&stranger, &inbound(&native, Some(&stored), &all)).outcome,
            InboundOutcome::IgnoredUnknownAttendee
        );
        let theirs = doc(&event(ORG, &[(ME, "ACCEPTED")], ""));
        assert_eq!(
            apply_inbound(&reply, &inbound(&native, Some(&theirs), &all)).outcome,
            InboundOutcome::IgnoredUnknownUid
        );
        // Replies apply to mirrored calendars too.
        let mirrored = cal(Provider::Google);
        assert!(matches!(
            apply_inbound(&reply, &inbound(&mirrored, Some(&stored), &all)).outcome,
            InboundOutcome::ReplyApplied { .. }
        ));
    }

    #[test]
    fn significant_change_detection() {
        let p = props();
        let a = doc(&event(ORG, &[(ME, "NEEDS-ACTION")], ""));
        let b = doc(&event(ORG, &[(ME, "NEEDS-ACTION")], "")
            .replace("DESCRIPTION:Agenda", "DESCRIPTION:Other"));
        assert!(!is_significant_change(&a, &b, &p));
        let c =
            doc(&event(ORG, &[(ME, "NEEDS-ACTION")], "")
                .replace("SUMMARY:Planning", "SUMMARY:Party"));
        assert!(is_significant_change(&a, &c, &p));
        // Same instant expressed in UTC vs TZID is not a change.
        let d = doc(&event(ORG, &[(ME, "NEEDS-ACTION")], "").replace(
            "DTSTART;TZID=Europe/Budapest:20240315T100000",
            "DTSTART:20240315T090000Z",
        ));
        assert!(!is_significant_change(&a, &d, &p));
        let custom = ids(&["DESCRIPTION"]);
        assert!(is_significant_change(&a, &b, &custom));
    }

    #[test]
    fn outbox_post_answers_per_recipient() {
        let (status, xml) = handle_outbox_post(
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VFREEBUSY\r\nORGANIZER:mailto:me@x\r\nATTENDEE:mailto:a@x\r\nATTENDEE:mailto:b@x\r\nEND:VFREEBUSY\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(xml.matches("<C:response>").count(), 2);
        assert!(xml.contains("3.7;Invalid calendar user"));
        let (_, xml) = handle_outbox_post(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nATTENDEE:mailto:a@x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert!(xml.contains("5.3;No scheduling support for user"));
    }
}
