//! Another CalDAV server as a mirror remote (iCloud, Fastmail, Radicale,
//! Nextcloud, or Google's CalDAV v2 endpoint with a bearer token as a
//! fallback for the REST provider).
//!
//! Built on this crate's own [`CalDavClient`]: RFC 6578 `sync-collection`
//! with paging (`truncated`), `calendar-query` fallback for servers without
//! sync tokens, `calendar-multiget` for bodies the sync report omitted, and
//! plain `GET`/`PUT`/`DELETE` with ETag preconditions. Remote ids are hrefs,
//! remote versions are the quoted ETags — which is why the client's
//! entity-reference fix mattered: an unquoted ETag never matches.
//!
//! Two deliberate behaviours:
//!
//! - **Ghosts**: a member the server lists but refuses to serve (`404` on
//!   `GET`; Google does this for split recurring series) is remembered in
//!   `mirror_skip` keyed by its ETag and retried only when that changes, so
//!   each is logged once instead of every cycle.
//! - **`SCHEDULE-AGENT=CLIENT`** is stamped on `ORGANIZER`/`ATTENDEE` lines
//!   of everything we `PUT` when jadav sends the iTIP mail itself
//!   (`send_via: smtp`), so an auto-scheduling remote does not *also* email
//!   invitations or replies. With `send_via: provider` the lines go out as
//!   they are and the remote schedules.

use crate::caldav::{CalDavClient, CalDavError, ChangedEvent, SyncOutcome};
use crate::calendar::{self, EventTime};
use crate::jadav::config::SendVia;
use crate::jadav::mirror::{
    Capabilities, Changes, Precondition, RemoteCalendar, RemoteError, RemoteId, RemoteObject,
    RemoteVersion, VCalendarDoc, fingerprint_ics,
};
use crate::jadav::store::Store;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use std::path::PathBuf;

const MULTIGET_BATCH: usize = 50;
const MAX_PAGES: usize = 200;

pub struct CalDavRemote {
    client: CalDavClient,
    collection_url: String,
    identity: String,
    slug: String,
    store_path: PathBuf,
    send_via: SendVia,
}

impl CalDavRemote {
    pub fn new(
        client: CalDavClient,
        collection_url: &str,
        identity: &str,
        slug: &str,
        store_path: PathBuf,
        send_via: SendVia,
    ) -> Self {
        Self {
            client,
            collection_url: collection_url.to_string(),
            identity: identity.to_ascii_lowercase(),
            slug: slug.to_string(),
            store_path,
            send_via,
        }
    }

    fn map_err(e: CalDavError) -> RemoteError {
        match e {
            CalDavError::Conflict(m) => RemoteError::Conflict(m),
            CalDavError::NotFound => RemoteError::NotFound,
            CalDavError::Unsupported(m) => RemoteError::Other(anyhow!("unsupported: {}", m)),
            CalDavError::Other(err) => {
                let text = format!("{:#}", err);
                if text.contains("HTTP 401") {
                    RemoteError::NeedsReauth
                } else if text.contains("HTTP 403") {
                    RemoteError::Forbidden(text)
                } else if text.contains("HTTP 5")
                    || text.contains("connecting to")
                    || text.contains("HTTP 429")
                {
                    RemoteError::Transient(err)
                } else {
                    RemoteError::Other(err)
                }
            }
        }
    }

    fn object_from(&self, href: &str, etag: Option<&str>, ics: &str) -> Result<RemoteObject> {
        let doc = VCalendarDoc::parse(ics)?;
        let version = etag
            .map(str::to_string)
            .unwrap_or_else(|| fingerprint_ics(ics));
        Ok(RemoteObject {
            id: RemoteId(href.to_string()),
            version: RemoteVersion(version),
            uid: doc.uid,
            fingerprint: fingerprint_ics(ics),
            ics: ics.to_string(),
        })
    }

    fn outgoing(&self, ics: &str) -> String {
        match self.send_via {
            SendVia::Smtp => stamp_schedule_agent_client(ics),
            SendVia::Provider => strip_schedule_agent(ics),
        }
    }

