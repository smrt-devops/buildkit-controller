//! End-to-end tests with an in-process HTTPS test issuer serving discovery and
//! JWKS on 127.0.0.1, keys generated per test, and a fake `TokenReviewer`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};
use ring::rand::SystemRandom;
use ring::signature::{self as sig, EcdsaKeyPair, Ed25519KeyPair, KeyPair as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

const AUD: &str = "buildkit-controller";

// ---------- keys and tokens ----------

enum TestKey {
    Es256(EcdsaKeyPair),
    Ed(Ed25519KeyPair),
    Rsa(sig::RsaKeyPair, &'static str),
}

struct Signer {
    kid: String,
    key: TestKey,
}

impl Signer {
    fn es256(kid: &str) -> Self {
        let rng = SystemRandom::new();
        let alg = &sig::ECDSA_P256_SHA256_FIXED_SIGNING;
        let der = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(alg, der.as_ref(), &rng).unwrap();
        Self {
            kid: kid.into(),
            key: TestKey::Es256(key),
        }
    }

    fn ed(kid: &str) -> Self {
        let der = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(der.as_ref()).unwrap();
        Self {
            kid: kid.into(),
            key: TestKey::Ed(key),
        }
    }

    /// RSA signer over a committed throwaway 2048-bit test key (ring cannot
    /// generate RSA keys). Its JWK declares no `alg`, so a padding mismatch is
    /// caught by the signature check, not by `check_jwk`.
    fn rsa(kid: &str, alg: &'static str) -> Self {
        let pkcs8 = include_bytes!("testdata/rsa2048-test-only.pk8");
        Self {
            kid: kid.into(),
            key: TestKey::Rsa(sig::RsaKeyPair::from_pkcs8(pkcs8).unwrap(), alg),
        }
    }

    fn alg(&self) -> &'static str {
        match self.key {
            TestKey::Es256(_) => "ES256",
            TestKey::Ed(_) => "EdDSA",
            TestKey::Rsa(_, alg) => alg,
        }
    }

    fn jwk(&self) -> Value {
        match &self.key {
            TestKey::Es256(k) => {
                let p = k.public_key().as_ref();
                json!({"kty": "EC", "crv": "P-256", "kid": self.kid, "use": "sig", "alg": "ES256",
                       "x": B64.encode(&p[1..33]), "y": B64.encode(&p[33..65])})
            }
            TestKey::Ed(k) => json!({"kty": "OKP", "crv": "Ed25519", "kid": self.kid,
                                     "x": B64.encode(k.public_key().as_ref())}),
            TestKey::Rsa(k, _) => json!({"kty": "RSA", "kid": self.kid, "use": "sig",
                "n": B64.encode(&sig::RsaPublicKeyComponents::<Vec<u8>>::from(k.public()).n),
                "e": B64.encode(&sig::RsaPublicKeyComponents::<Vec<u8>>::from(k.public()).e)}),
        }
    }

    fn public_bytes(&self) -> Vec<u8> {
        match &self.key {
            TestKey::Es256(k) => k.public_key().as_ref().to_vec(),
            TestKey::Ed(k) => k.public_key().as_ref().to_vec(),
            TestKey::Rsa(k, _) => k.public().as_ref().to_vec(),
        }
    }

    fn token(&self, claims: &Value) -> String {
        self.token_with_header(
            &json!({"alg": self.alg(), "kid": self.kid, "typ": "JWT"}),
            claims,
        )
    }

    fn token_with_header(&self, header: &Value, claims: &Value) -> String {
        let msg = format!(
            "{}.{}",
            B64.encode(header.to_string()),
            B64.encode(claims.to_string())
        );
        let signature = match &self.key {
            TestKey::Es256(k) => k
                .sign(&SystemRandom::new(), msg.as_bytes())
                .unwrap()
                .as_ref()
                .to_vec(),
            TestKey::Ed(k) => k.sign(msg.as_bytes()).as_ref().to_vec(),
            TestKey::Rsa(k, alg) => {
                let padding: &'static dyn sig::RsaEncoding = match *alg {
                    "PS256" => &sig::RSA_PSS_SHA256,
                    _ => &sig::RSA_PKCS1_SHA256,
                };
                let mut out = vec![0; k.public().modulus_len()];
                k.sign(padding, &SystemRandom::new(), msg.as_bytes(), &mut out)
                    .unwrap();
                out
            }
        };
        format!("{msg}.{}", B64.encode(signature))
    }
}

fn now() -> i64 {
    i64::try_from(jsonwebtoken::get_current_timestamp()).unwrap()
}

fn claims(iss: &str) -> Value {
    json!({"iss": iss, "aud": AUD, "sub": "repo:smrt-devops/app:ref:refs/heads/main",
           "exp": now() + 300, "iat": now(), "repository_owner": "smrt-devops"})
}

// ---------- HTTPS test issuer ----------

#[derive(Default)]
struct IssuerState {
    jwks: Value,
    jwks_fetches: usize,
    discovery_issuer: Option<String>,
    fail: bool,
    hang: bool,
    /// Delays each response by 500 ms.
    slow: bool,
}

struct TestIssuer {
    url: String,
    cert: rustls::pki_types::CertificateDer<'static>,
    roots: rustls::RootCertStore,
    state: Arc<Mutex<IssuerState>>,
}

impl TestIssuer {
    async fn start(keys: &[&Signer]) -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let leaf = cert_der.clone();
        let key_der =
            rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der.clone()).unwrap();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "https://127.0.0.1:{}",
            listener.local_addr().unwrap().port()
        );
        let state = Arc::new(Mutex::new(IssuerState {
            jwks: json!({"keys": keys.iter().map(|k| k.jwk()).collect::<Vec<_>>()}),
            discovery_issuer: Some(url.clone()),
            ..IssuerState::default()
        }));
        let (st, base) = (state.clone(), url.clone());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, st, base) = (acceptor.clone(), st.clone(), base.clone());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let path = String::from_utf8_lossy(&req)
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    if st.lock().unwrap().hang {
                        tokio::time::sleep(std::time::Duration::from_mins(1)).await;
                    }
                    if st.lock().unwrap().slow {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    let (status, body) = respond(&st, &base, &path);
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nCache-Control: max-age=600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tls.write_all(resp.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        Self {
            url,
            cert: leaf,
            roots,
            state,
        }
    }

    fn rotate(&self, keys: &[&Signer]) {
        self.state.lock().unwrap().jwks =
            json!({"keys": keys.iter().map(|k| k.jwk()).collect::<Vec<_>>()});
    }

    fn fetches(&self) -> usize {
        self.state.lock().unwrap().jwks_fetches
    }

    /// An authenticator whose shared client trusts nothing; the issuer is
    /// reachable only through its own `ca_certs`.
    fn auth_with_private_ca(&self, trusted: bool) -> Authenticator<FakeReviewer> {
        let cfg = AuthConfig {
            oidc: vec![OidcIssuer {
                url: self.url.clone(),
                audiences: vec![AUD.into()],
                jwks_url: None,
                ca_certs: if trusted {
                    vec![self.cert.clone()]
                } else {
                    Vec::new()
                },
            }],
            service_account_audiences: None,
        };
        let http = https_client(rustls::RootCertStore::empty()).unwrap();
        Authenticator::new(cfg, http, FakeReviewer::default()).unwrap()
    }

    fn auth(&self, sa: Option<FakeReviewer>) -> Authenticator<FakeReviewer> {
        let cfg = AuthConfig {
            oidc: vec![OidcIssuer {
                url: self.url.clone(),
                audiences: vec![AUD.into()],
                jwks_url: None,
                ca_certs: Vec::new(),
            }],
            service_account_audiences: sa.as_ref().map(|_| vec![AUD.to_owned()]),
        };
        let http = https_client(self.roots.clone()).unwrap();
        Authenticator::new(cfg, http, sa.unwrap_or_default()).unwrap()
    }
}

