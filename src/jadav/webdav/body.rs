//! Request bodies read as a stream (an upload never sits in memory) and
//! `Range` parsing for `GET`.
//!
//! `crate::httpc::read_chunked_body` collects the whole body into a
//! `Vec`, which is right for CalDAV's small documents and wrong for a file
//! server — macOS Finder uploads with `Transfer-Encoding: chunked` — so
//! [`BodyReader`] decodes both framings incrementally and enforces the
//! configured upload cap as it goes.

use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;

/// The framing a request declared for its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    None,
    Length(u64),
    Chunked,
}

impl BodyKind {
    /// `None` for a `Content-Length` that is not a number.
    pub fn from_headers(headers: &std::collections::HashMap<String, String>) -> Option<Self> {
        let chunked = headers
            .get("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);
        if chunked {
            return Some(Self::Chunked);
        }
        match headers.get("content-length") {
            Some(v) => match v.trim().parse::<u64>().ok()? {
                0 => Some(Self::None),
                n => Some(Self::Length(n)),
            },
            None => Some(Self::None),
        }
    }
}

/// The error payload a [`BodyReader`] uses when the body outgrows its cap,
/// so callers can tell "413" from a broken connection.
#[derive(Debug)]
pub struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body too large")
    }
}

impl std::error::Error for TooLarge {}

pub fn is_too_large(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<TooLarge>())
}

enum State {
    /// Bytes left in a `Content-Length` body.
    Length(u64),
    /// Bytes left in the current chunk; `None` before its size line.
    Chunk(Option<u64>),
    Done,
}

/// A request body as `Read`. Sends the deferred `100 Continue` on the first
/// read, so a request rejected before its body is touched never invites
/// the client to upload it.
pub struct BodyReader<'a, R: BufRead> {
    r: &'a mut R,
    state: State,
    limit: Option<u64>,
    total: u64,
    continue_to: Option<TcpStream>,
    started: bool,
}

impl<'a, R: BufRead> BodyReader<'a, R> {
    pub fn new(
        r: &'a mut R,
        kind: BodyKind,
        limit: Option<u64>,
        continue_to: Option<TcpStream>,
    ) -> Self {
        let state = match kind {
            BodyKind::None => State::Done,
            BodyKind::Length(n) => State::Length(n),
            BodyKind::Chunked => State::Chunk(None),
        };
        Self {
            r,
            state,
            limit,
            total: 0,
            continue_to,
            started: false,
        }
    }

    /// The whole body has been consumed (or there was none), so the
    /// connection can carry another request.
    pub fn finished(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// The client is still waiting for `100 Continue` and has sent nothing.
    pub fn untouched(&self) -> bool {
        !self.started
    }

    fn start(&mut self) -> io::Result<()> {
        if !self.started {
            self.started = true;
            if let Some(mut s) = self.continue_to.take() {
                s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
                s.flush()?;
            }
        }
        Ok(())
    }

    fn count(&mut self, n: usize) -> io::Result<()> {
        self.total += n as u64;
        if let Some(limit) = self.limit
            && self.total > limit
        {
            return Err(io::Error::other(TooLarge));
        }
        Ok(())
    }

    fn read_line(&mut self) -> io::Result<String> {
        let mut buf = Vec::new();
        // A size line is a few hex digits; refuse to buffer an endless one.
        let n = (&mut *self.r).take(1024).read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated chunked body",
            ));
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Read everything into memory, failing past `cap` bytes — for the XML
    /// bodies of `PROPFIND`/`PROPPATCH`/`LOCK`.
    pub fn read_small(&mut self, cap: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = self.read(&mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            if out.len() + n > cap {
                return Err(io::Error::other(TooLarge));
            }
            out.extend_from_slice(&buf[..n]);
        }
    }
}

