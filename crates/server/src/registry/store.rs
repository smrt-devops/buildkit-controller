//! The content store: one directory on one volume, content addressed.
//!
//! ```text
//! blobs/sha256/<hex>              content (manifests too)
//! repos/<repo>/_links/<hex>       empty: the blob is visible in <repo>
//! repos/<repo>/_manifests/<hex>   "<media type>\n<last pull or push, unix s>\n"
//! repos/<repo>/_tags/<tag>        "sha256:<hex>"
//! uploads/<id>                    an upload in progress
//! tmp/                            files on their way to an atomic rename
//! ```
//!
//! Repository components start with `[a-z0-9]`, so the `_` names cannot clash
//! with a nested repository. The index lives in memory: [`Store::open`]
//! rebuilds it from the tree, and every change is written through. Content
//! lands in `uploads/` or `tmp/`, is hashed as it streams, synced, and renamed
//! into `blobs/` under the index lock, the same lock garbage collection holds
//! while it deletes. A blob is visible in a repository only through a link,
//! so one scope cannot read another's cache by digest.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use ring::digest::{Context, SHA256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::warn;

use super::gc::{self, Budgets, Reason};

/// A `sha256:` digest, the only algorithm served.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(String);

impl Digest {
    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(hex(ring::digest::digest(&SHA256, bytes).as_ref()))
    }

    fn finish(ctx: Context) -> Self {
        Self(hex(ctx.finish().as_ref()))
    }

    fn from_hex(s: &str) -> Option<Self> {
        (s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(s.to_owned()))
    }

    /// The 64 hex characters.
    #[must_use]
    pub fn hex(&self) -> &str {
        &self.0
    }
}

impl FromStr for Digest {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        s.strip_prefix("sha256:").and_then(Self::from_hex).ok_or(())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.0)
    }
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// A manifest reference: a tag or a digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(Digest),
}

impl FromStr for Reference {
    type Err = ();
    /// A digest, or a tag `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`.
    fn from_str(s: &str) -> Result<Self, ()> {
        if s.contains(':') {
            return s.parse().map(Self::Digest);
        }
        let ok = s.len() <= 128
            && s.bytes()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'));
        if ok {
            Ok(Self::Tag(s.to_owned()))
        } else {
            Err(())
        }
    }
}

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("content does not match digest {0}")]
    DigestMismatch(Digest),
    #[error("blob larger than the {0} byte limit")]
    TooLarge(u64),
    #[error("invalid manifest: {0}")]
    ManifestInvalid(String),
    #[error("manifest references {0}, unknown in this repository")]
    ManifestBlobUnknown(Digest),
    #[error("open uploads hold all the upload space")]
    UploadSpace,
    #[error("request body stalled")]
    Stalled,
    #[error("request body: {0}")]
    Body(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One manifest of a repository.
#[derive(Clone, Debug)]
pub(super) struct Manifest {
    pub media_type: String,
    /// Digests it references, sorted, without duplicates.
    pub refs: Vec<Digest>,
    /// Last pull or push, unix seconds.
    pub last_used: u64,
    /// `last_used` as last written to disk.
    pub persisted: u64,
}

/// One repository.
#[derive(Debug, Default)]
pub(super) struct Repo {
    /// Linked blobs and when each link was last created or read (unix
    /// seconds; in memory only, set to the start time on open).
    pub links: HashMap<Digest, u64>,
    pub manifests: HashMap<Digest, Manifest>,
    pub tags: BTreeMap<String, Digest>,
}

/// The in-memory index.
#[derive(Debug, Default)]
pub(super) struct Index {
    /// Blob sizes.
    pub blobs: HashMap<Digest, u64>,
    pub repos: BTreeMap<String, Repo>,
}

/// An upload in progress.
pub struct Upload {
    repo: String,
    /// The worker that opened it: the DNS name of its certificate, nonce
    /// included, so a later pod of the same name starts with a clean quota.
    owner: String,
    file: tokio::fs::File,
    path: PathBuf,
    hasher: Context,
    size: u64,
    touched: Instant,
}

impl Upload {
    /// Bytes received so far.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// A manifest found by [`Store::manifest`].
#[derive(Debug)]
pub struct FoundManifest {
    pub digest: Digest,
    pub media_type: String,
    pub size: u64,
    pub path: PathBuf,
}

/// Totals for the gauges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes: u64,
    pub blobs: u64,
    pub manifests: u64,
}

/// What one garbage collection removed.
#[derive(Debug, Default)]
pub struct GcReport {
    pub evicted_pool: u64,
    pub evicted_volume: u64,
    pub deleted_blobs: u64,
    pub freed_bytes: u64,
}

/// Uploads open at once; more are refused.
pub const MAX_UPLOADS: usize = 1024;
/// Uploads one worker may hold open at once.
pub const MAX_UPLOADS_PER_OWNER: usize = 64;

/// Size bounds of the store.
#[derive(Clone, Copy, Debug)]
pub struct StoreLimits {
    /// Largest blob accepted.
    pub max_blob: u64,
    /// Bytes all open uploads may hold together. One worker may hold a
    /// quarter of it, and never less than one largest blob.
    pub max_upload_bytes: u64,
}

/// Slots and bytes held by open uploads, in total and per owner.
#[derive(Debug, Default)]
struct Quota {
    bytes: u64,
    owners: HashMap<String, (usize, u64)>,
}
/// How often a pulled manifest's last-used time is written back, at most.
const PERSIST_EVERY: u64 = 60;

/// The store.
pub struct Store {
    root: PathBuf,
    limits: StoreLimits,
    index: Mutex<Index>,
    uploads: std::sync::Mutex<HashMap<String, Arc<Mutex<Upload>>>>,
    quota: std::sync::Mutex<Quota>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn random_id() -> io::Result<String> {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| io::Error::other("OS random number generator failed"))?;
    Ok(hex(&b))
}

async fn remove(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %path.display(), error = %e, "removing a cache file"),
    }
}

