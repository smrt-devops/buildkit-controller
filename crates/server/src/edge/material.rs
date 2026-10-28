//! Edge TLS from the shared certificate material (`crate::certs`): each
//! snapshot with a client CA and a serving cert becomes one [`EdgeTls`]
//! generation. A snapshot that fails to build is not published; the previous
//! generation stays. Likewise a generation whose worker-leg config cannot be
//! built keeps the previous generation's.

use std::sync::Arc;

use rustls::ClientConfig;
use rustls::crypto::CryptoProvider;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::tls::{EdgeTls, TlsBuilder};
use super::upstream;
use crate::certs::CertMaterial;

/// Runs until `stop`, publishing a generation into `tx` for each snapshot.
pub async fn follow(
    mut rx: watch::Receiver<CertMaterial>,
    builder: TlsBuilder,
    tx: watch::Sender<Option<Arc<EdgeTls>>>,
    stop: CancellationToken,
) {
    loop {
        let m = rx.borrow_and_update().clone();
        let previous = tx.borrow().as_ref().and_then(|g| g.upstream.clone());
        if let Some(g) = generation(&m, &builder, previous) {
            tx.send_replace(Some(Arc::new(g)));
        }
        tokio::select! {
            () = stop.cancelled() => return,
            r = rx.changed() => if r.is_err() { return },
        }
    }
}

/// The TLS generation for `m`; `None` until the CA and the serving cert load.
fn generation(
    m: &CertMaterial,
    builder: &TlsBuilder,
    previous: Option<Arc<ClientConfig>>,
) -> Option<EdgeTls> {
    let (ca, serving) = (m.client_ca()?, m.serving()?);
    let mut g = match builder.build(serving, ca.clone()) {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "edge TLS config not built; keeping the previous");
            return None;
        }
    };
    g.upstream = upstream_config(m, builder.provider(), previous);
    Some(g)
}

/// The worker-leg config for `m`, or `previous` when `m` cannot give one.
fn upstream_config(
    m: &CertMaterial,
    provider: &Arc<CryptoProvider>,
    previous: Option<Arc<ClientConfig>>,
) -> Option<Arc<ClientConfig>> {
    let (Some(c), Some(internal)) = (m.edge_client(), m.internal_ca()) else {
        return previous;
    };
    match upstream::client_config(
        Arc::clone(provider),
        internal.clone(),
        c.chain.clone(),
        c.key.clone_key(),
    ) {
        Ok(u) => Some(u),
        Err(e) => {
            warn!(error = %e, "edge upstream config not built; keeping the previous");
            previous
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use time::OffsetDateTime;

    use super::*;
    use crate::certs::CaCertRule;
    use crate::pki::CaBundle;

    #[test]
    fn a_snapshot_without_an_edge_client_cert_keeps_the_previous_upstream() {
        let now = OffsetDateTime::now_utc();
        let bundle = CaBundle::generate(now).unwrap();
        let c = bundle
            .issue_edge_client_for(now, crate::pki::LeafValidity::default().edge_client)
            .unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let previous = upstream::client_config(
            Arc::clone(&provider),
            bundle.internal_ca().cert_der().clone(),
            vec![c.cert_der().clone()],
            c.private_key_der().unwrap(),
        )
        .unwrap();
        let m = CertMaterial::from_bundle(Arc::new(bundle), CaCertRule::ClientCa);
        assert!(m.edge_client().is_none());
        let kept = upstream_config(&m, &provider, Some(Arc::clone(&previous))).unwrap();
        assert!(Arc::ptr_eq(&kept, &previous));
        assert!(upstream_config(&m, &provider, None).is_none());
    }
}
