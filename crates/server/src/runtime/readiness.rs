//! Named readiness checks behind `/readyz`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;

/// One check; clone freely. Starts failing; [`ReadyFlag::set`] marks it passing.
#[derive(Clone, Debug)]
pub struct ReadyFlag(watch::Sender<bool>);

impl ReadyFlag {
    /// Mark the check passing (idempotent).
    pub fn set(&self) {
        self.0.send_replace(true);
    }

    /// Mark the check failing again (idempotent).
    pub fn clear(&self) {
        self.0.send_replace(false);
    }

    /// Follow the check's state.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// The set of checks.
#[derive(Debug, Default)]
pub struct Readiness {
    checks: Mutex<BTreeMap<String, watch::Sender<bool>>>,
    shutting_down: AtomicBool,
}

impl Readiness {
    /// Every spec check registered and failing: `config`, `pool_store`,
    /// `pod_store`, `ca_loaded`, `serving_cert`, `api_listener`, `edge_listener`.
    #[must_use]
    pub fn with_spec_checks() -> Self {
        let r = Self::default();
        for name in [
            "config",
            "pool_store",
            "pod_store",
            "ca_loaded",
            "serving_cert",
            "api_listener",
            "edge_listener",
        ] {
            r.register(name);
        }
        r
    }

    /// Register the check `name`, or fetch the existing one (same state).
    pub fn register(&self, name: &str) -> ReadyFlag {
        let mut checks = self
            .checks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tx = checks
            .entry(name.to_owned())
            .or_insert_with(|| watch::channel(false).0);
        ReadyFlag(tx.clone())
    }

    /// Start failing `/readyz` (shutdown began).
    pub fn set_shutting_down(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    /// Names of the failing checks; empty means ready.
    pub fn failing(&self) -> Vec<String> {
        let checks = self
            .checks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<String> = checks
            .iter()
            .filter(|(_, tx)| !*tx.borrow())
            .map(|(n, _)| n.clone())
            .collect();
        if self.shutting_down.load(Ordering::SeqCst) {
            out.push("shutting_down".into());
        }
        out
    }

    /// Resolve when every named check passes. An unknown name is a check that
    /// has not passed yet (it is registered failing).
    pub async fn wait_for(&self, names: &[&str]) {
        for name in names {
            self.register(name);
        }
        let rxs: Vec<watch::Receiver<bool>> = {
            let checks = self
                .checks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            names
                .iter()
                .filter_map(|n| checks.get(*n).map(watch::Sender::subscribe))
                .collect()
        };
        for mut rx in rxs {
            let _ = rx.wait_for(|v| *v).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn spec_checks_fail_until_marked_and_register_is_idempotent() {
        let r = Readiness::with_spec_checks();
        assert_eq!(r.failing().len(), 7);
        r.register("ca_loaded").set();
        let again = r.register("ca_loaded");
        again.set();
        assert!(!r.failing().contains(&"ca_loaded".to_owned()));
        assert_eq!(r.failing().len(), 6, "first flag was not orphaned");
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_names_are_not_ready() {
        let r = Readiness::default();
        let waited = tokio::time::timeout(Duration::from_secs(5), r.wait_for(&["nope"])).await;
        assert!(waited.is_err());
        r.register("nope").set();
        r.wait_for(&["nope"]).await;
    }
}