/// Digests a manifest references: `config`, and the `layers`, `manifests`
/// and `blobs` entries.
pub(super) fn manifest_refs(body: &[u8]) -> Result<(Option<String>, Vec<Digest>), StoreError> {
    let bad = |m: &str| StoreError::ManifestInvalid(m.to_owned());
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| StoreError::ManifestInvalid(e.to_string()))?;
    let obj = v.as_object().ok_or_else(|| bad("not a JSON object"))?;
    let mut refs = Vec::new();
    let mut add = |d: &serde_json::Value| -> Result<(), StoreError> {
        let s = d
            .get("digest")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| bad("descriptor without a digest"))?;
        refs.push(
            s.parse()
                .map_err(|()| StoreError::ManifestInvalid(format!("unsupported digest {s}")))?,
        );
        Ok(())
    };
    if let Some(c) = obj.get("config") {
        add(c)?;
    }
    for key in ["layers", "manifests", "blobs"] {
        if let Some(list) = obj.get(key) {
            for d in list
                .as_array()
                .ok_or_else(|| bad("descriptor list is not an array"))?
            {
                add(d)?;
            }
        }
    }
    refs.sort();
    refs.dedup();
    let media = obj
        .get("mediaType")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok((media, refs))
}

impl Store {
    /// Opens the store at `root`, creating it if needed: clears `tmp/` and
    /// `uploads/` and rebuilds the index. Blocking; run it before serving.
    pub fn open(root: &Path, limits: StoreLimits) -> io::Result<Self> {
        for d in ["tmp", "uploads"] {
            match std::fs::remove_dir_all(root.join(d)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            std::fs::create_dir_all(root.join(d))?;
        }
        std::fs::create_dir_all(root.join("blobs/sha256"))?;
        std::fs::create_dir_all(root.join("repos"))?;
        let mut idx = Index::default();
        for e in std::fs::read_dir(root.join("blobs/sha256"))? {
            let e = e?;
            match e.file_name().to_str().and_then(Digest::from_hex) {
                Some(d) => {
                    idx.blobs.insert(d, e.metadata()?.len());
                }
                None => std::fs::remove_file(e.path())?,
            }
        }
        let store = Self {
            root: root.to_owned(),
            limits,
            index: Mutex::new(Index::default()),
            uploads: std::sync::Mutex::default(),
            quota: std::sync::Mutex::default(),
        };
        store.scan(&root.join("repos"), "", &mut idx, now_secs())?;
        Ok(Self {
            index: Mutex::new(idx),
            ..store
        })
    }

    fn scan(&self, dir: &Path, name: &str, idx: &mut Index, now: u64) -> io::Result<()> {
        let mut repo = Repo::default();
        let entries = |sub: &str| -> io::Result<Vec<(String, PathBuf)>> {
            match std::fs::read_dir(dir.join(sub)) {
                Ok(rd) => rd
                    .map(|e| {
                        let e = e?;
                        Ok((e.file_name().to_string_lossy().into_owned(), e.path()))
                    })
                    .collect(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(e),
            }
        };
        for (f, path) in entries("_links")? {
            match Digest::from_hex(&f).filter(|d| idx.blobs.contains_key(d)) {
                Some(d) => {
                    repo.links.insert(d, now);
                }
                None => std::fs::remove_file(path)?,
            }
        }
        for (f, path) in entries("_manifests")? {
            match self.load_manifest(&f, &path, idx, &repo) {
                Some((d, m)) => {
                    repo.manifests.insert(d, m);
                }
                None => std::fs::remove_file(path)?,
            }
        }
        for (f, path) in entries("_tags")? {
            let d = std::fs::read_to_string(&path)?
                .trim()
                .parse::<Digest>()
                .ok();
            match d.filter(|d| repo.manifests.contains_key(d)) {
                Some(d) => {
                    repo.tags.insert(f, d);
                }
                None => std::fs::remove_file(path)?,
            }
        }
        if !name.is_empty()
            && (!repo.links.is_empty() || !repo.manifests.is_empty() || !repo.tags.is_empty())
        {
            idx.repos.insert(name.to_owned(), repo);
        }
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let f = e.file_name().to_string_lossy().into_owned();
            if e.file_type()?.is_dir() && !f.starts_with('_') {
                let child = if name.is_empty() {
                    f
                } else {
                    format!("{name}/{f}")
                };
                self.scan(&e.path(), &child, idx, now)?;
            }
        }
        Ok(())
    }

    fn load_manifest(
        &self,
        file: &str,
        path: &Path,
        idx: &Index,
        repo: &Repo,
    ) -> Option<(Digest, Manifest)> {
        let d = Digest::from_hex(file).filter(|d| repo.links.contains_key(d))?;
        let record = std::fs::read_to_string(path).ok()?;
        let mut lines = record.lines();
        let media_type = lines.next().filter(|m| !m.is_empty())?.to_owned();
        let last_used = lines.next()?.parse().ok()?;
        idx.blobs.get(&d)?;
        let body = std::fs::read(self.blob_path(&d)).ok()?;
        let (_, refs) = manifest_refs(&body).ok()?;
        Some((
            d,
            Manifest {
                media_type,
                refs,
                last_used,
                persisted: last_used,
            },
        ))
    }

    /// The largest blob accepted.
    #[must_use]
    pub fn max_blob(&self) -> u64 {
        self.limits.max_blob
    }

    fn blob_path(&self, d: &Digest) -> PathBuf {
        self.root.join("blobs/sha256").join(d.hex())
    }

    fn repo_dir(&self, repo: &str) -> PathBuf {
        self.root.join("repos").join(repo)
    }

    /// The blob `d` as seen from `repo`: its path and size. Renews the link.
    pub async fn blob(&self, repo: &str, d: &Digest) -> Option<(PathBuf, u64)> {
        let mut idx = self.index.lock().await;
        let size = *idx.blobs.get(d)?;
        let t = idx.repos.get_mut(repo)?.links.get_mut(d)?;
        *t = now_secs();
        Some((self.blob_path(d), size))
    }

    async fn link(&self, idx: &mut Index, repo: &str, d: &Digest) -> io::Result<()> {
        let r = idx.repos.entry(repo.to_owned()).or_default();
        if !r.links.contains_key(d) {
            let dir = self.repo_dir(repo).join("_links");
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::File::create(dir.join(d.hex())).await?;
        }
        r.links.insert(d.clone(), now_secs());
        Ok(())
    }

    async fn write_tmp(&self, bytes: &[u8]) -> io::Result<PathBuf> {
        let tmp = self.root.join("tmp").join(random_id()?);
        let mut f = tokio::fs::File::create(&tmp).await?;
        let r = async {
            f.write_all(bytes).await?;
            f.flush().await?;
            f.sync_all().await
        }
        .await;
        if let Err(e) = r {
            remove(&tmp).await;
            return Err(e);
        }
        Ok(tmp)
    }

    /// Moves the synced file `tmp` into the store as `d` (or drops it when
    /// `d` is already stored) and links it into `repo`. Holds the index lock.
    async fn commit(
        &self,
        idx: &mut Index,
        repo: &str,
        tmp: &Path,
        d: &Digest,
        size: u64,
    ) -> io::Result<()> {
        if idx.blobs.contains_key(d) {
            remove(tmp).await;
        } else {
            tokio::fs::rename(tmp, self.blob_path(d)).await?;
            idx.blobs.insert(d.clone(), size);
        }
        self.link(idx, repo, d).await
    }

    /// Starts an upload into `repo` for `owner`. `None` when [`MAX_UPLOADS`]
    /// are open, or [`MAX_UPLOADS_PER_OWNER`] of the owner's.
    pub async fn start_upload(&self, repo: &str, owner: &str) -> io::Result<Option<String>> {
        let id = random_id()?;
        let path = self.root.join("uploads").join(&id);
        let file = tokio::fs::File::create(&path).await?;
        let up = Upload {
            repo: repo.to_owned(),
            owner: owner.to_owned(),
            file,
            path: path.clone(),
            hasher: Context::new(&SHA256),
            size: 0,
            touched: Instant::now(),
        };
        let full = {
            let mut uploads = self.uploads();
            let mut q = self.quota();
            let slots = &mut q.owners.entry(owner.to_owned()).or_default().0;
            let full = uploads.len() >= MAX_UPLOADS || *slots >= MAX_UPLOADS_PER_OWNER;
            if !full {
                *slots += 1;
                uploads.insert(id.clone(), Arc::new(Mutex::new(up)));
            }
            full
        };
        if full {
            self.release(owner, 0, 0);
            remove(&path).await;
            return Ok(None);
        }
        Ok(Some(id))
    }

    fn uploads(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Mutex<Upload>>>> {
        self.uploads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn quota(&self) -> std::sync::MutexGuard<'_, Quota> {
        self.quota
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn owner_bytes(&self) -> u64 {
        let l = self.limits;
        (l.max_upload_bytes / 4)
            .max(l.max_blob)
            .min(l.max_upload_bytes)
    }

    /// Takes `n` more upload bytes for `owner`, or fails when the total or
    /// the owner's share would pass its bound.
    fn reserve(&self, owner: &str, n: u64) -> Result<(), StoreError> {
        let max_owner = self.owner_bytes();
        let mut q = self.quota();
        let held = q.owners.get(owner).map_or(0, |o| o.1);
        if q.bytes.saturating_add(n) > self.limits.max_upload_bytes
            || held.saturating_add(n) > max_owner
        {
            return Err(StoreError::UploadSpace);
        }
        q.bytes += n;
        q.owners.entry(owner.to_owned()).or_default().1 += n;
        Ok(())
    }

    /// Gives back `slots` uploads and `bytes` of `owner`.
    fn release(&self, owner: &str, slots: usize, bytes: u64) {
        let mut q = self.quota();
        q.bytes = q.bytes.saturating_sub(bytes);
        if let Some(o) = q.owners.get_mut(owner) {
            o.0 = o.0.saturating_sub(slots);
            o.1 = o.1.saturating_sub(bytes);
            if *o == (0, 0) {
                q.owners.remove(owner);
            }
        }
    }

    /// The upload `id` of `repo`, locked for this request. `Err(())` when
    /// another request holds it, `Ok(None)` when it does not exist.
    #[allow(clippy::result_unit_err)]
    pub fn upload(&self, repo: &str, id: &str) -> Result<Option<OwnedMutexGuard<Upload>>, ()> {
        let Some(slot) = self.uploads().get(id).cloned() else {
            return Ok(None);
        };
        let up = slot.try_lock_owned().map_err(|_| ())?;
        Ok((up.repo == repo).then_some(up))
    }

    /// Ends the upload `id` and deletes what it received.
    pub async fn abort(&self, id: &str, up: OwnedMutexGuard<Upload>) {
        self.uploads().remove(id);
        self.release(&up.owner, 1, up.size);
        let path = up.path.clone();
        drop(up);
        remove(&path).await;
    }

    /// Appends `body` to the upload, hashing as it streams. A gap of `idle`
    /// between chunks fails with [`StoreError::Stalled`]. On error the caller
    /// aborts the upload.
    pub async fn append<S, E>(
        &self,
        up: &mut Upload,
        mut body: S,
        idle: Duration,
    ) -> Result<(), StoreError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: fmt::Display,
    {
        up.touched = Instant::now();
        loop {
            let chunk = match tokio::time::timeout(idle, body.next()).await {
                Err(_) => return Err(StoreError::Stalled),
                Ok(None) => return Ok(()),
                Ok(Some(Err(e))) => return Err(StoreError::Body(e.to_string())),
                Ok(Some(Ok(c))) => c,
            };
            let size = up.size.saturating_add(chunk.len() as u64);
            if size > self.limits.max_blob {
                return Err(StoreError::TooLarge(self.limits.max_blob));
            }
            let n = chunk.len() as u64;
            self.reserve(&up.owner, n)?;
            if let Err(e) = up.file.write_all(&chunk).await {
                self.release(&up.owner, 0, n);
                return Err(e.into());
            }
            up.hasher.update(&chunk);
            up.size = size;
            up.touched = Instant::now();
        }
    }

    /// Ends the upload `id` as blob `want`: verifies the digest, syncs, and
    /// commits it into its repository. The upload is gone either way.
    pub async fn finish(
        &self,
        id: &str,
        mut up: OwnedMutexGuard<Upload>,
        want: &Digest,
    ) -> Result<(), StoreError> {
        self.uploads().remove(id);
        self.release(&up.owner, 1, up.size);
        let got = Digest::finish(up.hasher.clone());
        let synced = async {
            up.file.flush().await?;
            up.file.sync_all().await
        }
        .await;
        let (path, size, repo) = (up.path.clone(), up.size, up.repo.clone());
        drop(up);
        if &got != want {
            remove(&path).await;
            return Err(StoreError::DigestMismatch(want.clone()));
        }
        if let Err(e) = synced {
            remove(&path).await;
            return Err(e.into());
        }
        let mut idx = self.index.lock().await;
        let r = self.commit(&mut idx, &repo, &path, want, size).await;
        drop(idx);
        if r.is_err() {
            remove(&path).await;
        }
        Ok(r?)
    }

    /// Stores a manifest and, for a tag, points the tag at it. Every blob it
    /// references must already be linked in `repo`. The files are written and
    /// synced before the index lock and only renamed under it.
    pub async fn put_manifest(
        &self,
        repo: &str,
        reference: &Reference,
        media_type: Option<&str>,
        body: &[u8],
    ) -> Result<Digest, StoreError> {
        let d = Digest::of(body);
        if let Reference::Digest(want) = reference
            && *want != d
        {
            return Err(StoreError::DigestMismatch(want.clone()));
        }
        let (field, refs) = manifest_refs(body)?;
        let media_type = media_type
            .map(str::to_owned)
            .or(field)
            .ok_or_else(|| StoreError::ManifestInvalid("no media type".to_owned()))?;
        if media_type.contains('\n') {
            return Err(StoreError::ManifestInvalid("bad media type".to_owned()));
        }
        let now = now_secs();
        let dir = self.repo_dir(repo);
        // Each temporary file that is not renamed into place is removed below.
        let mut tmps: Vec<PathBuf> = Vec::new();
        let r: Result<(), StoreError> = async {
            tmps.push(self.write_tmp(body).await?);
            tmps.push(
                self.write_tmp(format!("{media_type}\n{now}\n").as_bytes())
                    .await?,
            );
            let mut places = vec![dir.join("_manifests").join(d.hex())];
            if let Reference::Tag(t) = reference {
                tmps.push(self.write_tmp(d.to_string().as_bytes()).await?);
                places.push(dir.join("_tags").join(t));
            }
            let mut idx = self.index.lock().await;
            let existing = idx.repos.get(repo);
            if let Some(r) = refs.iter().find(|r| {
                !idx.blobs.contains_key(*r) || !existing.is_some_and(|x| x.links.contains_key(*r))
            }) {
                return Err(StoreError::ManifestBlobUnknown(r.clone()));
            }
            self.commit(&mut idx, repo, &tmps[0], &d, body.len() as u64)
                .await?;
            for (tmp, path) in tmps[1..].iter().zip(&places) {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::rename(tmp, path).await?;
            }
            let r = idx.repos.entry(repo.to_owned()).or_default();
            r.manifests.insert(
                d.clone(),
                Manifest {
                    media_type,
                    refs,
                    last_used: now,
                    persisted: now,
                },
            );
            if let Reference::Tag(t) = reference {
                r.tags.insert(t.clone(), d.clone());
            }
            Ok(())
        }
        .await;
        for tmp in &tmps {
            remove(tmp).await;
        }
        r.map(|()| d)
    }

    /// The manifest `reference` of `repo`. Records the pull, and writes it to
    /// the manifest's record at most every [`PERSIST_EVERY`]: outside the
    /// index lock and best effort, so a full disk never fails a read.
    pub async fn manifest(&self, repo: &str, reference: &Reference) -> Option<FoundManifest> {
        let (found, due) = {
            let mut idx = self.index.lock().await;
            let Index { blobs, repos } = &mut *idx;
            let r = repos.get_mut(repo)?;
            let d = match reference {
                Reference::Tag(t) => r.tags.get(t)?.clone(),
                Reference::Digest(d) => d.clone(),
            };
            let size = *blobs.get(&d)?;
            let m = r.manifests.get_mut(&d)?;
            let now = now_secs();
            m.last_used = now;
            // A failed write is tried again after another PERSIST_EVERY.
            let due = now.saturating_sub(m.persisted) >= PERSIST_EVERY;
            if due {
                m.persisted = now;
            }
            let found = FoundManifest {
                media_type: m.media_type.clone(),
                size,
                path: self.blob_path(&d),
                digest: d,
            };
            (found, due.then_some(now))
        };
        if let Some(now) = due
            && let Err(e) = self.persist(repo, &found, now).await
        {
            warn!(repository = repo, digest = %found.digest, error = %e, "recording a manifest pull");
        }
        Some(found)
    }

    /// Writes the pull time `now` into the record of `found`, unless garbage
    /// collection has evicted it meanwhile.
    async fn persist(&self, repo: &str, found: &FoundManifest, now: u64) -> io::Result<()> {
        let record = format!("{}\n{now}\n", found.media_type);
        let tmp = self.write_tmp(record.as_bytes()).await?;
        let idx = self.index.lock().await;
        let r = if idx
            .repos
            .get(repo)
            .is_some_and(|r| r.manifests.contains_key(&found.digest))
        {
            let path = self
                .repo_dir(repo)
                .join("_manifests")
                .join(found.digest.hex());
            tokio::fs::rename(&tmp, path).await
        } else {
            Ok(())
        };
        drop(idx);
        remove(&tmp).await;
        r
    }

    /// Deletes uploads idle for `max_age` that no request holds. Returns how many.
    pub async fn expire_uploads(&self, max_age: Duration) -> usize {
        let mut gone = Vec::new();
        self.uploads().retain(|_, slot| match slot.try_lock() {
            Ok(up) if up.touched.elapsed() >= max_age => {
                self.release(&up.owner, 1, up.size);
                gone.push(up.path.clone());
                false
            }
            _ => true,
        });
        for p in &gone {
            remove(p).await;
        }
        gone.len()
    }

    /// Runs one garbage collection (see [`gc`]). Blobs are moved aside under
    /// the index lock and deleted after it is released, so deleting large
    /// files does not hold up requests.
    pub async fn gc(&self, budgets: &dyn Budgets, grace: Duration) -> GcReport {
        let mut idx = self.index.lock().await;
        let plan = gc::plan(&idx, budgets, now_secs(), grace.as_secs());
        let mut report = GcReport::default();
        for (name, d, reason) in &plan.evict {
            let dir = self.repo_dir(name);
            remove(&dir.join("_manifests").join(d.hex())).await;
            let Some(repo) = idx.repos.get_mut(name) else {
                continue;
            };
            repo.manifests.remove(d);
            let tags: Vec<String> = repo
                .tags
                .iter()
                .filter(|(_, t)| *t == d)
                .map(|(k, _)| k.clone())
                .collect();
            for t in tags {
                remove(&dir.join("_tags").join(&t)).await;
                repo.tags.remove(&t);
            }
            match reason {
                Reason::Pool => report.evicted_pool += 1,
                Reason::Volume => report.evicted_volume += 1,
            }
        }
        for (name, d) in &plan.unlink {
            remove(&self.repo_dir(name).join("_links").join(d.hex())).await;
            if let Some(repo) = idx.repos.get_mut(name) {
                repo.links.remove(d);
            }
        }
        let mut doomed = Vec::new();
        for d in &plan.delete {
            let Some(size) = idx.blobs.remove(d) else {
                continue;
            };
            report.deleted_blobs += 1;
            report.freed_bytes += size;
            let aside = self.root.join("tmp").join(format!("gc-{}", d.hex()));
            match tokio::fs::rename(self.blob_path(d), &aside).await {
                Ok(()) => doomed.push(aside),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(digest = %d, error = %e, "moving a cache blob aside; deleting it in place");
                    remove(&self.blob_path(d)).await;
                }
            }
        }
        idx.repos
            .retain(|_, r| !(r.links.is_empty() && r.manifests.is_empty() && r.tags.is_empty()));
        drop(idx);
        for p in &doomed {
            remove(p).await;
        }
        report
    }

    /// Totals for the gauges.
    pub async fn stats(&self) -> Stats {
        let idx = self.index.lock().await;
        Stats {
            bytes: idx.blobs.values().sum(),
            blobs: idx.blobs.len() as u64,
            manifests: idx.repos.values().map(|r| r.manifests.len() as u64).sum(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Recording a pull is bookkeeping: when the disk refuses the write, the
    /// manifest is still served.
    #[tokio::test]
    async fn a_failed_pull_record_does_not_fail_the_read() {
        let root = std::env::temp_dir().join(format!("bk-store-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let limits = StoreLimits {
            max_blob: 1 << 20,
            max_upload_bytes: 4 << 20,
        };
        let store = Store::open(&root, limits).unwrap();
        let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
        let tag = Reference::Tag("t".into());
        let d = store
            .put_manifest("ns/p/app", &tag, None, body)
            .await
            .unwrap();
        store
            .index
            .lock()
            .await
            .repos
            .get_mut("ns/p/app")
            .unwrap()
            .manifests
            .get_mut(&d)
            .unwrap()
            .persisted = 0;
        // No temporary file can be created any more.
        std::fs::remove_dir_all(root.join("tmp")).unwrap();
        std::fs::write(root.join("tmp"), b"").unwrap();
        let found = store.manifest("ns/p/app", &tag).await.unwrap();
        assert_eq!(found.digest, d);
        std::fs::remove_dir_all(&root).ok();
    }
}
