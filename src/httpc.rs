//! A minimal synchronous HTTP/1.1 client used only by [`crate::caldav`].
//!
//! CalDAV needs a small, specific slice of HTTP: arbitrary methods
//! (`PROPFIND`, `REPORT`, `MKCALENDAR`, alongside `GET`/`PUT`/`DELETE`), a
//! handful of headers (`Depth`, `If-Match`, `If-None-Match`,
//! `Content-Type`), request bodies, a few redirect hops, and response
//! bodies delimited by `Content-Length` or `Transfer-Encoding: chunked`.
//! Rather than take on an external HTTP client crate (and, with it, either
//! a second TLS stack alongside the `native-tls`/system-OpenSSL backend
//! already linked for IMAP/SMTP, or an unfamiliar API surface), this module
//! implements that slice directly against [`native_tls`] and
//! `std::net::TcpStream` — the same TLS backend already used elsewhere in
//! this crate, so no new system dependency and no new TLS stack.
//!
//! This is deliberately not a general-purpose HTTP client: no connection
//! pooling/keep-alive (`Connection: close` is always sent), no cookies, no
//! compression, no HTTP/2. CalDAV traffic is low-volume control/sync
//! traffic, so none of that matters here.
//!
//! Tests exercise the real request-building/response-parsing code (status
//! line, headers, chunked/content-length bodies, redirects) against a
//! local plain-`http://` mock server (see `tests` below) — the TLS wrapping
//! itself is not exercised by tests (same documented scope limitation as
//! `tests/smoke.rs` takes for IMAP: faking a trusted-CA TLS handshake is
//! disproportionate to what this module needs to prove, and `native_tls`
//! is an independently-tested crate).

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const MAX_REDIRECTS: u32 = 5;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// A parsed absolute HTTP(S) URL: just enough to drive requests and to
/// resolve `Location`/`href` values seen in responses. Not a general URL
/// parser (no query-string/userinfo handling beyond passing them through
/// verbatim as part of the path-and-query segment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Path plus optional `?query`, always starting with `/`.
    pub path_and_query: String,
}