fn respond(st: &Mutex<IssuerState>, base: &str, path: &str) -> (&'static str, String) {
    let mut st = st.lock().unwrap();
    if st.fail {
        return ("500 Internal Server Error", "{}".into());
    }
    match path {
        "/.well-known/openid-configuration" => (
            "200 OK",
            json!({"issuer": st.discovery_issuer, "jwks_uri": format!("{base}/jwks")}).to_string(),
        ),
        "/jwks" => {
            st.jwks_fetches += 1;
            ("200 OK", st.jwks.to_string())
        }
        _ => ("404 Not Found", "{}".into()),
    }
}

// ---------- TokenReview fake ----------

#[derive(Default)]
struct FakeReviewer {
    reply: Option<TokenReviewStatus>,
    upstream_error: bool,
    /// Each review waits for one permit (a stalled API server).
    gate: Option<Arc<tokio::sync::Semaphore>>,
    calls: AtomicUsize,
    seen_audiences: Mutex<Vec<String>>,
}

impl FakeReviewer {
    fn replying(authenticated: bool, audiences: &[&str], username: &str) -> Self {
        Self {
            reply: Some(TokenReviewStatus {
                authenticated: Some(authenticated),
                audiences: Some(audiences.iter().map(|a| (*a).to_owned()).collect()),
                user: Some(UserInfo {
                    username: Some(username.into()),
                    uid: Some("uid-1".into()),
                    ..UserInfo::default()
                }),
                ..TokenReviewStatus::default()
            }),
            ..Self::default()
        }
    }
}

impl TokenReviewer for FakeReviewer {
    async fn review(
        &self,
        _token: &str,
        audiences: &[String],
    ) -> Result<TokenReviewStatus, ReviewError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(g) = &self.gate {
            g.acquire().await.unwrap().forget();
        }
        *self.seen_audiences.lock().unwrap() = audiences.to_vec();
        if self.upstream_error {
            return Err(ReviewError::Upstream("timeout".into()));
        }
        self.reply
            .clone()
            .ok_or_else(|| ReviewError::Unexpected("no reply configured".into()))
    }
}

fn assert_invalid(r: Result<Identity, AuthError>, reason: &str) {
    match r {
        Err(AuthError::Invalid(got)) => assert_eq!(got, reason),
        other => panic!("expected Invalid({reason}), got {other:?}"),
    }
}

// ---------- OIDC ----------

#[tokio::test]
async fn valid_es256_and_eddsa_tokens() {
    let (es, ed) = (Signer::es256("k1"), Signer::ed("k2"));
    let iss = TestIssuer::start(&[&es, &ed]).await;
    let auth = iss.auth(None);
    for signer in [&es, &ed] {
        let id = auth
            .authenticate(&signer.token(&claims(&iss.url)))
            .await
            .unwrap();
        assert_eq!(
            id.owner(),
            format!("oidc:{}#repo:smrt-devops/app:ref:refs/heads/main", iss.url)
        );
        let Identity::Oidc { claims, .. } = id else {
            panic!("oidc")
        };
        assert_eq!(claims["repository_owner"], "smrt-devops");
    }
    assert_eq!(iss.fetches(), 1, "keys are cached");
}

#[tokio::test]
async fn time_claims_with_leeway() {
    let k = Signer::es256("k1");
    let iss = TestIssuer::start(&[&k]).await;
    let auth = iss.auth(None);
    let with = |field: &str, v: i64| {
        let mut c = claims(&iss.url);
        c[field] = json!(v);
        k.token(&c)
    };
    assert_invalid(
        auth.authenticate(&with("exp", now() - 120)).await,
        "expired",
    );
    auth.authenticate(&with("exp", now() - 30)).await.unwrap();
    assert_invalid(
        auth.authenticate(&with("nbf", now() + 300)).await,
        "not yet valid",
    );
    auth.authenticate(&with("nbf", now() + 30)).await.unwrap();
    assert_invalid(
        auth.authenticate(&with("iat", now() + 300)).await,
        "iat invalid or in the future",
    );
}

#[tokio::test]
async fn audience_and_required_claims() {
    let k = Signer::es256("k1");
    let iss = TestIssuer::start(&[&k]).await;
    let auth = iss.auth(None);
    let mut c = claims(&iss.url);
    c["aud"] = json!("other");
    assert_invalid(auth.authenticate(&k.token(&c)).await, "audience mismatch");
    c["aud"] = json!(["other", AUD]);
    auth.authenticate(&k.token(&c)).await.unwrap();
    for field in ["sub", "exp", "aud"] {
        let mut c = claims(&iss.url);
        c.as_object_mut().unwrap().remove(field);
        assert_invalid(
            auth.authenticate(&k.token(&c)).await,
            "missing required claim",
        );
    }
    let mut c = claims(&iss.url);
    c["sub"] = json!("");
    assert_invalid(auth.authenticate(&k.token(&c)).await, "sub missing");
}

#[tokio::test]
async fn wrong_issuer_never_uses_oidc_keys() {
    let k = Signer::es256("k1");
    let iss = TestIssuer::start(&[&k]).await;
    let token = k.token(&claims("https://evil.example"));
    assert!(matches!(
        iss.auth(None).authenticate(&token).await,
        Err(AuthError::UnknownIssuer)
    ));
    // With SA auth on, an unknown issuer goes to TokenReview only if the token
    // has SA shape; this one does not, so it is refused locally.
    let auth = iss.auth(Some(FakeReviewer::replying(false, &[], "")));
    assert_invalid(
        auth.authenticate(&token).await,
        "not a service account token",
    );
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(iss.fetches(), 0);
}

