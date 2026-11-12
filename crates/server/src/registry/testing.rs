//! A store wrapper for tests: records reads, fails calls, holds deletes.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};
use tokio::sync::watch;

#[derive(Debug)]
pub struct Probe {
    inner: Arc<dyn ObjectStore>,
    gets: Mutex<Vec<String>>,
    multiparts: AtomicUsize,
    fail_puts: watch::Sender<bool>,
    fail_all: watch::Sender<bool>,
    /// Delete batches that may still run; after them every delete fails.
    batches: Mutex<Option<usize>>,
    /// Deletes wait while this is false.
    deletes: watch::Sender<bool>,
}

fn refused() -> object_store::Error {
    object_store::Error::Generic {
        store: "probe",
        source: "refused".into(),
    }
}

impl Probe {
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            gets: Mutex::default(),
            multiparts: AtomicUsize::new(0),
            fail_puts: watch::Sender::new(false),
            fail_all: watch::Sender::new(false),
            batches: Mutex::new(None),
            deletes: watch::Sender::new(true),
        }
    }

    /// The keys read so far.
    pub fn gets(&self) -> Vec<String> {
        self.gets.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Multipart uploads started so far.
    pub fn multiparts(&self) -> usize {
        self.multiparts.load(Ordering::Relaxed)
    }

    /// Fails every put and multipart write from now on, or no longer.
    pub fn fail_puts(&self, on: bool) {
        self.fail_puts.send_replace(on);
    }

    /// Fails every call from now on, as an unreachable store would, or no
    /// longer.
    pub fn fail_all(&self, on: bool) {
        self.fail_all.send_replace(on);
    }

    /// Lets `n` more delete batches through; every later delete fails, as if
    /// the process had died.
    pub fn crash_after_deletes(&self, n: usize) {
        if let Ok(mut b) = self.batches.lock() {
            *b = Some(n);
        }
    }

    /// Makes deletes wait until [`Probe::release_deletes`].
    pub fn hold_deletes(&self) {
        self.deletes.send_replace(false);
    }

    pub fn release_deletes(&self) {
        self.deletes.send_replace(true);
    }

    fn check(&self) -> Result<()> {
        if *self.fail_all.borrow() {
            return Err(refused());
        }
        Ok(())
    }

    fn check_put(&self) -> Result<()> {
        self.check()?;
        if *self.fail_puts.borrow() {
            return Err(refused());
        }
        Ok(())
    }

    /// Whether this delete batch may run.
    fn take_batch(&self) -> bool {
        if *self.fail_all.borrow() {
            return false;
        }
        let Ok(mut b) = self.batches.lock() else {
            return false;
        };
        match &mut *b {
            None => true,
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
        }
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Probe({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Probe {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        self.check_put()?;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.check_put()?;
        self.multiparts.fetch_add(1, Ordering::Relaxed);
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        self.check()?;
        if let Ok(mut g) = self.gets.lock() {
            g.push(location.to_string());
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        if !self.take_batch() {
            return locations.map(|_| Err(refused())).boxed();
        }
        let inner = Arc::clone(&self.inner);
        let mut open = self.deletes.subscribe();
        futures::stream::once(async move {
            let _ = open.wait_for(|o| *o).await;
            inner.delete_stream(locations)
        })
        .flatten()
        .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        if self.check().is_err() {
            return futures::stream::once(async { Err(refused()) }).boxed();
        }
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.check()?;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.check()?;
        self.inner.copy_opts(from, to, options).await
    }
}
