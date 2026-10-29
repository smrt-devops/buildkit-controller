//! Public HTTPS API scenarios on a real API server. Run by `cargo xtask
//! api-verify` (which provides a kind cluster through `BK_TEST_KUBECONFIG`);
//! ignored otherwise.
//!
//! The controller runs on the host, with the certs, alloc and http components over
//! the kind API server, with OIDC discovery and JWKS served by an in-harness
//! issuer over HTTPS on localhost (its keys generated here, trusted through
//! the issuer's `caBundleFile` config option). Service-account tokens are real
//! (`kubectl create token`) and go through `TokenReview`. The replica leads, so
//! it also binds the claims the API creates. The harness stands in for the
//! reconciler: it creates warm pause pods with the labels, annotations and
//! ownerReference the reconciler would render.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

#[path = "support/kind.rs"]
mod harness;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use buildkit_controller::alloc::AllocService;
use buildkit_controller::certs::Certs;
use buildkit_controller::config::{Cli, Config, Settings};
use buildkit_controller::election::LeaderState;
use buildkit_controller::http::{HttpComponent, Limits};
use buildkit_controller::pki::CaBundle;
use buildkit_controller::render::{PodInputs, worker_pod};
use buildkit_controller::runtime::AppContext;
use buildkit_controller::runtime::hooks::Component;
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use buildkit_controller_api::v1alpha1::BuildKitPool;
use clap::Parser;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret};
use kube::api::{Api, Patch, PatchParams, PostParams};
use reqwest::StatusCode;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use rustls::pki_types::PrivateKeyDer;
use rustls::sign::CertifiedKey;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const NS: &str = "bk-api";
const POOL: &str = "p";
const HIDDEN: &str = "hidden";
const SERVER_NAME: &str = "bk.example.com";
const AUD: &str = "buildkit-controller";

async fn wait(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let end = tokio::time::Instant::now() + limit;
    while !f() {
        assert!(tokio::time::Instant::now() < end, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn kubectl(args: &[&str]) -> String {
    let out = harness::kubectl().args(args).output().unwrap();
    assert!(
        out.status.success(),
        "kubectl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The mock OIDC issuer with an ES256 key, discovery and JWKS over HTTPS.
struct Issuer {
    url: String,
    key: EcdsaKeyPair,
    ca_pem: String,
}

impl Issuer {
    async fn start(stop: CancellationToken) -> Self {
        let rng = SystemRandom::new();
        let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der.as_ref(), &rng).unwrap();
        let p = key.public_key().as_ref();
        let jwks = json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig",
            "alg": "ES256", "x": B64.encode(&p[1..33]), "y": B64.encode(&p[33..65])}]});

        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://localhost:{}", l.local_addr().unwrap().port());
        let discovery = json!({"issuer": url, "jwks_uri": format!("{url}/jwks")});
        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || async move { Json(discovery) }),
            )
            .route("/jwks", get(move || async move { Json(jwks) }));

        let c = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let signer = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
            c.signing_key.serialize_der().into(),
        ))
        .unwrap();
        let cert = Arc::new(CertifiedKey::new(vec![c.cert.der().clone()], signer));
        let (tx, rx) = watch::channel(Some(cert));
        std::mem::forget(tx);
        tokio::spawn(buildkit_controller::http::serve(
            l,
            app,
            rx,
            Limits::default(),
            stop,
        ));
        Self {
            url,
            key,
            ca_pem: c.cert.pem(),
        }
    }

    fn token(&self, aud: &str, team: &str) -> String {
        sign(&self.key, &self.url, aud, team)
    }
}

fn sign(key: &EcdsaKeyPair, iss: &str, aud: &str, team: &str) -> String {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let header = json!({"alg": "ES256", "typ": "JWT", "kid": "k1"});
    let claims = json!({"iss": iss, "sub": format!("user-{team}"), "aud": aud,
        "iat": now, "nbf": now - 5, "exp": now + 600, "team": team});
    let msg = format!(
        "{}.{}",
        B64.encode(header.to_string()),
        B64.encode(claims.to_string())
    );
    let sig = key.sign(&SystemRandom::new(), msg.as_bytes()).unwrap();
    format!("{msg}.{}", B64.encode(sig.as_ref()))
}

struct H {
    pods: Api<Pod>,
    pool: BuildKitPool,
    fp: String,
    image: String,
    ctx: AppContext,
}

