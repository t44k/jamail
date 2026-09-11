//! A minimal, real inbound CalDAV (RFC 4791) + WebDAV (RFC 4918) HTTP
//! server hosted inside `jamaild`, exposing each `caldav`-configured
//! account's local calendar cache to any CalDAV client — including this
//! crate's own [`crate::caldav::CalDavClient`], which the tests below use
//! as a genuine end-to-end interoperability check between this crate's
//! client and server implementations.
//!
//! This is the mirror image of [`crate::caldav`] (which makes jamaild a
//! CalDAV *client* against someone else's server): this module makes
//! jamaild itself answerable as a CalDAV *server*. The two are
//! independent and can both be active for the same account at once (sync
//! from a remote server into the cache, *and* serve that same cache back
//! out) — nothing here assumes only one role is in use.
//!
//! ## Opt-in and lifecycle
//!
//! Off by default. Set `daemon.caldav_server_listen` (e.g.
//! `"127.0.0.1:5232"`) in `config.yaml` to bind it; unset, `jamaild`
//! starts exactly as it did before this module existed. When bound, one
//! calendar named "Default" is auto-created (if not already present) for
//! every account with `caldav` configured, under
//! `calendar_url = "server:<account>"` — a namespace distinct from any
//! remote calendar URL a client-role sync populates, so the two never
//! collide or get served in place of each other (see [`propfind_calendars`]
//! filtering on the `"server:"` prefix).
//!
//! ## Wire shape
//!
//! ```text
//! /dav/<account>/                          PROPFIND -> current-user-principal
//! /dav/<account>/principal/                PROPFIND -> calendar-home-set
//! /dav/<account>/calendars/                PROPFIND -> list calendar collections
//! /dav/<account>/calendars/default/        PROPFIND -> collection props
//!                                           REPORT   -> calendar-query | sync-collection
//! /dav/<account>/calendars/default/<uid>.ics   GET / PUT / DELETE
//! ```
//!
//! Authentication is HTTP Basic, checked against that account's own
//! `caldav.login`/`caldav.auth` — the same credentials already used when
//! this account acts as a CalDAV *client*, reused for its server role
//! rather than inventing a second, parallel credential type.
//!
//! ## Explicit, documented limitations (a real but minimal server)
//!
//! - **One calendar per account** ("Default"), auto-created. No
//!   `MKCALENDAR`/calendar deletion.
//! - **`calendar-query` ignores filters** (time-range, comp-filter beyond
//!   "is it a VEVENT") and always returns every event in the calendar —
//!   correct-but-unfiltered, which every real CalDAV client tolerates
//!   (clients filter client-side too), just not bandwidth-optimal for a
//!   large calendar.
//! - **PROPFIND ignores the requested `<D:prop>` list** and always returns
//!   a fixed, small prop set appropriate to the resource — technically
//!   returns props a client didn't ask for, which RFC 4918 doesn't
//!   forbid a client from ignoring, and this crate's own client (and, in
//!   practice, real ones) does exactly that.
//! - **The sync-collection change log never compacts** — every
//!   create/update/delete appends a row to `calendar_sync_log` forever.
//!   Fine at the scale this is designed for (a personal calendar), a real
//!   concern only for extremely long-lived, high-churn calendars.
//! - **No `.well-known/caldav` redirect, no MKCALENDAR, no free-busy, no
//!   scheduling (iTIP)** — this serves calendar *storage*, not a full
//!   groupware scheduling stack.
//! - Graceful shutdown is best-effort (a polled flag, not a guaranteed
//!   in-flight-request drain) — consistent with how this crate already
//!   treats its other background threads (see `sync::SyncControl`'s own
//!   documented shutdown caveat).

use crate::config::JamailAccount;
use crate::db::{DeleteError, MailDb, PutError, PutOutcome};
use crate::httpc;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// How long the accept loop sleeps between non-blocking `accept()` polls
/// while idle — bounds how quickly a graceful shutdown is noticed.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// The internal `calendar_url` for a given account's server-hosted
/// "Default" calendar — namespaced with a `server:` prefix so it can never
/// collide with (or be confused for) a remote calendar URL synced via this
/// account's client role (see `caldav`/`calsync`).
fn default_calendar_url(account: &str) -> String {
    format!("server:{}", account)
}