#[tokio::test]
async fn unknown_kid_refetches_once_then_rate_limits() {
    let (k1, k2, k3) = (
        Signer::es256("k1"),
        Signer::es256("k2"),
        Signer::es256("k3"),
    );
    let iss = TestIssuer::start(&[&k1]).await;
    let auth = iss.auth(None);
    auth.authenticate(&k1.token(&claims(&iss.url)))
        .await
        .unwrap();
    iss.rotate(&[&k2, &k3]);
    auth.authenticate(&k2.token(&claims(&iss.url)))
        .await
        .unwrap();
    assert_eq!(iss.fetches(), 2, "unknown kid triggered one refetch");
    // k1 was rotated out; another unknown kid inside 30 s does not refetch.
    let k4 = Signer::es256("k4");
    assert_invalid(
        auth.authenticate(&k4.token(&claims(&iss.url))).await,
        "unknown kid",
    );
    assert_invalid(
        auth.authenticate(&k1.token(&claims(&iss.url))).await,
        "unknown kid",
    );
    assert_eq!(iss.fetches(), 2);
    auth.authenticate(&k3.token(&claims(&iss.url)))
        .await
        .unwrap();
}

#[tokio::test]
async fn alg_none_hmac_confusion_and_mismatch_rejected() {
    let k = Signer::es256("k1");
    let iss = TestIssuer::start(&[&k]).await;
    let auth = iss.auth(Some(FakeReviewer::default()));
    let c = claims(&iss.url);
    let body = format!(
        "{}.{}",
        B64.encode(json!({"alg": "none", "kid": "k1"}).to_string()),
        B64.encode(c.to_string())
    );
    {
        let token = format!("{body}.");
        assert!(matches!(
            auth.authenticate(&token).await,
            Err(AuthError::Invalid(_))
        ));
    }
    // HS256 keyed with the issuer's public key (classic alg confusion).
    let jwks = json!({"keys": [k.jwk()]}).to_string();
    for secret in [k.public_bytes(), jwks.into_bytes()] {
        let header = json!({"alg": "HS256", "kid": "k1"});
        let msg = format!(
            "{}.{}",
            B64.encode(header.to_string()),
            B64.encode(c.to_string())
        );
        let mac = ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &secret),
            msg.as_bytes(),
        );
        let token = format!("{msg}.{}", B64.encode(mac.as_ref()));
        assert_invalid(auth.authenticate(&token).await, "alg not allowed");
    }
    // A P-256 key may not verify an ES384 or EdDSA header.
    for alg in ["ES384", "EdDSA", "RS256"] {
        let token = k.token_with_header(&json!({"alg": alg, "kid": "k1"}), &c);
        assert_invalid(auth.authenticate(&token).await, "alg does not match jwk");
    }
    // No kid, tampered payload.
    assert_invalid(
        auth.authenticate(&k.token_with_header(&json!({"alg": "ES256"}), &c))
            .await,
        "missing kid",
    );
    let good = k.token(&c);
    let parts: Vec<&str> = good.split('.').collect();
    let mut evil = c.clone();
    evil["sub"] = json!("admin");
    let forged = format!("{}.{}.{}", parts[0], B64.encode(evil.to_string()), parts[2]);
    assert_invalid(auth.authenticate(&forged).await, "bad signature");
    assert_eq!(
        auth.reviewer.calls.load(Ordering::SeqCst),
        0,
        "configured iss never falls back to TokenReview"
    );
}

#[tokio::test]
async fn issuer_unreachable_or_mismatched_fails_closed() {
    let k = Signer::es256("k1");
    let iss = TestIssuer::start(&[&k]).await;
    iss.state.lock().unwrap().fail = true;
    let r = iss
        .auth(None)
        .authenticate(&k.token(&claims(&iss.url)))
        .await;
    assert!(matches!(r, Err(AuthError::KeysUnavailable)), "{r:?}");

    let iss = TestIssuer::start(&[&k]).await;
    iss.state.lock().unwrap().discovery_issuer = Some("https://other".into());
    let r = iss
        .auth(None)
        .authenticate(&k.token(&claims(&iss.url)))
        .await;
    assert!(matches!(r, Err(AuthError::KeysUnavailable)), "{r:?}");
}

#[tokio::test]
async fn cached_keys_survive_issuer_outage() {
    let k = Signer::es256("k1");
    let iss = TestIssuer::start(&[&k]).await;
    let auth = iss.auth(None);
    auth.authenticate(&k.token(&claims(&iss.url)))
        .await
        .unwrap();
    iss.state.lock().unwrap().fail = true;
    auth.authenticate(&k.token(&claims(&iss.url)))
        .await
        .unwrap();
}

#[test]
fn plain_http_issuer_rejected_at_construction() {
    let cfg = AuthConfig {
        oidc: vec![OidcIssuer {
            url: "http://127.0.0.1:1".into(),
            audiences: vec![AUD.into()],
            jwks_url: None,
            ca_certs: Vec::new(),
        }],
        service_account_audiences: None,
    };
    let http = https_client(rustls::RootCertStore::empty()).unwrap();
    assert!(Authenticator::new(cfg, http, FakeReviewer::default()).is_err());
}

// ---------- TokenReview ----------

async fn review(fake: FakeReviewer) -> (Result<Identity, AuthError>, Vec<String>) {
    let cfg = AuthConfig {
        oidc: vec![],
        service_account_audiences: Some(vec![AUD.into()]),
    };
    let http = https_client(rustls::RootCertStore::empty()).unwrap();
    let auth = Authenticator::new(cfg, http, fake).unwrap();
    let r = auth.authenticate(&sa_token(&sa_claims())).await;
    let seen = auth.reviewer.seen_audiences.lock().unwrap().clone();
    (r, seen)
}