impl H {
    /// One warm pause pod `p-w<tag>`, waited for in the controller's store.
    async fn warm(&self, tag: &str) -> String {
        let mut pod = worker_pod(
            &self.pool,
            &PodInputs {
                slot: 0,
                internal_ca_fingerprint: self.fp.clone(),
                worker_cert_not_after: "2099-01-01T00:00:00Z".into(),
                nonce: "00000000000000aa".into(),
            },
        )
        .unwrap();
        let name = format!("{POOL}-w{tag}");
        pod.metadata.name = Some(name.clone());
        pod.spec = Some(
            serde_json::from_value(json!({
                "terminationGracePeriodSeconds": 0,
                "containers": [{"name": "pause", "image": self.image, "imagePullPolicy": "Never"}]
            }))
            .unwrap(),
        );
        self.pods
            .create(&PostParams::default(), &pod)
            .await
            .unwrap();
        let store = self.ctx.pods();
        let r = kube::runtime::reflector::ObjectRef::new(&name).within(NS);
        wait("warm pod ready in store", Duration::from_mins(2), || {
            store.get(&r).is_some_and(|p| {
                p.status.as_ref().is_some_and(|s| {
                    s.pod_ip.is_some()
                        && s.conditions
                            .iter()
                            .flatten()
                            .any(|c| c.type_ == "Ready" && c.status == "True")
                })
            })
        })
        .await;
        name
    }

    async fn gone(&self, name: &str) {
        let store = self.ctx.pods();
        let r = kube::runtime::reflector::ObjectRef::new(name).within(NS);
        wait("released pod gone", Duration::from_mins(2), || {
            store.get(&r).is_none()
        })
        .await;
    }

    /// Wait until the store shows exactly one pod claimed by allocation `id`; its name.
    async fn wait_claimed(&self, id: &str) -> String {
        wait("claim in store", Duration::from_secs(30), || {
            self.claimed(id).len() == 1
        })
        .await;
        self.claimed(id).remove(0)
    }

    /// Names of the pods claimed by allocation `id` (annotation value).
    fn claimed(&self, id: &str) -> Vec<String> {
        self.ctx
            .pods()
            .state()
            .iter()
            .filter(|p| {
                p.metadata
                    .annotations
                    .iter()
                    .flatten()
                    .any(|(_, v)| v == id)
            })
            .map(|p| p.metadata.name.clone().unwrap_or_default())
            .collect()
    }
}