    /// Turn listed members into objects: use inline bodies, multiget the
    /// rest, `GET` stragglers, and remember ghosts.
    fn materialise(&mut self, listed: Vec<ChangedEvent>) -> Result<Vec<RemoteObject>, RemoteError> {
        let store = Store::open(&self.store_path).map_err(RemoteError::Other)?;
        let mut out = Vec::new();
        let mut missing: Vec<ChangedEvent> = Vec::new();
        for item in listed {
            let etag = item.etag.clone().unwrap_or_default();
            if let Ok(Some(skipped)) = store.mirror_skip_get(&self.slug, &item.href)
                && skipped == etag
            {
                continue;
            }
            match &item.calendar_data {
                Some(body) if !body.trim().is_empty() => {
                    match self.object_from(&item.href, item.etag.as_deref(), body) {
                        Ok(o) => out.push(o),
                        Err(e) => {
                            let _ = store.mirror_skip_put(
                                &self.slug,
                                &item.href,
                                &etag,
                                &format!("unparsable: {}", e),
                            );
                            eprintln!("jadav: mirror[{}] skipping {}: {}", self.slug, item.href, e);
                        }
                    }
                }
                _ => missing.push(item),
            }
        }
        for batch in missing.chunks(MULTIGET_BATCH) {
            let hrefs: Vec<String> = batch.iter().map(|i| i.href.clone()).collect();
            let fetched = self
                .client
                .multiget(&self.collection_url, &hrefs)
                .map_err(Self::map_err)?;
            let mut got: std::collections::HashSet<String> = std::collections::HashSet::new();
            for f in fetched {
                if let Some(body) = &f.calendar_data
                    && !body.trim().is_empty()
                {
                    got.insert(crate::httpc::percent_decode(&f.href));
                    match self.object_from(&f.href, f.etag.as_deref(), body) {
                        Ok(o) => out.push(o),
                        Err(e) => {
                            let _ = store.mirror_skip_put(
                                &self.slug,
                                &f.href,
                                &f.etag.clone().unwrap_or_default(),
                                &format!("unparsable: {}", e),
                            );
                        }
                    }
                }
            }
            for item in batch {
                if got.contains(&crate::httpc::percent_decode(&item.href)) {
                    continue;
                }
                match self.client.get_event(&item.href) {
                    Ok((etag, body)) => match self.object_from(
                        &item.href,
                        etag.as_deref().or(item.etag.as_deref()),
                        &body,
                    ) {
                        Ok(o) => out.push(o),
                        Err(e) => {
                            let _ = store.mirror_skip_put(
                                &self.slug,
                                &item.href,
                                &item.etag.clone().unwrap_or_default(),
                                &format!("unparsable: {}", e),
                            );
                        }
                    },
                    Err(CalDavError::NotFound) => {
                        // Listed but not served: a ghost. Remember its ETag.
                        let _ = store.mirror_skip_put(
                            &self.slug,
                            &item.href,
                            &item.etag.clone().unwrap_or_default(),
                            "listed but 404 on GET",
                        );
                        eprintln!(
                            "jadav: mirror[{}] {} is listed but not served; skipping until it changes",
                            self.slug, item.href
                        );
                    }
                    Err(e) => return Err(Self::map_err(e)),
                }
            }
        }
        Ok(out)
    }
}

/// Add `SCHEDULE-AGENT=CLIENT` to every `ORGANIZER`/`ATTENDEE` line that
/// lacks a `SCHEDULE-AGENT` parameter.
pub fn stamp_schedule_agent_client(ics: &str) -> String {
    rewrite_lines(ics, |name, line| {
        if name != "ORGANIZER" && name != "ATTENDEE" {
            return None;
        }
        if has_param(line, "SCHEDULE-AGENT") {
            return None;
        }
        Some(set_param(line, "SCHEDULE-AGENT", Some("CLIENT")))
    })
}

/// Remove `SCHEDULE-AGENT` from every `ORGANIZER`/`ATTENDEE` line.
pub fn strip_schedule_agent(ics: &str) -> String {
    rewrite_lines(ics, |name, line| {
        if (name == "ORGANIZER" || name == "ATTENDEE") && has_param(line, "SCHEDULE-AGENT") {
            Some(set_param(line, "SCHEDULE-AGENT", None))
        } else {
            None
        }
    })
}

