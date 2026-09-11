//! A synchronous CalDAV (RFC 4791) client built on [`crate::httpc`], plus
//! just enough WebDAV (RFC 4918) XML handling — via `quick-xml` — to drive
//! calendar discovery and change sync.
//!
//! ## What's implemented
//!
//! - **Discovery** ([`CalDavClient::discover_calendars`]): the standard
//!   `current-user-principal` -> `calendar-home-set` -> child-collection
//!   chain (RFC 4791 §5.2/§6.2, via `PROPFIND`). If a server doesn't expose
//!   `current-user-principal` (or the configured URL already *is* a
//!   calendar-home collection — common when a user pastes that URL
//!   directly), discovery falls back to treating the configured URL itself
//!   as the home set and listing its children.
//! - **Change sync** ([`CalDavClient::sync_calendar`]): RFC 6578
//!   `sync-collection` `REPORT`, requesting `getetag` and `calendar-data`
//!   together so a single round trip yields both change metadata and
//!   content. When a server rejects the request (unsupported, or an
//!   expired/invalid `sync-token` — servers vary in exactly which status
//!   they use: `400`, `403`, `405`, `501`, and `507` are all treated as
//!   "give up on the token, do a full listing") the caller falls back to
//!   [`CalDavClient::list_all_events`] (a `calendar-query` `REPORT` for
//!   every `VEVENT`) and reconciles deletions itself by diffing hrefs.
//! - **Read/write** ([`get_event`], [`put_event`], [`delete_event`]):
//!   plain `GET`, and `PUT`/`DELETE` with `If-Match`/`If-None-Match`
//!   conditional headers so a stale local copy can't silently clobber a
//!   change made elsewhere — a `412 Precondition Failed` (or `409
//!   Conflict`, which some servers use for the same case) comes back as
//!   [`CalDavError::Conflict`], never silently retried or ignored.
//!
//! ## Explicit limitations
//!
//! - **Authentication is HTTP Basic only.** No Digest, OAuth2, or
//!   client-certificate support. Basic auth over TLS is what the large
//!   majority of self-hosted CalDAV servers (Radicale, Baikal, Nextcloud)
//!   and Fastmail support; this is the same scope decision this crate
//!   already made for IMAP/SMTP (`password`/`command` auth only).
//! - **No `VFREEBUSY`/scheduling (`iTIP`) support** — creating an event
//!   with `ATTENDEE`s does not send invitations; it only stores the
//!   `VEVENT` as given.
//! - **A single calendar-home-set is assumed reachable via one PROPFIND
//!   chain from one configured URL.** Multi-principal setups (e.g. a
//!   shared/delegated calendar under a different principal) aren't
//!   auto-discovered — configure that calendar's collection URL directly
//!   and calendar discovery still works via the "already a home set"
//!   fallback.

use crate::httpc::{self, HttpResponse, HttpUrl};
use anyhow::{Context, Result, anyhow, bail};
use quick_xml::Reader;
use quick_xml::events::Event as XmlEvent;
use std::fmt;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum CalDavError {
    /// An `If-Match`/`If-None-Match` precondition failed (HTTP 412, or 409
    /// on servers that use that instead) — the remote copy changed (or, for
    /// a create, already exists) since the local copy was last read.
    Conflict(String),
    /// The requested resource is gone (HTTP 404) — treated specially by
    /// [`delete_event`] (idempotent) but surfaced as an error elsewhere.
    NotFound,
    /// The server doesn't support a feature this client needs (e.g. no
    /// calendar collections discoverable at all).
    Unsupported(String),
    Other(anyhow::Error),
}

impl fmt::Display for CalDavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalDavError::Conflict(msg) => write!(f, "CalDAV conflict: {}", msg),
            CalDavError::NotFound => write!(f, "CalDAV resource not found"),
            CalDavError::Unsupported(msg) => write!(f, "CalDAV server does not support: {}", msg),
            CalDavError::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for CalDavError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CalDavError::Other(e) => e.source(),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for CalDavError {
    fn from(e: anyhow::Error) -> Self {
        CalDavError::Other(e)
    }
}

#[derive(Debug)]
pub struct DiscoveredCalendar {
    pub url: String,
    pub display_name: String,
    pub ctag: Option<String>,
}

