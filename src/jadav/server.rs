//! `jadav`'s CalDAV/WebDAV HTTP server: the piece stock clients (iOS,
//! DAVx5, Thunderbird, `jamaild`) talk to.
//!
//! Generalises `crate::caldav_server` (jamaild's optional one-calendar
//! server) to many calendars per principal, the full property/report
//! surface real clients probe, and HTTP/1.1 keep-alive. Same threading
//! model — a non-blocking accept loop polled against a shutdown flag, one
//! thread per connection, a fresh [`Store`] per request — and the same
//! hand-rolled HTTP/1.1 framing on top of `crate::httpc`'s reader helpers.
//!
//! ## URL layout (Davis-compatible, so migrating clients keep their URLs)
//!
//! ```text
//! /.well-known/caldav                          301 → /dav/
//! /healthz                                     200 (unauthenticated)
//! /dav/                                        current-user-principal
//! /dav/principals/<login>/                     the principal
//! /dav/calendars/<login>/                      calendar home
//! /dav/calendars/<login>/<slug>/               calendar collection
//! /dav/calendars/<login>/<slug>/<name>.ics     object
//! /dav/calendars/<login>/inbox/ | outbox/      schedule collections
//! ```
//!
//! Request paths are percent-decoded before matching (`t%40mas.gg` and
//! `t@mas.gg` are the same principal) and hrefs are emitted with sabre's
//! encode set — everything except `A-Za-z0-9-_.~():@/` — which is why a
//! login such as `t@mas.gg` appears literally, as it does on Davis.
//!
//! ## Auth
//!
//! HTTP Basic against one precomputed header, compared in constant time.
//! Credentials are resolved once at startup (and again on `SIGHUP`), never
//! per request — an `auth: command` may shell out to a password manager.
//!
//! ## Writes and scheduling
//!
//! Every client `PUT`/`DELETE` runs as: parse → load old → build the change →
//! (scheduling broker, a no-op until that milestone) → one store
//! transaction → response. When the broker rewrites the stored document
//! the `PUT` response carries no `ETag` (RFC 6638 §3.2.10, sabre's
//! convention) so the client refetches. After a committed client write to
//! a mirrored calendar the optional [`ServerState::on_client_write`] hook
//! runs (the mirror engine uses it to wake up).
//!
//! ## Deliberate limits
//!
//! - `calendar-query` honours the `VEVENT` `comp-filter` and its
//!   `time-range` (recurring masters are expanded to test overlap); every
//!   other filter is ignored, so results are a correct superset.
//! - No `MOVE`/`COPY`, no `LOCK`, no quotas, no free-busy (the outbox `POST`
//!   answers "no scheduling support" per recipient until the scheduling
//!   milestone).
//! - Plain HTTP: TLS is the reverse proxy's job. Nothing from proxy headers
//!   is trusted except `X-Forwarded-For` for the request log.

use crate::calendar;
use crate::httpc;
use crate::jadav::config::RESERVED_SLUGS;
use crate::jadav::imip::{self, SchedulingRuntime};
use crate::jadav::itip;
use crate::jadav::store::{
    CalendarRow, DeleteError, InboxItem, ObjectRow, Origin, Precondition, PropPatch, Provider,
    PutError, Store, WriteOutcome, parse_sync_token,
};
use crate::jadav::xml::{
    self, MultiStatus, NS_APPLE, NS_CALDAV, NS_CS, NS_DAV, PropOp, PropfindRequest, QName,
    ReportRequest, prop_element,
};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUESTS_PER_CONNECTION: usize = 1000;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
pub const DAV_HEADER: &str = "1, 3, extended-mkcol, calendar-access, calendar-auto-schedule";
pub const REALM: &str = "jadav";

/// sabre/dav's `encodePath` set: percent-encode everything except
/// unreserved characters plus `( ) : @ /`.
const HREF_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'(')
    .remove(b')')
    .remove(b':')
    .remove(b'@')
    .remove(b'/');

pub fn encode_href(path: &str) -> String {
    utf8_percent_encode(path, HREF_ENCODE_SET).to_string()
}

pub struct Credentials {
    pub login: String,
    /// The full expected `Authorization` header value.
    pub expected_header: String,
}

impl Credentials {
    pub fn new(login: &str, password: &str) -> Self {
        Self {
            login: login.to_string(),
            expected_header: httpc::basic_auth_header(login, password),
        }
    }
}

pub type ClientWriteHook = Arc<dyn Fn(&WriteOutcome) + Send + Sync>;

pub struct ServerState {
    pub store_path: PathBuf,
    pub creds: RwLock<Credentials>,
    pub login: String,
    /// Lower-cased identities, primary first.
    pub identities: Vec<String>,
    pub shutdown: Arc<AtomicBool>,
    pub on_client_write: Option<ClientWriteHook>,
    /// The iTIP/iMIP broker. `None` (tests, `mail:` absent and no
    /// scheduling wanted) stores client documents exactly as received.
    pub scheduling: Option<Arc<SchedulingRuntime>>,
}

impl ServerState {
    pub fn new(store_path: PathBuf, login: &str, password: &str, identities: Vec<String>) -> Self {
        Self {
            store_path,
            creds: RwLock::new(Credentials::new(login, password)),
            login: login.to_string(),
            identities,
            shutdown: Arc::new(AtomicBool::new(false)),
            on_client_write: None,
            scheduling: None,
        }
    }
}

// ---------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------

pub fn bind(listen_addr: &str) -> Result<TcpListener> {
    let listener = TcpListener::bind(listen_addr)
        .with_context(|| format!("binding jadav to {}", listen_addr))?;
    listener
        .set_nonblocking(true)
        .context("setting listener non-blocking")?;
    Ok(listener)
}

pub fn serve(listener: TcpListener, state: Arc<ServerState>) {
    loop {
        if state.shutdown.load(Ordering::Relaxed) {
            return;
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    let _ = handle_connection(stream, peer, &state);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(_) => thread::sleep(ACCEPT_POLL_INTERVAL),
        }
    }
}

pub fn spawn(
    listen_addr: &str,
    state: Arc<ServerState>,
) -> Result<(thread::JoinHandle<()>, SocketAddr)> {
    let listener = bind(listen_addr)?;
    let addr = listener.local_addr().context("reading bound address")?;
    let handle = thread::spawn(move || serve(listener, state));
    Ok((handle, addr))
}

// ---------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    Zero,
    One,
    Infinity,
}

pub struct HttpRequest {
    pub method: String,
    pub raw_target: String,
    /// Percent-decoded path without the query string.
    pub path: String,
    pub segments: Vec<String>,
    pub trailing_slash: bool,
    pub version_11: bool,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|s| s.as_str())
    }

    pub fn depth(&self) -> Depth {
        match self.header("depth").map(|d| d.trim().to_ascii_lowercase()) {
            Some(d) if d == "1" => Depth::One,
            Some(d) if d == "infinity" => Depth::Infinity,
            _ => Depth::Zero,
        }
    }

    fn keep_alive(&self) -> bool {
        let conn = self
            .header("connection")
            .map(|c| c.to_ascii_lowercase())
            .unwrap_or_default();
        if conn.contains("close") {
            return false;
        }
        self.version_11 || conn.contains("keep-alive")
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
    pub fn text(status: u16, body: &str) -> Self {
        Self::empty(status)
            .header("Content-Type", "text/plain; charset=utf-8")
            .with_body(body.as_bytes().to_vec())
    }
    pub fn xml(status: u16, body: String) -> Self {
        Self::empty(status)
            .header("Content-Type", "application/xml; charset=utf-8")
            .with_body(body.into_bytes())
    }
    pub fn dav_error(status: u16, condition_xml: &str) -> Self {
        Self::xml(status, xml::dav_error(condition_xml))
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
    fn not_found() -> Self {
        Self::empty(404)
    }
    fn method_not_allowed(allow: &str) -> Self {
        Self::empty(405).header("Allow", allow)
    }
    fn internal_error(e: &anyhow::Error) -> Self {
        eprintln!("jadav: internal error: {:#}", e);
        Self::empty(500)
    }
    fn unauthorized() -> Self {
        Self::empty(401).header("WWW-Authenticate", &format!("Basic realm=\"{}\"", REALM))
    }
}

struct RequestHead {
    method: String,
    raw_target: String,
    version_11: bool,
    headers: HashMap<String, String>,
}

/// Read request line + headers. `Ok(None)` on a clean EOF before any byte
/// (the peer closed an idle keep-alive connection).
fn read_head(r: &mut impl BufRead) -> Result<Option<RequestHead>> {
    let mut first = Vec::new();
    let n = r
        .read_until(b'\n', &mut first)
        .context("reading request line")?;
    if n == 0 {
        return Ok(None);
    }
    while first.last() == Some(&b'\n') || first.last() == Some(&b'\r') {
        first.pop();
    }
    let line = String::from_utf8_lossy(&first).into_owned();
    let mut parts = line.split_whitespace();
    let method = parts.next().context("empty request line")?.to_string();
    let raw_target = parts
        .next()
        .context("request line missing target")?
        .to_string();
    let version = parts.next().unwrap_or("HTTP/1.0");
    let headers = httpc::read_headers(r)?;
    Ok(Some(RequestHead {
        method,
        raw_target,
        version_11: version.eq_ignore_ascii_case("HTTP/1.1"),
        headers,
    }))
}

fn read_body(r: &mut impl BufRead, headers: &HashMap<String, String>) -> Result<Vec<u8>> {
    let is_chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    if is_chunked {
        return httpc::read_chunked_body(r);
    }
    if let Some(len) = headers.get("content-length") {
        let len: usize = len
            .trim()
            .parse()
            .with_context(|| format!("invalid Content-Length: {}", len))?;
        if len > MAX_BODY_BYTES {
            anyhow::bail!("request body too large: {} bytes", len);
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).context("reading request body")?;
        return Ok(buf);
    }
    Ok(Vec::new())
}

fn split_target(raw_target: &str) -> (String, Vec<String>, bool) {
    let path_part = raw_target.split('?').next().unwrap_or("");
    let path_part = if path_part.is_empty() { "/" } else { path_part };
    let decoded = percent_decode_str(path_part)
        .decode_utf8_lossy()
        .into_owned();
    let trailing_slash = decoded.ends_with('/');
    let segments: Vec<String> = decoded
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    (decoded, segments, trailing_slash)
}

fn write_response(stream: &mut TcpStream, resp: &Resp, keep_alive: bool) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(resp.body.len() + 256);
    out.extend_from_slice(format!("{}\r\n", xml::status_line(resp.status)).as_bytes());
    for (k, v) in &resp.headers {
        out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
    out.extend_from_slice(if keep_alive {
        b"Connection: keep-alive\r\n\r\n" as &[u8]
    } else {
        b"Connection: close\r\n\r\n"
    });
    out.extend_from_slice(&resp.body);
    stream.write_all(&out)?;
    stream.flush()
}

fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: &Arc<ServerState>,
) -> Result<()> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    for _ in 0..MAX_REQUESTS_PER_CONNECTION {
        let Some(head) = (match read_head(&mut reader) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        }) else {
            return Ok(());
        };
        let expects_continue = head
            .headers
            .get("expect")
            .map(|e| e.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false);
        if expects_continue {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            stream.flush()?;
        }
        let body = match read_body(&mut reader, &head.headers) {
            Ok(b) => b,
            Err(e) => {
                let resp = if e.to_string().contains("too large") {
                    Resp::empty(413)
                } else {
                    Resp::empty(400)
                };
                let _ = write_response(&mut stream, &resp, false);
                return Ok(());
            }
        };
        let (path, segments, trailing_slash) = split_target(&head.raw_target);
        let req = HttpRequest {
            method: head.method,
            raw_target: head.raw_target,
            path,
            segments,
            trailing_slash,
            version_11: head.version_11,
            headers: head.headers,
            body,
        };
        let keep_alive = req.keep_alive();
        let started = Instant::now();
        let resp = dispatch(&req, state);
        let client = req
            .header("x-forwarded-for")
            .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| peer.ip().to_string());
        eprintln!(
            "jadav: {} {} -> {} {}ms client={}",
            req.method,
            req.raw_target,
            resp.status,
            started.elapsed().as_millis(),
            client
        );
        write_response(&mut stream, &resp, keep_alive)?;
        if !keep_alive {
            return Ok(());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn check_auth(req: &HttpRequest, state: &ServerState) -> Result<(), Resp> {
    let Some(given) = req.header("authorization") else {
        return Err(Resp::unauthorized());
    };
    let creds = state.creds.read().map_err(|_| Resp::empty(500))?;
    if constant_time_eq(given.trim().as_bytes(), creds.expected_header.as_bytes()) {
        Ok(())
    } else {
        Err(Resp::unauthorized())
    }
}

// ---------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------

struct Paths<'a> {
    login: &'a str,
}