fn pool_json(name: &str, rules: &Value) -> BuildKitPool {
    serde_json::from_value(json!({
        "apiVersion": "buildkit.smrt-devops.net/v1alpha1", "kind": "BuildKitPool",
        "metadata": {"name": name, "namespace": NS},
        "spec": {
            "template": {"spec": {"containers": [{"name": "buildkitd", "image": "moby/buildkit:v0.26.3",
                "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}}]}},
            "scaling": {"min": 0, "max": 1},
            "access": {"rules": rules},
            "allocation": {"maxQueueTimeout": "2m"}
        }
    }))
    .unwrap()
}

async fn setup(issuer: &Issuer, stop: &CancellationToken) -> (H, SocketAddr, Arc<Metrics>) {
    let client = harness::client().await;
    let image = std::env::var("API_VERIFY_PAUSE_IMAGE").unwrap();
    let work = std::path::PathBuf::from(std::env::var("API_VERIFY_WORK").unwrap());
    Api::<Namespace>::all(client.clone())
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({"metadata": {"name": NS}})).unwrap(),
        )
        .await
        .unwrap();

    let api = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let ca_file = work.join("issuer-ca.pem");
    std::fs::write(&ca_file, &issuer.ca_pem).unwrap();
    let yaml = format!(
        "watchNamespaces: [{NS}]\nlisten:\n  api: \"{api}\"\nedge:\n  endpoint: tcp://{SERVER_NAME}:8444\n  serverName: {SERVER_NAME}\n  allocationDomain: alloc.{SERVER_NAME}\nauth:\n  oidc:\n    - url: {}\n      audiences: [{AUD}]\n      caBundleFile: {}\n",
        issuer.url,
        ca_file.display()
    );
    let path = work.join("config.yaml");
    std::fs::write(&path, &yaml).unwrap();
    let mut cli = Cli::parse_from(["buildkit-controller", "--config", path.to_str().unwrap()]);
    cli.pod_name = "r1".into();
    cli.pod_namespace = NS.into();
    let config = Config::from_yaml(&yaml).unwrap();

    let ca = CaBundle::generate(OffsetDateTime::now_utc()).unwrap();
    let fp = ca.internal_ca().fingerprint_hex();
    let mut secret = Secret::default();
    secret.metadata.name = Some(config.pki.ca_secret_name.clone());
    secret.data = Some(
        ca.to_secret_data()
            .into_iter()
            .map(|(k, v)| (k, ByteString(v)))
            .collect(),
    );
    Api::<Secret>::namespaced(client.clone(), NS)
        .create(&PostParams::default(), &secret)
        .await
        .unwrap();

    let pools: Api<BuildKitPool> = Api::namespaced(client.clone(), NS);
    let ready = json!({"status": {"conditions": [{"type": "Ready", "status": "True",
        "reason": "Ready", "message": "", "lastTransitionTime": "2026-01-01T00:00:00Z"}]}});
    let mut pool = None;
    for (name, rules) in [
        (
            POOL,
            json!([{"oidc": {"issuer": issuer.url, "claims": {"team": "build"}}},
                   {"serviceAccount": {}}]),
        ),
        (
            HIDDEN,
            json!([{"oidc": {"issuer": issuer.url, "claims": {"team": "other"}}}]),
        ),
    ] {
        let p = pools
            .create(&PostParams::default(), &pool_json(name, &rules))
            .await
            .unwrap();
        pools
            .patch_status(name, &PatchParams::default(), &Patch::Merge(&ready))
            .await
            .unwrap();
        if name == POOL {
            pool = Some(p);
        }
    }

    let metrics = Arc::new(Metrics::new());
    // The leader: its allocator binds the claims.
    let (leader_tx, leader_rx) = watch::channel(LeaderState::Leader { term: 1 });
    std::mem::forget(leader_tx);
    let ctx = AppContext::build(
        client.clone(),
        Settings { cli, config },
        Arc::clone(&metrics),
        Arc::new(Readiness::with_spec_checks()),
        leader_rx,
        stop,
    );
    let certs = Certs::new();
    let svc = AllocService::new(certs.subscribe());
    let http = HttpComponent::new(svc.clone(), certs.subscribe());
    let components: [Arc<dyn Component>; 4] = [
        Arc::new(certs),
        Arc::new(svc.clone()),
        Arc::new(svc.allocator()),
        Arc::new(http),
    ];
    for c in components {
        let (ctx, stop) = (ctx.clone(), stop.clone());
        tokio::spawn(async move { c.run(ctx, stop).await });
    }
    wait("alloc running", Duration::from_mins(1), || svc.is_running()).await;
    wait("pool status in store", Duration::from_secs(30), || {
        ctx.pools()
            .state()
            .iter()
            .filter(|p| p.status.is_some())
            .count()
            == 2
    })
    .await;
    let h = H {
        pods: Api::namespaced(client, NS),
        pool: pool.unwrap(),
        fp,
        image,
        ctx,
    };
    (h, api, metrics)
}

fn metric(m: &Metrics, needle: &str) -> u64 {
    m.render()
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains(needle))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

/// An API client resolving `SERVER_NAME` to the listener and trusting `ca`
/// alone, or (bootstrap only) trusting nothing.
fn client(api: SocketAddr, ca: Option<&str>) -> reqwest::Client {
    let b = reqwest::Client::builder()
        .resolve(SERVER_NAME, api)
        .tls_built_in_root_certs(false)
        .timeout(Duration::from_mins(2));
    match ca {
        Some(pem) => {
            b.add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).unwrap())
        }
        None => b.danger_accept_invalid_certs(true),
    }
    .build()
    .unwrap()
}

fn base(api: SocketAddr) -> String {
    format!("https://{SERVER_NAME}:{}", api.port())
}

/// POST an allocation as a client does: with the public half of a key pair it
/// generated, so the response carries no private key.
async fn post(
    c: &reqwest::Client,
    api: SocketAddr,
    token: Option<&str>,
    pool: &str,
) -> (StatusCode, Value) {
    let key = rcgen::KeyPair::generate().unwrap().public_key_pem();
    let body = json!({"namespace": NS, "pool": pool, "queueTimeoutSeconds": 90, "publicKey": key});
    post_body(c, api, token, &body).await
}

