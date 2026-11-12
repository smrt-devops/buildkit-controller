//! Object keys, the one layout of every store.
//!
//! ```text
//! data/<id>                              the bytes of one blob or manifest
//! blobs/sha256/<hex>/<id>                empty: data/<id> holds digest <hex>
//! repos/<repo>/_manifests/<hex>/<id>     the manifest record
//! repos/<repo>/_tags/<tag>/<hex>/<id>    empty: tag <tag> names manifest <hex>
//! ```
//!
//! No key is written twice: every write goes to a fresh `<id>`, and a key is
//! deleted only once the index has stopped naming it. A repository's
//! components start with `[a-z0-9]`, so its first component that starts with
//! `_` is the kind marker.

use std::fmt;

use object_store::path::Path;

use super::store::Digest;

/// 32 lowercase hex digits from the OS random number generator. Never parses
/// as a 64-digit digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Id(String);

impl Id {
    /// A fresh id.
    pub fn random() -> std::io::Result<Self> {
        use ring::rand::SecureRandom;
        let mut b = [0u8; 16];
        ring::rand::SystemRandom::new()
            .fill(&mut b)
            .map_err(|_| std::io::Error::other("OS random number generator failed"))?;
        Ok(Self(super::store::hex(&b)))
    }

    pub fn parse(s: &str) -> Option<Self> {
        (s.len() == 32 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One key of the layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Key {
    Data(Id),
    Marker(Digest, Id),
    Record {
        repo: String,
        digest: Digest,
        id: Id,
    },
    Tag {
        repo: String,
        tag: String,
        digest: Digest,
        id: Id,
    },
}

/// The listing prefixes, one per top-level directory.
pub(super) const DATA: &str = "data";
pub(super) const BLOBS: &str = "blobs";
pub(super) const REPOS: &str = "repos";

impl Key {
    pub fn path(&self) -> Path {
        Path::from(self.to_string())
    }

    /// The key at `s`, a path relative to the store's root. `None` for
    /// anything that is not one of the four shapes.
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('/').collect();
        match parts.as_slice() {
            [DATA, id] => Id::parse(id).map(Self::Data),
            [BLOBS, "sha256", hex, id] => {
                Some(Self::Marker(Digest::from_hex(hex)?, Id::parse(id)?))
            }
            [REPOS, rest @ ..] => {
                let kind = rest.iter().position(|c| c.starts_with('_'))?;
                let repo = &rest[..kind];
                // The API accepts only names of three or more components.
                if repo.len() < 3 || repo.iter().any(|c| !valid_component(c)) {
                    return None;
                }
                let repo = repo.join("/");
                match &rest[kind..] {
                    ["_manifests", hex, id] => Some(Self::Record {
                        repo,
                        digest: Digest::from_hex(hex)?,
                        id: Id::parse(id)?,
                    }),
                    ["_tags", tag, hex, id] => Some(Self::Tag {
                        repo,
                        tag: valid_tag(tag)?.to_owned(),
                        digest: Digest::from_hex(hex)?,
                        id: Id::parse(id)?,
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn valid_component(c: &str) -> bool {
    c.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && c.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn valid_tag(t: &str) -> Option<&str> {
    matches!(t.parse(), Ok(super::store::Reference::Tag(_))).then_some(t)
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data(id) => write!(f, "{DATA}/{id}"),
            Self::Marker(d, id) => write!(f, "{BLOBS}/sha256/{}/{id}", d.hex()),
            Self::Record { repo, digest, id } => {
                write!(f, "{REPOS}/{repo}/_manifests/{}/{id}", digest.hex())
            }
            Self::Tag {
                repo,
                tag,
                digest,
                id,
            } => write!(f, "{REPOS}/{repo}/_tags/{tag}/{}/{id}", digest.hex()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn keys_round_trip_and_other_shapes_do_not_parse() {
        let d = Digest::of(b"x");
        let id = Id::random().unwrap();
        for k in [
            Key::Data(id.clone()),
            Key::Marker(d.clone(), id.clone()),
            Key::Record {
                repo: "ns/p/a/b".into(),
                digest: d.clone(),
                id: id.clone(),
            },
            Key::Tag {
                repo: "ns/p/app".into(),
                tag: "_v1.0-x".into(),
                digest: d.clone(),
                id: id.clone(),
            },
        ] {
            assert_eq!(Key::parse(&k.to_string()), Some(k.clone()), "{k}");
        }
        let hex = d.hex();
        for bad in [
            // 0.4 files.
            format!("blobs/sha256/{hex}"),
            format!("repos/ns/p/app/_links/{hex}"),
            format!("repos/ns/p/app/_manifests/{hex}"),
            "repos/ns/p/app/_tags/latest".to_owned(),
            format!("uploads/{id}"),
            format!("tmp/{id}"),
            // A staged file.
            format!("data/{id}#1"),
            // A digest where an id belongs, and the reverse.
            format!("data/{hex}"),
            format!("blobs/sha256/{id}/{hex}"),
            // Repository names the API never accepts.
            format!("repos/ns/p/_manifests/{hex}/{id}"),
            format!("repos/ns/p/App/_manifests/{hex}/{id}"),
            format!("repos/ns/p/app/_tags/.x/{hex}/{id}"),
        ] {
            assert_eq!(Key::parse(&bad), None, "{bad}");
        }
    }
}
