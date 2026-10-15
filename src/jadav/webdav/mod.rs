//! An optional WebDAV (RFC 4918, classes 1 and 2) file server inside
//! `jadav`, on the same listener as CalDAV under `/webdav/`.
//!
//! Off unless `jadav.webdav` is configured: without it no URL is claimed
//! (so `/webdav/` falls through to the CalDAV router's 404) and nothing is
//! created on disk. With it, each configured user gets a private home and
//! the configured shares appear to their members:
//!
//! ```text
//! /webdav/                    virtual, read-only: your home + shared/
//! /webdav/<login>/…           <root>/users/<login>/…      only <login>
//! /webdav/shared/             virtual, read-only: shares you belong to
//! /webdav/shared/<name>/…     <root>/shared/<name>/…      read_write / read_only members
//! ```
//!
//! Files are a plain directory tree, never jadav's store: they can be big,
//! they should be readable and backed up on the host as they are, and
//! nothing about them needs a transaction. Uploads stream through
//! `<root>/.tmp/` and are renamed into place, so a reader never sees half
//! a file and a broken upload leaves nothing behind.
//!
//! The handler takes over the connection right after the request head
//! (see `server::handle_connection`) because the CalDAV path reads whole
//! bodies into memory; here `PUT` streams to disk ([`body::BodyReader`],
//! chunked included — Finder uploads that way), `GET` streams from disk
//! with `Range` support, and `100 Continue` is sent only once auth and
//! permissions have passed, so a refused upload is never transmitted.
//!
//! ## Auth
//!
//! HTTP Basic against per-user credentials resolved at startup and again
//! on `SIGHUP` (along with the users and shares themselves). The presented
//! login and password are re-encoded and compared as SHA-256 digests, so
//! the comparison is constant-time and length-independent, and an unknown
//! login costs the same as a wrong password.
//!
//! ## Deliberate limits
//!
//! - Locks are in memory ([`locks`]); a restart forgets them and stale
//!   tokens are then ignored rather than refused.
//! - No dead properties: `PROPPATCH` answers `403` per property, except the
//!   `Win32*` times/attributes Windows Explorer sets after every copy —
//!   `Win32LastModifiedTime` is applied as the file's mtime, the rest are
//!   accepted and dropped.
//! - `PROPFIND` with `Depth: infinity` is refused (`propfind-finite-depth`);
//!   a missing `Depth` is treated as `1`.
//! - No per-user quotas; `quota-available-bytes` reports the filesystem's
//!   free space.
//! - Partial failures of a recursive `COPY`/`MOVE`/`DELETE` answer `500`
//!   rather than a per-member multistatus.

pub mod body;
pub mod locks;
pub mod paths;

pub use paths::claims;

use crate::httpc;
use crate::jadav::config::{JadavConfig, WebdavShareConfig};
use crate::jadav::server::{REALM, RequestHead};
use crate::jadav::xml::{self, MultiStatus, NS_DAV, PropOp, PropfindRequest, QName, prop_element};
use anyhow::{Context, Result};
use body::{BodyKind, BodyReader, RangeRequest, is_too_large, parse_range};
use chrono::{DateTime, SecondsFormat, Utc};
use indexmap::IndexMap;
use locks::{LockManager, LockRequest, SUPPORTED_LOCK_XML, UnlockError};
use paths::{PathError, Space, SpaceKind, Target, find_symlink, parse_target};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime};

pub const DAV_HEADER: &str = "1, 2";
const ALLOW: &str =
    "OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, COPY, MOVE, PROPFIND, PROPPATCH, LOCK, UNLOCK";
/// XML request bodies (`PROPFIND`, `PROPPATCH`, `LOCK`) are tiny.
const MAX_XML_BODY: usize = 1024 * 1024;
/// A body this small is read and discarded when a request is refused
/// before using it, so the connection survives.
const MAX_DRAIN: u64 = 64 * 1024;
const NS_MS: &str = "urn:schemas-microsoft-com:";

// ---------------------------------------------------------------------
// Users, shares, state
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    None,
    Read,
    Write,
}

/// Who may log in and what each share grants — everything config decides,
/// rebuilt wholesale on `SIGHUP`.
pub struct Access {
    /// login → SHA-256 of the expected `Authorization` header.
    users: HashMap<String, [u8; 32]>,
    shares: IndexMap<String, HashMap<String, Level>>,
}

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

impl Access {
    pub fn new(users: &[(String, String)], shares: &IndexMap<String, WebdavShareConfig>) -> Self {
        let users = users
            .iter()
            .map(|(login, pw)| (login.clone(), digest(&httpc::basic_auth_header(login, pw))))
            .collect();
        let shares = shares
            .iter()
            .map(|(name, share)| {
                let mut members = HashMap::new();
                for m in &share.read_only {
                    members.insert(m.clone(), Level::Read);
                }
                // read_write wins over read_only.
                for m in &share.read_write {
                    members.insert(m.clone(), Level::Write);
                }
                (name.clone(), members)
            })
            .collect();
        Self { users, shares }
    }

    /// Resolve every user's secret (`auth: command` may shell out), or
    /// `None` when `jadav.webdav` is not configured.
    pub fn from_config(cfg: &JadavConfig) -> Result<Option<Self>> {
        let Some(wd) = &cfg.webdav else {
            return Ok(None);
        };
        let mut users = Vec::new();
        for login in wd.users.keys() {
            let auth = wd
                .user_auth(login, &cfg.principal)
                .with_context(|| format!("webdav user {} has no auth", login))?;
            let pw = auth
                .resolve_password()
                .with_context(|| format!("resolving jadav.webdav.users.{}.auth", login))?;
            users.push((login.clone(), pw));
        }
        Ok(Some(Self::new(&users, &wd.shares)))
    }

