//! The reload loop: [`Loader`] is the synchronous state machine (testable
//! with plain values), [`drive`] feeds it from the Secret watches and timers.

use std::sync::Arc;
use std::time::Duration;

use rustls::sign::CertifiedKey;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::{CaCertRule, CertMaterial, EdgeClient, InvalidServingSecret, secret_data};
use crate::pki::{
    CaBundle, CertKind, EdgeNames, IssuedCert, LeafValidity, LoadOutcome, ServingCertError,
    UserServingCert, sanitize_ca_pem,
};
use crate::runtime::metrics::Metrics;
use crate::runtime::readiness::ReadyFlag;
use crate::runtime::stores::SecretState;

/// Retry cadence while a reissue is pending.
pub const RETRY: Duration = Duration::from_secs(30);

/// Shortest wake after a renewal time. A cert whose renewal time is already
/// due (its validity is too short) must not spin the loop.
pub const MIN_WAKE: Duration = Duration::from_secs(1);

pub(super) struct Config {
    pub user_serving: bool,
    pub server_name: String,
    pub names: Result<EdgeNames, String>,
    /// Self-issued serving and edge client validity.
    pub validity: LeafValidity,
}

type IssueFn =
    Box<dyn Fn(&CaBundle, &EdgeNames, OffsetDateTime) -> Result<IssuedCert, String> + Send>;

struct Serving {
    key: Arc<CertifiedKey>,
    /// Self-issued only.
    renew_at: Option<OffsetDateTime>,
}

struct Client {
    material: Arc<EdgeClient>,
    renew_at: Option<OffsetDateTime>,
}

/// What a step did.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Step {
    /// The snapshot differs from the last published one.
    pub changed: bool,
    /// When to run the next step without a Secret event.
    pub wake: Option<Duration>,
}

pub(super) struct Loader {
    cfg: Config,
    metrics: Arc<Metrics>,
    ready_ca: ReadyFlag,
    ready_serving: ReadyFlag,
    issue_serving: IssueFn,
    bundle: Option<Arc<CaBundle>>,
    usable: bool,
    serving: Option<Serving>,
    client: Option<Client>,
    rule_ca_crt: Option<String>,
    /// Set on a CA change and on any failed issue; cleared when every
    /// in-memory cert is current. Retried every [`RETRY`].
    reissue_pending: bool,
    serving_failing: bool,
    ca_rv: Option<String>,
    srv_rv: Option<String>,
    /// A user Secret rejected only for a future `notBefore`, with the instant
    /// to validate it again.
    srv_later: Option<(SecretState, OffsetDateTime)>,
}

fn rv(s: &SecretState) -> String {
    s.secret.as_ref().map_or_else(
        || "absent".to_owned(),
        |x| {
            format!(
                "rv:{}",
                x.metadata.resource_version.as_deref().unwrap_or_default()
            )
        },
    )
}

/// `Some` when the user Secret exists; its verdict.
fn validate_user(
    s: &SecretState,
    server_name: &str,
    now: OffsetDateTime,
) -> Option<Result<UserServingCert, ServingCertError>> {
    s.secret.as_ref()?;
    let data = secret_data(s).unwrap_or_default();
    let (Some(crt), Some(key)) = (data.get("tls.crt"), data.get("tls.key")) else {
        return Some(Err(ServingCertError::Missing));
    };
    Some(UserServingCert::from_tls_secret(crt, key, server_name, now))
}

/// The startup decision: a configured user Secret that exists
/// but is invalid ends startup (exit 2). An absent Secret waits.
///
/// # Errors
/// [`InvalidServingSecret`] carrying the reason.
pub fn startup_decision(
    user_serving: bool,
    state: &SecretState,
    server_name: &str,
    now: OffsetDateTime,
) -> Result<(), InvalidServingSecret> {
    if !user_serving {
        return Ok(());
    }
    match validate_user(state, server_name, now) {
        // Not valid yet but able to become valid: wait, not ready, and re-check.
        Some(Err(ServingCertError::NotYetValid(_))) => Ok(()),
        Some(Err(e)) => Err(InvalidServingSecret(e.to_string())),
        _ => Ok(()),
    }
}

fn key_for(
    c: &IssuedCert,
    provider: &rustls::crypto::CryptoProvider,
) -> Result<Arc<CertifiedKey>, String> {
    let key = c
        .private_key_der()
        .ok_or_else(|| "issued cert has no key".to_owned())?;
    let signer = provider
        .key_provider
        .load_private_key(key)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(CertifiedKey::new(
        vec![c.cert_der().clone()],
        signer,
    )))
}

