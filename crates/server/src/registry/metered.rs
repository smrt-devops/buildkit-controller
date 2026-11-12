//! A store wrapper that counts and times every call, for
//! `buildkit_registry_store_operations_total` and
//! `buildkit_registry_store_operation_duration_seconds`, in the style of
//! `object_store`'s own wrappers (`limit`, `throttle`).

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result, UploadPart,
};

use super::metrics::RegistryMetrics;

/// `inner`, with each call recorded in `metrics`.
#[derive(Debug)]
pub struct Metered {
    inner: Arc<dyn ObjectStore>,
    metrics: RegistryMetrics,
}

impl Metered {
    pub fn new(inner: Arc<dyn ObjectStore>, metrics: RegistryMetrics) -> Self {
        Self { inner, metrics }
    }
}

/// A missing key on a read or a delete is the expected end of a race with
/// garbage collection, not a store failure.
fn ok_or_missing<T>(r: &Result<T>) -> bool {
    matches!(r, Ok(_) | Err(object_store::Error::NotFound { .. }))
}

fn record<T>(m: &RegistryMetrics, op: &'static str, started: Instant, ok: bool, r: T) -> T {
    m.store_op(op, ok, started.elapsed().as_secs_f64());
    r
}

impl fmt::Display for Metered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Metered({})", self.inner)
    }
}

/// Records one listing when it is dropped: finished, failed or abandoned.
struct Listing {
    metrics: RegistryMetrics,
    started: Instant,
    failed: bool,
}

impl Drop for Listing {
    fn drop(&mut self) {
        self.metrics
            .store_op("list", !self.failed, self.started.elapsed().as_secs_f64());
    }
}

#[async_trait::async_trait]
impl ObjectStore for Metered {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        let t = Instant::now();
        let r = self.inner.put_opts(location, payload, opts).await;
        record(&self.metrics, "put", t, r.is_ok(), r)
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        let t = Instant::now();
        let r = self.inner.put_multipart_opts(location, opts).await;
        let r = record(&self.metrics, "put", t, r.is_ok(), r)?;
        Ok(Box::new(MeteredUpload {
            inner: r,
            metrics: self.metrics.clone(),
        }))
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let t = Instant::now();
        let r = self.inner.get_opts(location, options).await;
        record(&self.metrics, "get", t, ok_or_missing(&r), r)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        let metrics = self.metrics.clone();
        let t = Instant::now();
        self.inner
            .delete_stream(locations)
            .map(move |r| {
                let ok = ok_or_missing(&r);
                record(&metrics, "delete", t, ok, r)
            })
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        let mut listing = Listing {
            metrics: self.metrics.clone(),
            started: Instant::now(),
            failed: false,
        };
        self.inner
            .list(prefix)
            .map(move |r| {
                // The whole guard moves into the stream, and records when it drops.
                let l = &mut listing;
                l.failed |= r.is_err();
                r
            })
            .boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        let t = Instant::now();
        let r = self.inner.list_with_delimiter(prefix).await;
        record(&self.metrics, "list", t, r.is_ok(), r)
    }

    /// The registry never copies (the layout writes fresh keys), so a copy
    /// has no `op` of its own.
    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[derive(Debug)]
struct MeteredUpload {
    inner: Box<dyn MultipartUpload>,
    metrics: RegistryMetrics,
}

#[async_trait::async_trait]
impl MultipartUpload for MeteredUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        let metrics = self.metrics.clone();
        let t = Instant::now();
        self.inner
            .put_part(data)
            .map(move |r| record(&metrics, "put_part", t, r.is_ok(), r))
            .boxed()
    }

    async fn complete(&mut self) -> Result<PutResult> {
        let t = Instant::now();
        let r = self.inner.complete().await;
        record(&self.metrics, "complete", t, r.is_ok(), r)
    }

    async fn abort(&mut self) -> Result<()> {
        let t = Instant::now();
        let r = self.inner.abort().await;
        record(&self.metrics, "abort", t, r.is_ok(), r)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use futures::TryStreamExt;
    use object_store::ObjectStoreExt;
    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn every_call_is_counted_and_a_missing_key_is_ok() {
        let m = RegistryMetrics::default();
        let os = Metered::new(Arc::new(InMemory::new()), m.clone());
        let a = Path::from("data/a");
        os.put(&a, PutPayload::from("x")).await.unwrap();
        let mut up = os.put_multipart(&Path::from("data/b")).await.unwrap();
        up.put_part(PutPayload::from("y")).await.unwrap();
        up.complete().await.unwrap();
        let mut gone = os.put_multipart(&Path::from("data/c")).await.unwrap();
        gone.abort().await.unwrap();
        os.get(&a).await.unwrap();
        assert!(os.get(&Path::from("data/missing")).await.is_err());
        let listed: Vec<_> = os.list(None).try_collect().await.unwrap();
        assert_eq!(listed.len(), 2);
        let deleted: Vec<_> = os
            .delete_stream(
                futures::stream::iter([Ok(a.clone()), Ok(Path::from("data/missing"))]).boxed(),
            )
            .collect()
            .await;
        assert_eq!(deleted.len(), 2);
        for (op, n) in [
            ("put", 3),
            ("put_part", 1),
            ("complete", 1),
            ("abort", 1),
            ("get", 2),
            ("list", 1),
            ("delete", 2),
        ] {
            assert_eq!((op, m.store_ops(op, "ok")), (op, n));
            assert_eq!((op, m.store_ops(op, "error")), (op, 0));
        }
    }
}