impl HttpUrl {
    pub fn parse(s: &str) -> Result<Self> {
        let (scheme, rest) = s
            .split_once("://")
            .with_context(|| format!("URL missing scheme: {}", s))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            bail!("unsupported URL scheme (only http/https): {}", scheme);
        }
        let (authority, path_and_query) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            bail!("URL missing host: {}", s);
        }
        let (host, port) = match authority.rsplit_once(':') {
            // Guard against mistaking the last segment of an IPv6 literal
            // for a port; not otherwise supported (CalDAV servers are
            // configured by hostname in practice).
            Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                let port: u16 = p
                    .parse()
                    .with_context(|| format!("invalid port in URL: {}", s))?;
                (h.to_string(), port)
            }
            _ => {
                let default_port = if scheme == "https" { 443 } else { 80 };
                (authority.to_string(), default_port)
            }
        };
        let path_and_query = if path_and_query.is_empty() {
            "/".to_string()
        } else {
            path_and_query.to_string()
        };
        Ok(Self {
            scheme,
            host,
            port,
            path_and_query,
        })
    }

    pub fn to_absolute_string(&self) -> String {
        format!(
            "{}://{}{}{}",
            self.scheme,
            self.host,
            default_port_suffix(&self.scheme, self.port),
            self.path_and_query
        )
    }

    /// Resolve an `href`/`Location` value seen in a response against this
    /// URL: an absolute `http(s)://...` value is parsed as-is; a
    /// `/absolute/path` value keeps this URL's scheme/host/port; anything
    /// else (a bare relative path — rare for CalDAV, but seen from a few
    /// servers for `Location` on `MKCOL`/`PUT`) is resolved against this
    /// URL's own directory.
    pub fn resolve(&self, other: &str) -> Result<HttpUrl> {
        if other.starts_with("http://") || other.starts_with("https://") {
            return HttpUrl::parse(other);
        }
        if let Some(path) = other.strip_prefix('/') {
            return Ok(HttpUrl {
                scheme: self.scheme.clone(),
                host: self.host.clone(),
                port: self.port,
                path_and_query: format!("/{}", path),
            });
        }
        let dir = match self.path_and_query.rfind('/') {
            Some(idx) => &self.path_and_query[..=idx],
            None => "/",
        };
        Ok(HttpUrl {
            scheme: self.scheme.clone(),
            host: self.host.clone(),
            port: self.port,
            path_and_query: format!("{}{}", dir, other),
        })
    }

    fn host_header(&self) -> String {
        if (self.scheme == "https" && self.port == 443)
            || (self.scheme == "http" && self.port == 80)
        {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn default_port_suffix(scheme: &str, port: u16) -> String {
    let is_default = (scheme == "https" && port == 443) || (scheme == "http" && port == 80);
    if is_default {
        String::new()
    } else {
        format!(":{}", port)
    }
}

pub struct HttpResponse {
    pub status: u16,
    /// Header names are stored lower-cased for case-insensitive lookup.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }

    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Send `method` to `url` with `headers` and an optional `body`, following
/// up to [`MAX_REDIRECTS`] redirects (same method + body re-sent, which is
/// what CalDAV's `.well-known/caldav` redirect chains expect).
pub fn send(
    method: &str,
    url: &HttpUrl,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<HttpResponse> {
    let mut current = url.clone();
    for _ in 0..MAX_REDIRECTS {
        let resp = send_once(method, &current, headers, body, timeout)?;
        if matches!(resp.status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp.header("location")
        {
            current = current.resolve(loc)?;
            continue;
        }
        return Ok(resp);
    }
    bail!("too many redirects fetching {}", url.to_absolute_string())
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn send_once(
    method: &str,
    url: &HttpUrl,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<HttpResponse> {
    let addr = format!("{}:{}", url.host, url.port);
    let tcp = TcpStream::connect(&addr).with_context(|| format!("connecting to {}", addr))?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;

    let mut stream: Box<dyn ReadWrite> = if url.scheme == "https" {
        let connector = native_tls::TlsConnector::new().context("building TLS connector")?;
        Box::new(
            connector
                .connect(&url.host, tcp)
                .with_context(|| format!("TLS handshake with {}", url.host))?,
        )
    } else {
        Box::new(tcp)
    };

    let mut request = Vec::new();
    request.extend_from_slice(format!("{} {} HTTP/1.1\r\n", method, url.path_and_query).as_bytes());
    request.extend_from_slice(format!("Host: {}\r\n", url.host_header()).as_bytes());
    request.extend_from_slice(b"Connection: close\r\n");
    request.extend_from_slice(b"User-Agent: jamail-caldav/0.1\r\n");
    for (k, v) in headers {
        request.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    if let Some(b) = body {
        request.extend_from_slice(format!("Content-Length: {}\r\n", b.len()).as_bytes());
    }
    request.extend_from_slice(b"\r\n");
    if let Some(b) = body {
        request.extend_from_slice(b);
    }

    stream.write_all(&request).context("writing HTTP request")?;
    stream.flush().ok();

    let mut reader = BufReader::new(stream);
    let status = read_status_line(&mut reader)?;
    let resp_headers = read_headers(&mut reader)?;
    let resp_body = read_body(&mut reader, &resp_headers, method)?;

    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body: resp_body,
    })
}

pub(crate) fn read_line(r: &mut impl BufRead) -> Result<String> {
    let mut buf = Vec::new();
    let n = r.read_until(b'\n', &mut buf).context("reading line")?;
    if n == 0 {
        bail!("connection closed while reading a line");
    }
    while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read_status_line(r: &mut impl BufRead) -> Result<u16> {
    let line = read_line(r)?;
    let mut parts = line.splitn(3, ' ');
    let _version = parts.next().context("empty status line")?;
    let code = parts.next().context("missing status code")?;
    code.parse::<u16>()
        .with_context(|| format!("invalid status code in: {}", line))
}

pub(crate) fn read_headers(r: &mut impl BufRead) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::new();
    loop {
        let line = read_line(r)?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok(headers)
}

pub(crate) fn read_body(
    r: &mut impl BufRead,
    headers: &HashMap<String, String>,
    method: &str,
) -> Result<Vec<u8>> {
    if method.eq_ignore_ascii_case("HEAD") {
        return Ok(Vec::new());
    }
    let is_chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    if is_chunked {
        return read_chunked_body(r);
    }
    if let Some(len) = headers.get("content-length") {
        let len: usize = len
            .trim()
            .parse()
            .with_context(|| format!("invalid Content-Length: {}", len))?;
        if len > MAX_RESPONSE_BYTES {
            bail!("response body too large: {} bytes", len);
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).context("reading response body")?;
        return Ok(buf);
    }
    // No length given at all: read until the (Connection: close) peer
    // closes the socket.
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).context("reading response body")?;
    Ok(buf)
}

pub(crate) fn read_chunked_body(r: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let size_line = read_line(r)?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .with_context(|| format!("invalid chunk size: {}", size_line))?;
        if out.len() + size > MAX_RESPONSE_BYTES {
            bail!("chunked response body too large");
        }
        if size == 0 {
            // Trailing headers (usually none) end with a blank line.
            loop {
                let trailer = read_line(r)?;
                if trailer.is_empty() {
                    break;
                }
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        r.read_exact(&mut chunk).context("reading chunk body")?;
        out.extend_from_slice(&chunk);
        // Each chunk is followed by a bare CRLF.
        let crlf = read_line(r)?;
        if !crlf.is_empty() {
            bail!("malformed chunked encoding: expected CRLF after chunk");
        }
    }
    Ok(out)
}

/// Encode `login:password` as an HTTP Basic `Authorization` header value
/// (including the `Basic ` prefix).
pub fn basic_auth_header(login: &str, password: &str) -> String {
    use base64::Engine;
    let raw = format!("{}:{}", login, password);
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
    )
}

pub fn err_status(context: &str, status: u16, body: &str) -> anyhow::Error {
    let snippet: String = body.chars().take(300).collect();
    anyhow!("{}: HTTP {} — {}", context, status, snippet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// A minimal single-shot HTTP/1.1 mock server: accepts one connection,
    /// reads exactly one request (headers + Content-Length body, which is
    /// all this client ever sends), and writes back a canned response. Used
    /// to exercise the real request-building/response-parsing code above
    /// end-to-end without any external CalDAV server or TLS (see module
    /// doc's "Tests exercise..." note for why TLS itself isn't covered
    /// here).
    fn serve_once(response: &'static str) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let mut received = Vec::new();
            // Read whatever the client sent before it half-closes/finishes
            // writing; a short read timeout keeps this from hanging if the
            // client doesn't send a body.
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        received.extend_from_slice(&buf[..n]);
                        if received.len() >= 4 && received.windows(4).any(|w| w == b"\r\n\r\n") {
                            // Header/body boundary seen; for our fixed test
                            // payloads that's enough to proceed to writing
                            // the response (small bodies arrive in one read
                            // over loopback).
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            received
        });
        (format!("http://{}", addr), handle)
    }

    #[test]
    fn parses_absolute_https_url() {
        let u = HttpUrl::parse("https://cal.example.com/dav/calendars/user/").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "cal.example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path_and_query, "/dav/calendars/user/");
    }

    #[test]
    fn parses_explicit_port() {
        let u = HttpUrl::parse("http://localhost:5232/user/cal/").unwrap();
        assert_eq!(u.port, 5232);
        assert_eq!(u.to_absolute_string(), "http://localhost:5232/user/cal/");
    }

    #[test]
    fn rejects_unsupported_scheme() {
        assert!(HttpUrl::parse("ftp://example.com/").is_err());
    }

    #[test]
    fn resolve_handles_absolute_path_and_absolute_url() {
        let base = HttpUrl::parse("https://cal.example.com/dav/").unwrap();
        let abs_path = base.resolve("/other/place/").unwrap();
        assert_eq!(
            abs_path.to_absolute_string(),
            "https://cal.example.com/other/place/"
        );

        let abs_url = base.resolve("http://elsewhere.example.com/x").unwrap();
        assert_eq!(
            abs_url.to_absolute_string(),
            "http://elsewhere.example.com/x"
        );
    }

    #[test]
    fn round_trips_a_simple_get_with_content_length_body() {
        let (base, handle) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
        );
        let url = HttpUrl::parse(&base).unwrap();
        let resp = send("GET", &url, &[], None, Duration::from_secs(2)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_str(), "hello");
        assert_eq!(resp.header("content-type"), Some("text/plain"));
        let sent = handle.join().unwrap();
        let sent = String::from_utf8_lossy(&sent);
        assert!(sent.starts_with("GET / HTTP/1.1\r\n"));
    }

    #[test]
    fn sends_custom_method_and_headers_and_body() {
        let (base, handle) = serve_once("HTTP/1.1 207 Multi-Status\r\nContent-Length: 2\r\n\r\nok");
        let url = HttpUrl::parse(&base).unwrap();
        let resp = send(
            "PROPFIND",
            &url,
            &[
                ("Depth".to_string(), "0".to_string()),
                ("If-Match".to_string(), "\"abc\"".to_string()),
            ],
            Some(b"<propfind/>"),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(resp.status, 207);
        let sent = String::from_utf8_lossy(&handle.join().unwrap()).into_owned();
        assert!(sent.starts_with("PROPFIND / HTTP/1.1\r\n"));
        assert!(sent.contains("Depth: 0\r\n"));
        assert!(sent.contains("If-Match: \"abc\"\r\n"));
        assert!(sent.contains("Content-Length: 11\r\n"));
        assert!(sent.ends_with("<propfind/>"));
    }

    #[test]
    fn decodes_chunked_response_body() {
        let (base, _handle) = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        );
        let url = HttpUrl::parse(&base).unwrap();
        let resp = send("GET", &url, &[], None, Duration::from_secs(2)).unwrap();
        assert_eq!(resp.body_str(), "hello world");
    }

    #[test]
    fn reads_body_to_eof_when_no_length_is_given() {
        let (base, _handle) = serve_once("HTTP/1.1 200 OK\r\n\r\nno-length-body");
        let url = HttpUrl::parse(&base).unwrap();
        let resp = send("GET", &url, &[], None, Duration::from_secs(2)).unwrap();
        assert_eq!(resp.body_str(), "no-length-body");
    }

    #[test]
    fn follows_redirect_to_new_location() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let handle = thread::spawn(move || {
            // First connection: redirect.
            {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 301 Moved Permanently\r\nLocation: /new-place\r\nContent-Length: 0\r\n\r\n",
                );
            }
            // Second connection: final response.
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            req
        });
        let url = HttpUrl::parse(&base_url).unwrap();
        let resp = send("GET", &url, &[], None, Duration::from_secs(2)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_str(), "ok");
        let second_req = handle.join().unwrap();
        assert!(second_req.starts_with("GET /new-place HTTP/1.1\r\n"));
    }

    #[test]
    fn basic_auth_header_matches_known_vector() {
        // "Aladdin:open sesame" is the canonical RFC 7617 example.
        assert_eq!(
            basic_auth_header("Aladdin", "open sesame"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }
}