impl Loader {
    pub fn new(
        cfg: Config,
        metrics: Arc<Metrics>,
        ready_ca: ReadyFlag,
        ready_serving: ReadyFlag,
    ) -> Self {
        let v = cfg.validity.edge_serving;
        Self {
            rule_ca_crt: None,
            cfg,
            metrics,
            ready_ca,
            ready_serving,
            issue_serving: Box::new(move |b, n, now| {
                b.issue_edge_serving_for(n, now, v)
                    .map_err(|e| e.to_string())
            }),
            bundle: None,
            usable: false,
            serving: None,
            client: None,
            reissue_pending: false,
            serving_failing: false,
            ca_rv: None,
            srv_rv: None,
            srv_later: None,
        }
    }

    #[cfg(test)]
    pub fn with_issue_serving(mut self, f: IssueFn) -> Self {
        self.issue_serving = f;
        self
    }

    pub fn snapshot(&self) -> CertMaterial {
        CertMaterial {
            bundle: self.bundle.clone(),
            usable: self.usable,
            edge_client: self.client.as_ref().map(|c| Arc::clone(&c.material)),
            serving: self.serving.as_ref().map(|s| Arc::clone(&s.key)),
            rule: if self.cfg.user_serving {
                CaCertRule::Configured(self.rule_ca_crt.clone())
            } else {
                CaCertRule::ClientCa
            },
        }
    }

    fn on_ca(&mut self, s: &SecretState, now: OffsetDateTime) -> bool {
        let id = rv(s);
        if self.ca_rv.as_ref() == Some(&id) {
            return false;
        }
        self.ca_rv = Some(id);
        match CaBundle::load(secret_data(s).as_ref(), now) {
            LoadOutcome::Valid(b) => {
                self.metrics
                    .set_cert_not_after(CertKind::ClientCa, b.client_ca().not_after());
                self.metrics
                    .set_cert_not_after(CertKind::InternalCa, b.internal_ca().not_after());
                self.bundle = Some(Arc::new(*b));
                self.usable = true;
                self.reissue_pending = true;
                self.metrics.set_ca_valid(true);
                self.ready_ca.set();
            }
            LoadOutcome::Missing => {
                self.usable = false;
                self.metrics.set_ca_valid(false);
            }
            LoadOutcome::Invalid(e) => {
                self.usable = false;
                self.metrics.set_ca_valid(false);
                self.metrics.cert_load_error("ca");
                warn!(
                    reason = e.reason(),
                    "CA Secret not usable; keeping the last trust"
                );
            }
        }
        true
    }

    fn on_user_serving(&mut self, s: &SecretState, now: OffsetDateTime) -> bool {
        let id = rv(s);
        if self.srv_rv.as_ref() == Some(&id) {
            return false;
        }
        self.srv_rv = Some(id);
        self.apply_user(s, now);
        true
    }

    /// Validates a Secret held back for a future `notBefore` once it is due.
    fn recheck_user(&mut self, now: OffsetDateTime) -> bool {
        let Some((s, _)) = self.srv_later.take_if(|(_, at)| now >= *at) else {
            return false;
        };
        self.apply_user(&s, now);
        true
    }

    fn apply_user(&mut self, s: &SecretState, now: OffsetDateTime) {
        self.srv_later = None;
        match validate_user(s, &self.cfg.server_name, now) {
            Some(Ok(c)) => {
                // Moves with the cert it came with; kept when the Secret is deleted.
                self.rule_ca_crt = s
                    .secret
                    .as_ref()
                    .and_then(|x| x.data.as_ref())
                    .and_then(|d| d.get("ca.crt"))
                    .and_then(|v| sanitize_ca_pem(&v.0));
                self.metrics
                    .set_cert_not_after(CertKind::EdgeServing, c.not_after());
                self.serving = Some(Serving {
                    key: c.certified_key(),
                    renew_at: None,
                });
                self.ready_serving.set();
            }
            Some(Err(ServingCertError::NotYetValid(at))) => {
                self.metrics.cert_load_error("edge_serving");
                warn!(not_before = %at, "user serving cert not valid yet; keeping the previous cert and re-checking then");
                self.srv_later = Some((s.clone(), at));
            }
            Some(Err(e)) => {
                self.metrics.cert_load_error("edge_serving");
                warn!(error = %e, "user serving Secret rejected; keeping the previous cert");
            }
            None => {}
        }
    }

