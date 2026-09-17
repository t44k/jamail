//! iMIP (RFC 6047): iTIP messages over email, in both directions, through
//! our own mail server.
//!
//! **Outbound**: [`plan_client_write`] runs the pure broker for a client
//! `PUT`/`DELETE`, builds one RFC 5322 message per recipient
//! ([`build_imip_message`]) and hands the server the rows to queue inside
//! the very same store transaction that commits the object — a crash can
//! never leave an event stored without its invitations. A worker thread
//! ([`run_outbound_worker`]) drains `outbound_queue` over SMTP with
//! exponential backoff, then stamps `SCHEDULE-STATUS` on the stored object
//! (`1.1;Scheduled`, or `5.1;Could not complete delivery`) as an
//! `Origin::System` write that neither schedules nor is pushed to remotes.
//!
//! The message shape is the one Google Calendar itself sends and every
//! major client accepts: `multipart/mixed[ multipart/alternative[
//! text/plain ; text/calendar; method=… ] ; application/ics attachment ]`.
//! `From:` is the identity the message is sent as and there is **no
//! `Sender:` header**: lettre derives the SMTP envelope sender from
//! `From`, and this deployment's Postfix keys its sender-dependent relay
//! (each identity out through its own provider, SPF/DKIM-aligned) and its
//! sender-login check on that envelope address.
//!
//! **Inbound**: [`run_inbound_watcher`] polls/IDLEs the configured folders
//! of our IMAP account, takes every new message's `text/calendar` part
//! (falling back to `application/ics` / `*.ics`), routes it to one of our
//! identities — by the ICS `ATTENDEE`/`ORGANIZER` lines, the mail-puller's
//! `acct-<label>` keyword only breaking ties — and applies it with
//! `itip::apply_inbound`. Processed messages are remembered by IMAP
//! `(folder, uidvalidity, uid)` and by `Message-ID`, so the categorizer
//! moving a message between folders never applies it twice. Mail is never
//! flagged, moved or deleted: jamail is the mail client.

use crate::calendar::{self, EventTime, ICalendarDocument};
use crate::config::SmtpConfig;
use crate::jadav::config::MailConfig;
use crate::jadav::itip::{
    self, ItipMessage, ItipMethod, MessageKind, ObjectChange, SchedulingContext,
};
use crate::jadav::store::{
    CalendarRow, ImipProcessed, Origin, OutboundJob, Precondition, Provider, Store,
};
use crate::mail::{self, MailClient};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::{Address, Message, SmtpTransport, Transport};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------
// Runtime configuration shared by server, worker and watcher
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SubjectTemplates {
    pub request: String,
    pub request_update: String,
    pub cancel: String,
    pub reply_accepted: String,
    pub reply_tentative: String,
    pub reply_declined: String,
}

impl Default for SubjectTemplates {
    fn default() -> Self {
        Self {
            request: "Invitation: {summary} @ {when} ({tz})".into(),
            request_update: "Updated invitation: {summary} @ {when} ({tz})".into(),
            cancel: "Cancelled event: {summary} @ {when} ({tz})".into(),
            reply_accepted: "Accepted: {summary} @ {when} ({tz})".into(),
            reply_tentative: "Tentatively accepted: {summary} @ {when} ({tz})".into(),
            reply_declined: "Declined: {summary} @ {when} ({tz})".into(),
        }
    }
}

/// Everything the scheduling paths need, resolved once from config.
pub struct SchedulingRuntime {
    /// Lower-cased identities, primary first.
    pub identities: Vec<String>,
    /// Display names for identities configured as `Name <addr>`.
    pub identity_names: HashMap<String, String>,
    pub significant_properties: Vec<String>,
    pub prodid: String,
    pub templates: SubjectTemplates,
    pub default_timezone: String,
    /// Domain used in generated `Message-ID`s.
    pub message_id_domain: String,
    pub inbox_for_mirrored: bool,
    pub retry_max_age_secs: i64,
    /// Whether outbound mail is possible at all (a `mail:` section exists).
    pub mail_enabled: bool,
    /// Wake the outbound worker after queueing.
    pub outbound_wake: Arc<(Mutex<bool>, Condvar)>,
}

impl SchedulingRuntime {
    pub fn wake_outbound(&self) {
        let (lock, cv) = &*self.outbound_wake;
        if let Ok(mut flag) = lock.lock() {
            *flag = true;
        }
        cv.notify_all();
    }

    pub fn identity_display(&self, email: &str) -> IdentityDisplay {
        let email = email.to_ascii_lowercase();
        IdentityDisplay {
            name: self.identity_names.get(&email).cloned(),
            email,
        }
    }

