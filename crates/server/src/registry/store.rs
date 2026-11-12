//! The content store: one object store, content addressed, with the index in
//! memory.
//!
//! Every object has a key of the [`layout`]. No key is written twice: each
//! write goes to a fresh id, and a key is deleted only after the index has
//! stopped naming it, so no write can race a delete of the same key. Store
//! I/O happens outside the index lock, a [`std::sync::Mutex`] that no await
//! may cross. Deletes run after the lock is released; one that fails goes on
//! the store's one retry list, which the next collection deletes first.
//!
//! A blob is visible in a repository only through a link, so one scope
//! cannot read another's cache by digest. Links live in memory: [`Store::open`]
//! rebuilds them from the manifest records, which carry each manifest's
//! references, so the open never reads a manifest body or a tag.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io;
use std::str::FromStr;
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{GetOptions, MultipartUpload, ObjectStore, ObjectStoreExt, PutPayload};
use ring::digest::{Context, SHA256};
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tracing::warn;

use super::gc::{self, Budgets, Reason};
use super::layout::{self, Id, Key};

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

    pub(super) fn from_hex(s: &str) -> Option<Self> {
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

pub(super) fn hex(b: &[u8]) -> String {
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
    #[error("uploads hold every part buffer; try again")]
    PartsBusy,
    #[error("request body stalled")]
    Stalled,
    #[error("request body: {0}")]
    Body(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Store(#[from] object_store::Error),
}

/// One stored blob: its size and the id of its data object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Blob {
    pub size: u64,
    pub id: Id,
}

/// One manifest of a repository.
#[derive(Clone, Debug)]
pub(super) struct Manifest {
    pub media_type: String,
    /// Digests it references, sorted, without duplicates.
    pub refs: Vec<Digest>,
    /// Last pull or push, unix seconds.
    pub last_used: u64,
    /// `last_used` as its stored record holds it.
    pub persisted: u64,
    /// The id of its record's key.
    pub record: Id,
}

impl Manifest {
    /// The record: media type, last-used time, one line per reference.
    fn record_text(&self) -> String {
        use std::fmt::Write as _;
        let head = format!("{}\n{}\n", self.media_type, self.last_used);
        self.refs.iter().fold(head, |mut s, r| {
            let _ = writeln!(s, "{r}");
            s
        })
    }

    fn parse_record(text: &str, record: Id) -> Option<Self> {
        let mut lines = text.lines();
        let media_type = lines.next().filter(|m| !m.is_empty())?.to_owned();
        let last_used = lines.next()?.parse().ok()?;
        let mut refs = lines
            .map(|l| l.parse().ok())
            .collect::<Option<Vec<Digest>>>()?;
        refs.sort();
        refs.dedup();
        Some(Self {
            media_type,
            refs,
            last_used,
            persisted: last_used,
            record,
        })
    }
}

/// A tag: the manifest it names and the id of its marker.
#[derive(Clone, Debug)]
pub(super) struct Tag {
    pub digest: Digest,
    pub id: Id,
}

/// One repository.
#[derive(Debug, Default)]
pub(super) struct Repo {
    /// Linked blobs and when each link was last created or read (unix
    /// seconds; in memory only, set to the start time on open).
    pub links: HashMap<Digest, u64>,
    pub manifests: HashMap<Digest, Manifest>,
    pub tags: BTreeMap<String, Tag>,
}

/// The in-memory index.
#[derive(Debug, Default)]
pub(super) struct Index {
    pub blobs: HashMap<Digest, Blob>,
    pub repos: BTreeMap<String, Repo>,
}

/// An upload in progress. Its bytes go to `data/<id>`: on the volume as one
/// part per received chunk, on a bucket through a part buffer of
/// [`PART_SIZE`], or as one put if they never fill it.
pub struct Upload {
    repo: String,
    /// The worker that opened it: the DNS name of its certificate, nonce
    /// included, so a later pod of the same name starts with a clean quota.
    owner: String,
    id: Id,
    part: Option<Box<dyn MultipartUpload>>,
    /// Bytes not yet sent as a part. Holds its own copy, never a network
    /// buffer.
    buf: Vec<u8>,
    /// The part permit, held from the first byte to the end of the upload.
    permit: Option<OwnedSemaphorePermit>,
    hasher: Context,
    size: u64,
    /// When a request last wrote to it.
    touched: Instant,
}

impl Upload {
    /// Bytes received so far.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// A blob found by [`Store::blob`]: its size, and what [`Store::read`] needs.
#[derive(Debug)]
pub struct Found {
    pub size: u64,
    id: Id,
}

/// A manifest found by [`Store::manifest`].
#[derive(Debug)]
pub struct FoundManifest {
    pub digest: Digest,
    pub media_type: String,
    pub blob: Found,
}

/// A body read from the store.
pub type Body = BoxStream<'static, Result<Bytes, object_store::Error>>;

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
/// Record reads at open, and record writes in a collection, in flight at once.
const RECORD_IO: usize = 32;
/// Part buffers of uploads to a bucket, over all uploads.
pub const PART_PERMITS: usize = 48;
/// Part buffers one owner may hold: a quarter, as with upload bytes.
pub const PART_PERMITS_PER_OWNER: usize = PART_PERMITS / 4;
/// A part: the smallest S3 and the GCS XML API take, except the last.
pub const PART_SIZE: usize = 5 << 20;
/// An upload that holds a part permit and has seen no request for this long
/// is aborted when a permit is wanted, and at each collection. Twice the
/// longest gap within a request body.
pub const PART_IDLE: Duration = Duration::from_mins(2);

/// How the store takes the parts of an upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parts {
    /// Each chunk as it arrives: the volume, whose parts have no minimum.
    Direct,
    /// Through a part buffer of [`PART_SIZE`], one of [`PART_PERMITS`]: a
    /// bucket.
    Buffered,
}

/// Size bounds of the store.
#[derive(Clone, Copy, Debug)]
pub struct StoreLimits {
    /// Largest blob accepted.
    pub max_blob: u64,
    /// Bytes all open uploads may hold together. One worker may hold a
    /// quarter of it, and never less than one largest blob.
    pub max_upload_bytes: u64,
}

/// What one owner's open uploads hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Held {
    slots: usize,
    bytes: u64,
    permits: usize,
}

/// Slots, bytes and part permits held by open uploads, in total and per
/// owner.
#[derive(Debug, Default)]
struct Quota {
    bytes: u64,
    owners: HashMap<String, Held>,
}

type Slot = Arc<tokio::sync::Mutex<Upload>>;

/// The store.
pub struct Store {
    os: Arc<dyn ObjectStore>,
    limits: StoreLimits,
    parts: Parts,
    /// [`PART_PERMITS`], taken only with [`Parts::Buffered`].
    permits: Arc<Semaphore>,
    index: std::sync::Mutex<Index>,
    uploads: std::sync::Mutex<HashMap<String, Slot>>,
    quota: std::sync::Mutex<Quota>,
    /// Keys the index no longer names whose delete failed.
    retry: std::sync::Mutex<Vec<Path>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn lock<T>(m: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_not_found(e: &object_store::Error) -> bool {
    matches!(e, object_store::Error::NotFound { .. })
}

async fn abort_part(mut part: Box<dyn MultipartUpload>) {
    if let Err(e) = part.abort().await {
        warn!(error = %e, "aborting a cache upload");
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

/// Checks that every ref is a stored blob linked in `repo` or in `fallback`,
/// and returns those linked only in `fallback`.
fn borrowed(
    idx: &Index,
    repo: &str,
    fallback: Option<&str>,
    refs: &[Digest],
) -> Result<Vec<Digest>, StoreError> {
    let linked = |name: Option<&str>, r: &Digest| {
        name.and_then(|n| idx.repos.get(n))
            .is_some_and(|x| x.links.contains_key(r))
    };
    let mut out = Vec::new();
    for r in refs {
        let here = linked(Some(repo), r);
        if !idx.blobs.contains_key(r) || !(here || linked(fallback, r)) {
            return Err(StoreError::ManifestBlobUnknown(r.clone()));
        }
        if !here {
            out.push(r.clone());
        }
    }
    Ok(out)
}

fn link(idx: &mut Index, repo: &str, d: &Digest) {
    idx.repos
        .entry(repo.to_owned())
        .or_default()
        .links
        .insert(d.clone(), now_secs());
}

/// Installs `blob` as `d` (or keeps the stored one, returning the keys of
/// `blob` to delete) and links it into `repo`.
fn commit(idx: &mut Index, repo: &str, d: &Digest, blob: Blob) -> Vec<Path> {
    let doomed = if idx.blobs.contains_key(d) {
        vec![
            Key::Marker(d.clone(), blob.id.clone()).path(),
            Key::Data(blob.id).path(),
        ]
    } else {
        idx.blobs.insert(d.clone(), blob);
        Vec::new()
    };
    link(idx, repo, d);
    doomed
}

/// Installs manifest `m` of `repo` with its body `blob`, links the refs it
/// `borrowed`, and points `tag` at it. Returns the keys it replaced.
fn install(
    idx: &mut Index,
    repo: &str,
    d: &Digest,
    blob: Blob,
    borrowed: &[Digest],
    m: Manifest,
    tag: Option<(String, Id)>,
) -> Vec<Path> {
    let mut doomed = commit(idx, repo, d, blob);
    for r in borrowed {
        link(idx, repo, r);
    }
    let r = idx.repos.entry(repo.to_owned()).or_default();
    if let Some(old) = r.manifests.insert(d.clone(), m) {
        let key = Key::Record {
            repo: repo.to_owned(),
            digest: d.clone(),
            id: old.record,
        };
        doomed.push(key.path());
    }
    if let Some((t, id)) = tag {
        let new = Tag {
            digest: d.clone(),
            id,
        };
        if let Some(old) = r.tags.insert(t.clone(), new) {
            let key = Key::Tag {
                repo: repo.to_owned(),
                tag: t,
                digest: old.digest,
                id: old.id,
            };
            doomed.push(key.path());
        }
    }
    doomed
}

/// Keys found by the listings at open, sorted by what they hold.
#[derive(Default)]
struct Listed {
    data: HashMap<Id, u64>,
    markers: Vec<(Digest, Id, Path)>,
    records: Vec<(String, Digest, Manifest)>,
    tags: Vec<(String, String, Tag, (i64, u32))>,
    doomed: Vec<Path>,
}

impl Store {
    /// Opens the store on `os` and rebuilds the index from three listings
    /// and one read per manifest record. Keys that do not belong are queued
    /// for deletion, which the first collection performs, so they do not
    /// hold up serving.
    pub async fn open(
        os: Arc<dyn ObjectStore>,
        limits: StoreLimits,
        parts: Parts,
    ) -> Result<Self, StoreError> {
        let mut l = Self::list(&*os).await?;
        let now = now_secs();
        let mut idx = Index::default();

        // Markers with data are blobs; the first marker of a digest wins.
        let mut named = HashSet::new();
        for (d, id, path) in l.markers {
            match l.data.get(&id) {
                Some(&size) if !idx.blobs.contains_key(&d) && named.insert(id.clone()) => {
                    idx.blobs.insert(d, Blob { size, id });
                }
                _ => l.doomed.push(path),
            }
        }
        l.doomed.extend(
            l.data
                .into_keys()
                .filter(|id| !named.contains(id))
                .map(|id| Key::Data(id).path()),
        );

        // Records of stored manifests; of two for one manifest, the newer.
        for (repo, d, m) in l.records {
            let record = |id: &Id| {
                Key::Record {
                    repo: repo.clone(),
                    digest: d.clone(),
                    id: id.clone(),
                }
                .path()
            };
            if !idx.blobs.contains_key(&d) {
                l.doomed.push(record(&m.record));
                continue;
            }
            let r = idx.repos.entry(repo.clone()).or_default();
            match r.manifests.get(&d) {
                Some(old) if old.last_used >= m.last_used => l.doomed.push(record(&m.record)),
                _ => {
                    if let Some(old) = r.manifests.insert(d.clone(), m) {
                        l.doomed.push(record(&old.record));
                    }
                }
            }
        }
        // Links: every manifest and each of its refs that is stored.
        let Index { blobs, repos } = &mut idx;
        for r in repos.values_mut() {
            let reached: Vec<Digest> = r
                .manifests
                .iter()
                .flat_map(|(d, m)| std::iter::once(d).chain(&m.refs))
                .filter(|d| blobs.contains_key(*d))
                .cloned()
                .collect();
            r.links.extend(reached.into_iter().map(|d| (d, now)));
        }

        // Tags whose manifest is in the repository; of several for one tag,
        // the newest marker, then the newer manifest, then the digest.
        l.tags.sort_by(|a, b| {
            let used = |x: &(String, String, Tag, (i64, u32))| {
                idx.repos
                    .get(&x.0)
                    .and_then(|r| r.manifests.get(&x.2.digest))
                    .map(|m| m.last_used)
            };
            (&a.0, &a.1, a.3, used(a), &a.2.digest).cmp(&(&b.0, &b.1, b.3, used(b), &b.2.digest))
        });
        for (repo, tag, t, _) in l.tags {
            let key = Key::Tag {
                repo: repo.clone(),
                tag: tag.clone(),
                digest: t.digest.clone(),
                id: t.id.clone(),
            };
            match idx.repos.get_mut(&repo) {
                Some(r) if r.manifests.contains_key(&t.digest) => {
                    // Sorted oldest first: each later marker replaces the one before.
                    if let Some(old) = r.tags.insert(tag.clone(), t) {
                        l.doomed.push(
                            Key::Tag {
                                repo,
                                tag,
                                digest: old.digest,
                                id: old.id,
                            }
                            .path(),
                        );
                    }
                }
                _ => l.doomed.push(key.path()),
            }
        }

        Ok(Self {
            os,
            limits,
            parts,
            permits: Arc::new(Semaphore::new(PART_PERMITS)),
            index: std::sync::Mutex::new(idx),
            uploads: std::sync::Mutex::default(),
            quota: std::sync::Mutex::default(),
            retry: std::sync::Mutex::new(l.doomed),
        })
    }

    /// Lists `data/`, `blobs/` and `repos/` at once, and reads each record
    /// as the `repos/` listing yields it.
    async fn list(os: &dyn ObjectStore) -> Result<Listed, StoreError> {
        let data = os
            .list(Some(&Path::from(layout::DATA)))
            .try_collect::<Vec<_>>();
        let blobs = os
            .list(Some(&Path::from(layout::BLOBS)))
            .try_collect::<Vec<_>>();
        let mut tags = Vec::new();
        let mut doomed = Vec::new();
        let records = os
            .list(Some(&Path::from(layout::REPOS)))
            .try_filter_map(|meta| {
                let out = match Key::parse(meta.location.as_ref()) {
                    Some(Key::Record { repo, digest, id }) => {
                        Some((repo, digest, id, meta.location))
                    }
                    Some(Key::Tag {
                        repo,
                        tag,
                        digest,
                        id,
                    }) => {
                        let t = meta.last_modified;
                        let at = (t.timestamp(), t.timestamp_subsec_nanos());
                        tags.push((repo, tag, Tag { digest, id }, at));
                        None
                    }
                    _ => {
                        doomed.push(meta.location);
                        None
                    }
                };
                futures::future::ready(Ok(out))
            })
            .map_ok(|(repo, digest, id, path)| async move {
                let text = match os.get(&path).await {
                    Ok(r) => r.bytes().await,
                    Err(e) => Err(e),
                };
                match text {
                    Ok(b) => Ok((repo, digest, id, path, Some(b))),
                    Err(e) if is_not_found(&e) => Ok((repo, digest, id, path, None)),
                    Err(e) => Err(e),
                }
            })
            .try_buffer_unordered(RECORD_IO)
            .try_collect::<Vec<_>>();
        let (data, blobs, records) = futures::try_join!(data, blobs, records)?;

        let mut l = Listed {
            tags,
            doomed,
            ..Listed::default()
        };
        for meta in data {
            match Key::parse(meta.location.as_ref()) {
                Some(Key::Data(id)) => {
                    l.data.insert(id, meta.size);
                }
                _ => l.doomed.push(meta.location),
            }
        }
        for meta in blobs {
            match Key::parse(meta.location.as_ref()) {
                Some(Key::Marker(d, id)) => l.markers.push((d, id, meta.location)),
                _ => l.doomed.push(meta.location),
            }
        }
        for (repo, d, id, path, body) in records {
            let m = body
                .as_deref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .and_then(|t| Manifest::parse_record(t, id));
            match m {
                Some(m) => l.records.push((repo, d, m)),
                None => l.doomed.push(path),
            }
        }
        Ok(l)
    }

    /// The largest blob accepted.
    #[must_use]
    pub fn max_blob(&self) -> u64 {
        self.limits.max_blob
    }

    fn index(&self) -> MutexGuard<'_, Index> {
        lock(&self.index)
    }

    fn uploads(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        lock(&self.uploads)
    }

    fn quota(&self) -> MutexGuard<'_, Quota> {
        lock(&self.quota)
    }

    /// Deletes `keys`. When the store fails any of them, every key it did not
    /// confirm goes on the retry list; a key already gone counts as deleted.
    async fn delete(&self, keys: Vec<Path>) {
        if keys.is_empty() {
            return;
        }
        let sent = keys.clone();
        let mut done = HashSet::new();
        let mut failed = false;
        let mut results = self
            .os
            .delete_stream(stream::iter(keys.into_iter().map(Ok)).boxed());
        while let Some(r) = results.next().await {
            match r {
                Ok(p) => {
                    done.insert(p);
                }
                Err(e) if is_not_found(&e) => {}
                Err(e) => {
                    warn!(error = %e, "deleting a cache object");
                    failed = true;
                }
            }
        }
        if failed {
            lock(&self.retry).extend(sent.into_iter().filter(|p| !done.contains(p)));
        }
    }

    /// Deletes the keys on the retry list.
    async fn retry_deletes(&self) {
        let keys = std::mem::take(&mut *lock(&self.retry));
        self.delete(keys).await;
    }

    /// The blob `d` as seen from `repo`. Renews the link.
    pub fn blob(&self, repo: &str, d: &Digest) -> Option<Found> {
        let mut idx = self.index();
        let Index { blobs, repos } = &mut *idx;
        let b = blobs.get(d)?;
        *repos.get_mut(repo)?.links.get_mut(d)? = now_secs();
        Some(Found {
            size: b.size,
            id: b.id.clone(),
        })
    }

    /// The bytes of `found`, or the inclusive `range` of them. `None` when
    /// garbage collection deleted the blob after it was found.
    pub async fn read(
        &self,
        found: &Found,
        range: Option<(u64, u64)>,
    ) -> Result<Option<Body>, StoreError> {
        let opts = GetOptions::default().with_range(range.map(|(s, e)| s..e + 1));
        match self
            .os
            .get_opts(&Key::Data(found.id.clone()).path(), opts)
            .await
        {
            Ok(r) => Ok(Some(r.into_stream())),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Starts an upload into `repo` for `owner`. `None` when [`MAX_UPLOADS`]
    /// are open, or [`MAX_UPLOADS_PER_OWNER`] of the owner's.
    pub fn start_upload(&self, repo: &str, owner: &str) -> io::Result<Option<String>> {
        let id = Id::random()?;
        let up = Upload {
            repo: repo.to_owned(),
            owner: owner.to_owned(),
            id: id.clone(),
            part: None,
            buf: Vec::new(),
            permit: None,
            hasher: Context::new(&SHA256),
            size: 0,
            touched: Instant::now(),
        };
        let mut uploads = self.uploads();
        let mut q = self.quota();
        let held = q.owners.entry(owner.to_owned()).or_default();
        if uploads.len() >= MAX_UPLOADS || held.slots >= MAX_UPLOADS_PER_OWNER {
            if *held == Held::default() {
                q.owners.remove(owner);
            }
            return Ok(None);
        }
        held.slots += 1;
        let id = id.as_str().to_owned();
        uploads.insert(id.clone(), Arc::new(tokio::sync::Mutex::new(up)));
        Ok(Some(id))
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
        let held = q.owners.get(owner).map_or(0, |o| o.bytes);
        if q.bytes.saturating_add(n) > self.limits.max_upload_bytes
            || held.saturating_add(n) > max_owner
        {
            return Err(StoreError::UploadSpace);
        }
        q.bytes += n;
        q.owners.entry(owner.to_owned()).or_default().bytes += n;
        Ok(())
    }

    /// Gives back what `owner` held: `slots` uploads, `bytes` and `permits`.
    fn release(&self, owner: &str, slots: usize, bytes: u64, permits: usize) {
        let mut q = self.quota();
        q.bytes = q.bytes.saturating_sub(bytes);
        if let Some(o) = q.owners.get_mut(owner) {
            o.slots = o.slots.saturating_sub(slots);
            o.bytes = o.bytes.saturating_sub(bytes);
            o.permits = o.permits.saturating_sub(permits);
            if *o == Held::default() {
                q.owners.remove(owner);
            }
        }
    }

    /// Ends `up`: gives back its slot, bytes and permit, drops its buffer,
    /// and returns its multipart upload for the caller to abort or complete.
    fn end(&self, up: &mut Upload) -> Option<Box<dyn MultipartUpload>> {
        let permits = usize::from(up.permit.take().is_some());
        up.buf = Vec::new();
        self.release(&up.owner, 1, up.size, permits);
        up.part.take()
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
    pub async fn abort(&self, id: &str, mut up: OwnedMutexGuard<Upload>) {
        self.uploads().remove(id);
        let part = self.end(&mut up);
        drop(up);
        if let Some(p) = part {
            abort_part(p).await;
        }
    }

    /// Gives `up` a part permit before a request writes to it, unless the
    /// store takes chunks directly, the request declares an empty body, or
    /// `up` holds one. With none free, it first aborts the uploads that hold
    /// one and have been idle for [`PART_IDLE`]. Fails with
    /// [`StoreError::PartsBusy`] when still none is free, or when the owner
    /// holds [`PART_PERMITS_PER_OWNER`]; the upload stays as it was.
    pub async fn take_permit(&self, up: &mut Upload, empty: bool) -> Result<(), StoreError> {
        if self.parts == Parts::Direct || empty || up.permit.is_some() {
            return Ok(());
        }
        if self.try_permit(up) {
            return Ok(());
        }
        self.expire_where(|u| u.permit.is_some() && u.touched.elapsed() >= PART_IDLE)
            .await;
        if self.try_permit(up) {
            Ok(())
        } else {
            Err(StoreError::PartsBusy)
        }
    }

    fn try_permit(&self, up: &mut Upload) -> bool {
        let mut q = self.quota();
        let held = q.owners.entry(up.owner.clone()).or_default();
        if held.permits >= PART_PERMITS_PER_OWNER {
            return false;
        }
        let Ok(p) = Arc::clone(&self.permits).try_acquire_owned() else {
            return false;
        };
        held.permits += 1;
        up.permit = Some(p);
        true
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
            if chunk.is_empty() {
                continue;
            }
            let size = up.size.saturating_add(chunk.len() as u64);
            if size > self.limits.max_blob {
                return Err(StoreError::TooLarge(self.limits.max_blob));
            }
            let n = chunk.len() as u64;
            self.reserve(&up.owner, n)?;
            if let Err(e) = self.write(up, &chunk).await {
                self.release(&up.owner, 0, n, 0);
                return Err(e);
            }
            up.hasher.update(&chunk);
            up.size = size;
            up.touched = Instant::now();
        }
    }

    /// Writes `chunk`: on the volume as one part; on a bucket into the part
    /// buffer, sending each full part and waiting for it before more of the
    /// body is read, so an upload holds one buffer at most.
    async fn write(&self, up: &mut Upload, chunk: &Bytes) -> Result<(), StoreError> {
        if self.parts == Parts::Direct {
            return Ok(self.send_part(up, chunk.clone()).await?);
        }
        // A request that declared an empty body and sent bytes all the same.
        if up.permit.is_none() && !self.try_permit(up) {
            return Err(StoreError::PartsBusy);
        }
        let mut rest = &chunk[..];
        while !rest.is_empty() {
            if up.buf.capacity() == 0 {
                up.buf.reserve_exact(PART_SIZE);
            }
            let take = (PART_SIZE - up.buf.len()).min(rest.len());
            up.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if up.buf.len() == PART_SIZE {
                let part = Bytes::from(std::mem::take(&mut up.buf));
                self.send_part(up, part).await?;
            }
        }
        Ok(())
    }

    async fn send_part(&self, up: &mut Upload, part: Bytes) -> Result<(), object_store::Error> {
        if up.part.is_none() {
            let key = Key::Data(up.id.clone()).path();
            up.part = Some(self.os.put_multipart(&key).await?);
        }
        match up.part.as_mut() {
            Some(p) => p.put_part(PutPayload::from(part)).await,
            None => Ok(()),
        }
    }

    /// Ends the upload `id` as blob `want`: verifies the digest, completes
    /// its data object, or puts it whole if it never filled a part, writes
    /// its marker and commits it into its repository. The upload is gone
    /// either way; it keeps its permit until its last bytes are sent.
    pub async fn finish(
        &self,
        id: &str,
        mut up: OwnedMutexGuard<Upload>,
        want: &Digest,
    ) -> Result<(), StoreError> {
        self.uploads().remove(id);
        let got = Digest::finish(up.hasher.clone());
        let (repo, size, data_id) = (up.repo.clone(), up.size, up.id.clone());
        let buf = Bytes::from(std::mem::take(&mut up.buf));
        let data = Key::Data(data_id.clone()).path();
        let marker = Key::Marker(want.clone(), data_id.clone()).path();
        let written = if &got == want {
            let part = up.part.take();
            async {
                match part {
                    Some(mut p) => {
                        let sent = async {
                            if !buf.is_empty() {
                                p.put_part(PutPayload::from(buf)).await?;
                            }
                            p.complete().await
                        }
                        .await;
                        if let Err(e) = sent {
                            abort_part(p).await;
                            return Err(e);
                        }
                    }
                    None => {
                        self.os.put(&data, PutPayload::from(buf)).await?;
                    }
                }
                self.os.put(&marker, PutPayload::default()).await
            }
            .await
            .map_err(StoreError::from)
        } else {
            Err(StoreError::DigestMismatch(want.clone()))
        };
        let part = self.end(&mut up);
        drop(up);
        if let Some(p) = part {
            abort_part(p).await;
        }
        if let Err(e) = written {
            if !matches!(e, StoreError::DigestMismatch(_)) {
                self.delete(vec![marker, data]).await;
            }
            return Err(e);
        }
        let doomed = commit(&mut self.index(), &repo, want, Blob { size, id: data_id });
        self.delete(doomed).await;
        Ok(())
    }

    /// Stores a manifest and, for a tag, points the tag at it. Every blob it
    /// references must already be linked in `repo`, or in `fallback`, the
    /// trusted repository that a branch repository reads through: such a blob
    /// is then linked into `repo` too, so the branch cache holds everything it
    /// references. The check runs before anything is written and again under
    /// the lock that installs the manifest.
    pub async fn put_manifest(
        &self,
        repo: &str,
        fallback: Option<&str>,
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
        borrowed(&self.index(), repo, fallback, &refs)?;

        let now = now_secs();
        let data_id = Id::random()?;
        let m = Manifest {
            media_type,
            refs,
            last_used: now,
            persisted: now,
            record: Id::random()?,
        };
        let tag = match reference {
            Reference::Tag(t) => Some((t.clone(), Id::random()?)),
            Reference::Digest(_) => None,
        };
        let data = Key::Data(data_id.clone()).path();
        let mut rest = vec![
            (Key::Marker(d.clone(), data_id.clone()).path(), Bytes::new()),
            (
                Key::Record {
                    repo: repo.to_owned(),
                    digest: d.clone(),
                    id: m.record.clone(),
                }
                .path(),
                Bytes::from(m.record_text()),
            ),
        ];
        if let Some((t, id)) = &tag {
            let key = Key::Tag {
                repo: repo.to_owned(),
                tag: t.clone(),
                digest: d.clone(),
                id: id.clone(),
            };
            rest.push((key.path(), Bytes::new()));
        }
        let keys: Vec<Path> = std::iter::once(data.clone())
            .chain(rest.iter().map(|(k, _)| k.clone()))
            .collect();
        // The data first, so a marker never names a missing object.
        let written = async {
            self.os
                .put(&data, PutPayload::from(Bytes::copy_from_slice(body)))
                .await?;
            futures::future::try_join_all(
                rest.into_iter()
                    .map(|(k, b)| async move { self.os.put(&k, PutPayload::from(b)).await }),
            )
            .await
        }
        .await;
        if let Err(e) = written {
            self.delete(keys).await;
            return Err(e.into());
        }

        let (outcome, doomed) = {
            let mut idx = self.index();
            match borrowed(&idx, repo, fallback, &m.refs) {
                Err(e) => (Err(e), keys),
                Ok(borrowed) => {
                    let blob = Blob {
                        size: body.len() as u64,
                        id: data_id,
                    };
                    let doomed = install(&mut idx, repo, &d, blob, &borrowed, m, tag);
                    (Ok(()), doomed)
                }
            }
        };
        self.delete(doomed).await;
        outcome.map(|()| d)
    }

    /// The manifest `reference` of `repo`. Records the pull in memory; the
    /// next collection writes it to the record.
    pub fn manifest(&self, repo: &str, reference: &Reference) -> Option<FoundManifest> {
        let mut idx = self.index();
        let Index { blobs, repos } = &mut *idx;
        let r = repos.get_mut(repo)?;
        let d = match reference {
            Reference::Tag(t) => r.tags.get(t)?.digest.clone(),
            Reference::Digest(d) => d.clone(),
        };
        let b = blobs.get(&d)?;
        let m = r.manifests.get_mut(&d)?;
        m.last_used = now_secs();
        Some(FoundManifest {
            media_type: m.media_type.clone(),
            blob: Found {
                size: b.size,
                id: b.id.clone(),
            },
            digest: d,
        })
    }

    /// Deletes uploads that no request holds and that have been idle for
    /// `max_age`, or for [`PART_IDLE`] while holding a part permit. Returns
    /// how many.
    pub async fn expire_uploads(&self, max_age: Duration) -> usize {
        let part_idle = PART_IDLE.min(max_age);
        self.expire_where(|u| {
            let idle = u.touched.elapsed();
            idle >= max_age || (u.permit.is_some() && idle >= part_idle)
        })
        .await
    }

    /// Deletes the uploads that no request holds and that `doomed` picks.
    async fn expire_where(&self, doomed: impl Fn(&Upload) -> bool) -> usize {
        let mut parts = Vec::new();
        let mut n = 0;
        self.uploads().retain(|_, slot| match slot.try_lock() {
            Ok(mut up) if doomed(&up) => {
                parts.extend(self.end(&mut up));
                n += 1;
                false
            }
            _ => true,
        });
        for p in parts {
            abort_part(p).await;
        }
        n
    }

    /// Runs one garbage collection (see [`gc`]): deletes the retry list,
    /// applies the plan to the index under the lock, deletes what it removed
    /// after releasing it, then writes the records whose last-used time
    /// changed.
    pub async fn gc(&self, budgets: &dyn Budgets, grace: Duration) -> GcReport {
        self.retry_deletes().await;
        let (report, rounds) = self.apply(budgets, grace);
        for round in rounds {
            self.delete(round).await;
        }
        self.write_last_used().await;
        report
    }

    /// Applies one plan. Returns the keys to delete in three rounds: records
    /// and tag markers, then blob markers, then data objects, so a crash
    /// between rounds leaves only what the next open cleans.
    fn apply(&self, budgets: &dyn Budgets, grace: Duration) -> (GcReport, [Vec<Path>; 3]) {
        let mut idx = self.index();
        let plan = gc::plan(&idx, budgets, now_secs(), grace.as_secs());
        let mut report = GcReport::default();
        let [mut repo_keys, mut markers, mut data] = [Vec::new(), Vec::new(), Vec::new()];
        for (name, d, reason) in &plan.evict {
            let Some(repo) = idx.repos.get_mut(name) else {
                continue;
            };
            let Some(m) = repo.manifests.remove(d) else {
                continue;
            };
            repo_keys.push(
                Key::Record {
                    repo: name.clone(),
                    digest: d.clone(),
                    id: m.record,
                }
                .path(),
            );
            let tags: Vec<String> = repo
                .tags
                .iter()
                .filter(|(_, t)| t.digest == *d)
                .map(|(k, _)| k.clone())
                .collect();
            for t in tags {
                if let Some(old) = repo.tags.remove(&t) {
                    repo_keys.push(
                        Key::Tag {
                            repo: name.clone(),
                            tag: t,
                            digest: old.digest,
                            id: old.id,
                        }
                        .path(),
                    );
                }
            }
            match reason {
                Reason::Pool => report.evicted_pool += 1,
                Reason::Volume => report.evicted_volume += 1,
            }
        }
        for (name, d) in &plan.unlink {
            if let Some(repo) = idx.repos.get_mut(name) {
                repo.links.remove(d);
            }
        }
        for d in &plan.delete {
            let Some(b) = idx.blobs.remove(d) else {
                continue;
            };
            report.deleted_blobs += 1;
            report.freed_bytes += b.size;
            markers.push(Key::Marker(d.clone(), b.id.clone()).path());
            data.push(Key::Data(b.id).path());
        }
        idx.repos
            .retain(|_, r| !(r.links.is_empty() && r.manifests.is_empty() && r.tags.is_empty()));
        (report, [repo_keys, markers, data])
    }

    /// Writes each manifest whose last-used time differs from its record to
    /// a fresh record key, and swaps the new key in where the index still
    /// names the old one. A failed write leaves the manifest changed for the
    /// next pass.
    async fn write_last_used(&self) {
        let changed: Vec<(String, Digest, Id, u64, String)> = {
            let idx = self.index();
            idx.repos
                .iter()
                .flat_map(|(name, r)| {
                    r.manifests
                        .iter()
                        .filter(|(_, m)| m.last_used != m.persisted)
                        .map(move |(d, m)| {
                            (
                                name.clone(),
                                d.clone(),
                                m.record.clone(),
                                m.last_used,
                                m.record_text(),
                            )
                        })
                })
                .collect()
        };
        if changed.is_empty() {
            return;
        }
        let written: Vec<_> = stream::iter(changed)
            .map(|(repo, d, old, used, text)| async move {
                let id = Id::random()?;
                let key = Key::Record {
                    repo: repo.clone(),
                    digest: d.clone(),
                    id: id.clone(),
                };
                self.os.put(&key.path(), PutPayload::from(text)).await?;
                Ok::<_, StoreError>((repo, d, old, used, id))
            })
            .buffer_unordered(RECORD_IO)
            .filter_map(|r| async move {
                r.inspect_err(|e| warn!(error = %e, "recording manifest pulls"))
                    .ok()
            })
            .collect()
            .await;
        let doomed: Vec<Path> = {
            let mut idx = self.index();
            written
                .into_iter()
                .map(|(repo, d, old, used, id)| {
                    let m = idx
                        .repos
                        .get_mut(&repo)
                        .and_then(|r| r.manifests.get_mut(&d))
                        .filter(|m| m.record == old);
                    // The new key replaces the old one, or loses to a push or
                    // an eviction that came in between.
                    let gone = match m {
                        Some(m) => {
                            m.record = id;
                            m.persisted = used;
                            old
                        }
                        None => id,
                    };
                    Key::Record {
                        repo,
                        digest: d,
                        id: gone,
                    }
                    .path()
                })
                .collect()
        };
        self.delete(doomed).await;
    }

    /// Totals for the gauges.
    pub fn stats(&self) -> Stats {
        let idx = self.index();
        Stats {
            bytes: idx.blobs.values().map(|b| b.size).sum(),
            blobs: idx.blobs.len() as u64,
            manifests: idx.repos.values().map(|r| r.manifests.len() as u64).sum(),
        }
    }

    /// Every key the index names.
    #[cfg(test)]
    pub(super) fn keys(&self) -> std::collections::BTreeSet<String> {
        let idx = self.index();
        let mut out = std::collections::BTreeSet::new();
        for (d, b) in &idx.blobs {
            out.insert(Key::Data(b.id.clone()).to_string());
            out.insert(Key::Marker(d.clone(), b.id.clone()).to_string());
        }
        for (name, r) in &idx.repos {
            for (d, m) in &r.manifests {
                let k = Key::Record {
                    repo: name.clone(),
                    digest: d.clone(),
                    id: m.record.clone(),
                };
                out.insert(k.to_string());
            }
            for (tag, t) in &r.tags {
                let k = Key::Tag {
                    repo: name.clone(),
                    tag: tag.clone(),
                    digest: t.digest.clone(),
                    id: t.id.clone(),
                };
                out.insert(k.to_string());
            }
        }
        out
    }

    /// Every key in the store.
    #[cfg(test)]
    pub(super) async fn listed(&self) -> std::collections::BTreeSet<String> {
        self.os
            .list(None)
            .map_ok(|m| m.location.to_string())
            .try_collect()
            .await
            .unwrap_or_default()
    }

    /// The last-used time of each manifest record in the store, read from
    /// the store.
    #[cfg(test)]
    pub(super) async fn recorded_times(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for key in self.listed().await {
            if !key.contains("/_manifests/") {
                continue;
            }
            let Ok(r) = self.os.get(&Path::from(key)).await else {
                continue;
            };
            let text = r.bytes().await.unwrap_or_default();
            let text = String::from_utf8_lossy(&text);
            out.extend(text.lines().nth(1).and_then(|l| l.parse::<u64>().ok()));
        }
        out
    }

    /// The blobs linked in `repo`.
    #[cfg(test)]
    pub(super) fn links(&self, repo: &str) -> Vec<Digest> {
        self.index()
            .repos
            .get(repo)
            .map(|r| r.links.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether every manifest's record holds its last-used time.
    #[cfg(test)]
    pub(super) fn persisted(&self) -> bool {
        self.index()
            .repos
            .values()
            .flat_map(|r| r.manifests.values())
            .all(|m| m.last_used == m.persisted)
    }

    /// Sets every manifest's last-used time to `t`, as a pull at `t` would.
    #[cfg(test)]
    pub(super) fn set_last_used(&self, t: u64) {
        for r in self.index().repos.values_mut() {
            for m in r.manifests.values_mut() {
                m.last_used = t;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use object_store::memory::InMemory;

    use super::*;
    use crate::registry::testing::Probe;

    const LIMITS: StoreLimits = StoreLimits {
        max_blob: 1 << 20,
        max_upload_bytes: 4 << 20,
    };

    /// Recording a pull is bookkeeping: when the store refuses the write, the
    /// manifest is still served, and the next pass tries again.
    #[tokio::test]
    async fn a_failed_pull_record_does_not_fail_the_read() {
        let probe = Arc::new(Probe::new(Arc::new(InMemory::new())));
        let store = Store::open(probe.clone(), LIMITS, Parts::Buffered)
            .await
            .unwrap();
        let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
        let tag = Reference::Tag("t".into());
        let d = store
            .put_manifest("ns/p/app", None, &tag, None, body)
            .await
            .unwrap();
        store.set_last_used(1);
        probe.fail_puts(true);
        let big = gc::FixedBudgets {
            pool: u64::MAX,
            total: u64::MAX,
        };
        store.gc(&big, Duration::from_hours(1)).await;
        assert!(!store.persisted());
        let found = store.manifest("ns/p/app", &tag).unwrap();
        assert_eq!(found.digest, d);
        assert!(store.read(&found.blob, None).await.unwrap().is_some());
        // Once the store answers again, the pass writes the record.
        probe.fail_puts(false);
        store.gc(&big, Duration::from_hours(1)).await;
        assert!(store.persisted());
        assert_eq!(store.keys(), store.listed().await);
    }

    #[test]
    fn records_round_trip() {
        let m = Manifest {
            media_type: "application/vnd.oci.image.index.v1+json".into(),
            refs: {
                let mut r = vec![Digest::of(b"a"), Digest::of(b"b")];
                r.sort();
                r
            },
            last_used: 42,
            persisted: 42,
            record: Id::random().unwrap(),
        };
        let back = Manifest::parse_record(&m.record_text(), m.record.clone()).unwrap();
        assert_eq!(
            (back.media_type, back.refs, back.last_used),
            (m.media_type, m.refs, m.last_used)
        );
        for bad in ["", "\n1\n", "x\nnot a time\n", "x\n1\nsha256:00\n"] {
            assert!(
                Manifest::parse_record(bad, m.record.clone()).is_none(),
                "{bad:?}"
            );
        }
    }
}
