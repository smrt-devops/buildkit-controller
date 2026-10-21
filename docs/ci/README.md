# Using buildkit-controller from CI

Clients use the buildx remote driver, and there is no CLI. The GitHub Action and the `buildkit-allocate.sh` script below both call `POST /v1/allocations` and `DELETE /v1/allocations/{id}`, and the response carries the endpoint, the CA, the allocation certificate and the cache flags.

## GitHub Actions

See `action/`. The job needs `permissions: id-token: write`.

```yaml
- uses: smrt-devops/buildkit-controller/action@v0
  with:
    url: https://bk.example.com:8443
    pools: |
      ci/amd64
      ci/arm64        # more than one pool needs edge.allocationDomain and distinct pool platforms
- run: docker buildx build --platform linux/amd64,linux/arm64 .
```

The post step removes the builder and releases every allocation, even when the job fails.

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
    url: https://bk.example.com:8443
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
export API=https://bk.example.com:8443 POOL=ci/amd64
export TOKEN=...   # OIDC token or service-account token with the controller audience, e.g.
                   # kubectl create token builder --audience buildkit-controller
sh docs/ci/buildkit-allocate.sh sh -c 'docker buildx build --builder "$BUILDER" .'
```

Optional variables are `TTL` and `QUEUE_TIMEOUT` (seconds), `API_CA` (PEM file for the API listener),
`CACHE_KEY` and `CACHE` (the Action's `cache-key` and `cache` inputs). The script prints the registry cache
flags to stderr and exports them to the command as `$CACHE_ARGS`:

```sh
CACHE_KEY=app-main sh docs/ci/buildkit-allocate.sh \
  sh -c 'docker buildx build --builder "$BUILDER" $CACHE_ARGS .'
```

A killed job skips the trap, and the worker then drains at the allocation's `expiresAt`.
Keep `TOKEN` out of logs (mask it in your CI system). The script handles one pool per run. For multi-platform builders,
use the GitHub Action or repeat the recipe with `docker buildx create --append`.