    /// The login a Basic `Authorization` header proves, if any.
    fn authenticate(&self, header: Option<&str>) -> Option<String> {
        use base64::Engine;
        let given = header?.trim();
        let (scheme, encoded) = given.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .ok()?;
        let decoded = String::from_utf8(decoded).ok()?;
        let (login, pw) = decoded.split_once(':')?;
        let presented = digest(&httpc::basic_auth_header(login, pw));
        let expected = self.users.get(login).copied().unwrap_or([0u8; 32]);
        let same = presented
            .iter()
            .zip(expected.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
        (same && self.users.contains_key(login)).then(|| login.to_string())
    }

    fn share_level(&self, share: &str, user: &str) -> Level {
        self.shares
            .get(share)
            .and_then(|m| m.get(user))
            .copied()
            .unwrap_or(Level::None)
    }

    fn level(&self, user: &str, target: &Target) -> Level {
        match target {
            Target::Root | Target::SharedRoot => Level::Read,
            Target::Item { space, .. } => match space.kind {
                SpaceKind::Home if space.name == user => Level::Write,
                SpaceKind::Home => Level::None,
                SpaceKind::Share => self.share_level(&space.name, user),
            },
        }
    }

    /// The shares `user` can see, in config order.
    fn visible_shares(&self, user: &str) -> Vec<String> {
        self.shares
            .keys()
            .filter(|name| self.share_level(name, user) != Level::None)
            .cloned()
            .collect()
    }
}

pub struct WebdavState {
    root: PathBuf,
    max_upload: Option<u64>,
    access: RwLock<Arc<Access>>,
    locks: LockManager,
}

fn mkdir_private(p: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(p)
}

impl WebdavState {
    /// Create every space directory and an empty `.tmp/` under `root`.
    pub fn open(root: &Path, max_upload: Option<u64>, access: Access) -> Result<Self> {
        let state = Self {
            root: root.to_path_buf(),
            max_upload,
            access: RwLock::new(Arc::new(Access {
                users: HashMap::new(),
                shares: IndexMap::new(),
            })),
            locks: LockManager::default(),
        };
        let tmp = state.tmp_dir();
        mkdir_private(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        // Uploads that were in flight when the last run stopped.
        for entry in fs::read_dir(&tmp)?.flatten() {
            let _ = fs::remove_file(entry.path());
        }
        state.reload(access)?;
        Ok(state)
    }

    /// `None` when `jadav.webdav` is not configured.
    pub fn from_config(cfg: &JadavConfig) -> Result<Option<Self>> {
        let (Some(wd), Some(access)) = (&cfg.webdav, Access::from_config(cfg)?) else {
            return Ok(None);
        };
        Self::open(&wd.root, wd.max_upload_bytes, access).map(Some)
    }

    /// Swap in new users/shares (creating directories for new ones).
    pub fn reload(&self, access: Access) -> Result<()> {
        for login in access.users.keys() {
            let dir = self.root.join("users").join(login);
            mkdir_private(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        for name in access.shares.keys() {
            let dir = self.root.join("shared").join(name);
            mkdir_private(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        *self.access.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(access);
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join(".tmp")
    }
}

// ---------------------------------------------------------------------
// Request / reply plumbing
// ---------------------------------------------------------------------

struct Req {
    method: String,
    raw_target: String,
    headers: HashMap<String, String>,
    keep_alive: bool,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

enum Payload {
    Bytes(Vec<u8>),
    File { file: File, len: u64 },
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    payload: Payload,
}

impl Reply {
    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            payload: Payload::Bytes(Vec::new()),
        }
    }
    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        let mut r = Self::empty(status).header("Content-Type", content_type);
        r.payload = Payload::Bytes(body);
        r
    }
    fn xml(status: u16, body: String) -> Self {
        Self::bytes(status, "application/xml; charset=utf-8", body.into_bytes())
    }
    fn dav_error(status: u16, condition_xml: &str) -> Self {
        Self::xml(status, xml::dav_error(condition_xml))
    }
    fn unauthorized() -> Self {
        Self::empty(401).header(
            "WWW-Authenticate",
            &format!("Basic realm=\"{}\", charset=\"UTF-8\"", REALM),
        )
    }
    fn internal(context: &str, e: &dyn std::fmt::Display) -> Self {
        eprintln!("jadav: webdav: {}: {}", context, e);
        Self::empty(500)
    }
    /// Map a filesystem error to the status a client can act on.
    fn io(context: &str, e: &io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => Self::empty(404),
            io::ErrorKind::PermissionDenied => Self::empty(403),
            io::ErrorKind::AlreadyExists => Self::empty(405),
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => Self::empty(507),
            _ => Self::internal(context, e),
        }
    }
}

fn write_reply(
    stream: &mut TcpStream,
    reply: Reply,
    keep_alive: bool,
    head_only: bool,
) -> io::Result<()> {
    let len = match &reply.payload {
        Payload::Bytes(b) => b.len() as u64,
        Payload::File { len, .. } => *len,
    };
    let mut out = format!("{}\r\n", xml::status_line(reply.status));
    for (k, v) in &reply.headers {
        out.push_str(&format!("{}: {}\r\n", k, v));
    }
    out.push_str(&format!("Content-Length: {}\r\n", len));
    out.push_str(if keep_alive {
        "Connection: keep-alive\r\n\r\n"
    } else {
        "Connection: close\r\n\r\n"
    });
    stream.write_all(out.as_bytes())?;
    if !head_only {
        match reply.payload {
            Payload::Bytes(b) => stream.write_all(&b)?,
            Payload::File { file, len } => {
                let copied = io::copy(&mut file.take(len), stream)?;
                if copied != len {
                    // The file shrank under us; the framing is now wrong,
                    // so the connection cannot be reused.
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "file shorter than announced",
                    ));
                }
            }
        }
    }
    stream.flush()
}

/// Serve one WebDAV request whose head `server::handle_connection` has
/// already read; returns whether the connection may carry another one.
pub(crate) fn handle<R: BufRead>(
    head: RequestHead,
    reader: &mut R,
    stream: &mut TcpStream,
    peer: SocketAddr,
    state: &WebdavState,
) -> io::Result<bool> {
    let started = Instant::now();
    let conn = head
        .headers
        .get("connection")
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or_default();
    let req = Req {
        keep_alive: !conn.contains("close") && (head.version_11 || conn.contains("keep-alive")),
        method: head.method,
        raw_target: head.raw_target,
        headers: head.headers,
    };
    let client = req
        .header("x-forwarded-for")
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| peer.ip().to_string());
    let Some(kind) = BodyKind::from_headers(&req.headers) else {
        write_reply(stream, Reply::empty(400), false, false)?;
        return Ok(false);
    };
    let expects_continue = req
        .header("expect")
        .is_some_and(|e| e.eq_ignore_ascii_case("100-continue"));
    let continue_to = if expects_continue && kind != BodyKind::None {
        stream.try_clone().ok()
    } else {
        None
    };
    let mut body = BodyReader::new(reader, kind, state.max_upload, continue_to);
    let (reply, user) = respond(&req, &mut body, kind, state);
    if !body.finished()
        && !expects_continue
        && matches!(kind, BodyKind::Length(n) if n <= MAX_DRAIN)
    {
        let _ = io::copy(&mut body, &mut io::sink());
    }
    let keep_alive = req.keep_alive && body.finished();
    eprintln!(
        "jadav: {} {} -> {} {}ms client={} user={}",
        req.method,
        req.raw_target,
        reply.status,
        started.elapsed().as_millis(),
        client,
        user.as_deref().unwrap_or("-")
    );
    write_reply(stream, reply, keep_alive, req.method == "HEAD")?;
    Ok(keep_alive)
}

fn respond<R: BufRead>(
    req: &Req,
    body: &mut BodyReader<'_, R>,
    kind: BodyKind,
    state: &WebdavState,
) -> (Reply, Option<String>) {
    if req.method == "OPTIONS" {
        let reply = Reply::empty(200)
            .header("DAV", DAV_HEADER)
            .header("MS-Author-Via", "DAV")
            .header("Allow", ALLOW);
        return (reply, None);
    }
    let access = Arc::clone(&state.access.read().unwrap_or_else(|p| p.into_inner()));
    let Some(user) = access.authenticate(req.header("authorization")) else {
        return (Reply::unauthorized(), None);
    };
    let target = match parse_target(&req.raw_target) {
        Ok(t) => t,
        Err(_) => return (Reply::empty(400), Some(user)),
    };
    let ctx = Ctx {
        state,
        access: &access,
        user: &user,
        req,
    };
    let result = match req.method.as_str() {
        "GET" | "HEAD" => ctx.get(&target),
        "PUT" => ctx.put(&target, body, kind),
        "DELETE" => ctx.delete(&target),
        "MKCOL" => ctx.mkcol(&target, kind),
        "COPY" => ctx.copy_move(&target, false),
        "MOVE" => ctx.copy_move(&target, true),
        "PROPFIND" => ctx.propfind(&target, body),
        "PROPPATCH" => ctx.proppatch(&target, body),
        "LOCK" => ctx.lock(&target, body),
        "UNLOCK" => ctx.unlock(&target),
        _ => Err(Reply::empty(405).header("Allow", ALLOW)),
    };
    (result.unwrap_or_else(|r| r), Some(user))
}

// ---------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------

/// A resolved request target: what it is, who may do what, and whether
/// (and as what) it exists on disk.
struct Node {
    target: Target,
    fs: Option<PathBuf>,
    /// `None`: does not exist. Always `None` for the virtual collections.
    meta: Option<Metadata>,
    level: Level,
}

impl Node {
    fn is_virtual(&self) -> bool {
        self.fs.is_none()
    }
    fn exists(&self) -> bool {
        self.is_virtual() || self.meta.is_some()
    }
    fn is_collection(&self) -> bool {
        self.is_virtual() || self.meta.as_ref().is_some_and(Metadata::is_dir)
    }
    fn fs(&self) -> &Path {
        self.fs.as_deref().unwrap_or(Path::new("/nonexistent"))
    }
    /// Below a space root: a path that can be created, deleted or moved.
    fn is_member(&self) -> bool {
        matches!(&self.target, Target::Item { rel, .. } if !rel.is_empty())
    }
}

/// A strong-enough validator: mtime in nanoseconds plus size.
fn etag_of(meta: &Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", mtime, meta.len())
}

fn http_date(t: SystemTime) -> String {
    DateTime::<Utc>::from(t)
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
}

/// Directory entry names a URL can address (and so may be listed).
fn addressable(name: &str) -> bool {
    name != "."
        && name != ".."
        && !name.contains(['\\', '\0'])
        && !name.chars().any(char::is_control)
}

