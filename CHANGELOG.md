# Changelog

All notable changes are listed here, newest first.

## 0.2.0

0.2.0 replaces the per-replica allocation queues with one claim queue and replaces both cache layers with a built-in cache registry. Read [From 0.1.0 to 0.2.0](docs/operations.md#from-010-to-020) before upgrading.

### Breaking

- `spec.workers.cache.local` and `spec.workers.cache.registry` of `BuildKitPool` are removed, with their CEL rules, defaults and status fields. A pool that still sets them has the fields pruned by the API server, and kubectl's strict field validation refuses the apply. `spec.workers.cache` now holds `scope`, `writers` and `maxSize`.
- Workers have no volumes. The PVC per slot, the `cache-init` init container, scope binding on pods, soft affinity, spill and the wipe markers are gone. BuildKit build history is no longer turned off on any pool.
- The external cache registry layer is removed. A team that wants its own registry passes its own buildx flags.
- Helm values `cacheDefaults` and the `cache` key of a `pools` item are removed, and the chart rejects them. The chart gains a `cache` block for the registry (`cache.enabled`, `maxSize`, `poolMaxSize`, `maxBlobSize`, `maxUploadSize`, `gcInterval`, `persistence`, `resources`).
- The allocation API: `cache` accepts `auto` and `none` only (`registry` and `local` are gone), and the `201` response no longer has `cache.local`. `cache.registry` now carries `to` (absent when the caller may only read the cache) and `from`. `cacheKey` is the cache tag and defaults to the pool name; the pool `registry.defaultKey` is gone.
- The script variable `CACHE` accepts `auto` or `none`. The Action input `cache` does too, and still takes `local` and `registry` as `auto` with a deprecation warning.
- `POST /v1/allocations` may answer `202` with an id when no worker binds within about 20 seconds. A client from before 0.2.0 does not follow it. Update the Action (and `buildkit-allocate.sh`) with the controller.
- `spec.workers.cache.sizeLimit` moves to `spec.workers.stateSizeLimit`. It sizes the buildkitd state `emptyDir`, not the cache.
- Helm value `claims.enabled` is removed, and the chart rejects it. A non-empty `claims.namespaces` turns on in-cluster claims.
- Helm value `watchNamespace` and the controller config key of the same name are removed. Use `watchNamespaces: [<namespace>]`.
- The TokenReview reserve for recently seen service accounts is removed, with its two Helm values and config keys under `auth.serviceAccount`. The chart and the controller reject them.
- The claim CRD moves from a chart template to `crds/`, beside the pool CRD. Helm installs both on first install and upgrades neither, so apply `crds/` yourself before `helm upgrade`. A claim CRD that v0.1.0 installed as a template carries `helm.sh/resource-policy: keep`, so the upgrade does not delete it.
- The cache registry runs as `buildkit-controller registry`, its chart template is `registry.yaml` with the label `app.kubernetes.io/component: registry`, and the controller config keys are `registry.host` and `registry.tlsSecretName`. The chart writes them; only a hand-written config needs them.
- The error codes `shutting_down` and `start_timeout` are gone from the API. The Action and `buildkit-allocate.sh` still retry them, so they keep working against a v0.1.0 controller during the upgrade.
- Metrics removed: `buildkit_queue_depth`, `buildkit_cache_affinity_total` and the volume metrics. Alert on `buildkit_pool_waiting` instead of `buildkit_queue_depth`. `buildkit_releases_total` no longer has the `cache_scope` reason.
- `BuildKitClaim` spec gains `queueTimeout`, `publicKey`, `owner`, `cacheScope` and `cacheTier`, and its status gains `certificate` and `queuePosition`. The controller honours `owner`, `cacheScope` and `cacheTier` only in its own namespace. `status.cache` no longer has a local result. Apply the regenerated CRDs.
- `GET` and `DELETE /v1/allocations/{id}` answer `404 not_found`, not `403`, for another caller's allocation, so a leaked id says nothing about a live allocation. A repeated `POST` with another caller's key still gets `403`.
- Allocation owners changed. An OIDC owner includes the token's `run_id`, `run_attempt` and `job_workflow_ref`, `%` and `#` in the issuer and `sub` are percent-encoded, and a ServiceAccount owner includes `#pod=<uid>` for a pod-bound token. A release of an allocation made before the upgrade answers `404`, and its worker is freed at its expiry.
- One owner holds at most half of a pool's `scaling.max` workers at once (at least 1). Its further claims wait while other callers bind, and a `POST` from an owner that already holds or waits for twice that many gets `503 pool_exhausted`.
- The default cache writers changed. A GitHub Actions build writes by default only when its `event_name` is `push`, `schedule` or `workflow_dispatch` and its `ref_protected` claim is `true`. `merge_group` builds no longer write by default.
- Builds from `pull_request`, `pull_request_target`, `workflow_run`, `issue_comment`, `pull_request_review`, `pull_request_review_comment`, `deployment` and `deployment_status` events, and GitLab pipelines whose `pipeline_source` is `merge_request_event`, `external_pull_request_event` or `parent_pipeline`, never write the cache, whatever `spec.workers.cache.writers` says.
- A pool's `allowCIDRs` entry that overlaps a protected range, `workerNetwork.clusterCIDRs` or `workerNetwork.extraDeniedCIDRs` makes the pool `Ready=False/InvalidSpec`, unless `workerNetwork.permittedCIDRs` permits it. While `clusterCIDRs` holds no range of the entry's family, an IPv6 entry and an IPv4 entry that overlaps `deniedCIDRs` are refused too. On a default install, which sets no `clusterCIDRs`, that refuses every IPv6 and private IPv4 entry.
- A pool in the controller's namespace is refused with `Ready=False/InvalidSpec`, and the controller refuses to start with its own namespace in `watchNamespaces` or `claims.namespaces`.
- A pod template annotation that Multus, Calico, Cilium, OVN, Kata or CRI-O reads is refused with `TemplateConflict`.
- The edge accepts TLS 1.3 only.
- Startup refuses a `proxyProtocol.trustedCIDRs` entry shorter than /8 (IPv4) or /32 (IPv6), a `workerNetwork.dns.cidrs` entry that holds a whole protected range, a `leaderElection.leaseDuration` over 10 minutes, a `shutdown.delay` over 1 hour, a `shutdown.drainTimeout` over 48 hours, and any other duration over 366 days.
- A `publicKey` over 1024 bytes gets `400 invalid_request`.
- A cache grant covers exactly one repository, `<namespace>/<pool>/<scope>`. Cross-repository blob mounts are gone.

### Added

- One durable, first-in, first-out queue per pool: every allocation is a `BuildKitClaim`. The API creates the claim in the controller's namespace, and the leader binds claims in order of creation, ties broken by uid. A queued request survives a replica restart or a leader change, and `kubectl get bkc` shows it with a Position column.
- `GET /v1/allocations/{id}?wait=25s` long-polls an allocation, and any replica answers. No API request stays open longer than 25 seconds, under every common gateway and load-balancer idle timeout. A client that loses its connection keeps its place.
- `POST /v1/allocations` is idempotent per public key: the allocation id is derived from `publicKey`, and a repeated `POST` with the same key from the same caller answers for the allocation the first one made, in its place in the queue, instead of making a second one. The same key from another caller gets `403`. A `POST` the client drops no longer deletes its claim at once: the retry finds it. The leader deletes a CI claim whose bound answer was never delivered and whose caller has not asked for it (`POST` or `GET`) for 3 minutes, so a cancelled job frees its place and its worker within about 3 minutes. The rule compares clocks of different nodes, which must be synchronized. The controller needs `patch` on `buildkitclaims` for the `seen-at` and `delivered` annotations; the chart grants it.
- The Action and `docs/ci/buildkit-allocate.sh` repeat a `POST` with the same key on any connection failure (a reset or a timeout after sending too) and on any `5xx` other than `pool_exhausted` and `pod_lost`. A queue timeout starts the next allocation with a new key. Releases and the pool listing retry connection failures and any `5xx` too.
- `queueTimeout` on a claim and the `QueueTimeout` reason when a claim stays pending too long. An in-cluster claim whose `queueTimeout` is above the pool's `maxQueueTimeout` fails with `InvalidRequest`. `status.waiting` of a pool is the exact number of pending claims.
- Finished CI claims are deleted 10 minutes after they end, so recent history shows without growing.
- The Action and `docs/ci/buildkit-allocate.sh` poll a queued allocation and share one retry budget: the queue timeout plus 60 seconds. `upstream_error` now carries `Retry-After: 2` and is retried on `DELETE` too.
- A built-in cache registry, the fourth role of the binary (`buildkit-controller registry`), run by the chart as a one-replica StatefulSet with one volume. It serves the OCI distribution subset that BuildKit's registry cache uses, over mTLS with the internal CA, and only worker pods reach it. It authorizes each worker from the pod's live allocation: a pull from the caller's own cache scope, and a push only when the pool trusts the caller.
- Trust tiers for the cache. `spec.workers.cache.writers` lists the callers that may write; by default protected-ref pushes, schedules and `workflow_dispatch` runs and in-cluster callers write, and pull requests only read, so fork builds start warm and cannot poison the cache. `spec.workers.cache.scope` (`identity` or `pool`) and `spec.workers.cache.maxSize` set the scope and the pool budget.
- Cache garbage collection by pool and volume budget. It keeps the newest first, manifests by last use and unreferenced recent uploads by upload time, removes what does not fit and goes on, then marks and sweeps unreferenced blobs. It evicts nothing for a pool budget while it cannot list pools. `cache.maxUploadSize` bounds uploads in progress, per worker identity.
- `buildkit_registry_*` metrics, `buildkit_cache_allocations_total{tier}`, `buildkit_claims`, `buildkit_claim_reconciles_total` and `buildkit_claim_conflicts_total`.
- `cargo xtask registry-verify`.
- A one-time sweep that deletes the cache PVCs of v0.1.0 when the leader starts, and logs each one. It keeps a labelled PVC that a controller other than a `BuildKitPool` holds. The sweep and the controller's `list` and `delete` rights on PVCs are removed in the release after 0.2.0.
- `buildkit-controller --version` and `buildkit-controller registry --version` print the version.
- `allowCIDRs` checks against protected ranges, `clusterCIDRs`, `extraDeniedCIDRs` and `permittedCIDRs` (see Breaking).
- `TOKEN_FILE` for `buildkit-allocate.sh`: a file the script reads again before every request, for tokens shorter-lived than the build.
- Release notes give the image digest and the `cosign` and `gh attestation verify` commands. The chart package has build provenance. The release refuses a tag that differs from the Cargo and chart versions.

### Changed

- Each worker gets a second certificate, a clientAuth-only leaf from the client CA, which the pool's `buildkitd.toml` presents to the cache registry. The serving certificate stays serverAuth only. The registry trusts the client CA for clients, and mTLS is its boundary.
- The Action and the script give a `POST` a 40 second timeout, like a poll. Against a v0.1.0 controller, which does not report `queueTimeoutSeconds`, the `POST` waits for the whole remaining budget, so a slow allocation still completes. They stop sending new `POST`s once the budget leaves no time to queue.
- After a `202` that came back within a second, the Action and the script wait 1 to 2 seconds before the next poll. The Action reuses an OIDC token until 60 seconds before it expires.
- `buildkit-allocate.sh` unsets `TOKEN` before it runs the command, and keeps the allocation id after a `401` on a poll so it can still release it.
- The claim CRD is always installed. `claims.namespaces` only adds the tenant namespaces whose claims the controller serves.
- `gateway.api.timeouts.request` and `.backendRequest` default to 60s instead of 15m, since no API request lasts longer than 25 seconds. The Envoy stream idle timeout follows them.
- Behind a Gateway without `edge.proxyProtocol`, the chart renders `edge.maxHandshakesPerIp` as 0.
- A draining replica no longer hands queued requests off. Pending claims stay pending and another replica answers the client's next poll.
- `GET` and `DELETE` of an allocation read the claim from the replica's cache only, and wait up to 2 seconds for a claim another replica just created. Only claims the leader admitted count as pool demand.
- Over a handshake cap, the edge closes the oldest handshake in progress instead of the new connection. A connection that sends no TLS ClientHello within 2 seconds is closed. The allocations of one pool namespace may hold at most a quarter of `edge.maxConnections` per replica (`namespace_limit`).
- The API's per-address caps count an IPv6 client by its /56.
- Every TokenReview shares one limit of 50 a second per replica. Over a limit, the caller gets the retryable `503 upstream_error`.
- An OIDC token signed with a key id that the controller has not fetched yet, while a recent fetch holds the next one back, gets `503 upstream_error` instead of `401`.
- Every pool's NetworkPolicy denies the protected link-local, loopback, metadata and NAT64 ranges, whatever `workerNetwork.deniedCIDRs` says.
- The leader applies a pool's ServiceAccount and NetworkPolicy before every pass that creates pods, and again every 5 minutes.
- After a refused pod create, the pass creates nothing more and the create backoff starts. A pod whose Secret create failed is deleted at once.
- In the last 25 hours of either CA's life the leader creates no worker pods.
- Worker containers drop `NET_RAW`.
- A CA Secret backup (`<name>-replaced-<timestamp>`) holds the certificates and no private keys.
- The registry reissues its TLS Secret when the key does not match the certificate. It takes at most 64 connections per address with a 5 second handshake, buffers at most 16 manifest bodies (a push that waits 5 seconds for a slot gets `429`), and needs a manifest body within 30 seconds. A failed pull record never fails a read.
- The registry pod sets `fsGroupChangePolicy: OnRootMismatch`, so a restart skips the recursive chown of the volume.
- A replica campaigns for the Lease only once its caches have synced. A replica whose component fails releases the Lease and exits non-zero: at once when the API, the edge, the certificate loader or the ops server failed, after a drain of at most 5 minutes otherwise. A deleted Lease whose holder is alive is not taken until its lease runs out, and a holder's lease duration counts at most 10 minutes.
- The claim watch relists after every error. A claims namespace that cannot be listed is left out, shown as `degraded: claim_store/<namespace>` on `/readyz`, and its claims are left alone until it lists again.
- Removed from the allocator: per-replica queues and wake-ups, API replicas creating pre-claimed pods, the provisional-expiry commit and its orphan guard, and the commit-on-body step.

## 0.1.0

The first release.
