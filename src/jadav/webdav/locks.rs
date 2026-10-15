//! WebDAV class 2 write locks (RFC 4918 §6–7), held in memory.
//!
//! macOS Finder and Windows Explorer refuse to mount a share read-write
//! unless the server advertises `DAV: 2`, and they lock every file they
//! write. Locks are advisory bookkeeping for exactly those clients, so
//! they live in a `Mutex<Vec<_>>` and vanish on restart; a client that
//! then presents a stale token in `If:` is let through rather than refused
//! (unknown tokens are ignored, see [`tokens_in_if`]), which is the
//! forgiving side of RFC 4918's `If` evaluation.
//!
//! Paths are *logical* — the decoded URL segments after `/webdav/` — so a
//! lock follows the URL namespace, not the filesystem.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
pub const MAX_TIMEOUT_SECS: u64 = 3600;

#[derive(Clone, Debug)]
pub struct Lock {
    pub token: String,
    pub path: Vec<String>,
    /// Encoded href of the lock root, for `lockroot`.
    pub root_href: String,
    pub exclusive: bool,
    /// `Depth: infinity` (covers every descendant) vs `0`.
    pub infinite: bool,
    /// The client's `<D:owner>` content, echoed back verbatim.
    pub owner_xml: String,
    pub user: String,
    pub timeout_secs: u64,
    expires: Instant,
}

impl Lock {
    fn covers(&self, path: &[String]) -> bool {
        self.path == path || (self.infinite && is_proper_prefix(&self.path, path))
    }

    /// Seconds left before the lock lapses.
    pub fn remaining_secs(&self) -> u64 {
        self.expires
            .saturating_duration_since(Instant::now())
            .as_secs()
    }

    /// The `<D:activelock>` element for `lockdiscovery` and `LOCK` replies.
    pub fn activelock_xml(&self) -> String {
        format!(
            "<D:activelock><D:locktype><D:write/></D:locktype><D:lockscope>{}</D:lockscope>\
             <D:depth>{}</D:depth>{}<D:timeout>Second-{}</D:timeout>\
             <D:locktoken><D:href>{}</D:href></D:locktoken>\
             <D:lockroot><D:href>{}</D:href></D:lockroot></D:activelock>",
            if self.exclusive {
                "<D:exclusive/>"
            } else {
                "<D:shared/>"
            },
            if self.infinite { "infinity" } else { "0" },
            if self.owner_xml.is_empty() {
                String::new()
            } else {
                format!("<D:owner>{}</D:owner>", self.owner_xml)
            },
            self.remaining_secs(),
            crate::jadav::xml::escape_text(&self.token),
            crate::jadav::xml::escape_text(&self.root_href),
        )
    }
}

pub const SUPPORTED_LOCK_XML: &str = "<D:lockentry><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockentry><D:lockentry><D:lockscope><D:shared/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockentry>";

fn is_proper_prefix(prefix: &[String], path: &[String]) -> bool {
    prefix.len() < path.len() && path[..prefix.len()] == *prefix
}

/// What a new lock request looks like.
pub struct LockRequest<'a> {
    pub path: &'a [String],
    pub root_href: &'a str,
    pub exclusive: bool,
    pub infinite: bool,
    pub owner_xml: &'a str,
    pub user: &'a str,
    pub timeout_secs: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UnlockError {
    /// No lock with that token covers the URL (409 `lock-token-matches-request-uri`).
    NoSuchLock,
    /// The lock belongs to another user (403).
    NotOwner,
}

#[derive(Default)]
pub struct LockManager {
    locks: Mutex<Vec<Lock>>,
}

impl LockManager {
    fn with<T>(&self, f: impl FnOnce(&mut Vec<Lock>) -> T) -> T {
        let mut locks = self.locks.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        locks.retain(|l| l.expires > now);
        f(&mut locks)
    }

    /// Take a new lock, or return the root hrefs of the locks it conflicts
    /// with.
    pub fn lock(&self, req: LockRequest<'_>) -> Result<Lock, Vec<String>> {
        self.with(|locks| {
            let conflicts: Vec<String> = locks
                .iter()
                .filter(|l| {
                    let overlaps =
                        l.covers(req.path) || (req.infinite && is_proper_prefix(req.path, &l.path));
                    overlaps && (req.exclusive || l.exclusive)
                })
                .map(|l| l.root_href.clone())
                .collect();
            if !conflicts.is_empty() {
                return Err(conflicts);
            }
            let timeout = req.timeout_secs.clamp(1, MAX_TIMEOUT_SECS);
            let lock = Lock {
                token: format!("opaquelocktoken:{}", random_uuid()),
                path: req.path.to_vec(),
                root_href: req.root_href.to_string(),
                exclusive: req.exclusive,
                infinite: req.infinite,
                owner_xml: req.owner_xml.to_string(),
                user: req.user.to_string(),
                timeout_secs: timeout,
                expires: Instant::now() + Duration::from_secs(timeout),
            };
            locks.push(lock.clone());
            Ok(lock)
        })
    }