impl Paths<'_> {
    fn root(&self) -> String {
        "/dav/".to_string()
    }
    fn principals(&self) -> String {
        "/dav/principals/".to_string()
    }
    fn principal(&self) -> String {
        format!("/dav/principals/{}/", self.login)
    }
    fn calendars_root(&self) -> String {
        "/dav/calendars/".to_string()
    }
    fn home(&self) -> String {
        format!("/dav/calendars/{}/", self.login)
    }
    fn calendar(&self, slug: &str) -> String {
        format!("/dav/calendars/{}/{}/", self.login, slug)
    }
    fn object(&self, slug: &str, name: &str) -> String {
        format!("/dav/calendars/{}/{}/{}", self.login, slug, name)
    }
    fn inbox(&self) -> String {
        format!("/dav/calendars/{}/inbox/", self.login)
    }
    fn outbox(&self) -> String {
        format!("/dav/calendars/{}/outbox/", self.login)
    }
}

fn href_elem(path: &str) -> String {
    format!("<D:href>{}</D:href>", xml::escape_text(&encode_href(path)))
}

// ---------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------

fn dispatch(req: &HttpRequest, state: &ServerState) -> Resp {
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();

    if req.method == "OPTIONS" {
        return options(&segs, state);
    }
    match segs.as_slice() {
        [".well-known", "caldav"] => {
            return Resp::empty(301).header("Location", "/dav/");
        }
        ["healthz"] => return Resp::text(200, "ok\n"),
        _ => {}
    }
    if let Err(resp) = check_auth(req, state) {
        return resp;
    }
    let store = match Store::open(&state.store_path) {
        Ok(s) => s,
        Err(e) => return Resp::internal_error(&e),
    };
    let ctx = Ctx {
        state,
        store: &store,
        paths: Paths {
            login: &state.login,
        },
    };
    match ctx.route(req, &segs) {
        Ok(resp) => resp,
        Err(e) => Resp::internal_error(&e),
    }
}

fn options(segs: &[&str], state: &ServerState) -> Resp {
    let allow = match segs {
        [] | ["dav"] | ["dav", "principals"] | ["dav", "principals", _] | ["dav", "calendars"] => {
            "OPTIONS, PROPFIND, REPORT"
        }
        ["dav", "calendars", login] if *login == state.login => {
            "OPTIONS, PROPFIND, MKCALENDAR, MKCOL"
        }
        ["dav", "calendars", _, "outbox"] => "OPTIONS, PROPFIND, POST",
        ["dav", "calendars", _, "inbox"] => "OPTIONS, PROPFIND, REPORT",
        ["dav", "calendars", _, _] => {
            "OPTIONS, PROPFIND, PROPPATCH, MKCALENDAR, MKCOL, DELETE, REPORT"
        }
        ["dav", "calendars", _, _, _] => "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND",
        _ => "OPTIONS",
    };
    Resp::empty(200)
        .header("DAV", DAV_HEADER)
        .header("Allow", allow)
}

struct Ctx<'a> {
    state: &'a ServerState,
    store: &'a Store,
    paths: Paths<'a>,
}