#[tokio::test]
async fn token_review_outcomes() {
    let (r, seen) = review(FakeReviewer::replying(
        true,
        &[AUD],
        "system:serviceaccount:ci:builder",
    ))
    .await;
    let id = r.unwrap();
    assert_eq!(seen, vec![AUD.to_owned()]);
    assert_eq!(id.owner(), "sa:ci/builder");
    assert_eq!(
        id,
        Identity::ServiceAccount {
            namespace: "ci".into(),
            name: "builder".into(),
            uid: "uid-1".into()
        }
    );

    let (r, _) = review(FakeReviewer::replying(
        false,
        &[AUD],
        "system:serviceaccount:ci:builder",
    ))
    .await;
    assert_invalid(r, "tokenreview: not authenticated");
    let (r, _) = review(FakeReviewer::replying(
        true,
        &["https://kubernetes.default.svc"],
        "system:serviceaccount:ci:builder",
    ))
    .await;
    assert_invalid(r, "tokenreview: audience mismatch");
    let (r, _) = review(FakeReviewer::replying(
        true,
        &[],
        "system:serviceaccount:ci:builder",
    ))
    .await;
    assert_invalid(r, "tokenreview: audience mismatch");
    for user in [
        "alice",
        "system:serviceaccount:ci",
        "system:serviceaccount::x",
        "system:serviceaccount:a:b:c",
    ] {
        let (r, _) = review(FakeReviewer::replying(true, &[AUD], user)).await;
        assert_invalid(r, "tokenreview: not a service account");
    }
    let (r, _) = review(FakeReviewer {
        upstream_error: true,
        ..FakeReviewer::default()
    })
    .await;
    let e = r.unwrap_err();
    assert_eq!((e.status(), e.code()), (503, "upstream_error"));
    let (r, _) = review(FakeReviewer::default()).await;
    assert_eq!(r.unwrap_err().status(), 500);
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Claims of a projected service-account token for this server.
fn sa_claims() -> Value {
    json!({
        "iss": "https://kubernetes.default.svc.cluster.local",
        "sub": "system:serviceaccount:ci:builder",
        "aud": [AUD],
        "exp": now_secs() + 600,
        "iat": now_secs(),
    })
}

/// An unsigned token of SA shape: the controller never verifies it, `TokenReview` does.
fn sa_token(claims: &Value) -> String {
    sa_token_alg("RS256", claims)
}

fn sa_token_alg(alg: &str, claims: &Value) -> String {
    let h = B64.encode(json!({"alg": alg, "kid": "k"}).to_string());
    format!("{h}.{}.c2ln", B64.encode(claims.to_string()))
}

fn sa_auth(fake: FakeReviewer) -> Authenticator<FakeReviewer> {
    let cfg = AuthConfig {
        oidc: vec![],
        service_account_audiences: Some(vec![AUD.into()]),
    };
    let http = https_client(rustls::RootCertStore::empty()).unwrap();
    Authenticator::new(cfg, http, fake).unwrap()
}

fn sa_ok() -> FakeReviewer {
    FakeReviewer::replying(true, &[AUD], "system:serviceaccount:ci:builder")
}

/// Tokens that cannot be service-account tokens for this server are refused
/// locally: no `TokenReview` is sent for them.
#[tokio::test]
async fn non_sa_tokens_never_reach_token_review() {
    let auth = sa_auth(sa_ok());
    let mut c = sa_claims();
    let mut bad = vec![
        "x".to_owned(),
        "a.b.c".to_owned(),
        "a.b".to_owned(),
        format!("{}.", sa_token(&c)),
        sa_token_alg("none", &c),
        sa_token_alg("HS256", &c),
    ];
    c["aud"] = json!("https://kubernetes.default.svc.cluster.local");
    bad.push(sa_token(&c));
    c["aud"] = json!([]);
    bad.push(sa_token(&c));
    let mut c = sa_claims();
    c["exp"] = json!(now_secs() - 3600);
    bad.push(sa_token(&c));
    c["exp"] = json!("never");
    bad.push(sa_token(&c));
    let mut c = sa_claims();
    c["sub"] = json!("repo:acme/app:ref:refs/heads/main");
    bad.push(sa_token(&c));
    c.as_object_mut().unwrap().remove("sub");
    bad.push(sa_token(&c));
    for t in &bad {
        assert_invalid(auth.authenticate(t).await, "not a service account token");
    }
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 0);
    // A string `aud`, ES256 and an exp inside the leeway still go to review.
    let mut c = sa_claims();
    c["aud"] = json!(AUD);
    c["exp"] = json!(now_secs() - 30);
    auth.authenticate(&sa_token_alg("ES256", &c)).await.unwrap();
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 1);
}

/// Answers are reused for `REVIEW_CACHE_TTL`, positive and negative alike,
/// and the cache holds only token hashes.
#[tokio::test(start_paused = true)]
async fn review_answers_are_cached_briefly() {
    let auth = sa_auth(sa_ok());
    let token = sa_token(&sa_claims());
    for _ in 0..5 {
        assert_eq!(
            auth.authenticate(&token).await.unwrap().owner(),
            "sa:ci/builder"
        );
    }
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(REVIEW_CACHE_TTL).await;
    auth.authenticate(&token).await.unwrap();
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 2);

    let deny = sa_auth(FakeReviewer::replying(
        false,
        &[AUD],
        "system:serviceaccount:ci:builder",
    ));
    for _ in 0..5 {
        assert_invalid(
            deny.authenticate(&token).await,
            "tokenreview: not authenticated",
        );
    }
    assert_eq!(deny.reviewer.calls.load(Ordering::SeqCst), 1);
    assert_eq!((deny.reviews.positives(), deny.reviews.negatives()), (0, 1));

    // Upstream failures are not cached.
    let down = sa_auth(FakeReviewer {
        upstream_error: true,
        ..FakeReviewer::default()
    });
    for _ in 0..3 {
        assert_eq!(down.authenticate(&token).await.unwrap_err().status(), 503);
    }
    assert_eq!(down.reviewer.calls.load(Ordering::SeqCst), 3);
}

/// A positive answer never outlives the token's `exp`.
#[tokio::test(start_paused = true)]
async fn cached_identity_expires_with_the_token() {
    let auth = sa_auth(sa_ok());
    let mut c = sa_claims();
    c["exp"] = json!(now_secs() + 2);
    let token = sa_token(&c);
    auth.authenticate(&token).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    auth.authenticate(&token).await.unwrap();
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 2);
}

/// Reviews in flight are bounded; a caller over the bound gets 503 at once.
#[tokio::test]
async fn concurrent_reviews_are_bounded() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let auth = Arc::new(
        sa_auth(FakeReviewer {
            gate: Some(Arc::clone(&gate)),
            ..sa_ok()
        })
        .with_max_concurrent_reviews(2)
        .with_reserved_reviews(0, DEFAULT_RECENT_SUBJECT_TTL),
    );
    let mut held = Vec::new();
    for i in 0..2 {
        let (a, mut c) = (Arc::clone(&auth), sa_claims());
        c["jti"] = json!(i);
        held.push(tokio::spawn(
            async move { a.authenticate(&sa_token(&c)).await },
        ));
    }
    while auth.reviewer.calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    let mut c = sa_claims();
    c["jti"] = json!("third");
    let e = auth.authenticate(&sa_token(&c)).await.unwrap_err();
    assert_eq!((e.status(), e.code()), (503, "upstream_error"));
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 2);
    gate.add_permits(3);
    for h in held {
        h.await.unwrap().unwrap();
    }
    auth.authenticate(&sa_token(&c)).await.unwrap();
}

// ---------- header parsing and error mapping ----------

#[test]
fn bearer_header_parsing() {
    assert_eq!(bearer_token(Some("Bearer abc")).unwrap(), "abc");
    assert_eq!(bearer_token(Some("bearer abc")).unwrap(), "abc");
    for bad in [
        None,
        Some(""),
        Some("Bearer "),
        Some("Basic abc"),
        Some("Bearerabc"),
    ] {
        assert!(bearer_token(bad).is_err(), "{bad:?}");
    }
    let big = format!("Bearer {}", "a".repeat(MAX_TOKEN_BYTES + 1));
    assert!(bearer_token(Some(&big)).is_err());
}

#[test]
fn error_mapping() {
    let cases = [
        (AuthError::Invalid("x"), 401, "unauthenticated", "invalid"),
        (
            AuthError::UnknownIssuer,
            401,
            "unauthenticated",
            "unknown_issuer",
        ),
        (AuthError::KeysUnavailable, 401, "unauthenticated", "error"),
        (
            AuthError::Upstream("x".into()),
            503,
            "upstream_error",
            "error",
        ),
        (AuthError::Internal("x".into()), 500, "internal", "error"),
    ];
    for (e, status, code, metric) in cases {
        assert_eq!(
            (e.status(), e.code(), e.metric_result()),
            (status, code, metric)
        );
    }
}

