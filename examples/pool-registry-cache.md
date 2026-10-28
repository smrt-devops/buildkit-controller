# Registry cache

Each allocation gets a new worker pod, so cache comes from a registry or from
the pool's local volumes, which are kept per cache scope.
[pool-cache.yaml](pool-cache.yaml) turns both on. The controller
returns the registry flags in each allocation response, and the GitHub Action
writes them to its `cache-from`, `cache-to` and `cache-args` outputs
([docs/ci/README.md](../docs/ci/README.md)).

```sh
docker buildx build ${CACHE_ARGS} --push -t ghcr.io/example/app:latest .
# with CACHE_ARGS from the Action's cache-args output, or from buildkit-allocate.sh, e.g.
#   --cache-from=type=registry,ref=ghcr.io/example/buildkit-cache:app-feature
#   --cache-from=type=registry,ref=ghcr.io/example/buildkit-cache:app-main
#   --cache-to=type=registry,ref=ghcr.io/example/buildkit-cache:app-feature,mode=max,image-manifest=true,oci-mediatypes=true
```

A pool without `cache.registry` returns no flags. A client can still pass its
own `--cache-from` and `--cache-to` values.

The registry cache is not scoped to a caller. Every caller the pool admits can
name any `cacheKey`, and anyone who can push to the repository can overwrite
its refs. Use it on pools whose access rules admit one trust domain.

The worker needs egress to the registry, which the default pool network policy allows for public addresses. For a registry on a private address, add its CIDR to `spec.network.egress.allowCIDRs`. The controller refuses a range that overlaps the cluster's own pod, service or node ranges, link-local or loopback; a registry inside the cluster needs its range in the operator's `workerNetwork.permittedCIDRs` (see [Worker egress](../docs/operations.md#worker-egress)).
