# Quickstart

This page installs the controller, creates a pool and runs one build with the GitHub Action and with plain curl. You need `kubectl`, `helm` 3, and a cluster on Kubernetes 1.30 or later. The commands install the chart from a checkout of this repository (`helm/buildkit-controller`); the next section shows how to install it from a release instead.

## Requirements

- The cluster's CNI must enforce NetworkPolicy. The worker sandbox is a NetworkPolicy, and a CNI that ignores it, such as kindnet or the default EKS VPC CNI, lets a build reach every pod and the cloud metadata endpoint. The pool still reports Ready, so check your CNI before you run untrusted builds.
- Each pool namespace must admit privileged pods under Pod Security Admission, because rootless BuildKit needs unconfined seccomp and AppArmor. Step 2 sets the `pod-security.kubernetes.io/enforce=privileged` label. Without it the pool reports `Degraded` with reason `PodCreateFailed` and the PodSecurity error.
- Give each pool its own namespace, with no other workloads and no other NetworkPolicies. NetworkPolicies add up, so any other policy that selects the worker pods widens the sandbox.
- On an IPv6 or dual-stack cluster, list the IPv6 pod, service and node ranges in `workerNetwork.clusterCIDRs`, plus any IPv4 pod or node ranges outside RFC 1918 and 100.64.0.0/10. Until it holds an IPv6 range, workers get no IPv6 egress, and on an IPv6-only cluster every image pull fails while the pool reports Ready.

## 1. Install

The chart is also a release asset. Wherever a command below says `helm install bk helm/buildkit-controller`, you can install the packaged chart instead, without a checkout:

```sh
helm install bk https://github.com/smrt-devops/buildkit-controller/releases/download/v0.1.0/buildkit-controller-0.1.0.tgz \
  -n buildkit-system --create-namespace   # plus the same --set flags as below
```

That release predates some features and fixes in this repository. A setting or behavior these docs describe may need a newer release, so use the chart from a checkout of the matching tag or commit when a `--set` flag is rejected.

### On a real cluster

