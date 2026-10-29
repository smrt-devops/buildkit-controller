# Using buildkit-controller from CI

Clients use the buildx remote driver, and there is no CLI. The GitHub Action and the `buildkit-allocate.sh` script below both call `POST /v1/allocations`, `GET /v1/allocations/{id}` and `DELETE /v1/allocations/{id}`, and the response carries the endpoint, the CA, the allocation certificate and the cache flags.

## GitHub Actions

See `action/`. The job needs `permissions: id-token: write`.

```yaml
- uses: smrt-devops/buildkit-controller/action@v0
  with:
    url: https://bk.example.com
    # ca-cert: ${{ vars.BK_CA }}   # see "Trusting the API" below
    pools: |
      ci/amd64
      ci/arm64        # more than one pool needs edge.allocationDomain and distinct pool platforms
- run: docker buildx build --platform linux/amd64,linux/arm64 .
```

The post step removes the builder and releases every allocation, even when the job fails.

### Trusting the API

`ca-cert` is the CA the Action trusts for the API listener; without it, the system roots. Which certificate the API presents depends on how the chart exposes it:

- Behind a Gateway with `gateway.api.mode: terminate` (the chart's default), the Gateway's own certificate. With a publicly trusted one, leave `ca-cert` out. With a private CA, pass that CA.
- With `service.type: LoadBalancer` or `gateway.api.mode: passthrough`, the controller's serving certificate. Pass the controller's CA from the `buildkit-controller-ca-bundle` ConfigMap (key `ca.crt`), unless `edge.tls.secretName` holds a publicly trusted certificate.

The edge needs no input: each allocation response carries the CA for the edge, and the Action uses it.

### How an allocation completes

The Action and the script follow the same steps.

1. `POST /v1/allocations` creates the allocation. If a worker binds within about 20 seconds, the answer is `201` with the full response, so a warm pool costs one round trip.
2. Otherwise the answer is `202` with `{"id", "state": "pending", "position"}`. The allocation is queued in the pool. The client keeps the id.
3. The client calls `GET /v1/allocations/{id}?wait=25s` until it gets an answer. `200` is the full response. `202` means still pending, and the client asks again at once. No request stays open longer than 25 seconds, so gateway and load-balancer timeouts never cut the wait, and a client that loses its connection asks again and keeps its place.
4. When the build ends, `DELETE /v1/allocations/{id}` releases the allocation.

The whole allocation, the first `POST`, every poll and any new `POST`, shares one budget: the queue timeout plus 60 seconds. The queue timeout is `queue-timeout` (`QUEUE_TIMEOUT` for the script), or, when that is unset, the pool's default `queueTimeoutSeconds` from `GET /v1/pools`. A controller too old to report the default leaves the budget at 1 hour (the largest queue timeout a pool allows) plus 60 seconds. Each request has a 40 second timeout.

### Retries

Which answers the client repeats, and which end the step:

| Answer | Client does |
|---|---|
| `201` on `POST`, `200` on `GET` | Done. |
| `202` | Poll the id again. |
| `503 pool_exhausted` on `POST` (`Retry-After: 5`) | The pool's queue timeout ran out. Wait, then `POST` again while the budget lasts. |
| `503 pool_exhausted` on `GET` | The claim hit its queue timeout and the id is dead. Same as above: a new `POST` while the budget lasts, then fail. |
| `503 upstream_error` (`Retry-After: 2`) | The controller could not reach the Kubernetes API or the issuer's keys (token review, JWKS). Repeat the request after `Retry-After`. It is retried on `POST`, `GET` and `DELETE`. |
| Other `5xx` on `GET`, and connection failures while polling | Poll the same id again after `Retry-After`, else 2 seconds. |
| `503 pod_lost` on `GET` | The worker is gone. The id is dead. Fail the step. |
| `404 not_found` and `403 forbidden` on `GET`, other `4xx` | Fail the step. |
| Other `503`, for example `ca_unavailable`, with no `Retry-After` | Fail the step. |

Two older `503` codes with `Retry-After`, for a worker that did not start in time and for a replica that was stopping, are no longer sent. A client still treats them like `upstream_error`, so it works against an older controller.

The Action repeats `POST /v1/allocations` only on a 503 with `Retry-After` and one of the codes above, because an intermediary that adds its own `Retry-After` to a 503 may have forwarded the request. It also retries connection errors raised before the request was sent (connection refused, DNS failure). It waits the `Retry-After` seconds (at least 1, at most 30), logs each retry, and fetches a fresh OIDC token for every attempt. A retry asks for just the time left in the budget, so the total never doubles.

Nothing else is retried: a connection reset after the request was sent and a timeout waiting for the response of the `POST` fail the step, because the controller may have created an allocation whose id the client never saw. That allocation ends at its TTL. With several pools, each pool allocates on its own; if one pool fails, the allocations that succeeded are released. Releases (`DELETE`; a 404 counts as released) and the pool listing (`GET /v1/pools`) retry any 503 with `Retry-After`, since repeating them is safe. They have their own budget of 60 seconds, because neither queues.

### Build cache

The pool's built-in cache registry is on by default (see [Build cache](../architecture.md#build-cache)). The controller returns the cache flags in the allocation response, and the Action turns them into outputs. A build that does not pass them on runs without the cache.

A repository has its own cache. A build that the pool trusts to write (by default `push`, `schedule`, `workflow_dispatch` and `merge_group` events) gets `--cache-from` and `--cache-to`. A build that it does not trust, such as a `pull_request`, gets `--cache-from` only: it starts warm and cannot change the cache. The Action then logs `read only for this build`, and the `cache-to` output is empty. A cache outage never fails a build, because the flags carry `ignore-error=true`.

| Input | Meaning |
|---|---|
| `cache` | `auto` (default) or `none` |
| `cache-key` | Cache tag, so one repository can keep several caches, for example `main` and `release`. Defaults to the pool name. |
| `cache-mode` | `max` or `min`, replaces the mode in `cache-to` |
| `cache-from` | extra `--cache-from` values, one per line |

| Output | Use |
|---|---|
| `cache-to`, `cache-from` | one value per line, for the inputs of the same name of `docker/build-push-action` |
| `cache-args` | `--cache-from=...` and `--cache-to=...` flags for `docker buildx build` |

With `docker/build-push-action`:

```yaml
- id: bk
  uses: smrt-devops/buildkit-controller/action@v0
  with:
    url: https://bk.example.com
    pools: ci/amd64
- uses: docker/build-push-action@v6
  with:
    builder: ${{ steps.bk.outputs.builder }}
    cache-from: ${{ steps.bk.outputs.cache-from }}
    cache-to: ${{ steps.bk.outputs.cache-to }}
    push: true
    tags: ghcr.io/org/app:${{ github.sha }}
```

With `docker buildx build`, leave the output unquoted so that each flag is one word:

```yaml
- run: docker buildx build ${{ steps.bk.outputs.cache-args }} -t ghcr.io/org/app .
```

The cache needs no registry login. The workers reach the cache registry with their own certificates, and no credential comes from the job.

## Any other CI

`buildkit-allocate.sh` needs `curl`, `jq`, `openssl` and `docker buildx`. It makes a P-256 key locally,
sends only the public key, creates a remote builder, runs your command with `$BUILDER` set, and releases
the allocation on exit, including after a failure or a signal.

```sh
export API=https://bk.example.com POOL=ci/amd64
export TOKEN=...   # OIDC token or service-account token with the controller audience, e.g.
                   # kubectl create token builder --audience buildkit-controller
sh docs/ci/buildkit-allocate.sh sh -c 'docker buildx build --builder "$BUILDER" .'
```

Optional variables are `TTL` and `QUEUE_TIMEOUT` (seconds), `API_CA` (PEM file for the API listener, needed under the same conditions as the Action's `ca-cert`),
`CACHE_KEY` and `CACHE` (the Action's `cache-key` and `cache` inputs). The script retries the allocation
the same way as the Action (see [How an allocation completes](#how-an-allocation-completes) and [Retries](#retries)): it follows a `202` by polling, retries a 503 with `Retry-After` and a code from the table, or a curl failure to resolve or connect, and stops at the queue timeout plus 60 seconds, shrinking the queue timeout it sends on each retry. Its release and pool listing retry any 503 with `Retry-After` for 60 seconds.
Without `QUEUE_TIMEOUT` it reads the pool default from `GET /v1/pools` first, as the Action does. It reuses `TOKEN` for every
attempt, so give it a token that outlives that window. The script prints the cache
flags to stderr and exports them to the command as `$CACHE_ARGS`. A build that the pool does not trust to write the cache gets `--cache-from` only (see [Build cache](#build-cache)):

```sh
CACHE_KEY=app-main sh docs/ci/buildkit-allocate.sh \
  sh -c 'docker buildx build --builder "$BUILDER" $CACHE_ARGS .'
```

A killed job skips the trap, and the worker then drains at the allocation's `expiresAt`.
Keep `TOKEN` out of logs (mask it in your CI system). The script handles one pool per run. For multi-platform builders,
use the GitHub Action or repeat the recipe with `docker buildx create --append`.
