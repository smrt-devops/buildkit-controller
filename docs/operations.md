# Operations

## Configuration

The chart renders one YAML file from `values.yaml` into a ConfigMap, and the controller reads it. Unknown keys and invalid values stop startup with exit code 2 and a list of every error. There is no live reload. Helm rolls the Deployment when the file changes. Every key is documented in [values.yaml](../helm/buildkit-controller/values.yaml) and checked by `values.schema.json`, the chart is described in the [Helm README](../helm/buildkit-controller/README.md), and the pool fields are in [architecture.md](architecture.md#two-crds).

Startup validation also refuses:

- an `edge.handshakeTimeout` or `edge.upstreamConnectTimeout` of zero;
- a `leaderElection.leaseName` that is not a lowercase DNS-1123 subdomain;
- a `claims.namespaces` entry that is not a namespace name or is listed twice;
- the controller's own namespace in `watchNamespaces` or `claims.namespaces`;
- an `auth.oidc[].url` that contains `?` or `#`. The OIDC URL is compared with the token's `iss` as an exact string, and discovery appends a path to it;
- a `proxyProtocol.trustedCIDRs` entry shorter than /8 for IPv4 or /32 for IPv6;
- a `workerNetwork.dns.cidrs` entry that holds a whole protected range (see [Worker egress](#worker-egress));
- a `leaderElection.leaseDuration` over 10 minutes, a `shutdown.delay` over 1 hour, a `shutdown.drainTimeout` over 48 hours, and any other duration over 366 days.

If `workerNetwork.clusterCIDRs` is empty, the controller starts and logs a warning, because it cannot tell the cluster's addresses from others (see [Worker egress](#worker-egress)).

## Namespace scope

Set `watchNamespaces` to the namespaces that hold pools, for example `watchNamespaces: [ci-builders, team-b-builders]`. The chart then renders a Role and RoleBinding in each listed namespace, and the controller watches pools and worker pods in those namespaces only. A pool in another namespace gets no status and no pods. A pool in the controller's own namespace reports `Ready=False/InvalidSpec` and gets no pods, because its workers could mount the CA Secret. The only cluster-wide grant left is TokenReview, and the release namespace keeps its own Role for the Lease, the CA Secret, Events and the CA bundle ConfigMap. To add a pool namespace, add it to the list and run `helm upgrade`; the controller restarts and picks it up.

The workload Role grants:

- pods: get, list, watch, create, patch and delete;
- Secrets: create only (a worker Secret is owned by its pod, and the garbage collector deletes it);
- ServiceAccounts and NetworkPolicies: create and patch;
- PVCs: list and delete, only for the one-time sweep of 0.1.0 cache volumes;
- `buildkitpools`: get, list and watch, and patch on their status.

The default, `[]`, grants the same through a ClusterRole in every namespace, kube-system included, so anyone who takes over the controller controls the cluster. The cache registry's ServiceAccount then reads every pod in the cluster, including inline environment values, and the registry parses uploads from builds. `helm install` prints a warning in that mode.

## Worker egress

A pool's `spec.network.egress.allowCIDRs` opens ranges such as a private registry or proxy to its workers. The controller refuses an entry that overlaps a protected range, any range in `workerNetwork.clusterCIDRs` or `workerNetwork.extraDeniedCIDRs`, or is 0.0.0.0/0 or ::/0. The protected ranges are:

- link-local, 169.254.0.0/16 and fe80::/10, which holds the IPv4 metadata endpoint of AWS, GCE, Azure and Oracle;
- loopback, 127.0.0.0/8 and ::1;
- the Azure wire server, 168.63.129.16;
- metadata endpoints outside link-local: Alibaba 100.100.100.200 and the IPv6 endpoints of AWS (`fd00:ec2::254`), GCE (`fd20:ce::254`), Akamai (`fd00:a9fe:a9fe::1`), Oracle (`fd00:c1::a9fe:a9fe`) and Scaleway (`fd00:42::42`);
- 64:ff9b:1::/48, the local-use NAT64 prefix.

Every pool's NetworkPolicy denies the protected ranges other than loopback, whatever `workerNetwork.deniedCIDRs` says. The list does not cover every provider's metadata endpoint. If your cloud serves metadata from another address, add it to `workerNetwork.extraDeniedCIDRs`. To deny more, add to `extraDeniedCIDRs` rather than replacing `deniedCIDRs`, because Helm replaces a list you set.

Overlap means either range holds the other. An IPv4-mapped IPv6 entry (`::ffff:a.b.c.d`) and an entry in the NAT64 well-known prefix 64:ff9b::/96 count as the IPv4 range they reach.

When `workerNetwork.clusterCIDRs` holds no range of an entry's family, the controller cannot tell cluster addresses from others. It then also refuses an IPv6 entry, and an IPv4 entry that overlaps `workerNetwork.deniedCIDRs` (the private ranges by default). List the cluster's ranges in `clusterCIDRs`, or permit the entry in `permittedCIDRs`.

A refused entry makes the pool report `Ready=False/InvalidSpec` naming the entry. The pool gets no new pods or allocations. The controller still applies its NetworkPolicy, without the refused entries, so a policy that an older spec or config opened wider is narrowed. NetworkPolicy applies to running pods at once: warm pods and running builds lose the refused ranges, and the builds keep running. The same happens when an operator adds a range to `clusterCIDRs` or `extraDeniedCIDRs`, or removes one from `permittedCIDRs`, once the controller restarts with the new config. The controller applies its NetworkPolicy again before every pass that creates pods and every 5 minutes, so a new pod never starts under a policy that someone deleted or edited, and running pods get it back within 5 minutes. The worker ServiceAccount comes back the same way, and the CA bundle ConfigMap every 5 minutes.

List the pod, service and node ranges of the cluster in `workerNetwork.clusterCIDRs` so no pool can open them. To let pools open one of these ranges anyway, for example a NodeLocal DNSCache address or an in-cluster proxy's service IP, list it in `workerNetwork.permittedCIDRs`. An entry passes only when one permitted range holds all of it, so permit the narrowest range that works. `permittedCIDRs` never permits 0.0.0.0/0 or ::/0, and the controller refuses to start with either in the list.

### Who may write a pool

A pool author is as trusted as anyone who can create pods in the pool's namespace. The pod template reaches past the controller in three ways:

- Template labels are copied onto the worker pods, and NetworkPolicies add up. A label that matches another policy in the namespace grants that policy's egress to the workers, whatever `allowCIDRs` says.
- A `secret` volume or `secretKeyRef` in the template can name any Secret in the namespace. BuildKit runs without a process sandbox (`--oci-worker-no-process-sandbox`), so a build step can read buildkitd's environment and files, including that Secret, through `/proc`.
- Template annotations are copied onto the worker pods. The controller refuses the keys that Multus, Calico, Cilium, OVN, Kata and CRI-O read to change a pod's network or runtime (`TemplateConflict`), but an admission webhook or another component may act on other annotations.

Give each pool its own namespace, with no other workloads, Secrets or NetworkPolicies in it, and grant create and update on `buildkitpools` only to people you would trust with pods there.

## Running untrusted builds

A build step is code that the caller chose, and it runs in the worker pod. The controller starts rootless buildkitd as user 1000, but with the seccomp and AppArmor profiles set to `Unconfined` and with `--oci-worker-no-process-sandbox`, which rootless BuildKit needs here. So every `RUN` step reaches the node's kernel through an unfiltered system call interface, and build steps share buildkitd's process namespace. On a node that also runs other workloads, a kernel bug in that interface is a way out of the pod. The NetworkPolicy limits what a build can reach over the network. It does not limit the kernel. The worker container drops `NET_RAW` and keeps the other default capabilities, which build steps need inside buildkitd's user namespace. A pod has no process limit of its own, so set the kubelet's `podPidsLimit` on the nodes that run workers to stop a fork bomb.

Pull requests from forks are the usual case of untrusted code. The build cache already keeps them from poisoning it: a build the pool does not trust (a `pull_request` event by default, see [Cache trust](#cache-trust)) reads its repository's cache and never writes it. That protects the cache, not the node. For the node, do one or both of the following:

- Run the workers on nodes that run nothing else. Taint a node group, then select and tolerate it in the pool template (`spec.template.spec.nodeSelector` and `tolerations`, as in [pool-template.yaml](../examples/pool-template.yaml)). Keep the pool for untrusted builds separate from the pool for trusted ones, and admit fork pull requests only to the former through its access rules.
- Set `spec.template.spec.runtimeClassName` in the pool to a sandboxed runtime class, such as gVisor or Kata. The controller copies the field onto the worker pods and changes nothing else for it. It does not check that the runtime supports rootless BuildKit with these settings, so run a representative build on the class before you rely on it.

Either way, give an untrusted pool no registry credentials, no cache write access (leave out `spec.workers.cache.writers` entries that match it) and no secrets that a trusted build also uses, and keep it in a namespace of its own (see [Who may write a pool](#who-may-write-a-pool)).

## Exposure

By default the chart exposes the controller through a Gateway API Gateway that you run, and its Service is ClusterIP. The API gets an HTTPRoute, and the Gateway re-encrypts to the controller under a BackendTLSPolicy that trusts the controller's CA bundle ConfigMap. The edge gets a TLSRoute in TLS passthrough, because the controller authenticates each build by its client certificate. Set `gateway.api.parentRefs` and `gateway.edge.parentRefs` to the Gateway's HTTPS and TLS passthrough listeners, each with a `sectionName`. Without one, the API route also attaches to any plain HTTP listener that admits it, which would carry bearer tokens in cleartext. The routes need the Gateway API v1.5.0 or later standard-channel CRDs. `gateway.api.mode: passthrough` turns the API route into a TLSRoute as well. Where no implementation with TLS passthrough is available, such as GKE's managed Gateway or the AWS Load Balancer Controller, set `service.type: LoadBalancer` instead. [the chart README](../helm/buildkit-controller/README.md#exposure) has the modes, an Envoy Gateway example and the implementations that support them.

Timeouts on the path need no tuning for the queue. A queued allocation is a claim, and no API request stays open longer than 25 seconds (`POST /v1/allocations` answers within about 20 seconds, `GET /v1/allocations/{id}?wait=25s` within 25), so the HTTPRoute's `timeouts.request` and `timeouts.backendRequest` (60 seconds by default, `gateway.api.timeouts`) and every common gateway and load-balancer idle timeout are above it. Only builds matter: a build can be quiet for a long step on the edge connection, which TCP keepalive covers on most paths (see the idle-timeout table in [the chart README](../helm/buildkit-controller/README.md#load-balancer-idle-timeout)).

Behind a gateway, the controller sees the gateway's addresses instead of the clients', and the logs show the gateway's pod addresses. The edge's handshake cap per address evicts the oldest handshake of that address, so behind a gateway one client could evict the others. The chart therefore sets `edge.maxHandshakesPerIp` to 0 when the edge route is on a Gateway and `edge.proxyProtocol` is off. The API caps (128 connections and 4 TokenReviews in flight per address) apply to every client together. Either turn on PROXY protocol (below) on both the gateway (`gateway.envoy.proxyProtocol`) and the controller (`api.proxyProtocol`, `edge.proxyProtocol`) so the caps count clients again, or raise `api.maxConnectionsPerIp` and `auth.serviceAccount.maxReviewsPerIp` to the number of jobs that start at once, or set them to 0 and limit clients at the gateway.

### PROXY protocol

With PROXY protocol the gateway opens each backend connection with a short binary header that names the client's address. The controller reads version 2 (the binary form; v1 text headers are refused) on each listener where it is on, and uses the client's address for the per-address caps, the TokenReview cap and the `peer` log field. The gateway's own address is logged as `via`.

Turn it on per listener, together with the gateway side:

```yaml
api:
  proxyProtocol: {enabled: true, trustedCIDRs: [10.244.3.17/32, 10.244.5.9/32]}   # the Envoy pods
edge:
  proxyProtocol: {enabled: true, trustedCIDRs: [10.244.3.17/32, 10.244.5.9/32]}   # the Envoy pods
gateway:
  envoy: {enabled: true, proxyProtocol: true}   # Envoy Gateway: BackendTrafficPolicy proxyProtocol V2
```

`gateway.envoy.proxyProtocol` asks Envoy to send the header to each listener whose `proxyProtocol` is on. On another implementation, configure its PROXY protocol v2 option for the controller's Service, for example through `gateway.extraManifests`. `helm install` warns when the chart asks Envoy to send PROXY protocol to a listener that does not read it, or the reverse.

The trust model:

- Only connections from `trustedCIDRs` are parsed. List the gateway's source addresses: the Envoy pods' addresses, or a node or subnet range that only the gateway uses. Never a range that clients or other pods share, because any host in it could then claim any address. Pods in a trusted range that dial the Service directly, without a header, are refused. `0.0.0.0/0` and `::/0` are refused.
- A connection from a trusted address must open with a valid header within `headerTimeout` (5 s), or it is closed. There is no fallback to the gateway's address. The header may be at most 1040 bytes (1024 of addresses and TLVs).
- A connection from any other address is served as itself and never parsed. A client that sends its own header from outside the trusted range only breaks its own TLS handshake.
- A `LOCAL` header (a gateway health check) is accepted and the connection counts as the gateway's address. So is a `PROXY` header with an unspecified address family (`AF_UNSPEC`), which some load balancers send for their own probes. Both count as `local`.

Each header from a trusted source is counted in `buildkit_api_proxy_protocol_headers_total` or `buildkit_edge_proxy_protocol_headers_total` by `result`: `proxied`, `local`, or why the connection was closed (`timeout`, `truncated`, `bad_signature`, `bad_version`, `bad_command`, `oversized`, `short`, `unsupported_family`, `io_error`). `unsupported_family` means an address family other than TCP over IPv4 or IPv6, for example UNIX or UDP. A rising `bad_signature` or `timeout` count usually means the gateway does not send the header, or something else connects from a trusted range. The refused connections are logged at `debug`.

Pod addresses change when the Envoy pods are rescheduled. Pin them to a node pool or subnet and trust that range, or keep the list in sync. Without PROXY protocol, or when the gateway does not send it, everything works as before, and the caps count the gateway's addresses.

## Sizing

- Pool size. Set `scaling.max` to the number of builds you want to run at once. Each worker pod requests 500m CPU, 1Gi memory and 1Gi ephemeral storage by default, with limits of 4Gi memory and 20Gi ephemeral storage. Override these in `spec.template.spec.containers[0].resources`. Limits must include `memory` and `ephemeral-storage`.
- Worker storage. A worker has no volume. Its BuildKit state is an `emptyDir` that disappears with the pod, and the ephemeral-storage limit bounds it. `spec.workers.stateSizeLimit` caps the `emptyDir` explicitly.
- Cache volume. The cache registry keeps its store on one PVC (`cache.persistence`, 60Gi by default, the cluster's default StorageClass unless `cache.persistence.storageClass` is set). The volume must hold `cache.maxSize` (40Gi) plus `cache.maxUploadSize` (10Gi): blobs of uploads in progress sit on the volume beside the stored cache until their upload ends, and garbage collection only enforces `maxSize` on what is stored. A volume smaller than the sum can fill during a burst of pushes, and then writes fail and builds run uncached. When you raise `cache.maxSize`, raise `cache.persistence.size` with it. `cache.poolMaxSize` (10Gi) caps each pool that sets no `spec.workers.cache.maxSize`, and the pools' budgets together can exceed `cache.maxSize`, in which case the volume budget decides. Resize the claim with your storage class's volume expansion, or recreate it (the cache restarts empty). See [Build cache](architecture.md#build-cache).
- Controller. The chart defaults are 2 replicas, a 100m CPU and 128Mi memory request, and a 512Mi memory limit. The edge copies bytes for every build, so watch CPU on the replicas under heavy load and raise `replicaCount` or enable `autoscaling` (CPU-based HPA). Each proxied connection holds about 200 KiB (two 64 KiB copy buffers and two TLS sessions) and an idle replica about 14 MiB, so the 512Mi limit holds about 2300 connections, and 2000 is sized to it. Raise `resources.limits.memory` and `edge.maxConnections` together. One allocation may hold at most 64 edge connections per replica; a plain build uses 2. Per replica, the edge allows 2000 established connections (`edge.maxConnections`), a quarter of them for the allocations of one pool namespace, and 1024 connections in the TLS handshake (`edge.maxHandshakes`), 32 of them from one client address (`edge.maxHandshakesPerIp`). Over a handshake cap the edge closes the oldest handshake in progress, not the new connection, and it closes a connection that sends no TLS ClientHello within 2 s. The edge accepts TLS 1.3 only. The API allows 4096 connections (`api.maxConnections`), 128 of them from one client address (`api.maxConnectionsPerIp`). An IPv6 client address is its /64 on the edge and its /56 on the API. Other connections over a cap are closed on accept and logged at `debug`. ServiceAccount tokens that are not cached cost a TokenReview: at most 50 a second and `auth.serviceAccount.maxConcurrentReviews` (32) in flight per replica, 4 of them for one client address. The per-address caps count the source address the controller sees, so they need source-IP preservation. With `service.type: LoadBalancer` the chart sets `service.externalTrafficPolicy: Local` for that. Behind a Gateway without [PROXY protocol](#proxy-protocol), with `Cluster`, a load balancer that rewrites the source, a proxy, or runners behind one NAT address, many clients share one address. Turn on PROXY protocol where the proxy supports it, or raise `api.maxConnectionsPerIp`, `edge.maxHandshakesPerIp` and `auth.serviceAccount.maxReviewsPerIp`, or set them to 0 to disable them. Behind a Gateway the chart already sets `edge.maxHandshakesPerIp` to 0 (see [Exposure](#exposure)).
- Load balancer and gateway. Queued requests are short polls, so only long quiet builds need the idle timeout of the load balancer or gateway raised. See [Exposure](#exposure) and the table in [the chart README](../helm/buildkit-controller/README.md#load-balancer-idle-timeout).
- Quotas. Pods count against the pool namespace's ResourceQuota. A refused create shows as `Degraded=True/PodCreateFailed`.

## Scaling

The reconciler keeps `max(effective min, pending claims)` pods warm, capped at `scaling.max`. The pending claims are the pool's queue.

- `scaling.min` is the floor. The default is 0, so an idle pool costs nothing and the first build pays a pod start.
- `scaling.schedule` raises the floor in time windows (days, `HH:MM` start and end, and a `min`). The effective floor is the larger of `scaling.min` and every open window. `scaling.timeZone` is an IANA zone and defaults to UTC.
- `scaling.idleTimeout` (10 minutes by default) is how long a free pod above the floor lives before the reconciler removes it.
- When the pool is at `max`, requests wait in the pool's queue for `allocation.queueTimeout` (2 minutes by default), in the order they arrived. Callers can ask for a longer wait, capped by `allocation.maxQueueTimeout` (10 minutes by default, up to 1 hour). A claim that is still pending then fails with `QueueTimeout`, and the client gets `pool_exhausted`. Set it to `0s` to fail at once. The wait is not one open request: the client polls (see [Using it from CI](ci/README.md#how-an-allocation-completes)).
- An allocation lasts `defaultTTL` (1 hour) and never more than `maxTTL` (24 hours). The time counts from the moment the request wins a worker, after any queue wait, so a cold pod start comes out of the TTL. Releasing early frees the pod immediately.
- Fair share: while a claim of another owner waits for a pool, an owner (an OIDC run, a ServiceAccount or pod, a claim) that holds half of `scaling.max` workers, at least 1, binds no more. A worker that frees goes to the other owner first, even when this owner's claim is older. With nobody else waiting, one owner may use the whole pool, so a single tenant or one wide CI matrix is not capped. Fair share never takes back a bound worker: a claim frees its worker only at release or expiry.
Check the state with `kubectl get bkp -A`. The Warm, Allocated and Waiting columns show pods and queued requests. `kubectl get bkc -A` lists the queue itself: each claim with its pool and phase, and the recently finished ones. Add `--sort-by=.metadata.creationTimestamp` to see the queue order.

## Upgrades

- Upgrade with `helm upgrade`. The Deployment rolls one replica at a time and the PodDisruptionBudget keeps one replica up. Running builds survive it: see [Stopping a replica](#stopping-a-replica).
- Helm installs both CRDs (`crds/`) on first install only and never upgrades them. When a release changes `crds/`, apply them yourself before `helm upgrade` with `kubectl apply --server-side --force-conflicts -f helm/buildkit-controller/crds/`. `--force-conflicts` takes over the fields that Helm wrote at install. `helm upgrade` fails while a CRD in the cluster is older than the chart.
- Install the chart from the release, or from a checkout of the release tag. The chart's `appVersion` picks the image, so a checkout of another commit can run an image that does not match its config.
- A change to a pool's template changes its pod template hash. The reconciler retires free pods with the old hash and starts new ones. Allocated pods run to the end of their allocation.
- A config change restarts the controller. No pool state is lost, because state lives in pods and pool status.

### From 0.1.0 to 0.2.0

0.2.0 replaces the per-replica queues with the claim queue and replaces both cache layers with the built-in cache registry. The upgrade removes things, so go in this order.

1. Update the clients first, then the controller, and keep the window short. The 0.2.0 Action and `buildkit-allocate.sh` follow a `202` by polling. Against a controller that does not report `queueTimeoutSeconds` in `GET /v1/pools`, which is v0.1.0, their `POST` waits for the whole remaining budget (the queue timeout plus 60 seconds), because v0.1.0 holds the `POST` open through the queue wait and the pod start. So a slow allocation still completes. They also repeat a `POST` whose answer was lost. v0.1.0 does not name the allocation after the key, so such a repeat makes a second allocation, which ends at its TTL. A client from before 0.2.0 against a 0.2.0 controller does not understand a `202`: it fails its step when an allocation queues for more than about 20 seconds, and the claim it created stays queued or bound until its TTL, because the client never learned the id. Warm-pod allocations answer `201` in one round trip for both, so old clients work until the first queued request. Set a short `queue-timeout` on old clients during the window if you cannot update them first.
2. Edit your pools. The `local` and `registry` blocks under `spec.workers.cache` no longer exist. Delete them. What remains under `spec.workers.cache` is `scope`, `writers` and `maxSize`. `sizeLimit` moved to `spec.workers.stateSizeLimit`, because it sizes the buildkitd state, not the cache. The new CRD does not know the old fields. The API server prunes them, with a warning, from an apply that still has them, and kubectl's strict field validation (recent versions) refuses the apply.
3. Move any pool in the controller's namespace to a namespace of its own. The controller refuses to start when `watchNamespaces` lists its own namespace, and otherwise does not watch pools there, so such a pool gets no pods and no status. Then check your other pools against the new refusals. Each refused pool reports `Ready=False` and gets no new pods, so find them before the upgrade with `kubectl get bkp -A -o yaml`:
   - an `allowCIDRs` entry that overlaps a protected, cluster or extra denied range (`InvalidSpec`), and, while `workerNetwork.clusterCIDRs` holds no range of its family, an IPv6 entry or a private IPv4 entry. Narrow the entry, list the cluster's ranges in `clusterCIDRs`, or permit it in `workerNetwork.permittedCIDRs` (see [Worker egress](#worker-egress)). The NetworkPolicy loses refused ranges at once, also for builds already running;
   - a template annotation that Multus, Calico, Cilium, OVN, Kata or CRI-O reads (`TemplateConflict`): remove it.
4. Edit your Helm values. `cacheDefaults` and the `cache` key of a `pools` item are gone, and the chart rejects them. The new top-level `cache` block configures the registry (see [Sizing](#sizing)). `claims.enabled` is gone: a non-empty `claims.namespaces` turns on in-cluster claims, so delete the flag. `watchNamespace` is gone: write `watchNamespaces: [<namespace>]` instead. Neither `watchNamespaces` nor `claims.namespaces` may hold the release namespace.
5. Apply both CRDs yourself, because Helm does not upgrade `crds/`: `kubectl apply --server-side --force-conflicts -f helm/buildkit-controller/crds/`. The claim CRD now lives in `crds/` as well. If v0.1.0 installed it as a chart template (`claims.enabled: true`), its `helm.sh/resource-policy: keep` annotation stops `helm upgrade` from deleting it, and every claim with it.
6. Run `helm upgrade`. The chart now installs a `<fullname>-registry` StatefulSet, Service, NetworkPolicy and ServiceAccount, and a 60Gi PVC for the cache. Check that the cluster has a default StorageClass, or set `cache.persistence.storageClass`. Set `cache.enabled: false` to run without a cache.

What happens next:

- The pool template hash changes (workers lose their volumes, drop `NET_RAW`, gain the registry entry in `buildkitd.toml` and new probe timings). The reconciler retires each pool's free pods once and starts new ones. Allocated pods run to the end of their allocation.
- The leader deletes the cache volumes of v0.1.0 once, at the start of its first term. It lists PVCs labelled `app.kubernetes.io/name=buildkit-cache` and `app.kubernetes.io/managed-by=buildkit-controller` in the watched namespaces, deletes each, and logs `deleted a cache volume of v0.1.0` with the namespace and name. It keeps a labelled PVC that a controller other than a `BuildKitPool` holds, such as a StatefulSet. If a list or delete fails, it tries again on the next term. The controller's RBAC on `persistentvolumeclaims` is only `list` and `delete` for this, and the sweep and that grant are removed in the release after 0.2.0. Those caches were per slot and cannot move into the registry, so the first builds after the upgrade run cold. If your StorageClass has `reclaimPolicy: Retain`, the PersistentVolumes stay behind; delete them yourself.
- Allocations in flight keep working. An allocation that v0.1.0 made through the API has no claim. Releasing it with `DELETE /v1/allocations/{id}` still retires its pod when the caller owns it, and otherwise it ends at its TTL. A v0.1.0 in-cluster claim's worker is retired by the leader's tidy pass once the claim is gone or ended.
- Owners changed (see [Allocation owner](architecture.md#allocation-owner)). An OIDC owner now includes the token's run claims, `%` and `#` in the issuer and `sub` are percent-encoded, and a ServiceAccount owner includes the pod of a pod-bound token. A release of an allocation made before the upgrade then answers `404`, which the Action and the script count as released, and the worker is freed at the allocation's expiry. To avoid holding workers, upgrade while few builds are running.
- The cache writers changed. Without `spec.workers.cache.writers`, a GitHub Actions build writes only from a protected ref, and `merge_group` builds no longer write. A pool whose builds should keep writing from other refs needs `writers` rules (see [Cache trust](#cache-trust)).
- Metrics changed. See the list under [Observability](#observability) and the [changelog](../CHANGELOG.md).

## Stopping a replica

A replica that gets SIGTERM, from a rollout, an HPA scale-in, a node drain or a pod delete, does not cut a running build:

1. `/readyz` fails, so the replica leaves the Service endpoints.
2. If it leads, it stops the reconciler and releases the Lease at once. Another replica leads within a few seconds, whatever the drain takes.
3. `shutdown.delay` (5 s) after the signal, the API stops accepting connections. Requests already in flight finish. A queued allocation is a claim in the API server, not state in the replica, so it stays queued. Long polls on this replica return `202` at once, and the client's next poll reaches another replica and keeps its place.
4. The edge stops accepting connections and keeps the open ones until each ends on its own: the build finishes and the client releases the allocation, the allocation expires, the worker pod goes away, or the client certificate's `notAfter` passes. The Pod watch, revocation and certificate timers keep running until the last connection closes. Then the process exits.

With `shutdown.drainTimeout` unset (the default), the drain has no fixed budget. It is bounded by the longest a connection can live: the 24h `maxTTL` cap plus the 5-minute certificate margin, 24h5m. The chart sets `terminationGracePeriodSeconds` to `shutdown.delay` + 86700 s + 10 s, so Kubernetes does not kill a draining replica early. Set `shutdown.drainTimeout` to cap the drain instead, and the grace period becomes `shutdown.delay` + `shutdown.drainTimeout` + 10 s. The delay and the drain budget both count from the signal, and the drain ends at the signal time + `shutdown.delay` + the budget. A draining replica logs `edge draining` with its open connection count every minute.

With a cap, the edge cuts the connections still open 30 s before the drain ends (half the remaining time when the drain has under a minute left) and counts them as `buildkit_edge_revocations_total{reason="drain"}`. The replica then keeps serving `/metrics` until the cap, so one scrape sees the count. A second SIGTERM or SIGINT while the replica is stopping ends the process at once, which cuts every open build.

A replica whose component fails exits non-zero, so the kubelet restarts it, and it releases the Lease first. When the API, the edge, the certificate loader or the ops server failed, it exits at once, with no delay and no drain, which cuts the builds open through it. When other work failed, such as the leader's, it drains for at most 5 minutes.

A Deployment rollout does not wait for old replicas to finish draining: new replicas take traffic as soon as they are Ready, and the old ones exit when their last build ends. The same holds for HPA scale-in, so `autoscaling` is safe to enable. A terminating replica is not Ready, so the PodDisruptionBudget counts it as down and lets the next eviction wait for its replacement.

Node drains are bounded by whatever evicts the pod:

- `kubectl drain` waits for evicted pods to exit. Its `--timeout` (none by default) caps the wait, and on expiry it stops and leaves the node cordoned; the pod keeps draining.
- cluster-autoscaler caps the grace period of pods it evicts on scale-down at `--max-graceful-termination-sec`, 600 s by default. A build that runs longer through a replica on that node is cut. Raise the flag, or keep the controller on nodes it does not scale down (annotate the pods `cluster-autoscaler.kubernetes.io/safe-to-evict: "false"` with `podAnnotations`).
- Karpenter honours the pod's `terminationGracePeriodSeconds` when it drains a node, up to the NodePool's `terminationGracePeriod` when that is set.

## CA rotation

The controller creates its CAs on first start and stores them in the Secret `buildkit-controller-ca` in the release namespace. They are valid for 20 years. From 365 days before either CA expires, every pool shows `CAExpiringSoon=True`. Rotate during a quiet period, and before the last 25 hours of the CA's life: from then on the leader creates no worker pods, because each would be retired at once for its short certificate.

To rotate, delete the Secret.

```sh
kubectl -n buildkit-system delete secret buildkit-controller-ca
```

The leader creates a new one on its next pass. If the Secret is ever invalid (corrupted, expired, not made by the controller), the leader first copies its certificates, not its private keys, to `buildkit-controller-ca-replaced-<timestamp>` and then replaces it. The controller never deletes those copies.

What breaks:

- Every allocation in flight. Its pod was issued under the old internal CA, so the leader retires it and the edge closes its connections. Those builds fail and the job must retry.
- Clients that saved a copy of the CA bundle ConfigMap for a self-issued serving certificate must fetch the new one. Clients that trust a public CA are unaffected.
- `POST /v1/allocations` returns 503 `ca_unavailable` until the new CA loads on each replica. The Action and the curl recipe repeat the `POST` with the same key, backing off up to 30 seconds, until their budget runs out.

Several things recover on their own. Replicas reload the CA within the watch latency. Each worker pod carries its own serving certificate and Secret, issued when the pod is created, so nothing is reissued in place. The leader retires every free pod issued under the old CA (`buildkit_releases_total{reason="ca_replaced"}` counts the allocated ones it retires) and starts new ones. A request never gets a pod from the old CA, so it takes a new pod, or waits in the queue until one is Ready. Expect one reconcile pass plus one pod start. While old pods remain, pools show `Degraded=True/CAReplaced`. No pool status field gates new requests.

Worker, edge and serving certificates renew without any action. Editing only the labels or annotations of the CA Secret reissues nothing; the controller reissues certificates when a CA certificate changes.

## Build cache

The chart runs the cache registry by default (`cache.enabled`). How it works is in [architecture.md](architecture.md#build-cache). This section is what an operator sets and watches.

| Value | Default | Meaning |
|---|---|---|
| `cache.enabled` | `true` | `false` removes the registry. Allocations then carry no cache flags and builds run uncached. |
| `cache.maxSize` | `40Gi` | Budget of the whole store. Over it, the least recently used manifests go first. |
| `cache.poolMaxSize` | `10Gi` | Budget of a pool that sets no `spec.workers.cache.maxSize`. |
| `cache.maxBlobSize` | `5Gi` | A larger layer is refused. |
| `cache.maxUploadSize` | `10Gi` | Bytes that uploads in progress may hold together. One worker may hold a quarter of it, or one `cache.maxBlobSize` if that is larger, and a worker gets `429` beyond that. |
| `cache.gcInterval` | `10m` | Time between garbage collections. |
| `cache.persistence.size`, `.storageClass` | `60Gi`, cluster default | The volume. |

Each value is also a flag of the `registry` role (`--max-size`, `--pool-max-size`, `--max-blob-size`, `--max-upload-size`, `--gc-interval`) with the environment variable `BUILDKIT_CACHE_MAX_SIZE` and so on, if you run the role yourself. The flags have the chart's defaults, except `--max-size`, which defaults to 50Gi, so set it to fit your volume.

The registry takes at most 64 connections from one address and gives each TLS handshake 5 s. It holds at most 16 manifest bodies in memory at once. A manifest push that finds no free slot gets `429` at once, and its body must arrive within 30 s. A trusted caller keeps its share of the upload space until its uploads end or expire after 1 hour idle, so one trusted tenant can make other tenants' exports fail with `429` for that long. Exports carry `ignore-error=true`, so those builds still succeed, uncached.
### Sizing the volume

Size `cache.persistence.size` to at least `cache.maxSize` plus `cache.maxUploadSize`. The defaults are 40Gi plus 10Gi, so the 60Gi volume has 10Gi to spare. Garbage collection holds the stored cache to `maxSize` every `gcInterval`, but between runs the store can grow past it, and uploads in progress add up to `maxUploadSize` more on top. A pool budget (`spec.workers.cache.maxSize`) cuts only that pool. Blobs of uploads that no manifest references yet count against both budgets. When the volume is full, pushes fail and pulls keep working.

### Cache trust

A pool decides which callers may write the cache with `spec.workers.cache.writers`, in the syntax of `access.rules`. By default a GitHub Actions build writes when its `event_name` is `push`, `schedule` or `workflow_dispatch` and its `ref_protected` claim is `true`, and every ServiceAccount and `BuildKitClaim` caller writes. A `merge_group` build does not write by default, because its queue ref is never protected. A caller whose token has no `event_name` claim, such as a GitLab job, reads only; list it in `writers` to let it write. `writers: []` makes the pool read-only for everyone.

Some builds never write, whatever `writers` says, because they can run code from forks or from unreviewed refs:

- GitHub Actions events `pull_request`, `pull_request_target`, `workflow_run`, `issue_comment`, `pull_request_review`, `pull_request_review_comment`, `deployment` and `deployment_status`;
- GitLab pipelines whose `pipeline_source` is `merge_request_event`, `external_pull_request_event` or `parent_pipeline`.

Every branch of a repository shares its cache, so pin `event_name` and `ref_protected: "true"` in every GitHub `writers` rule, and `ref_protected` and `pipeline_source` in every GitLab one. A rule that matches only the repository lets a build of any branch plant what the protected branch then reads. See [`examples/pool-cache.yaml`](../examples/pool-cache.yaml).

The rights come from the worker pod's live allocation, so they end with it, and a build cannot read or write the cache of another repository. Anyone who may patch pods in a pool namespace can change that allocation, and so write any cache of the pool. Within one repository's cache, the untrusted builds of the repository read everything that a trusted build exported, intermediate stages included (`mode=max`). Keep secrets out of every build stage that a trusted build caches. `spec.workers.cache.scope: pool` makes every caller of a pool share one cache: use it only when they are one trust domain.

### When the cache pod restarts

The registry is one pod with one volume. Builds that start while it restarts run uncached. A running build that imported from it before the restart can fail when it fetches a layer it matched, because `ignore-error=true` covers only the export. A node drain moves the pod, so drain during a quiet period. The store survives the restart on its volume. To empty the cache, scale the StatefulSet to zero, delete the PVC `data-<fullname>-registry-0`, and scale it back to one.

## Observability

Metrics are on the ops port (8080, `/metrics`) through the `<fullname>-metrics` Service (`buildkit-controller-metrics` with the default naming). Set `metrics.serviceMonitor.enabled` to `true`, or `auto` to create it only when the Prometheus Operator CRD exists. The cache registry is a separate pod and serves its own `/metrics` on the `metrics` port of the `<fullname>-registry` Service. The chart's ServiceMonitor does not select it, so add a scrape for that Service to collect the `buildkit_registry_*` metrics. All names start with `buildkit_`, and no metric has a per-allocation or per-pod label. The table in Observability below lists the ones to watch. Logs are JSON on stderr, so a log shipper must read stderr, and they never contain tokens, private keys or Secret data.

Metrics to watch:

| Metric | Use |
|---|---|
| `buildkit_allocations_total{result}` | Allocation outcomes. `result` is `ok` or the error code the client got (`pool_exhausted` when a claim hit its queue timeout, `pool_not_found`, `ca_unavailable` and so on). The API counts its refusals, and the leader counts how each claim ended. A rise in `pool_exhausted` or `ca_unavailable` needs action. |
| `buildkit_allocation_duration_seconds{path}` | Claim creation to bind, `warm` or `cold`. |
| `buildkit_queue_wait_seconds{result}` | Time claims waited in the queue; `result` is `ok` when they bound and `timeout` when they ran out. Demand above `scaling.max`. |
| `buildkit_pool_waiting` | Pending claims of the pool, exact. The queue length. |
| `buildkit_pool_pods{state}`, `buildkit_pool_desired_warm` | Pool state (leader only). |
| `buildkit_claims{phase}`, `buildkit_claim_reconciles_total{result}` | Claims by phase and the outcomes of the leader's tidy pass (for example `collected`, `released`). |
| `buildkit_claim_conflicts_total` | Claim patches that lost a race, because the claim changed or was deleted. A few are normal; for a steady rate, read the leader's logs. |
| `buildkit_cache_allocations_total{tier}` | Allocations whose response carried cache flags, by `trusted` or `untrusted` tier. |
| `buildkit_releases_total{reason}` | Pod retirements by reason: `released` (the claim was deleted or ended), `pod_lost`, `expired`, `failed`, `start_timeout`, `unready`, `ca_replaced`, `stale_template`, `cert_expiring`, `out_of_range` and `idle`. |
| `buildkit_edge_connections_total{result}` | Edge outcomes. Failures include `tls_error`, `expired` and `upstream_error`. Refusals for a connection cap are `handshake_limit` and `peer_handshake_limit` (the oldest handshake in progress, closed for a newer one, replica-wide or from one address), `connection_limit` (`edge.maxConnections`), `namespace_limit` (a quarter of `edge.maxConnections` for the allocations of one pool namespace) and `allocation_limit` (64 connections for one allocation on one replica). |
| `buildkit_watch_namespace_unlisted{kind,namespace}` | 1 while a pool or pod watch runs without a namespace it cannot list. See [Kubernetes API outages](#kubernetes-api-outages). |
| `buildkit_leader` | Sum across replicas must be 1. |
| `buildkit_ca_valid`, `buildkit_cert_not_after_seconds{kind}` | CA and certificate health. |
| `buildkit_auth_total{result}`, `buildkit_jwks_fetch_total{result}` | Login failures and issuer reachability. |
| `buildkit_registry_stored_bytes`, `buildkit_registry_blobs`, `buildkit_registry_manifests` | The cache registry's store (cache pod). Compare `stored_bytes` with `cache.maxSize`. |
| `buildkit_registry_requests_total{route,method,status}`, `buildkit_registry_request_duration_seconds{route}` | Registry traffic and latency. A run of `5xx` or `429` statuses means the cache is failing or uploads hit their limit. |
| `buildkit_registry_denied_total{action}` | Pushes or pulls the registry refused for a worker's allocation. `push` denials are expected for untrusted builds. A burst of `pull` denials is not. |
| `buildkit_registry_connections_refused_total{reason}` | Connections closed before a request: a failed handshake, or a client certificate that names no worker. |
| `buildkit_registry_received_bytes_total` | Bytes pushed. |
| `buildkit_registry_gc_evicted_manifests_total{budget}`, `buildkit_registry_gc_deleted_blobs_total` | What garbage collection removed. `budget` is `pool` or `volume`. |

Suggested alerts, with thresholds to tune:

| Alert | Expression | For |
|---|---|---|
| No leader | `sum(buildkit_leader) == 0` | 2m |
| CA unusable | `min(buildkit_ca_valid) == 0` | 2m |
| CA expiring | `min(buildkit_cert_not_after_seconds{kind=~"client_ca|internal_ca"}) - time() < 365 * 86400` | 1h |
| Pool saturated | `buildkit_pool_waiting > 0` | 10m |
| Queue timeouts | `sum(rate(buildkit_allocations_total{result="pool_exhausted"}[10m])) > 0` | 10m |
| Cache volume nearly full | `buildkit_registry_stored_bytes > 0.9 * <cache.maxSize in bytes>` | 15m |
| Cache failing | `sum(rate(buildkit_registry_requests_total{status=~"5.."}[5m])) > 0` | 10m |
| Allocation failures | `sum(rate(buildkit_allocations_total{result!="ok"}[5m])) / sum(rate(buildkit_allocations_total[5m])) > 0.05` | 10m |
| JWKS fetch failing | `increase(buildkit_jwks_fetch_total{result="error"}[10m]) > 5` | 10m |

Health probes are `/healthz` (process alive) and `/readyz` (config loaded, Pool, Pod and claim caches listed and not stale, CA loaded, serving certificate loaded, listeners bound). `/readyz` also lists namespaces the replica serves without (`degraded: pod_store/<namespace>`, `claim_store/<namespace>`) while staying ready. The ops server takes 64 connections at once and closes a connection that has not sent its request head within 10 s. It has no authentication: any pod that reaches port 8080 reads the metrics, which name every pool namespace and OIDC issuer. Restrict it with a NetworkPolicy of your own that admits only your Prometheus. Leadership is not part of readiness. See [Kubernetes API outages](#kubernetes-api-outages) for when a cache counts as stale.

## Kubernetes API outages

Each replica keeps in-memory caches of pools, pods and claims, fed by watches. When a watch fails, for example while the API server restarts, the replica keeps its last list and starts a new watch after 1, 2, 4, 8, 16 and then every 30 s. Each new watch lists everything again. An API server blip reaches every replica at once, so the replicas ride it out instead of all leaving the Service together.

- For 60 s after the first watch error the replica stays Ready and the edge keeps admitting new connections from the cache. Five retries fit in that window, so an outage of up to about 30 s shows only in `buildkit_watch_errors_total`.
- If no new list completes within 60 s, `/readyz` fails naming `pool_store` or `pod_store`, and the edge refuses new connections with `store_stale`. By then the cache may have missed too many changes to vouch for an allocation.
- The first completed list makes the replica Ready again and the edge admits again.
- Builds already connected keep their connections throughout. A release or pod deletion during the outage closes its connection once the cache catches up.
- At startup there is no grace: a replica is not Ready until both caches have listed once.

With several `watchNamespaces`, each namespace has its own watch, and one namespace that cannot be listed does not stop the others. A namespace that has failed for 30 s is left out of the cache. The claim watch does the same for each `claims.namespaces` entry, and the leader leaves the claims of a namespace it cannot list alone. The replica keeps the objects it last listed for it, logs a warning, sets `buildkit_watch_namespace_unlisted{kind,namespace}` to 1 and lists `pool_store/<namespace>` or `pod_store/<namespace>` as `degraded:` on `/readyz`, which still answers 200. Every replica sees the same failure, so failing readiness would empty the Service. When every namespace fails, the replica stops being Ready after 60 s. A later successful list clears the gauge and the line.

Inside the 60 s the cache can miss a release or a deletion. Workers are single-use, so that can only admit the client that already held the worker. Expiry is checked against the clock, so an expired allocation is refused anyway. If a deleted pod's IP now belongs to another pod, the connection fails at the upstream handshake, because each worker's serving certificate names its own pod and nonce.

### Stalled connections

A watch can also hang without failing, for example when the API server freezes or a load balancer drops the flow without closing it. The controller's Kubernetes client fails any request that receives nothing for 90 s: no response headers within 90 s of sending it, or no data for 90 s after that. The limit belongs to each request, so a request sent on a connection that sat idle still gets the full 90 s. A watch that fails this way is handled like any other watch error. A stalled watch therefore goes unnoticed for at most 90 s. If no list completes after that, the 60 s bound above applies, so `/readyz` fails and the edge refuses with `store_stale` at most 150 s after the API server last sent anything. Until then a release or pod deletion can be missed, so a released allocation's open connection can stay up that long.

- A healthy watch is never silent for 90 s. The API server sends a bookmark on every watch about once a minute, and 90 s leaves 30 s of margin.
- If the API server runs with the watch cache disabled (`--watch-cache=false`), there are no bookmarks. A watch that sees no change then fails after 90 s and starts again; for the Pool and Pod caches that means a full list, so expect one extra list of each every 90 s on each replica. The replica stays Ready through these relists, but `buildkit_watch_errors_total` counts each one.
- The timeout covers every request on the client: the Pool, Pod, Secret, Lease and claim watches, each namespace's watch when `watchNamespaces` lists several, and every write. It counts silence, not duration, so a large list that keeps streaming is not cut. Allocation, reconcile, TokenReview and Lease calls give up after `kube.requestTimeout`. The controller refuses a `kube.requestTimeout` of 90 s or more, because the 90 s limit would end those calls first and the setting would have no effect. The default is 5 s. The chart renders no `kube` key, so the default applies to a chart install. In practice the 90 s limit ends only stalled watches and lists, and calls that have no timeout of their own.

## Abandoned allocations

A CI job that is cancelled before it learns its allocation id cannot release it. The API stamps each claim with a `seen-at` annotation whenever its caller asks for it (`POST` or `GET`, at most every 15 seconds), and with `delivered` once it answers with the bound allocation. The leader deletes a claim that was never delivered and has not been asked for in 3 minutes, pending or bound, and then retires its worker. The log line is `deleted an abandoned claim`, and `buildkit_claim_reconciles_total{result="abandoned"}` counts them. A delivered claim is left to its TTL or its release.

`seen-at` is written with the clock of the API replica that answered and compared with the clock of the leader. Keep the nodes' clocks synchronized (NTP or your cloud's time service) to well within a minute. A leader whose clock runs ahead by more than about a minute can delete the claim of a client that is still polling, which then sees `404 not_found` and fails its step.

## Troubleshooting

Start with the pool conditions.

```sh
kubectl get bkp -n ci <pool> -o jsonpath='{range .status.conditions[*]}{.type}={.status}/{.reason}: {.message}{"\n"}{end}'
```

| Condition and reason | Cause | Fix |
|---|---|---|
| `Ready=False/InvalidSpec` | The controller rejected the spec, for example an unknown `scaling.timeZone`, or an `allowCIDRs` entry it refuses ([Worker egress](#worker-egress)). | Fix the field named in the message. |
| `Ready=False/TemplateConflict` | The template sets a key the controller owns, such as a reserved label, volume name, mount path, a CPU architecture that contradicts `platforms`, an annotation that changes the pod's network or runtime ([Who may write a pool](#who-may-write-a-pool)), or (with the cache registry on) a `buildkitdConfig` that is not valid TOML or sets `registry` or the cache registry's entry under it to something other than a table. | Remove the fields named in the message. No pod is created while this is set. |
| `Ready=False/CAUnavailable` | No valid CA is loaded yet. | Check the leader's logs and the Secret. The leader repairs it by itself. |
| `Ready=False/SandboxPending` | The worker ServiceAccount or NetworkPolicy could not be applied, at creation or in the 5-minute re-apply. | Check the message and the pool namespace for admission webhooks that reject them. |
| `Degraded=True/PodCreateFailed` | Pod creation failed (quota, admission, API error). | `kubectl get events -n <ns>`, then fix quota or policy. The controller retries with backoff. |
| `Degraded=True/PodStartTimeout` | A pod that never became Ready was not Ready within `workers.startTimeout`. | Check scheduling, image pull and node resources. Raise `startTimeout` for slow image pulls. A free pod that was Ready and then stayed unready for `startTimeout` is replaced without `Degraded`. |
| `Degraded=True/PodFailed` | A pod ended in phase `Failed` or `Succeeded`. | Read the pod's logs. |
| `Degraded=True/UnknownIssuer` | An `oidc` rule names an issuer that is not in `auth.oidc`. | Match `issuer` to the Helm `auth.oidc[].url` byte for byte, or add the issuer. The rule never matches until you do. |
| `Degraded=True/SlotTaken` | A pod named `<pool>-<n>` exists that the pool does not own. | Delete or rename it. |
| `Degraded=True/CAReplaced` | The CA was replaced and old pods remain. | Clears when the old pods retire. |
| `CAExpiringSoon=True` | A CA expires within a year. | Rotate as described above. |

Errors the API returns to clients, and the usual cause:

- 401 `unauthenticated`. The token is invalid, expired, has the wrong audience, or comes from an issuer that is not configured. Check `buildkit_auth_total`. The refusal log is sampled at 10 lines a second, so count with the metric, not the log. Each line has a `detail` field with a fixed reason text and never token or claim data.
- 503 `upstream_error` with `Retry-After: 2` on authentication. A ServiceAccount token could not be reviewed: the API server timed out (`kube.requestTimeout`), throttled the controller (429), or the replica hit a review limit: 50 a second, `auth.serviceAccount.maxConcurrentReviews` in flight, or 4 in flight for one address. A sustained rate usually means a flood of SA-shaped tokens. Look at `buildkit_auth_total{method="serviceaccount",result="error"}`.
- 503 `upstream_error` with `Retry-After: 30` on authentication. The keys of an OIDC issuer could not be fetched. The message reads "token issuer keys unavailable". It also answers for a token signed with a key id the controller has not fetched yet, while a recent fetch holds the next one back. Check that the controller can reach the issuer's `jwksUrl` and look at `buildkit_jwks_fetch_total{result="error"}`. The Action and the curl recipe retry it.
- 503 `upstream_error` with `Retry-After: 2` on `POST` or `DELETE /v1/allocations/{id}`. The API server throttled the request (429 or 408), answered with a 5xx, or did not answer, so the controller does not know the outcome. Repeating either is safe (a `POST` with the same key finds the claim the first one made), so the Action and the curl recipe retry it. If the retries run out, they log a warning and the worker drains at the allocation's expiry.
- 404 `pool_not_found`. The pool does not exist, or no access rule matches the caller. The API never answers 403 for this, so pools cannot be probed.
- 503 `pool_exhausted`. The pool is at `scaling.max` and the claim was still pending when its queue timeout ran out (`QueueTimeout` on the claim). It carries `Retry-After: 5`. Raise `max`, or `queueTimeout`.
- 202 `{"state": "pending"}` is not an error: the claim is queued. The client polls `GET /v1/allocations/{id}?wait=25s` with the returned id. Each answer carries the claim's `position` in the queue. `kubectl get bkc -n <release namespace>` shows the claim.
- 503 `pod_lost`. The worker pod of a bound allocation is gone. It is final for that id. Run the job again.
- 404 `not_found` on `GET` or `DELETE /v1/allocations/{id}`. The id is unknown, its claim finished and was cleaned up (10 minutes after it ended), or it belongs to another caller. The API does not tell these apart, so a leaked id says nothing about a live allocation. A claim that another replica created a moment ago is waited for up to 2 seconds.
- 503 `upstream_error` on `GET`, from a replica that has started to stop. The client's next poll reaches another replica.
- 400 `invalid_request`. The request is malformed, for example a `publicKey` over 1024 bytes.
- 503 `ca_unavailable`. No valid CA is loaded on that replica, for example while the CA Secret is being replaced. It also answers when the internal CA ends before the allocation would, because no worker certificate could outlive the allocation. It carries no `Retry-After`. The Action and the curl recipe repeat the `POST` with backoff until their budget runs out, then fail. Run the job again.

A client that cannot connect to the edge usually has a certificate problem. Check `buildkit_edge_connections_total` for `tls_error`, `bad_san` and `expired`, and set `--set logLevel=debug` on the chart to see handshake failures. The curl recipe can use `API_CA` to trust a self-issued certificate.

## Multi-pool builders and allocation domain

A builder that combines several pools, which is what the GitHub Action does with more than one `pools` entry, needs one edge address per allocation. Set `edge.allocationDomain` (for example `alloc.bk.example.com`) and create a wildcard DNS record `*.alloc.bk.example.com` that points at the edge address. Endpoints then become `tcp://<alloc-id>.alloc.bk.example.com:8444`. The pools must declare distinct `platforms`. With a single pool you can leave the domain empty.

The serving certificate does not need to cover those names. Clients always send `edge.serverName` as the TLS server name (the API returns it as the buildx `servername` driver option) and verify the certificate against that one name, so only DNS needs the wildcard.

## In-cluster claims

Every allocation is a `BuildKitClaim` ([architecture.md](architecture.md#buildkitclaim)), so the claim CRD is always installed and the controller serves the claims the API creates in its own namespace. `claims.namespaces` is only for pipelines that create claims in their own namespaces. It is empty by default.

List those namespaces in `claims.namespaces` to start serving their claims and to grant the controller `buildkitclaims` read and status write across namespaces. Secret `get`, `create` and `delete` are granted by a
Role in each listed namespace only, and a claim from any other namespace fails
with `NamespaceNotServed`. With `watchNamespaces` set, the claim read and status rights are
Roles in each `claims.namespaces` namespace as well, and a claim in any other namespace is
ignored: it gets no status, not `NamespaceNotServed`.

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

The controller honours the `owner`, `cacheScope` and `cacheTier` fields of a claim only in its own namespace, so a tenant who can create claims cannot pass as a CI identity, borrow another scope's cache or write a cache the pool does not let it write. Without a `publicKey`, the controller generates the key and writes the Secret `<claim>-buildkit`.

`kubectl get bkc -A` shows each claim's phase. The `Bound` condition's reason
says why a claim waits or failed:

| Reason | Meaning | Action |
|---|---|---|
| `Queued` | No free worker; the claim waits in the pool queue | Raise `scaling.max` or wait |
| `QueueTimeout` | The claim was still pending at its `queueTimeout`. It is `Failed` | Raise `scaling.max` or the timeout, then recreate the claim |
| `PoolNotReady`, `CAUnavailable` | Transient; retried | Check the pool's conditions and controller logs |
| `PoolNotFound` | Pool missing or no `claims` rule names this namespace. A claim younger than 30 s stays Pending with this reason, so a pool created just after the claim is found | Fix `poolRef` or the pool's access rules, then recreate the claim |
| `InvalidRequest` | `ttl` above the pool's `maxTTL`, or a `queueTimeout`, key or cache choice the pool does not allow | Recreate with valid values |
| `NamespaceNotServed` | The claim's namespace is not in `claims.namespaces` | Add the namespace to `claims.namespaces` and upgrade, then recreate the claim |
| `SecretConflict` | A Secret `<claim>-buildkit` exists and the claim does not own it | Rename the claim or remove the Secret |
| `Expired` | The TTL ran out and the Secret was deleted | Delete the claim and create a new one |
| `WorkerLost` | The worker pod was deleted, evicted or failed before expiry; the Secret was deleted. The controller does not rebind, because the client holds a certificate for a pod that is gone | Delete the claim and create a new one |
| `SecretRejected` | The API server refused to create the claim Secret with a final error (a 4xx other than 401, 409 and 429), for example an admission policy. The controller retires the worker first | Fix the policy or the Secret settings, then recreate the claim |

Metrics: `buildkit_claims{phase}`, `buildkit_claim_reconciles_total{result}` and `buildkit_pool_waiting`.
Allocations for claims also count in `buildkit_allocations_total`.

A deleted claim's worker is released within a few seconds by the leader's tidy pass (every 2 seconds); if the controller is down at the time, the next leader releases it, and
the TTL bounds the leak in any case. The leader deletes finished CI claims 10 minutes after they end. In-cluster claims stay until their owner deletes them. Emptying `claims.namespaces` keeps every claim and claim Secret in tenant namespaces. Nothing reconciles them, and workers expire at their TTL. Delete
the claims to remove them.
