//! Reconciler scenarios on a real API server. Run by `cargo xtask
//! reconcile-verify`, which provides a kind cluster (`KUBECONFIG`, admin), a
//! controller process on the host (`RV_OPS` ops address, authenticated as the
//! only user the API server's audit policy logs) and the node container
//! (`RV_NODE`) holding the audit log; ignored otherwise.
//!
//! Pod and pool transitions are recorded by watches (a retire patch and its
//! delete land milliseconds apart, so polling would miss `draining`); the
//! controller's own requests are counted from the audit log.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use buildkit_controller::config::Config;
use buildkit_controller::render::{
    self, ANN_ALLOC, ANN_ALLOCATED_AT, ANN_EXPIRES_AT, ANN_INTERNAL_CA, ANN_WORKER_NONCE,
    LABEL_POOL,
};
use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::StreamExt;
use k8s_openapi::api::authentication::v1::TokenRequest;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret, ServiceAccount};
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use serde_json::{Value, json};

const NS: &str = "bk-rv";
const CA: &str = "buildkit-controller-ca";

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} unset: run `cargo xtask reconcile-verify`"))
}

#[derive(Default)]
struct Rec {
    live: HashMap<String, BTreeSet<String>>,
    peak: HashMap<String, usize>,
    /// (pool, pod, "apply"|"delete", alloc annotation, wall-clock receipt time;
    /// receipt is at or after the write, which only weakens an early-retire check
    /// by the watch latency)
    pods: Vec<(String, String, &'static str, String, jiff::Timestamp)>,
    /// (pool, type, status, reason), on change only.
    conds: Vec<(String, String, String, String)>,
    /// Per pod uid: when the watch first saw it, and first saw it Ready.
    seen_at: HashMap<String, jiff::Timestamp>,
    ready_at: HashMap<String, jiff::Timestamp>,
    /// Per worker Secret (pool-labelled): every owner kind it was seen with.
    secret_owners: BTreeMap<String, BTreeSet<String>>,
}

type Shared = Arc<Mutex<Rec>>;

fn record_pod(rec: &Shared, p: &Pod, kind: &'static str) {
    let Some(pool) = p.labels().get(LABEL_POOL).cloned() else {
        return;
    };
    let alloc = p.annotations().get(ANN_ALLOC).cloned().unwrap_or_default();
    let mut r = rec.lock().unwrap();
    let set = r.live.entry(pool.clone()).or_default();
    if kind == "delete" {
        set.remove(&p.name_any());
    } else {
        set.insert(p.name_any());
    }
    let n = set.len();
    let peak = r.peak.entry(pool.clone()).or_default();
    *peak = (*peak).max(n);
    let now = jiff::Timestamp::now();
    let uid = p.metadata.uid.clone().unwrap_or_default();
    r.seen_at.entry(uid.clone()).or_insert(now);
    if ready(p) {
        r.ready_at.entry(uid).or_insert(now);
    }
    r.pods.push((pool, p.name_any(), kind, alloc, now));
}

fn record_secret(rec: &Shared, s: &Secret) {
    if !s.labels().contains_key(LABEL_POOL) {
        return;
    }
    let kinds = s
        .metadata
        .owner_references
        .iter()
        .flatten()
        .map(|o| o.kind.clone());
    let mut r = rec.lock().unwrap();
    let seen = r.secret_owners.entry(s.name_any()).or_default();
    seen.extend(kinds);
    if s.metadata
        .owner_references
        .as_ref()
        .is_none_or(Vec::is_empty)
    {
        seen.insert("<none>".into());
    }
}

fn record_pool(rec: &Shared, p: &BuildKitPool) {
    let mut r = rec.lock().unwrap();
    for c in p.status.iter().flat_map(|s| &s.conditions) {
        let row = (
            p.name_any(),
            c.type_.clone(),
            c.status.clone(),
            c.reason.clone(),
        );
        let last = r.conds.iter().rev().find(|x| x.0 == row.0 && x.1 == row.1);
        if last != Some(&row) {
            r.conds.push(row);
        }
    }
}

fn spawn_recorders(client: &Client, rec: &Shared) {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    let r = Arc::clone(rec);
    tokio::spawn(async move {
        let mut s = std::pin::pin!(watcher(pods, watcher::Config::default()));
        while let Some(ev) = s.next().await {
            match ev {
                Ok(watcher::Event::Apply(p) | watcher::Event::InitApply(p)) => {
                    record_pod(&r, &p, "apply");
                }
                Ok(watcher::Event::Delete(p)) => record_pod(&r, &p, "delete"),
                _ => {}
            }
        }
    });
    let secrets: Api<Secret> = Api::namespaced(client.clone(), NS);
    let r = Arc::clone(rec);
    tokio::spawn(async move {
        let mut s = std::pin::pin!(watcher(secrets, watcher::Config::default()));
        while let Some(ev) = s.next().await {
            if let Ok(watcher::Event::Apply(x) | watcher::Event::InitApply(x)) = ev {
                record_secret(&r, &x);
            }
        }
    });
    let pools: Api<BuildKitPool> = Api::namespaced(client.clone(), NS);
    let r = Arc::clone(rec);
    tokio::spawn(async move {
        let mut s = std::pin::pin!(watcher(pools, watcher::Config::default()));
        while let Some(ev) = s.next().await {
            if let Ok(watcher::Event::Apply(p) | watcher::Event::InitApply(p)) = ev {
                record_pool(&r, &p);
            }
        }
    });
}

async fn until<F, Fut>(what: &str, secs: u64, mut f: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if f().await {
            return start.elapsed();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("timed out after {secs}s waiting for {what}");
}

struct H {
    client: Client,
    pods: Api<Pod>,
    pools: Api<BuildKitPool>,
    ctrl_secrets: Api<Secret>,
    ctrl_ns: String,
    image: String,
    rec: Shared,
}

fn ready(p: &Pod) -> bool {
    p.status
        .iter()
        .flat_map(|s| s.conditions.iter().flatten())
        .any(|c| c.type_ == "Ready" && c.status == "True")
}

fn alloc(p: &Pod) -> &str {
    p.annotations().get(ANN_ALLOC).map_or("", String::as_str)
}

impl H {
    async fn list(&self, pool: &str) -> Vec<Pod> {
        self.pods
            .list(&ListParams::default().labels(&format!("{LABEL_POOL}={pool}")))
            .await
            .unwrap()
            .items
    }

    /// (all pods, ready `free` pods not terminating)
    async fn counts(&self, pool: &str) -> (usize, usize) {
        let pods = self.list(pool).await;
        let rf = pods
            .iter()
            .filter(|p| ready(p) && alloc(p) == "free" && p.metadata.deletion_timestamp.is_none())
            .count();
        (pods.len(), rf)
    }

    async fn pool(&self, name: &str) -> BuildKitPool {
        self.pools.get(name).await.unwrap()
    }

    /// The `P internal-ca` all of the pool's pods carry, or empty while they differ.
    async fn fingerprint(&self, pool: &str) -> String {
        let fps: BTreeSet<String> = self
            .list(pool)
            .await
            .iter()
            .map(|p| {
                p.annotations()
                    .get(ANN_INTERNAL_CA)
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        match fps.into_iter().collect::<Vec<_>>().as_slice() {
            [one] => one.clone(),
            _ => String::new(),
        }
    }

    /// Per pod: its `<pod>-<nonce>` Secret exists, is owned by the pod alone,
    /// and is the one the pod mounts. Returns the Secret names.
    async fn worker_secrets(&self, pool: &str) -> Result<BTreeSet<String>, String> {
        let api = Api::<Secret>::namespaced(self.client.clone(), NS);
        let mut names = BTreeSet::new();
        for p in self.list(pool).await {
            let pod = p.name_any();
            let nonce = p
                .annotations()
                .get(ANN_WORKER_NONCE)
                .ok_or(format!("{pod}: no nonce"))?;
            let name = render::worker_secret_name(&pod, nonce);
            let mounted: Vec<String> = p
                .spec
                .iter()
                .flat_map(|s| s.volumes.iter().flatten())
                .filter_map(|v| v.secret.as_ref()?.secret_name.clone())
                .collect();
            if mounted != [name.clone()] {
                return Err(format!("{pod} mounts {mounted:?}, not {name}"));
            }
            let s = api
                .get_opt(&name)
                .await
                .map_err(|e| e.to_string())?
                .ok_or(format!("{name} missing"))?;
            let owners = s.metadata.owner_references.unwrap_or_default();
            match owners.as_slice() {
                [o] if o.kind == "Pod" && Some(&o.uid) == p.metadata.uid.as_ref() => {}
                _ => return Err(format!("{name} owners {owners:?}")),
            }
            names.insert(name);
        }
        Ok(names)
    }

    async fn set_waiters(&self, pool: &str, count: i32) {
        let waiters = if count > 0 {
            json!([{"replica": "rv-test", "count": count,
                    "updatedAt": jiff::Timestamp::now().round(jiff::Unit::Second).unwrap().to_string()}])
        } else {
            Value::Null
        };
        self.pools
            .patch_status(
                pool,
                &PatchParams::default(),
                &Patch::Merge(json!({"status": {"waiters": waiters}})),
            )
            .await
            .unwrap();
    }

    fn deletes_since(&self, mark: usize, pool: &str) -> Vec<String> {
        let r = self.rec.lock().unwrap();
        r.pods[mark..]
            .iter()
            .filter(|e| e.0 == pool && e.2 == "delete")
            .map(|e| e.1.clone())
            .collect()
    }

    fn mark(&self) -> usize {
        self.rec.lock().unwrap().pods.len()
    }

    async fn events(&self) -> Vec<Event> {
        Api::<Event>::namespaced(self.client.clone(), &self.ctrl_ns)
            .list(&ListParams::default())
            .await
            .unwrap()
            .items
    }
}

fn audit() -> Vec<Value> {
    let out = Command::new("docker")
        .args([
            "exec",
            &env("RV_NODE"),
            "cat",
            "/var/log/kubernetes/audit.log",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "reading the audit log");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// `verb resource[/subresource] code` of a completed audit event.
fn key(e: &Value) -> String {
    let o = &e["objectRef"];
    let sub = o["subresource"]
        .as_str()
        .map_or(String::new(), |s| format!("/{s}"));
    format!(
        "{} {}{} {}",
        e["verb"].as_str().unwrap_or("?"),
        o["resource"].as_str().unwrap_or("?"),
        sub,
        e["responseStatus"]["code"]
    )
}

fn completed(events: &[Value]) -> impl Iterator<Item = &Value> {
    events.iter().filter(|e| e["stage"] == "ResponseComplete")
}

fn metric(needle: &str) -> f64 {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(env("RV_OPS")).unwrap();
    s.write_all(b"GET /metrics HTTP/1.0\r\nHost: x\r\n\r\n")
        .unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    body.lines()
        .filter(|l| l.starts_with(needle))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

/// Every field of `want` is present and equal in `have` (arrays pairwise).
fn subset(want: &Value, have: &Value, path: &str) -> Result<(), String> {
    match (want, have) {
        (Value::Object(w), Value::Object(h)) => w.iter().try_for_each(|(k, v)| {
            subset(v, h.get(k).unwrap_or(&Value::Null), &format!("{path}.{k}"))
        }),
        (Value::Array(w), Value::Array(h)) if w.len() == h.len() => w
            .iter()
            .zip(h)
            .enumerate()
            .try_for_each(|(i, (a, b))| subset(a, b, &format!("{path}[{i}]"))),
        _ if want == have => Ok(()),
        _ => Err(format!("{path}: want {want}, have {have}")),
    }
}

fn pool_json(name: &str, image: &str, min: i32, max: i32, internet: &str) -> BuildKitPool {
    serde_json::from_value(json!({
        "apiVersion": "buildkit.smrt-devops.net/v1alpha1",
        "kind": "BuildKitPool",
        "metadata": {"name": name, "namespace": NS},
        "spec": {
            "template": {"spec": {"containers": [
                {"name": "buildkitd", "image": image, "imagePullPolicy": "IfNotPresent"}]}},
            "scaling": {"min": min, "max": max, "idleTimeout": "1m"},
            "network": {"egress": {"internet": internet}},
            "access": {"rules": [{"serviceAccount": {"name": "builder"}}]}
        }
    }))
    .unwrap()
}

fn cond(p: &BuildKitPool, t: &str) -> Option<(String, String)> {
    p.status
        .iter()
        .flat_map(|s| &s.conditions)
        .find(|c| c.type_ == t)
        .map(|c| (c.status.clone(), c.reason.clone()))
}

/// Like [`until`], polling every 50 ms, for a state that lasts only seconds.
async fn soon<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out after {secs}s waiting for {what}");
}

/// A second controller replica, started and killed by the scenarios. It runs
/// the same binary and identity as the first, on its own loopback ports.
struct Replica {
    child: Child,
    /// `listen.api`.
    api: String,
}

impl Replica {
    async fn start(name: &str) -> Self {
        let config = std::path::PathBuf::from(env("RV_CONFIG"));
        let dir = config.parent().unwrap().to_owned();
        let mut text = std::fs::read_to_string(&config).unwrap();
        // Every listen address gets a fresh port.
        let mut at = 0;
        let mut api = String::new();
        while let Some(i) = text[at..].find("127.0.0.1:") {
            let start = at + i;
            let end = start
                + "127.0.0.1:".len()
                + text[start + "127.0.0.1:".len()..]
                    .bytes()
                    .take_while(u8::is_ascii_digit)
                    .count();
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let addr = format!("127.0.0.1:{port}");
            if text[..start].ends_with("api: ") {
                api.clone_from(&addr);
            }
            text.replace_range(start..end, &addr);
            at = start + addr.len();
        }
        let ops = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("ops: "))
            .unwrap()
            .to_owned();
        assert!(!api.is_empty(), "no listen.api in {text}");
        let path = dir.join(format!("{name}.yaml"));
        std::fs::write(&path, &text).unwrap();
        let log = std::fs::File::create(dir.join(format!("{name}.log"))).unwrap();
        let child = Command::new(env("RV_BIN"))
            .arg("--config")
            .arg(&path)
            .env("KUBECONFIG", env("RV_CTRL_KUBECONFIG"))
            .env("POD_NAME", name)
            .env("POD_NAMESPACE", env("RV_NAMESPACE"))
            .env("LOG_LEVEL", "info,buildkit_controller=debug")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        until(&format!("{name} ready"), 60, || {
            let url = format!("http://{ops}/readyz");
            async move {
                reqwest::get(url)
                    .await
                    .is_ok_and(|r| r.status().is_success())
            }
        })
        .await;
        Self { child, api }
    }

    /// SIGKILL: no orphan guard runs, nothing is cleaned up.
    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A token for the `ServiceAccount` `name` in [`NS`], created if missing,
/// with the audience the controller accepts by default.
async fn sa_token(client: &Client, name: &str) -> String {
    let sas = Api::<ServiceAccount>::namespaced(client.clone(), NS);
    let sa = serde_json::from_value(json!({"metadata": {"name": name}})).unwrap();
    if let Err(e) = sas.create(&PostParams::default(), &sa).await {
        assert!(e.to_string().contains("AlreadyExists"), "{e}");
    }
    let tr: TokenRequest = serde_json::from_value(
        json!({"spec": {"audiences": ["buildkit-controller"], "expirationSeconds": 3600}}),
    )
    .unwrap();
    sas.create_token_request(name, &PostParams::default(), &tr)
        .await
        .unwrap()
        .status
        .unwrap()
        .token
}

fn ts(p: &Pod, key: &str) -> jiff::Timestamp {
    p.annotations()[key].parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a kind cluster; run `cargo xtask reconcile-verify`"]
async fn reconcile_verify() {
    let client = Client::try_default().await.unwrap();
    let ctrl_ns = env("RV_NAMESPACE");
    let image = env("RV_IMAGE");
    let config = Config::from_yaml(&std::fs::read_to_string(env("RV_CONFIG")).unwrap()).unwrap();
    Api::<Namespace>::all(client.clone())
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({"metadata": {"name": NS}})).unwrap(),
        )
        .await
        .unwrap();
    let rec = Shared::default();
    spawn_recorders(&client, &rec);
    let h = H {
        pods: Api::namespaced(client.clone(), NS),
        pools: Api::namespaced(client.clone(), NS),
        ctrl_secrets: Api::namespaced(client.clone(), &ctrl_ns),
        client: client.clone(),
        ctrl_ns: ctrl_ns.clone(),
        image,
        rec: Arc::clone(&rec),
    };
    let mut summary: Vec<String> = Vec::new();

    // 1. The controller creates min warm pods and they become Ready (real buildkitd).
    h.pools
        .create(
            &PostParams::default(),
            &pool_json("p", &h.image, 2, 4, "Allow"),
        )
        .await
        .unwrap();
    let took = until("2 Ready free pods in p", 300, || async {
        h.counts("p").await == (2, 2)
    })
    .await;
    // The status follows in the pass the last Ready event triggers.
    until("status warm 2", 10, || async {
        h.pool("p").await.status.and_then(|s| s.warm) == Some(2)
    })
    .await;
    let p = h.pool("p").await;
    let st = p.status.clone().unwrap();
    assert_eq!(st.observed_generation, p.metadata.generation);
    assert_eq!((st.warm, st.desired_warm), (Some(2), Some(2)));
    // Re-owned by a create, a pod-uid patch and no read of Secrets.
    until("each pod owns its worker Secret", 30, || async {
        h.worker_secrets("p").await.is_ok()
    })
    .await;
    assert_eq!(h.worker_secrets("p").await.unwrap().len(), 2);
    summary.push(format!(
        "S1 PASS: 2/2 Ready free in {}s; status applied (observedGeneration {:?}, warm 2, Ready {:?})",
        took.as_secs(),
        st.observed_generation,
        cond(&p, "Ready")
    ));

    // 7. ServiceAccount and NetworkPolicy equal render's output.
    let rc = config.render_config(&ctrl_ns);
    let name = render::worker_object_name("p");
    let sa = Api::<ServiceAccount>::namespaced(client.clone(), NS)
        .get(&name)
        .await
        .unwrap();
    let np = Api::<NetworkPolicy>::namespaced(client.clone(), NS)
        .get(&name)
        .await
        .unwrap();
    let want_sa = serde_json::to_value(render::service_account(&p).unwrap()).unwrap();
    let want_np = serde_json::to_value(render::network_policy(&p, &rc).unwrap()).unwrap();
    subset(&want_sa, &serde_json::to_value(&sa).unwrap(), "sa").unwrap();
    let have_np = serde_json::to_value(&np).unwrap();
    subset(&want_np, &have_np, "np").unwrap();
    // The harness config's non-default workerNetwork, literally (not via render).
    let egress = &have_np["spec"]["egress"];
    assert_eq!(
        egress[0]["to"][0],
        json!({"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "rv-dns"}},
               "podSelector": {"matchLabels": {"rv-dns": "yes"}}}),
        "dns pod peer: {egress}"
    );
    assert_eq!(
        egress[0]["to"][1]["ipBlock"]["cidr"], "198.51.100.53/32",
        "dns cidr peer: {egress}"
    );
    let internet = egress
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["to"][0]["ipBlock"]["cidr"] == "0.0.0.0/0")
        .expect("internet rule (pool p allows internet)");
    assert!(
        internet["to"][0]["ipBlock"]["except"]
            .as_array()
            .unwrap()
            .contains(&json!("203.0.113.0/24")),
        "extraDeniedCIDRs in except: {internet}"
    );
    summary.push(format!(
        "S7 PASS: SA and NetworkPolicy `{name}` contain every rendered field ({} NP egress rules)",
        np.spec.and_then(|s| s.egress).map_or(0, |e| e.len())
    ));

    // 2. A demand burst never exceeds max.
    let mark = h.mark();
    h.set_waiters("p", 10).await;
    let took = until("4 Ready free pods in p", 300, || async {
        h.counts("p").await == (4, 4)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let p = h.pool("p").await;
    let st = p.status.clone().unwrap();
    let (peak, names) = {
        let r = rec.lock().unwrap();
        let names: BTreeSet<String> = r.pods[mark..]
            .iter()
            .filter(|e| e.0 == "p")
            .map(|e| e.1.clone())
            .collect();
        (r.peak["p"], names)
    };
    assert!(peak <= 4, "peak {peak} > max 4");
    assert!(
        names
            .iter()
            .all(|n| ["p-0", "p-1", "p-2", "p-3"].contains(&n.as_str()))
    );
    assert_eq!(
        (st.waiting, st.desired_warm),
        (Some(10), Some(4)),
        "desiredWarm is capped by free slots"
    );
    summary.push(format!(
        "S2 PASS: waiters=10, max=4: 4 Ready in {}s, peak {peak} pods, names {names:?}, status waiting 10, desiredWarm 4 (capped by slots)",
        took.as_secs()
    ));

    // 3. Idle reaping. Once demand is gone, free pods above min retire after idleTimeout (1m).
    let mark = h.mark();
    h.set_waiters("p", 0).await;
    let took = until("p back to 2 pods", 180, || async {
        h.counts("p").await == (2, 2)
    })
    .await;
    let reaped = h.deletes_since(mark, "p");
    let drained = rec.lock().unwrap().pods[mark..]
        .iter()
        .filter(|e| e.0 == "p" && e.3 == "draining")
        .map(|e| e.1.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(reaped.len(), 2, "reaped {reaped:?}");
    assert!(reaped.iter().all(|n| drained.contains(n)));
    summary.push(format!(
        "S3 PASS: 2 idle pods retired (draining then deleted) {reaped:?} in {}s, 2 Ready free remain",
        took.as_secs()
    ));

    // 4. An allocated pod past expires-at drains, is deleted, then is replaced.
    let victim = h
        .list("p")
        .await
        .into_iter()
        .find(|p| alloc(p) == "free")
        .unwrap()
        .name_any();
    let now = jiff::Timestamp::now().round(jiff::Unit::Second).unwrap();
    let expires = now + jiff::SignedDuration::from_secs(15);
    let mark = h.mark();
    h.pods
        .patch(
            &victim,
            &PatchParams::default(),
            &Patch::Merge(json!({"metadata": {"annotations": {
                ANN_ALLOC: "aaaaaaaaaaaaaaaaaaaaaaaaaa",
                ANN_ALLOCATED_AT: now.to_string(),
                ANN_EXPIRES_AT: expires.to_string(),
            }}})),
        )
        .await
        .unwrap();
    // The pod is still allocated 2 s before expiry, so nothing retires early.
    let check_at = expires - jiff::SignedDuration::from_secs(2);
    tokio::time::sleep(
        check_at
            .duration_since(jiff::Timestamp::now())
            .unsigned_abs(),
    )
    .await;
    assert!(jiff::Timestamp::now() < expires);
    let pre = h.pods.get(&victim).await.unwrap();
    assert_eq!(
        alloc(&pre),
        "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "allocated at expires-2s"
    );
    until("victim deleted", 90, || async {
        h.deletes_since(mark, "p").contains(&victim)
    })
    .await;
    let first_drain = rec.lock().unwrap().pods[mark..]
        .iter()
        .find(|e| e.1 == victim && e.3 == "draining")
        .map(|e| e.4)
        .expect("draining seen");
    let late_d = first_drain.duration_since(expires);
    assert!(
        late_d >= jiff::SignedDuration::ZERO && late_d <= jiff::SignedDuration::from_secs(5),
        "first draining {late_d:?} after expires-at (want 0..=5 s)"
    );
    let late = late_d.as_secs_f64();
    let seq: Vec<String> = rec.lock().unwrap().pods[mark..]
        .iter()
        .filter(|e| e.1 == victim)
        .map(|e| format!("{}:{}", e.2, e.3))
        .collect();
    let i_drain = seq
        .iter()
        .position(|s| s == "apply:draining")
        .expect("draining seen");
    assert!(
        seq[..i_drain]
            .iter()
            .any(|s| s.ends_with("aaaaaaaaaaaaaaaaaaaaaaaaaa"))
    );
    assert!(seq.last().unwrap().starts_with("delete"));
    until("p back to 2 Ready free", 180, || async {
        h.counts("p").await == (2, 2)
    })
    .await;
    summary.push(format!(
        "S4 PASS: {victim} allocated, expired, retired {late:.1}s after expires-at; sequence {seq:?}; replaced"
    ));

    // 5. After the CA Secret is deleted, the controller creates a new CA, issues fresh per-pod Secrets, retires stale pods and creates fresh ones.
    let fp0 = h.fingerprint("p").await;
    let old: BTreeSet<String> = h.list("p").await.iter().map(|p| p.uid().unwrap()).collect();
    let ws0 = h.worker_secrets("p").await.unwrap();
    let mark = h.mark();
    h.ctrl_secrets
        .delete(CA, &DeleteParams::default())
        .await
        .unwrap();
    let took = until("CA healed and pods recycled", 300, || async {
        let fp = h.fingerprint("p").await;
        let pods = h.list("p").await;
        h.ctrl_secrets.get_opt(CA).await.unwrap().is_some()
            && !fp.is_empty()
            && fp != fp0
            && pods.len() == 2
            && pods.iter().all(|p| {
                ready(p)
                    && !old.contains(&p.uid().unwrap())
                    && p.annotations().get(ANN_INTERNAL_CA).map(String::as_str) == Some(fp.as_str())
            })
    })
    .await;
    let ws1 = h.worker_secrets("p").await.unwrap();
    assert!(
        ws0.is_disjoint(&ws1),
        "fresh pods, fresh Secrets: {ws0:?} {ws1:?}"
    );
    // The retired pods' Secrets go with them (garbage collection by owner).
    let secrets = Api::<Secret>::namespaced(client.clone(), NS);
    until("retired pods' Secrets collected", 60, || async {
        for n in &ws0 {
            if secrets.get_opt(n).await.unwrap().is_some() {
                return false;
            }
        }
        true
    })
    .await;
    let retired = h.deletes_since(mark, "p");
    assert!(retired.len() >= 2);
    let evs = h.events().await;
    let created_ev: Vec<&Event> = evs
        .iter()
        .filter(|e| e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some(CA))
        .collect();
    assert!(!created_ev.is_empty(), "CA Event recorded");
    summary.push(format!(
        "S5 PASS: CA deleted -> new CA (fp {}.. -> {}..), per-pod Secrets reissued and old ones collected, {} stale pods retired, 2 fresh Ready in {}s; Events {:?}",
        &fp0[..8],
        &h.fingerprint("p").await[..8],
        retired.len(),
        took.as_secs(),
        created_ev.iter().map(|e| e.reason.clone().unwrap_or_default()).collect::<Vec<_>>()
    ));

    // 6. A corrupted CA Secret is backed up and replaced, with a CAReplaced condition and Event.
    let fp1 = h.fingerprint("p").await;
    let garbage = k8s_openapi::ByteString(b"garbage".to_vec());
    let mut s = h.ctrl_secrets.get(CA).await.unwrap();
    let key0 = s.data.as_ref().unwrap().keys().next().unwrap().clone();
    s.data
        .as_mut()
        .unwrap()
        .insert(key0.clone(), garbage.clone());
    h.ctrl_secrets
        .replace(CA, &PostParams::default(), &s)
        .await
        .unwrap();
    let took = until("CA replaced and pods recycled", 300, || async {
        let fp = h.fingerprint("p").await;
        let pods = h.list("p").await;
        !fp.is_empty()
            && fp != fp1
            && pods.len() == 2
            && pods.iter().all(|p| {
                ready(p) && p.annotations().get(ANN_INTERNAL_CA).map(String::as_str) == fp.get(..16)
            })
    })
    .await;
    let backups: Vec<Secret> = h
        .ctrl_secrets
        .list(&ListParams::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|s| s.name_any().starts_with(&format!("{CA}-replaced-")))
        .collect();
    assert_eq!(backups.len(), 1, "one backup");
    let b = &backups[0];
    assert_eq!(b.data.as_ref().unwrap().get(&key0), Some(&garbage));
    assert!(
        b.labels().values().any(|v| v == "ca-replaced"),
        "{:?}",
        b.labels()
    );
    let live = h.ctrl_secrets.get(CA).await.unwrap();
    assert_ne!(live.data.as_ref().unwrap().get(&key0), Some(&garbage));
    let evs = h.events().await;
    let ev = evs
        .iter()
        .find(|e| e.reason.as_deref() == Some("CAReplaced"))
        .expect("CAReplaced Event");
    assert!(ev.reporting_controller.is_some() && ev.reporting_instance.is_some());
    assert!(ev.action.is_some() && ev.event_time.is_some());
    let degraded_seen = rec
        .lock()
        .unwrap()
        .conds
        .iter()
        .any(|c| c.0 == "p" && c.1 == "Degraded" && c.2 == "True" && c.3 == "CAReplaced");
    assert!(degraded_seen, "Degraded=CAReplaced recorded");
    summary.push(format!(
        "S6 PASS: corrupted `{key0}` -> backup {} (labels {:?}), CA replaced in {}s, Degraded=CAReplaced seen, Event {}/{} by {:?}",
        b.name_any(),
        b.labels(),
        took.as_secs(),
        ev.type_.clone().unwrap_or_default(),
        ev.reason.clone().unwrap_or_default(),
        ev.reporting_controller
    ));

    // 8. A foreign unlabelled pod on slot q-0 gives Degraded=SlotTaken and one 409 per hold.
    let foreign: Pod = serde_json::from_value(json!({
        "metadata": {"name": "q-0", "namespace": NS},
        "spec": {"containers": [{"name": "c", "image": h.image,
            "imagePullPolicy": "IfNotPresent", "command": ["sleep", "3600"]}]}
    }))
    .unwrap();
    h.pods
        .create(&PostParams::default(), &foreign)
        .await
        .unwrap();
    h.pools
        .create(
            &PostParams::default(),
            &pool_json("q", &h.image, 1, 1, "Deny"),
        )
        .await
        .unwrap();
    until("q Degraded=SlotTaken", 120, || async {
        cond(&h.pool("q").await, "Degraded") == Some(("True".into(), "SlotTaken".into()))
    })
    .await;
    tokio::time::sleep(Duration::from_secs(70)).await;
    let conflicts = completed(&audit())
        .filter(|e| {
            e["verb"] == "create"
                && e["objectRef"]["resource"] == "pods"
                && e["objectRef"]["name"] == "q-0"
        })
        .map(|e| e["responseStatus"]["code"].as_i64().unwrap_or(0))
        .collect::<Vec<_>>();
    assert_eq!(conflicts, vec![409], "creates of q-0: {conflicts:?}");
    let metric_409 = metric("buildkit_pod_operations_total{");
    assert!(h.list("q").await.is_empty(), "no labelled q pods");
    summary.push(format!(
        "S8 PASS: Degraded=SlotTaken; creates of q-0 over 70s+: {conflicts:?} (one 409 per 10 min hold); pod_operations_total sum {metric_409}"
    ));

    // 9. The controller does not poll the API while idle.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let before = audit().len();
    tokio::time::sleep(Duration::from_mins(1)).await;
    let all = audit();
    let mut idle: BTreeMap<String, usize> = BTreeMap::new();
    for e in completed(&all[before..]) {
        *idle.entry(key(e)).or_default() += 1;
    }
    // The idle allowance is Lease renewals (every 2 s, so 30 per 60 s, bound 35) and
    // watch completions (server-side timeouts of the few long-running watches,
    // each 5 min or more, bound 8). Any other request is polling.
    let renewals = idle.get("update leases 200").copied().unwrap_or(0);
    let watches: usize = idle
        .iter()
        .filter(|(k, _)| k.starts_with("watch ") && k.ends_with(" 200"))
        .map(|(_, n)| n)
        .sum();
    let other: Vec<&String> = idle
        .keys()
        .filter(|k| *k != "update leases 200" && !(k.starts_with("watch ") && k.ends_with(" 200")))
        .collect();
    assert!(
        other.is_empty(),
        "non-renewal requests while idle: {idle:?}"
    );
    assert!(
        (1..=35).contains(&renewals),
        "lease renewals {renewals} in 60 s: {idle:?}"
    );
    assert!(
        watches <= 8,
        "watch completions {watches} in 60 s: {idle:?}"
    );
    summary.push(format!(
        "S9 PASS: 60s idle: {renewals} lease renewals (<= 35), {watches} watch completions (<= 8), no other request; all: {idle:?}"
    ));

    // 10 and 11 run a second replica, which serves the API, against pool `r`.
    // The pool keeps no warm pods, so each allocation creates its pod.
    let start_timeout = 20;
    let mut pool_r = pool_json("r", &h.image, 0, 1, "Allow");
    pool_r.spec.workers.start_timeout =
        serde_json::from_value(json!(format!("{start_timeout}s"))).unwrap();
    h.pools
        .create(&PostParams::default(), &pool_r)
        .await
        .unwrap();
    until("r Ready", 60, || async {
        cond(&h.pool("r").await, "Ready").is_some_and(|c| c.0 == "True")
    })
    .await;
    let mut replica = Replica::start("rv-1").await;
    let token = sa_token(&client, "builder").await;
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let allocate = || {
        let req = http
            .post(format!("https://{}/v1/allocations", replica.api))
            .bearer_auth(&token)
            .json(&json!({"namespace": NS, "pool": "r", "ttlSeconds": 3600, "queueTimeoutSeconds": 0}));
        tokio::spawn(req.send())
    };
    let r_secrets = || async {
        Api::<Secret>::namespaced(client.clone(), NS)
            .list(&ListParams::default().labels(&format!("{LABEL_POOL}=r")))
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|s| s.name_any())
            .collect::<Vec<_>>()
    };
    let claimed = |p: &Pod| !matches!(alloc(p), "" | "free" | "draining");

    // 12. Cold-start latency, printed for comparison across changes: each of
    // five allocations creates its pod. Pod times are watch receipt times.
    let (mut answered, mut started) = (Vec::new(), Vec::new());
    for _ in 0..5 {
        let mark = h.mark();
        let sent = Instant::now();
        let resp = allocate().await.unwrap().unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
        answered.push(sent.elapsed().as_millis());
        let body: Value = resp.json().await.unwrap();
        let uid = h.pods.get("r-0").await.unwrap().metadata.uid.unwrap();
        {
            let log = rec.lock().unwrap();
            let ms = log.ready_at[&uid]
                .duration_since(log.seen_at[&uid])
                .as_millis();
            started.push(u128::try_from(ms).unwrap());
        }
        let released = http
            .delete(format!(
                "https://{}/v1/allocations/{}",
                replica.api,
                body["id"].as_str().unwrap()
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(released.status(), reqwest::StatusCode::NO_CONTENT);
        until("r-0 released and gone", 60, || async {
            h.deletes_since(mark, "r").contains(&"r-0".to_owned()) && h.list("r").await.is_empty()
        })
        .await;
    }
    let stats = |v: &mut Vec<u128>| {
        v.sort_unstable();
        format!(
            "p50 {} ms, max {} ms, all {v:?}",
            v[v.len() / 2],
            v[v.len() - 1]
        )
    };
    summary.push(format!(
        "S12 latency (n=5): POST to 201 {}; pod seen to Ready {}",
        stats(&mut answered),
        stats(&mut started)
    ));
    println!("{}", summary.last().unwrap());

    // 10. A client that gives up mid cold start (a cancelled CI job) leaves
    // neither the pod nor its worker Secret behind.
    let mark = h.mark();
    let inflight = allocate();
    soon("r-0 created", 60, || async {
        h.pods.get_opt("r-0").await.unwrap().is_some()
    })
    .await;
    inflight.abort();
    let pre = h.pods.get("r-0").await.unwrap();
    assert!(
        claimed(&pre) && !ready(&pre),
        "r-0 not mid cold start: {pre:?}"
    );
    let took = until(
        "r-0 retired and its worker Secret collected",
        120,
        || async { h.list("r").await.is_empty() && r_secrets().await.is_empty() },
    )
    .await;
    let seq: Vec<String> = rec.lock().unwrap().pods[mark..]
        .iter()
        .filter(|e| e.1 == "r-0")
        .map(|e| format!("{}:{}", e.2, if e.3.len() == 26 { "<id>" } else { &e.3 }))
        .collect();
    assert!(seq.contains(&"apply:draining".to_owned()), "{seq:?}");
    summary.push(format!(
        "S10 PASS: client cancelled mid cold start; r-0 retired and no worker Secret of r left after {}s; sequence {seq:?}",
        took.as_secs()
    ));

    // 11. A replica killed (SIGKILL) between the claim and the response
    // holds the pod only until the claim's short expiry, at most
    // startTimeout + 60 s, and the leader then retires it.
    let mark = h.mark();
    let inflight = allocate();
    soon("r-0 created pre-claimed", 60, || async {
        h.pods
            .get_opt("r-0")
            .await
            .unwrap()
            .is_some_and(|p| claimed(&p))
    })
    .await;
    replica.kill();
    inflight.abort();
    let pre = h.pods.get("r-0").await.unwrap();
    assert!(
        !ready(&pre),
        "r-0 was Ready before the kill, so it may have committed"
    );
    let id = alloc(&pre).to_owned();
    let (allocated, expires) = (ts(&pre, ANN_ALLOCATED_AT), ts(&pre, ANN_EXPIRES_AT));
    let bound = jiff::SignedDuration::from_secs(start_timeout + 60);
    let window = expires.duration_since(allocated);
    assert!(
        window >= bound && window <= bound + jiff::SignedDuration::from_secs(1),
        "claim window {window:?}, want startTimeout + 60 s"
    );
    // The pod starts, so no start timeout applies: only the expiry can free it.
    until("r-0 Running with an IP", 60, || async {
        h.pods
            .get("r-0")
            .await
            .unwrap()
            .status
            .is_some_and(|s| s.phase.as_deref() == Some("Running") && s.pod_ip.is_some())
    })
    .await;
    let check_at = expires - jiff::SignedDuration::from_secs(2);
    tokio::time::sleep(
        check_at
            .duration_since(jiff::Timestamp::now())
            .unsigned_abs(),
    )
    .await;
    assert!(jiff::Timestamp::now() < expires);
    assert_eq!(
        alloc(&h.pods.get("r-0").await.unwrap()),
        id,
        "r-0 retired early"
    );
    until("r-0 deleted", 90, || async {
        h.deletes_since(mark, "r").contains(&"r-0".to_owned())
    })
    .await;
    let first_drain = rec.lock().unwrap().pods[mark..]
        .iter()
        .find(|e| e.1 == "r-0" && e.3 == "draining")
        .map(|e| e.4)
        .expect("draining seen");
    let late = first_drain.duration_since(expires);
    assert!(
        late >= jiff::SignedDuration::ZERO && late <= jiff::SignedDuration::from_secs(5),
        "first draining {late:?} after the claim's expires-at (want 0..=5 s)"
    );
    until("r's worker Secret collected", 60, || async {
        r_secrets().await.is_empty()
    })
    .await;
    summary.push(format!(
        "S11 PASS: replica SIGKILLed mid cold start; claim expires-at = allocated-at + {}s; leader retired r-0 {:.1}s after it ({:.0}s after the claim), Secret collected",
        window.as_secs(),
        late.as_secs_f64(),
        first_drain.duration_since(allocated).as_secs_f64()
    ));

    // Every controller request the API server rejected over the whole run.
    let mut rejected: BTreeMap<String, usize> = BTreeMap::new();
    for e in completed(&all) {
        if e["responseStatus"]["code"].as_i64().unwrap_or(0) >= 400 {
            *rejected.entry(key(e)).or_default() += 1;
        }
    }
    let bad: Vec<&String> = rejected
        .keys()
        .filter(|k| k.contains("buildkitpools/status") || k.contains(" events "))
        .collect();
    assert!(
        bad.is_empty(),
        "status apply / Event rejected: {rejected:?}"
    );
    summary.push(format!(
        "API: no status apply or Event rejected; all >=400 responses: {rejected:?}"
    ));
    // No worker Secret was ever owned by anything but its pod.
    let owners = rec.lock().unwrap().secret_owners.clone();
    let wrong: Vec<_> = owners
        .iter()
        .filter(|(_, kinds)| kinds.iter().any(|k| k != "Pod"))
        .collect();
    assert!(
        wrong.is_empty(),
        "worker Secrets with other owners: {wrong:?}"
    );
    summary.push(format!(
        "Secrets: {} worker Secrets seen, each owned only by its pod",
        owners.len()
    ));
    let conds = rec.lock().unwrap().conds.clone();
    summary.push(format!("conditions seen: {conds:?}"));
    println!("\n==== reconcile-verify ====\n{}", summary.join("\n"));
}