    /// Refresh (a `LOCK` without a body): extend the first lock that covers
    /// `path`, belongs to `user` and whose token was submitted.
    pub fn refresh(
        &self,
        path: &[String],
        tokens: &[String],
        user: &str,
        timeout_secs: u64,
    ) -> Option<Lock> {
        self.with(|locks| {
            let lock = locks
                .iter_mut()
                .find(|l| l.covers(path) && l.user == user && tokens.contains(&l.token))?;
            let timeout = timeout_secs.clamp(1, MAX_TIMEOUT_SECS);
            lock.timeout_secs = timeout;
            lock.expires = Instant::now() + Duration::from_secs(timeout);
            Some(lock.clone())
        })
    }

    pub fn unlock(&self, path: &[String], token: &str, user: &str) -> Result<(), UnlockError> {
        self.with(|locks| {
            let idx = locks
                .iter()
                .position(|l| l.token == token && l.covers(path))
                .ok_or(UnlockError::NoSuchLock)?;
            if locks[idx].user != user {
                return Err(UnlockError::NotOwner);
            }
            locks.remove(idx);
            Ok(())
        })
    }

    /// May `user`, presenting `tokens`, modify `path`? `recursive` covers
    /// the descendants too (DELETE/MOVE of a collection); `membership`
    /// also checks the parent collection, whose member list changes when
    /// `path` is created, deleted or moved. `Err` carries the root hrefs
    /// of the locks in the way, for the 423 body.
    pub fn check_write(
        &self,
        path: &[String],
        recursive: bool,
        membership: bool,
        tokens: &[String],
        user: &str,
    ) -> Result<(), Vec<String>> {
        let parent = &path[..path.len().saturating_sub(1)];
        self.with(|locks| {
            let blocking: Vec<String> = locks
                .iter()
                .filter(|l| {
                    let applies = l.covers(path)
                        || (recursive && is_proper_prefix(path, &l.path))
                        || (membership && !path.is_empty() && l.path == parent);
                    applies && !(l.user == user && tokens.contains(&l.token))
                })
                .map(|l| l.root_href.clone())
                .collect();
            if blocking.is_empty() {
                Ok(())
            } else {
                Err(blocking)
            }
        })
    }

    /// Every lock that covers `path`, for `lockdiscovery`.
    pub fn covering(&self, path: &[String]) -> Vec<Lock> {
        self.with(|locks| locks.iter().filter(|l| l.covers(path)).cloned().collect())
    }

    /// Drop the locks on `path` and below — the resource is gone.
    pub fn remove_under(&self, path: &[String]) {
        self.with(|locks| locks.retain(|l| l.path != path && !is_proper_prefix(path, &l.path)));
    }
}

/// Every `opaquelocktoken:` URI inside angle brackets in an `If:` header
/// (or a `Lock-Token:` header). Resource tags and ETag conditions are not
/// evaluated: a lock is satisfied by its token, nothing else.
pub fn tokens_in_if(header: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(mut rest) = header else {
        return out;
    };
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            break;
        };
        let inner = after[..end].trim();
        if inner.starts_with("opaquelocktoken:") {
            out.push(inner.to_string());
        }
        rest = &after[end + 1..];
    }
    out
}

/// `Timeout: Second-600, Infinite` → the first value we understand,
/// capped; `Infinite` means the cap.
pub fn parse_timeout(header: Option<&str>) -> u64 {
    for part in header.unwrap_or("").split(',') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("infinite") {
            return MAX_TIMEOUT_SECS;
        }
        if let Some(n) = part
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("second-"))
            .and_then(|_| part[7..].parse::<u64>().ok())
        {
            return n.clamp(1, MAX_TIMEOUT_SECS);
        }
    }
    DEFAULT_TIMEOUT_SECS
}

