# buildkit-controller

GitHub ARC, but for remote BuildKit daemons. The controller keeps a pool of BuildKit workers on your Kubernetes cluster, hands one to each CI job, and deletes it when the job ends. Jobs reach their worker with buildx's remote driver. There is no client CLI.

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## Why

Long-lived BuildKit daemons share caches and CPU between unrelated jobs, and a compromised build can reach the next one. Per-job builders on a shared cluster fix that, but you then need warm capacity, queueing, expiry and certificates. buildkit-controller does that work.

- Each build gets its own worker pod. No other build runs on it, and the controller deletes the pod when the allocation ends.
- A pool keeps `scaling.min` warm pods (zero by default, or a time-of-day schedule) and grows to `scaling.max`. When the pool is full, requests wait in one first-in, first-out queue per pool, up to the pool's `queueTimeout`. The queue is a list of `BuildKitClaim` objects, so it survives restarts and `kubectl get bkc -A` shows it. No HTTP request stays open longer than 25 seconds: the client polls for its place.
- One install serves any number of pools in the namespaces you list in `watchNamespaces`, so amd64, arm64 and per-team pools live side by side. Outside them, its own namespace and `claims.namespaces` the controller can write nothing.
- The GitHub Action allocates builders for one or more pools and releases them when the job finishes, even after a failure.
- Builds get a cache with nothing to set up. The chart runs a small registry that only worker pods can reach, and the allocation response carries the `--cache-from` and `--cache-to` values for it. Each repository has its own cache. Pushes, schedules and manual runs read and write it. Fork pull requests read it and cannot write it, so they start warm and cannot poison it ([details](docs/architecture.md#build-cache)).

## Requirements

The build sandbox depends on these cluster settings. Without an enforcing CNI or the IPv6 cluster ranges, pools still report Ready while builds are not isolated or have no IPv6 egress. A Pod Security refusal shows as `Degraded` with reason `PodCreateFailed`.

- A CNI that enforces NetworkPolicy, such as Calico, Cilium, or the AWS VPC CNI with network policy turned on. On a CNI that ignores NetworkPolicy, such as kindnet or the default EKS VPC CNI, a build can reach every pod in the cluster and the cloud metadata endpoint.
- Pool namespaces that admit privileged pods under Pod Security Admission. Rootless BuildKit needs unconfined seccomp and AppArmor, which `baseline` and `restricted` refuse. Label each pool namespace `pod-security.kubernetes.io/enforce=privileged` and give each pool its own namespace. If the label is missing, the pool reports `Degraded` with reason `PodCreateFailed` and the PodSecurity error.
- A default StorageClass, or `cache.persistence.storageClass`, for the cache registry's 60Gi volume. Set `cache.enabled=false` to run without a cache.
- On an IPv6 or dual-stack cluster, list the IPv6 pod, service and node ranges in `workerNetwork.clusterCIDRs`, plus any IPv4 pod or node ranges outside RFC 1918 and 100.64.0.0/10. Until it holds an IPv6 range, workers get no IPv6 egress, and on an IPv6-only cluster every image pull fails while the pool reports Ready.

## Quickstart

You need a cluster that meets the requirements above, `helm`, and a GitHub repository that can run Actions. The chart exposes the controller through a Gateway API Gateway you run: an HTTPS listener for the API and a TLS passthrough listener for the edge, with the Gateway API v1.5.0 or later CRDs ([helm/README.md](helm/README.md#exposure) has an example and the implementations that support it). Without one, add `--set service.type=LoadBalancer`. [docs/quickstart.md](docs/quickstart.md) has the kind version.

```sh
# 1. Install the controller and its CRD, attached to the Gateway bk in gateway-system.
helm install bk helm/buildkit-controller -n buildkit-system --create-namespace \
  --set edge.endpoint=tcp://bk.example.com:8444 --set edge.serverName=bk.example.com \
  --set-json 'gateway.api.parentRefs=[{"name":"bk","namespace":"gateway-system","sectionName":"api"}]' \
  --set-json 'gateway.edge.parentRefs=[{"name":"bk","namespace":"gateway-system","sectionName":"edge"}]' \
  --set 'watchNamespaces={ci}' \
  --set 'auth.oidc[0].url=https://token.actions.githubusercontent.com' \
  --set 'auth.oidc[0].audiences={buildkit-controller}'

# 2. Create a namespace and a pool that accepts your GitHub organization.
#    Edit the access rules in the example first: set repository_owner_id to your
#    organization's id (gh api orgs/<org> --jq .id) and replace the sample repository. The
#    first rule allows the main branch only; to allow pull requests, replace its `ref` and `event_name` claims with `event_name: pull_request`.
kubectl create namespace ci
kubectl label namespace ci pod-security.kubernetes.io/enforce=privileged
kubectl apply -f examples/pool-github-oidc.yaml

# 3. Check that the pool is ready.
kubectl get bkp -n ci
```

Then add the Action to a workflow (the job needs `id-token: write`):

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    permissions:
      id-token: write
      contents: read
    steps:
      - uses: actions/checkout@v4
      - uses: smrt-devops/buildkit-controller/action@v0
        with:
          url: https://bk.example.com
          pools: ci/amd64
      - run: docker buildx build --platform linux/amd64 .
```

Replace `bk.example.com` with a DNS name that points at the Gateway, and use the port of its HTTPS listener in `url` if it is not 443. The Gateway terminates the API's TLS with its own certificate, so with a publicly trusted one the Action needs no `ca-cert`. The edge passes through to the controller, and the Action takes its CA from each allocation.

With `service.type=LoadBalancer`, the API URL is `https://bk.example.com:8443` and the controller serves its own certificate on it. Unless you give it a certificate from a public CA (`edge.tls.secretName`), the Action must trust the controller's CA through `ca-cert: ${{ vars.BK_CA }}`. Store it in a repository or organization variable named `BK_CA`:

```sh
kubectl -n buildkit-system get configmap buildkit-controller-ca-bundle -o 'jsonpath={.data.ca\.crt}' \
  | gh variable set BK_CA --org <org> --visibility all
```

## Security model

- Callers authenticate with a GitHub OIDC token or a Kubernetes ServiceAccount token. Each pool's `spec.access.rules` decides who may use it, and a pool with no rules denies everyone.
- Every allocation gets a client certificate valid for at most `maxTTL` (24 hours by default). The client generates the key and sends only the public half, so the private key never leaves the CI job. A BuildKitClaim is the exception: the controller generates its key and writes it to the claim's Secret in the claim's namespace.
- Each allocation gets its own pod. The pod runs rootless BuildKit behind a deny-by-default NetworkPolicy (enforced only by a NetworkPolicy-aware CNI), and the controller retires it when the allocation ends. Nothing of the pod outlives it: workers have no volumes. The pod runs with seccomp and AppArmor Unconfined, so for untrusted code such as fork pull requests use a dedicated node pool or a sandboxed runtime class ([Running untrusted builds](docs/operations.md#running-untrusted-builds)).
- The build cache lives in a registry that only worker pods reach, over mTLS, with a client certificate that only the registry accepts. The registry reads the calling pod's live allocation and allows a pull from that caller's own cache and a push only when the pool trusts the caller. A build cannot read or write another repository's cache whatever `--cache-to` it passes. A fork pull request reads the cache and cannot write it. A pool decides who may write with `spec.workers.cache.writers`.
- All traffic to a worker goes through the mTLS edge. The edge checks the client certificate, finds the pod in its watch cache, and opens a second mTLS connection to buildkitd, which only trusts the controller's internal CA.
- The controller makes and repairs its own CAs. If the CA Secret goes missing or is corrupted, the leader saves a copy, creates a new one, and the pools recover without operator action.

## Documentation

The documentation site is at <https://smrt-devops.github.io/buildkit-controller/>. The same pages are linked below.

- [docs/quickstart.md](docs/quickstart.md): install, first pool, first build.
- [docs/architecture.md](docs/architecture.md): roles, the CRD, the data path.
- [docs/operations.md](docs/operations.md): sizing, scaling, upgrades, CA rotation, metrics, troubleshooting.
- [helm/README.md](helm/README.md): chart values.
- [action/](action/action.yml) and [docs/ci/](docs/ci/README.md): the GitHub Action and the curl recipe for other CI systems.
- [examples/](examples/README.md): pools and Helm values.
- [BuildKitClaim](docs/architecture.md#buildkitclaim): the object behind every allocation. In-cluster pipelines (Tekton, Argo) can create one to get a builder without the HTTP API.
- [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md).

## License

Apache 2.0. See [LICENSE](LICENSE).