fn account_base_path(account: &str) -> String {
    format!("/dav/{}/", account)
}
fn principal_path(account: &str) -> String {
    format!("/dav/{}/principal/", account)
}
fn calendars_base_path(account: &str) -> String {
    format!("/dav/{}/calendars/", account)
}
fn default_calendar_path(account: &str) -> String {
    format!("/dav/{}/calendars/default/", account)
}

/// The href this server assigns to an event with `uid` in `account`'s
/// hosted "Default" calendar — exactly the path a CalDAV client would
/// `PUT` it to. Used by `calsync` to give a locally-created event a real
/// path when it commits a write to a server-hosted calendar, so the event
/// is immediately `GET`-able by clients of this same server.
pub fn default_event_href(account: &str, uid: &str) -> String {
    format!(
        "{}{}.ics",
        default_calendar_path(account),
        crate::caldav::sanitize_uid(uid)
    )
}

/// Bind `listen_addr` (e.g. `"127.0.0.1:5232"` or `"127.0.0.1:0"` to let
/// the OS pick a free port — used by tests) without starting the accept
/// loop yet. Split from [`spawn`] so tests can learn the real bound
/// address ([`TcpListener::local_addr`]) before anything starts serving.
pub fn bind(listen_addr: &str) -> Result<TcpListener> {
    let listener = TcpListener::bind(listen_addr)
        .with_context(|| format!("binding CalDAV server to {}", listen_addr))?;
    listener
        .set_nonblocking(true)
        .context("setting CalDAV server listener non-blocking")?;
    Ok(listener)
}

/// Ensure every `caldav`-configured account has its "Default" calendar
/// row. Idempotent (`upsert_calendar` is itself idempotent and never
/// resets sync state on a repeat call).
fn ensure_default_calendars(
    db_path: &Path,
    accounts: &HashMap<String, JamailAccount>,
) -> Result<()> {
    let db = MailDb::open_at(db_path)?;
    for (name, account) in accounts {
        if account.caldav.is_some() {
            db.upsert_calendar(name, &default_calendar_url(name), "Default")?;
        }
    }
    Ok(())
}

/// Run the accept loop on an already-bound listener until `shutdown` is
/// set. Each connection is handled on its own thread, one request per
/// connection (this server, like [`crate::httpc`]'s client, doesn't do
/// HTTP keep-alive).
pub fn serve(
    listener: TcpListener,
    db_path: PathBuf,
    accounts: Arc<HashMap<String, JamailAccount>>,
    shutdown: Arc<AtomicBool>,
) {
    if let Err(e) = ensure_default_calendars(&db_path, &accounts) {
        eprintln!(
            "jamaild: CalDAV server: failed to initialize default calendars: {}",
            e
        );
    }
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // On some platforms, a stream accepted from a
                // non-blocking listener inherits non-blocking mode too
                // (e.g. via `accept4`+`SOCK_NONBLOCK`); force it back to
                // blocking so the read/write timeouts set in
                // `handle_connection` actually apply, instead of every
                // read failing immediately with `EAGAIN`.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let accounts = Arc::clone(&accounts);
                let db_path = db_path.clone();
                thread::spawn(move || {
                    let _ = handle_connection(stream, &db_path, &accounts);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(_) => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
        }
    }
}