pub struct ChangedEvent {
    pub href: String,
    pub etag: Option<String>,
    /// Present when the server returned `calendar-data` inline (requested
    /// on every sync/list call this client makes); `None` only if a
    /// server ignores that request, in which case the caller must `GET`
    /// the href separately.
    pub calendar_data: Option<String>,
}

pub enum SyncOutcome {
    /// The server understood the sync token (or this was an initial sync
    /// with an empty one) and returned only what changed since then.
    Delta {
        token: Option<String>,
        changed: Vec<ChangedEvent>,
        deleted: Vec<String>,
    },
    /// The server rejected the token or doesn't support `sync-collection`
    /// at all; the caller should fall back to [`CalDavClient::list_all_events`].
    FullResyncRequired,
}

pub struct CalDavClient {
    base_url: HttpUrl,
    login: String,
    password: String,
    timeout: Duration,
}

impl CalDavClient {
    pub fn new(base_url: &str, login: &str, password: &str) -> Result<Self> {
        Ok(Self {
            base_url: HttpUrl::parse(base_url)?,
            login: login.to_string(),
            password: password.to_string(),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    fn request(
        &self,
        method: &str,
        url: &HttpUrl,
        mut headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse> {
        headers.push((
            "Authorization".to_string(),
            httpc::basic_auth_header(&self.login, &self.password),
        ));
        httpc::send(method, url, &headers, body.as_deref(), self.timeout)
    }

    fn propfind(&self, url: &HttpUrl, depth: u8, body: &str) -> Result<MultiStatusDoc> {
        let resp = self.request(
            "PROPFIND",
            url,
            vec![
                ("Depth".to_string(), depth.to_string()),
                (
                    "Content-Type".to_string(),
                    "application/xml; charset=utf-8".to_string(),
                ),
            ],
            Some(body.as_bytes().to_vec()),
        )?;
        if resp.status != 207 {
            bail!(httpc::err_status(
                "PROPFIND failed",
                resp.status,
                &resp.body_str()
            ));
        }
        parse_multistatus(&resp.body_str())
    }

    /// RFC 4791 discovery chain: `current-user-principal` ->
    /// `calendar-home-set` -> child calendar collections. See module docs
    /// for the fallback behavior when a step is unsupported.
    pub fn discover_calendars(&self) -> Result<Vec<DiscoveredCalendar>, CalDavError> {
        let principal_href = self
            .propfind(&self.base_url, 0, PRINCIPAL_PROPFIND_BODY)
            .ok()
            .and_then(|doc| doc.responses.into_iter().next())
            .and_then(|r| r.current_user_principal);

        let home_set_url = match principal_href {
            Some(href) => {
                let purl = self.base_url.resolve(&href)?;
                self.propfind(&purl, 0, HOMESET_PROPFIND_BODY)
                    .ok()
                    .and_then(|doc| doc.responses.into_iter().next())
                    .and_then(|r| r.calendar_home_set)
                    .map(|h| self.base_url.resolve(&h))
                    .transpose()?
            }
            None => None,
        };
        let home_url = home_set_url.unwrap_or_else(|| self.base_url.clone());

        let doc = self.propfind(&home_url, 1, CALENDAR_LIST_PROPFIND_BODY)?;
        let mut out = Vec::new();
        for r in doc.responses {
            if r.is_calendar_collection
                && let Some(href) = r.href
            {
                let resolved = self.base_url.resolve(&href)?;
                out.push(DiscoveredCalendar {
                    url: resolved.to_absolute_string(),
                    display_name: r.displayname.unwrap_or_else(|| href.clone()),
                    ctag: r.ctag,
                });
            }
        }
        if out.is_empty() {
            return Err(CalDavError::Unsupported(format!(
                "no calendar collections discovered under {}",
                home_url.to_absolute_string()
            )));
        }
        Ok(out)
    }

    /// RFC 6578 `sync-collection` REPORT. `prior_token` is `None`/empty for
    /// an initial sync.
    pub fn sync_calendar(
        &self,
        calendar_url: &str,
        prior_token: Option<&str>,
    ) -> Result<SyncOutcome, CalDavError> {
        let url = HttpUrl::parse(calendar_url)?;
        let body = sync_collection_body(prior_token);
        let resp = self.request(
            "REPORT",
            &url,
            vec![
                ("Depth".to_string(), "1".to_string()),
                (
                    "Content-Type".to_string(),
                    "application/xml; charset=utf-8".to_string(),
                ),
            ],
            Some(body.into_bytes()),
        )?;
        match resp.status {
            200 | 207 => {
                let doc = parse_multistatus(&resp.body_str())?;
                let mut changed = Vec::new();
                let mut deleted = Vec::new();
                for r in doc.responses {
                    let Some(href) = r.href else { continue };
                    if let Some(code) = r.response_status
                        && code == 404
                    {
                        deleted.push(href);
                        continue;
                    }
                    changed.push(ChangedEvent {
                        href,
                        etag: r.etag,
                        calendar_data: r.calendar_data,
                    });
                }
                Ok(SyncOutcome::Delta {
                    token: doc.sync_token,
                    changed,
                    deleted,
                })
            }
            400 | 403 | 405 | 501 | 507 => Ok(SyncOutcome::FullResyncRequired),
            other => Err(CalDavError::Other(httpc::err_status(
                "sync-collection REPORT failed",
                other,
                &resp.body_str(),
            ))),
        }
    }

    /// Full `calendar-query` REPORT for every `VEVENT` in the collection —
    /// the fallback path when [`sync_calendar`](Self::sync_calendar)
    /// reports [`SyncOutcome::FullResyncRequired`], and also usable for a
    /// first sync against a server with no `sync-collection` support at
    /// all.
    pub fn list_all_events(&self, calendar_url: &str) -> Result<Vec<ChangedEvent>, CalDavError> {
        let url = HttpUrl::parse(calendar_url)?;
        let resp = self.request(
            "REPORT",
            &url,
            vec![
                ("Depth".to_string(), "1".to_string()),
                (
                    "Content-Type".to_string(),
                    "application/xml; charset=utf-8".to_string(),
                ),
            ],
            Some(CALENDAR_QUERY_VEVENT_BODY.as_bytes().to_vec()),
        )?;
        if resp.status != 207 && resp.status != 200 {
            return Err(CalDavError::Other(httpc::err_status(
                "calendar-query REPORT failed",
                resp.status,
                &resp.body_str(),
            )));
        }
        let doc = parse_multistatus(&resp.body_str())?;
        Ok(doc
            .responses
            .into_iter()
            .filter_map(|r| {
                r.href.map(|href| ChangedEvent {
                    href,
                    etag: r.etag,
                    calendar_data: r.calendar_data,
                })
            })
            .collect())
    }

    pub fn get_event(&self, href: &str) -> Result<(Option<String>, String), CalDavError> {
        let url = self.base_url.resolve(href)?;
        let resp = self.request("GET", &url, vec![], None)?;
        match resp.status {
            200 => Ok((resp.header("etag").map(|s| s.to_string()), resp.body_str())),
            404 => Err(CalDavError::NotFound),
            other => Err(CalDavError::Other(httpc::err_status(
                "GET event failed",
                other,
                &resp.body_str(),
            ))),
        }
    }

    /// `PUT` an event body. `if_match` should be `Some(etag)` for an update
    /// (fails with [`CalDavError::Conflict`] if the remote copy's ETag
    /// differs) or `None` with `create` set for a brand-new resource
    /// (`If-None-Match: *`, fails with [`CalDavError::Conflict`] if a
    /// resource already exists at `href` — e.g. a UID collision).
    pub fn put_event(
        &self,
        href: &str,
        ics: &str,
        if_match: Option<&str>,
        create: bool,
    ) -> Result<String, CalDavError> {
        let url = self.base_url.resolve(href)?;
        let mut headers = vec![(
            "Content-Type".to_string(),
            "text/calendar; charset=utf-8".to_string(),
        )];
        if let Some(etag) = if_match {
            headers.push(("If-Match".to_string(), etag.to_string()));
        } else if create {
            headers.push(("If-None-Match".to_string(), "*".to_string()));
        }
        let resp = self.request("PUT", &url, headers, Some(ics.as_bytes().to_vec()))?;
        match resp.status {
            200 | 201 | 204 => match resp.header("etag") {
                Some(etag) => Ok(etag.to_string()),
                // Some servers don't return ETag on PUT; re-GET to learn it.
                None => {
                    let (etag, _) = self.get_event(href)?;
                    etag.ok_or_else(|| {
                        CalDavError::Other(anyhow!(
                            "server returned no ETag for {} after PUT, and none on re-GET",
                            href
                        ))
                    })
                }
            },
            412 | 409 => Err(CalDavError::Conflict(format!(
                "PUT {} rejected: {}",
                href,
                resp.body_str()
            ))),
            other => Err(CalDavError::Other(httpc::err_status(
                "PUT event failed",
                other,
                &resp.body_str(),
            ))),
        }
    }

    /// `DELETE` an event. `if_match: None` deletes unconditionally.
    /// A `404` is treated as success (already gone — idempotent).
    pub fn delete_event(&self, href: &str, if_match: Option<&str>) -> Result<(), CalDavError> {
        let url = self.base_url.resolve(href)?;
        let mut headers = Vec::new();
        if let Some(etag) = if_match {
            headers.push(("If-Match".to_string(), etag.to_string()));
        }
        let resp = self.request("DELETE", &url, headers, None)?;
        match resp.status {
            200 | 202 | 204 | 404 => Ok(()),
            412 | 409 => Err(CalDavError::Conflict(format!(
                "DELETE {} rejected: {}",
                href,
                resp.body_str()
            ))),
            other => Err(CalDavError::Other(httpc::err_status(
                "DELETE event failed",
                other,
                &resp.body_str(),
            ))),
        }
    }
}

/// Build a resource href for a brand-new event: `{calendar_url}{uid}.ics`.
pub fn new_event_href(calendar_url: &str, uid: &str) -> Result<String> {
    let url = HttpUrl::parse(calendar_url).context("invalid calendar URL")?;
    let sanitized: String = uid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = if url.path_and_query.ends_with('/') {
        url.path_and_query.clone()
    } else {
        format!("{}/", url.path_and_query)
    };
    Ok(format!("{}{}.ics", dir, sanitized))
}

// ---------------------------------------------------------------------
// WebDAV multistatus XML parsing
// ---------------------------------------------------------------------

#[derive(Default)]
struct ResponseAccum {
    href: Option<String>,
    response_status: Option<u16>,
    etag: Option<String>,
    calendar_data: Option<String>,
    displayname: Option<String>,
    ctag: Option<String>,
    is_calendar_collection: bool,
    current_user_principal: Option<String>,
    calendar_home_set: Option<String>,
}

#[derive(Default)]
struct MultiStatusDoc {
    sync_token: Option<String>,
    responses: Vec<ResponseAccum>,
}

#[derive(Default)]
struct PropStatBuf {
    etag: Option<String>,
    calendar_data: Option<String>,
    displayname: Option<String>,
    ctag: Option<String>,
    is_calendar: bool,
    current_user_principal: Option<String>,
    calendar_home_set: Option<String>,
    status_ok: bool,
}

fn local_name(qname: &str) -> String {
    qname
        .rsplit(':')
        .next()
        .unwrap_or(qname)
        .to_ascii_lowercase()
}

fn parse_http_status_line(text: &str) -> Option<u16> {
    text.split_whitespace().nth(1).and_then(|c| c.parse().ok())
}

fn parse_multistatus(xml: &str) -> Result<MultiStatusDoc> {
    let mut reader = Reader::from_str(xml);
    let mut doc = MultiStatusDoc::default();
    let mut cur: Option<ResponseAccum> = None;
    let mut propstat_depth: i32 = 0;
    let mut buf = PropStatBuf {
        status_ok: true,
        ..Default::default()
    };
    let mut tag_stack: Vec<String> = Vec::new();
    let mut text = String::new();

    loop {
        match reader.read_event().context("parsing WebDAV XML response")? {
            XmlEvent::Eof => break,
            XmlEvent::Start(e) => {
                let name = local_name(e.name().as_ref());
                text.clear();
                if name == "response" {
                    cur = Some(ResponseAccum::default());
                }
                if name == "propstat" {
                    propstat_depth += 1;
                    buf = PropStatBuf {
                        status_ok: true,
                        ..Default::default()
                    };
                }
                tag_stack.push(name);
            }
            XmlEvent::Empty(e) => {
                let name = local_name(e.name().as_ref());
                if name == "calendar"
                    && tag_stack.last().map(String::as_str) == Some("resourcetype")
                {
                    buf.is_calendar = true;
                }
            }
            XmlEvent::Text(e) => {
                let raw: &str = e.as_ref();
                match quick_xml::escape::unescape(raw) {
                    Ok(decoded) => text.push_str(&decoded),
                    Err(_) => text.push_str(raw),
                }
            }
            XmlEvent::CData(e) => {
                let raw: &str = e.as_ref();
                text.push_str(raw);
            }
            XmlEvent::End(e) => {
                let name = local_name(e.name().as_ref());
                tag_stack.pop();
                let parent = tag_stack.last().cloned();

                match name.as_str() {
                    "href" => match parent.as_deref() {
                        Some("current-user-principal") => {
                            buf.current_user_principal = Some(text.trim().to_string())
                        }
                        Some("calendar-home-set") => {
                            buf.calendar_home_set = Some(text.trim().to_string())
                        }
                        _ => {
                            if let Some(c) = cur.as_mut() {
                                c.href = Some(text.trim().to_string());
                            }
                        }
                    },
                    "getetag" => buf.etag = Some(text.trim().to_string()),
                    "calendar-data" => buf.calendar_data = Some(text.clone()),
                    "displayname" => buf.displayname = Some(text.trim().to_string()),
                    "getctag" => buf.ctag = Some(text.trim().to_string()),
                    "sync-token" => doc.sync_token = Some(text.trim().to_string()),
                    "status" => {
                        let code = parse_http_status_line(text.trim());
                        if propstat_depth > 0 {
                            buf.status_ok = code.map(|c| (200..300).contains(&c)).unwrap_or(false);
                        } else if let Some(c) = cur.as_mut() {
                            c.response_status = code;
                        }
                    }
                    "propstat" => {
                        propstat_depth -= 1;
                        if buf.status_ok
                            && let Some(c) = cur.as_mut()
                        {
                            if buf.etag.is_some() {
                                c.etag = buf.etag.take();
                            }
                            if buf.calendar_data.is_some() {
                                c.calendar_data = buf.calendar_data.take();
                            }
                            if buf.displayname.is_some() {
                                c.displayname = buf.displayname.take();
                            }
                            if buf.ctag.is_some() {
                                c.ctag = buf.ctag.take();
                            }
                            if buf.is_calendar {
                                c.is_calendar_collection = true;
                            }
                            if buf.current_user_principal.is_some() {
                                c.current_user_principal = buf.current_user_principal.take();
                            }
                            if buf.calendar_home_set.is_some() {
                                c.calendar_home_set = buf.calendar_home_set.take();
                            }
                        }
                    }
                    "response" => {
                        if let Some(c) = cur.take() {
                            doc.responses.push(c);
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            _ => {}
        }
    }
    Ok(doc)
}

const PRINCIPAL_PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:current-user-principal/>
  </D:prop>
</D:propfind>"#;

const HOMESET_PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <C:calendar-home-set/>
  </D:prop>
</D:propfind>"#;

const CALENDAR_LIST_PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
    <CS:getctag/>
  </D:prop>
</D:propfind>"#;

const CALENDAR_QUERY_VEVENT_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;

fn sync_collection_body(prior_token: Option<&str>) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8" ?>
<D:sync-collection xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:sync-token>{}</D:sync-token>
  <D:sync-level>1</D:sync-level>
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
</D:sync-collection>"#,
        prior_token.unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// A tiny scripted mock CalDAV server: accepts connections in a loop
    /// and, for each, reads one request and writes the next canned
    /// response from `responses`. Returns the accumulated raw requests
    /// (one string per connection) so tests can assert on what the client
    /// actually sent (method, headers, body).
    fn mock_server(responses: Vec<&'static str>) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let mut received = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            received.extend_from_slice(&buf[..n]);
                            if received.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                requests.push(String::from_utf8_lossy(&received).into_owned());
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
            requests
        });
        (format!("http://{}", addr), handle)
    }

    fn client_for(base: &str) -> CalDavClient {
        CalDavClient::new(base, "alice", "secret").unwrap()
    }

    #[test]
    fn discover_calendars_follows_principal_and_homeset_chain() {
        let principal_resp = "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {LEN}\r\n\r\n{BODY}"
            .replace("{BODY}", PRINCIPAL_RESPONSE_XML)
            .replace("{LEN}", &PRINCIPAL_RESPONSE_XML.len().to_string());
        let homeset_resp = "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {LEN}\r\n\r\n{BODY}"
            .replace("{BODY}", HOMESET_RESPONSE_XML)
            .replace("{LEN}", &HOMESET_RESPONSE_XML.len().to_string());
        let list_resp = "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {LEN}\r\n\r\n{BODY}"
            .replace("{BODY}", CALENDAR_LIST_RESPONSE_XML)
            .replace("{LEN}", &CALENDAR_LIST_RESPONSE_XML.len().to_string());

        let responses: Vec<&'static str> = vec![
            Box::leak(principal_resp.into_boxed_str()),
            Box::leak(homeset_resp.into_boxed_str()),
            Box::leak(list_resp.into_boxed_str()),
        ];
        let (base, handle) = mock_server(responses);
        let client = client_for(&base);
        let calendars = client.discover_calendars().unwrap();
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].display_name, "Personal");
        assert!(calendars[0].url.ends_with("/cal/personal/"));
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("PROPFIND / HTTP/1.1"));
        assert!(requests[0].contains("Authorization: Basic"));
        assert!(requests[0].contains("Depth: 0"));
    }

    #[test]
    fn sync_calendar_returns_delta_with_changed_and_deleted() {
        let body = SYNC_COLLECTION_RESPONSE_XML;
        let resp = format!(
            "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (base, _handle) = mock_server(vec![Box::leak(resp.into_boxed_str())]);
        let client = client_for(&base);
        let outcome = client
            .sync_calendar(&format!("{}/cal/personal/", base), Some("old-token"))
            .unwrap();
        match outcome {
            SyncOutcome::Delta {
                token,
                changed,
                deleted,
            } => {
                assert_eq!(token.as_deref(), Some("https://cal.example.com/sync/2"));
                assert_eq!(changed.len(), 1);
                assert_eq!(changed[0].href, "/cal/personal/event1.ics");
                assert_eq!(changed[0].etag.as_deref(), Some("\"etag-1\""));
                assert!(
                    changed[0]
                        .calendar_data
                        .as_ref()
                        .unwrap()
                        .contains("UID:event1")
                );
                assert_eq!(deleted, vec!["/cal/personal/event2.ics".to_string()]);
            }
            SyncOutcome::FullResyncRequired => panic!("expected Delta"),
        }
    }

    #[test]
    fn sync_calendar_requests_full_resync_on_invalid_token_status() {
        let resp = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
        let (base, _handle) = mock_server(vec![resp]);
        let client = client_for(&base);
        let outcome = client
            .sync_calendar(&format!("{}/cal/personal/", base), Some("stale"))
            .unwrap();
        assert!(matches!(outcome, SyncOutcome::FullResyncRequired));
    }

    #[test]
    fn get_event_returns_etag_and_body() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nEND:VEVENT\r\n";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nETag: \"abc\"\r\nContent-Length: {}\r\n\r\n{}",
            ics.len(),
            ics
        );
        let (base, _handle) = mock_server(vec![Box::leak(resp.into_boxed_str())]);
        let client = client_for(&base);
        let (etag, body) = client.get_event("/cal/personal/x.ics").unwrap();
        assert_eq!(etag.as_deref(), Some("\"abc\""));
        assert_eq!(body, ics);
    }