enum Res<'a> {
    Root,
    Principals,
    Principal,
    CalendarsRoot,
    Home,
    Calendar(&'a CalendarRow),
    Inbox,
    Outbox,
    Object(&'a CalendarRow, &'a ObjectRow),
    InboxItem(&'a InboxItem),
}

impl Ctx<'_> {
    fn route(&self, req: &HttpRequest, segs: &[&str]) -> Result<Resp> {
        let login = self.state.login.as_str();
        let m = req.method.as_str();
        Ok(match segs {
            [] | ["dav"] => match m {
                "PROPFIND" => self.propfind(req, Res::Root)?,
                _ => Resp::method_not_allowed("OPTIONS, PROPFIND"),
            },
            ["dav", "principals"] => match m {
                "PROPFIND" => self.propfind(req, Res::Principals)?,
                _ => Resp::method_not_allowed("OPTIONS, PROPFIND"),
            },
            ["dav", "principals", l] if *l == login => match m {
                "PROPFIND" => self.propfind(req, Res::Principal)?,
                "PROPPATCH" => Resp::dav_error(403, "<D:cannot-modify-protected-property/>"),
                "REPORT" => Resp::dav_error(403, "<D:supported-report/>"),
                _ => Resp::method_not_allowed("OPTIONS, PROPFIND"),
            },
            ["dav", "calendars"] => match m {
                "PROPFIND" => self.propfind(req, Res::CalendarsRoot)?,
                _ => Resp::method_not_allowed("OPTIONS, PROPFIND"),
            },
            ["dav", "calendars", l] if *l == login => match m {
                "PROPFIND" => self.propfind(req, Res::Home)?,
                "MKCALENDAR" | "MKCOL" | "PUT" | "DELETE" | "PROPPATCH" => {
                    Resp::method_not_allowed("OPTIONS, PROPFIND")
                }
                _ => Resp::method_not_allowed("OPTIONS, PROPFIND"),
            },
            ["dav", "calendars", l, "inbox"] if *l == login => match m {
                "PROPFIND" => self.propfind(req, Res::Inbox)?,
                "REPORT" => self.report_inbox(req)?,
                "PUT" | "MKCALENDAR" | "MKCOL" | "DELETE" | "PROPPATCH" => {
                    Resp::dav_error(403, "<D:need-privileges/>")
                }
                _ => Resp::method_not_allowed("OPTIONS, PROPFIND, REPORT"),
            },
            ["dav", "calendars", l, "inbox", name] if *l == login => match m {
                "PROPFIND" => match self.store.inbox_get(name)? {
                    Some(item) => self.propfind(req, Res::InboxItem(&item))?,
                    None => Resp::not_found(),
                },
                "GET" => self.get_inbox_item(req, name, false)?,
                "HEAD" => self.get_inbox_item(req, name, true)?,
                "DELETE" => {
                    if self.store.inbox_delete(name)? {
                        Resp::empty(204)
                    } else {
                        Resp::not_found()
                    }
                }
                "PUT" | "PROPPATCH" | "MKCALENDAR" | "MKCOL" => {
                    Resp::dav_error(403, "<D:need-privileges/>")
                }
                _ => Resp::method_not_allowed("OPTIONS, PROPFIND, GET, HEAD, DELETE"),
            },
            ["dav", "calendars", l, "outbox"] if *l == login => match m {
                "PROPFIND" => self.propfind(req, Res::Outbox)?,
                "POST" => {
                    let (status, body) =
                        itip::handle_outbox_post(&String::from_utf8_lossy(&req.body));
                    Resp::xml(status, body)
                }
                _ => Resp::method_not_allowed("OPTIONS, PROPFIND, POST"),
            },
            ["dav", "calendars", l, slug] if *l == login => match m {
                "MKCALENDAR" | "MKCOL" => self.mkcalendar(req, slug)?,
                _ => match self.store.get_calendar(slug)? {
                    None => Resp::not_found(),
                    Some(cal) => match m {
                        "PROPFIND" => self.propfind(req, Res::Calendar(&cal))?,
                        "PROPPATCH" => self.proppatch(req, &cal)?,
                        "DELETE" => self.delete_calendar(&cal)?,
                        "REPORT" => self.report(req, &cal)?,
                        "GET" | "HEAD" => Resp::method_not_allowed(
                            "OPTIONS, PROPFIND, PROPPATCH, MKCALENDAR, MKCOL, DELETE, REPORT",
                        ),
                        _ => Resp::method_not_allowed(
                            "OPTIONS, PROPFIND, PROPPATCH, MKCALENDAR, MKCOL, DELETE, REPORT",
                        ),
                    },
                },
            },
            ["dav", "calendars", l, slug, name] if *l == login => {
                match self.store.get_calendar(slug)? {
                    None => Resp::not_found(),
                    Some(cal) => match m {
                        "GET" | "HEAD" => self.get_object(req, &cal, name, m == "HEAD")?,
                        "PUT" => self.put_object(req, &cal, name)?,
                        "DELETE" => self.delete_object(req, &cal, name)?,
                        "PROPFIND" => match self.store.get_object(slug, name)? {
                            Some(obj) => self.propfind(req, Res::Object(&cal, &obj))?,
                            None => Resp::not_found(),
                        },
                        "MOVE" | "COPY" => Resp::empty(501),
                        _ => Resp::method_not_allowed("OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND"),
                    },
                }
            }
            _ => Resp::not_found(),
        })
    }

    // ------------------------------------------------------------------
    // Properties
    // ------------------------------------------------------------------

    fn res_href(&self, res: &Res<'_>) -> String {
        match res {
            Res::Root => self.paths.root(),
            Res::Principals => self.paths.principals(),
            Res::Principal => self.paths.principal(),
            Res::CalendarsRoot => self.paths.calendars_root(),
            Res::Home => self.paths.home(),
            Res::Calendar(c) => self.paths.calendar(&c.slug),
            Res::Inbox => self.paths.inbox(),
            Res::Outbox => self.paths.outbox(),
            Res::Object(c, o) => self.paths.object(&c.slug, &o.meta.href_name),
            Res::InboxItem(i) => format!("{}{}", self.paths.inbox(), i.href_name),
        }
    }

    /// The `allprop` set for a resource (what a client gets without asking).
    fn allprop_names(&self, res: &Res<'_>) -> Vec<QName> {
        let mut v = vec![
            QName::dav("resourcetype"),
            QName::dav("displayname"),
            QName::dav("current-user-principal"),
            QName::dav("principal-collection-set"),
            QName::dav("owner"),
            QName::dav("current-user-privilege-set"),
        ];
        match res {
            Res::Principal => v.extend([
                QName::dav("principal-URL"),
                QName::caldav("calendar-home-set"),
                QName::caldav("calendar-user-address-set"),
                QName::caldav("schedule-inbox-URL"),
                QName::caldav("schedule-outbox-URL"),
                QName::caldav("calendar-user-type"),
            ]),
            Res::Calendar(_) => v.extend([
                QName::cs("getctag"),
                QName::dav("sync-token"),
                QName::dav("supported-report-set"),
                QName::caldav("supported-calendar-component-set"),
                QName::caldav("calendar-description"),
                QName::apple("calendar-color"),
                QName::apple("calendar-order"),
                QName::caldav("calendar-timezone"),
                QName::caldav("schedule-calendar-transp"),
            ]),
            Res::Inbox => v.extend([
                QName::cs("getctag"),
                QName::dav("sync-token"),
                QName::dav("supported-report-set"),
                QName::caldav("supported-calendar-component-set"),
                QName::caldav("schedule-default-calendar-URL"),
            ]),
            Res::Object(_, _) | Res::InboxItem(_) => v.extend([
                QName::dav("getetag"),
                QName::dav("getcontenttype"),
                QName::dav("getcontentlength"),
            ]),
            _ => {}
        }
        v
    }

    /// Render one property's inner XML for `res`, or `None` when the
    /// property does not exist on that resource (a 404 propstat entry).
    fn prop(&self, res: &Res<'_>, name: &QName) -> Result<Option<String>> {
        let p = &self.paths;
        let principal_href = href_elem(&p.principal());
        let v = match (name.ns.as_str(), name.local.as_str()) {
            (NS_DAV, "resourcetype") => Some(match res {
                Res::Principal => "<D:collection/><D:principal/>".to_string(),
                Res::Calendar(_) => "<D:collection/><C:calendar/>".to_string(),
                Res::Inbox => "<D:collection/><C:schedule-inbox/>".to_string(),
                Res::Outbox => "<D:collection/><C:schedule-outbox/>".to_string(),
                Res::Object(_, _) | Res::InboxItem(_) => String::new(),
                _ => "<D:collection/>".to_string(),
            }),
            (NS_DAV, "displayname") => match res {
                Res::Principal | Res::Home => Some(xml::escape_text(&self.state.login)),
                Res::Calendar(c) => Some(xml::escape_text(&c.display_name)),
                Res::Inbox => Some("Inbox".to_string()),
                Res::Outbox => Some("Outbox".to_string()),
                _ => None,
            },
            (NS_DAV, "current-user-principal") => Some(principal_href.clone()),
            (NS_DAV, "principal-collection-set") => Some(href_elem(&p.principals())),
            (NS_DAV, "principal-URL") => match res {
                Res::Principal => Some(principal_href.clone()),
                _ => None,
            },
            (NS_DAV, "owner") => match res {
                Res::Root | Res::Principals | Res::CalendarsRoot => None,
                _ => Some(principal_href.clone()),
            },
            (NS_DAV, "current-user-privilege-set") => {
                let writable = match res {
                    Res::Calendar(c) => c.writable(),
                    Res::Object(c, _) => c.writable(),
                    Res::Home => true,
                    _ => false,
                };
                let mut s = String::from(
                    "<D:privilege><D:read/></D:privilege><D:privilege><D:read-current-user-privilege-set/></D:privilege>",
                );
                if writable {
                    s.push_str("<D:privilege><D:write/></D:privilege><D:privilege><D:write-content/></D:privilege><D:privilege><D:write-properties/></D:privilege><D:privilege><D:bind/></D:privilege><D:privilege><D:unbind/></D:privilege>");
                }
                match res {
                    Res::Inbox => s.push_str("<D:privilege><C:schedule-deliver/></D:privilege>"),
                    Res::Outbox => s.push_str("<D:privilege><C:schedule-send/></D:privilege>"),
                    _ => {}
                }
                Some(s)
            }
            (NS_CALDAV, "calendar-home-set") => match res {
                Res::Principal => Some(href_elem(&p.home())),
                _ => None,
            },
            (NS_CALDAV, "calendar-user-address-set") => match res {
                Res::Principal => Some(
                    self.state
                        .identities
                        .iter()
                        .map(|id| format!("<D:href>mailto:{}</D:href>", xml::escape_text(id)))
                        .chain(std::iter::once(principal_href.clone()))
                        .collect::<String>(),
                ),
                _ => None,
            },
            (NS_CALDAV, "schedule-inbox-URL") => match res {
                Res::Principal => Some(href_elem(&p.inbox())),
                _ => None,
            },
            (NS_CALDAV, "schedule-outbox-URL") => match res {
                Res::Principal => Some(href_elem(&p.outbox())),
                _ => None,
            },
            (NS_CALDAV, "calendar-user-type") => match res {
                Res::Principal => Some("INDIVIDUAL".to_string()),
                _ => None,
            },
            (NS_CALDAV, "schedule-default-calendar-URL") => match res {
                Res::Principal | Res::Inbox => {
                    let primary = self.state.identities.first().cloned().unwrap_or_default();
                    self.store
                        .default_calendar_for_identity(&primary)?
                        .map(|c| href_elem(&p.calendar(&c.slug)))
                }
                _ => None,
            },
            (NS_CS, "getctag") => match res {
                Res::Calendar(c) => Some(self.store.ctag(&c.slug)?.to_string()),
                Res::Inbox => Some(self.store.inbox_ctag()?.to_string()),
                _ => None,
            },
            (NS_DAV, "sync-token") => match res {
                Res::Calendar(c) => Some(xml::escape_text(&self.store.sync_token(&c.slug)?)),
                Res::Inbox => Some(format!("urn:jadav:inbox:{}", self.store.inbox_ctag()?)),
                _ => None,
            },
            (NS_DAV, "supported-report-set") => match res {
                Res::Calendar(_) | Res::Inbox => Some(
                    "<D:supported-report><D:report><C:calendar-multiget/></D:report></D:supported-report>\
                     <D:supported-report><D:report><C:calendar-query/></D:report></D:supported-report>\
                     <D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report>"
                        .to_string(),
                ),
                _ => None,
            },
            (NS_CALDAV, "supported-calendar-component-set") => match res {
                Res::Calendar(_) | Res::Inbox => Some("<C:comp name=\"VEVENT\"/>".to_string()),
                _ => None,
            },
            (NS_CALDAV, "supported-calendar-data") => match res {
                Res::Calendar(_) => Some(
                    "<C:calendar-data content-type=\"text/calendar\" version=\"2.0\"/>".to_string(),
                ),
                _ => None,
            },
            (NS_CALDAV, "calendar-description") => match res {
                Res::Calendar(c) => Some(xml::escape_text(&c.description)),
                _ => None,
            },
            (NS_APPLE, "calendar-color") => match res {
                Res::Calendar(c) => c.color.as_ref().map(|col| xml::escape_text(col)),
                _ => None,
            },
            (NS_APPLE, "calendar-order") => match res {
                Res::Calendar(c) => Some(c.order.to_string()),
                _ => None,
            },
            (NS_CALDAV, "calendar-timezone") => match res {
                Res::Calendar(c) => c.timezone.as_ref().map(|tz| xml::escape_text(tz)),
                _ => None,
            },
            (NS_CALDAV, "schedule-calendar-transp") => match res {
                Res::Calendar(c) => Some(if c.transparent {
                    "<C:transparent/>".to_string()
                } else {
                    "<C:opaque/>".to_string()
                }),
                _ => None,
            },
            (NS_DAV, "getetag") => match res {
                Res::Object(_, o) => Some(xml::escape_text(&o.meta.etag)),
                Res::InboxItem(i) => Some(xml::escape_text(&i.etag)),
                _ => None,
            },
            (NS_DAV, "getcontenttype") => match res {
                Res::Object(_, _) => Some("text/calendar; charset=utf-8; component=VEVENT".to_string()),
                Res::InboxItem(_) => Some("text/calendar; charset=utf-8".to_string()),
                _ => None,
            },
            (NS_DAV, "getcontentlength") => match res {
                Res::Object(_, o) => Some(o.ics.len().to_string()),
                Res::InboxItem(i) => Some(i.ics.len().to_string()),
                _ => None,
            },
            (NS_DAV, "getlastmodified") => match res {
                Res::Object(_, o) => Some(http_date(o.meta.updated_at)),
                Res::InboxItem(i) => Some(http_date(i.received_at)),
                _ => None,
            },
            (NS_CALDAV, "calendar-data") => match res {
                Res::Object(_, o) => Some(xml::escape_text(&o.ics)),
                Res::InboxItem(i) => Some(xml::escape_text(&i.ics)),
                _ => None,
            },
            _ => None,
        };
        Ok(v)
    }

    fn render_props(
        &self,
        ms: &mut MultiStatus,
        res: &Res<'_>,
        request: &PropfindRequest,
    ) -> Result<()> {
        let href = self.res_href(res);
        let names: Vec<QName> = match request {
            PropfindRequest::Prop(names) => names.clone(),
            PropfindRequest::AllProp | PropfindRequest::PropName => self.allprop_names(res),
        };
        let mut found = String::new();
        let mut missing = String::new();
        for name in &names {
            match self.prop(res, name)? {
                Some(inner) => {
                    let inner = if matches!(request, PropfindRequest::PropName) {
                        String::new()
                    } else {
                        inner
                    };
                    found.push_str(&prop_element(name, &inner));
                }
                None => missing.push_str(&prop_element(name, "")),
            }
        }
        ms.response(&encode_href(&href))
            .propstat(200, &found)
            .propstat(404, &missing)
            .end();
        Ok(())
    }

    fn propfind(&self, req: &HttpRequest, res: Res<'_>) -> Result<Resp> {
        let request = match xml::parse_propfind(&req.body) {
            Ok(r) => r,
            Err(_) => return Ok(Resp::empty(400)),
        };
        let depth = req.depth();
        if depth == Depth::Infinity {
            return Ok(Resp::dav_error(403, "<D:propfind-finite-depth/>"));
        }
        let mut ms = MultiStatus::new();
        self.render_props(&mut ms, &res, &request)?;
        if depth == Depth::One {
            match &res {
                Res::Root => {
                    self.render_props(&mut ms, &Res::Principals, &request)?;
                    self.render_props(&mut ms, &Res::CalendarsRoot, &request)?;
                }
                Res::Principals => self.render_props(&mut ms, &Res::Principal, &request)?,
                Res::CalendarsRoot => self.render_props(&mut ms, &Res::Home, &request)?,
                Res::Home => {
                    for cal in self.store.list_calendars()? {
                        self.render_props(&mut ms, &Res::Calendar(&cal), &request)?;
                    }
                    self.render_props(&mut ms, &Res::Inbox, &request)?;
                    self.render_props(&mut ms, &Res::Outbox, &request)?;
                }
                Res::Calendar(cal) => {
                    for obj in self.store.list_objects(&cal.slug)? {
                        self.render_props(&mut ms, &Res::Object(cal, &obj), &request)?;
                    }
                }
                Res::Inbox => {
                    for item in self.store.inbox_list()? {
                        self.render_props(&mut ms, &Res::InboxItem(&item), &request)?;
                    }
                }
                _ => {}
            }
        }
        Ok(Resp::xml(207, ms.finish()).header("DAV", DAV_HEADER))
    }

    // ------------------------------------------------------------------
    // PROPPATCH / MKCALENDAR / DELETE calendar
    // ------------------------------------------------------------------

    fn proppatch(&self, req: &HttpRequest, cal: &CalendarRow) -> Result<Resp> {
        let ops = match xml::parse_proppatch(&req.body) {
            Ok(o) => o,
            Err(_) => return Ok(Resp::empty(400)),
        };
        let mut patch = PropPatch::default();
        // (name, status, error condition) per property.
        let mut results: Vec<(QName, u16, Option<&'static str>)> = Vec::new();
        let native = cal.provider == Provider::Native;
        for op in &ops {
            let (name, value) = match op {
                PropOp::Set(n, v) => (n, Some(v)),
                PropOp::Remove(n) => (n, None),
            };
            let key = (name.ns.as_str(), name.local.as_str());
            let protected_on_mirror = matches!(
                key,
                (NS_DAV, "displayname")
                    | (NS_CALDAV, "calendar-description")
                    | (NS_CALDAV, "calendar-timezone")
            ) && !native;
            if protected_on_mirror {
                results.push((
                    name.clone(),
                    403,
                    Some("<D:cannot-modify-protected-property/>"),
                ));
                continue;
            }
            match key {
                (NS_DAV, "displayname") => {
                    let v = value.map(|v| v.text.trim().to_string()).unwrap_or_default();
                    if v.is_empty() {
                        results.push((name.clone(), 409, None));
                    } else {
                        patch.display_name = Some(v);
                        results.push((name.clone(), 200, None));
                    }
                }
                (NS_CALDAV, "calendar-description") => {
                    patch.description =
                        Some(value.map(|v| v.text.trim().to_string()).unwrap_or_default());
                    results.push((name.clone(), 200, None));
                }
                (NS_APPLE, "calendar-color") => {
                    patch.color = Some(
                        value
                            .map(|v| v.text.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                    results.push((name.clone(), 200, None));
                }
                (NS_APPLE, "calendar-order") => match value {
                    None => {
                        patch.order = Some(0);
                        results.push((name.clone(), 200, None));
                    }
                    Some(v) => match v.text.trim().parse::<i64>() {
                        Ok(n) => {
                            patch.order = Some(n);
                            results.push((name.clone(), 200, None));
                        }
                        Err(_) => results.push((name.clone(), 409, None)),
                    },
                },
                (NS_CALDAV, "calendar-timezone") => {
                    patch.timezone = Some(
                        value
                            .map(|v| v.text.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                    results.push((name.clone(), 200, None));
                }
                (NS_CALDAV, "schedule-calendar-transp") => {
                    let transparent = value
                        .map(|v| v.child_names.iter().any(|c| c.is(NS_CALDAV, "transparent")))
                        .unwrap_or(false);
                    patch.transparent = Some(transparent);
                    results.push((name.clone(), 200, None));
                }
                _ => results.push((name.clone(), 403, None)),
            }
        }
        let any_failed = results.iter().any(|(_, s, _)| *s != 200);
        let mut ms = MultiStatus::new();
        let mut rb = ms.response(&encode_href(&self.paths.calendar(&cal.slug)));
        let mut by_status: Vec<(u16, Option<&'static str>, String)> = Vec::new();
        for (name, status, condition) in &results {
            let status = if any_failed && *status == 200 {
                424
            } else {
                *status
            };
            match by_status
                .iter_mut()
                .find(|(s, c, _)| *s == status && c == condition)
            {
                Some((_, _, inner)) => inner.push_str(&prop_element(name, "")),
                None => by_status.push((status, *condition, prop_element(name, ""))),
            }
        }
        for (status, condition, inner) in &by_status {
            rb = rb.propstat_with_error(*status, inner, *condition);
        }
        rb.end();
        if !any_failed {
            self.store.update_calendar_props(&cal.slug, &patch)?;
        }
        Ok(Resp::xml(207, ms.finish()))
    }

    fn mkcalendar(&self, req: &HttpRequest, slug: &str) -> Result<Resp> {
        let valid = {
            let mut chars = slug.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
                && slug.len() <= 64
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        };
        if !valid || RESERVED_SLUGS.contains(&slug) {
            return Ok(Resp::empty(403));
        }
        if self.store.get_calendar(slug)?.is_some() {
            return Ok(Resp::method_not_allowed(
                "OPTIONS, PROPFIND, PROPPATCH, DELETE, REPORT",
            ));
        }
        let (ops, declares_calendar) = match xml::parse_mkcalendar(&req.body) {
            Ok(v) => v,
            Err(_) => return Ok(Resp::empty(400)),
        };
        if req.method == "MKCOL" && !declares_calendar {
            // A plain WebDAV collection is not something this server stores.
            return Ok(Resp::empty(403));
        }
        let mut props = PropPatch::default();
        for op in &ops {
            if let PropOp::Set(name, value) = op {
                match (name.ns.as_str(), name.local.as_str()) {
                    (NS_DAV, "displayname") => {
                        let v = value.text.trim();
                        if !v.is_empty() {
                            props.display_name = Some(v.to_string());
                        }
                    }
                    (NS_CALDAV, "calendar-description") => {
                        props.description = Some(value.text.trim().to_string())
                    }
                    (NS_APPLE, "calendar-color") => {
                        props.color =
                            Some(Some(value.text.trim().to_string()).filter(|s| !s.is_empty()))
                    }
                    (NS_APPLE, "calendar-order") => props.order = value.text.trim().parse().ok(),
                    (NS_CALDAV, "calendar-timezone") => {
                        props.timezone =
                            Some(Some(value.text.trim().to_string()).filter(|s| !s.is_empty()))
                    }
                    (NS_CALDAV, "schedule-calendar-transp") => {
                        props.transparent = Some(
                            value
                                .child_names
                                .iter()
                                .any(|c| c.is(NS_CALDAV, "transparent")),
                        )
                    }
                    _ => {}
                }
            }
        }
        let identity = self.state.identities.first().cloned().unwrap_or_default();
        match self.store.create_calendar(slug, &identity, &props)? {
            Ok(_) => Ok(Resp::empty(201)),
            Err(_) => Ok(Resp::method_not_allowed(
                "OPTIONS, PROPFIND, PROPPATCH, DELETE, REPORT",
            )),
        }
    }

    fn delete_calendar(&self, cal: &CalendarRow) -> Result<Resp> {
        if cal.provider != Provider::Native {
            return Ok(Resp::dav_error(403, "<D:need-privileges/>"));
        }
        self.store.delete_calendar(&cal.slug)?;
        Ok(Resp::empty(204))
    }

    // ------------------------------------------------------------------
    // REPORT
    // ------------------------------------------------------------------

    fn object_block(
        &self,
        ms: &mut MultiStatus,
        cal: &CalendarRow,
        obj: &ObjectRow,
        props: &[QName],
    ) -> Result<()> {
        self.res_block(ms, &Res::Object(cal, obj), props)
    }

    /// One `<D:response>` for a resource in a REPORT: the requested
    /// properties (default `getetag` + `calendar-data`) split 200/404.
    fn res_block(&self, ms: &mut MultiStatus, res: &Res<'_>, props: &[QName]) -> Result<()> {
        let names: Vec<QName> = if props.is_empty() {
            vec![QName::dav("getetag"), QName::caldav("calendar-data")]
        } else {
            props.to_vec()
        };
        let mut found = String::new();
        let mut missing = String::new();
        for name in &names {
            match self.prop(res, name)? {
                Some(inner) => found.push_str(&prop_element(name, &inner)),
                None => missing.push_str(&prop_element(name, "")),
            }
        }
        ms.response(&encode_href(&self.res_href(res)))
            .propstat(200, &found)
            .propstat(404, &missing)
            .end();
        Ok(())
    }

    fn report(&self, req: &HttpRequest, cal: &CalendarRow) -> Result<Resp> {
        let request = match xml::parse_report(&req.body) {
            Ok(r) => r,
            Err(_) => return Ok(Resp::empty(400)),
        };
        let mut ms = MultiStatus::new();
        match request {
            ReportRequest::Multiget { props, hrefs } => {
                let prefix = self.paths.calendar(&cal.slug);
                for href in hrefs {
                    let decoded = percent_decode_str(href.split('?').next().unwrap_or(""))
                        .decode_utf8_lossy()
                        .into_owned();
                    // Accept absolute URLs too: keep only the path.
                    let path = match decoded.find("://") {
                        Some(i) => decoded[i + 3..]
                            .find('/')
                            .map(|j| decoded[i + 3 + j..].to_string())
                            .unwrap_or_default(),
                        None => decoded.clone(),
                    };
                    let name = match path.strip_prefix(prefix.as_str()) {
                        Some(n) if !n.is_empty() && !n.contains('/') => n.to_string(),
                        _ => {
                            ms.status_only(&href, 404);
                            continue;
                        }
                    };
                    match self.store.get_object(&cal.slug, &name)? {
                        Some(obj) => self.object_block(&mut ms, cal, &obj, &props)?,
                        None => ms.status_only(&href, 404),
                    }
                }
            }
            ReportRequest::Query {
                props,
                other_component,
                time_range,
            } => {
                if other_component.is_none() {
                    let rows = match time_range {
                        Some((start, end)) => {
                            self.store.list_objects_overlapping(&cal.slug, start, end)?
                        }
                        None => self.store.list_objects(&cal.slug)?,
                    };
                    for obj in rows {
                        if let Some((start, end)) = time_range
                            && obj.meta.rrule.is_some()
                            && !recurring_overlaps(&obj, start, end)
                        {
                            continue;
                        }
                        self.object_block(&mut ms, cal, &obj, &props)?;
                    }
                }
            }
            ReportRequest::SyncCollection { token, props } => {
                let since = match &token {
                    None => 0,
                    Some(t) => match parse_sync_token(t, &cal.slug) {
                        Some(n) => n,
                        None => return Ok(Resp::dav_error(403, "<D:valid-sync-token/>")),
                    },
                };
                let (changes, head) = match self.store.changes_since(&cal.slug, since)? {
                    Ok(v) => v,
                    Err(_) => return Ok(Resp::dav_error(403, "<D:valid-sync-token/>")),
                };
                for change in changes {
                    let href = self.paths.object(&cal.slug, &change.href_name);
                    if change.deleted {
                        ms.status_only(&encode_href(&href), 404);
                        continue;
                    }
                    match self.store.get_object(&cal.slug, &change.href_name)? {
                        Some(obj) => self.object_block(&mut ms, cal, &obj, &props)?,
                        None => ms.status_only(&encode_href(&href), 404),
                    }
                }
                ms.sync_token(&crate::jadav::store::format_sync_token(&cal.slug, head));
            }
            ReportRequest::Unsupported(_) => {
                return Ok(Resp::dav_error(403, "<D:supported-report/>"));
            }
        }
        Ok(Resp::xml(207, ms.finish()))
    }

    fn report_inbox(&self, req: &HttpRequest) -> Result<Resp> {
        let request = match xml::parse_report(&req.body) {
            Ok(r) => r,
            Err(_) => return Ok(Resp::empty(400)),
        };
        let mut ms = MultiStatus::new();
        match request {
            ReportRequest::Multiget { props, hrefs } => {
                let prefix = self.paths.inbox();
                for href in hrefs {
                    let path = request_path_of(&href);
                    match path
                        .strip_prefix(prefix.as_str())
                        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
                        .map(|name| self.store.inbox_get(name))
                        .transpose()?
                        .flatten()
                    {
                        Some(item) => self.res_block(&mut ms, &Res::InboxItem(&item), &props)?,
                        None => ms.status_only(&href, 404),
                    }
                }
            }
            ReportRequest::SyncCollection { .. } => {
                ms.sync_token(&format!("urn:jadav:inbox:{}", self.store.inbox_ctag()?));
            }
            ReportRequest::Query { props, .. } => {
                // Inbox items carry no time-range worth filtering on; the
                // whole inbox is small and the client deletes what it read.
                for item in self.store.inbox_list()? {
                    self.res_block(&mut ms, &Res::InboxItem(&item), &props)?;
                }
            }
            ReportRequest::Unsupported(_) => {
                return Ok(Resp::dav_error(403, "<D:supported-report/>"));
            }
        }
        Ok(Resp::xml(207, ms.finish()))
    }

    fn get_inbox_item(&self, req: &HttpRequest, name: &str, head_only: bool) -> Result<Resp> {
        let Some(item) = self.store.inbox_get(name)? else {
            return Ok(Resp::not_found());
        };
        if let Some(inm) = req.header("if-none-match")
            && etag_list(inm).iter().any(|t| t == "*" || t == &item.etag)
        {
            return Ok(Resp::empty(304).header("ETag", &item.etag));
        }
        let resp = Resp::empty(200)
            .header("Content-Type", "text/calendar; charset=utf-8")
            .header("ETag", &item.etag)
            .header("Last-Modified", &http_date(item.received_at));
        if head_only {
            return Ok(resp.header("Content-Length", &item.ics.len().to_string()));
        }
        Ok(resp.with_body(item.ics.into_bytes()))
    }

    // ------------------------------------------------------------------
    // Objects
    // ------------------------------------------------------------------

    fn get_object(
        &self,
        req: &HttpRequest,
        cal: &CalendarRow,
        name: &str,
        head_only: bool,
    ) -> Result<Resp> {
        let Some(obj) = self.store.get_object(&cal.slug, name)? else {
            return Ok(Resp::not_found());
        };
        if let Some(inm) = req.header("if-none-match")
            && etag_list(inm)
                .iter()
                .any(|t| t == "*" || t == &obj.meta.etag)
        {
            return Ok(Resp::empty(304).header("ETag", &obj.meta.etag));
        }
        let mut resp = Resp::empty(200)
            .header("Content-Type", "text/calendar; charset=utf-8")
            .header("ETag", &obj.meta.etag)
            .header("Last-Modified", &http_date(obj.meta.updated_at));
        if obj.meta.organizer.is_some() {
            resp = resp.header("Schedule-Tag", &format!("\"{}\"", obj.meta.schedule_tag));
        }
        if head_only {
            // HEAD: same headers, declared length, no body.
            resp = resp.header("Content-Length", &obj.ics.len().to_string());
            return Ok(resp);
        }
        Ok(resp.with_body(obj.ics.into_bytes()))
    }

    fn put_object(&self, req: &HttpRequest, cal: &CalendarRow, name: &str) -> Result<Resp> {
        if let Some(ct) = req.header("content-type")
            && !ct.to_ascii_lowercase().starts_with("text/calendar")
        {
            return Ok(Resp::empty(415));
        }
        let ics = String::from_utf8_lossy(&req.body).into_owned();
        let if_match = req.header("if-match").map(etag_list);
        let if_none_match = req.header("if-none-match").map(etag_list);
        let pre = match (&if_match, &if_none_match) {
            (Some(list), _) if list.iter().any(|t| t == "*") => Precondition::IfMatchAny,
            (Some(list), _) => Precondition::IfMatch(list),
            (None, Some(list)) if list.iter().any(|t| t == "*") => Precondition::IfNoneMatchAny,
            (None, Some(list)) => Precondition::IfNoneMatch(list),
            (None, None) => Precondition::None,
        };
        let existing = self.store.get_object(&cal.slug, name)?;
        if let Some(tag) = req.header("if-schedule-tag-match")
            && let Some(existing) = &existing
            && tag.trim() != format!("\"{}\"", existing.meta.schedule_tag)
        {
            return Ok(Resp::empty(412));
        }
        // The iTIP broker runs before the transaction: it may rewrite the
        // document (SEQUENCE bump, SCHEDULE-STATUS stamps) and produces the
        // iMIP messages that are queued inside the very same transaction.
        let plan = match &self.state.scheduling {
            Some(rt) => imip::plan_client_write(
                self.store,
                rt,
                cal,
                name,
                existing.as_ref().map(|o| o.ics.as_str()),
                Some(&ics),
                Utc::now(),
            ),
            None => imip::PlannedWrite::default(),
        };
        let rewritten = plan.rewritten();
        let store_ics = plan.store_ics.as_deref().unwrap_or(&ics);
        let jobs = plan.jobs;
        let outcome = match self.store.put_object_with(
            &cal.slug,
            name,
            store_ics,
            pre,
            Origin::Client,
            |conn| {
                for job in &jobs {
                    Store::enqueue_outbound_on(conn, job)?;
                }
                Ok(())
            },
        )? {
            Ok(o) => o,
            Err(PutError::PreconditionFailed { .. }) => return Ok(Resp::empty(412)),
            Err(PutError::UidConflict { other_href_name }) => {
                return Ok(Resp::dav_error(
                    403,
                    &format!(
                        "<C:no-uid-conflict>{}</C:no-uid-conflict>",
                        href_elem(&self.paths.object(&cal.slug, &other_href_name))
                    ),
                ));
            }
            Err(PutError::UidChanged) => return Ok(Resp::dav_error(403, "<C:no-uid-conflict/>")),
            Err(PutError::NotVcalendar) => return Ok(Resp::empty(415)),
            Err(PutError::InvalidBody(msg)) => {
                return Ok(Resp::dav_error(
                    403,
                    &format!(
                        "<C:valid-calendar-data/><D:responsedescription>{}</D:responsedescription>",
                        xml::escape_text(&msg)
                    ),
                ));
            }
            Err(PutError::UnsupportedComponent(_)) => {
                return Ok(Resp::dav_error(403, "<C:supported-calendar-component/>"));
            }
            Err(PutError::ReadOnly) => return Ok(Resp::dav_error(403, "<D:need-privileges/>")),
        };
        let created = outcome.old.is_none();
        let mut resp = Resp::empty(if created { 201 } else { 204 });
        if let Some(new) = &outcome.new {
            if !rewritten {
                resp = resp.header("ETag", &new.meta.etag);
            }
            if new.meta.organizer.is_some() {
                resp = resp.header("Schedule-Tag", &format!("\"{}\"", new.meta.schedule_tag));
            }
        }
        self.after_client_write(&outcome, !jobs.is_empty());
        Ok(resp)
    }

    fn delete_object(&self, req: &HttpRequest, cal: &CalendarRow, name: &str) -> Result<Resp> {
        let if_match = req.header("if-match").map(etag_list);
        let pre = match &if_match {
            Some(list) if list.iter().any(|t| t == "*") => Precondition::IfMatchAny,
            Some(list) => Precondition::IfMatch(list),
            None => Precondition::None,
        };
        let existing = self.store.get_object(&cal.slug, name)?;
        if let Some(tag) = req.header("if-schedule-tag-match")
            && let Some(existing) = &existing
            && tag.trim() != format!("\"{}\"", existing.meta.schedule_tag)
        {
            return Ok(Resp::empty(412));
        }
        // Deleting an attended object is a DECLINE, deleting an organised
        // one a CANCEL: the broker decides, the queue rows ride along.
        let jobs = match (&self.state.scheduling, &existing) {
            (Some(rt), Some(old)) => {
                imip::plan_client_write(self.store, rt, cal, name, Some(&old.ics), None, Utc::now())
                    .jobs
            }
            _ => Vec::new(),
        };
        match self
            .store
            .delete_object_with(&cal.slug, name, pre, Origin::Client, |conn| {
                for job in &jobs {
                    Store::enqueue_outbound_on(conn, job)?;
                }
                Ok(())
            })? {
            Ok(outcome) => {
                self.after_client_write(&outcome, !jobs.is_empty());
                Ok(Resp::empty(204))
            }
            Err(DeleteError::NotFound) => Ok(Resp::not_found()),
            Err(DeleteError::PreconditionFailed { .. }) => Ok(Resp::empty(412)),
            Err(DeleteError::ReadOnly) => Ok(Resp::dav_error(403, "<D:need-privileges/>")),
        }
    }

    /// Post-commit fan-out for a client write: wake the mirror for the
    /// calendar and, when messages were queued, the outbound mail worker.
    fn after_client_write(&self, outcome: &WriteOutcome, queued_mail: bool) {
        if let Some(hook) = &self.state.on_client_write {
            hook(outcome);
        }
        if queued_mail && let Some(rt) = &self.state.scheduling {
            rt.wake_outbound();
        }
    }
}

/// The percent-decoded path of an href that may be absolute
/// (`https://host/dav/...`) or already a path.
fn request_path_of(href: &str) -> String {
    let decoded = percent_decode_str(href.split('?').next().unwrap_or(""))
        .decode_utf8_lossy()
        .into_owned();
    match decoded.find("://") {
        Some(i) => decoded[i + 3..]
            .find('/')
            .map(|j| decoded[i + 3 + j..].to_string())
            .unwrap_or_default(),
        None => decoded,
    }
}

/// Whether a recurring object has at least one occurrence overlapping
/// `[start, end)`; unparsable objects count as overlapping (superset
/// rule).
fn recurring_overlaps(obj: &ObjectRow, start: Option<i64>, end: Option<i64>) -> bool {
    let events = match calendar::parse_vevents(&obj.ics) {
        Ok(e) => e,
        Err(_) => return true,
    };
    let Some(master) = events
        .iter()
        .find(|e| e.uid == obj.meta.uid && !calendar::is_recurrence_override(e))
    else {
        return true;
    };
    let ws = start
        .and_then(|s| Utc.timestamp_opt(s, 0).single())
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let we = end
        .and_then(|e| Utc.timestamp_opt(e, 0).single())
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    !calendar::expand_occurrences(master, ws, we).is_empty()
}

/// Split a comma-separated ETag list header value.
fn etag_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn http_date(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .unwrap_or_else(Utc::now)
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caldav::{CalDavClient, CalDavError, SyncOutcome};
    use crate::httpc::HttpUrl;
    use crate::jadav::store::{CalendarSpec, SendVia};
    use std::io::Read;

    const LOGIN: &str = "alice@example.com";
    const PASSWORD: &str = "secret";

    struct TestServer {
        base: String,
        store_path: PathBuf,
        state: Arc<ServerState>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.state.shutdown.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_file(&self.store_path);
            let _ = std::fs::remove_file(format!("{}-wal", self.store_path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.store_path.display()));
        }
    }

    fn spec(slug: &str, provider: Provider, two_way: bool) -> CalendarSpec {
        CalendarSpec {
            slug: slug.to_string(),
            display_name: match slug {
                "personal" => "Personal".to_string(),
                other => other.to_string(),
            },
            description: Some("desc".to_string()),
            color: Some("#00FF00".to_string()),
            timezone: None,
            identity: LOGIN.to_string(),
            provider,
            remote_account: (provider != Provider::Native).then(|| "r".to_string()),
            remote_calendar_id: None,
            two_way,
            send_via: SendVia::Smtp,
            is_default_for_identity: slug == "personal",
        }
    }

    fn start(label: &str) -> TestServer {
        start_with(label, None)
    }

    fn start_with(label: &str, scheduling: Option<imip::SchedulingRuntime>) -> TestServer {
        let store_path = std::env::temp_dir().join(format!(
            "jadav-server-test-{}-{}-{}.db",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let store = Store::open(&store_path).unwrap();
            store
                .set_principal(
                    LOGIN,
                    &[LOGIN.to_string(), "alice@other.example".to_string()],
                )
                .unwrap();
            store
                .reconcile_calendars(&[
                    spec("personal", Provider::Native, true),
                    spec("ro-mirror", Provider::Caldav, false),
                    spec("rw-mirror", Provider::Google, true),
                ])
                .unwrap();
        }
        let mut state = ServerState::new(
            store_path.clone(),
            LOGIN,
            PASSWORD,
            vec![LOGIN.to_string(), "alice@other.example".to_string()],
        );
        state.scheduling = scheduling.map(Arc::new);
        let state = Arc::new(state);
        let (_handle, addr) = spawn("127.0.0.1:0", Arc::clone(&state)).unwrap();
        TestServer {
            base: format!("http://{}", addr),
            store_path,
            state,
        }
    }

    fn client(ts: &TestServer) -> CalDavClient {
        CalDavClient::new(&format!("{}/dav/", ts.base), LOGIN, PASSWORD).unwrap()
    }

    fn auth() -> (String, String) {
        (
            "Authorization".to_string(),
            httpc::basic_auth_header(LOGIN, PASSWORD),
        )
    }

    fn raw(
        ts: &TestServer,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: Option<&str>,
    ) -> httpc::HttpResponse {
        let url = HttpUrl::parse(&format!("{}{}", ts.base, path)).unwrap();
        let mut headers = vec![auth()];
        for (k, v) in extra {
            headers.push((k.to_string(), v.to_string()));
        }
        httpc::send(
            method,
            &url,
            &headers,
            body.map(str::as_bytes),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn ics(uid: &str, summary: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:{summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    #[test]
    fn discovery_via_own_client_lists_calendars_not_inbox_or_outbox() {
        let ts = start("discovery");
        let cals = client(&ts).discover_calendars().unwrap();
        let mut names: Vec<String> = cals.iter().map(|c| c.display_name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["Personal", "ro-mirror", "rw-mirror"]);
        assert!(
            cals.iter()
                .all(|c| c.url.contains("/dav/calendars/alice@example.com/"))
        );
    }

    #[test]
    fn crud_and_sync_round_trip_via_own_client() {
        let ts = start("crud");
        let c = client(&ts);
        let cal_url = format!("{}/dav/calendars/{}/personal/", ts.base, LOGIN);
        let href = crate::caldav::new_event_href(&cal_url, "e1").unwrap();
        let etag1 = c.put_event(&href, &ics("e1", "One"), None, true).unwrap();
        assert!(etag1.starts_with('"'));
        // Duplicate create is refused.
        assert!(matches!(
            c.put_event(&href, &ics("e1", "Dup"), None, true),
            Err(CalDavError::Conflict(_))
        ));
        let (etag_get, body) = c.get_event(&href).unwrap();
        assert_eq!(etag_get.as_deref(), Some(etag1.as_str()));
        assert!(body.contains("SUMMARY:One"));
        // Stale update fails, fresh update succeeds and changes the etag.
        assert!(matches!(
            c.put_event(&href, &ics("e1", "Two"), Some("\"stale\""), false),
            Err(CalDavError::Conflict(_))
        ));
        let etag2 = c
            .put_event(&href, &ics("e1", "Two"), Some(&etag1), false)
            .unwrap();
        assert_ne!(etag1, etag2);
        // Sync: initial, then delta after a delete.
        let outcome = c.sync_calendar(&cal_url, None).unwrap();
        let token = match outcome {
            SyncOutcome::Delta {
                token,
                changed,
                deleted,
                ..
            } => {
                assert_eq!(changed.len(), 1);
                assert_eq!(changed[0].etag.as_deref(), Some(etag2.as_str()));
                assert!(
                    changed[0]
                        .calendar_data
                        .as_ref()
                        .unwrap()
                        .contains("SUMMARY:Two")
                );
                assert!(deleted.is_empty());
                token.unwrap()
            }
            SyncOutcome::FullResyncRequired => panic!("initial sync must succeed"),
        };
        assert!(token.starts_with("urn:jadav:personal:"));
        c.delete_event(&href, Some(&etag2)).unwrap();
        match c.sync_calendar(&cal_url, Some(&token)).unwrap() {
            SyncOutcome::Delta {
                changed, deleted, ..
            } => {
                assert!(changed.is_empty());
                assert_eq!(deleted.len(), 1);
                assert!(deleted[0].ends_with("/personal/e1.ics"));
            }
            SyncOutcome::FullResyncRequired => panic!("delta expected"),
        }
        // A garbage token asks for a full resync; the fallback listing works.
        assert!(matches!(
            c.sync_calendar(&cal_url, Some("urn:jadav:personal:99999"))
                .unwrap(),
            SyncOutcome::FullResyncRequired
        ));
        assert!(matches!(
            c.sync_calendar(&cal_url, Some("http://sabre.io/ns/sync/3"))
                .unwrap(),
            SyncOutcome::FullResyncRequired
        ));
        assert!(c.list_all_events(&cal_url).unwrap().is_empty());
        // Idempotent delete.
        c.delete_event(&href, None).unwrap();
    }

    #[test]
    fn options_and_well_known_and_healthz_without_auth() {
        let ts = start("options");
        let url = HttpUrl::parse(&format!("{}/dav/", ts.base)).unwrap();
        let resp = httpc::send("OPTIONS", &url, &[], None, Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("dav"), Some(DAV_HEADER));
        assert!(resp.header("allow").unwrap().contains("PROPFIND"));
        let health = httpc::send(
            "GET",
            &HttpUrl::parse(&format!("{}/healthz", ts.base)).unwrap(),
            &[],
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(health.status, 200);
        // .well-known redirect, checked on a bare socket (httpc follows redirects).
        let addr = ts.base.trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(&addr).unwrap();
        s.write_all(b"GET /.well-known/caldav HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 301"), "{out}");
        assert!(out.contains("Location: /dav/"));
    }

    #[test]
    fn missing_and_wrong_credentials_are_401_with_realm() {
        let ts = start("auth");
        let url = HttpUrl::parse(&format!("{}/dav/", ts.base)).unwrap();
        let resp = httpc::send(
            "PROPFIND",
            &url,
            &[("Depth".to_string(), "0".to_string())],
            Some(b""),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(resp.status, 401);
        assert_eq!(
            resp.header("www-authenticate"),
            Some("Basic realm=\"jadav\"")
        );
        let bad = vec![(
            "Authorization".to_string(),
            httpc::basic_auth_header(LOGIN, "nope"),
        )];
        let resp = httpc::send("PROPFIND", &url, &bad, Some(b""), Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 401);
    }

    #[test]
    fn propfind_splits_known_and_unknown_props_and_honours_depth() {
        let ts = start("propfind");
        let body = r#"<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:current-user-principal/><D:resourcetype/><X:nope xmlns:X="http://example.com/x"/></D:prop></D:propfind>"#;
        let resp = raw(&ts, "PROPFIND", "/dav/", &[("Depth", "0")], Some(body));
        assert_eq!(resp.status, 207);
        let text = resp.body_str();
        assert!(text.contains("<D:current-user-principal><D:href>/dav/principals/alice@example.com/</D:href></D:current-user-principal>"), "{text}");
        assert!(text.contains("<D:status>HTTP/1.1 404 Not Found</D:status>"));
        assert!(text.contains("<nope xmlns=\"http://example.com/x\"/>"));
        assert_eq!(text.matches("<D:response>").count(), 1);
        // Depth 1 on the root lists the two child collections.
        let resp = raw(&ts, "PROPFIND", "/dav/", &[("Depth", "1")], Some(body));
        assert_eq!(resp.body_str().matches("<D:response>").count(), 3);
        // Infinity is refused.
        let resp = raw(
            &ts,
            "PROPFIND",
            "/dav/",
            &[("Depth", "infinity")],
            Some(body),
        );
        assert_eq!(resp.status, 403);
        assert!(resp.body_str().contains("propfind-finite-depth"));
    }

    #[test]
    fn principal_carries_home_addresses_and_schedule_urls() {
        let ts = start("principal");
        let body = r#"<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><C:calendar-home-set/><C:calendar-user-address-set/><C:schedule-inbox-URL/><C:schedule-outbox-URL/><C:calendar-user-type/><D:principal-URL/></D:prop></D:propfind>"#;
        // Percent-encoded login in the path must resolve to the same principal.
        let resp = raw(
            &ts,
            "PROPFIND",
            "/dav/principals/alice%40example.com/",
            &[("Depth", "0")],
            Some(body),
        );
        assert_eq!(resp.status, 207);
        let text = resp.body_str();
        assert!(
            text.contains("<D:href>/dav/principals/alice@example.com/</D:href>"),
            "{text}"
        );
        assert!(text.contains("<C:calendar-home-set><D:href>/dav/calendars/alice@example.com/</D:href></C:calendar-home-set>"));
        assert!(text.contains(
            "<D:href>mailto:alice@example.com</D:href><D:href>mailto:alice@other.example</D:href>"
        ));
        assert!(text.contains("<C:schedule-inbox-URL><D:href>/dav/calendars/alice@example.com/inbox/</D:href></C:schedule-inbox-URL>"));
        assert!(text.contains("<C:calendar-user-type>INDIVIDUAL</C:calendar-user-type>"));
        assert!(!text.contains("404"));
    }

    #[test]
    fn home_depth_one_lists_calendars_then_inbox_and_outbox_with_privileges() {
        let ts = start("home");
        let body = r#"<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/" xmlns:A="http://apple.com/ns/ical/"><D:prop><D:resourcetype/><D:displayname/><CS:getctag/><D:sync-token/><A:calendar-color/><C:supported-calendar-component-set/><D:current-user-privilege-set/></D:prop></D:propfind>"#;
        let resp = raw(
            &ts,
            "PROPFIND",
            "/dav/calendars/alice@example.com/",
            &[("Depth", "1")],
            Some(body),
        );
        assert_eq!(resp.status, 207);
        let text = resp.body_str();
        assert_eq!(text.matches("<D:response>").count(), 6, "{text}");
        assert!(text.contains("<C:schedule-inbox/>"));
        assert!(text.contains("<C:schedule-outbox/>"));
        assert!(text.contains("<A:calendar-color>#00FF00</A:calendar-color>"));
        assert!(text.contains("<C:comp name=\"VEVENT\"/>"));
        // The read-only mirror advertises no write privilege; personal does.
        let ro_start = text.find("/ro-mirror/").unwrap();
        let ro_block = &text[ro_start..text[ro_start..].find("</D:response>").unwrap() + ro_start];
        assert!(!ro_block.contains("<D:write/>"), "{ro_block}");
        let p_start = text.find("/personal/").unwrap();
        let p_block = &text[p_start..text[p_start..].find("</D:response>").unwrap() + p_start];
        assert!(p_block.contains("<D:write/>"));
        assert!(p_block.contains("urn:jadav:personal:"));
    }

    #[test]
    fn proppatch_native_vs_mirror_and_all_or_nothing() {
        let ts = start("proppatch");
        let body = r#"<D:propertyupdate xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/"><D:set><D:prop><D:displayname>Renamed &amp; Co</D:displayname><A:calendar-color>#FF0000</A:calendar-color><A:calendar-order>7</A:calendar-order><C:schedule-calendar-transp><C:transparent/></C:schedule-calendar-transp></D:prop></D:set></D:propertyupdate>"#;
        let resp = raw(
            &ts,
            "PROPPATCH",
            "/dav/calendars/alice@example.com/personal/",
            &[],
            Some(body),
        );
        assert_eq!(resp.status, 207);
        assert!(!resp.body_str().contains("403"));
        let store = Store::open(&ts.store_path).unwrap();
        let cal = store.get_calendar("personal").unwrap().unwrap();
        assert_eq!(cal.display_name, "Renamed & Co");
        assert_eq!(cal.color.as_deref(), Some("#FF0000"));
        assert_eq!(cal.order, 7);
        assert!(cal.transparent);
        // On a mirror: colour allowed, name protected → nothing applied, 403 + 424.
        let resp = raw(
            &ts,
            "PROPPATCH",
            "/dav/calendars/alice@example.com/ro-mirror/",
            &[],
            Some(body),
        );
        let text = resp.body_str();
        assert!(text.contains("HTTP/1.1 403 Forbidden"));
        assert!(text.contains("HTTP/1.1 424 Failed Dependency"));
        assert!(text.contains("cannot-modify-protected-property"));
        let cal = store.get_calendar("ro-mirror").unwrap().unwrap();
        assert_eq!(cal.color.as_deref(), Some("#00FF00"));
        // Unknown property fails the whole batch too.
        let unknown = r#"<D:propertyupdate xmlns:D="DAV:"><D:set><D:prop><D:displayname>X</D:displayname><D:quota-used-bytes>1</D:quota-used-bytes></D:prop></D:set></D:propertyupdate>"#;
        let resp = raw(
            &ts,
            "PROPPATCH",
            "/dav/calendars/alice@example.com/personal/",
            &[],
            Some(unknown),
        );
        assert!(resp.body_str().contains("424"));
        assert_eq!(
            store
                .get_calendar("personal")
                .unwrap()
                .unwrap()
                .display_name,
            "Renamed & Co"
        );
    }

    #[test]
    fn mkcalendar_creates_then_405_and_delete_rules() {
        let ts = start("mkcalendar");
        let body = r#"<C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/"><D:set><D:prop><D:displayname>Work</D:displayname><A:calendar-color>#0000FF</A:calendar-color></D:prop></D:set></C:mkcalendar>"#;
        let resp = raw(
            &ts,
            "MKCALENDAR",
            "/dav/calendars/alice@example.com/7F3A-work/",
            &[],
            Some(body),
        );
        assert_eq!(resp.status, 201);
        let store = Store::open(&ts.store_path).unwrap();
        let cal = store.get_calendar("7F3A-work").unwrap().unwrap();
        assert_eq!(cal.display_name, "Work");
        assert_eq!(cal.identity, LOGIN);
        assert_eq!(cal.provider, Provider::Native);
        let resp = raw(
            &ts,
            "MKCALENDAR",
            "/dav/calendars/alice@example.com/7F3A-work/",
            &[],
            Some(body),
        );
        assert_eq!(resp.status, 405);
        assert_eq!(
            raw(
                &ts,
                "MKCALENDAR",
                "/dav/calendars/alice@example.com/inbox/",
                &[],
                Some(body)
            )
            .status,
            403
        );
        // Extended MKCOL without a calendar resourcetype is refused; with it, created.
        let plain = r#"<D:mkcol xmlns:D="DAV:"><D:set><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:set></D:mkcol>"#;
        assert_eq!(
            raw(
                &ts,
                "MKCOL",
                "/dav/calendars/alice@example.com/plain/",
                &[],
                Some(plain)
            )
            .status,
            403
        );
        let ext = r#"<D:mkcol xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:set><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype><D:displayname>Ext</D:displayname></D:prop></D:set></D:mkcol>"#;
        assert_eq!(
            raw(
                &ts,
                "MKCOL",
                "/dav/calendars/alice@example.com/ext/",
                &[],
                Some(ext)
            )
            .status,
            201
        );
        // DELETE: native 204, mirror 403, gone 404.
        assert_eq!(
            raw(
                &ts,
                "DELETE",
                "/dav/calendars/alice@example.com/ext/",
                &[],
                None
            )
            .status,
            204
        );
        assert_eq!(
            raw(
                &ts,
                "DELETE",
                "/dav/calendars/alice@example.com/ext/",
                &[],
                None
            )
            .status,
            404
        );
        assert_eq!(
            raw(
                &ts,
                "DELETE",
                "/dav/calendars/alice@example.com/rw-mirror/",
                &[],
                None
            )
            .status,
            403
        );
    }

    #[test]
    fn put_error_mapping_and_read_only_mirror() {
        let ts = start("put");
        let base = "/dav/calendars/alice@example.com";
        let ct = [("Content-Type", "text/calendar; charset=utf-8")];
        // Bare VEVENT → 415; VTODO → 403 supported-calendar-component.
        let bare = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20240101T000000Z\r\nEND:VEVENT\r\n";
        assert_eq!(
            raw(
                &ts,
                "PUT",
                &format!("{base}/personal/x.ics"),
                &ct,
                Some(bare)
            )
            .status,
            415
        );
        let todo = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:t\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let resp = raw(
            &ts,
            "PUT",
            &format!("{base}/personal/t.ics"),
            &ct,
            Some(todo),
        );
        assert_eq!(resp.status, 403);
        assert!(resp.body_str().contains("supported-calendar-component"));
        // Wrong media type → 415.
        assert_eq!(
            raw(
                &ts,
                "PUT",
                &format!("{base}/personal/j.ics"),
                &[("Content-Type", "application/json")],
                Some("{}")
            )
            .status,
            415
        );
        // Create, then the same UID under another href → 403 no-uid-conflict with the href.
        assert_eq!(
            raw(
                &ts,
                "PUT",
                &format!("{base}/personal/a.ics"),
                &ct,
                Some(&ics("a", "A"))
            )
            .status,
            201
        );
        let resp = raw(
            &ts,
            "PUT",
            &format!("{base}/personal/b.ics"),
            &ct,
            Some(&ics("a", "A2")),
        );
        assert_eq!(resp.status, 403);
        let text = resp.body_str();
        assert!(text.contains("no-uid-conflict"));
        assert!(text.contains("/personal/a.ics"), "{text}");
        // Read-only mirror refuses PUT and DELETE with need-privileges.
        let resp = raw(
            &ts,
            "PUT",
            &format!("{base}/ro-mirror/m.ics"),
            &ct,
            Some(&ics("m", "M")),
        );
        assert_eq!(resp.status, 403);
        assert!(resp.body_str().contains("need-privileges"));
        // If-Match list handling on PUT and DELETE.
        let etag = raw(&ts, "GET", &format!("{base}/personal/a.ics"), &[], None)
            .header("etag")
            .unwrap()
            .to_string();
        assert_eq!(
            raw(
                &ts,
                "PUT",
                &format!("{base}/personal/a.ics"),
                &[ct[0], ("If-Match", "\"x\", \"y\"")],
                Some(&ics("a", "B"))
            )
            .status,
            412
        );
        assert_eq!(
            raw(
                &ts,
                "PUT",
                &format!("{base}/personal/a.ics"),
                &[ct[0], ("If-Match", &format!("\"x\", {etag}"))],
                Some(&ics("a", "B"))
            )
            .status,
            204
        );
        assert_eq!(
            raw(
                &ts,
                "DELETE",
                &format!("{base}/personal/a.ics"),
                &[("If-Match", "\"stale\"")],
                None
            )
            .status,
            412
        );
        assert_eq!(
            raw(
                &ts,
                "DELETE",
                &format!("{base}/personal/a.ics"),
                &[("If-Match", "*")],
                None
            )
            .status,
            204
        );
    }

    #[test]
    fn get_head_and_304() {
        let ts = start("get");
        let base = "/dav/calendars/alice@example.com/personal";
        let ct = [("Content-Type", "text/calendar")];
        assert_eq!(
            raw(
                &ts,
                "PUT",
                &format!("{base}/g.ics"),
                &ct,
                Some(&ics("g", "G"))
            )
            .status,
            201
        );
        let get = raw(&ts, "GET", &format!("{base}/g.ics"), &[], None);
        assert_eq!(get.status, 200);
        assert_eq!(
            get.header("content-type"),
            Some("text/calendar; charset=utf-8")
        );
        assert!(get.header("last-modified").unwrap().ends_with("GMT"));
        let etag = get.header("etag").unwrap().to_string();
        assert!(get.body_str().contains("SUMMARY:G"));
        let head = raw(&ts, "HEAD", &format!("{base}/g.ics"), &[], None);
        assert_eq!(head.status, 200);
        assert_eq!(head.header("etag"), Some(etag.as_str()));
        assert!(head.body.is_empty());
        assert_eq!(
            raw(
                &ts,
                "GET",
                &format!("{base}/g.ics"),
                &[("If-None-Match", &etag)],
                None
            )
            .status,
            304
        );
        assert_eq!(
            raw(&ts, "GET", &format!("{base}/missing.ics"), &[], None).status,
            404
        );
    }

    #[test]
    fn calendar_query_time_range_keeps_recurring_master_outside_its_own_window() {
        let ts = start("query");
        let base = "/dav/calendars/alice@example.com/personal";
        let ct = [("Content-Type", "text/calendar")];
        raw(
            &ts,
            "PUT",
            &format!("{base}/single.ics"),
            &ct,
            Some(&ics("single", "Jan 15")),
        );
        let recurring = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:weekly\r\nDTSTART:20230102T090000Z\r\nDTEND:20230102T100000Z\r\nRRULE:FREQ=WEEKLY;BYDAY=MO\r\nSUMMARY:Weekly\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        raw(
            &ts,
            "PUT",
            &format!("{base}/weekly.ics"),
            &ct,
            Some(recurring),
        );
        let ended = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ended\r\nDTSTART:20230102T090000Z\r\nDTEND:20230102T100000Z\r\nRRULE:FREQ=WEEKLY;COUNT=2\r\nSUMMARY:Ended\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        raw(&ts, "PUT", &format!("{base}/ended.ics"), &ct, Some(ended));
        // Monday 2024-01-15 is in the window; the ended series is not.
        let query = r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="20240115T000000Z" end="20240116T000000Z"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#;
        let resp = raw(
            &ts,
            "REPORT",
            &format!("{base}/"),
            &[("Depth", "1")],
            Some(query),
        );
        assert_eq!(resp.status, 207);
        let text = resp.body_str();
        assert!(text.contains("/personal/single.ics"));
        assert!(text.contains("/personal/weekly.ics"));
        assert!(!text.contains("/personal/ended.ics"), "{text}");
        assert!(
            !text.contains("calendar-data"),
            "only getetag was asked for"
        );
        // A VTODO filter matches nothing.
        let todo = r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VTODO"/></C:comp-filter></C:filter></C:calendar-query>"#;
        assert_eq!(
            raw(&ts, "REPORT", &format!("{base}/"), &[], Some(todo))
                .body_str()
                .matches("<D:response>")
                .count(),
            0
        );
    }

    #[test]
    fn multiget_reports_404_for_unknown_hrefs_and_sync_token_validation() {
        let ts = start("multiget");
        let base = "/dav/calendars/alice@example.com/personal";
        let ct = [("Content-Type", "text/calendar")];
        raw(
            &ts,
            "PUT",
            &format!("{base}/m.ics"),
            &ct,
            Some(&ics("m", "M")),
        );
        let mg = format!(
            r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><D:href>{base}/m.ics</D:href><D:href>{base}/nope.ics</D:href><D:href>/elsewhere/x.ics</D:href></C:calendar-multiget>"#
        );
        let text = raw(&ts, "REPORT", &format!("{base}/"), &[], Some(&mg)).body_str();
        assert!(text.contains("SUMMARY:M"));
        assert_eq!(text.matches("HTTP/1.1 404 Not Found").count(), 2);
        // Foreign / malformed sync tokens are a 403 valid-sync-token.
        let sync = r#"<D:sync-collection xmlns:D="DAV:"><D:sync-token>http://sabre.io/ns/sync/10</D:sync-token><D:sync-level>1</D:sync-level><D:prop><D:getetag/></D:prop></D:sync-collection>"#;
        let resp = raw(&ts, "REPORT", &format!("{base}/"), &[], Some(sync));
        assert_eq!(resp.status, 403);
        assert!(resp.body_str().contains("valid-sync-token"));
        let unsupported = r#"<D:expand-property xmlns:D="DAV:"/>"#;
        assert_eq!(
            raw(&ts, "REPORT", &format!("{base}/"), &[], Some(unsupported)).status,
            403
        );
    }

    #[test]
    fn keep_alive_serves_two_requests_on_one_connection_and_answers_expect() {
        let ts = start("keepalive");
        let addr = ts.base.trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(&addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let a = httpc::basic_auth_header(LOGIN, PASSWORD);
        let body = ics("k", "K");
        let req1 = format!(
            "PUT /dav/calendars/alice@example.com/personal/k.ics HTTP/1.1\r\nHost: x\r\nAuthorization: {a}\r\nContent-Type: text/calendar\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(req1.as_bytes()).unwrap();
        // Read the interim 100 Continue before sending the body.
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("HTTP/1.1 100"), "{line}");
        reader.read_line(&mut line).unwrap(); // blank
        s.write_all(body.as_bytes()).unwrap();
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.starts_with("HTTP/1.1 201"), "{status}");
        // Drain headers of response 1.
        let mut headers = String::new();
        let mut saw_keep_alive = false;
        loop {
            headers.clear();
            reader.read_line(&mut headers).unwrap();
            if headers
                .to_ascii_lowercase()
                .contains("connection: keep-alive")
            {
                saw_keep_alive = true;
            }
            if headers == "\r\n" {
                break;
            }
        }
        assert!(saw_keep_alive);
        // Second request on the same socket.
        let req2 = format!(
            "GET /dav/calendars/alice@example.com/personal/k.ics HTTP/1.1\r\nHost: x\r\nAuthorization: {a}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req2.as_bytes()).unwrap();
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        assert!(rest.starts_with("HTTP/1.1 200"), "{rest}");
        assert!(rest.contains("SUMMARY:K"));
    }

    #[test]
    fn href_encoding_matches_sabre() {
        assert_eq!(
            encode_href("/dav/calendars/t@mas.gg/personal/"),
            "/dav/calendars/t@mas.gg/personal/"
        );
        assert_eq!(encode_href("/dav/a b/c(d):e.ics"), "/dav/a%20b/c(d):e.ics");
        assert_eq!(encode_href("/dav/ü.ics"), "/dav/%C3%BC.ics");
        assert_eq!(encode_href("/dav/x#y?z.ics"), "/dav/x%23y%3Fz.ics");
    }

    #[test]
    fn split_target_decodes_and_strips_query() {
        let (path, segs, slash) = split_target("/dav/calendars/alice%40example.com/p/?x=1");
        assert_eq!(path, "/dav/calendars/alice@example.com/p/");
        assert_eq!(segs, vec!["dav", "calendars", "alice@example.com", "p"]);
        assert!(slash);
        let (_, segs, slash) = split_target("/dav/calendars/a/p/e%20v.ics");
        assert_eq!(segs.last().unwrap(), "e v.ics");
        assert!(!slash);
    }

    // ------------------------------------------------------------------
    // Scheduling surfaces (RFC 6638) with the iTIP broker enabled
    // ------------------------------------------------------------------

    fn start_scheduling(label: &str) -> TestServer {
        start_with(
            label,
            Some(imip::SchedulingRuntime {
                identities: vec![LOGIN.to_string(), "alice@other.example".to_string()],
                identity_names: HashMap::from([(LOGIN.to_string(), "Alice".to_string())]),
                significant_properties: itip::DEFAULT_SIGNIFICANT_PROPERTIES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                prodid: itip::DEFAULT_PRODID.to_string(),
                templates: imip::SubjectTemplates::default(),
                default_timezone: "UTC".to_string(),
                message_id_domain: "example.com".to_string(),
                inbox_for_mirrored: false,
                retry_max_age_secs: 3600,
                mail_enabled: true,
                outbound_wake: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
            }),
        )
    }

    fn organised_ics(uid: &str, summary: &str, attendees: &[&str]) -> String {
        let mut s = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20240101T000000Z\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:{summary}\r\nSEQUENCE:0\r\nORGANIZER;CN=Alice:mailto:{LOGIN}\r\n"
        );
        for a in attendees {
            s.push_str(&format!(
                "ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{a}\r\n"
            ));
        }
        s.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
        s
    }

    #[test]
    fn organiser_put_queues_invitations_stamps_status_and_omits_etag() {
        let ts = start_scheduling("sched-put");
        let base = format!("/dav/calendars/{LOGIN}/personal");
        let ct = ("Content-Type", "text/calendar; charset=utf-8");
        let put = raw(
            &ts,
            "PUT",
            &format!("{base}/org.ics"),
            &[ct],
            Some(&organised_ics(
                "org-1",
                "Kickoff",
                &["bob@example.com", "carol@example.com"],
            )),
        );
        assert_eq!(put.status, 201);
        assert!(
            put.header("etag").is_none(),
            "rewritten document must not echo an ETag"
        );
        assert!(put.header("schedule-tag").is_some());

        let store = Store::open(&ts.store_path).unwrap();
        let obj = store.get_object("personal", "org.ics").unwrap().unwrap();
        assert_eq!(obj.ics.matches("SCHEDULE-STATUS=1.0").count(), 2);
        assert_eq!(store.outbound_counts().unwrap(), (2, 0, 0));
        let queued = store.due_outbound(i64::MAX, 10).unwrap();
        let mut recipients: Vec<&str> = queued.iter().map(|q| q.job.recipient.as_str()).collect();
        recipients.sort();
        assert_eq!(recipients, vec!["bob@example.com", "carol@example.com"]);
        assert!(
            queued
                .iter()
                .all(|q| q.job.method == "REQUEST" && q.job.sender == LOGIN)
        );

        // A pure re-PUT of what the server stored changes nothing and
        // queues nothing more.
        let again = raw(
            &ts,
            "PUT",
            &format!("{base}/org.ics"),
            &[ct],
            Some(&obj.ics),
        );
        assert_eq!(again.status, 204);
        assert_eq!(store.outbound_counts().unwrap().0, 2);

        // Deleting the organised event cancels it for everybody.
        let del = raw(&ts, "DELETE", &format!("{base}/org.ics"), &[], None);
        assert_eq!(del.status, 204);
        assert_eq!(store.outbound_counts().unwrap().0, 4);
        let cancels = store
            .due_outbound(i64::MAX, 10)
            .unwrap()
            .into_iter()
            .filter(|q| q.job.method == "CANCEL")
            .count();
        assert_eq!(cancels, 2);
    }

    #[test]
    fn attendee_rsvp_queues_one_reply_and_import_queues_nothing() {
        let ts = start_scheduling("sched-rsvp");
        let base = format!("/dav/calendars/{LOGIN}/personal");
        let ct = ("Content-Type", "text/calendar; charset=utf-8");
        let invite = |partstat: &str| {
            format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//g//EN\r\nBEGIN:VEVENT\r\nUID:inv-1\r\nDTSTAMP:20240101T000000Z\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:Lunch\r\nSEQUENCE:0\r\nORGANIZER;CN=Boss:mailto:boss@example.com\r\nATTENDEE;PARTSTAT={partstat};RSVP=TRUE:mailto:{LOGIN}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:dave@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            )
        };
        let first = raw(
            &ts,
            "PUT",
            &format!("{base}/inv.ics"),
            &[ct],
            Some(&invite("NEEDS-ACTION")),
        );
        assert_eq!(first.status, 201);
        assert!(
            first.header("etag").is_some(),
            "an import is stored as received"
        );
        let store = Store::open(&ts.store_path).unwrap();
        assert_eq!(store.outbound_counts().unwrap(), (0, 0, 0));

        let rsvp = raw(
            &ts,
            "PUT",
            &format!("{base}/inv.ics"),
            &[ct],
            Some(&invite("ACCEPTED")),
        );
        assert_eq!(rsvp.status, 204);
        assert_eq!(store.outbound_counts().unwrap(), (1, 0, 0));
        let q = store.due_outbound(i64::MAX, 10).unwrap();
        assert_eq!(q[0].job.method, "REPLY");
        assert_eq!(q[0].job.recipient, "boss@example.com");
        assert_eq!(q[0].job.sender, LOGIN);
        let obj = store.get_object("personal", "inv.ics").unwrap().unwrap();
        assert!(
            obj.ics
                .contains("ORGANIZER;CN=Boss;SCHEDULE-STATUS=1.0:mailto:boss@example.com")
        );
    }

    #[test]
    fn schedule_tag_mismatch_is_412_before_the_broker_runs() {
        let ts = start_scheduling("sched-tag");
        let base = format!("/dav/calendars/{LOGIN}/personal");
        let ct = ("Content-Type", "text/calendar; charset=utf-8");
        let put = raw(
            &ts,
            "PUT",
            &format!("{base}/t.ics"),
            &[ct],
            Some(&organised_ics("tag-1", "Tagged", &["bob@example.com"])),
        );
        let tag = put.header("schedule-tag").unwrap().to_string();
        let store = Store::open(&ts.store_path).unwrap();
        let stored = store.get_object("personal", "t.ics").unwrap().unwrap();
        let stale = raw(
            &ts,
            "PUT",
            &format!("{base}/t.ics"),
            &[ct, ("If-Schedule-Tag-Match", "\"999\"")],
            Some(&stored.ics),
        );
        assert_eq!(stale.status, 412);
        let del_stale = raw(
            &ts,
            "DELETE",
            &format!("{base}/t.ics"),
            &[("If-Schedule-Tag-Match", "\"999\"")],
            None,
        );
        assert_eq!(del_stale.status, 412);
        assert_eq!(
            store.outbound_counts().unwrap().0,
            1,
            "nothing queued by rejected writes"
        );
        let fresh = raw(
            &ts,
            "PUT",
            &format!("{base}/t.ics"),
            &[ct, ("If-Schedule-Tag-Match", &tag)],
            Some(&stored.ics),
        );
        assert_eq!(fresh.status, 204);
    }

    #[test]
    fn inbox_lists_gets_multigets_and_deletes_items() {
        let ts = start("inbox");
        let store = Store::open(&ts.store_path).unwrap();
        let itip = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//g//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:in-1\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:Inbound\r\nORGANIZER:mailto:boss@example.com\r\nATTENDEE:mailto:alice@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let item = store.inbox_insert(LOGIN, "in-1", "REQUEST", itip).unwrap();
        let inbox = format!("/dav/calendars/{LOGIN}/inbox/");

        let list = raw(&ts, "PROPFIND", &inbox, &[("Depth", "1")], None);
        assert_eq!(list.status, 207);
        let text = String::from_utf8_lossy(&list.body).into_owned();
        assert!(
            text.contains(&format!("{inbox}{}", item.href_name)),
            "{text}"
        );
        assert!(text.contains(&xml::escape_text(&item.etag)));
        assert!(text.contains("schedule-inbox"));

        let get = raw(&ts, "GET", &format!("{inbox}{}", item.href_name), &[], None);
        assert_eq!(get.status, 200);
        assert_eq!(get.header("etag"), Some(item.etag.as_str()));
        assert_eq!(String::from_utf8_lossy(&get.body), itip);
        let head = raw(
            &ts,
            "HEAD",
            &format!("{inbox}{}", item.href_name),
            &[],
            None,
        );
        assert_eq!(head.status, 200);
        assert!(head.body.is_empty());

        let multiget = format!(
            r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><D:href>{inbox}{}</D:href><D:href>{inbox}missing.ics</D:href></C:calendar-multiget>"#,
            item.href_name
        );
        let rep = raw(
            &ts,
            "REPORT",
            &inbox,
            &[("Content-Type", "application/xml"), ("Depth", "1")],
            Some(&multiget),
        );
        assert_eq!(rep.status, 207);
        let text = String::from_utf8_lossy(&rep.body).into_owned();
        assert!(text.contains("METHOD:REQUEST"), "{text}");
        assert!(text.contains("HTTP/1.1 404"), "{text}");

        let put = raw(
            &ts,
            "PUT",
            &format!("{inbox}{}", item.href_name),
            &[("Content-Type", "text/calendar")],
            Some(itip),
        );
        assert_eq!(put.status, 403);
        let del = raw(
            &ts,
            "DELETE",
            &format!("{inbox}{}", item.href_name),
            &[],
            None,
        );
        assert_eq!(del.status, 204);
        let gone = raw(&ts, "GET", &format!("{inbox}{}", item.href_name), &[], None);
        assert_eq!(gone.status, 404);
        assert!(store.inbox_list().unwrap().is_empty());
    }

    #[test]
    fn outbox_post_answers_free_busy_with_no_scheduling_support() {
        let ts = start("outbox");
        let outbox = format!("/dav/calendars/{LOGIN}/outbox/");
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//ios//EN\r\nMETHOD:REQUEST\r\nBEGIN:VFREEBUSY\r\nUID:fb-1\r\nDTSTAMP:20240101T000000Z\r\nDTSTART:20240115T000000Z\r\nDTEND:20240116T000000Z\r\nORGANIZER:mailto:alice@example.com\r\nATTENDEE:mailto:bob@example.com\r\nEND:VFREEBUSY\r\nEND:VCALENDAR\r\n";
        let resp = raw(
            &ts,
            "POST",
            &outbox,
            &[("Content-Type", "text/calendar; charset=utf-8")],
            Some(body),
        );
        assert_eq!(resp.status, 200);
        let text = String::from_utf8_lossy(&resp.body).into_owned();
        assert!(text.contains("schedule-response"), "{text}");
        assert!(text.contains("mailto:bob@example.com"), "{text}");
        assert!(text.contains("3.7"), "{text}");
    }
}
