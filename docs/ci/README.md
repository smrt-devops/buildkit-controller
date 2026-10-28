# Using buildkit-controller from CI

Clients use the buildx remote driver, and there is no CLI. The GitHub Action and the `buildkit-allocate.sh` script below both call `POST /v1/allocations` and `DELETE /v1/allocations/{id}`, and the response carries the endpoint, the CA, the allocation certificate and the cache flags.

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

### Retries

The controller answers `503` with a `Retry-After` header when it holds no claim for the request: the pool
is exhausted or its queue is full (`pool_exhausted`), a worker did not start in time (`start_timeout`),
token review is unavailable, or a replica is shutting down (`shutting_down`). Other 503 answers, such as
an unavailable CA, carry no `Retry-After` and are not retried. The Action retries `POST /v1/allocations` on
exactly that, and on connection errors raised before the request was sent (connection refused, DNS
failure). It waits the `Retry-After` seconds (at least 1, at most 30), logs each retry, and fetches a
fresh OIDC token for every attempt. Retries stop after the queue timeout plus 60 seconds and the step then fails with the last error. The
queue timeout is `queue-timeout`, or, when that is unset, the pool's default `queueTimeoutSeconds` from
`GET /v1/pools`. A pool that was exhausted answered only after the full queue timeout, so each retry asks
for just the time left in that bound (the queue timeout less the time already spent) and the total does
not double. A controller too old to report the pool default gets no `queueTimeoutSeconds` from the
Action, and the bound is then 1 hour (the largest queue timeout a pool allows) plus 60 seconds.

Nothing else is retried: other statuses, a 503 without `Retry-After`, a connection reset after the request
was sent and a timeout waiting for the response all fail the step, because the controller may have
claimed a worker whose id the client never saw. With several pools, each pool retries on its own; if one
pool still fails, the allocations that succeeded are released. Releases (`DELETE`) retry a 503 with
`Retry-After` under the same limits, since a repeated release is safe.

### Build cache

Workers are single-use, so a build gets cache only from the pool's cache layers
(see [Build cache](../architecture.md#build-cache)). The controller returns registry cache flags in
the allocation response, and the Action turns them into outputs. A build that
does not pass them on runs without the registry layer.

| Input | Meaning |
|---|---|
| `cache` | `auto` (default, every layer the pool enables), `registry`, `local` or `none` |
| `cache-key` | key of both layers, for example the repository name. Defaults to the pool's `defaultKey` |
| `cache-mode` | `max` or `min`, replaces the pool's mode in `cache-to` |
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
    cache-key: ${{ github.event.repository.name }}
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

Buildx pushes and pulls the cache with the job's own registry login, so log in
to the cache repository (for example with `docker/login-action`) before the build.
The local layer needs no flags. It works through the worker the build lands on.

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
under the same rules and limits as the Action (see Retries above): a 503 with `Retry-After`, or a curl
failure to resolve or connect, for at most the queue timeout plus 60 seconds, shrinking the queue timeout it sends on each retry.
Without `QUEUE_TIMEOUT` it reads the pool default from `GET /v1/pools` first, as the Action does. It reuses `TOKEN` for every
attempt, so give it a token that outlives that window. The script prints the registry cache
flags to stderr and exports them to the command as `$CACHE_ARGS`:

```sh
CACHE_KEY=app-main sh docs/ci/buildkit-allocate.sh \
  sh -c 'docker buildx build --builder "$BUILDER" $CACHE_ARGS .'
```

A killed job skips the trap, and the worker then drains at the allocation's `expiresAt`.
Keep `TOKEN` out of logs (mask it in your CI system). The script handles one pool per run. For multi-platform builders,
use the GitHub Action or repeat the recipe with `docker buildx create --append`.