    /// Applies the Secret states that changed (`None`: unchanged) and any due
    /// reissue, then reports the next wake-up.
    pub fn step(
        &mut self,
        ca: Option<&SecretState>,
        srv: Option<&SecretState>,
        now: OffsetDateTime,
    ) -> Step {
        let mut changed = ca.is_some_and(|s| self.on_ca(s, now));
        if self.cfg.user_serving {
            changed |= srv.is_some_and(|s| self.on_user_serving(s, now));
            changed |= self.recheck_user(now);
        }
        changed |= self.reissue(now);
        let until = |t: OffsetDateTime| {
            Duration::try_from(t - now)
                .unwrap_or(Duration::ZERO)
                .max(MIN_WAKE)
        };
        let renew = [
            self.serving.as_ref().and_then(|s| s.renew_at),
            self.client.as_ref().and_then(|c| c.renew_at),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(until);
        // Issuing needs a usable CA (the next CA event wakes the loop
        // otherwise); the held-back user cert does not.
        let issue = if !self.usable {
            None
        } else if self.reissue_pending {
            Some(RETRY)
        } else {
            renew
        };
        let recheck = self.srv_later.as_ref().map(|(_, at)| until(*at));
        let wake = [issue, recheck].into_iter().flatten().min();
        Step { changed, wake }
    }

    fn reissue(&mut self, now: OffsetDateTime) -> bool {
        let Some(b) = self.bundle.clone().filter(|_| self.usable) else {
            return false;
        };
        let due = |renew: Option<OffsetDateTime>| renew.is_some_and(|t| now >= t);
        let mut changed = false;
        let mut failed = false;
        if self.reissue_pending || self.client.as_ref().is_none_or(|c| due(c.renew_at)) {
            match b
                .issue_edge_client_for(now, self.cfg.validity.edge_client)
                .map_err(|e| e.to_string())
                .and_then(|c| {
                    let key = c.private_key_der().ok_or("issued cert has no key")?;
                    self.metrics
                        .set_cert_not_after(CertKind::EdgeClient, c.not_after());
                    Ok(Client {
                        material: Arc::new(EdgeClient {
                            chain: vec![c.cert_der().clone()],
                            key,
                        }),
                        renew_at: c.renew_at(),
                    })
                }) {
                Ok(c) => {
                    info!(renew_at = ?c.renew_at, "edge client cert issued");
                    self.client = Some(c);
                    changed = true;
                }
                Err(e) => {
                    error!(error = %e, "issuing the edge client cert");
                    failed = true;
                }
            }
        }
        if !self.cfg.user_serving
            && (self.reissue_pending || self.serving.as_ref().is_none_or(|s| due(s.renew_at)))
        {
            let provider = rustls::crypto::ring::default_provider();
            let r = match &self.cfg.names {
                Ok(n) => (self.issue_serving)(&b, n, now).and_then(|c| {
                    self.metrics
                        .set_cert_not_after(CertKind::EdgeServing, c.not_after());
                    Ok(Serving {
                        key: key_for(&c, &provider)?,
                        renew_at: c.renew_at(),
                    })
                }),
                Err(e) => Err(e.clone()),
            };
            match r {
                Ok(s) => {
                    info!(renew_at = ?s.renew_at, "edge serving cert issued");
                    self.serving = Some(s);
                    self.serving_failing = false;
                    self.ready_serving.set();
                    changed = true;
                }
                Err(e) => {
                    if !self.serving_failing {
                        self.metrics.cert_load_error("edge_serving");
                    }
                    self.serving_failing = true;
                    error!(error = %e, "self-issuing the edge serving cert");
                    failed = true;
                }
            }
        }
        self.reissue_pending = failed;
        changed
    }
}

/// Runs until `stop`, publishing each changed snapshot into `tx`.
pub(super) async fn drive(
    mut loader: Loader,
    mut ca_rx: watch::Receiver<SecretState>,
    mut srv_rx: watch::Receiver<SecretState>,
    tx: watch::Sender<CertMaterial>,
    stop: CancellationToken,
) {
    let (mut ca_dirty, mut srv_dirty) = (true, true);
    loop {
        let ca = ca_dirty.then(|| ca_rx.borrow_and_update().clone());
        let srv = srv_dirty.then(|| srv_rx.borrow_and_update().clone());
        let out = loader.step(ca.as_ref(), srv.as_ref(), OffsetDateTime::now_utc());
        if out.changed {
            tx.send_replace(loader.snapshot());
        }
        (ca_dirty, srv_dirty) = (false, false);
        tokio::select! {
            () = stop.cancelled() => return,
            r = ca_rx.changed() => { if r.is_err() { return } ca_dirty = true; }
            r = srv_rx.changed(), if loader.cfg.user_serving => {
                if r.is_err() { return }
                srv_dirty = true;
            }
            () = async {
                match out.wake {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
    }
}
