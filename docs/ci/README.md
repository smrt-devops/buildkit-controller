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

1. `POST /v1/allocations` creates the allocation, named after the request's public key. If a worker binds within about 20 seconds, the answer is `201` with the full response, so a warm pool costs one round trip. A client that loses the answer sends the same `POST` again and finds the same allocation, in the same place in the queue.
2. Otherwise the answer is `202` with `{"id", "state": "pending", "position"}`. The allocation is queued in the pool. The client keeps the id.
3. The client calls `GET /v1/allocations/{id}?wait=25s` until it gets an answer. `200` is the full response. `202` means still pending, and the client asks again at once. No request stays open longer than 25 seconds, so gateway and load-balancer timeouts never cut the wait, and a client that loses its connection asks again and keeps its place.
4. When the build ends, `DELETE /v1/allocations/{id}` releases the allocation.

The whole allocation, the first `POST`, every poll and any new `POST`, shares one budget: the queue timeout plus 60 seconds. The queue timeout is `queue-timeout` (`QUEUE_TIMEOUT` for the script), or, when that is unset, the pool's default `queueTimeoutSeconds` from `GET /v1/pools`. A controller too old to report the default leaves the budget at 1 hour (the largest queue timeout a pool allows) plus 60 seconds. A poll has a 40 second timeout. A `POST` waits for the whole remaining budget, so a v0.1.0 controller, which holds the request open through the queue wait, still works.

### Retries

The client makes a new key pair for each allocation, and the public key names it: the controller derives the allocation id from the key. A `POST` repeated with the same key, by the same caller, finds the allocation the first one made and answers for it as for a new one (`201` when bound, otherwise `202` after up to 20 seconds). It never makes a second allocation. So a `POST` whose answer is lost, through a reset, a timeout or a `5xx` from a gateway, is safe to send again, and the allocation keeps its place in the queue. The same key from another caller gets `403 forbidden`. Retry within 3 minutes: the controller deletes an allocation whose caller has not asked for it (by `POST` or `GET`) for 3 minutes, unless it already answered with the bound allocation. The Action and the script never leave a gap that long while they keep trying. That frees the place and the worker of a job that was cancelled before it learned its id.

Which answers the client repeats, and which end the step:

| Answer | Client does |
|---|---|
| `201` on `POST`, `200` on `GET` | Done. |
| `202` | Poll the id again. |
| Connection refused, reset or timed out on `POST`, and any other `5xx` on `POST` | Repeat the `POST` with the same key after `Retry-After`, else after 1, 2, 4 and up to 30 seconds. |
| `503 pool_exhausted` on `POST` (`Retry-After: 5`) | The allocation hit its queue timeout and that key is spent. Wait, then `POST` again with a new key while the budget lasts. |
| `503 pool_exhausted` on `GET` | The claim hit its queue timeout and the id is dead. Same as above: a new `POST` with a new key while the budget lasts, then fail. |
| `503 upstream_error` (`Retry-After: 2`) | The controller could not reach the Kubernetes API or the issuer's keys (token review, JWKS). Repeat the request after `Retry-After`, on `POST`, `GET` and `DELETE`. |
| Other `5xx` on `GET`, and connection failures while polling | Poll the same id again after `Retry-After`, else 2 seconds. |
| `503 pod_lost` | The worker is gone. The id is dead. Fail the step. |
| `404 not_found` and `403 forbidden` on `GET`, other `4xx` | Fail the step. |

Every retry stops once the next wait would pass the budget. The client waits the `Retry-After` seconds (at least 1, at most 30), logs each retry, and the Action fetches a fresh OIDC token for every attempt. A retry asks for just the time left in the budget, so the total never doubles. With several pools, each pool allocates on its own; if one pool fails, the allocations that succeeded are released.

Releases (`DELETE`; a 404 counts as released) and the pool listing (`GET /v1/pools`) retry connection failures and `5xx` the same way, since repeating them is safe. They have their own budget of 60 seconds, because neither queues.

A v0.1.0 controller does not derive the id from the key, so a `POST` it received but whose answer was lost makes a second allocation when repeated. The first one ends at its TTL.

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