/// Rewrite `identity`'s own `ATTENDEE` line(s) in the component matching
/// `rid` (`None` = the master) to `partstat`, dropping `RSVP`. `None` when
/// no such attendee exists.
pub fn rewrite_own_partstat(
    ics: &str,
    identity: &str,
    partstat: &str,
    rid: Option<&EventTime>,
    keep_schedule_agent: bool,
) -> Option<String> {
    let me = identity.to_ascii_lowercase();
    let lines = calendar::unfold(ics);
    // Find component ranges at depth 1 inside VCALENDAR (or depth 0 for a
    // bare VEVENT) and their RECURRENCE-ID.
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut changed = false;
    while i < lines.len() {
        if lines[i].eq_ignore_ascii_case("BEGIN:VEVENT") {
            let start = i;
            let mut depth = 1;
            i += 1;
            while i < lines.len() && depth > 0 {
                if crate::calendar::starts_with_ignore_case(&lines[i], "BEGIN:") {
                    depth += 1;
                } else if crate::calendar::starts_with_ignore_case(&lines[i], "END:") {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                i += 1;
            }
            let end = i.min(lines.len() - 1);
            let block: Vec<String> = lines[start..=end].to_vec();
            let block_text = block.join("\r\n");
            let block_rid = calendar::raw_property_lines(&block_text, &["RECURRENCE-ID"])
                .first()
                .and_then(|l| calendar::parse_property_time(l).ok());
            let matches = match (rid, &block_rid) {
                (None, None) => true,
                (Some(r), Some(b)) => r.utc == b.utc && r.all_day == b.all_day,
                _ => false,
            };
            if matches {
                let mut depth_in = 0i32;
                for line in &block {
                    if crate::calendar::starts_with_ignore_case(line, "BEGIN:") {
                        depth_in += 1;
                    } else if crate::calendar::starts_with_ignore_case(line, "END:") {
                        depth_in -= 1;
                    }
                    let is_top_attendee = depth_in == 1
                        && calendar::parse_content_line(line).is_some_and(|cl| {
                            cl.name == "ATTENDEE"
                                && cl
                                    .value
                                    .trim()
                                    .to_ascii_lowercase()
                                    .trim_start_matches("mailto:")
                                    == me
                        });
                    if is_top_attendee {
                        let mut l =
                            set_param(line, "PARTSTAT", Some(&partstat.to_ascii_uppercase()));
                        l = set_param(&l, "RSVP", None);
                        if !keep_schedule_agent {
                            l = set_param(&l, "SCHEDULE-AGENT", None);
                        }
                        out.push(l);
                        changed = true;
                    } else {
                        out.push(line.clone());
                    }
                }
            } else {
                out.extend(block);
            }
            i = end + 1;
            continue;
        }
        out.push(lines[i].clone());
        i += 1;
    }
    if !changed {
        return None;
    }
    let joined = out
        .into_iter()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n";
    Some(calendar::fold_ics(&joined))
}

/// Apply `f` to every unfolded property line (given its upper-cased name);
/// `None` keeps the line. Output is re-folded.
fn rewrite_lines(ics: &str, mut f: impl FnMut(&str, &str) -> Option<String>) -> String {
    let mut out = Vec::new();
    for line in calendar::unfold(ics) {
        if line.is_empty() {
            continue;
        }
        let name = calendar::parse_content_line(&line)
            .map(|cl| cl.name)
            .unwrap_or_default();
        match f(&name, &line) {
            Some(new) => out.push(new),
            None => out.push(line),
        }
    }
    calendar::fold_ics(&(out.join("\r\n") + "\r\n"))
}

use crate::calendar::{line_has_param as has_param, set_param_on_line as set_param};

impl RemoteCalendar for CalDavRemote {
    fn list_changes(
        &mut self,
        token: Option<&str>,
        window_start: Option<DateTime<Utc>>,
    ) -> Result<Changes, RemoteError> {
        let mut listed: Vec<ChangedEvent> = Vec::new();
        let mut deleted: Vec<RemoteId> = Vec::new();
        let mut next_token: Option<String> = None;
        let mut full = token.is_none();
        let mut current = token.map(str::to_string);
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(RemoteError::Other(anyhow!(
                    "sync-collection paging never ended"
                )));
            }
            match self
                .client
                .sync_calendar(&self.collection_url, current.as_deref())
                .map_err(Self::map_err)?
            {
                SyncOutcome::Delta {
                    token: t,
                    changed,
                    deleted: gone,
                    truncated,
                } => {
                    listed.extend(changed);
                    deleted.extend(gone.into_iter().map(RemoteId));
                    next_token = t.clone().or(next_token);
                    if truncated && t.is_some() {
                        current = t;
                        continue;
                    }
                    break;
                }
                SyncOutcome::FullResyncRequired => {
                    if token.is_some() {
                        return Err(RemoteError::FullResyncRequired);
                    }
                    // No sync-collection support at all: enumerate.
                    full = true;
                    let all = match window_start {
                        Some(w) => self
                            .client
                            .list_events_in_range(&self.collection_url, w, None),
                        None => self.client.list_all_events(&self.collection_url),
                    }
                    .map_err(Self::map_err)?;
                    listed.extend(all);
                    next_token = None;
                    break;
                }
            }
        }
        let items = self.materialise(listed)?;
        Ok(Changes {
            items,
            deleted,
            next_token,
            full_resync: full,
        })
    }

    fn fetch(&mut self, id: &RemoteId) -> Result<Option<RemoteObject>, RemoteError> {
        match self.client.get_event(&id.0) {
            Ok((etag, body)) => Ok(Some(self.object_from(&id.0, etag.as_deref(), &body)?)),
            Err(CalDavError::NotFound) => Ok(None),
            Err(e) => Err(Self::map_err(e)),
        }
    }

    fn create(&mut self, doc: &VCalendarDoc) -> Result<RemoteObject, RemoteError> {
        let href = crate::caldav::new_event_href(&self.collection_url, &doc.uid)
            .map_err(RemoteError::Other)?;
        let body = self.outgoing(&doc.ics);
        self.client
            .put_event(&href, &body, None, true)
            .map_err(Self::map_err)?;
        self.fetch(&RemoteId(href.clone()))?
            .ok_or_else(|| RemoteError::Other(anyhow!("created {} but cannot read it back", href)))
    }

    fn update(
        &mut self,
        id: &RemoteId,
        doc: &VCalendarDoc,
        pre: Precondition,
    ) -> Result<RemoteObject, RemoteError> {
        let if_match = match &pre {
            Precondition::None => None,
            Precondition::IfMatch(v) => Some(v.0.clone()),
        };
        let body = self.outgoing(&doc.ics);
        self.client
            .put_event(&id.0, &body, if_match.as_deref(), false)
            .map_err(Self::map_err)?;
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
        let (etag, body) = self.client.get_event(&id.0).map_err(Self::map_err)?;
        if let Precondition::IfMatch(v) = &pre
            && etag.as_deref().is_some_and(|e| e != v.0)
        {
            return Err(RemoteError::Conflict("etag changed".to_string()));
        }
        let keep_agent = self.send_via == SendVia::Smtp;
        // The engine names the calendar's identity; fall back to the
        // provider's own configured identity if the two ever differ.
        let rewritten = rewrite_own_partstat(&body, identity, partstat, recurrence_id, keep_agent)
            .or_else(|| {
                rewrite_own_partstat(&body, &self.identity, partstat, recurrence_id, keep_agent)
            })
            .ok_or_else(|| RemoteError::Forbidden(format!("{} is not an attendee", identity)))?;
        let rewritten = if keep_agent {
            stamp_schedule_agent_client(&rewritten)
        } else {
            rewritten
        };
        self.client
            .put_event(&id.0, &rewritten, etag.as_deref(), false)
            .map_err(Self::map_err)?;
        self.fetch(id)?.ok_or(RemoteError::NotFound)
    }

    fn delete(&mut self, id: &RemoteId, pre: Precondition) -> Result<(), RemoteError> {
        let if_match = match &pre {
            Precondition::None => None,
            Precondition::IfMatch(v) => Some(v.0.clone()),
        };
        match self.client.delete_event(&id.0, if_match.as_deref()) {
            Ok(()) | Err(CalDavError::NotFound) => Ok(()),
            Err(e) => Err(Self::map_err(e)),
        }
    }

    fn find_by_uid(&mut self, uid: &str) -> Result<Option<RemoteObject>, RemoteError> {
        let href =
            crate::caldav::new_event_href(&self.collection_url, uid).map_err(RemoteError::Other)?;
        self.fetch(&RemoteId(href))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            rsvp_only_writes: true,
            server_schedules: self.send_via == SendVia::Provider,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nRRULE:FREQ=DAILY\r\nORGANIZER;CN=\"Boss; Big\":mailto:boss@example.com\r\nATTENDEE;CN=Me;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@example.com\r\nATTENDEE;CN=Other;PARTSTAT=ACCEPTED:mailto:other@example.com\r\nSUMMARY:S\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:u\r\nRECURRENCE-ID:20240116T090000Z\r\nDTSTART:20240116T110000Z\r\nDTEND:20240116T120000Z\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:MAILTO:me@example.com\r\nSUMMARY:S moved\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn set_param_replaces_inserts_removes_and_keeps_quotes() {
        let line = "ATTENDEE;CN=\"Doe; John\";PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:j@example.com";
        let a = set_param(line, "PARTSTAT", Some("ACCEPTED"));
        assert_eq!(
            a,
            "ATTENDEE;CN=\"Doe; John\";RSVP=TRUE;PARTSTAT=ACCEPTED:mailto:j@example.com"
        );
        let b = set_param(&a, "RSVP", None);
        assert_eq!(
            b,
            "ATTENDEE;CN=\"Doe; John\";PARTSTAT=ACCEPTED:mailto:j@example.com"
        );
        let c = set_param("ATTENDEE:mailto:x@y", "PARTSTAT", Some("DECLINED"));
        assert_eq!(c, "ATTENDEE;PARTSTAT=DECLINED:mailto:x@y");
        assert!(has_param(&c, "partstat"));
        assert!(!has_param(&c, "RSVP"));
        let q = set_param("X:v", "P", Some("a;b"));
        assert_eq!(q, "X;P=\"a;b\":v");
        // A value containing a colon inside quotes does not confuse the split.
        let d = set_param(
            "ATTENDEE;CN=\"a:b\":mailto:z@y",
            "PARTSTAT",
            Some("ACCEPTED"),
        );
        assert_eq!(d, "ATTENDEE;CN=\"a:b\";PARTSTAT=ACCEPTED:mailto:z@y");
    }

    #[test]
    fn stamp_and_strip_schedule_agent() {
        let stamped = stamp_schedule_agent_client(ICS);
        assert_eq!(stamped.matches("SCHEDULE-AGENT=CLIENT").count(), 4);
        assert!(
            stamped.contains(
                "ORGANIZER;CN=\"Boss; Big\";SCHEDULE-AGENT=CLIENT:mailto:boss@example.com"
            )
        );
        // Idempotent.
        assert_eq!(stamp_schedule_agent_client(&stamped), stamped);
        let stripped = strip_schedule_agent(&stamped);
        assert!(!stripped.contains("SCHEDULE-AGENT"));
        assert_eq!(stripped, calendar::fold_ics(ICS));
    }

    #[test]
    fn rewrite_own_partstat_targets_master_or_override_only() {
        let master = rewrite_own_partstat(ICS, "ME@example.com", "accepted", None, true).unwrap();
        assert!(master.contains("ATTENDEE;CN=Me;PARTSTAT=ACCEPTED:mailto:me@example.com"));
        assert!(
            master.contains("ATTENDEE;PARTSTAT=NEEDS-ACTION:MAILTO:me@example.com"),
            "override untouched"
        );
        assert!(master.contains("ATTENDEE;CN=Other;PARTSTAT=ACCEPTED:mailto:other@example.com"));
        let rid = calendar::parse_property_time("RECURRENCE-ID:20240116T090000Z").unwrap();
        let ovr =
            rewrite_own_partstat(ICS, "me@example.com", "DECLINED", Some(&rid), false).unwrap();
        assert!(ovr.contains("ATTENDEE;PARTSTAT=DECLINED:MAILTO:me@example.com"));
        assert!(
            ovr.contains("ATTENDEE;CN=Me;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@example.com"),
            "master untouched"
        );
        assert!(rewrite_own_partstat(ICS, "nobody@example.com", "ACCEPTED", None, true).is_none());
        // Result still parses with the same structure.
        let doc = VCalendarDoc::parse(&master).unwrap();
        assert_eq!(doc.overrides.len(), 1);
    }

    #[test]
    fn caldav_errors_map_to_remote_errors() {
        assert!(matches!(
            CalDavRemote::map_err(CalDavError::NotFound),
            RemoteError::NotFound
        ));
        assert!(matches!(
            CalDavRemote::map_err(CalDavError::Conflict("x".into())),
            RemoteError::Conflict(_)
        ));
        assert!(matches!(
            CalDavRemote::map_err(CalDavError::Other(anyhow!(
                "PROPFIND failed: HTTP 401 — nope"
            ))),
            RemoteError::NeedsReauth
        ));
        assert!(matches!(
            CalDavRemote::map_err(CalDavError::Other(anyhow!("PUT failed: HTTP 503 — busy"))),
            RemoteError::Transient(_)
        ));
        assert!(matches!(
            CalDavRemote::map_err(CalDavError::Other(anyhow!("PUT failed: HTTP 403 — no"))),
            RemoteError::Forbidden(_)
        ));
    }
}
