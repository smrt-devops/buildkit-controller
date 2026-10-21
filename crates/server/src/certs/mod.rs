//! Shared certificate material: the one loader
//! for the CA Secret and the optional user serving Secret, plugged into the
//! runtime's `pki` hook slot.
//!
//! [`Certs`] reads the Secrets through the [`AppContext`] stores, validates them
//! with [`crate::pki`], issues and renews the in-memory certs (the self-issued
//! serving cert, the edge-to-worker client cert) and publishes every change as a
//! [`CertMaterial`] snapshot on a `tokio::sync::watch` channel. It never writes
//! the CA (the reconciler does). It also sets the `ca_loaded` and `serving_cert`
//! readiness checks and the `buildkit_ca_valid` and
//! `buildkit_cert_load_errors_total` metrics, each once per Secret change.
//!
//! # Consuming it
//!
//! ```ignore
//! let certs = Arc::new(Certs::new());              // main.rs, the `pki` hook
//! let rx: watch::Receiver<CertMaterial> = certs.subscribe();
//! // API TLS uses the same cert the edge serves and swaps it on every change.
//! let key: Option<Arc<CertifiedKey>> = rx.borrow().serving();
//! ```
//!
//! Wait on `rx.changed()` to pick up a new generation; a snapshot taken before
//! the first load has no serving cert (`serving()` is `None`).
//!
//! # Rules
//!
//! * The API and the edge serve the SAME [`CertifiedKey`] per replica.
//! * A user serving Secret that can never become valid ends [`Certs::run`] at
//!   startup with [`InvalidServingSecret`], which `main` maps to exit 2. Later
//!   the loader keeps the previous cert and counts the error.
//! * A user cert that fails only because its `notBefore` is more than 5
//!   minutes ahead never ends startup. The replica stays not ready, or keeps
//!   the previous cert, counts one error and checks the Secret again at
//!   `notBefore`.
//! * An unusable CA keeps the last trust but withdraws the issuer
//!   ([`CertMaterial::issuer`] is `None`), so allocation answers `ca_unavailable`.
//! * A failed self-issue is retried every 30 s ([`follow::RETRY`]) instead of
//!   waiting for the cert's renewal time.

mod follow;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use rustls::sign::CertifiedKey;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::pki::{CaBundle, EdgeNames, Namespace};
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component};
use crate::runtime::stores::SecretState;
pub use follow::{RETRY, startup_decision};

/// The user serving Secret was invalid at startup: exit 2.
#[derive(Debug, thiserror::Error)]
#[error("edge.tls.secretName Secret is invalid: {0}")]
pub struct InvalidServingSecret(pub String);

/// The edge-to-worker client cert, in memory only.
pub struct EdgeClient {
    /// Leaf first.
    pub chain: Vec<CertificateDer<'static>>,
    /// The leaf's private key.
    pub key: PrivateKeyDer<'static>,
}

/// Which `caCert` the allocation response carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaCertRule {
    /// The serving cert is self-issued, so the rule returns the client CA.
    ClientCa,
    /// `edge.tls.secretName` is set. The rule returns that Secret's last known
    /// `ca.crt` and never the client CA. `None` means the Secret has not been
    /// seen yet or has no `ca.crt`.
    Configured(Option<String>),
}

/// One consistent snapshot of the certificate material.
#[derive(Clone)]
pub struct CertMaterial {
    bundle: Option<Arc<CaBundle>>,
    usable: bool,
    edge_client: Option<Arc<EdgeClient>>,
    serving: Option<Arc<CertifiedKey>>,
    rule: CaCertRule,
}

impl fmt::Debug for CertMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertMaterial")
            .field("ca_loaded", &self.bundle.is_some())
            .field("usable", &self.usable)
            .field("serving", &self.serving.is_some())
            .field("rule", &self.rule)
            .finish_non_exhaustive()
    }
}

