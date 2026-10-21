//! Edge TLS from the shared certificate material (`crate::certs`): each
//! snapshot with a client CA and a serving cert becomes one [`EdgeTls`]
//! generation. A snapshot that fails to build is not published; the previous
//! generation stays.

use std::sync::Arc;

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
        if let Some(g) = generation(&m, &builder) {
            tx.send_replace(Some(Arc::new(g)));
        }
        tokio::select! {
            () = stop.cancelled() => return,
            r = rx.changed() => if r.is_err() { return },
        }
    }
}

/// The TLS generation for `m`; `None` until the CA and the serving cert load.
fn generation(m: &CertMaterial, builder: &TlsBuilder) -> Option<EdgeTls> {
    let (ca, serving) = (m.client_ca()?, m.serving()?);
    let mut g = match builder.build(serving, ca.clone()) {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "edge TLS config not built; keeping the previous");
            return None;
        }
    };
    if let (Some(c), Some(internal)) = (m.edge_client(), m.internal_ca()) {
        match upstream::client_config(
            Arc::clone(builder.provider()),
            internal.clone(),
            c.chain.clone(),
            c.key.clone_key(),
        ) {
            Ok(u) => g.upstream = Some(u),
            Err(e) => warn!(error = %e, "edge upstream config not built"),
        }
    }
    Some(g)
}
