# buildkit-controller chart

Installs the controller, its CRDs and RBAC, and optionally one `BuildKitPool`
per `pools` entry. `values.yaml` documents every value and
`values.schema.json` validates them. The operator guide is
[docs/operations.md](../../docs/operations.md).

```sh
helm install bk helm/buildkit-controller -n buildkit-system --create-namespace \
  --set edge.endpoint=tcp://bk.example.com:8444 --set edge.serverName=bk.example.com \
  --set-json 'gateway.api.parentRefs=[{"name":"bk","namespace":"gateway-system","sectionName":"api"}]' \
  --set-json 'gateway.edge.parentRefs=[{"name":"bk","namespace":"gateway-system","sectionName":"edge"}]'
```

## Exposure

By default the Service is ClusterIP and the chart attaches Gateway API routes to
the Gateway listeners in `gateway.api.parentRefs` and `gateway.edge.parentRefs`: an HTTPRoute with a BackendTLSPolicy for the
API (`gateway.api.mode: passthrough` makes it a TLSRoute) and a TLS passthrough
TLSRoute for the edge. They need the Gateway API v1.5.0 or later standard-channel
CRDs. Without `gateway.parentRefs` the chart fails to render, unless
`service.type: LoadBalancer` exposes the Service directly or `gateway.enabled: false`
keeps the install in-cluster. `gateway.envoy.enabled` adds Envoy Gateway policies
(with `gateway.envoy.proxyProtocol`, PROXY protocol to the listeners whose
`api.proxyProtocol` or `edge.proxyProtocol` is on), and `gateway.extraManifests` takes objects for other implementations. Modes,
listeners, timeouts and supported implementations are in
[helm/README.md](../README.md#exposure).

## Requirements

The build sandbox depends on these cluster settings. Without an enforcing CNI or the IPv6 cluster ranges, pools still report Ready while builds are not isolated or have no IPv6 egress. A Pod Security refusal shows as `Degraded` with reason `PodCreateFailed`.

- The cluster's CNI must enforce NetworkPolicy. The worker sandbox is a NetworkPolicy. kindnet and the default EKS VPC CNI ignore it, and on them a build reaches every pod and the cloud metadata endpoint.
- Each pool namespace must admit privileged pods under Pod Security Admission. Label it `pod-security.kubernetes.io/enforce=privileged`. Rootless BuildKit needs unconfined seccomp and AppArmor, which `baseline` and `restricted` refuse.
- Give each pool its own namespace, with no other workloads or NetworkPolicies, and never the release namespace, which holds the CA Secret. NetworkPolicies add up, so another policy that selects the worker pods widens the sandbox.
- On an IPv6 or dual-stack cluster, list the IPv6 pod, service and node ranges in `workerNetwork.clusterCIDRs`, plus any IPv4 pod or node ranges outside RFC 1918 and 100.64.0.0/10. Until it holds an IPv6 range, workers get no IPv6 egress, so on an IPv6-only cluster image pulls fail.

## Pools

Each `pools` item is `{name, namespace, spec}`. The chart copies `spec`
into the pool unchanged. `namespace` defaults to the only entry of `watchNamespaces`, must be listed in
`watchNamespaces` when that is set, and must not be the release namespace.

## Build cache

`cache.enabled` (on by default) runs a cache registry: a one-replica
StatefulSet with one volume (`cache.persistence`) that only worker pods reach,
over mTLS with the internal CA. Allocations carry `--cache-to` and
`--cache-from` flags for it. A caller that a pool's `spec.workers.cache.writers`
rules do not trust gets `--cache-from` only; by default pushes, schedules,
`workflow_dispatch`, `merge_group` and in-cluster callers write, and pull
requests read. Each repository has its own cache unless the pool sets
`spec.workers.cache.scope: pool`. `cache.maxSize` bounds the volume and
`cache.poolMaxSize` each pool without `spec.workers.cache.maxSize`. The volume
needs room for `cache.maxSize` plus `cache.maxUploadSize`.

The controller's RBAC keeps `list` and `delete` on `persistentvolumeclaims`
wherever it manages pools for one release, to remove the cache volumes of v0.1.0.
