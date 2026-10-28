# Operations

## Configuration

The chart renders one YAML file from `values.yaml` into a ConfigMap, and the controller reads it. Unknown keys and invalid values stop startup with exit code 2 and a list of every error. There is no live reload. Helm rolls the Deployment when the file changes. Every key is documented in [values.yaml](../helm/buildkit-controller/values.yaml) and checked by `values.schema.json`, the chart is described in the [Helm README](../helm/buildkit-controller/README.md), and the pool fields are in [architecture.md](architecture.md#one-crd).

## Namespace scope

Set `watchNamespaces` to the namespaces that hold pools, for example `watchNamespaces: [ci-builders, team-b-builders]`. The chart then renders a Role and RoleBinding in each listed namespace for the worker pods, Secrets, ServiceAccounts, PVCs and NetworkPolicies, and the controller watches pools and worker pods in those namespaces only. A pool in another namespace gets no status and no pods. The only cluster-wide grant left is TokenReview, and the release namespace keeps its own Role for the Lease, the CA Secret, Events and the CA bundle ConfigMap. To add a pool namespace, add it to the list and run `helm upgrade`; the controller restarts and picks it up.

The default, `[]`, serves every namespace through a ClusterRole that can create and delete pods, Secrets and ServiceAccounts in any namespace, kube-system included. Anyone who takes over the controller then controls the cluster. `helm install` prints a warning in that mode.

`watchNamespace` (one string) still works as a deprecated alias for a one-entry list. Setting both fails, in the chart and at controller startup.

## Sizing

- Pool size. Set `scaling.max` to the number of builds you want to run at once. Each worker pod requests 500m CPU, 1Gi memory and 1Gi ephemeral storage by default, with limits of 4Gi memory and 20Gi ephemeral storage. Override these in `spec.template.spec.containers[0].resources`. Limits must include `memory` and `ephemeral-storage`.
- Cache. Worker storage is an `emptyDir`, so the BuildKit cache disappears with the pod. Use registry cache (`--cache-to` and `--cache-from`) in your builds. The ephemeral-storage limit bounds the local cache, and `spec.workers.cache.sizeLimit` caps the `emptyDir`.
- Controller. The chart defaults are 2 replicas, a 100m CPU and 128Mi memory request, and a 512Mi memory limit. The edge copies bytes for every build, so watch CPU on the replicas under heavy load and raise `replicaCount` or enable `autoscaling` (CPU-based HPA). Per replica, the edge allows 10000 established connections (`edge.maxConnections`) and 1024 connections in the TLS handshake (`edge.maxHandshakes`), 32 of them from one client address (`edge.maxHandshakesPerIp`). The API allows 4096 connections (`api.maxConnections`), 128 of them from one client address (`api.maxConnectionsPerIp`). An IPv6 client address is its /64. Connections over a cap are closed on accept and logged at `debug`. The per-address caps count the source address the controller sees, so they need source-IP preservation. The chart sets `service.externalTrafficPolicy: Local` for that. With `Cluster`, a load balancer that rewrites the source, a proxy, or runners behind one NAT address, many clients share one address: raise `api.maxConnectionsPerIp`, `edge.maxHandshakesPerIp` and `auth.serviceAccount.maxReviewsPerIp`, or set them to 0 to disable them.
- Load balancer. A queued request is silent for up to 12 minutes 10 seconds with default pool timeouts. Raise the load balancer idle timeout above that, per the table in [../helm/README.md](../helm/README.md).
- Quotas. Pods count against the pool namespace's ResourceQuota. A refused create shows as `Degraded=True/PodCreateFailed`.

## Scaling

The reconciler keeps `max(effective min, waiting requests)` pods warm, capped at `scaling.max`.

- `scaling.min` is the floor. The default is 0, so an idle pool costs nothing and the first build pays a pod start.
- `scaling.schedule` raises the floor in time windows (days, `HH:MM` start and end, and a `min`). The effective floor is the larger of `scaling.min` and every open window. `scaling.timeZone` is an IANA zone and defaults to UTC.
- `scaling.idleTimeout` (10 minutes by default) is how long a free pod above the floor lives before the reconciler removes it.
- When the pool is at `max`, requests queue for `allocation.queueTimeout` (2 minutes by default). Callers can ask for a longer wait, capped by `allocation.maxQueueTimeout` (10 minutes by default, up to 1 hour). A request that waits too long fails with `pool_exhausted`. Set it to `0s` to fail at once.
- An allocation lasts `defaultTTL` (1 hour) and never more than `maxTTL` (24 hours). Releasing early frees the pod immediately.

Check the state with `kubectl get bkp -A`. The Warm, Allocated and Waiting columns show pods and queued requests.

## Upgrades

- Upgrade with `helm upgrade`. The Deployment rolls one replica at a time and the PodDisruptionBudget keeps one replica up. A leaving replica keeps accepting for `shutdown.delay` (5 s), closes its listeners, and lets open edge connections run for up to `shutdown.drainTimeout` (300 s). Running builds on other replicas are unaffected, and a build on a draining replica has up to that long to finish.
- Helm installs the CRD on first install only and never upgrades it. When a release changes `crds/`, apply it yourself first with `kubectl apply --server-side -f helm/buildkit-controller/crds/`.
- A change to a pool's template changes its pod template hash. The reconciler retires free pods with the old hash and starts new ones. Allocated pods run to the end of their allocation.
- Since the run-bound owner change, an OIDC allocation's owner includes the token's run claims (see [Allocation owner](architecture.md#allocation-owner)). An allocation a GitHub Actions run made before the upgrade has the old owner `oidc:<iss>#<sub>`, so a release from that run after the upgrade is refused with 403 (the Action's post step logs a warning and the job still passes) and the worker is freed at the allocation's expiry instead. Accepting the old form would let every run with the same `sub` release those workers, which is the hole the change closes. To avoid holding workers, upgrade while few builds are running.
- A config change restarts the controller. No pool state is lost, because state lives in pods and pool status.

## CA rotation

The controller creates its CAs on first start and stores them in the Secret `buildkit-controller-ca` in the release namespace. They are valid for 20 years. From 365 days before either CA expires, every pool shows `CAExpiringSoon=True`. Rotate during a quiet period.

To rotate, delete the Secret.

```sh
kubectl -n buildkit-system delete secret buildkit-controller-ca
```

The leader creates a new one on its next pass. If the Secret is ever invalid (corrupted, expired, not made by the controller), the leader first copies it to `buildkit-controller-ca-replaced-<timestamp>` and then replaces it. The controller never deletes those copies.

What breaks:

- Every allocation in flight. Its pod was issued under the old internal CA, so the leader retires it and the edge closes its connections. Those builds fail and the job must retry.
- Clients that saved a copy of the CA bundle ConfigMap for a self-issued serving certificate must fetch the new one. Clients that trust a public CA are unaffected.
- `POST /v1/allocations` returns 503 `ca_unavailable` until the new CA loads on each replica.

Several things recover on their own. Replicas reload the CA within the watch latency, the reconciler reissues worker Secrets, the reconciler retires free pods issued under the old CA and starts new ones, and new requests queue until the pool's `status.workerTLS.caFingerprint` matches. Expect one reconcile pass plus one pod start. While old pods remain, pools show `Degraded=True/CAReplaced`.

Worker, edge and serving certificates renew without any action.

## Observability

Metrics are on the ops port (8080, `/metrics`) through the `<fullname>-metrics` Service (`buildkit-controller-metrics` with the default naming). Set `metrics.serviceMonitor.enabled` to `true`, or `auto` to create it only when the Prometheus Operator CRD exists. All names start with `buildkit_`, and no metric has a per-allocation or per-pod label. The table in Observability below lists the ones to watch. Logs are JSON on stdout and never contain tokens, private keys or Secret data.

Metrics to watch:

| Metric | Use |
|---|---|
| `buildkit_allocations_total{result}` | Rate by result. A rise in `pool_exhausted`, `start_timeout` or `ca_unavailable` needs action. |
| `buildkit_allocation_duration_seconds{path}` | Warm and cold latency. |
| `buildkit_queue_depth`, `buildkit_queue_wait_seconds` | Demand above `scaling.max`. |
| `buildkit_pool_pods{state}`, `buildkit_pool_desired_warm` | Pool state (leader only). |
| `buildkit_edge_connections_total{result}` | Edge failures such as `tls_error`, `expired` and `upstream_error`. |
| `buildkit_leader` | Sum across replicas must be 1. |
| `buildkit_ca_valid`, `buildkit_cert_not_after_seconds{kind}` | CA and certificate health. |
| `buildkit_auth_total{result}`, `buildkit_jwks_fetch_total{result}` | Login failures and issuer reachability. |

Suggested alerts, with thresholds to tune:

| Alert | Expression | For |
|---|---|---|
| No leader | `sum(buildkit_leader) == 0` | 2m |
| CA unusable | `min(buildkit_ca_valid) == 0` | 2m |
| CA expiring | `min(buildkit_cert_not_after_seconds{kind=~"client_ca|internal_ca"}) - time() < 365 * 86400` | 1h |
| Pool saturated | `buildkit_queue_depth > 0` | 10m |
| Allocation failures | `sum(rate(buildkit_allocations_total{result!="ok"}[5m])) / sum(rate(buildkit_allocations_total[5m])) > 0.05` | 10m |
| JWKS fetch failing | `increase(buildkit_jwks_fetch_total{result="error"}[10m]) > 5` | 10m |

Health probes are `/healthz` (process alive) and `/readyz` (config loaded, caches synced, CA loaded, serving certificate loaded, listeners bound). Leadership is not part of readiness.

## Troubleshooting

Start with the pool conditions.

```sh
kubectl get bkp -n ci <pool> -o jsonpath='{range .status.conditions[*]}{.type}={.status}/{.reason}: {.message}{"\n"}{end}'
```

| Condition and reason | Cause | Fix |
|---|---|---|
| `Ready=False/InvalidSpec` | The controller rejected the spec, for example an unknown `scaling.timeZone`. | Fix the field named in the message. |
| `Ready=False/TemplateConflict` | The template sets a key the controller owns, such as a reserved label, volume name, mount path or a CPU architecture that contradicts `platforms`. | Remove the fields named in the message. No pod is created while this is set. |
| `Ready=False/CAUnavailable` | No valid CA is loaded yet. | Check the leader's logs and the Secret. The leader repairs it by itself. |
| `Ready=False/WorkerTLSPending` | The worker Secret is not written, or was issued under another internal CA. | Wait one reconcile pass. If it persists, check the controller's RBAC and logs for write errors. |
| `Ready=False/SandboxPending` | The worker ServiceAccount or NetworkPolicy could not be applied. | Check the message and the pool namespace for admission webhooks that reject them. |
| `Degraded=True/PodCreateFailed` | Pod creation failed (quota, admission, API error). | `kubectl get events -n <ns>`, then fix quota or policy. The controller retries with backoff. |
| `Degraded=True/PodStartTimeout` | A pod was not Ready within `workers.startTimeout`. | Check scheduling, image pull and node resources. Raise `startTimeout` for slow image pulls. |
| `Degraded=True/PodFailed` | A pod ended in phase `Failed` or `Succeeded`. | Read the pod's logs. |
| `Degraded=True/UnknownIssuer` | An `oidc` rule names an issuer that is not in `auth.oidc`. | Match `issuer` to the Helm `auth.oidc[].url` byte for byte, or add the issuer. The rule never matches until you do. |
| `Degraded=True/SlotTaken` | A pod named `<pool>-<n>` exists that the pool does not own. | Delete or rename it. |
| `Degraded=True/CAReplaced` | The CA was replaced and old pods remain. | Clears when the old pods retire. |
| `CAExpiringSoon=True` | A CA expires within a year. | Rotate as described above. |

Errors the API returns to clients, and the usual cause:

- 401 `unauthenticated`. The token is invalid, expired, has the wrong audience, or comes from an issuer that is not configured. Check `buildkit_auth_total`. The refusal log is sampled at 10 lines a second, so count with the metric, not the log.
- 503 `upstream_error` with `Retry-After: 2` on authentication. A ServiceAccount token could not be reviewed: the API server timed out (`kube.requestTimeout`), throttled the controller (429), or `auth.serviceAccount.maxConcurrentReviews` reviews were already in flight. A sustained rate usually means a flood of SA-shaped tokens. Look at `buildkit_auth_total{method="serviceaccount",result="error"}`.
- 404 `pool_not_found`. The pool does not exist, or no access rule matches the caller. The API never answers 403 for this, so pools cannot be probed.
- 503 `pool_exhausted`. The pool is at `scaling.max` and the queue wait ran out. Raise `max`, or `queueTimeout`.
- 503 `ca_unavailable`. The CA is being replaced. Retry shortly.

A client that cannot connect to the edge usually has a certificate problem. Check `buildkit_edge_connections_total` for `tls_error`, `bad_san` and `expired`, and set `--set logLevel=debug` on the chart to see handshake failures. The curl recipe can use `API_CA` to trust a self-issued certificate.

## Multi-pool builders and allocation domain

A builder that combines several pools, which is what the GitHub Action does with more than one `pools` entry, needs one edge address per allocation. Set `edge.allocationDomain` (for example `alloc.bk.example.com`) and create a wildcard DNS record `*.alloc.bk.example.com` that points at the edge address. Endpoints then become `tcp://<alloc-id>.alloc.bk.example.com:8444`. The pools must declare distinct `platforms`. With a single pool you can leave the domain empty.

The serving certificate does not need to cover those names. Clients always send `edge.serverName` as the TLS server name (the API returns it as the buildx `servername` driver option) and verify the certificate against that one name, so only DNS needs the wildcard.

## In-cluster claims

`BuildKitClaim` ([architecture.md](architecture.md#buildkitclaim)) is off by default. Set
`claims.enabled: true` in the chart to install the claim CRD, start the claim
controller on the leader and grant it `buildkitclaims` read and status write.
Also set `claims.namespaces` to the namespaces whose claims it serves; the
chart fails without it. Secret `get`, `create` and `delete` are granted by a
Role in each listed namespace only, and a claim from any other namespace fails
with `NamespaceNotServed`. With `watchNamespaces` set, the claim read and status rights are
Roles in the listed namespaces as well, and a claim in any other namespace is
ignored: it gets no status, not `NamespaceNotServed`. This also applies to the
deprecated `watchNamespace`, so an install that set it and relied on
`NamespaceNotServed` no longer sees that status.

A pool admits claims only through an access rule naming their namespaces:

```yaml
access:
  rules:
  - claims: {namespaces: [ci]}
```

Who in that namespace may create claims is ordinary RBAC on
`buildkitclaims`. Cap the number of claims with a ResourceQuota:

```yaml
apiVersion: v1
kind: ResourceQuota
metadata: {name: claims, namespace: ci}
spec:
  hard: {count/buildkitclaims.buildkit.smrt-devops.net: "10"}
```

`kubectl get bkc -A` shows each claim's phase. The `Bound` condition's reason
says why a claim waits or failed:

| Reason | Meaning | Action |
|---|---|---|
| `Queued` | No free worker; the claim waits in the pool queue and retries with backoff | Raise `scaling.max` or wait |
| `PoolNotReady`, `CAUnavailable`, `StartTimeout`, `PodLost`, `Error` | Transient; retried | Check the pool's conditions and controller logs |
| `PoolNotFound` | Pool missing or no `claims` rule names this namespace | Fix `poolRef` or the pool's access rules, then recreate the claim |
| `InvalidRequest` | `ttl` above the pool's `maxTTL` | Recreate with a shorter `ttl` |
| `NamespaceNotServed` | The claim's namespace is not in `claims.namespaces` | Add the namespace to `claims.namespaces` and upgrade, then recreate the claim |
| `SecretConflict` | A Secret `<claim>-buildkit` exists and the claim does not own it | Rename the claim or remove the Secret |
| `Expired` | The TTL ran out and the Secret was deleted | Delete the claim and create a new one |
| `WorkerLost` | The worker pod was deleted, evicted or failed before expiry; the Secret was deleted. The controller does not rebind, because the client holds a certificate for a pod that is gone | Delete the claim and create a new one |

Metrics: `buildkit_claims{phase}` and `buildkit_claim_reconciles_total{result}`.
Allocations for claims also count in `buildkit_allocations_total`.

A deleted claim's worker is released within a few seconds by a sweep on the
leader; if the controller is down at the time, the next leader releases it, and
the TTL bounds the leak in any case. Turning the feature off keeps the claim CRD,
which carries `helm.sh/resource-policy: keep`, and so keeps every claim and
claim Secret. Nothing reconciles them, and workers expire at their TTL. Delete
the claims, or the CRD itself, to remove them.