The chart exposes the controller through a Gateway API Gateway: an HTTPRoute for the API and a TLS passthrough TLSRoute for the edge. You need the Gateway API v1.5.0 or later standard-channel CRDs, an implementation that supports TLSRoute, and a Gateway with two listeners: HTTPS for the API, with a certificate for your DNS name, and TLS in Passthrough mode for the edge. [helm/README.md](../helm/README.md#exposure) has an Envoy Gateway example and the list of implementations. This page assumes a Gateway `bk` in namespace `gateway-system` with the API on 443 and the edge on 8444.

Pick a DNS name, install, then point the name at the Gateway's address.

```sh
helm install bk helm/buildkit-controller -n buildkit-system --create-namespace \
  --set edge.endpoint=tcp://bk.example.com:8444 --set edge.serverName=bk.example.com \
  --set-json 'gateway.api.parentRefs=[{"name":"bk","namespace":"gateway-system","sectionName":"api"}]' \
  --set-json 'gateway.edge.parentRefs=[{"name":"bk","namespace":"gateway-system","sectionName":"edge"}]' \
  --set 'watchNamespaces={ci}' \
  --set 'auth.oidc[0].url=https://token.actions.githubusercontent.com' \
  --set 'auth.oidc[0].audiences={buildkit-controller}'
kubectl get httproute,tlsroute -n buildkit-system
```

`watchNamespaces` lists the namespaces that hold pools (`ci`, created in step 2). The chart grants the controller pod and Secret access in those namespaces only, and the controller ignores pools anywhere else. Leave it out and the controller serves every namespace through a ClusterRole that can create pods and Secrets anywhere, kube-system included. The chart refuses to render without `edge.endpoint`, `edge.serverName` and `gateway.parentRefs`. The API URL is `https://bk.example.com`, and `edge.endpoint` must be the Gateway's edge listener. Without `auth.oidc`, only ServiceAccount tokens authenticate.

Behind a gateway every client reaches the controller from the gateway's addresses, so the per-address caps apply to all clients together. Raise `edge.maxHandshakesPerIp`, `api.maxConnectionsPerIp` and `auth.serviceAccount.maxReviewsPerIp`, or set them to 0, before many jobs start at once (see [operations.md](operations.md#exposure)).

Without a Gateway, or where the managed gateway has no TLS passthrough (GKE's managed Gateway, the AWS Load Balancer Controller), use a load balancer instead: replace the two `parentRefs` flags with `--set service.type=LoadBalancer`, point the DNS name at the Service address from `kubectl get svc -n buildkit-system`, and use `https://bk.example.com:8443` as the API URL. Some load balancers need a higher idle timeout. See [helm/README.md](../helm/README.md#load-balancer-idle-timeout).

### On kind

kind has no load balancer, so this installs without exposure and uses a port-forward and a local name. kind's default CNI, kindnet, does not enforce NetworkPolicy, so this cluster suits a trial with trusted builds only.

```sh
kind create cluster --name bk
cargo xtask image --tag buildkit-controller:dev --load-kind bk
helm install bk helm/buildkit-controller -n buildkit-system --create-namespace \
  --set fullnameOverride=buildkit-controller \
  --set image.repository=buildkit-controller --set image.tag=dev \
  --set gateway.enabled=false --set replicaCount=1 \
  --set 'watchNamespaces={ci}' \
  --set edge.endpoint=tcp://localhost:8444 --set edge.serverName=localhost
kubectl -n buildkit-system port-forward svc/buildkit-controller 8443:8443 8444:8444 &
```

## 2. Create a pool

```sh
kubectl create namespace ci
kubectl label namespace ci pod-security.kubernetes.io/enforce=privileged
kubectl -n ci create serviceaccount builder
kubectl apply -f - <<'YAML'
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata:
  name: dev
  namespace: ci
spec:
  access:
    rules:
      - serviceAccount:
          name: builder
YAML
kubectl get bkp -n ci
```

`READY` turns `True` when the spec is valid, the controller has a CA and the pool's sandbox objects (ServiceAccount and NetworkPolicy) are applied. With the default `scaling.min: 0` the pool starts no pods until the first request, so the first build waits for one pod start (up to `workers.startTimeout`, 2 minutes by default). Set `scaling.min: 1` to keep one warm.

A caller that matches no rule gets 404 `pool_not_found`.

## 3. First build with the GitHub Action

Add this to a workflow in a repository the pool allows. The pool above has only the ServiceAccount rule, so for GitHub Actions apply `examples/pool-github-oidc.yaml` (edit its claims first, and use `pools: ci/amd64`) and keep the `auth.oidc` issuer from step 1.

```yaml
permissions:
  id-token: write
  contents: read
steps:
  - uses: actions/checkout@v4
  - uses: smrt-devops/buildkit-controller/action@v0
    with:
      url: https://bk.example.com   # https://bk.example.com:8443 with a LoadBalancer
      pools: ci/amd64
      # ca-cert: ${{ vars.BK_CA }}   # only when the API certificate is not publicly trusted; see below
  - run: docker buildx build .
```

The inputs are:

| Input | Meaning |
|---|---|
| `url` | Controller API base URL. Required. |
| `pools` | Pools to allocate, one `namespace/pool` per entry. Required. |
| `ttl` | Allocation TTL, for example `3600`, `90m` or `1h`. Defaults to the pool's. |
| `queue-timeout` | How long to wait in the queue, for example `300` or `5m`. Defaults to the pool's. It also bounds the retries (see [Retries](ci/README.md#retries)). |
| `audience` | OIDC token audience. Default `buildkit-controller`. |
| `builder-name` | Name of the buildx builder. Default `bk-<first allocation id>`. |
| `ca-cert` | PEM CA to trust for the API listener. Default the system roots. |
| `cache` | `auto` (default, every layer the pool enables), `registry`, `local` or `none`. |
| `cache-key` | Cache key for both layers. Defaults to the pool's registry `defaultKey`, else the pool name. |
| `cache-mode` | `max` or `min`, replaces the pool's mode in `cache-to`. |
| `cache-from` | Extra `--cache-from` values, one per line. |

The outputs are `builder`, `platforms` (the union of the pools' platforms), `allocation-ids`, `cache-to` and `cache-from` (one value per line, for `docker/build-push-action`) and `cache-args` (the same values as flags for `docker buildx build`). The cache inputs and outputs are explained in [Using it from CI](ci/README.md#build-cache). The post step removes the builder and releases every allocation even when the job fails.

Behind the Gateway, the API presents the Gateway's certificate. When that certificate is publicly trusted, the Action needs no `ca-cert`. The edge always passes through to the controller, and the Action trusts the CA that comes with each allocation.

With a LoadBalancer, or `gateway.api.mode: passthrough`, the API serves the controller's own certificate, issued by its client CA. To trust it, read the CA from the ConfigMap the leader maintains and store it in the `BK_CA` variable that the workflow passes as `ca-cert`.

```sh
kubectl -n buildkit-system get configmap buildkit-controller-ca-bundle -o 'jsonpath={.data.ca\.crt}' \
  | gh variable set BK_CA --repo <owner>/<repo>
```

To skip this step, give the controller a certificate from a public CA through `edge.tls.secretName`. If the Gateway's own certificate comes from a private CA, store that CA in `BK_CA` instead.

### Choosing access rules

The GitHub OIDC rule in `examples/pool-github-oidc.yaml` decides which workflows can use the pool. Match on ids, not names:

- Prefer `repository_id` and `repository_owner_id` over `repository` and `repository_owner`. A name can be re-registered after a rename or deletion by someone else, but an id cannot.
- A `ref` rule also matches `pull_request_target` runs. For that event GitHub sets `ref` to the base branch, for example `refs/heads/main`, even though the workflow can check out and run code from the pull request. Add `event_name` (for example `push`) to the same rule to exclude it. `job_workflow_ref` excludes the event only when the named workflow has no `pull_request_target` trigger, because a run of that workflow on the base branch carries the same value for every event.

## 4. First build with curl

The script `docs/ci/buildkit-allocate.sh` needs `curl`, `jq`, `openssl` and `docker buildx`. It generates a P-256 key, sends only the public key, creates a remote builder, runs your command, and releases the allocation on exit.

On a cluster, a ServiceAccount token is the simplest credential. The pool above already allows the `builder` ServiceAccount. Run these commands.

```sh
export TOKEN=$(kubectl -n ci create token builder --audience buildkit-controller)
export API=https://bk.example.com POOL=ci/dev   # :8443 with a LoadBalancer, https://localhost:8443 on kind
# Only when the API serves the controller's certificate (LoadBalancer, passthrough, kind):
kubectl -n buildkit-system get configmap buildkit-controller-ca-bundle -o 'jsonpath={.data.ca\.crt}' > /tmp/bk-ca.pem
export API_CA=/tmp/bk-ca.pem
sh docs/ci/buildkit-allocate.sh sh -c 'docker buildx build --builder "$BUILDER" .'
```

The `TTL` and `QUEUE_TIMEOUT` variables (seconds) are optional. Mask `TOKEN` in any CI log.

## Clean up

```sh
helm uninstall bk -n buildkit-system
kubectl delete bkp --all -A
kind delete cluster --name bk
```

Helm does not remove the CRD. Delete it with `kubectl delete crd buildkitpools.buildkit.smrt-devops.net` when you no longer need pools.
