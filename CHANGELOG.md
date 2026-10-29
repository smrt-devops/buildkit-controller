# Changelog

All notable changes are listed here, newest first.

## Unreleased

v3 replaces the per-replica allocation queues with one claim queue and replaces both cache layers with a built-in cache registry. Read [From v0.1.0 to v3](docs/operations.md#from-v010-to-v3) before upgrading.

### Breaking

- `spec.workers.cache.local` and `spec.workers.cache.registry` of `BuildKitPool` are removed, with their CEL rules, defaults and status fields. A pool that still sets them has the fields pruned by the API server, and kubectl's strict field validation refuses the apply. `spec.workers.cache` now holds `sizeLimit`, `scope`, `writers` and `maxSize`.
- Workers have no volumes. The PVC per slot, the `cache-init` init container, scope binding on pods, soft affinity, spill and the wipe markers are gone. BuildKit build history is no longer turned off on any pool.
- The external cache registry layer is removed. A team that wants its own registry passes its own buildx flags.
- Helm values `cacheDefaults` and the `cache` key of a `pools` item are removed, and the chart rejects them. The chart gains a `cache` block for the registry (`cache.enabled`, `maxSize`, `poolMaxSize`, `maxBlobSize`, `maxUploadSize`, `gcInterval`, `persistence`, `resources`).
- The allocation API: `cache` accepts `auto` and `none` only (`registry` and `local` are gone), and the `201` response no longer has `cache.local`. `cache.registry` now carries `to` (absent when the caller may only read the cache) and `from`. `cacheKey` is the cache tag and defaults to the pool name; the pool `registry.defaultKey` is gone.
- The Action input `cache` and the script variable `CACHE` accept `auto` or `none`.
- `POST /v1/allocations` may answer `202` with an id when no worker binds within about 20 seconds. A client from before v3 does not follow it. Update the Action (and `buildkit-allocate.sh`) with the controller.
- The error codes `shutting_down` and `start_timeout` are no longer sent. They stay in the API enum so older clients parse them.
- Metrics removed: `buildkit_queue_depth`, `buildkit_cache_affinity_total` and the volume metrics. Alert on `buildkit_pool_waiting` instead of `buildkit_queue_depth`. `buildkit_releases_total` no longer has the `cache_scope` reason.
- `BuildKitClaim` spec gains `queueTimeout`, `publicKey`, `owner`, `cacheScope` and `cacheTier`, and its status gains `certificate` and `queuePosition`. The controller honours `owner`, `cacheScope` and `cacheTier` only in its own namespace. `status.cache` no longer has a local result. Apply the regenerated CRDs.

### Added

- One durable, first-in, first-out queue per pool: every allocation is a `BuildKitClaim`. The API creates the claim in the controller's namespace, and the leader binds claims in order of creation. A queued request survives a replica restart or a leader change, and `kubectl get bkc` shows it with a Position column.
- `GET /v1/allocations/{id}?wait=25s` long-polls an allocation, and any replica answers. No API request stays open longer than 25 seconds, under every common gateway and load-balancer idle timeout. A client that loses its connection keeps its place.
- `queueTimeout` on a claim and the `QueueTimeout` reason when a claim stays pending too long. `status.waiting` of a pool is the exact number of pending claims.
- Finished CI claims are deleted 10 minutes after they end, so recent history shows without growing.
- The Action and `docs/ci/buildkit-allocate.sh` poll a queued allocation and share one retry budget: the queue timeout plus 60 seconds. `upstream_error` now carries `Retry-After: 2` and is retried on `DELETE` too.
- A built-in cache registry, the fourth role of the binary (`buildkit-controller cache`), run by the chart as a one-replica StatefulSet with one volume. It serves the OCI distribution subset that BuildKit's registry cache uses, over mTLS with the internal CA, and only worker pods reach it. It authorizes each worker from the pod's live allocation: a pull from the caller's own cache scope, and a push only when the pool trusts the caller.
- Trust tiers for the cache. `spec.workers.cache.writers` lists the callers that may write. By default pushes, schedules, `workflow_dispatch` and `merge_group` events and in-cluster callers write, and pull requests only read, so fork builds start warm and cannot poison the cache. `spec.workers.cache.scope` (`identity` or `pool`) and `spec.workers.cache.maxSize` set the scope and the pool budget.
- Cache garbage collection by pool and volume budget, oldest manifest first, with mark and sweep of unreferenced blobs. `cache.maxUploadSize` bounds uploads in progress.
- `buildkit_registry_*` metrics, `buildkit_cache_allocations_total{tier}`, `buildkit_claims`, `buildkit_claim_reconciles_total` and `buildkit_claim_conflicts_total`.
- `cargo xtask registry-verify`.
- A one-time sweep that deletes the cache PVCs of v0.1.0 when the leader starts, and logs each one. The sweep and the controller's `list` and `delete` rights on PVCs are removed in the release after this one.

### Changed

- Each worker gets a second certificate, a clientAuth-only leaf from the client CA, which the pool's `buildkitd.toml` presents to the cache registry. The serving certificate stays serverAuth only. The registry trusts the client CA for clients, and mTLS is its boundary.
- Builds from `pull_request`, `pull_request_target`, `workflow_run`, `issue_comment`, `pull_request_review` and `pull_request_review_comment` events never write the cache, whatever `spec.workers.cache.writers` says.
- A `POST` from the Action and the script waits for the whole remaining budget (queue timeout plus 60 seconds) instead of 40 seconds, so a slow allocation against a v0.1.0 controller completes. Polls keep a 40 second timeout.
- The claim CRD is always installed. `claims.enabled` only adds the tenant namespaces whose claims the controller serves.
- A draining replica no longer hands queued requests off. Pending claims stay pending and another replica answers the client's next poll.
- Removed from the allocator: per-replica queues and wake-ups, API replicas creating pre-claimed pods, the provisional-expiry commit and its orphan guard, and the commit-on-body step.

## 0.1.0

The first release.
