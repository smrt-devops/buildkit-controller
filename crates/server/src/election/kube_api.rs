//! [`LeaseApi`] over the Kubernetes API.

use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, PostParams};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Client, Error};
use tracing::warn;

use super::{ApiError, LeaseApi, LeaseSnapshot};

/// Called for every watcher `Err` item.
pub type OnWatchError = Arc<dyn Fn() + Send + Sync>;

/// Lease access for one named Lease.
pub struct KubeLeaseApi {
    api: Api<Lease>,
    name: String,
    on_watch_error: OnWatchError,
}

impl KubeLeaseApi {
    /// Lease `name` in `namespace`.
    #[must_use]
    pub fn new(client: Client, namespace: &str, name: &str, on_watch_error: OnWatchError) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
            name: name.to_owned(),
            on_watch_error,
        }
    }
}

fn snap(l: Lease) -> LeaseSnapshot {
    LeaseSnapshot {
        resource_version: l.metadata.resource_version.unwrap_or_default(),
        spec: l.spec.unwrap_or_default(),
    }
}

fn map_err(e: &Error) -> ApiError {
    match e {
        Error::Api(s) if s.code == 409 => ApiError::Conflict,
        Error::Api(s) if s.code == 404 => ApiError::NotFound,
        other => ApiError::Other(other.to_string()),
    }
}

impl LeaseApi for KubeLeaseApi {
    async fn get(&self) -> Result<Option<LeaseSnapshot>, ApiError> {
        self.api
            .get_opt(&self.name)
            .await
            .map(|o| o.map(snap))
            .map_err(|e| map_err(&e))
    }

    async fn create(&self, spec: LeaseSpec) -> Result<LeaseSnapshot, ApiError> {
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(spec),
        };
        self.api
            .create(&PostParams::default(), &lease)
            .await
            .map(snap)
            .map_err(|e| map_err(&e))
    }

    async fn replace(
        &self,
        prev: &LeaseSnapshot,
        spec: LeaseSpec,
    ) -> Result<LeaseSnapshot, ApiError> {
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                resource_version: Some(prev.resource_version.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(spec),
        };
        self.api
            .replace(&self.name, &PostParams::default(), &lease)
            .await
            .map(snap)
            .map_err(|e| map_err(&e))
    }

    fn watch(&self) -> BoxStream<'static, Option<LeaseSnapshot>> {
        let cfg = watcher::Config::default().fields(&format!("metadata.name={}", self.name));
        let on_err = Arc::clone(&self.on_watch_error);
        let events = watcher(self.api.clone(), cfg)
            .default_backoff()
            .filter_map(move |ev| {
                let ev = match ev {
                    Ok(ev) => Some(ev),
                    Err(e) => {
                        warn!(error = %e, kind = "lease", "watch error");
                        on_err();
                        None
                    }
                };
                std::future::ready(ev)
            });
        lease_changes(events).boxed()
    }
}

/// The Lease as each watcher event leaves it, `None` when absent. A list that
/// finds no Lease reports `None` at its `InitDone`, so a deletion missed while
/// the watch was down (or ignored while leading) is still learned on the relist.
fn lease_changes(
    events: impl futures::Stream<Item = watcher::Event<Lease>> + Send + 'static,
) -> impl futures::Stream<Item = Option<LeaseSnapshot>> + Send + 'static {
    let mut listed = false;
    events.filter_map(move |ev| {
        let out = match ev {
            watcher::Event::Init => {
                listed = false;
                None
            }
            watcher::Event::Apply(l) | watcher::Event::InitApply(l) => {
                listed = true;
                Some(Some(snap(l)))
            }
            watcher::Event::Delete(_) => Some(None),
            watcher::Event::InitDone => (!listed).then_some(None),
        };
        std::future::ready(out)
    })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use futures::stream;
    use k8s_openapi::api::coordination::v1::Lease;
    use kube::runtime::watcher::Event;

    use super::lease_changes;

    fn lease(rv: &str) -> Lease {
        let mut l = Lease::default();
        l.metadata.resource_version = Some(rv.into());
        l
    }

    fn versions(events: Vec<Event<Lease>>) -> Vec<Option<String>> {
        futures::executor::block_on(
            lease_changes(stream::iter(events))
                .map(|s| s.map(|s| s.resource_version))
                .collect(),
        )
    }

    #[test]
    fn a_relist_without_the_lease_reports_it_gone() {
        assert_eq!(
            versions(vec![
                Event::Init,
                Event::InitApply(lease("1")),
                Event::InitDone,
                Event::Apply(lease("2")),
                // The watch fails and relists; the Lease was deleted meanwhile.
                Event::Init,
                Event::InitDone,
            ]),
            [Some("1".into()), Some("2".into()), None]
        );
    }

    #[test]
    fn a_relist_with_the_lease_reports_only_the_lease() {
        assert_eq!(
            versions(vec![
                Event::Init,
                Event::InitApply(lease("1")),
                Event::InitDone,
                Event::Delete(lease("1")),
            ]),
            [Some("1".into()), None]
        );
    }
}