impl<R: BufRead> Read for BodyReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.finished() {
            return Ok(0);
        }
        self.start()?;
        loop {
            match self.state {
                State::Done => return Ok(0),
                State::Length(0) => {
                    self.state = State::Done;
                    return Ok(0);
                }
                State::Length(left) => {
                    let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
                    let n = self.r.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "request body shorter than Content-Length",
                        ));
                    }
                    self.state = match left - n as u64 {
                        0 => State::Done,
                        rest => State::Length(rest),
                    };
                    self.count(n)?;
                    return Ok(n);
                }
                State::Chunk(None) => {
                    let line = self.read_line()?;
                    let hex = line.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(hex, 16).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size")
                    })?;
                    if size == 0 {
                        // Trailers (usually none) end with a blank line.
                        while !self.read_line()?.is_empty() {}
                        self.state = State::Done;
                        return Ok(0);
                    }
                    self.state = State::Chunk(Some(size));
                }
                State::Chunk(Some(left)) => {
                    let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
                    let n = self.r.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "truncated chunk",
                        ));
                    }
                    let left = left - n as u64;
                    if left == 0 {
                        if !self.read_line()?.is_empty() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "missing CRLF after chunk",
                            ));
                        }
                        self.state = State::Chunk(None);
                    } else {
                        self.state = State::Chunk(Some(left));
                    }
                    self.count(n)?;
                    return Ok(n);
                }
            }
        }
    }
}

/// What a `Range` header asks of a body of `len` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeRequest {
    /// No usable range: send everything with 200 (also for multi-range
    /// requests, which this server does not answer piecewise).
    Full,
    /// Inclusive byte offsets.
    Partial(u64, u64),
    Unsatisfiable,
}

pub fn parse_range(header: Option<&str>, len: u64) -> RangeRequest {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        return RangeRequest::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeRequest::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix: the last `b` bytes.
        let Ok(n) = b.parse::<u64>() else {
            return RangeRequest::Full;
        };
        if n == 0 || len == 0 {
            return RangeRequest::Unsatisfiable;
        }
        return RangeRequest::Partial(len.saturating_sub(n), len - 1);
    }
    let Ok(start) = a.parse::<u64>() else {
        return RangeRequest::Full;
    };
    if start >= len {
        return RangeRequest::Unsatisfiable;
    }
    let end = if b.is_empty() {
        len - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) if e >= start => e.min(len - 1),
            _ => return RangeRequest::Full,
        }
    };
    RangeRequest::Partial(start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_all(input: &[u8], kind: BodyKind, limit: Option<u64>) -> io::Result<Vec<u8>> {
        let mut cur = Cursor::new(input.to_vec());
        let mut body = BodyReader::new(&mut cur, kind, limit, None);
        let mut out = Vec::new();
        body.read_to_end(&mut out)?;
        assert!(body.finished());
        Ok(out)
    }

    #[test]
    fn decodes_chunked_with_extensions_and_trailers() {
        let input = b"5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: y\r\n\r\nNEXT";
        assert_eq!(
            read_all(input, BodyKind::Chunked, None).unwrap(),
            b"hello world"
        );
    }

    #[test]
    fn content_length_body_stops_at_its_length() {
        let mut cur = Cursor::new(b"abcdefNEXT".to_vec());
        let mut body = BodyReader::new(&mut cur, BodyKind::Length(6), None, None);
        let mut out = Vec::new();
        body.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abcdef");
        assert!(body.finished());
        let mut rest = String::new();
        cur.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "NEXT");
    }

    #[test]
    fn limit_is_enforced_for_both_framings() {
        let e = read_all(b"5\r\nhello\r\n0\r\n\r\n", BodyKind::Chunked, Some(4)).unwrap_err();
        assert!(is_too_large(&e));
        let e = read_all(b"hello", BodyKind::Length(5), Some(4)).unwrap_err();
        assert!(is_too_large(&e));
        assert_eq!(
            read_all(b"hello", BodyKind::Length(5), Some(5)).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn truncated_bodies_are_errors_not_short_files() {
        assert!(read_all(b"abc", BodyKind::Length(5), None).is_err());
        assert!(read_all(b"5\r\nhel", BodyKind::Chunked, None).is_err());
        assert!(read_all(b"zz\r\n", BodyKind::Chunked, None).is_err());
    }

    #[test]
    fn ranges() {
        use RangeRequest::*;
        assert_eq!(parse_range(None, 10), Full);
        assert_eq!(parse_range(Some("bytes=0-3"), 10), Partial(0, 3));
        assert_eq!(parse_range(Some("bytes=5-"), 10), Partial(5, 9));
        assert_eq!(parse_range(Some("bytes=-4"), 10), Partial(6, 9));
        assert_eq!(parse_range(Some("bytes=-40"), 10), Partial(0, 9));
        assert_eq!(parse_range(Some("bytes=8-100"), 10), Partial(8, 9));
        assert_eq!(parse_range(Some("bytes=10-"), 10), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-1,4-5"), 10), Full);
        assert_eq!(parse_range(Some("bytes=5-2"), 10), Full);
        assert_eq!(parse_range(Some("items=1-2"), 10), Full);
    }
}