/// `n` random bytes from the kernel, hex-encoded; falls back to a clock +
/// counter mix if `/dev/urandom` is unavailable.
pub fn random_hex(n: usize) -> String {
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = vec![0u8; n];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        use sha2::{Digest, Sha256};
        let seed = format!(
            "{:?}-{}-{}",
            std::time::SystemTime::now(),
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let digest = Sha256::digest(seed.as_bytes());
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = digest[i % digest.len()];
        }
    }
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn random_uuid() -> String {
    let h = random_hex(16);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Vec<String> {
        s.split('/')
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    }

    fn req<'a>(
        path: &'a [String],
        exclusive: bool,
        infinite: bool,
        user: &'a str,
    ) -> LockRequest<'a> {
        LockRequest {
            path,
            root_href: "/webdav/x",
            exclusive,
            infinite,
            owner_xml: "",
            user,
            timeout_secs: 60,
        }
    }

    #[test]
    fn exclusive_conflicts_shared_coexist() {
        let m = LockManager::default();
        let f = p("alice/a.txt");
        let a = m.lock(req(&f, false, false, "alice")).unwrap();
        assert!(a.token.starts_with("opaquelocktoken:"));
        m.lock(req(&f, false, false, "alice")).unwrap();
        assert!(m.lock(req(&f, true, false, "alice")).is_err());
        // An infinite lock on the parent overlaps the file's locks.
        assert!(m.lock(req(&p("alice"), true, true, "alice")).is_err());
        // A depth-0 lock on the parent does not.
        m.lock(req(&p("alice"), true, false, "alice")).unwrap();
    }

    #[test]
    fn writes_need_the_owners_token() {
        let m = LockManager::default();
        let dir = p("shared/fam/docs");
        let lock = m.lock(req(&dir, true, true, "alice")).unwrap();
        let file = p("shared/fam/docs/x.txt");
        assert!(m.check_write(&file, false, true, &[], "alice").is_err());
        m.check_write(
            &file,
            false,
            true,
            std::slice::from_ref(&lock.token),
            "alice",
        )
        .unwrap();
        // Bob holding alice's token is still refused.
        assert!(
            m.check_write(&file, false, true, std::slice::from_ref(&lock.token), "bob")
                .is_err()
        );
        // Deleting the parent collection touches the locked subtree.
        assert!(
            m.check_write(&p("shared/fam"), true, true, &[], "alice")
                .is_err()
        );
        assert!(
            m.check_write(&p("shared/fam"), false, false, &[], "alice")
                .is_ok()
        );
        m.remove_under(&p("shared/fam"));
        assert!(m.check_write(&file, false, true, &[], "alice").is_ok());
    }

    #[test]
    fn depth_zero_collection_lock_guards_membership_only() {
        let m = LockManager::default();
        m.lock(req(&p("alice/dir"), true, false, "alice")).unwrap();
        assert!(
            m.check_write(&p("alice/dir/new"), false, true, &[], "alice")
                .is_err()
        );
        assert!(
            m.check_write(&p("alice/dir/old"), false, false, &[], "alice")
                .is_ok()
        );
    }

    #[test]
    fn unlock_and_refresh() {
        let m = LockManager::default();
        let f = p("alice/a");
        let lock = m.lock(req(&f, true, false, "alice")).unwrap();
        assert_eq!(
            m.unlock(&f, "opaquelocktoken:nope", "alice"),
            Err(UnlockError::NoSuchLock)
        );
        assert_eq!(m.unlock(&f, &lock.token, "bob"), Err(UnlockError::NotOwner));
        let refreshed = m
            .refresh(&f, std::slice::from_ref(&lock.token), "alice", 3000)
            .unwrap();
        assert_eq!(refreshed.timeout_secs, 3000);
        m.unlock(&f, &lock.token, "alice").unwrap();
        assert!(m.covering(&f).is_empty());
    }

    #[test]
    fn if_header_and_timeout_parsing() {
        assert_eq!(
            tokens_in_if(Some(
                "<http://h/webdav/a> (<opaquelocktoken:abc> [\"etag\"]) (Not <opaquelocktoken:def>)"
            )),
            vec![
                "opaquelocktoken:abc".to_string(),
                "opaquelocktoken:def".to_string()
            ]
        );
        assert!(tokens_in_if(None).is_empty());
        assert_eq!(parse_timeout(None), DEFAULT_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some("Second-30")), 30);
        assert_eq!(
            parse_timeout(Some("Infinite, Second-4100000000")),
            MAX_TIMEOUT_SECS
        );
        assert_eq!(parse_timeout(Some("Second-99999")), MAX_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some("garbage")), DEFAULT_TIMEOUT_SECS);
    }
}
