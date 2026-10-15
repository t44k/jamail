//! The `/webdav/` URL namespace: which space a URL names, who may touch
//! it, and where it lives on disk.
//!
//! ```text
//! /webdav/                        virtual: your home + shared/
//! /webdav/<login>/…               <root>/users/<login>/…   owner only
//! /webdav/shared/                 virtual: the shares you belong to
//! /webdav/shared/<name>/…         <root>/shared/<name>/…   members only
//! ```
//!
//! Segments are percent-decoded strictly — invalid UTF-8, `.`/`..`, and an
//! encoded `/`, `\` or NUL are refused outright rather than normalised, so
//! no URL can climb out of its space — and symlinks inside a space are
//! never followed (see [`find_symlink`]).

use crate::jadav::config::WEBDAV_SHARED_SEGMENT;
use crate::jadav::server::encode_href;
use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};

pub const PREFIX: &str = "/webdav";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceKind {
    Home,
    Share,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Space {
    pub kind: SpaceKind,
    pub name: String,
}

impl Space {
    pub fn dir(&self, root: &Path) -> PathBuf {
        match self.kind {
            SpaceKind::Home => root.join("users").join(&self.name),
            SpaceKind::Share => root.join("shared").join(&self.name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// `/webdav/`
    Root,
    /// `/webdav/shared/`
    SharedRoot,
    /// A path inside a space; `rel` empty is the space's own root.
    Item { space: Space, rel: Vec<String> },
}

impl Target {
    /// Decoded URL segments after `/webdav/` — the key locks are held by.
    pub fn logical(&self) -> Vec<String> {
        match self {
            Target::Root => Vec::new(),
            Target::SharedRoot => vec![WEBDAV_SHARED_SEGMENT.to_string()],
            Target::Item { space, rel } => {
                let mut out = match space.kind {
                    SpaceKind::Home => vec![space.name.clone()],
                    SpaceKind::Share => {
                        vec![WEBDAV_SHARED_SEGMENT.to_string(), space.name.clone()]
                    }
                };
                out.extend(rel.iter().cloned());
                out
            }
        }
    }

    /// Where it lives on disk; `None` for the virtual collections.
    pub fn fs_path(&self, root: &Path) -> Option<PathBuf> {
        match self {
            Target::Item { space, rel } => {
                let mut p = space.dir(root);
                p.extend(rel);
                Some(p)
            }
            _ => None,
        }
    }

    /// The target one level down.
    pub fn child(&self, name: &str) -> Option<Target> {
        match self {
            Target::Root => None,
            Target::SharedRoot => Some(Target::Item {
                space: Space {
                    kind: SpaceKind::Share,
                    name: name.to_string(),
                },
                rel: Vec::new(),
            }),
            Target::Item { space, rel } => {
                let mut rel = rel.clone();
                rel.push(name.to_string());
                Some(Target::Item {
                    space: space.clone(),
                    rel,
                })
            }
        }
    }

    /// The parent collection, for anything below a space root.
    pub fn parent(&self) -> Option<Target> {
        match self {
            Target::Item { space, rel } if !rel.is_empty() => Some(Target::Item {
                space: space.clone(),
                rel: rel[..rel.len() - 1].to_vec(),
            }),
            _ => None,
        }
    }

    /// The last segment, for `displayname`.
    pub fn name(&self) -> String {
        self.logical().last().cloned().unwrap_or_default()
    }

    /// Percent-encoded href; collections end in `/`.
    pub fn href(&self, collection: bool) -> String {
        let logical = self.logical();
        let mut path = format!("{}/", PREFIX);
        path.push_str(&logical.join("/"));
        if collection && !logical.is_empty() {
            path.push('/');
        }
        encode_href(&path)
    }
}

/// Why a URL was refused before reaching the filesystem.
#[derive(Debug, PartialEq, Eq)]
pub enum PathError {
    /// Not under `/webdav` at all (a `Destination` elsewhere → 502).
    Foreign,
    /// Malformed or escaping segment (400).
    Invalid,
}

/// The path part of a request target or `Destination` header: an
/// absolute URL's scheme and authority are dropped (a reverse proxy may
/// rewrite either), the query is dropped.
pub fn path_of(target: &str) -> &str {
    let mut t = target;
    for scheme in ["http://", "https://"] {
        if t.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
        {
            let rest = &t[scheme.len()..];
            t = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
            break;
        }
    }
    let t = t.split('?').next().unwrap_or("");
    t.split('#').next().unwrap_or("")
}

/// Does this request target belong to the WebDAV file server?
pub fn claims(raw_target: &str) -> bool {
    let p = path_of(raw_target);
    p == PREFIX || p.starts_with("/webdav/")
}

pub fn parse_target(target: &str) -> Result<Target, PathError> {
    let path = path_of(target);
    let rest = if path == PREFIX {
        ""
    } else if let Some(r) = path.strip_prefix("/webdav/") {
        r
    } else {
        return Err(PathError::Foreign);
    };
    let mut segs = Vec::new();
    for raw in rest.split('/').filter(|s| !s.is_empty()) {
        let seg = percent_decode_str(raw)
            .decode_utf8()
            .map_err(|_| PathError::Invalid)?;
        if seg == "."
            || seg == ".."
            || seg.contains(['/', '\\', '\0'])
            || seg.chars().any(|c| c.is_control())
        {
            return Err(PathError::Invalid);
        }
        segs.push(seg.into_owned());
    }
    let mut it = segs.into_iter();
    Ok(match it.next() {
        None => Target::Root,
        Some(first) if first == WEBDAV_SHARED_SEGMENT => match it.next() {
            None => Target::SharedRoot,
            Some(name) => Target::Item {
                space: Space {
                    kind: SpaceKind::Share,
                    name,
                },
                rel: it.collect(),
            },
        },
        Some(login) => Target::Item {
            space: Space {
                kind: SpaceKind::Home,
                name: login,
            },
            rel: it.collect(),
        },
    })
}

/// Is any existing component of `rel` under `base` a symlink? Spaces are
/// confined by refusing to traverse links at all: following one could
/// lead into another user's home or out of the root.
pub fn find_symlink(base: &Path, rel: &[String]) -> bool {
    let mut p = base.to_path_buf();
    for seg in rel {
        p.push(seg);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: SpaceKind, name: &str, rel: &[&str]) -> Target {
        Target::Item {
            space: Space {
                kind,
                name: name.to_string(),
            },
            rel: rel.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parses_the_namespace() {
        assert_eq!(parse_target("/webdav").unwrap(), Target::Root);
        assert_eq!(parse_target("/webdav/").unwrap(), Target::Root);
        assert_eq!(parse_target("/webdav/shared/").unwrap(), Target::SharedRoot);
        assert_eq!(
            parse_target("/webdav/alice/a%20b/c.txt?x=1").unwrap(),
            item(SpaceKind::Home, "alice", &["a b", "c.txt"])
        );
        assert_eq!(
            parse_target("https://dav.example.com/webdav/shared/fam/x").unwrap(),
            item(SpaceKind::Share, "fam", &["x"])
        );
        assert_eq!(
            parse_target("/webdav/t%40mas.gg/").unwrap(),
            item(SpaceKind::Home, "t@mas.gg", &[])
        );
        assert_eq!(parse_target("/dav/x"), Err(PathError::Foreign));
        assert_eq!(
            parse_target("https://other.example/dav/x"),
            Err(PathError::Foreign)
        );
    }

    #[test]
    fn refuses_escapes() {
        for bad in [
            "/webdav/alice/..",
            "/webdav/alice/%2e%2e/bob",
            "/webdav/alice/%2E",
            "/webdav/alice/a%2Fb",
            "/webdav/alice/a%5Cb",
            "/webdav/alice/a%00b",
            "/webdav/alice/%ff",
        ] {
            assert_eq!(parse_target(bad), Err(PathError::Invalid), "{bad}");
        }
    }

    #[test]
    fn logical_paths_and_hrefs() {
        let t = item(SpaceKind::Share, "fam", &["a b"]);
        assert_eq!(t.logical(), vec!["shared", "fam", "a b"]);
        assert_eq!(t.href(false), "/webdav/shared/fam/a%20b");
        assert_eq!(t.href(true), "/webdav/shared/fam/a%20b/");
        assert_eq!(Target::Root.href(true), "/webdav/");
        assert_eq!(Target::SharedRoot.href(true), "/webdav/shared/");
        assert_eq!(
            t.parent().unwrap().fs_path(Path::new("/r")).unwrap(),
            PathBuf::from("/r/shared/fam")
        );
        assert_eq!(
            item(SpaceKind::Home, "alice", &["x"])
                .fs_path(Path::new("/r"))
                .unwrap(),
            PathBuf::from("/r/users/alice/x")
        );
        assert!(item(SpaceKind::Home, "alice", &[]).parent().is_none());
    }

    #[test]
    fn claims_only_its_prefix() {
        assert!(claims("/webdav"));
        assert!(claims("/webdav/alice/x"));
        assert!(!claims("/webdavx"));
        assert!(!claims("/dav/calendars/"));
    }
}
