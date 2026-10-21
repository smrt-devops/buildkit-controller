# Registry cache

Workers are single-use, so cache comes from a registry or from the pool's local
volumes. [pool-cache.yaml](pool-cache.yaml) turns both on. The controller
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

The worker needs egress to the registry, which the default pool network policy allows for public addresses. For a registry on a private address, add its CIDR to `spec.network.egress.allowCIDRs`.