// ---------- RSA (GitHub Actions signs RS256) ----------

#[tokio::test]
async fn rs256_and_ps256_tokens() {
    let (rs, ps) = (Signer::rsa("rs", "RS256"), Signer::rsa("ps", "PS256"));
    let iss = TestIssuer::start(&[&rs, &ps]).await;
    let auth = iss.auth(None);
    for k in [&rs, &ps] {
        let who = auth
            .authenticate(&k.token(&claims(&iss.url)))
            .await
            .unwrap();
        assert_eq!(
            who.owner(),
            format!("oidc:{}#repo:smrt-devops/app:ref:refs/heads/main", iss.url)
        );
    }

    // Forgery, with the payload swapped under a valid signature.
    let good = rs.token(&claims(&iss.url));
    let mut forged_claims = claims(&iss.url);
    forged_claims["repository_owner"] = json!("attacker");
    let mut parts: Vec<String> = good.split('.').map(str::to_owned).collect();
    parts[1] = B64.encode(forged_claims.to_string());
    assert_invalid(auth.authenticate(&parts.join(".")).await, "bad signature");

    // PKCS#1 v1.5 signature presented as PS256 (and PSS as RS256).
    let rs_as_ps = rs.token_with_header(&json!({"alg": "PS256", "kid": "rs"}), &claims(&iss.url));
    assert_invalid(auth.authenticate(&rs_as_ps).await, "bad signature");
    let ps_as_rs = ps.token_with_header(&json!({"alg": "RS256", "kid": "ps"}), &claims(&iss.url));
    assert_invalid(auth.authenticate(&ps_as_rs).await, "bad signature");
}

#[tokio::test]
async fn duplicate_kid_picks_the_key_consistent_with_alg() {
    let ec = Signer::es256("dup");
    let rsa = Signer::rsa("dup", "RS256");
    let iss = TestIssuer::start(&[&ec, &rsa]).await;
    let auth = iss.auth(None);
    auth.authenticate(&rsa.token(&claims(&iss.url)))
        .await
        .unwrap();
    auth.authenticate(&ec.token(&claims(&iss.url)))
        .await
        .unwrap();
    assert_eq!(iss.fetches(), 1);
}

// ---------- single-flight refresh and fetch observer ----------

fn observed(
    auth: Authenticator<FakeReviewer>,
) -> (Authenticator<FakeReviewer>, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let auth = auth.with_jwks_observer(move |issuer, result| {
        sink.lock()
            .unwrap()
            .push(format!("{issuer} {}", result.label()));
    });
    (auth, seen)
}

#[tokio::test]
async fn concurrent_cold_callers_share_one_fetch() {
    let signer = Signer::es256("k1");
    let iss = TestIssuer::start(&[&signer]).await;
    let (auth, seen) = observed(iss.auth(None));
    let token = signer.token(&claims(&iss.url));
    let results = tokio::join!(
        auth.authenticate(&token),
        auth.authenticate(&token),
        auth.authenticate(&token),
        auth.authenticate(&token)
    );
    for r in <[_; 4]>::from(results) {
        r.unwrap();
    }
    assert_eq!(iss.fetches(), 1);
    assert_eq!(*seen.lock().unwrap(), vec![format!("{} ok", iss.url)]);
}

#[tokio::test]
async fn hung_issuer_costs_one_fetch_budget() {
    let signer = Signer::es256("k1");
    let iss = TestIssuer::start(&[&signer]).await;
    iss.state.lock().unwrap().hang = true;
    let (auth, seen) = observed(iss.auth(None));
    let token = signer.token(&claims(&iss.url));
    let started = std::time::Instant::now();
    let (first, second) = tokio::join!(auth.authenticate(&token), auth.authenticate(&token));
    // Discovery + JWKS share one 5 s budget; the second caller waits on the
    // same fetch instead of starting its own.
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
    assert!(matches!(first, Err(AuthError::KeysUnavailable)));
    assert!(matches!(second, Err(AuthError::KeysUnavailable)));
    assert_eq!(*seen.lock().unwrap(), vec![format!("{} error", iss.url)]);
}

#[tokio::test]
async fn stale_keys_keep_serving_while_a_refresh_hangs() {
    let signer = Signer::es256("k1");
    let iss = TestIssuer::start(&[&signer]).await;
    let (auth, seen) = observed(iss.auth(None));
    let token = signer.token(&claims(&iss.url));
    auth.authenticate(&token).await.unwrap();
    iss.state.lock().unwrap().hang = true;
    auth.issuers[&iss.url].expire();

    let started = std::time::Instant::now();
    // The first caller starts the refresh but does not wait for it; the
    // second finds it in flight.
    auth.authenticate(&token).await.unwrap();
    auth.authenticate(&token).await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited < std::time::Duration::from_secs(1),
        "waited {waited:?}"
    );
    assert_eq!(*seen.lock().unwrap(), vec![format!("{} ok", iss.url)]);
    // The detached refresh still reports its outcome.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while seen.lock().unwrap().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![format!("{} ok", iss.url), format!("{} error", iss.url)]
    );
}

#[tokio::test]
async fn cancelled_refresher_does_not_strand_waiters() {
    let signer = Signer::es256("k1");
    let iss = TestIssuer::start(&[&signer]).await;
    iss.state.lock().unwrap().slow = true;
    let (auth, seen) = observed(iss.auth(None));
    let token = signer.token(&claims(&iss.url));
    // The first caller starts the fetch and is cancelled during the JWKS
    // request (as by a client disconnect); two cold callers queue behind it.
    // The fetch still completes once, and its result serves them.
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(750),
        auth.authenticate(&token),
    );
    let waiters = async {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::join!(auth.authenticate(&token), auth.authenticate(&token))
    };
    let (cancelled, (a, b)) = tokio::join!(cancelled, waiters);
    assert!(cancelled.is_err(), "first caller was not cancelled");
    a.unwrap();
    b.unwrap();
    assert_eq!(iss.fetches(), 1);
    assert_eq!(*seen.lock().unwrap(), vec![format!("{} ok", iss.url)]);
}

#[tokio::test(start_paused = true)]
async fn cancelled_callers_cannot_bypass_backoff() {
    let signer = Signer::es256("k1");
    let iss = TestIssuer::start(&[&signer]).await;
    iss.state.lock().unwrap().hang = true;
    let (auth, seen) = observed(iss.auth(None));
    let token = signer.token(&claims(&iss.url));
    let started = tokio::time::Instant::now();
    // One caller per second, each cancelled after at most 1 s.
    let call_until = |secs| {
        let deadline = started + std::time::Duration::from_secs(secs);
        let (auth, token) = (&auth, &token);
        async move {
            while tokio::time::Instant::now() < deadline {
                let tick = tokio::time::sleep(std::time::Duration::from_secs(1));
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    auth.authenticate(token),
                )
                .await;
                tick.await;
            }
        }
    };
    call_until(29).await;
    // The first fetch timed out after 5 s; nothing else ran inside 30 s of it.
    assert_eq!(*seen.lock().unwrap(), vec![format!("{} error", iss.url)]);
    call_until(40).await;
    assert_eq!(seen.lock().unwrap().len(), 2, "{:?}", seen.lock().unwrap());
}

