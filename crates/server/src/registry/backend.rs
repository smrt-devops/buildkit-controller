//! The object store behind the registry, chosen by `BUILDKIT_CACHE_STORE_URL`.
//!
//! `file:///<abs path>` is the volume: a `LocalFileSystem` rooted there,
//! syncing each file and its directory before a write returns, and removing
//! the empty directories that deletes leave behind. Before it is handed out,
//! a sweep removes every file without a key shape of the layout
//! ([`super::layout`]): the 0.4 tree, and files staged by a process that died
//! mid-write, which listings never show. A 0.4 file left where 0.5 needs a
//! directory would fail every push of that digest or tag.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use object_store::ObjectStore;
use object_store::local::LocalFileSystem;
use tracing::info;

use super::layout::Key;

/// The default store: the registry's volume.
pub const DEFAULT_URL: &str = "file:///var/lib/buildkit-cache";

/// Where a store URL points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreUrl {
    /// A directory on a volume.
    File(PathBuf),
}

impl std::fmt::Display for StoreUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(p) => write!(f, "file://{}", p.display()),
        }
    }
}

impl StoreUrl {
    /// Parses `s`. Only `file://` with an absolute path is served.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let url = url::Url::parse(s).with_context(|| format!("store URL `{s}` does not parse"))?;
        match url.scheme() {
            "file" => {
                if url.host_str().is_some_and(|h| !h.is_empty()) {
                    bail!("store URL `{s}` names a host; use file:///<absolute path>");
                }
                if url.query().is_some() || url.fragment().is_some() {
                    bail!("store URL `{s}` has a query or fragment");
                }
                let path = url
                    .to_file_path()
                    .map_err(|()| anyhow::anyhow!("store URL `{s}` needs an absolute path"))?;
                Ok(Self::File(path))
            }
            other => bail!("store URL scheme `{other}` is not served; use file:///<absolute path>"),
        }
    }

    /// Opens the store, creating the directory if needed. Blocking: the
    /// sweep walks the whole volume, and must finish before the registry
    /// serves.
    pub fn open(&self) -> anyhow::Result<Arc<dyn ObjectStore>> {
        match self {
            Self::File(root) => {
                std::fs::create_dir_all(root)
                    .with_context(|| format!("creating {}", root.display()))?;
                let removed =
                    sweep(root).with_context(|| format!("sweeping {}", root.display()))?;
                if removed > 0 {
                    info!(root = %root.display(), removed, "removed files outside the store layout");
                }
                let fs = LocalFileSystem::new_with_prefix(root)
                    .with_context(|| format!("opening {}", root.display()))?
                    .with_fsync(true)
                    .with_automatic_cleanup(true);
                Ok(Arc::new(fs))
            }
        }
    }
}

/// Removes every file under `root` that is not a key of the layout, then
/// every empty directory below `root`. Returns how many files it removed.
pub fn sweep(root: &Path) -> std::io::Result<u64> {
    fn walk(root: &Path, dir: &Path, removed: &mut u64) -> std::io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let path = e.path();
            if e.file_type()?.is_dir() {
                walk(root, &path, removed)?;
                if std::fs::read_dir(&path)?.next().is_none() {
                    std::fs::remove_dir(&path)?;
                }
                continue;
            }
            let key = path
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .map(|s| s.replace(std::path::MAIN_SEPARATOR, "/"));
            if key.as_deref().and_then(Key::parse).is_none() {
                std::fs::remove_file(&path)?;
                *removed += 1;
            }
        }
        Ok(())
    }
    let mut removed = 0;
    walk(root, root, &mut removed)?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn only_absolute_file_urls_parse() {
        assert_eq!(
            StoreUrl::parse(DEFAULT_URL).unwrap(),
            StoreUrl::File("/var/lib/buildkit-cache".into())
        );
        assert_eq!(
            StoreUrl::parse("file:///data/a%20b").unwrap(),
            StoreUrl::File("/data/a b".into())
        );
        for bad in [
            "/var/lib/buildkit-cache",
            "file://host/data",
            "file:///data?x=1",
            "s3://bucket/prefix",
            "s3a://bucket",
            "http://example.invalid/x",
        ] {
            assert!(StoreUrl::parse(bad).is_err(), "{bad}");
        }
    }
}