impl CertMaterial {
    /// Nothing loaded yet.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            bundle: None,
            usable: false,
            edge_client: None,
            serving: None,
            rule: CaCertRule::ClientCa,
        }
    }

    /// A usable CA and nothing else (tests and fakes).
    #[must_use]
    pub fn from_bundle(bundle: Arc<CaBundle>, rule: CaCertRule) -> Self {
        Self {
            bundle: Some(bundle),
            usable: true,
            rule,
            ..Self::empty()
        }
    }

    /// The client CA certificate edge client-cert verification trusts (the last
    /// usable one, kept while the Secret is unusable).
    #[must_use]
    pub fn client_ca(&self) -> Option<&CertificateDer<'static>> {
        self.bundle.as_ref().map(|b| b.client_ca().cert_der())
    }

    /// The internal CA certificate the edge trusts for workers.
    #[must_use]
    pub fn internal_ca(&self) -> Option<&CertificateDer<'static>> {
        self.bundle.as_ref().map(|b| b.internal_ca().cert_der())
    }

    /// The edge-to-worker client cert, once issued.
    #[must_use]
    pub fn edge_client(&self) -> Option<&EdgeClient> {
        self.edge_client.as_deref()
    }

    /// The serving cert the API and the edge share.
    #[must_use]
    pub fn serving(&self) -> Option<Arc<CertifiedKey>> {
        self.serving.clone()
    }

    /// The bundle to issue allocation certs with; `None` while the CA Secret
    /// is missing or unusable (`ca_unavailable`).
    #[must_use]
    pub fn issuer(&self) -> Option<Arc<CaBundle>> {
        self.bundle.clone().filter(|_| self.usable)
    }

    /// `caCert` for a response issued by `bundle` (see [`CaCertRule`]).
    #[must_use]
    pub fn ca_cert_pem(&self, bundle: &CaBundle) -> Option<String> {
        match &self.rule {
            CaCertRule::ClientCa => Some(bundle.client_ca().cert_pem().to_owned()),
            CaCertRule::Configured(v) => v.clone(),
        }
    }
}

/// The runtime's `pki` component. It owns the publisher of [`CertMaterial`].
pub struct Certs {
    tx: watch::Sender<CertMaterial>,
}

impl Default for Certs {
    fn default() -> Self {
        Self::new()
    }
}

impl Certs {
    /// Publishes [`CertMaterial::empty`] until [`Component::run`] loads.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tx: watch::channel(CertMaterial::empty()).0,
        }
    }

    /// A receiver for the live material; clone it into API, edge and alloc.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<CertMaterial> {
        self.tx.subscribe()
    }
}

impl Component for Certs {
    fn name(&self) -> &'static str {
        "pki"
    }

    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let tx = self.tx.clone();
        Box::pin(async move {
            let edge = &ctx.config().edge;
            let user_serving = !edge.tls.secret_name.is_empty();
            let mut srv_rx = ctx.serving_secret();
            if user_serving {
                let ready = tokio::select! {
                    () = shutdown.cancelled() => return Ok(()),
                    r = srv_rx.wait_for(|s| s.init_done) => r.is_ok(),
                };
                if !ready {
                    return Ok(());
                }
                startup_decision(
                    user_serving,
                    &srv_rx.borrow(),
                    &edge.server_name,
                    OffsetDateTime::now_utc(),
                )?;
            }
            let names = ctx
                .namespace()
                .parse::<Namespace>()
                .map_err(|e| e.to_string())
                .and_then(|ns| {
                    EdgeNames::new(
                        &edge.server_name,
                        &edge.extra_names,
                        &edge.service_name,
                        &ns,
                    )
                    .map_err(|e| e.to_string())
                });
            let loader = follow::Loader::new(
                follow::Config {
                    user_serving,
                    server_name: edge.server_name.clone(),
                    names,
                    validity: ctx.config().leaf_validity(),
                },
                Arc::clone(ctx.metrics()),
                ctx.readiness().register("ca_loaded"),
                ctx.readiness().register("serving_cert"),
            );
            follow::drive(loader, ctx.ca_secret(), srv_rx, tx, shutdown).await;
            Ok(())
        })
    }
}

pub(crate) fn secret_data(s: &SecretState) -> Option<std::collections::BTreeMap<String, &[u8]>> {
    let data = s.secret.as_ref()?.data.as_ref()?;
    Some(
        data.iter()
            .map(|(k, v)| (k.clone(), v.0.as_slice()))
            .collect(),
    )
}

#[cfg(test)]
impl CertMaterial {
    fn rule_for_test(&self) -> CaCertRule {
        self.rule.clone()
    }
}
