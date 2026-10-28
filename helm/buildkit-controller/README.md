# buildkit-controller chart

Installs the controller, its CRDs and RBAC, and optionally one `BuildKitPool`
per `pools` entry. `values.yaml` documents every value and
`values.schema.json` validates them. The operator guide is
[docs/operations.md](../../docs/operations.md).

```sh
helm install bk helm/buildkit-controller -n buildkit-system --create-namespace \
  --set edge.endpoint=tcp://bk.example.com:8444 --set edge.serverName=bk.example.com \
  --set 'gateway.parentRefs[0].name=bk' --set 'gateway.parentRefs[0].namespace=gateway-system'
```

## Exposure

By default the Service is ClusterIP and the chart attaches Gateway API routes to
the Gateway in `gateway.parentRefs`: an HTTPRoute with a BackendTLSPolicy for the
API (`gateway.api.mode: passthrough` makes it a TLSRoute) and a TLS passthrough
TLSRoute for the edge. They need the Gateway API v1.5.0 or later standard-channel
CRDs. Without `gateway.parentRefs` the chart fails to render, unless
`service.type: LoadBalancer` exposes the Service directly or `gateway.enabled: false`
keeps the install in-cluster. `gateway.envoy.enabled` adds Envoy Gateway policies,
and `gateway.extraManifests` takes objects for other implementations. Modes,
listeners, timeouts and supported implementations are in
[helm/README.md](../README.md#exposure).

## Pools

Each `pools` item is `{name, namespace, spec, cache}`. The chart copies `spec`
into the pool unchanged. `namespace` defaults to the only entry of `watchNamespaces`, must be listed in
`watchNamespaces` when that is set, and must not be the release namespace.

## Build cache

A pool has two cache layers ([details](../../docs/architecture.md#build-cache)):

- `cache.registry` is the primary layer. Buildx imports and exports a registry
  ref per cache key, so concurrent builds of one key all share it.
- `cache.local` keeps a PVC per warm slot. It speeds up only the one build that
  lands on that slot, and is off unless set.

The chart merges a pool item's `cache` over its `spec.workers.cache`.
`cacheDefaults.registry.repository` goes into every pool that sets no registry
repository, which turns the registry layer on for it. A pool opts out with
`cache: {registry: {enabled: false}}`.

```yaml
cacheDefaults:
  registry: {repository: ghcr.io/org/buildkit-cache}
pools:
  - name: amd64
    namespace: ci-builders
    cache:
      registry: {importFrom: [app-main]}   # repository comes from cacheDefaults
      local: {size: 50Gi, maxVolumes: 4}
    spec:
      scaling: {min: 2, max: 10}
      access:
        rules:
          - oidc: {issuer: https://token.actions.githubusercontent.com, claims: {repository_owner: org}}
```

The controller holds no registry credentials. CI jobs log in to the cache
repository themselves, and buildx pushes with that login. Every caller a pool
admits can write cache that later callers read, so enable cache only on pools
whose rules admit one trust domain.

The controller's RBAC already grants `get`, `list`, `create`, `patch` and
`delete` on `persistentvolumeclaims` wherever it manages pools, for the local layer.