    fn context<'a>(
        &'a self,
        calendar_identity: &'a str,
        now: DateTime<Utc>,
    ) -> SchedulingContext<'a> {
        SchedulingContext {
            calendar_identity: self
                .identities
                .iter()
                .find(|i| i.eq_ignore_ascii_case(calendar_identity))
                .map(String::as_str)
                .unwrap_or(calendar_identity),
            all_identities: &self.identities,
            significant_properties: &self.significant_properties,
            prodid: &self.prodid,
            now,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IdentityDisplay {
    pub email: String,
    pub name: Option<String>,
}

// ---------------------------------------------------------------------
// Message building
// ---------------------------------------------------------------------

/// `("Tue 16 Sep 2026 12:00 – 13:00", "CEST")` or, for all-day,
/// `("Tue 16 Sep 2026", "")`.
pub fn format_when(dtstart: &EventTime, dtend: &EventTime, fallback_tz: &str) -> (String, String) {
    if dtstart.all_day {
        let days = (dtend.utc - dtstart.utc).num_days();
        let start = dtstart.utc.format("%a %d %b %Y").to_string();
        if days > 1 {
            let last = dtend.utc - chrono::Duration::days(1);
            return (
                format!("{} – {}", start, last.format("%a %d %b %Y")),
                String::new(),
            );
        }
        return (start, String::new());
    }
    let tzid = dtstart
        .tzid
        .clone()
        .unwrap_or_else(|| fallback_tz.to_string());
    let (start_local, end_local, abbr) = match calendar::resolve_tz(&tzid) {
        Some(calendar::TzAlias::Iana(tz)) => {
            use chrono_tz::OffsetName;
            let s = tz.from_utc_datetime(&dtstart.utc.naive_utc());
            let e = tz.from_utc_datetime(&dtend.utc.naive_utc());
            let abbr = s.offset().abbreviation().unwrap_or("").to_string();
            (
                s.naive_local(),
                e.naive_local(),
                if abbr.is_empty() { tzid.clone() } else { abbr },
            )
        }
        Some(calendar::TzAlias::Fixed(off)) => (
            dtstart.utc.naive_utc() + chrono::Duration::seconds(i64::from(off)),
            dtend.utc.naive_utc() + chrono::Duration::seconds(i64::from(off)),
            tzid.clone(),
        ),
        None => (
            dtstart.utc.naive_utc(),
            dtend.utc.naive_utc(),
            "UTC".to_string(),
        ),
    };
    let when = if start_local.date() == end_local.date() {
        format!(
            "{} {} – {}",
            start_local.format("%a %d %b %Y"),
            start_local.format("%H:%M"),
            end_local.format("%H:%M")
        )
    } else {
        format!(
            "{} – {}",
            start_local.format("%a %d %b %Y %H:%M"),
            end_local.format("%a %d %b %Y %H:%M")
        )
    };
    (when, abbr)
}

fn fill(template: &str, msg: &ItipMessage, when: &str, tz: &str, from: &IdentityDisplay) -> String {
    let attendee_name = from.name.clone().unwrap_or_else(|| from.email.clone());
    let s = template
        .replace("{summary}", &msg.summary)
        .replace("{when}", when)
        .replace("{tz}", tz)
        .replace("{organizer}", &msg.organizer)
        .replace("{attendee}", &attendee_name);
    // Tidy an empty timezone placeholder: "… (CEST)" vs "… ()".
    s.replace(" ()", "")
}

pub fn subject_for(
    msg: &ItipMessage,
    templates: &SubjectTemplates,
    when: &str,
    tz: &str,
    from: &IdentityDisplay,
) -> String {
    let template = match &msg.kind {
        MessageKind::Invitation => &templates.request,
        MessageKind::UpdatedInvitation => &templates.request_update,
        MessageKind::Cancellation => &templates.cancel,
        MessageKind::Reply { partstat } => match partstat.as_str() {
            "ACCEPTED" => &templates.reply_accepted,
            "TENTATIVE" => &templates.reply_tentative,
            _ => &templates.reply_declined,
        },
    };
    fill(template, msg, when, tz, from)
}

pub fn plain_text_body(msg: &ItipMessage, from: &IdentityDisplay, when: &str, tz: &str) -> String {
    let mut out = String::new();
    let who = from.name.clone().unwrap_or_else(|| from.email.clone());
    match &msg.kind {
        MessageKind::Invitation => out.push_str(&format!("{} invites you to:\n\n", who)),
        MessageKind::UpdatedInvitation => out.push_str(&format!("{} updated the event:\n\n", who)),
        MessageKind::Cancellation => out.push_str(&format!("{} cancelled the event:\n\n", who)),
        MessageKind::Reply { partstat } => {
            let verb = match partstat.as_str() {
                "ACCEPTED" => "accepted",
                "TENTATIVE" => "tentatively accepted",
                _ => "declined",
            };
            out.push_str(&format!("{} has {} the invitation to:\n\n", who, verb));
        }
    }
    out.push_str(&format!("  {}\n", msg.summary));
    out.push_str(&format!(
        "  When:  {}{}\n",
        when,
        if tz.is_empty() {
            String::new()
        } else {
            format!(" ({})", tz)
        }
    ));
    if let Some(rid) = &msg.recurrence_id {
        out.push_str(&format!(
            "  (this occurrence only: {})\n",
            rid.utc.format("%a %d %b %Y")
        ));
    }
    if !msg.location.is_empty() {
        out.push_str(&format!("  Where: {}\n", msg.location));
    }
    if !msg.organizer.is_empty() {
        out.push_str(&format!("  Organizer: {}\n", msg.organizer));
    }
    if !msg.description.is_empty() && !matches!(msg.kind, MessageKind::Reply { .. }) {
        out.push('\n');
        out.push_str(&msg.description);
        out.push('\n');
    }
    out.push_str(
        "\nThis message contains a calendar invitation your calendar application can process.\n",
    );
    out
}

pub fn new_message_id(
    uid: &str,
    sequence: i64,
    method: ItipMethod,
    now: DateTime<Utc>,
    domain: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(uid.as_bytes());
    let short: String = hash[..8].iter().map(|b| format!("{:02x}", b)).collect();
    let nonce: u64 = {
        use std::io::Read;
        let mut b = [0u8; 8];
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        u64::from_le_bytes(b)
    };
    format!(
        "<jadav.{}.{}.{}.{}.{:x}@{}>",
        short,
        sequence,
        method.as_str().to_ascii_lowercase(),
        now.timestamp(),
        nonce,
        domain
    )
}

/// Build the RFC 5322 message for one iTIP message.
pub fn build_imip_message(
    msg: &ItipMessage,
    from: &IdentityDisplay,
    templates: &SubjectTemplates,
    in_reply_to: Option<&str>,
    message_id: &str,
    default_tz: &str,
) -> Result<Message> {
    // RFC 5545 §3.1: content lines are at most 75 octets. The broker already
    // folds; fold again so a caller handing us raw ICS cannot break that.
    let ics = calendar::fold_ics(&msg.ics);
    let from_mailbox = Mailbox::new(
        from.name.clone(),
        Address::from_str(&from.email).with_context(|| format!("invalid sender {}", from.email))?,
    );
    let to_mailbox = Mailbox::new(
        msg.recipient_cn.clone().filter(|n| !n.trim().is_empty()),
        Address::from_str(&msg.recipient)
            .with_context(|| format!("invalid recipient {}", msg.recipient))?,
    );
    let (when, tz) = format_when(&msg.dtstart, &msg.dtend, default_tz);
    let subject = subject_for(msg, templates, &when, &tz, from);
    let text = plain_text_body(msg, from, &when, &tz);
    let method = msg.method.as_str();

    let calendar_part = SinglePart::builder()
        .header(
            ContentType::parse(&format!(
                "text/calendar; charset=utf-8; method={}; name=invite.ics",
                method
            ))
            .map_err(|e| anyhow!("content type: {}", e))?,
        )
        .body(ics.clone());
    let text_part = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(text);
    let attachment = Attachment::new("invite.ics".to_string()).body(
        ics.into_bytes(),
        ContentType::parse("application/ics; name=invite.ics")
            .map_err(|e| anyhow!("content type: {}", e))?,
    );
    let body = MultiPart::mixed()
        .multipart(
            MultiPart::alternative()
                .singlepart(text_part)
                .singlepart(calendar_part),
        )
        .singlepart(attachment);

    let mut builder = Message::builder()
        .from(from_mailbox.clone())
        .reply_to(from_mailbox)
        .to(to_mailbox)
        .subject(subject)
        .date_now()
        .message_id(Some(message_id.to_string()))
        .raw_header(HeaderValue::new(
            HeaderName::new_from_ascii("Content-Class".to_string())
                .map_err(|e| anyhow!("{}", e))?,
            "urn:content-classes:calendarmessage".to_string(),
        ))
        .raw_header(HeaderValue::new(
            HeaderName::new_from_ascii("X-Mailer".to_string()).map_err(|e| anyhow!("{}", e))?,
            "jadav".to_string(),
        ));
    if let Some(irt) = in_reply_to
        && !irt.trim().is_empty()
    {
        builder = builder.in_reply_to(irt.trim().to_string());
    }
    builder.multipart(body).context("building iMIP message")
}

// ---------------------------------------------------------------------
// Client-write planning (called by the server before it commits)
// ---------------------------------------------------------------------

/// What the server should store and queue for a client write.
#[derive(Default, Debug)]
pub struct PlannedWrite {
    /// The document to store instead of the client's (SEQUENCE bump,
    /// SCHEDULE-STATUS stamps); `None` = store as received.
    pub store_ics: Option<String>,
    pub jobs: Vec<OutboundJob>,
}

impl PlannedWrite {
    pub fn rewritten(&self) -> bool {
        self.store_ics.is_some()
    }
}

/// Run the broker for a client write to `calendar` and build the mail.
/// Returns an empty plan when scheduling is off for the calendar
/// (`send_via: provider`), when no mail server is configured, or when the
/// documents do not parse.
pub fn plan_client_write(
    store: &Store,
    runtime: &SchedulingRuntime,
    calendar: &CalendarRow,
    href_name: &str,
    old_ics: Option<&str>,
    new_ics: Option<&str>,
    now: DateTime<Utc>,
) -> PlannedWrite {
    let mut plan = PlannedWrite::default();
    if !runtime.mail_enabled || calendar.send_via != crate::jadav::config::SendVia::Smtp {
        return plan;
    }
    let old = old_ics.and_then(|s| calendar::parse_document(s).ok());
    let new = new_ics.and_then(|s| calendar::parse_document(s).ok());
    if new_ics.is_some() && new.is_none() {
        return plan;
    }
    let change = ObjectChange {
        old,
        new,
        origin: Origin::Client,
    };
    let ctx = runtime.context(&calendar.identity, now);
    let out = itip::process_client_change(&change, &ctx);
    if let Some(doc) = out.rewritten_new {
        plan.store_ics = Some(doc.to_ics());
    }
    for msg in out.messages {
        let from = runtime.identity_display(&msg.sender);
        let message_id = new_message_id(
            &msg.uid,
            msg.sequence,
            msg.method,
            now,
            &runtime.message_id_domain,
        );
        let in_reply_to = if msg.method == ItipMethod::Reply {
            store.invitation_message_id(&msg.uid).ok().flatten()
        } else {
            None
        };
        match build_imip_message(
            &msg,
            &from,
            &runtime.templates,
            in_reply_to.as_deref(),
            &message_id,
            &runtime.default_timezone,
        ) {
            Ok(mail) => plan.jobs.push(OutboundJob {
                calendar_slug: calendar.slug.clone(),
                href_name: href_name.to_string(),
                ical_uid: msg.uid.clone(),
                recurrence_id: msg.recurrence_id.as_ref().map(|r| r.utc.to_rfc3339()),
                method: msg.method.as_str().to_string(),
                sender: msg.sender.clone(),
                recipient: msg.recipient.clone(),
                message_id,
                raw_message: mail.formatted(),
            }),
            Err(e) => eprintln!(
                "jadav: imip: could not build {} to {} for {}: {:#}",
                msg.method.as_str(),
                msg.recipient,
                msg.uid,
                e
            ),
        }
    }
    plan
}

// ---------------------------------------------------------------------
// Outbound delivery
// ---------------------------------------------------------------------

#[derive(Debug)]
pub struct SendFailure {
    pub permanent: bool,
    pub detail: String,
}

pub trait MailSender: Send {
    fn send_raw(&mut self, envelope_from: &str, to: &str, raw: &[u8]) -> Result<(), SendFailure>;
}

pub struct SmtpSender {
    cfg: SmtpConfig,
    transport: Option<SmtpTransport>,
}

impl SmtpSender {
    pub fn new(cfg: SmtpConfig) -> Self {
        Self {
            cfg,
            transport: None,
        }
    }
}

impl MailSender for SmtpSender {
    fn send_raw(&mut self, envelope_from: &str, to: &str, raw: &[u8]) -> Result<(), SendFailure> {
        if self.transport.is_none() {
            self.transport =
                Some(
                    crate::smtp::build_transport(&self.cfg).map_err(|e| SendFailure {
                        permanent: false,
                        detail: format!("{:#}", e),
                    })?,
                );
        }
        let from = Address::from_str(envelope_from).map_err(|e| SendFailure {
            permanent: true,
            detail: format!("bad sender {}: {}", envelope_from, e),
        })?;
        let to_addr = Address::from_str(to).map_err(|e| SendFailure {
            permanent: true,
            detail: format!("bad recipient {}: {}", to, e),
        })?;
        let envelope =
            lettre::address::Envelope::new(Some(from), vec![to_addr]).map_err(|e| SendFailure {
                permanent: true,
                detail: format!("envelope: {}", e),
            })?;
        let transport = self.transport.as_ref().expect("built above");
        match transport.send_raw(&envelope, raw) {
            Ok(_) => Ok(()),
            Err(e) => {
                let permanent = e.is_permanent();
                self.transport = None;
                Err(SendFailure {
                    permanent,
                    detail: format!("{}", e),
                })
            }
        }
    }
}

/// `min(30 s · 2^attempts, 1 h)`.
pub fn backoff_for(attempts: i64) -> Duration {
    let secs = 30u64.saturating_mul(2u64.saturating_pow(attempts.clamp(0, 12) as u32));
    Duration::from_secs(secs.min(3600))
}

fn stamp_status(store: &Store, job: &OutboundJob, status: &str) {
    let Ok(Some(obj)) = store.get_object(&job.calendar_slug, &job.href_name) else {
        return;
    };
    let Ok(mut doc) = calendar::parse_document(&obj.ics) else {
        return;
    };
    let rid = job
        .recurrence_id
        .as_deref()
        .and_then(|r| DateTime::parse_from_rfc3339(r).ok())
        .map(|d| EventTime::utc(d.with_timezone(&Utc)));
    let target = if job.method == "REPLY" {
        doc.master()
            .and_then(|m| m.organizer.clone())
            .unwrap_or_else(|| job.recipient.clone())
    } else {
        job.recipient.clone()
    };
    let changed = doc
        .set_schedule_status(&target, status, rid.as_ref())
        .unwrap_or(false);
    if changed {
        let _ = store.put_object(
            &job.calendar_slug,
            &job.href_name,
            &doc.to_ics(),
            Precondition::None,
            Origin::System,
        );
    }
}

/// Drain the outbound queue until `shutdown`. Wakes on `wake`, on the next
/// retry time, or every 30 s.
pub fn run_outbound_worker(
    store_path: PathBuf,
    mut sender: Box<dyn MailSender>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    max_age_secs: i64,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let now = Utc::now().timestamp();
        let next_due = match Store::open(&store_path) {
            Ok(store) => {
                match store.due_outbound(now, 20) {
                    Ok(due) => {
                        for item in due {
                            let job = &item.job;
                            match sender.send_raw(&job.sender, &job.recipient, &job.raw_message) {
                                Ok(()) => {
                                    let _ = store.mark_outbound_sent(item.id);
                                    stamp_status(&store, job, "1.1;Scheduled");
                                    eprintln!(
                                        "jadav: imip: sent {} for {} to {} (as {})",
                                        job.method, job.ical_uid, job.recipient, job.sender
                                    );
                                }
                                Err(f) => {
                                    let too_old = now - item.created_at > max_age_secs;
                                    if f.permanent || too_old {
                                        let _ = store.mark_outbound_failed(item.id, &f.detail);
                                        stamp_status(
                                            &store,
                                            job,
                                            "5.1;Could not complete delivery",
                                        );
                                        eprintln!(
                                            "jadav: imip: giving up on {} for {} to {}: {}",
                                            job.method, job.ical_uid, job.recipient, f.detail
                                        );
                                    } else {
                                        let wait = backoff_for(item.attempts);
                                        let _ = store.mark_outbound_retry(
                                            item.id,
                                            now + wait.as_secs() as i64,
                                            &f.detail,
                                        );
                                        eprintln!(
                                            "jadav: imip: {} to {} failed ({}); retry in {}s",
                                            job.method,
                                            job.recipient,
                                            f.detail,
                                            wait.as_secs()
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("jadav: imip: reading queue: {:#}", e),
                }
                store.next_outbound_due().ok().flatten()
            }
            Err(e) => {
                eprintln!("jadav: imip: cannot open store: {:#}", e);
                None
            }
        };
        let wait_secs = next_due
            .map(|t| (t - Utc::now().timestamp()).clamp(1, 30) as u64)
            .unwrap_or(30);
        let (lock, cv) = &*wake;
        if let Ok(mut flag) = lock.lock() {
            if !*flag {
                let _ = cv.wait_timeout(flag, Duration::from_secs(wait_secs));
                if let Ok(mut f) = lock.lock() {
                    *f = false;
                }
            } else {
                *flag = false;
            }
        }
    }
}

// ---------------------------------------------------------------------
// Inbound
// ---------------------------------------------------------------------

/// The iTIP documents carried by a mail: `text/calendar` parts first, then
/// `application/ics` / `*.ics` attachments; duplicates (Google sends the
/// same ICS twice) collapse by `(uid, method, sequence, dtstamp)`.
pub fn find_itip_parts(processed: &mail::ProcessedEmail) -> Vec<ICalendarDocument> {
    let mut candidates: Vec<(u8, &mail::AttachmentData)> = processed
        .attachments
        .iter()
        .filter_map(|a| {
            let mime = a.mime_type.to_ascii_lowercase();
            let name = a.filename.to_ascii_lowercase();
            if mime.starts_with("text/calendar") {
                Some((0u8, a))
            } else if mime == "application/ics" || name.ends_with(".ics") {
                Some((1u8, a))
            } else {
                None
            }
        })
        .collect();
    candidates.sort_by_key(|(rank, _)| *rank);
    let mut out: Vec<ICalendarDocument> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (_, att) in candidates {
        let text = String::from_utf8_lossy(&att.data);
        let text = text.trim_start_matches('\u{feff}');
        let Ok(doc) = calendar::parse_document(text) else {
            continue;
        };
        if doc.method.is_none() || doc.events.is_empty() {
            continue;
        }
        let master = doc.master().expect("non-empty");
        let key = format!(
            "{}|{}|{}|{:?}",
            master.uid,
            doc.method.clone().unwrap_or_default(),
            master.sequence,
            master.dtstamp
        );
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(doc);
    }
    out
}

/// Which of our identities a message is for. Candidates come from the ICS
/// (`ATTENDEE`s for REQUEST/CANCEL/ADD, `ORGANIZER` for REPLY/COUNTER/
/// REFRESH); ties are broken by the `acct-<label>` keyword, then by the
/// `To:` header, then config order.
pub fn route_identity(
    doc: &ICalendarDocument,
    identities: &[String],
    keywords: &[String],
    keyword_identities: &HashMap<String, String>,
    to_header: &str,
) -> Option<String> {
    let method = doc.method.clone().unwrap_or_default();
    let mut candidates: Vec<String> = Vec::new();
    let push = |candidates: &mut Vec<String>, email: &str| {
        let e = email.to_ascii_lowercase();
        if identities.iter().any(|i| i.eq_ignore_ascii_case(&e)) && !candidates.contains(&e) {
            candidates.push(e);
        }
    };
    match method.as_str() {
        "REPLY" | "COUNTER" | "REFRESH" => {
            for ev in &doc.events {
                if let Some(org) = &ev.organizer {
                    push(&mut candidates, org);
                }
            }
        }
        _ => {
            for ev in &doc.events {
                for a in &ev.attendees {
                    push(&mut candidates, &a.email);
                }
            }
        }
    }
    match candidates.len() {
        0 => None,
        1 => candidates.pop(),
        _ => {
            for k in keywords {
                if let Some(id) = keyword_identities.get(k)
                    && let Some(c) = candidates.iter().find(|c| c.eq_ignore_ascii_case(id))
                {
                    return Some(c.clone());
                }
            }
            let to = to_header.to_ascii_lowercase();
            if let Some(c) = candidates.iter().find(|c| to.contains(c.as_str())) {
                return Some(c.clone());
            }
            for id in identities {
                if let Some(c) = candidates.iter().find(|c| c.eq_ignore_ascii_case(id)) {
                    return Some(c.clone());
                }
            }
            candidates.pop()
        }
    }
}

/// Configuration for the inbound watcher (from `jadav.mail`).
pub struct InboundConfig {
    pub imap: crate::config::ImapConfig,
    pub folders: Vec<String>,
    pub since_days: u32,
    pub poll_interval: Duration,
    pub keyword_identities: HashMap<String, String>,
}

impl InboundConfig {
    pub fn from_mail_config(m: &MailConfig) -> Self {
        Self {
            imap: m.imap.clone(),
            folders: if m.folders.is_empty() {
                vec!["INBOX".to_string()]
            } else {
                m.folders.clone()
            },
            since_days: m.since_days,
            poll_interval: Duration::from_secs(m.poll_interval_secs.max(15)),
            keyword_identities: m
                .keyword_identities
                .iter()
                .map(|(k, v)| (k.clone(), v.to_ascii_lowercase()))
                .collect(),
        }
    }
}

/// Apply one fetched mail. Returns the outcomes recorded (one per iTIP
/// document found), or an empty list when the mail carried none.
#[allow(clippy::too_many_arguments)]
pub fn process_message(
    store: &Store,
    runtime: &SchedulingRuntime,
    cfg: &InboundConfig,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    keywords: &[String],
    body: &[u8],
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    if !store.imap_seen_insert(folder, uidvalidity, uid)? {
        return Ok(Vec::new());
    }
    let processed = mail::process_raw_email(body, false)?;
    let docs = find_itip_parts(&processed);
    if docs.is_empty() {
        return Ok(Vec::new());
    }
    let message_id = if processed.message_id.trim().is_empty() {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(body);
        format!(
            "<synth-{}@jadav>",
            h[..12]
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        )
    } else {
        processed.message_id.trim().to_string()
    };
    if let Some(prev) = store.imip_processed_find(&message_id)? {
        // Seen before (the categorizer moved it between folders).
        return Ok(vec![format!("already_processed:{}", prev)]);
    }
    let mut outcomes = Vec::new();
    for doc in docs {
        let method = doc.method.clone().unwrap_or_default();
        let master = doc.master().expect("non-empty");
        let mut rec = ImipProcessed {
            message_id: message_id.clone(),
            folder: Some(folder.to_string()),
            uid: Some(uid),
            method: Some(method.clone()),
            ical_uid: Some(master.uid.clone()),
            recurrence_id: master.recurrence_id.as_ref().map(|r| r.utc.to_rfc3339()),
            sequence: Some(master.sequence),
            dtstamp: master.dtstamp.map(|d| d.to_rfc3339()),
            identity: None,
            calendar_slug: None,
            outcome: String::new(),
        };
        let Some(identity) = route_identity(
            &doc,
            &runtime.identities,
            keywords,
            &cfg.keyword_identities,
            &processed.to,
        ) else {
            rec.outcome = "no_identity_match".to_string();
            outcomes.push(rec.outcome.clone());
            store.imip_processed_insert(&rec)?;
            continue;
        };
        rec.identity = Some(identity.clone());
        // Target calendar: the one already holding the UID, else the
        // identity's default.
        let existing = store.find_object_by_uid(&master.uid)?;
        let calendar = match &existing {
            Some(obj) => store.get_calendar(&obj.meta.calendar_slug)?,
            None => store.default_calendar_for_identity(&identity)?,
        };
        let Some(calendar) = calendar else {
            rec.outcome = "no_calendar_for_identity".to_string();
            outcomes.push(rec.outcome.clone());
            store.imip_processed_insert(&rec)?;
            continue;
        };
        rec.calendar_slug = Some(calendar.slug.clone());
        let stored_doc = existing
            .as_ref()
            .and_then(|o| calendar::parse_document(&o.ics).ok());
        let ctx = itip::InboundContext {
            calendar: &calendar,
            identity: &identity,
            all_identities: &runtime.identities,
            stored: stored_doc.as_ref(),
            inbox_for_mirrored: runtime.inbox_for_mirrored,
            now,
        };
        let decision = itip::apply_inbound(&doc, &ctx);
        if let Some(write) = &decision.write {
            let href = existing
                .as_ref()
                .map(|o| o.meta.href_name.clone())
                .unwrap_or_else(|| format!("{}.ics", crate::caldav::sanitize_uid(&master.uid)));
            match store.put_object(
                &calendar.slug,
                &href,
                &write.to_ics(),
                Precondition::None,
                Origin::Inbound,
            )? {
                Ok(_) => {}
                Err(e) => {
                    rec.outcome = format!("store_rejected:{:?}", e);
                    outcomes.push(rec.outcome.clone());
                    store.imip_processed_insert(&rec)?;
                    continue;
                }
            }
        }
        if decision.inbox_copy
            && (calendar.provider == Provider::Native || runtime.inbox_for_mirrored)
        {
            let _ = store.inbox_insert(&identity, &master.uid, &method, &doc.to_ics());
        }
        rec.outcome = decision.outcome.as_str();
        outcomes.push(rec.outcome.clone());
        store.imip_processed_insert(&rec)?;
    }
    Ok(outcomes)
}

/// Watch the configured folders and apply every iTIP mail that arrives.
pub fn run_inbound_watcher(
    cfg: InboundConfig,
    store_path: PathBuf,
    runtime: Arc<SchedulingRuntime>,
    shutdown: Arc<AtomicBool>,
) {
    let log = |msg: &str| eprintln!("jadav: imip-in: {}", msg);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let mut client = match MailClient::connect_imap(&cfg.imap) {
            Ok(c) => c,
            Err(e) => {
                log(&format!("connect failed: {:#}; retrying in 30s", e));
                sleep_unless_shutdown(&shutdown, Duration::from_secs(30));
                continue;
            }
        };
        log(&format!(
            "connected to {} as {}",
            cfg.imap.host, cfg.imap.login
        ));
        'session: loop {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            let store = match Store::open(&store_path) {
                Ok(s) => s,
                Err(e) => {
                    log(&format!("cannot open store: {:#}", e));
                    sleep_unless_shutdown(&shutdown, Duration::from_secs(30));
                    continue;
                }
            };
            for folder in &cfg.folders {
                let (uidvalidity, _) = match client.select_folder_info(folder) {
                    Ok(v) => v,
                    Err(e) => {
                        log(&format!("SELECT {} failed: {:#}; reconnecting", folder, e));
                        break 'session;
                    }
                };
                let cursor = store.imap_cursor(folder).ok().flatten();
                let uids = match cursor {
                    Some((v, last)) if v == uidvalidity => client.search_uids_after(last),
                    _ => {
                        let since = (Utc::now()
                            - chrono::Duration::days(i64::from(cfg.since_days)))
                        .date_naive();
                        client.search_uids_since(since)
                    }
                };
                let uids = match uids {
                    Ok(u) => u,
                    Err(e) => {
                        log(&format!(
                            "search in {} failed: {:#}; reconnecting",
                            folder, e
                        ));
                        break 'session;
                    }
                };
                if uids.is_empty() {
                    if cursor.is_none() || cursor.is_some_and(|(v, _)| v != uidvalidity) {
                        let _ = store.set_imap_cursor(folder, uidvalidity, 0);
                    }
                    continue;
                }
                let messages = match client.fetch_new_bodies(&uids) {
                    Ok(m) => m,
                    Err(e) => {
                        log(&format!(
                            "fetch in {} failed: {:#}; reconnecting",
                            folder, e
                        ));
                        break 'session;
                    }
                };
                let mut max_uid = cursor.map(|(_, l)| l).unwrap_or(0);
                for m in &messages {
                    match process_message(
                        &store,
                        &runtime,
                        &cfg,
                        folder,
                        uidvalidity,
                        m.uid,
                        &m.keywords,
                        &m.body,
                        Utc::now(),
                    ) {
                        Ok(outcomes) => {
                            for o in outcomes {
                                log(&format!("{}#{} -> {}", folder, m.uid, o));
                            }
                        }
                        Err(e) => log(&format!("{}#{} failed: {:#}", folder, m.uid, e)),
                    }
                    max_uid = max_uid.max(m.uid);
                }
                let max_uid = max_uid.max(uids.iter().copied().max().unwrap_or(0));
                let _ = store.set_imap_cursor(folder, uidvalidity, max_uid);
            }
            drop(store);
            // Wait for news on the first folder (IDLE), polling the rest.
            if let Some(first) = cfg.folders.first() {
                if client.select_folder(first).is_err() {
                    break 'session;
                }
                if client.wait_for_changes(cfg.poll_interval).is_err() {
                    break 'session;
                }
            } else {
                sleep_unless_shutdown(&shutdown, cfg.poll_interval);
            }
        }
        sleep_unless_shutdown(&shutdown, Duration::from_secs(5));
    }
}

fn sleep_unless_shutdown(shutdown: &AtomicBool, total: Duration) {
    let step = Duration::from_millis(250);
    let mut waited = Duration::ZERO;
    while waited < total {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(step);
        waited += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jadav::store::SendVia;

    fn runtime() -> SchedulingRuntime {
        let mut names = HashMap::new();
        names.insert("me@example.com".to_string(), "Me Myself".to_string());
        SchedulingRuntime {
            identities: vec!["me@example.com".into(), "alt@example.com".into()],
            identity_names: names,
            significant_properties: itip::DEFAULT_SIGNIFICANT_PROPERTIES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            prodid: itip::DEFAULT_PRODID.to_string(),
            templates: SubjectTemplates::default(),
            default_timezone: "Europe/Budapest".into(),
            message_id_domain: "example.com".into(),
            inbox_for_mirrored: false,
            retry_max_age_secs: 86_400,
            mail_enabled: true,
            outbound_wake: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    fn sample_message(kind: MessageKind, method: ItipMethod) -> ItipMessage {
        ItipMessage {
            method,
            kind,
            sender: "me@example.com".into(),
            recipient: "bob@example.com".into(),
            recipient_cn: Some("Bob".into()),
            uid: "u1".into(),
            sequence: 2,
            recurrence_id: None,
            summary: "Planning & review".into(),
            dtstart: EventTime::with_tz(
                Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
                "Europe/Budapest",
            ),
            dtend: EventTime::with_tz(
                Utc.with_ymd_and_hms(2026, 9, 16, 11, 0, 0).unwrap(),
                "Europe/Budapest",
            ),
            location: "Room 1".into(),
            description: "Bring ünïcode".into(),
            organizer: "me@example.com".into(),
            ics: format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nMETHOD:{}\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTART:20260916T080000Z\r\nDTEND:20260916T090000Z\r\nSUMMARY:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                method.as_str(),
                "x".repeat(120)
            ),
        }
    }

    #[test]
    fn format_when_renders_local_time_and_all_day() {
        let s = EventTime::with_tz(
            Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
            "Europe/Budapest",
        );
        let e = EventTime::with_tz(
            Utc.with_ymd_and_hms(2026, 9, 16, 11, 0, 0).unwrap(),
            "Europe/Budapest",
        );
        let (when, tz) = format_when(&s, &e, "UTC");
        assert_eq!(when, "Wed 16 Sep 2026 12:00 – 13:00");
        assert_eq!(tz, "CEST");
        let d = EventTime::all_day(chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap());
        let d2 = EventTime::all_day(chrono::NaiveDate::from_ymd_opt(2026, 9, 18).unwrap());
        assert_eq!(
            format_when(&d, &d2, "UTC"),
            (
                "Wed 16 Sep 2026 – Thu 17 Sep 2026".to_string(),
                String::new()
            )
        );
        let u = EventTime::utc(Utc.with_ymd_and_hms(2026, 9, 16, 22, 0, 0).unwrap());
        let u2 = EventTime::utc(Utc.with_ymd_and_hms(2026, 9, 17, 1, 0, 0).unwrap());
        let (when, tz) = format_when(&u, &u2, "Europe/Budapest");
        assert_eq!(when, "Thu 17 Sep 2026 00:00 – 03:00");
        assert_eq!(tz, "CEST");
    }

    #[test]
    fn imip_message_has_the_expected_mime_shape_and_headers() {
        let rt = runtime();
        let msg = sample_message(MessageKind::Invitation, ItipMethod::Request);
        let from = rt.identity_display("ME@example.com");
        let mid = new_message_id("u1", 2, ItipMethod::Request, Utc::now(), "example.com");
        assert!(mid.starts_with("<jadav.") && mid.ends_with("@example.com>"));
        let mail =
            build_imip_message(&msg, &from, &rt.templates, Some("<orig@x>"), &mid, "UTC").unwrap();
        let raw = mail.formatted();
        let parsed = mailparse::parse_mail(&raw).unwrap();
        let hdr = |name: &str| {
            parsed
                .headers
                .iter()
                .find(|h| h.get_key().eq_ignore_ascii_case(name))
                .map(|h| h.get_value())
        };
        assert_eq!(
            hdr("From").as_deref(),
            Some("\"Me Myself\" <me@example.com>")
        );
        assert_eq!(hdr("To").as_deref(), Some("Bob <bob@example.com>"));
        assert_eq!(
            hdr("Reply-To").as_deref(),
            Some("\"Me Myself\" <me@example.com>")
        );
        assert!(
            hdr("Sender").is_none(),
            "no Sender header: the envelope must be the identity"
        );
        assert_eq!(
            hdr("Content-Class").as_deref(),
            Some("urn:content-classes:calendarmessage")
        );
        assert_eq!(hdr("In-Reply-To").as_deref(), Some("<orig@x>"));
        assert_eq!(hdr("Message-ID").as_deref(), Some(mid.as_str()));
        assert_eq!(
            hdr("Subject").as_deref(),
            Some("Invitation: Planning & review @ Wed 16 Sep 2026 12:00 – 13:00 (CEST)")
        );
        assert!(parsed.ctype.mimetype.starts_with("multipart/mixed"));
        assert_eq!(parsed.subparts.len(), 2);
        let alt = &parsed.subparts[0];
        assert_eq!(alt.ctype.mimetype, "multipart/alternative");
        assert_eq!(alt.subparts[0].ctype.mimetype, "text/plain");
        assert!(
            alt.subparts[0]
                .get_body()
                .unwrap()
                .contains("Me Myself invites you to:")
        );
        let cal = &alt.subparts[1];
        assert_eq!(cal.ctype.mimetype, "text/calendar");
        assert_eq!(
            cal.ctype.params.get("method").map(String::as_str),
            Some("REQUEST")
        );
        assert_eq!(
            cal.ctype
                .params
                .get("charset")
                .map(|s| s.to_ascii_lowercase())
                .as_deref(),
            Some("utf-8")
        );
        let body = cal.get_body().unwrap();
        assert!(body.contains("METHOD:REQUEST"));
        for line in body.split("\r\n") {
            assert!(line.len() <= 75, "{line}");
        }
        assert_eq!(parsed.subparts[1].ctype.mimetype, "application/ics");
    }

    #[test]
    fn subjects_and_bodies_per_kind() {
        let rt = runtime();
        let from = rt.identity_display("alt@example.com");
        let (when, tz) = ("Mon 1 Jan 2026 09:00 – 10:00", "CET");
        let cases = [
            (
                MessageKind::Invitation,
                ItipMethod::Request,
                "Invitation: Planning & review @ Mon 1 Jan 2026 09:00 – 10:00 (CET)",
            ),
            (
                MessageKind::UpdatedInvitation,
                ItipMethod::Request,
                "Updated invitation: Planning & review @ Mon 1 Jan 2026 09:00 – 10:00 (CET)",
            ),
            (
                MessageKind::Cancellation,
                ItipMethod::Cancel,
                "Cancelled event: Planning & review @ Mon 1 Jan 2026 09:00 – 10:00 (CET)",
            ),
            (
                MessageKind::Reply {
                    partstat: "ACCEPTED".into(),
                },
                ItipMethod::Reply,
                "Accepted: Planning & review @ Mon 1 Jan 2026 09:00 – 10:00 (CET)",
            ),
            (
                MessageKind::Reply {
                    partstat: "TENTATIVE".into(),
                },
                ItipMethod::Reply,
                "Tentatively accepted: Planning & review @ Mon 1 Jan 2026 09:00 – 10:00 (CET)",
            ),
            (
                MessageKind::Reply {
                    partstat: "DECLINED".into(),
                },
                ItipMethod::Reply,
                "Declined: Planning & review @ Mon 1 Jan 2026 09:00 – 10:00 (CET)",
            ),
        ];
        for (kind, method, expected) in cases {
            let msg = sample_message(kind.clone(), method);
            assert_eq!(subject_for(&msg, &rt.templates, when, tz, &from), expected);
            let body = plain_text_body(&msg, &from, when, tz);
            assert!(body.contains("Planning & review"));
            if let MessageKind::Reply { .. } = kind {
                assert!(body.contains("alt@example.com has"));
                assert!(
                    !body.contains("Bring ünïcode"),
                    "replies carry no description"
                );
            }
        }
        let msg = sample_message(MessageKind::Invitation, ItipMethod::Request);
        assert_eq!(
            subject_for(&msg, &rt.templates, "Mon 1 Jan 2026", "", &from),
            "Invitation: Planning & review @ Mon 1 Jan 2026"
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_for(0), Duration::from_secs(30));
        assert_eq!(backoff_for(1), Duration::from_secs(60));
        assert_eq!(backoff_for(3), Duration::from_secs(240));
        assert_eq!(backoff_for(20), Duration::from_secs(3600));
    }

    fn cal_row(slug: &str, provider: Provider) -> CalendarRow {
        CalendarRow {
            slug: slug.into(),
            display_name: slug.into(),
            description: String::new(),
            color: None,
            order: 0,
            timezone: None,
            transparent: false,
            identity: "me@example.com".into(),
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

    #[test]
    fn plan_client_write_builds_jobs_and_rewritten_document() {
        let store = Store::open_in_memory().unwrap();
        let rt = runtime();
        let cal = cal_row("personal", Provider::Native);
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\nUID:p1\r\nDTSTART:20260916T080000Z\r\nDTEND:20260916T090000Z\r\nSUMMARY:Sync\r\nSEQUENCE:0\r\nORGANIZER:mailto:me@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:carol@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let plan = plan_client_write(&store, &rt, &cal, "p1.ics", None, Some(ics), Utc::now());
        assert_eq!(plan.jobs.len(), 2);
        assert!(plan.rewritten());
        assert!(
            plan.store_ics
                .as_ref()
                .unwrap()
                .contains("SCHEDULE-STATUS=1.0")
        );
        let job = plan
            .jobs
            .iter()
            .find(|j| j.recipient == "bob@example.com")
            .unwrap();
        assert_eq!(job.method, "REQUEST");
        assert_eq!(job.sender, "me@example.com");
        assert_eq!(job.ical_uid, "p1");
        let parsed = mailparse::parse_mail(&job.raw_message).unwrap();
        assert!(
            parsed
                .headers
                .iter()
                .any(|h| h.get_key() == "Subject" && h.get_value().starts_with("Invitation: Sync"))
        );
        // A provider-scheduled calendar produces nothing.
        let mut google = cal_row("g", Provider::Google);
        google.send_via = SendVia::Provider;
        assert!(
            plan_client_write(&store, &rt, &google, "p1.ics", None, Some(ics), Utc::now())
                .jobs
                .is_empty()
        );
        // Queue it inside a put_object transaction and read it back.
        let jobs = plan.jobs.clone();
        store
            .set_principal("me@example.com", &rt.identities)
            .unwrap();
        store
            .reconcile_calendars(&[crate::jadav::store::CalendarSpec {
                slug: "personal".into(),
                display_name: "Personal".into(),
                description: None,
                color: None,
                timezone: None,
                identity: "me@example.com".into(),
                provider: Provider::Native,
                remote_account: None,
                remote_calendar_id: None,
                two_way: true,
                send_via: SendVia::Smtp,
                is_default_for_identity: true,
            }])
            .unwrap();
        store
            .put_object_with(
                "personal",
                "p1.ics",
                plan.store_ics.as_deref().unwrap(),
                Precondition::None,
                Origin::Client,
                |conn| {
                    for j in &jobs {
                        Store::enqueue_outbound_on(conn, j)?;
                    }
                    Ok(())
                },
            )
            .unwrap()
            .unwrap();
        let due = store.due_outbound(Utc::now().timestamp() + 1, 10).unwrap();
        assert_eq!(due.len(), 2);
        assert_eq!(store.outbound_counts().unwrap(), (2, 0, 0));
        // A scripted sender: one transient failure, then success.
        struct Script(Vec<Result<(), SendFailure>>);
        impl MailSender for Script {
            fn send_raw(&mut self, _f: &str, _t: &str, _r: &[u8]) -> Result<(), SendFailure> {
                self.0.remove(0)
            }
        }
        let mut sender = Script(vec![
            Err(SendFailure {
                permanent: false,
                detail: "busy".into(),
            }),
            Ok(()),
        ]);
        let now = Utc::now().timestamp();
        for item in store.due_outbound(now + 1, 10).unwrap() {
            match sender.send_raw(&item.job.sender, &item.job.recipient, &item.job.raw_message) {
                Ok(()) => {
                    store.mark_outbound_sent(item.id).unwrap();
                    stamp_status(&store, &item.job, "1.1;Scheduled");
                }
                Err(f) => store
                    .mark_outbound_retry(item.id, now + 60, &f.detail)
                    .unwrap(),
            }
        }
        assert_eq!(store.outbound_counts().unwrap(), (1, 1, 0));
        let obj = store.get_object("personal", "p1.ics").unwrap().unwrap();
        assert!(
            obj.ics.contains("SCHEDULE-STATUS=1.1;Scheduled")
                || obj.ics.contains("SCHEDULE-STATUS=\"1.1;Scheduled\""),
            "{}",
            obj.ics
        );
        assert_eq!(obj.meta.origin, Origin::System);
        assert!(store.next_outbound_due().unwrap().unwrap() > now);
    }

    #[test]
    fn route_identity_precedence() {
        let ids = vec!["me@example.com".to_string(), "alt@example.com".to_string()];
        let mut kw = HashMap::new();
        kw.insert("acct-alt".to_string(), "alt@example.com".to_string());
        let req = |attendees: &str| {
            calendar::parse_document(&format!(
                "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20260101T000000Z\r\nORGANIZER:mailto:boss@example.com\r\n{}END:VEVENT\r\nEND:VCALENDAR\r\n",
                attendees
            ))
            .unwrap()
        };
        let single = req("ATTENDEE:mailto:ME@example.com\r\n");
        assert_eq!(
            route_identity(&single, &ids, &[], &kw, ""),
            Some("me@example.com".into())
        );
        let both = req("ATTENDEE:mailto:me@example.com\r\nATTENDEE:mailto:alt@example.com\r\n");
        assert_eq!(
            route_identity(&both, &ids, &["acct-alt".into()], &kw, ""),
            Some("alt@example.com".into())
        );
        assert_eq!(
            route_identity(&both, &ids, &[], &kw, "To: Alt <alt@example.com>"),
            Some("alt@example.com".into())
        );
        assert_eq!(
            route_identity(&both, &ids, &[], &kw, ""),
            Some("me@example.com".into())
        );
        let none = req("ATTENDEE:mailto:stranger@example.com\r\n");
        assert_eq!(route_identity(&none, &ids, &[], &kw, ""), None);
        let reply = calendar::parse_document("BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20260101T000000Z\r\nORGANIZER:mailto:alt@example.com\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:bob@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();
        assert_eq!(
            route_identity(&reply, &ids, &[], &kw, ""),
            Some("alt@example.com".into())
        );
    }

    #[test]
    fn find_itip_parts_prefers_text_calendar_and_dedups() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20260101T000000Z\r\nSEQUENCE:1\r\nDTSTAMP:20260101T000000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let processed = mail::ProcessedEmail {
            from: "a@b".into(),
            to: "me@example.com".into(),
            subject: "Invitation".into(),
            date: chrono::Local::now(),
            is_unread: true,
            is_flagged: false,
            preview: String::new(),
            text_body: String::new(),
            html_body: None,
            raw_headers: String::new(),
            message_id: "<m@x>".into(),
            in_reply_to: String::new(),
            references: String::new(),
            has_attachments: true,
            attachments: vec![
                mail::AttachmentData {
                    filename: "invite.ics".into(),
                    mime_type: "application/ics".into(),
                    size_bytes: ics.len(),
                    data: ics.as_bytes().to_vec(),
                },
                mail::AttachmentData {
                    filename: "attachment1.ics".into(),
                    mime_type: "text/calendar".into(),
                    size_bytes: ics.len(),
                    data: format!("\u{feff}{}", ics).into_bytes(),
                },
                mail::AttachmentData {
                    filename: "notes.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size_bytes: 3,
                    data: b"pdf".to_vec(),
                },
            ],
        };
        let docs = find_itip_parts(&processed);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].method.as_deref(), Some("REQUEST"));
    }

    #[test]
    fn process_message_end_to_end_with_dedup_and_routing() {
        let store = Store::open_in_memory().unwrap();
        let rt = runtime();
        store
            .set_principal("me@example.com", &rt.identities)
            .unwrap();
        store
            .reconcile_calendars(&[crate::jadav::store::CalendarSpec {
                slug: "personal".into(),
                display_name: "Personal".into(),
                description: None,
                color: None,
                timezone: None,
                identity: "me@example.com".into(),
                provider: Provider::Native,
                remote_account: None,
                remote_calendar_id: None,
                two_way: true,
                send_via: SendVia::Smtp,
                is_default_for_identity: true,
            }])
            .unwrap();
        let cfg = InboundConfig {
            imap: crate::config::ImapConfig {
                host: "x".into(),
                port: 993,
                login: "l".into(),
                auth: crate::config::AuthConfig {
                    auth_type: "password".into(),
                    value: "p".into(),
                },
            },
            folders: vec!["FRESHBOX".into(), "INBOX".into()],
            since_days: 7,
            poll_interval: Duration::from_secs(60),
            keyword_identities: HashMap::new(),
        };
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//g//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:inv-1\r\nDTSTART:20260920T080000Z\r\nDTEND:20260920T090000Z\r\nSUMMARY:Lunch\r\nSEQUENCE:0\r\nDTSTAMP:20260901T000000Z\r\nORGANIZER:mailto:boss@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let raw = format!(
            "From: boss@example.com\r\nTo: me@example.com\r\nSubject: Invitation: Lunch\r\nMessage-ID: <inv-1@boss>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"B\"\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nLunch?\r\n--B\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\n{}\r\n--B--\r\n",
            ics
        );
        let now = Utc::now();
        let outcomes = process_message(
            &store,
            &rt,
            &cfg,
            "FRESHBOX",
            7,
            100,
            &["acct-local".into()],
            raw.as_bytes(),
            now,
        )
        .unwrap();
        assert_eq!(outcomes, vec!["created".to_string()]);
        let obj = store.find_object_by_uid("inv-1").unwrap().unwrap();
        assert_eq!(obj.meta.calendar_slug, "personal");
        assert_eq!(obj.meta.origin, Origin::Inbound);
        assert!(!obj.ics.contains("METHOD:"));
        assert_eq!(store.inbox_list().unwrap().len(), 1);
        assert_eq!(
            store.invitation_message_id("inv-1").unwrap().as_deref(),
            Some("<inv-1@boss>")
        );
        // The same UID again in FRESHBOX: seen by (folder, uidvalidity, uid).
        assert!(
            process_message(
                &store,
                &rt,
                &cfg,
                "FRESHBOX",
                7,
                100,
                &[],
                raw.as_bytes(),
                now
            )
            .unwrap()
            .is_empty()
        );
        // Moved to INBOX by the categorizer: Message-ID dedup.
        let again =
            process_message(&store, &rt, &cfg, "INBOX", 3, 55, &[], raw.as_bytes(), now).unwrap();
        assert_eq!(again, vec!["already_processed:created".to_string()]);
        assert_eq!(store.list_objects("personal").unwrap().len(), 1);
        // A mail without calendar parts is ignored silently.
        let plain =
            "From: a@b\r\nTo: me@example.com\r\nSubject: hi\r\nMessage-ID: <p@x>\r\n\r\nhello\r\n";
        assert!(
            process_message(
                &store,
                &rt,
                &cfg,
                "INBOX",
                3,
                56,
                &[],
                plain.as_bytes(),
                now
            )
            .unwrap()
            .is_empty()
        );
        // An invitation for a stranger records no_identity_match.
        let stranger = raw
            .replace("mailto:me@example.com", "mailto:someone@else.example")
            .replace("<inv-1@boss>", "<inv-2@boss>")
            .replace("UID:inv-1", "UID:inv-2");
        assert_eq!(
            process_message(
                &store,
                &rt,
                &cfg,
                "INBOX",
                3,
                57,
                &[],
                stranger.as_bytes(),
                now
            )
            .unwrap(),
            vec!["no_identity_match".to_string()]
        );
    }
}