/// Copy a file or a whole tree (`deep: false` copies a collection without
/// its members), never following symlinks.
fn copy_tree(src: &Path, dst: &Path, deep: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir(dst)?;
        if deep {
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                if entry.file_type()?.is_symlink() {
                    continue;
                }
                copy_tree(&entry.path(), &dst.join(entry.file_name()), true)?;
            }
        }
    } else if meta.is_file() {
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn remove_any(path: &Path, meta: &Metadata) -> io::Result<()> {
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// `rename`, falling back to copy + delete across filesystems (a space
/// directory may be a separate mount).
fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_tree(src, dst, true)?;
            let meta = fs::symlink_metadata(src)?;
            remove_any(src, &meta)
        }
        other => other,
    }
}

struct Ctx<'a> {
    state: &'a WebdavState,
    access: &'a Access,
    user: &'a str,
    req: &'a Req,
}

type Handled = Result<Reply, Reply>;

impl Ctx<'_> {
    /// Resolve `target` for this user: `Err` when they may not see it at
    /// all (403 for someone else's home, 404 for a share they are not in)
    /// or when the path runs through a symlink.
    fn node(&self, target: &Target) -> Result<Node, Reply> {
        let level = self.access.level(self.user, target);
        if level == Level::None {
            return Err(match target {
                Target::Item { space, .. }
                    if space.kind == SpaceKind::Home
                        && self.access.users.contains_key(&space.name) =>
                {
                    Reply::empty(403)
                }
                _ => Reply::empty(404),
            });
        }
        let fs_path = target.fs_path(&self.state.root);
        let meta = match (&fs_path, target) {
            (Some(p), Target::Item { space, rel }) => {
                if find_symlink(&space.dir(&self.state.root), rel) {
                    return Err(Reply::empty(404));
                }
                fs::symlink_metadata(p)
                    .ok()
                    .filter(|m| m.is_file() || m.is_dir())
            }
            _ => None,
        };
        Ok(Node {
            target: target.clone(),
            fs: fs_path,
            meta,
            level,
        })
    }

    fn need_write(&self, node: &Node) -> Result<(), Reply> {
        if node.level == Level::Write && !node.is_virtual() {
            Ok(())
        } else {
            Err(Reply::empty(403))
        }
    }

    fn tokens(&self) -> Vec<String> {
        locks::tokens_in_if(self.req.header("if"))
    }

    fn check_locks(&self, target: &Target, recursive: bool, membership: bool) -> Result<(), Reply> {
        self.state
            .locks
            .check_write(
                &target.logical(),
                recursive,
                membership,
                &self.tokens(),
                self.user,
            )
            .map_err(|hrefs| {
                let inner: String = hrefs
                    .iter()
                    .map(|h| format!("<D:href>{}</D:href>", xml::escape_text(h)))
                    .collect();
                Reply::dav_error(
                    423,
                    &format!("<D:lock-token-submitted>{}</D:lock-token-submitted>", inner),
                )
            })
    }

    /// The parent of a member must be an existing collection (409 when not).
    fn parent_dir(&self, node: &Node) -> Result<(), Reply> {
        let parent = node.target.parent().ok_or_else(|| Reply::empty(403))?;
        let p = self.node(&parent).map_err(|_| Reply::empty(409))?;
        if p.is_collection() && p.exists() {
            Ok(())
        } else {
            Err(Reply::empty(409))
        }
    }

    fn children(&self, node: &Node) -> Vec<Node> {
        let targets: Vec<Target> = match &node.target {
            Target::Root => vec![
                Target::Item {
                    space: Space {
                        kind: SpaceKind::Home,
                        name: self.user.to_string(),
                    },
                    rel: Vec::new(),
                },
                Target::SharedRoot,
            ],
            Target::SharedRoot => self
                .access
                .visible_shares(self.user)
                .iter()
                .filter_map(|name| node.target.child(name))
                .collect(),
            Target::Item { .. } => {
                let Ok(entries) = fs::read_dir(node.fs()) else {
                    return Vec::new();
                };
                let mut names: Vec<String> = entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| !t.is_symlink()))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| addressable(n))
                    .collect();
                names.sort();
                names.iter().filter_map(|n| node.target.child(n)).collect()
            }
        };
        targets
            .iter()
            .filter_map(|t| self.node(t).ok())
            .filter(Node::exists)
            .collect()
    }

    // -- GET / HEAD ----------------------------------------------------

    fn get(&self, target: &Target) -> Handled {
        let node = self.node(target)?;
        if !node.exists() {
            return Err(Reply::empty(404));
        }
        if node.is_collection() {
            return Ok(self.listing(&node));
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(node.fs())
            .map_err(|e| Reply::io("opening file", &e))?;
        let meta = file.metadata().map_err(|e| Reply::io("stat", &e))?;
        let etag = etag_of(&meta);
        let len = meta.len();
        let mut headers = vec![
            ("ETag".to_string(), etag.clone()),
            ("Accept-Ranges".to_string(), "bytes".to_string()),
        ];
        if let Ok(m) = meta.modified() {
            headers.push(("Last-Modified".to_string(), http_date(m)));
        }
        if let Some(inm) = self.req.header("if-none-match")
            && inm.split(',').any(|t| t.trim() == "*" || t.trim() == etag)
        {
            return Ok(Reply {
                status: 304,
                headers,
                payload: Payload::Bytes(Vec::new()),
            });
        }
        headers.push((
            "Content-Type".to_string(),
            mime_guess::from_path(node.fs())
                .first_or_octet_stream()
                .to_string(),
        ));
        // An If-Range that no longer matches means "send it all".
        let range_header = match self.req.header("if-range") {
            Some(v) if v.trim() != etag => None,
            _ => self.req.header("range"),
        };
        let (status, start, count) = match parse_range(range_header, len) {
            RangeRequest::Full => (200, 0, len),
            RangeRequest::Partial(a, b) => {
                headers.push((
                    "Content-Range".to_string(),
                    format!("bytes {}-{}/{}", a, b, len),
                ));
                (206, a, b - a + 1)
            }
            RangeRequest::Unsatisfiable => {
                return Ok(Reply::empty(416).header("Content-Range", &format!("bytes */{}", len)));
            }
        };
        let mut file = file;
        if start > 0 {
            file.seek(SeekFrom::Start(start))
                .map_err(|e| Reply::io("seek", &e))?;
        }
        Ok(Reply {
            status,
            headers,
            payload: Payload::File { file, len: count },
        })
    }

    /// A minimal HTML index, so a browser pointed at a space shows it.
    fn listing(&self, node: &Node) -> Reply {
        let title = xml::escape_text(&format!("/webdav/{}", node.target.logical().join("/")));
        let mut html = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{0}</title><h1>{0}</h1><ul>",
            title
        );
        if node.target != Target::Root {
            html.push_str("<li><a href=\"../\">../</a></li>");
        }
        for child in self.children(node) {
            let coll = child.is_collection();
            let name = child.target.name();
            html.push_str(&format!(
                "<li><a href=\"{}\">{}{}</a></li>",
                xml::escape_attr(&child.target.href(coll)),
                xml::escape_text(&name),
                if coll { "/" } else { "" }
            ));
        }
        html.push_str("</ul>\n");
        Reply::bytes(200, "text/html; charset=utf-8", html.into_bytes())
    }

    // -- PUT -----------------------------------------------------------

    fn put<R: BufRead>(
        &self,
        target: &Target,
        body: &mut BodyReader<'_, R>,
        kind: BodyKind,
    ) -> Handled {
        let node = self.node(target)?;
        if !node.is_member() || node.is_collection() && node.exists() {
            return Err(Reply::empty(405).header("Allow", ALLOW));
        }
        self.need_write(&node)?;
        self.parent_dir(&node)?;
        let existed = node.exists();
        let current = node.meta.as_ref().map(etag_of);
        if let Some(inm) = self.req.header("if-none-match")
            && existed
            && inm
                .split(',')
                .any(|t| t.trim() == "*" || Some(t.trim()) == current.as_deref())
        {
            return Err(Reply::empty(412));
        }
        if let Some(im) = self.req.header("if-match")
            && !im
                .split(',')
                .any(|t| (t.trim() == "*" && existed) || Some(t.trim()) == current.as_deref())
        {
            return Err(Reply::empty(412));
        }
        self.check_locks(target, false, !existed)?;
        if let (BodyKind::Length(n), Some(limit)) = (kind, self.state.max_upload)
            && n > limit
        {
            return Err(Reply::empty(413));
        }

        let tmp = self
            .state
            .tmp_dir()
            .join(format!("put-{}", locks::random_hex(12)));
        let written = (|| -> io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            io::copy(body, &mut f)?;
            f.sync_all()
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(if is_too_large(&e) {
                Reply::empty(413)
            } else {
                eprintln!(
                    "jadav: webdav: upload of {} failed: {}",
                    self.req.raw_target, e
                );
                Reply::empty(400)
            });
        }
        if let Err(e) = move_path(&tmp, node.fs()) {
            let _ = fs::remove_file(&tmp);
            return Err(Reply::io("storing upload", &e));
        }
        let mut reply = Reply::empty(if existed { 204 } else { 201 });
        if let Ok(meta) = fs::symlink_metadata(node.fs()) {
            reply = reply.header("ETag", &etag_of(&meta));
        }
        Ok(reply)
    }

    // -- DELETE / MKCOL ------------------------------------------------

    fn delete(&self, target: &Target) -> Handled {
        let node = self.node(target)?;
        if !node.exists() {
            return Err(Reply::empty(404));
        }
        if !node.is_member() {
            return Err(Reply::empty(403));
        }
        self.need_write(&node)?;
        self.check_locks(target, true, true)?;
        let meta = node.meta.as_ref().expect("exists");
        remove_any(node.fs(), meta).map_err(|e| Reply::io("deleting", &e))?;
        self.state.locks.remove_under(&target.logical());
        Ok(Reply::empty(204))
    }

    fn mkcol(&self, target: &Target, kind: BodyKind) -> Handled {
        if kind != BodyKind::None {
            return Err(Reply::empty(415));
        }
        let node = self.node(target)?;
        if node.exists() {
            return Err(Reply::empty(405).header("Allow", ALLOW));
        }
        if !node.is_member() {
            return Err(Reply::empty(403));
        }
        self.need_write(&node)?;
        self.parent_dir(&node)?;
        self.check_locks(target, false, true)?;
        fs::create_dir(node.fs()).map_err(|e| Reply::io("creating collection", &e))?;
        Ok(Reply::empty(201))
    }

    // -- COPY / MOVE ---------------------------------------------------

    fn copy_move(&self, target: &Target, is_move: bool) -> Handled {
        let dest_raw = self
            .req
            .header("destination")
            .ok_or_else(|| Reply::empty(400))?;
        let dest = match parse_target(dest_raw) {
            Ok(t) => t,
            Err(PathError::Foreign) => return Err(Reply::empty(502)),
            Err(PathError::Invalid) => return Err(Reply::empty(400)),
        };
        let src = self.node(target)?;
        if !src.exists() {
            return Err(Reply::empty(404));
        }
        if src.is_virtual() || (is_move && !src.is_member()) {
            return Err(Reply::empty(403));
        }
        if is_move {
            self.need_write(&src)?;
        }
        let dst = self.node(&dest)?;
        if !dst.is_member() {
            return Err(Reply::empty(403));
        }
        self.need_write(&dst)?;
        let (src_l, dst_l) = (target.logical(), dest.logical());
        if dst_l.starts_with(&src_l) {
            // Onto itself, or a collection into its own subtree.
            return Err(Reply::empty(403));
        }
        self.parent_dir(&dst)?;
        let overwrite = !self
            .req
            .header("overwrite")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("f"));
        let existed = dst.exists();
        if existed && !overwrite {
            return Err(Reply::empty(412));
        }
        if is_move {
            self.check_locks(target, true, true)?;
        }
        self.check_locks(&dest, true, true)?;
        if let Some(meta) = &dst.meta {
            remove_any(dst.fs(), meta).map_err(|e| Reply::io("replacing destination", &e))?;
            self.state.locks.remove_under(&dst_l);
        }
        if is_move {
            move_path(src.fs(), dst.fs()).map_err(|e| Reply::io("moving", &e))?;
            self.state.locks.remove_under(&src_l);
        } else {
            let deep = !self.req.header("depth").is_some_and(|d| d.trim() == "0");
            copy_tree(src.fs(), dst.fs(), deep).map_err(|e| Reply::io("copying", &e))?;
        }
        Ok(Reply::empty(if existed { 204 } else { 201 }))
    }

    // -- PROPFIND ------------------------------------------------------

    fn propfind<R: BufRead>(&self, target: &Target, body: &mut BodyReader<'_, R>) -> Handled {
        let node = self.node(target)?;
        if !node.exists() {
            return Err(Reply::empty(404));
        }
        let depth_one = match self
            .req
            .header("depth")
            .map(|d| d.trim().to_ascii_lowercase())
        {
            Some(d) if d == "0" => false,
            Some(d) if d == "infinity" => {
                return Err(Reply::dav_error(403, "<D:propfind-finite-depth/>"));
            }
            _ => true,
        };
        let raw = read_xml(body)?;
        let request = xml::parse_propfind(&raw).map_err(|_| Reply::empty(400))?;
        let mut ms = MultiStatus::new();
        self.render_props(&mut ms, &node, &request);
        if depth_one && node.is_collection() {
            for child in self.children(&node) {
                self.render_props(&mut ms, &child, &request);
            }
        }
        Ok(Reply::xml(207, ms.finish()))
    }

    fn render_props(&self, ms: &mut MultiStatus, node: &Node, request: &PropfindRequest) {
        let href = node.target.href(node.is_collection());
        let mut found = String::new();
        let mut missing = String::new();
        match request {
            PropfindRequest::Prop(names) => {
                for name in names {
                    match self.live_prop(node, name) {
                        Some(inner) => found.push_str(&prop_element(name, &inner)),
                        None => missing.push_str(&prop_element(name, "")),
                    }
                }
            }
            PropfindRequest::AllProp => {
                for local in ALLPROP {
                    let name = QName::dav(local);
                    if let Some(inner) = self.live_prop(node, &name) {
                        found.push_str(&prop_element(&name, &inner));
                    }
                }
            }
            PropfindRequest::PropName => {
                for local in ALLPROP.iter().chain(&["quota-available-bytes"]) {
                    let name = QName::dav(local);
                    if self.live_prop(node, &name).is_some() {
                        found.push_str(&prop_element(&name, ""));
                    }
                }
            }
        }
        ms.response(&href)
            .propstat(200, &found)
            .propstat(404, &missing)
            .end();
    }

    /// The rendered content of one live property, or `None` when this
    /// resource does not have it.
    fn live_prop(&self, node: &Node, name: &QName) -> Option<String> {
        if name.ns != NS_DAV {
            return None;
        }
        let file_meta = node.meta.as_ref().filter(|m| m.is_file());
        match name.local.as_str() {
            "resourcetype" => Some(if node.is_collection() {
                "<D:collection/>".to_string()
            } else {
                String::new()
            }),
            "displayname" => {
                let n = node.target.name();
                Some(xml::escape_text(if n.is_empty() { "webdav" } else { &n }))
            }
            "getcontentlength" => file_meta.map(|m| m.len().to_string()),
            "getcontenttype" => file_meta.map(|_| {
                mime_guess::from_path(node.fs())
                    .first_or_octet_stream()
                    .to_string()
            }),
            "getetag" => file_meta.map(|m| xml::escape_text(&etag_of(m))),
            "getlastmodified" => node
                .meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(http_date),
            "creationdate" => node
                .meta
                .as_ref()
                .and_then(|m| m.created().or_else(|_| m.modified()).ok())
                .map(|t| DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Secs, true)),
            "supportedlock" if !node.is_virtual() => Some(SUPPORTED_LOCK_XML.to_string()),
            "lockdiscovery" if !node.is_virtual() => Some(
                self.state
                    .locks
                    .covering(&node.target.logical())
                    .iter()
                    .map(locks::Lock::activelock_xml)
                    .collect(),
            ),
            "quota-available-bytes" if !node.is_virtual() => {
                available_bytes(node.fs()).map(|n| n.to_string())
            }
            _ => None,
        }
    }

    // -- PROPPATCH -----------------------------------------------------

    fn proppatch<R: BufRead>(&self, target: &Target, body: &mut BodyReader<'_, R>) -> Handled {
        let node = self.node(target)?;
        if !node.exists() {
            return Err(Reply::empty(404));
        }
        self.need_write(&node)?;
        self.check_locks(target, false, false)?;
        let raw = read_xml(body)?;
        let ops = xml::parse_proppatch(&raw).map_err(|_| Reply::empty(400))?;
        let mut mtime: Option<SystemTime> = None;
        let mut results: Vec<(QName, u16, Option<&str>)> = Vec::new();
        for op in &ops {
            let (name, value) = match op {
                PropOp::Set(n, v) => (n, Some(v)),
                PropOp::Remove(n) => (n, None),
            };
            let result = if name.ns == NS_MS {
                match (name.local.as_str(), value) {
                    ("Win32LastModifiedTime", Some(v)) => {
                        match DateTime::parse_from_rfc2822(v.text.trim()) {
                            Ok(t) => {
                                mtime = Some(t.with_timezone(&Utc).into());
                                (200, None)
                            }
                            Err(_) => (409, None),
                        }
                    }
                    (
                        "Win32LastModifiedTime"
                        | "Win32CreationTime"
                        | "Win32LastAccessTime"
                        | "Win32FileAttributes",
                        _,
                    ) => (200, None),
                    _ => (403, None),
                }
            } else if name.ns == NS_DAV {
                (403, Some("<D:cannot-modify-protected-property/>"))
            } else {
                (403, None)
            };
            results.push((name.clone(), result.0, result.1));
        }
        let failed = results.iter().any(|(_, s, _)| *s != 200);
        if failed {
            for r in results.iter_mut().filter(|r| r.1 == 200) {
                r.1 = 424;
            }
        } else if let Some(t) = mtime {
            let applied = File::open(node.fs()).and_then(|f| f.set_modified(t));
            if let Err(e) = applied {
                return Err(Reply::io("setting mtime", &e));
            }
        }
        let mut ms = MultiStatus::new();
        let mut rb = ms.response(&node.target.href(node.is_collection()));
        for status in [200u16, 403, 409, 424] {
            let group: Vec<_> = results.iter().filter(|r| r.1 == status).collect();
            if group.is_empty() {
                continue;
            }
            let inner: String = group.iter().map(|r| prop_element(&r.0, "")).collect();
            let error = group.iter().find_map(|r| r.2);
            rb = rb.propstat_with_error(status, &inner, error);
        }
        rb.end();
        Ok(Reply::xml(207, ms.finish()))
    }

    // -- LOCK / UNLOCK -------------------------------------------------

    fn lock<R: BufRead>(&self, target: &Target, body: &mut BodyReader<'_, R>) -> Handled {
        let node = self.node(target)?;
        self.need_write(&node)?;
        let timeout = locks::parse_timeout(self.req.header("timeout"));
        let raw = read_xml(body)?;
        let logical = target.logical();
        if raw.iter().all(u8::is_ascii_whitespace) {
            // Refresh of a lock whose token is in `If:`.
            let lock = self
                .state
                .locks
                .refresh(&logical, &self.tokens(), self.user, timeout)
                .ok_or_else(|| Reply::dav_error(412, "<D:lock-token-matches-request-uri/>"))?;
            return Ok(lock_reply(200, &lock));
        }
        let info = xml::parse_element_tree(&raw).map_err(|_| Reply::empty(400))?;
        if !info.name.is(NS_DAV, "lockinfo") {
            return Err(Reply::empty(400));
        }
        let exclusive = info
            .child(NS_DAV, "lockscope")
            .is_none_or(|s| s.child(NS_DAV, "shared").is_none());
        let owner_xml = info
            .child(NS_DAV, "owner")
            .map(|o| o.inner_xml())
            .unwrap_or_default();
        let infinite =
            node.is_collection() && !self.req.header("depth").is_some_and(|d| d.trim() == "0");

        // Locking an unmapped URL creates an empty resource (RFC 4918 §7.3),
        // which is how Finder and Explorer start every new file.
        let mut created = false;
        if !node.exists() {
            if !node.is_member() {
                return Err(Reply::empty(403));
            }
            self.parent_dir(&node)?;
            self.check_locks(target, false, true)?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(node.fs())
                .map_err(|e| Reply::io("creating locked resource", &e))?;
            created = true;
        }
        let href = node.target.href(node.is_collection());
        let lock = self.state.locks.lock(LockRequest {
            path: &logical,
            root_href: &href,
            exclusive,
            infinite,
            owner_xml: &owner_xml,
            user: self.user,
            timeout_secs: timeout,
        });
        match lock {
            Ok(lock) => Ok(lock_reply(if created { 201 } else { 200 }, &lock)),
            Err(hrefs) => {
                if created {
                    let _ = fs::remove_file(node.fs());
                }
                let inner: String = hrefs
                    .iter()
                    .map(|h| format!("<D:href>{}</D:href>", xml::escape_text(h)))
                    .collect();
                Err(Reply::dav_error(
                    423,
                    &format!("<D:no-conflicting-lock>{}</D:no-conflicting-lock>", inner),
                ))
            }
        }
    }

    fn unlock(&self, target: &Target) -> Handled {
        self.node(target)?;
        let token = self
            .req
            .header("lock-token")
            .map(|t| {
                t.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .trim()
            })
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Reply::empty(400))?;
        match self.state.locks.unlock(&target.logical(), token, self.user) {
            Ok(()) => Ok(Reply::empty(204)),
            Err(UnlockError::NoSuchLock) => {
                Err(Reply::dav_error(409, "<D:lock-token-matches-request-uri/>"))
            }
            Err(UnlockError::NotOwner) => Err(Reply::empty(403)),
        }
    }
}