/// Bind `listen_addr` and spawn the accept loop on a background thread.
/// Returns the thread handle and the actual bound address (useful when
/// `listen_addr` ends in `:0`).
pub fn spawn(
    listen_addr: &str,
    db_path: PathBuf,
    accounts: Arc<HashMap<String, JamailAccount>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(thread::JoinHandle<()>, std::net::SocketAddr)> {
    let listener = bind(listen_addr)?;
    let addr = listener
        .local_addr()
        .context("reading CalDAV server's bound local address")?;
    let handle = thread::spawn(move || serve(listener, db_path, accounts, shutdown));
    Ok((handle, addr))
}

// ---------------------------------------------------------------------
// Request/response plumbing
// ---------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

type Resp = (u16, &'static str, Vec<(String, String)>, Vec<u8>);

fn read_request(r: &mut impl std::io::BufRead) -> Result<HttpRequest> {
    let line = httpc::read_line(r)?;
    let mut parts = line.splitn(3, ' ');
    let method = parts.next().context("empty request line")?.to_string();
    let path = parts
        .next()
        .context("request line missing path")?
        .to_string();
    let headers = httpc::read_headers(r)?;
    let body = read_request_body(r, &headers)?;
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

/// Read a *request* body — deliberately not [`httpc::read_body`] (which is
/// correct for *responses*, where "no Content-Length, no
/// Transfer-Encoding" legitimately means "read until the peer closes the
/// connection", since `Connection: close` is this crate's client's own
/// convention). For a request, no declared length means no body at all —
/// treating it as "read to EOF" would deadlock, since the client isn't
/// expected to half-close its write side after a bodyless GET/DELETE
/// while it's still waiting to read our response on the same socket.
fn read_request_body(
    r: &mut impl std::io::BufRead,
    headers: &HashMap<String, String>,
) -> Result<Vec<u8>> {
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
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        return Ok(buf);
    }
    Ok(Vec::new())
}

fn write_response(stream: &mut TcpStream, resp: Resp) -> std::io::Result<()> {
    let (status, reason, headers, body) = resp;
    let mut out = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", status, reason).as_bytes());
    for (k, v) in &headers {
        out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(&body);
    stream.write_all(&out)?;
    stream.flush()
}

fn handle_connection(
    mut stream: TcpStream,
    db_path: &Path,
    accounts: &HashMap<String, JamailAccount>,
) -> Result<()> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let req = match read_request(&mut reader) {
        Ok(r) => r,
        Err(_) => return Ok(()), // malformed request: just close, like a real server would on garbage input
    };
    let resp = dispatch(&req, db_path, accounts);
    write_response(&mut stream, resp)?;
    Ok(())
}

fn not_found() -> Resp {
    (404, "Not Found", vec![], Vec::new())
}
fn internal_error() -> Resp {
    (500, "Internal Server Error", vec![], Vec::new())
}
fn unauthorized() -> Resp {
    (
        401,
        "Unauthorized",
        vec![(
            "WWW-Authenticate".to_string(),
            "Basic realm=\"jamaild\"".to_string(),
        )],
        Vec::new(),
    )
}
fn multistatus(body: String) -> Resp {
    (
        207,
        "Multi-Status",
        vec![(
            "Content-Type".to_string(),
            "application/xml; charset=utf-8".to_string(),
        )],
        body.into_bytes(),
    )
}

fn check_auth(req: &HttpRequest, caldav_cfg: &crate::config::CalDavConfig) -> Result<(), Resp> {
    let Some(auth_header) = req.headers.get("authorization") else {
        return Err(unauthorized());
    };
    let password = caldav_cfg
        .auth
        .resolve_password()
        .map_err(|_| internal_error())?;
    let expected = httpc::basic_auth_header(&caldav_cfg.login, &password);
    if auth_header == &expected {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

fn dispatch(req: &HttpRequest, db_path: &Path, accounts: &HashMap<String, JamailAccount>) -> Resp {
    let segments: Vec<&str> = req
        .path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if segments.len() < 2 || segments[0] != "dav" {
        return not_found();
    }
    let account_name = segments[1];
    let Some(account) = accounts.get(account_name) else {
        return not_found();
    };
    let Some(caldav_cfg) = &account.caldav else {
        return not_found();
    };
    if let Err(resp) = check_auth(req, caldav_cfg) {
        return resp;
    }

    let db = match MailDb::open_at(db_path) {
        Ok(d) => d,
        Err(_) => return internal_error(),
    };

    let rest = &segments[2..];
    match (req.method.as_str(), rest) {
        ("PROPFIND", []) => propfind_account_root(account_name),
        ("PROPFIND", ["principal"]) => propfind_principal(account_name),
        ("PROPFIND", ["calendars"]) => propfind_calendars(account_name, &db),
        ("PROPFIND", ["calendars", "default"]) => propfind_calendar_collection(account_name, &db),
        ("REPORT", ["calendars", "default"]) => report_calendar(account_name, &db, req),
        ("GET", ["calendars", "default", _tail]) => get_event(account_name, &db, req),
        ("PUT", ["calendars", "default", _tail]) => put_event(account_name, &db, req),
        ("DELETE", ["calendars", "default", _tail]) => delete_event(account_name, &db, req),
        _ => not_found(),
    }
}

// ---------------------------------------------------------------------
// XML building
// ---------------------------------------------------------------------

const MULTISTATUS_OPEN: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/">"#;
const MULTISTATUS_CLOSE: &str = "</D:multistatus>";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn propfind_account_root(account: &str) -> Resp {
    let body = format!(
        "{}<D:response><D:href>{}</D:href><D:propstat><D:prop><D:current-user-principal><D:href>{}</D:href></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>{}",
        MULTISTATUS_OPEN,
        xml_escape(&account_base_path(account)),
        xml_escape(&principal_path(account)),
        MULTISTATUS_CLOSE
    );
    multistatus(body)
}

fn propfind_principal(account: &str) -> Resp {
    let body = format!(
        "{}<D:response><D:href>{}</D:href><D:propstat><D:prop><C:calendar-home-set><D:href>{}</D:href></C:calendar-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>{}",
        MULTISTATUS_OPEN,
        xml_escape(&principal_path(account)),
        xml_escape(&calendars_base_path(account)),
        MULTISTATUS_CLOSE
    );
    multistatus(body)
}

fn propfind_calendars(account: &str, db: &MailDb) -> Resp {
    let calendar_url = default_calendar_url(account);
    let ctag = db.server_current_token(account, &calendar_url).unwrap_or(0);
    let body = format!(
        "{}<D:response><D:href>{}</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype><D:displayname>Default</D:displayname><CS:getctag>{}</CS:getctag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>{}",
        MULTISTATUS_OPEN,
        xml_escape(&default_calendar_path(account)),
        ctag,
        MULTISTATUS_CLOSE
    );
    multistatus(body)
}

fn propfind_calendar_collection(account: &str, db: &MailDb) -> Resp {
    // Same representation as one row of the calendars listing above — a
    // client PROPFINDing the collection directly (depth 0) expects the
    // same props.
    propfind_calendars(account, db)
}

fn event_response_block(href: &str, etag: &str, ics: &str) -> String {
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop><D:getetag>{}</D:getetag><C:calendar-data>{}</C:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        xml_escape(href),
        xml_escape(etag),
        xml_escape(ics)
    )
}

fn deleted_response_block(href: &str) -> String {
    format!(
        "<D:response><D:href>{}</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>",
        xml_escape(href)
    )
}

/// Very small, deliberately non-general REPORT body sniffing: real clients
/// (and this crate's own [`crate::caldav`]) always name the report type as
/// the XML document's root element, so a substring check for the two
/// report names this server supports is sufficient without pulling in a
/// second full XML parser for request bodies whose shape this crate fully
/// controls on the client side already (see module docs).
fn is_sync_collection_report(body: &str) -> bool {
    body.contains("sync-collection")
}

/// Extract the text content of a `<...sync-token>...</...:sync-token>`
/// element regardless of namespace prefix. Returns `None` if absent or
/// empty (an initial sync).
fn extract_sync_token(body: &str) -> Option<i64> {
    let start_tag = "sync-token>";
    let start = body.find(start_tag)? + start_tag.len();
    let end = body[start..].find("</")? + start;
    body[start..end].trim().parse::<i64>().ok()
}

fn report_calendar(account: &str, db: &MailDb, req: &HttpRequest) -> Resp {
    let calendar_url = default_calendar_url(account);
    let body_str = String::from_utf8_lossy(&req.body);

    if is_sync_collection_report(&body_str) {
        let since = extract_sync_token(&body_str).unwrap_or(0);
        let (changes, new_token) = match db.server_sync_changes(account, &calendar_url, since) {
            Ok(v) => v,
            Err(_) => return internal_error(),
        };
        let mut inner = String::new();
        for entry in &changes {
            let href = &entry.href;
            if entry.deleted {
                inner.push_str(&deleted_response_block(href));
            } else if let Ok(Some(row)) =
                db.get_calendar_event_by_href(account, &calendar_url, href)
            {
                let ics = row.raw_ics.unwrap_or_default();
                inner.push_str(&event_response_block(
                    href,
                    &row.etag.unwrap_or_default(),
                    &ics,
                ));
            }
        }
        inner.push_str(&format!("<D:sync-token>{}</D:sync-token>", new_token));
        multistatus(format!(
            "{}{}{}",
            MULTISTATUS_OPEN, inner, MULTISTATUS_CLOSE
        ))
    } else {
        // calendar-query (or anything else REPORT-shaped): full listing,
        // filters ignored — see module docs.
        let events = match db.server_list_events(account, &calendar_url) {
            Ok(v) => v,
            Err(_) => return internal_error(),
        };
        let mut inner = String::new();
        for row in &events {
            let ics = row.raw_ics.clone().unwrap_or_default();
            inner.push_str(&event_response_block(
                &row.href,
                &row.etag.clone().unwrap_or_default(),
                &ics,
            ));
        }
        multistatus(format!(
            "{}{}{}",
            MULTISTATUS_OPEN, inner, MULTISTATUS_CLOSE
        ))
    }
}

fn get_event(account: &str, db: &MailDb, req: &HttpRequest) -> Resp {
    let calendar_url = default_calendar_url(account);
    match db.get_calendar_event_by_href(account, &calendar_url, &req.path) {
        Ok(Some(row)) => (
            200,
            "OK",
            vec![
                (
                    "Content-Type".to_string(),
                    "text/calendar; charset=utf-8".to_string(),
                ),
                ("ETag".to_string(), row.etag.unwrap_or_default()),
            ],
            row.raw_ics.unwrap_or_default().into_bytes(),
        ),
        Ok(None) => not_found(),
        Err(_) => internal_error(),
    }
}

fn put_event(account: &str, db: &MailDb, req: &HttpRequest) -> Resp {
    let calendar_url = default_calendar_url(account);
    let ics = String::from_utf8_lossy(&req.body).into_owned();
    let if_match = req.headers.get("if-match").map(|s| s.trim().to_string());
    let if_none_match_star = req
        .headers
        .get("if-none-match")
        .map(|v| v.trim() == "*")
        .unwrap_or(false);

    match db.server_put_event(
        account,
        &calendar_url,
        &req.path,
        &ics,
        if_match.as_deref(),
        if_none_match_star,
    ) {
        Ok(Ok(PutOutcome::Created(etag))) => {
            (201, "Created", vec![("ETag".to_string(), etag)], Vec::new())
        }
        Ok(Ok(PutOutcome::Updated(etag))) => (
            204,
            "No Content",
            vec![("ETag".to_string(), etag)],
            Vec::new(),
        ),
        Ok(Err(PutError::PreconditionFailed(msg))) => {
            (412, "Precondition Failed", vec![], msg.into_bytes())
        }
        Ok(Err(PutError::InvalidBody(msg))) => (400, "Bad Request", vec![], msg.into_bytes()),
        Err(_) => internal_error(),
    }
}

fn delete_event(account: &str, db: &MailDb, req: &HttpRequest) -> Resp {
    let calendar_url = default_calendar_url(account);
    let if_match = req.headers.get("if-match").map(|s| s.trim().to_string());
    match db.server_delete_event(account, &calendar_url, &req.path, if_match.as_deref()) {
        Ok(Ok(())) => (204, "No Content", vec![], Vec::new()),
        Ok(Err(DeleteError::NotFound)) => not_found(),
        Ok(Err(DeleteError::PreconditionFailed(msg))) => {
            (412, "Precondition Failed", vec![], msg.into_bytes())
        }
        Err(_) => internal_error(),
    }
}

/// Real local integration tests: bind a real listener on `127.0.0.1:0`,
/// serve it on a background thread against a real temp-file database, and
/// drive it with this crate's own real [`crate::caldav::CalDavClient`] —
/// genuine end-to-end interop between this crate's client and server, over
/// a real TCP socket and real HTTP/1.1 framing, not a mocked transport.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::caldav::{CalDavClient, CalDavError, SyncOutcome};
    use crate::config::{AuthConfig, CalDavConfig, ImapConfig};

    fn test_imap_config() -> ImapConfig {
        ImapConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            login: "test@example.invalid".to_string(),
            auth: AuthConfig {
                auth_type: "password".to_string(),
                value: "unused".to_string(),
            },
        }
    }

    fn test_account(login: &str, password: &str) -> JamailAccount {
        JamailAccount {
            default: false,
            email: "test@example.invalid".to_string(),
            display_name: None,
            imap: test_imap_config(),
            smtp: None,
            folders: None,
            show_unlisted_folders: false,
            senders: None,
            sent_folder: None,
            draft_folder: None,
            notify_folders: None,
            color: None,
            caldav: Some(CalDavConfig {
                // Not dialed by the server role — this is the *client*
                // role's target, irrelevant here (see module docs: the
                // server reuses this account's login/auth for the
                // *server* role's Basic-auth check, not this URL).
                url: "http://unused.invalid/".to_string(),
                login: login.to_string(),
                auth: AuthConfig {
                    auth_type: "password".to_string(),
                    value: password.to_string(),
                },
                calendars: None,
                poll_interval_secs: 300,
                default_alarm_minutes_before: None,
            }),
        }
    }

    struct TestServer {
        base_url: String,
        db_path: PathBuf,
        shutdown: Arc<AtomicBool>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_file(&self.db_path);
            let _ = std::fs::remove_file(format!("{}-wal", self.db_path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.db_path.display()));
        }
    }

    fn start_test_server(label: &str, login: &str, password: &str) -> TestServer {
        let db_path = std::env::temp_dir().join(format!(
            "jamail-caldav-server-test-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut accounts = HashMap::new();
        accounts.insert("acct".to_string(), test_account(login, password));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (_handle, addr) = spawn(
            "127.0.0.1:0",
            db_path.clone(),
            Arc::new(accounts),
            Arc::clone(&shutdown),
        )
        .expect("spawn test CalDAV server");
        TestServer {
            base_url: format!("http://{}/dav/acct/", addr),
            db_path,
            shutdown,
        }
    }

    fn client_for(server: &TestServer, login: &str, password: &str) -> CalDavClient {
        CalDavClient::new(&server.base_url, login, password).unwrap()
    }

    #[test]
    fn discovery_finds_the_auto_created_default_calendar() {
        let server = start_test_server("discover", "alice", "secret");
        let client = client_for(&server, "alice", "secret");
        let calendars = client.discover_calendars().unwrap();
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].display_name, "Default");
        assert!(calendars[0].url.ends_with("/calendars/default/"));
    }

    #[test]
    fn wrong_credentials_are_rejected_with_401() {
        let server = start_test_server("badauth", "alice", "secret");
        let client = client_for(&server, "alice", "WRONG-PASSWORD");
        // The first two discovery steps (current-user-principal,
        // calendar-home-set) swallow failures via `.ok()` and fall back to
        // treating the base URL as the home set directly (see
        // `discover_calendars` docs), so it's the final "list calendars"
        // PROPFIND — which does *not* swallow its error — whose 401
        // surfaces, wrapped as `CalDavError::Other`.
        let err = client.discover_calendars().unwrap_err();
        assert!(matches!(err, CalDavError::Other(_)));
        assert!(err.to_string().contains("401"));
    }

    #[test]
    fn missing_credentials_are_rejected_with_401() {
        let server = start_test_server("noauth", "alice", "secret");
        // Bypass the client (which always sends Basic auth) to prove the
        // server itself demands credentials, not just that the client
        // supplies wrong ones.
        let url = httpc::HttpUrl::parse(&server.base_url).unwrap();
        let resp = httpc::send("PROPFIND", &url, &[], None, Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 401);
        assert_eq!(
            resp.header("www-authenticate"),
            Some("Basic realm=\"jamaild\"")
        );
    }

    #[test]
    fn unknown_account_in_path_is_404() {
        let server = start_test_server("unknownacct", "alice", "secret");
        let bad_url = server.base_url.replace("/dav/acct/", "/dav/nosuchaccount/");
        let url = httpc::HttpUrl::parse(&bad_url).unwrap();
        let resp = httpc::send("PROPFIND", &url, &[], None, Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn create_get_update_conflict_and_delete_round_trip() {
        let server = start_test_server("crud", "alice", "secret");
        let client = client_for(&server, "alice", "secret");
        let calendars = client.discover_calendars().unwrap();
        let cal_url = calendars[0].url.clone();

        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:e1\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let href = crate::caldav::new_event_href(&cal_url, "e1").unwrap();

        // Create.
        let etag1 = client.put_event(&href, ics, None, true).unwrap();
        assert!(!etag1.is_empty());

        // A second create at the same href must be rejected.
        let err = client.put_event(&href, ics, None, true).unwrap_err();
        assert!(matches!(err, CalDavError::Conflict(_)));

        // Get.
        let (etag_get, body) = client.get_event(&href).unwrap();
        assert_eq!(etag_get.as_deref(), Some(etag1.as_str()));
        assert!(body.contains("UID:e1"));
        assert!(body.contains("Standup"));

        // Update with the correct ETag.
        let ics2 = ics.replace("Standup", "Standup (moved)");
        let etag2 = client.put_event(&href, &ics2, Some(&etag1), false).unwrap();
        assert_ne!(etag1, etag2, "changing content must change the ETag");

        // A stale If-Match must now be rejected (proves real conflict
        // protection, not just "the server accepts anything").
        let err = client
            .put_event(&href, &ics2, Some(&etag1), false)
            .unwrap_err();
        assert!(matches!(err, CalDavError::Conflict(_)));

        // Delete requires the current ETag.
        let err = client.delete_event(&href, Some(&etag1)).unwrap_err();
        assert!(matches!(err, CalDavError::Conflict(_)));
        client.delete_event(&href, Some(&etag2)).unwrap();

        // Deleted resource reads back as 404, and re-deleting is idempotent.
        let err = client.get_event(&href).unwrap_err();
        assert!(matches!(err, CalDavError::NotFound));
        client.delete_event(&href, None).unwrap();
    }

    #[test]
    fn sync_collection_reports_create_then_delete_via_the_real_client() {
        let server = start_test_server("sync", "alice", "secret");
        let client = client_for(&server, "alice", "secret");
        let calendars = client.discover_calendars().unwrap();
        let cal_url = calendars[0].url.clone();

        let initial = client.sync_calendar(&cal_url, None).unwrap();
        let token0 = match initial {
            SyncOutcome::Delta {
                token,
                changed,
                deleted,
            } => {
                assert!(changed.is_empty());
                assert!(deleted.is_empty());
                token
            }
            SyncOutcome::FullResyncRequired => panic!("expected Delta on a fresh calendar"),
        };

        let ics = "BEGIN:VEVENT\r\nUID:s1\r\nDTSTART:20240115T090000Z\r\nEND:VEVENT\r\n";
        let href = crate::caldav::new_event_href(&cal_url, "s1").unwrap();
        client.put_event(&href, ics, None, true).unwrap();

        let after_create = client.sync_calendar(&cal_url, token0.as_deref()).unwrap();
        let token1 = match after_create {
            SyncOutcome::Delta {
                token,
                changed,
                deleted,
            } => {
                assert_eq!(changed.len(), 1);
                assert_eq!(changed[0].href, href);
                assert!(
                    changed[0]
                        .calendar_data
                        .as_ref()
                        .is_some_and(|d| d.contains("UID:s1"))
                );
                assert!(deleted.is_empty());
                token
            }
            SyncOutcome::FullResyncRequired => panic!("expected Delta after a create"),
        };

        client.delete_event(&href, None).unwrap();

        let after_delete = client.sync_calendar(&cal_url, token1.as_deref()).unwrap();
        match after_delete {
            SyncOutcome::Delta {
                changed, deleted, ..
            } => {
                assert!(changed.is_empty());
                assert_eq!(deleted, vec![href.clone()]);
            }
            SyncOutcome::FullResyncRequired => panic!("expected Delta after a delete"),
        }

        // And a client that never held a token (or lost it) can still
        // fall back to a full listing that reflects final state (empty).
        let full = client.list_all_events(&cal_url).unwrap();
        assert!(full.is_empty());
    }

    #[test]
    fn calendar_query_report_lists_every_event() {
        let server = start_test_server("query", "alice", "secret");
        let client = client_for(&server, "alice", "secret");
        let calendars = client.discover_calendars().unwrap();
        let cal_url = calendars[0].url.clone();

        for uid in ["q1", "q2", "q3"] {
            let ics = format!(
                "BEGIN:VEVENT\r\nUID:{}\r\nDTSTART:20240115T090000Z\r\nEND:VEVENT\r\n",
                uid
            );
            let href = crate::caldav::new_event_href(&cal_url, uid).unwrap();
            client.put_event(&href, &ics, None, true).unwrap();
        }

        let events = client.list_all_events(&cal_url).unwrap();
        assert_eq!(events.len(), 3);
        let uids: Vec<String> = events
            .iter()
            .map(|e| {
                crate::calendar::parse_vevents(e.calendar_data.as_ref().unwrap())
                    .unwrap()
                    .remove(0)
                    .uid
            })
            .collect();
        assert!(uids.contains(&"q1".to_string()));
        assert!(uids.contains(&"q2".to_string()));
        assert!(uids.contains(&"q3".to_string()));
    }

    #[test]
    fn malformed_ics_on_put_is_rejected_as_bad_request_not_stored() {
        let server = start_test_server("badbody", "alice", "secret");
        let url = httpc::HttpUrl::parse(&server.base_url).unwrap();
        let put_url = url.resolve("calendars/default/bad.ics").unwrap();
        let auth = (
            "Authorization".to_string(),
            httpc::basic_auth_header("alice", "secret"),
        );
        let resp = httpc::send(
            "PUT",
            &put_url,
            &[auth],
            Some(b"not an event"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(resp.status, 400);
    }
}