#[tokio::test]
async fn panicking_observer_still_records_keys() {
    let signer = Signer::es256("k1");
    let iss = TestIssuer::start(&[&signer]).await;
    let auth = iss
        .auth(None)
        .with_jwks_observer(|_, _| panic!("observer panicked"));
    let token = signer.token(&claims(&iss.url));
    auth.authenticate(&token).await.unwrap();
    auth.authenticate(&token).await.unwrap();
    assert_eq!(iss.fetches(), 1);
}

#[tokio::test]
async fn private_ca_issuer_needs_its_ca_bundle() {
    let k = Signer::es256("k1");
    let iss = TestIssuer::start(&[&k]).await;
    let token = k.token(&claims(&iss.url));

    let r = iss.auth_with_private_ca(false).authenticate(&token).await;
    assert!(matches!(r, Err(AuthError::KeysUnavailable)), "{r:?}");
    assert_eq!(iss.fetches(), 0, "the handshake fails before JWKS");

    let id = iss
        .auth_with_private_ca(true)
        .authenticate(&token)
        .await
        .unwrap();
    assert!(matches!(id, Identity::Oidc { .. }));
    assert_eq!(iss.fetches(), 1);
}

#[test]
fn api_server_audience_is_detected() {
    let mut own = sa_claims();
    own["aud"] = json!(["https://kubernetes.default.svc.cluster.local", "k3s"]);
    let own = sa_token(&own);
    let sa = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert!(api_audience_overlap(&own, &sa(&[AUD])).is_empty());
    assert_eq!(
        api_audience_overlap(&format!("{own}\n"), &sa(&[AUD, "k3s"])),
        vec!["k3s".to_owned()]
    );
    let mut one = sa_claims();
    one["aud"] = json!("https://kubernetes.default.svc");
    assert_eq!(
        api_audience_overlap(&sa_token(&one), &sa(&["https://kubernetes.default.svc"])).len(),
        1
    );
    assert!(api_audience_overlap("not a jwt", &sa(&[AUD])).is_empty());
}

/// Accepts tokens with a `legit` claim at once; refuses every other token,
/// after waiting on `gate` when one is set (a slow API server).
#[derive(Default)]
struct Sorting {
    gate: Option<Arc<tokio::sync::Semaphore>>,
    legit_calls: AtomicUsize,
    forged_calls: AtomicUsize,
}

impl TokenReviewer for Sorting {
    async fn review(
        &self,
        token: &str,
        audiences: &[String],
    ) -> Result<TokenReviewStatus, ReviewError> {
        let claims: Value = jsonwebtoken::dangerous::insecure_decode_claims(token).unwrap();
        let legit = claims.get("legit").is_some();
        if legit {
            self.legit_calls.fetch_add(1, Ordering::SeqCst);
        } else {
            self.forged_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(g) = &self.gate {
                g.acquire().await.unwrap().forget();
            }
        }
        Ok(TokenReviewStatus {
            authenticated: Some(legit),
            audiences: Some(audiences.to_vec()),
            user: Some(UserInfo {
                username: Some("system:serviceaccount:ci:builder".into()),
                ..UserInfo::default()
            }),
            ..TokenReviewStatus::default()
        })
    }
}

fn forged(n: usize) -> String {
    let mut c = sa_claims();
    c["jti"] = json!(n);
    sa_token(&c)
}

fn legit(n: usize) -> String {
    let mut c = sa_claims();
    c["legit"] = json!(true);
    c["jti"] = json!(n);
    sa_token(&c)
}

/// A flood of unique forged SA-shaped tokens from one address holds at most
/// the per-address review cap, so a legitimate caller from another address
/// still gets a review; and refusals never stop identities being cached.
#[tokio::test]
async fn forged_flood_from_one_address_cannot_starve_others() {
    let attacker = Some("203.0.113.9".parse().unwrap());
    let ci = Some("198.51.100.4".parse().unwrap());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let cfg = AuthConfig {
        oidc: vec![],
        service_account_audiences: Some(vec![AUD.into()]),
    };
    let http = https_client(rustls::RootCertStore::empty()).unwrap();
    let reviewer = Sorting {
        gate: Some(Arc::clone(&gate)),
        ..Sorting::default()
    };
    let auth = Arc::new(Authenticator::new(cfg, http, reviewer).unwrap());
    let mut held = Vec::new();
    for i in 0..64 {
        let a = Arc::clone(&auth);
        held.push(tokio::spawn(async move {
            a.authenticate_from(&forged(i), attacker).await
        }));
    }
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        auth.reviewer.forged_calls.load(Ordering::SeqCst),
        DEFAULT_MAX_REVIEWS_PER_IP
    );
    let token = legit(0);
    for _ in 0..3 {
        let id = auth.authenticate_from(&token, ci).await.unwrap();
        assert_eq!(id.owner(), "sa:ci/builder");
    }
    assert_eq!(auth.reviewer.legit_calls.load(Ordering::SeqCst), 1);

    // Let the held reviews finish, then fill the refusal cache to the brim.
    gate.add_permits(usize::MAX >> 4);
    for h in held {
        let _ = h.await.unwrap();
    }
    for i in 64..3000 {
        let _ = auth.authenticate_from(&forged(i), attacker).await;
    }
    assert!(auth.reviews.negatives() > 0);
    // A new identity is still cached after the flood.
    let token = legit(1);
    auth.authenticate_from(&token, ci).await.unwrap();
    auth.authenticate_from(&token, ci).await.unwrap();
    assert_eq!(auth.reviewer.legit_calls.load(Ordering::SeqCst), 2);
}

// ---------- reserved review slots ----------

fn sub_token(sub: &str, jti: usize) -> String {
    let mut c = sa_claims();
    c["sub"] = json!(format!("system:serviceaccount:{sub}"));
    c["jti"] = json!(jti);
    sa_token(&c)
}

/// A gated authenticator where `ci/builder` has passed a review, then forged
/// `evil/x` tokens from 16 addresses (2 each) hold all 24 shared slots.
async fn flooded() -> (
    Arc<Authenticator<FakeReviewer>>,
    Arc<tokio::sync::Semaphore>,
) {
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let auth = Arc::new(sa_auth(FakeReviewer {
        gate: Some(Arc::clone(&gate)),
        ..sa_ok()
    }));
    auth.authenticate(&sub_token("ci:builder", 0))
        .await
        .unwrap();
    for i in 0..32_u16 {
        let a = Arc::clone(&auth);
        let ip: std::net::IpAddr = format!("10.0.0.{}", i % 16).parse().unwrap();
        tokio::spawn(async move {
            a.authenticate_from(&sub_token("evil:x", 1 + usize::from(i)), Some(ip))
                .await
        });
    }
    while auth.reviewer.calls.load(Ordering::SeqCst) < 25 {
        tokio::task::yield_now().await;
    }
    (auth, gate)
}