async fn post_body(
    c: &reqwest::Client,
    api: SocketAddr,
    token: Option<&str>,
    body: &Value,
) -> (StatusCode, Value) {
    let mut r = c.post(format!("{}/v1/allocations", base(api))).json(body);
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    let resp = r.send().await.unwrap();
    let st = resp.status();
    (st, resp.json().await.unwrap_or(Value::Null))
}

fn check_grant(v: &Value, ca: &str) {
    assert!(
        v.get("clientKey").is_none(),
        "the server sent a private key: {v}"
    );
    assert!(
        v["clientCert"]
            .as_str()
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----"),
        "{v}"
    );
    assert_eq!(v["serverName"], SERVER_NAME);
    let id = v["id"].as_str().unwrap();
    assert_eq!(
        v["endpoint"],
        format!("tcp://{id}.alloc.{SERVER_NAME}:8444")
    );
    assert_eq!(v["caCert"].as_str().unwrap(), ca, "caCert stable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs a kind cluster: cargo xtask api-verify"]
async fn api_verify() {
    let stop = CancellationToken::new();
    let issuer = Issuer::start(stop.clone()).await;
    let (h, api, metrics) = setup(&issuer, &stop).await;
    let w1 = h.warm("1").await;
    let w2 = h.warm("2").await;
    let alice = issuer.token(AUD, "build");

    // a. OIDC 201; its caCert alone verifies the API's serving cert.
    let boot = client(api, None);
    wait_listener(&boot, api).await;
    let (st, a) = post(&boot, api, Some(&alice), POOL).await;
    assert_eq!(st, StatusCode::CREATED, "{a}");
    let ca = a["caCert"].as_str().expect("caCert").to_owned();
    check_grant(&a, &ca);
    let c = client(api, Some(&ca));
    // A client trusting only caCert completes the TLS handshake with the API.
    c.get(format!("{}/v1/pools", base(api)))
        .bearer_auth(&alice)
        .send()
        .await
        .expect("caCert verifies the API serving cert");
    let a_id = a["id"].as_str().unwrap().to_owned();
    let pa = h.wait_claimed(&a_id).await;
    println!("a. OIDC POST 201, caCert verifies the serving cert: PASS");

    // b. Service-account token through TokenReview.
    kubectl(&["-n", NS, "create", "serviceaccount", "ci"]);
    let sa = kubectl(&["-n", NS, "create", "token", "ci", "--audience", AUD]);
    let (st, b) = post(&c, api, Some(&sa), POOL).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    check_grant(&b, &ca);
    assert_eq!(b["owner"], format!("sa:{NS}/ci"));
    let pb = h.wait_claimed(b["id"].as_str().unwrap()).await;
    let mut both = [pa.clone(), pb];
    both.sort();
    assert_eq!(both, [w1, w2], "one warm pod each");
    println!("b. SA token POST 201: PASS");

    // c. Pools lists only the accessible one.
    for (tok, want) in [
        (&alice, vec![POOL]),
        (&issuer.token(AUD, "other"), vec![HIDDEN]),
    ] {
        let v: Value = c
            .get(format!("{}/v1/pools", base(api)))
            .bearer_auth(tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let text = v.to_string();
        for p in [POOL, HIDDEN] {
            assert_eq!(
                text.contains(&format!("\"{p}\"")),
                want.contains(&p),
                "{text}"
            );
        }
    }
    println!("c. GET /v1/pools lists only accessible pools: PASS");

    // f (start). The pool is exhausted, so the next POST waits in the queue.
    let queued = {
        let (c, tok) = (c.clone(), alice.clone());
        tokio::spawn(async move { post(&c, api, Some(&tok), POOL).await })
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!queued.is_finished(), "queued while no warm pod is free");

    // d. Release. The non-owner is refused, the owner gets 204, and the pod drains.
    let del = |tok: String, id: String| {
        let c = c.clone();
        async move {
            c.delete(format!("{}/v1/allocations/{id}", base(api)))
                .bearer_auth(tok)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(del(sa.clone(), a_id.clone()).await, StatusCode::FORBIDDEN);
    assert_eq!(
        del(issuer.token(AUD, "other"), a_id.clone()).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(h.claimed(&a_id), std::slice::from_ref(&pa), "still claimed");
    assert_eq!(
        del(alice.clone(), a_id.clone()).await,
        StatusCode::NO_CONTENT
    );
    h.gone(&pa).await;
    println!("d. DELETE non-owner 403, owner 204, pod drained: PASS");

    // f. With no worker for 20 s the POST answers 202 with the id and the
    // place; the reconciler's replacement arrives; a long poll follows the
    // allocation to 200.
    let (st, p) = tokio::time::timeout(Duration::from_mins(1), queued)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(st, StatusCode::ACCEPTED, "{p}");
    assert_eq!(
        (p["state"].as_str(), p["position"].as_u64()),
        (Some("pending"), Some(1)),
        "{p}"
    );
    let f_id = p["id"].as_str().unwrap().to_owned();
    let get = |tok: String, q: &'static str| {
        let (c, id) = (c.clone(), f_id.clone());
        async move {
            let r = c
                .get(format!("{}/v1/allocations/{id}{q}", base(api)))
                .bearer_auth(tok)
                .send()
                .await
                .unwrap();
            let st = r.status();
            (st, r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let (st, v) = get(sa.clone(), "").await;
    assert_eq!(
        (st, v["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden")),
        "{v}"
    );
    let (st, v) = get(alice.clone(), "").await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    let poll = tokio::spawn(get(alice.clone(), "?wait=25s"));
    let w3 = h.warm("3").await;
    let (st, f) = tokio::time::timeout(Duration::from_secs(30), poll)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(st, StatusCode::OK, "{f}");
    check_grant(&f, &ca);
    assert_eq!(f["id"], f_id.as_str());
    assert_eq!(h.wait_claimed(&f_id).await, w3);
    println!(
        "f. POST 202 pending at place 1, GET 403 for others, long poll 200 once a worker is free: PASS"
    );
    assert_eq!(
        del(alice.clone(), a_id.clone()).await,
        StatusCode::NOT_FOUND,
        "released"
    );

    // e. The 401s and the 404 look the same as a missing pool.
    let forged = {
        let rng = SystemRandom::new();
        let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let k =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der.as_ref(), &rng).unwrap();
        sign(&k, &issuer.url, AUD, "build")
    };
    let wrong_sa = kubectl(&["-n", NS, "create", "token", "ci", "--audience", "other"]);
    for tok in [
        None,
        Some(forged.as_str()),
        Some(&issuer.token("other", "build")),
        Some(wrong_sa.as_str()),
    ] {
        let (st, v) = post(&c, api, tok, POOL).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"]["code"], "unauthenticated");
    }
    let (st1, v1) = post(&c, api, Some(&alice), HIDDEN).await;
    let (st2, v2) = post(&c, api, Some(&alice), "nope").await;
    assert_eq!((st1, st2), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
    assert_eq!(v1["error"]["code"], "pool_not_found");
    assert_eq!(v1, v2, "no access is identical to a missing pool");
    println!("e. 401 none/forged/wrong-aud (OIDC and SA), 404 pool_not_found identical: PASS");

    // h. The HTTP API never generates a private key: no publicKey is a 400.
    let (st, v) = post_body(
        &c,
        api,
        Some(&alice),
        &json!({"namespace": NS, "pool": POOL}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "invalid_request");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("publicKey is required"),
        "{v}"
    );
    println!("h. POST without publicKey 400 invalid_request, nothing allocated: PASS");

    // g. One counter for service outcomes and HTTP-layer rejections.
    let text = metrics.render();
    let total = |result: &str, ns: &str, pool: &str| {
        metric(
            &metrics,
            &format!(
                r#"buildkit_allocations_total{{namespace="{ns}",pool="{pool}",result="{result}"}}"#
            ),
        )
    };
    assert_eq!(total("unauthenticated", "", ""), 4, "{text}");
    assert_eq!(total("ok", NS, POOL), 3, "{text}");
    // Missing and inaccessible pools look alike here too and get empty labels.
    assert_eq!(total("pool_not_found", "", ""), 2, "{text}");
    assert_eq!(total("invalid_request", "", ""), 1, "{text}");
    println!("g. buildkit_allocations_total counts service and HTTP rejections: PASS");
    for l in text
        .lines()
        .filter(|l| l.starts_with("buildkit_allocations_total"))
    {
        println!("   {l}");
    }
    stop.cancel();
}

async fn wait_listener(c: &reqwest::Client, api: SocketAddr) {
    let end = tokio::time::Instant::now() + Duration::from_mins(1);
    loop {
        if c.get(format!("{}/v1/pools", base(api)))
            .send()
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "API listener never served"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