const ALLPROP: [&str; 9] = [
    "resourcetype",
    "displayname",
    "getcontentlength",
    "getcontenttype",
    "getetag",
    "getlastmodified",
    "creationdate",
    "supportedlock",
    "lockdiscovery",
];

fn read_xml<R: BufRead>(body: &mut BodyReader<'_, R>) -> Result<Vec<u8>, Reply> {
    body.read_small(MAX_XML_BODY).map_err(|e| {
        if is_too_large(&e) {
            Reply::empty(413)
        } else {
            Reply::empty(400)
        }
    })
}

fn lock_reply(status: u16, lock: &locks::Lock) -> Reply {
    Reply::xml(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:prop xmlns:D=\"DAV:\"><D:lockdiscovery>{}</D:lockdiscovery></D:prop>",
            lock.activelock_xml()
        ),
    )
    .header("Lock-Token", &format!("<{}>", lock.token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpc::{HttpResponse, HttpUrl};
    use crate::jadav::server::{ServerState, spawn};
    use std::io::BufReader;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    struct TestDav {
        base: String,
        root: PathBuf,
        state: Arc<ServerState>,
    }

    impl Drop for TestDav {
        fn drop(&mut self) {
            self.state.shutdown.store(true, Ordering::Relaxed);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const ALICE: (&str, &str) = ("alice", "pa");
    const BOB: (&str, &str) = ("bob", "pb");
    const CAROL: (&str, &str) = ("carol", "pc");

    fn start(label: &str, max_upload: Option<u64>) -> TestDav {
        let root = std::env::temp_dir().join(format!(
            "jadav-webdav-test-{}-{}-{}",
            label,
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut shares = IndexMap::new();
        shares.insert(
            "fam".to_string(),
            WebdavShareConfig {
                read_write: vec!["alice".into(), "bob".into()],
                read_only: vec!["carol".into()],
            },
        );
        shares.insert(
            "bobs".to_string(),
            WebdavShareConfig {
                read_write: vec!["bob".into()],
                read_only: vec![],
            },
        );
        let users: Vec<(String, String)> = [ALICE, BOB, CAROL]
            .iter()
            .map(|(l, p)| (l.to_string(), p.to_string()))
            .collect();
        let wd = WebdavState::open(&root, max_upload, Access::new(&users, &shares)).unwrap();
        let mut state = ServerState::new(
            root.join("jadav.db"),
            "cal@example.com",
            "calpw",
            vec!["cal@example.com".to_string()],
        );
        state.webdav = Some(Arc::new(wd));
        let state = Arc::new(state);
        let (_h, addr) = spawn("127.0.0.1:0", Arc::clone(&state)).unwrap();
        TestDav {
            base: format!("http://{}", addr),
            root,
            state,
        }
    }

    fn send(
        t: &TestDav,
        user: Option<(&str, &str)>,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> HttpResponse {
        let url = HttpUrl::parse(&format!("{}{}", t.base, path)).unwrap();
        let mut headers: Vec<(String, String)> = extra
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some((l, p)) = user {
            headers.push(("Authorization".into(), httpc::basic_auth_header(l, p)));
        }
        httpc::send(method, &url, &headers, body, Duration::from_secs(5)).unwrap()
    }

    fn put(t: &TestDav, user: (&str, &str), path: &str, body: &[u8]) -> u16 {
        send(t, Some(user), "PUT", path, &[], Some(body)).status
    }

    fn get(t: &TestDav, user: (&str, &str), path: &str) -> HttpResponse {
        send(t, Some(user), "GET", path, &[], None)
    }

    fn propfind(t: &TestDav, user: (&str, &str), path: &str, depth: &str) -> HttpResponse {
        send(t, Some(user), "PROPFIND", path, &[("Depth", depth)], None)
    }

    #[test]
    fn auth_is_required_and_checked_per_user() {
        let t = start("auth", None);
        let r = send(&t, None, "PROPFIND", "/webdav/", &[], None);
        assert_eq!(r.status, 401);
        assert!(r.header("www-authenticate").unwrap().contains("Basic"));
        assert_eq!(
            propfind(&t, ("alice", "wrong"), "/webdav/", "0").status,
            401
        );
        assert_eq!(propfind(&t, ("mallory", "pa"), "/webdav/", "0").status, 401);
        // The CalDAV principal is not a WebDAV user here.
        assert_eq!(
            propfind(&t, ("cal@example.com", "calpw"), "/webdav/", "0").status,
            401
        );
        assert_eq!(propfind(&t, ALICE, "/webdav/", "0").status, 207);
        // OPTIONS answers without credentials and advertises class 2.
        let o = send(&t, None, "OPTIONS", "/webdav/", &[], None);
        assert_eq!(o.status, 200);
        assert_eq!(o.header("dav"), Some("1, 2"));
        assert!(o.header("allow").unwrap().contains("LOCK"));
    }

    #[test]
    fn homes_are_private_and_shares_follow_membership() {
        let t = start("spaces", None);
        assert_eq!(put(&t, ALICE, "/webdav/alice/note.txt", b"hi"), 201);
        assert_eq!(get(&t, BOB, "/webdav/alice/note.txt").status, 403);
        assert_eq!(put(&t, BOB, "/webdav/alice/x.txt", b"x"), 403);
        assert_eq!(get(&t, ALICE, "/webdav/nobody/x").status, 404);

        // Root lists your own home and shared/ only.
        let root = propfind(&t, ALICE, "/webdav/", "1").body_str();
        assert!(root.contains("<D:href>/webdav/alice/</D:href>"), "{root}");
        assert!(root.contains("<D:href>/webdav/shared/</D:href>"), "{root}");
        assert!(!root.contains("/webdav/bob/"), "{root}");
        // shared/ lists only the shares you are in.
        let shared = propfind(&t, ALICE, "/webdav/shared/", "1").body_str();
        assert!(shared.contains("/webdav/shared/fam/"), "{shared}");
        assert!(!shared.contains("/webdav/shared/bobs/"), "{shared}");
        assert_eq!(propfind(&t, ALICE, "/webdav/shared/bobs/", "0").status, 404);

        // fam: alice and bob write, carol reads.
        assert_eq!(put(&t, ALICE, "/webdav/shared/fam/list.txt", b"milk"), 201);
        assert_eq!(get(&t, BOB, "/webdav/shared/fam/list.txt").body, b"milk");
        assert_eq!(get(&t, CAROL, "/webdav/shared/fam/list.txt").body, b"milk");
        assert_eq!(put(&t, CAROL, "/webdav/shared/fam/list.txt", b"x"), 403);
        assert_eq!(
            send(
                &t,
                Some(CAROL),
                "DELETE",
                "/webdav/shared/fam/list.txt",
                &[],
                None
            )
            .status,
            403
        );
        // On disk, exactly where the layout says.
        assert_eq!(
            fs::read(t.root.join("shared/fam/list.txt")).unwrap(),
            b"milk"
        );
        assert!(t.root.join("users/alice/note.txt").is_file());
        // Space roots and virtual collections cannot be removed or replaced.
        assert_eq!(
            send(&t, Some(ALICE), "DELETE", "/webdav/alice/", &[], None).status,
            403
        );
        assert_eq!(
            send(&t, Some(ALICE), "DELETE", "/webdav/shared/", &[], None).status,
            403
        );
        assert_eq!(put(&t, ALICE, "/webdav/alice/", b"x"), 405);
    }

    #[test]
    fn put_get_head_range_and_conditionals() {
        let t = start("rw", None);
        assert_eq!(
            put(&t, ALICE, "/webdav/alice/a%20b.txt", b"0123456789"),
            201
        );
        let r = get(&t, ALICE, "/webdav/alice/a%20b.txt");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"0123456789");
        assert_eq!(r.header("content-type"), Some("text/plain"));
        let etag = r.header("etag").unwrap().to_string();

        let h = send(
            &t,
            Some(ALICE),
            "HEAD",
            "/webdav/alice/a%20b.txt",
            &[],
            None,
        );
        assert_eq!(h.status, 200);
        assert_eq!(h.header("content-length"), Some("10"));

        let p = send(
            &t,
            Some(ALICE),
            "GET",
            "/webdav/alice/a%20b.txt",
            &[("Range", "bytes=2-4")],
            None,
        );
        assert_eq!(p.status, 206);
        assert_eq!(p.body, b"234");
        assert_eq!(p.header("content-range"), Some("bytes 2-4/10"));
        let bad = send(
            &t,
            Some(ALICE),
            "GET",
            "/webdav/alice/a%20b.txt",
            &[("Range", "bytes=50-")],
            None,
        );
        assert_eq!(bad.status, 416);

        let nm = send(
            &t,
            Some(ALICE),
            "GET",
            "/webdav/alice/a%20b.txt",
            &[("If-None-Match", &etag)],
            None,
        );
        assert_eq!(nm.status, 304);
        let no_clobber = send(
            &t,
            Some(ALICE),
            "PUT",
            "/webdav/alice/a%20b.txt",
            &[("If-None-Match", "*")],
            Some(b"x"),
        );
        assert_eq!(no_clobber.status, 412);
        let stale = send(
            &t,
            Some(ALICE),
            "PUT",
            "/webdav/alice/a%20b.txt",
            &[("If-Match", "\"nope\"")],
            Some(b"x"),
        );
        assert_eq!(stale.status, 412);
        let ok = send(
            &t,
            Some(ALICE),
            "PUT",
            "/webdav/alice/a%20b.txt",
            &[("If-Match", &etag)],
            Some(b"new"),
        );
        assert_eq!(ok.status, 204);
        assert_eq!(get(&t, ALICE, "/webdav/alice/a%20b.txt").body, b"new");

        // A PUT into a missing collection conflicts; nothing lands in .tmp.
        assert_eq!(put(&t, ALICE, "/webdav/alice/no/such.txt", b"x"), 409);
        assert_eq!(fs::read_dir(t.root.join(".tmp")).unwrap().count(), 0);
    }

    #[test]
    fn upload_limit_and_chunked_uploads() {
        let t = start("limit", Some(8));
        assert_eq!(put(&t, ALICE, "/webdav/alice/small", b"12345678"), 201);
        assert_eq!(put(&t, ALICE, "/webdav/alice/big", b"123456789"), 413);
        assert!(!t.root.join("users/alice/big").exists());

        // Chunked, keep-alive, then a second request on the same socket.
        let addr = t.base.trim_start_matches("http://");
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let auth = httpc::basic_auth_header("alice", "pa");
        write!(
            s,
            "PUT /webdav/alice/chunked HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"
        )
        .unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let status = httpc::read_line(&mut r).unwrap();
        assert!(status.contains("201"), "{status}");
        let headers = httpc::read_headers(&mut r).unwrap();
        assert_eq!(
            headers.get("connection").map(String::as_str),
            Some("keep-alive")
        );
        write!(
            s,
            "GET /webdav/alice/chunked HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let status = httpc::read_line(&mut r).unwrap();
        assert!(status.contains("200"), "{status}");
        let headers = httpc::read_headers(&mut r).unwrap();
        let mut body = Vec::new();
        r.read_to_end(&mut body).unwrap();
        assert_eq!(headers.get("content-length").map(String::as_str), Some("5"));
        assert_eq!(body, b"abcde");

        // Chunked past the limit: 413, nothing stored.
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "PUT /webdav/alice/toobig HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\nTransfer-Encoding: chunked\r\n\r\n9\r\n123456789\r\n0\r\n\r\n"
        )
        .unwrap();
        let mut r = BufReader::new(s);
        assert!(httpc::read_line(&mut r).unwrap().contains("413"));
        assert!(!t.root.join("users/alice/toobig").exists());
    }

    #[test]
    fn expect_continue_is_only_sent_once_the_request_is_allowed() {
        let t = start("expect", None);
        let addr = t.base.trim_start_matches("http://");
        // Refused: the final status comes at once, no 100.
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let bob = httpc::basic_auth_header("bob", "pb");
        write!(
            s,
            "PUT /webdav/alice/x HTTP/1.1\r\nHost: x\r\nAuthorization: {bob}\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n"
        )
        .unwrap();
        let mut r = BufReader::new(s);
        assert!(httpc::read_line(&mut r).unwrap().contains("403"));

        // Allowed: 100 first, then the body, then 201.
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let alice = httpc::basic_auth_header("alice", "pa");
        write!(
            s,
            "PUT /webdav/alice/x HTTP/1.1\r\nHost: x\r\nAuthorization: {alice}\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n"
        )
        .unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        assert!(httpc::read_line(&mut r).unwrap().contains("100"));
        assert_eq!(httpc::read_line(&mut r).unwrap(), "");
        s.write_all(b"abc").unwrap();
        assert!(httpc::read_line(&mut r).unwrap().contains("201"));
        assert_eq!(fs::read(t.root.join("users/alice/x")).unwrap(), b"abc");
    }

    #[test]
    fn mkcol_propfind_copy_move_delete() {
        let t = start("tree", None);
        assert_eq!(
            send(&t, Some(ALICE), "MKCOL", "/webdav/alice/docs", &[], None).status,
            201
        );
        assert_eq!(
            send(&t, Some(ALICE), "MKCOL", "/webdav/alice/docs", &[], None).status,
            405
        );
        assert_eq!(
            send(&t, Some(ALICE), "MKCOL", "/webdav/alice/x/y", &[], None).status,
            409
        );
        assert_eq!(
            put(&t, ALICE, "/webdav/alice/docs/r%C3%A9sum%C3%A9.txt", b"cv"),
            201
        );

        let pf = propfind(&t, ALICE, "/webdav/alice/docs/", "1");
        assert_eq!(pf.status, 207);
        let body = pf.body_str();
        assert!(
            body.contains("<D:href>/webdav/alice/docs/</D:href>"),
            "{body}"
        );
        assert!(
            body.contains("<D:href>/webdav/alice/docs/r%C3%A9sum%C3%A9.txt</D:href>"),
            "{body}"
        );
        assert!(
            body.contains("<D:displayname>résumé.txt</D:displayname>"),
            "{body}"
        );
        assert!(
            body.contains("<D:getcontentlength>2</D:getcontentlength>"),
            "{body}"
        );
        assert!(body.contains("<D:collection/>"), "{body}");
        assert!(body.contains("<D:supportedlock>"), "{body}");
        let one = send(
            &t,
            Some(ALICE),
            "PROPFIND",
            "/webdav/alice/docs/",
            &[("Depth", "0")],
            Some(br#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:prop><D:quota-available-bytes/><X:nope xmlns:X="urn:x"/></D:prop></D:propfind>"#),
        )
        .body_str();
        assert!(one.contains("quota-available-bytes>"), "{one}");
        assert!(one.contains("404 Not Found"), "{one}");
        assert_eq!(
            propfind(&t, ALICE, "/webdav/alice/", "infinity").status,
            403
        );

        // COPY a tree into the shared space, then MOVE it back under a new name.
        let dest = format!("{}/webdav/shared/fam/docs", t.base);
        let c = send(
            &t,
            Some(ALICE),
            "COPY",
            "/webdav/alice/docs/",
            &[("Destination", &dest)],
            None,
        );
        assert_eq!(c.status, 201);
        assert_eq!(
            get(&t, BOB, "/webdav/shared/fam/docs/r%C3%A9sum%C3%A9.txt").body,
            b"cv"
        );
        let again = send(
            &t,
            Some(ALICE),
            "COPY",
            "/webdav/alice/docs/",
            &[("Destination", &dest), ("Overwrite", "F")],
            None,
        );
        assert_eq!(again.status, 412);
        let over = send(
            &t,
            Some(ALICE),
            "COPY",
            "/webdav/alice/docs/",
            &[("Destination", &dest)],
            None,
        );
        assert_eq!(over.status, 204);

        let m = send(
            &t,
            Some(BOB),
            "MOVE",
            "/webdav/shared/fam/docs",
            &[("Destination", "/webdav/bob/from-fam")],
            None,
        );
        assert_eq!(m.status, 201);
        assert!(!t.root.join("shared/fam/docs").exists());
        assert!(t.root.join("users/bob/from-fam/résumé.txt").is_file());
        // No moving into someone else's home, into itself, or off-server.
        let into_alice = send(
            &t,
            Some(BOB),
            "MOVE",
            "/webdav/bob/from-fam",
            &[("Destination", "/webdav/alice/stolen")],
            None,
        );
        assert_eq!(into_alice.status, 403);
        let into_self = send(
            &t,
            Some(BOB),
            "MOVE",
            "/webdav/bob/from-fam",
            &[("Destination", "/webdav/bob/from-fam/sub")],
            None,
        );
        assert_eq!(into_self.status, 403);
        let foreign = send(
            &t,
            Some(BOB),
            "COPY",
            "/webdav/bob/from-fam",
            &[("Destination", "http://elsewhere/dav/x")],
            None,
        );
        assert_eq!(foreign.status, 502);
        // Carol may copy out of fam but not move out of it.
        put(&t, ALICE, "/webdav/shared/fam/f", b"f");
        let carol_copy = send(
            &t,
            Some(CAROL),
            "COPY",
            "/webdav/shared/fam/f",
            &[("Destination", "/webdav/carol/f")],
            None,
        );
        assert_eq!(carol_copy.status, 201);
        let carol_move = send(
            &t,
            Some(CAROL),
            "MOVE",
            "/webdav/shared/fam/f",
            &[("Destination", "/webdav/carol/g")],
            None,
        );
        assert_eq!(carol_move.status, 403);

        assert_eq!(
            send(&t, Some(BOB), "DELETE", "/webdav/bob/from-fam", &[], None).status,
            204
        );
        assert!(!t.root.join("users/bob/from-fam").exists());
        assert_eq!(
            send(&t, Some(BOB), "DELETE", "/webdav/bob/from-fam", &[], None).status,
            404
        );
    }

    #[test]
    fn traversal_and_symlinks_are_refused() {
        let t = start("escape", None);
        put(&t, BOB, "/webdav/bob/secret", b"s");
        for path in [
            "/webdav/alice/%2e%2e/%2e%2e/users/bob/secret",
            "/webdav/alice/..%2Fbob%2Fsecret",
            "/webdav/alice/a%5Cb",
        ] {
            assert_eq!(get(&t, ALICE, path).status, 400, "{path}");
        }
        std::os::unix::fs::symlink(t.root.join("users/bob"), t.root.join("users/alice/link"))
            .unwrap();
        assert_eq!(get(&t, ALICE, "/webdav/alice/link/secret").status, 404);
        assert_eq!(put(&t, ALICE, "/webdav/alice/link/new", b"x"), 404);
        assert!(
            !propfind(&t, ALICE, "/webdav/alice/", "1")
                .body_str()
                .contains("link")
        );
        assert!(!t.root.join("users/bob/new").exists());
    }

    #[test]
    fn locks_guard_writes_until_the_token_is_presented() {
        let t = start("locks", None);
        let lockinfo: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner><D:href>mailto:alice@example.com</D:href></D:owner></D:lockinfo>"#;
        // Locking an unmapped URL creates the file.
        let l = send(
            &t,
            Some(ALICE),
            "LOCK",
            "/webdav/shared/fam/doc.txt",
            &[("Timeout", "Second-120")],
            Some(lockinfo),
        );
        assert_eq!(l.status, 201);
        assert!(t.root.join("shared/fam/doc.txt").is_file());
        let token_header = l.header("lock-token").unwrap().to_string();
        let token = token_header
            .trim_matches(|c| c == '<' || c == '>')
            .to_string();
        let lb = l.body_str();
        assert!(lb.contains("<D:exclusive/>") && lb.contains(&token), "{lb}");
        assert!(lb.contains("mailto:alice@example.com"), "{lb}");

        // Bob cannot write or lock it; alice cannot write without the token.
        assert_eq!(put(&t, BOB, "/webdav/shared/fam/doc.txt", b"b"), 423);
        assert_eq!(put(&t, ALICE, "/webdav/shared/fam/doc.txt", b"a"), 423);
        let b = send(
            &t,
            Some(BOB),
            "LOCK",
            "/webdav/shared/fam/doc.txt",
            &[],
            Some(lockinfo),
        );
        assert_eq!(b.status, 423);
        let if_hdr = format!("(<{}>)", token);
        let w = send(
            &t,
            Some(ALICE),
            "PUT",
            "/webdav/shared/fam/doc.txt",
            &[("If", &if_hdr)],
            Some(b"a"),
        );
        assert_eq!(w.status, 204);
        // Bob presenting alice's token is still locked out.
        let w = send(
            &t,
            Some(BOB),
            "PUT",
            "/webdav/shared/fam/doc.txt",
            &[("If", &if_hdr)],
            Some(b"b"),
        );
        assert_eq!(w.status, 423);
        // Deleting the parent collection would remove the locked file.
        assert_eq!(
            send(&t, Some(BOB), "MKCOL", "/webdav/shared/fam/d", &[], None).status,
            201
        );

        // lockdiscovery shows it; refresh extends it.
        let pf = propfind(&t, BOB, "/webdav/shared/fam/doc.txt", "0").body_str();
        assert!(pf.contains("<D:activelock>"), "{pf}");
        let r = send(
            &t,
            Some(ALICE),
            "LOCK",
            "/webdav/shared/fam/doc.txt",
            &[("If", &if_hdr), ("Timeout", "Second-600")],
            None,
        );
        assert_eq!(r.status, 200);
        assert!(r.body_str().contains("Second-600") || r.body_str().contains("Second-599"));

        // UNLOCK: wrong owner, wrong token, then the real thing.
        let tok = format!("<{}>", token);
        assert_eq!(
            send(
                &t,
                Some(BOB),
                "UNLOCK",
                "/webdav/shared/fam/doc.txt",
                &[("Lock-Token", &tok)],
                None
            )
            .status,
            403
        );
        assert_eq!(
            send(
                &t,
                Some(ALICE),
                "UNLOCK",
                "/webdav/shared/fam/doc.txt",
                &[("Lock-Token", "<opaquelocktoken:x>")],
                None
            )
            .status,
            409
        );
        assert_eq!(
            send(
                &t,
                Some(ALICE),
                "UNLOCK",
                "/webdav/shared/fam/doc.txt",
                &[("Lock-Token", &tok)],
                None
            )
            .status,
            204
        );
        assert_eq!(put(&t, BOB, "/webdav/shared/fam/doc.txt", b"b"), 204);

        // A depth-infinity lock on a collection covers new members.
        let dl = send(
            &t,
            Some(ALICE),
            "LOCK",
            "/webdav/shared/fam/d/",
            &[],
            Some(lockinfo),
        );
        assert_eq!(dl.status, 200);
        assert_eq!(put(&t, BOB, "/webdav/shared/fam/d/new", b"n"), 423);
        assert_eq!(
            send(&t, Some(BOB), "DELETE", "/webdav/shared/fam/d", &[], None).status,
            423
        );
        // Read-only members cannot lock.
        assert_eq!(
            send(
                &t,
                Some(CAROL),
                "LOCK",
                "/webdav/shared/fam/x",
                &[],
                Some(lockinfo)
            )
            .status,
            403
        );
    }

    #[test]
    fn proppatch_accepts_windows_times_and_refuses_the_rest() {
        let t = start("proppatch", None);
        put(&t, ALICE, "/webdav/alice/w.txt", b"w");
        let win = br#"<?xml version="1.0" encoding="utf-8"?><D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32CreationTime>Mon, 01 Jan 2024 10:00:00 GMT</Z:Win32CreationTime><Z:Win32LastModifiedTime>Mon, 01 Jan 2024 10:00:00 GMT</Z:Win32LastModifiedTime><Z:Win32FileAttributes>00000020</Z:Win32FileAttributes></D:prop></D:set></D:propertyupdate>"#;
        let r = send(
            &t,
            Some(ALICE),
            "PROPPATCH",
            "/webdav/alice/w.txt",
            &[],
            Some(win),
        );
        assert_eq!(r.status, 207);
        assert!(r.body_str().contains("200 OK"), "{}", r.body_str());
        let mtime = fs::metadata(t.root.join("users/alice/w.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            DateTime::<Utc>::from(mtime).to_rfc3339(),
            "2024-01-01T10:00:00+00:00"
        );
        let other = br#"<?xml version="1.0"?><D:propertyupdate xmlns:D="DAV:" xmlns:X="urn:x"><D:set><D:prop><X:color>red</X:color><D:getetag>x</D:getetag></D:prop></D:set></D:propertyupdate>"#;
        let r = send(
            &t,
            Some(ALICE),
            "PROPPATCH",
            "/webdav/alice/w.txt",
            &[],
            Some(other),
        )
        .body_str();
        assert!(r.contains("403 Forbidden"), "{r}");
        assert!(r.contains("cannot-modify-protected-property"), "{r}");
    }

    #[test]
    fn caldav_routes_are_untouched() {
        let t = start("caldav", None);
        let h = send(&t, None, "GET", "/healthz", &[], None);
        assert_eq!(h.status, 200);
        // WebDAV users are not CalDAV users, and vice versa.
        assert_eq!(
            send(
                &t,
                Some(ALICE),
                "PROPFIND",
                "/dav/",
                &[("Depth", "0")],
                None
            )
            .status,
            401
        );
        assert_eq!(
            send(
                &t,
                Some(("cal@example.com", "calpw")),
                "PROPFIND",
                "/dav/",
                &[("Depth", "0")],
                None
            )
            .status,
            207
        );
        // The browser listing works for a collection.
        put(&t, ALICE, "/webdav/alice/a%26b.txt", b"x");
        let l = get(&t, ALICE, "/webdav/alice/").body_str();
        assert!(l.contains("href=\"/webdav/alice/a%26b.txt\""), "{l}");
        assert!(l.contains(">a&amp;b.txt<"), "{l}");
    }

    #[test]
    fn reload_swaps_users_and_creates_new_spaces() {
        let t = start("reload", None);
        let wd = t.state.webdav.as_ref().unwrap();
        let mut shares = IndexMap::new();
        shares.insert(
            "new".to_string(),
            WebdavShareConfig {
                read_write: vec!["dave".into()],
                read_only: vec![],
            },
        );
        wd.reload(Access::new(&[("dave".into(), "pd".into())], &shares))
            .unwrap();
        assert!(t.root.join("users/dave").is_dir());
        assert!(t.root.join("shared/new").is_dir());
        assert_eq!(propfind(&t, ALICE, "/webdav/", "0").status, 401);
        assert_eq!(put(&t, ("dave", "pd"), "/webdav/shared/new/x", b"x"), 201);
    }

    #[test]
    fn disabled_webdav_claims_nothing() {
        let root = std::env::temp_dir().join(format!("jadav-webdav-off-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let state = Arc::new(ServerState::new(
            root.join("jadav.db"),
            "cal@example.com",
            "calpw",
            vec!["cal@example.com".to_string()],
        ));
        let (_h, addr) = spawn("127.0.0.1:0", Arc::clone(&state)).unwrap();
        let url = HttpUrl::parse(&format!("http://{}/webdav/", addr)).unwrap();
        let headers = vec![(
            "Authorization".to_string(),
            httpc::basic_auth_header("cal@example.com", "calpw"),
        )];
        let r = httpc::send("PROPFIND", &url, &headers, None, Duration::from_secs(5)).unwrap();
        assert_eq!(r.status, 404);
        state.shutdown.store(true, Ordering::Relaxed);
        let _ = fs::remove_dir_all(&root);
    }
}