/// A flood of forged unknown-subject tokens from 16 addresses fills the shared
/// slots but not the reserve, so a known service account is still reviewed.
#[tokio::test(start_paused = true)]
async fn forged_unknown_subjects_cannot_starve_a_known_one() {
    let (auth, gate) = flooded().await;
    let ip = Some("10.9.9.9".parse().unwrap());
    let e = tokio::time::timeout(
        Duration::from_secs(1),
        auth.authenticate_from(&sub_token("evil:y", 99), ip),
    )
    .await
    .expect("an unknown subject must be refused at once, not reviewed")
    .unwrap_err();
    assert_eq!((e.status(), e.code()), (503, "upstream_error"));
    let a = Arc::clone(&auth);
    let known =
        tokio::spawn(async move { a.authenticate_from(&sub_token("ci:builder", 100), ip).await });
    while auth.reviewer.calls.load(Ordering::SeqCst) < 26 {
        tokio::task::yield_now().await;
    }
    gate.add_permits(64);
    assert_eq!(known.await.unwrap().unwrap().owner(), "sa:ci/builder");
}

/// Forging a known `sub` takes at most that subject's share of the reserve.
#[tokio::test(start_paused = true)]
async fn forged_known_subject_is_capped_per_subject() {
    let (auth, gate) = flooded().await;
    let mut tasks = Vec::new();
    for i in 0..8_u8 {
        let a = Arc::clone(&auth);
        let ip: std::net::IpAddr = format!("10.1.0.{i}").parse().unwrap();
        tasks.push(tokio::spawn(async move {
            a.authenticate_from(&sub_token("ci:builder", 200 + usize::from(i)), Some(ip))
                .await
        }));
    }
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    // 1 setup review + 24 flood reviews (the shared slots) + the 2 reserved slots of one subject.
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 27);
    assert_eq!(auth.review_slots.reserve_free(), 6);
    gate.add_permits(64);
    let ok = futures_ok(tasks).await;
    assert_eq!(ok, 2);
    assert_eq!(auth.review_slots.reserve_free(), 8);
}

async fn futures_ok(tasks: Vec<tokio::task::JoinHandle<Result<Identity, AuthError>>>) -> usize {
    let mut ok = 0;
    for t in tasks {
        ok += usize::from(t.await.unwrap().is_ok());
    }
    ok
}

/// A subject unseen for `recentSubjectTtl` loses its reserve.
#[tokio::test(start_paused = true)]
async fn recently_seen_subjects_expire() {
    let (auth, gate) = flooded().await;
    tokio::time::advance(DEFAULT_RECENT_SUBJECT_TTL).await;
    let e = tokio::time::timeout(
        Duration::from_secs(1),
        auth.authenticate_from(&sub_token("ci:builder", 300), None),
    )
    .await
    .expect("an expired subject must be refused at once, not reviewed")
    .unwrap_err();
    assert_eq!(e.status(), 503);
    gate.add_permits(64);
}

