//! Where allocation gets the CA bundle and the `caCert` rule, namely the shared
//! certificate material (`crate::certs`). Sets no readiness checks and no CA
//! metrics (the pki component owns those).

use std::sync::Arc;

use tokio::sync::watch;

use crate::certs::CertMaterial;
use crate::pki::CaBundle;

/// CA material for allocation.
pub trait CertSource: Send + Sync {
    /// The loaded, usable CA bundle; `None` yields `ca_unavailable`.
    fn bundle(&self) -> Option<Arc<CaBundle>>;
    /// `caCert` of the response: with `edge.tls.secretName` set, that
    /// Secret's last known `ca.crt` (`None` if it has none), never the client
    /// CA; when self-issued, the client CA.
    fn ca_cert_pem(&self, bundle: &CaBundle) -> Option<String>;
}

/// [`CertSource`] over the live [`CertMaterial`] snapshot.
pub struct MaterialSource(watch::Receiver<CertMaterial>);

impl MaterialSource {
    /// From a subscription to the pki component.
    #[must_use]
    pub fn new(rx: watch::Receiver<CertMaterial>) -> Self {
        Self(rx)
    }
}

impl CertSource for MaterialSource {
    fn bundle(&self) -> Option<Arc<CaBundle>> {
        self.0.borrow().issuer()
    }

    fn ca_cert_pem(&self, bundle: &CaBundle) -> Option<String> {
        self.0.borrow().ca_cert_pem(bundle)
    }
}