    #[test]
    fn get_event_404_is_not_found() {
        let (base, _handle) =
            mock_server(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
        let client = client_for(&base);
        let err = client.get_event("/cal/personal/missing.ics").unwrap_err();
        assert!(matches!(err, CalDavError::NotFound));
    }

    #[test]
    fn put_event_create_sends_if_none_match_star() {
        let resp = "HTTP/1.1 201 Created\r\nETag: \"new-etag\"\r\nContent-Length: 0\r\n\r\n";
        let (base, handle) = mock_server(vec![resp]);
        let client = client_for(&base);
        let etag = client
            .put_event(
                "/cal/personal/new.ics",
                "BEGIN:VEVENT\r\nEND:VEVENT\r\n",
                None,
                true,
            )
            .unwrap();
        assert_eq!(etag, "\"new-etag\"");
        let req = handle.join().unwrap().remove(0);
        assert!(req.starts_with("PUT /cal/personal/new.ics HTTP/1.1"));
        assert!(req.contains("If-None-Match: *"));
    }

    #[test]
    fn put_event_conflict_on_412_precondition_failed() {
        let resp = "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\n\r\n";
        let (base, _handle) = mock_server(vec![resp]);
        let client = client_for(&base);
        let err = client
            .put_event(
                "/cal/personal/x.ics",
                "BEGIN:VEVENT\r\nEND:VEVENT\r\n",
                Some("\"stale-etag\""),
                false,
            )
            .unwrap_err();
        assert!(matches!(err, CalDavError::Conflict(_)));
    }

    #[test]
    fn put_event_update_sends_if_match_with_given_etag() {
        let resp = "HTTP/1.1 204 No Content\r\nETag: \"v2\"\r\nContent-Length: 0\r\n\r\n";
        let (base, handle) = mock_server(vec![resp]);
        let client = client_for(&base);
        let etag = client
            .put_event(
                "/cal/personal/x.ics",
                "BEGIN:VEVENT\r\nEND:VEVENT\r\n",
                Some("\"v1\""),
                false,
            )
            .unwrap();
        assert_eq!(etag, "\"v2\"");
        let req = handle.join().unwrap().remove(0);
        assert!(req.contains("If-Match: \"v1\"\r\n"));
    }

    #[test]
    fn delete_event_is_idempotent_on_404() {
        let (base, _handle) =
            mock_server(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
        let client = client_for(&base);
        client
            .delete_event("/cal/personal/already-gone.ics", Some("\"x\""))
            .unwrap();
    }

    #[test]
    fn delete_event_conflict_on_precondition_failed() {
        let (base, _handle) = mock_server(vec![
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\n\r\n",
        ]);
        let client = client_for(&base);
        let err = client
            .delete_event("/cal/personal/x.ics", Some("\"stale\""))
            .unwrap_err();
        assert!(matches!(err, CalDavError::Conflict(_)));
    }

    #[test]
    fn list_all_events_parses_calendar_query_response() {
        let body = CALENDAR_QUERY_RESPONSE_XML;
        let resp = format!(
            "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (base, _handle) = mock_server(vec![Box::leak(resp.into_boxed_str())]);
        let client = client_for(&base);
        let events = client
            .list_all_events(&format!("{}/cal/personal/", base))
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].href, "/cal/personal/a.ics");
        assert!(events[0].calendar_data.as_ref().unwrap().contains("UID:a"));
        assert_eq!(events[1].href, "/cal/personal/b.ics");
    }

    #[test]
    fn new_event_href_sanitizes_uid_and_appends_ics() {
        let href = new_event_href("https://cal.example.com/dav/personal/", "abc@def.com").unwrap();
        assert_eq!(href, "/dav/personal/abc@def.com.ics");
        let href2 = new_event_href("https://cal.example.com/dav/personal", "weird uid!").unwrap();
        assert_eq!(href2, "/dav/personal/weird_uid_.ics");
    }

    const PRINCIPAL_RESPONSE_XML: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/</D:href>
    <D:propstat>
      <D:prop>
        <D:current-user-principal><D:href>/principals/alice/</D:href></D:current-user-principal>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    const HOMESET_RESPONSE_XML: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/principals/alice/</D:href>
    <D:propstat>
      <D:prop>
        <C:calendar-home-set><D:href>/cal/</D:href></C:calendar-home-set>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    const CALENDAR_LIST_RESPONSE_XML: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/">
  <D:response>
    <D:href>/cal/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:displayname>Home</D:displayname>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/cal/personal/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>
        <D:displayname>Personal</D:displayname>
        <CS:getctag>"ctag-1"</CS:getctag>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    const SYNC_COLLECTION_RESPONSE_XML: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/cal/personal/event1.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"etag-1"</D:getetag>
        <C:calendar-data>BEGIN:VEVENT\r\nUID:event1\r\nEND:VEVENT\r\n</C:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/cal/personal/event2.ics</D:href>
    <D:status>HTTP/1.1 404 Not Found</D:status>
  </D:response>
  <D:sync-token>https://cal.example.com/sync/2</D:sync-token>
</D:multistatus>"#;

    const CALENDAR_QUERY_RESPONSE_XML: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/cal/personal/a.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"a1"</D:getetag>
        <C:calendar-data>BEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT\r\n</C:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/cal/personal/b.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"b1"</D:getetag>
        <C:calendar-data>BEGIN:VEVENT\r\nUID:b\r\nEND:VEVENT\r\n</C:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
}