/// IPv6 peers are grouped at /56 for the review cap; IPv4 and mapped IPv4
/// addresses are not.
#[tokio::test]
async fn review_cap_groups_ipv6_at_56() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let auth = Arc::new(
        sa_auth(FakeReviewer {
            gate: Some(Arc::clone(&gate)),
            ..sa_ok()
        })
        .with_max_reviews_per_ip(1),
    );
    let first = {
        let a = Arc::clone(&auth);
        let ip: std::net::IpAddr = "2001:db8:0:1::1".parse().unwrap();
        tokio::spawn(async move {
            a.authenticate_from(&sub_token("ci:builder", 1), Some(ip))
                .await
        })
    };
    while auth.reviewer.calls.load(Ordering::SeqCst) < 1 {
        tokio::task::yield_now().await;
    }
    // Another /64 of the same /56 shares the first peer's cap.
    let same56: std::net::IpAddr = "2001:db8:0:2::1".parse().unwrap();
    let e = auth
        .authenticate_from(&sub_token("ci:builder", 2), Some(same56))
        .await
        .unwrap_err();
    assert_eq!(e.status(), 503);
    assert_eq!(auth.reviewer.calls.load(Ordering::SeqCst), 1);
    // The next /56 is a separate peer.
    let other56: std::net::IpAddr = "2001:db8:0:100::1".parse().unwrap();
    let b = Arc::clone(&auth);
    let second = tokio::spawn(async move {
        b.authenticate_from(&sub_token("ci:builder", 3), Some(other56))
            .await
    });
    while auth.reviewer.calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    // IPv4-mapped peers are IPv4 peers, each with its own cap.
    let mut mapped = Vec::new();
    for (i, ip) in ["::ffff:10.0.0.1", "::ffff:10.0.0.2"]
        .into_iter()
        .enumerate()
    {
        let a = Arc::clone(&auth);
        let ip: std::net::IpAddr = ip.parse().unwrap();
        mapped.push(tokio::spawn(async move {
            a.authenticate_from(&sub_token("ci:builder", 10 + i), Some(ip))
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while auth.reviewer.calls.load(Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("each mapped IPv4 peer has its own cap");
    gate.add_permits(8);
    for m in mapped {
        m.await.unwrap().unwrap();
    }
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
}

/// A returned reserved slot gives its subject's share back.
#[tokio::test(start_paused = true)]
async fn reserved_slots_return_to_their_subject() {
    let slots = token_review::ReviewSlots::new(32, 8, DEFAULT_RECENT_SUBJECT_TTL);
    let now = tokio::time::Instant::now();
    slots.note_authenticated("ci/builder", now);
    let shared: Vec<_> = (0..24).map(|_| slots.acquire("other", now)).collect();
    assert!(shared.iter().all(Option::is_some));
    for _ in 0..2 {
        let a = slots.acquire("ci/builder", now).expect("first share");
        let b = slots.acquire("ci/builder", now).expect("second share");
        assert!(slots.acquire("ci/builder", now).is_none());
        assert_eq!(slots.reserve_free(), 6);
        drop((a, b));
        assert_eq!(slots.reserve_free(), 8);
    }
    assert!(slots.acquire("evil/x", now).is_none());
}

/// Only a review that authenticated marks a subject as seen, so a refused
/// token naming `ci/builder` wins it no reserve.
#[tokio::test(start_paused = true)]
async fn refused_review_does_not_mark_a_subject_seen() {
    let refusing = sa_auth(FakeReviewer::replying(
        false,
        &[AUD],
        "system:serviceaccount:ci:builder",
    ));
    refusing
        .authenticate(&sub_token("ci:builder", 1))
        .await
        .unwrap_err();
    let now = tokio::time::Instant::now();
    let _shared: Vec<_> = (0..24)
        .map(|_| refusing.review_slots.acquire("other", now))
        .collect();
    assert!(refusing.review_slots.acquire("ci/builder", now).is_none());

    let accepting = sa_auth(sa_ok());
    accepting
        .authenticate(&sub_token("ci:builder", 1))
        .await
        .unwrap();
    let _shared: Vec<_> = (0..24)
        .map(|_| accepting.review_slots.acquire("other", now))
        .collect();
    assert!(accepting.review_slots.acquire("ci/builder", now).is_some());
}

// ---------- owner ----------

const GH: &str = "https://token.actions.githubusercontent.com";
const GH_SUB: &str = "repo:smrt-devops/app:pull_request";

fn oidc(sub: &str, claims: Value) -> Identity {
    let Value::Object(claims) = claims else {
        panic!("object")
    };
    Identity::Oidc {
        issuer: GH.into(),
        subject: sub.into(),
        claims,
    }
}

fn gh_run(run_id: &str, attempt: &str) -> Value {
    json!({"sub": GH_SUB, "run_id": run_id, "run_attempt": attempt,
           "job_workflow_ref": "smrt-devops/app/.github/workflows/ci.yml@refs/pull/7/merge"})
}

#[test]
fn github_owner_is_bound_to_the_run() {
    assert_eq!(
        oidc(GH_SUB, gh_run("42", "1")).owner(),
        format!(
            "oidc:{GH}#{GH_SUB}#run_id=42#run_attempt=1\
             #job_workflow_ref=smrt-devops/app/.github/workflows/ci.yml@refs/pull/7/merge"
        )
    );
    // Same repo and same `sub`, but another run or another attempt: another owner.
    let a = oidc(GH_SUB, gh_run("42", "1")).owner();
    assert_ne!(a, oidc(GH_SUB, gh_run("43", "1")).owner());
    assert_ne!(a, oidc(GH_SUB, gh_run("42", "2")).owner());
    let mut other_wf = gh_run("42", "1");
    other_wf["job_workflow_ref"] = json!("smrt-devops/app/.github/workflows/x.yml@refs/heads/main");
    assert_ne!(a, oidc(GH_SUB, other_wf).owner());
    // A fresh token of the same run (the post step) keeps the owner.
    assert_eq!(a, oidc(GH_SUB, gh_run("42", "1")).owner());
}

#[test]
fn owner_without_run_claims_is_iss_and_sub() {
    let plain = format!("oidc:{GH}#{GH_SUB}");
    assert_eq!(oidc(GH_SUB, json!({"sub": GH_SUB})).owner(), plain);
    // Empty or non-scalar values are not run claims.
    assert_eq!(
        oidc(
            GH_SUB,
            json!({"run_id": "", "run_attempt": null, "job_workflow_ref": {"a": 1}})
        )
        .owner(),
        plain
    );
    // `%` and `#` in `sub` are escaped without run claims too.
    assert_eq!(
        oidc("a#b%c", json!({})).owner(),
        format!("oidc:{GH}#a%23b%25c")
    );
}

#[test]
fn partial_and_numeric_run_claims() {
    assert_eq!(
        oidc("s", json!({"run_id": 7, "run_attempt": 2})).owner(),
        format!("oidc:{GH}#s#run_id=7#run_attempt=2")
    );
    assert_eq!(
        oidc("s", json!({"job_workflow_ref": "o/r/w.yml@x"})).owner(),
        format!("oidc:{GH}#s#job_workflow_ref=o/r/w.yml@x")
    );
}

/// A `#` in `sub` or a claim cannot forge another run's owner string.
#[test]
fn run_bound_owner_is_unambiguous() {
    let forged = oidc("s#run_id=1", json!({"run_attempt": "2"})).owner();
    let real = oidc("s", json!({"run_id": "1", "run_attempt": "2"})).owner();
    assert_eq!(real, format!("oidc:{GH}#s#run_id=1#run_attempt=2"));
    assert_eq!(forged, format!("oidc:{GH}#s%23run_id=1#run_attempt=2"));
    assert_ne!(forged, real);
    let a = oidc("s", json!({"run_id": "1#run_attempt=2"})).owner();
    assert_ne!(a, real);
    let pct = oidc("s%23x", json!({"run_id": "1"})).owner();
    assert_ne!(pct, oidc("s#x", json!({"run_id": "1"})).owner());
}

/// For an issuer that mints tokens with and without run claims, a `sub`
/// carrying a run claim cannot pass for another caller's run-bound owner.
#[test]
fn plain_owner_cannot_forge_a_run_bound_one() {
    let forged = oidc("x#run_id=1", json!({})).owner();
    let real = oidc("x", json!({"run_id": "1"})).owner();
    assert_eq!(real, format!("oidc:{GH}#x#run_id=1"));
    assert_eq!(forged, format!("oidc:{GH}#x%23run_id=1"));
    assert_ne!(forged, real);
}

#[test]
fn service_account_and_claim_owners_are_unchanged() {
    let sa = Identity::ServiceAccount {
        namespace: "ci".into(),
        name: "builder".into(),
        uid: "u".into(),
    };
    assert_eq!(sa.owner(), "sa:ci/builder");
    let claim = Identity::Claim {
        namespace: "ci".into(),
        name: "c".into(),
        uid: "u".into(),
    };
    assert_eq!(claim.owner(), "claim:ci/c#u");
}

/// End to end through signature verification: the run claims of a real token
/// reach the owner.
#[tokio::test]
async fn verified_token_owner_carries_run_claims() {
    let es = Signer::es256("k1");
    let iss = TestIssuer::start(&[&es]).await;
    let auth = iss.auth(None);
    let mut c = claims(&iss.url);
    c["run_id"] = json!("9001");
    c["run_attempt"] = json!("3");
    c["job_workflow_ref"] = json!("smrt-devops/app/.github/workflows/ci.yml@refs/heads/main");
    let id = auth.authenticate(&es.token(&c)).await.unwrap();
    assert_eq!(
        id.owner(),
        format!(
            "oidc:{}#repo:smrt-devops/app:ref:refs/heads/main#run_id=9001#run_attempt=3\
             #job_workflow_ref=smrt-devops/app/.github/workflows/ci.yml@refs/heads/main",
            iss.url
        )
    );
}

/// The issuer is escaped like `sub`, so an issuer containing `#` cannot
/// collide with another issuer's owner.
#[test]
fn issuer_hash_cannot_forge_another_issuers_owner() {
    let owner = |iss: &str, sub: &str, claims: Value| {
        let Value::Object(claims) = claims else {
            panic!("object")
        };
        Identity::Oidc {
            issuer: iss.into(),
            subject: sub.into(),
            claims,
        }
        .owner()
    };
    // Unescaped, both were `oidc:https://a#x#run_id=1`.
    let forged = owner("https://a#x", "run_id=1", json!({}));
    let real = owner("https://a", "x", json!({"run_id": "1"}));
    assert_eq!(real, "oidc:https://a#x#run_id=1");
    assert_eq!(forged, "oidc:https://a%23x#run_id=1");
    assert_eq!(owner(GH, "s", json!({})), format!("oidc:{GH}#s"));
}
