//! A store wrapper for tests: records reads, fails writes, holds deletes.

use std::fmt;
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
    fail_puts: watch::Sender<bool>,
    /// Deletes wait while this is false.
    deletes: watch::Sender<bool>,
}

impl Probe {
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            gets: Mutex::default(),
            fail_puts: watch::Sender::new(false),
            deletes: watch::Sender::new(true),
        }
    }

    /// The keys read so far.
    pub fn gets(&self) -> Vec<String> {
        self.gets.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Fails every put and multipart write from now on, or no longer.
    pub fn fail_puts(&self, on: bool) {
        self.fail_puts.send_replace(on);
    }

    /// Makes deletes wait until [`Probe::release_deletes`].
    pub fn hold_deletes(&self) {
        self.deletes.send_replace(false);
    }

    pub fn release_deletes(&self) {
        self.deletes.send_replace(true);
    }

    fn check_put(&self) -> Result<()> {
        if *self.fail_puts.borrow() {
            return Err(object_store::Error::Generic {
                store: "probe",
                source: "writes refused".into(),
            });
        }
        Ok(())
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
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        if let Ok(mut g) = self.gets.lock() {
            g.push(location.to_string());
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
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
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}
