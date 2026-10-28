# Quickstart

This page installs the controller, creates a pool and runs one build with the GitHub Action and with plain curl. You need `kubectl`, `helm` 3, and a cluster on Kubernetes 1.30 or later. Check out this repository first, because the chart is installed from `helm/buildkit-controller`.

## Requirements

- The cluster's CNI must enforce NetworkPolicy. The worker sandbox is a NetworkPolicy, and a CNI that ignores it, such as kindnet or the default EKS VPC CNI, lets a build reach every pod and the cloud metadata endpoint. The pool still reports Ready, so check your CNI before you run untrusted builds.
- Each pool namespace must admit privileged pods under Pod Security Admission, because rootless BuildKit needs unconfined seccomp and AppArmor. Step 2 sets the `pod-security.kubernetes.io/enforce=privileged` label. Without it the pool reports `Degraded` with reason `PodCreateFailed` and the PodSecurity error.
- Give each pool its own namespace, with no other workloads and no other NetworkPolicies. NetworkPolicies add up, so any other policy that selects the worker pods widens the sandbox.
- On an IPv6 or dual-stack cluster, list the IPv6 pod, service and node ranges in `workerNetwork.clusterCIDRs`, plus any IPv4 pod or node ranges outside RFC 1918 and 100.64.0.0/10. Until it holds an IPv6 range, workers get no IPv6 egress, and on an IPv6-only cluster every image pull fails while the pool reports Ready.

## 1. Install

### On a real cluster

Pick a DNS name for the load balancer, install, then point the name at the Service address.

```sh
helm install bk helm/buildkit-controller -n buildkit-system --create-namespace \
  --set edge.endpoint=tcp://bk.example.com:8444 --set edge.serverName=bk.example.com \
  --set 'auth.oidc[0].url=https://token.actions.githubusercontent.com' \
  --set 'auth.oidc[0].audiences={buildkit-controller}'
kubectl get svc -n buildkit-system
```

The chart refuses to render without `edge.endpoint` and `edge.serverName`. The API listens on 8443 and the edge on 8444. Without `auth.oidc`, only ServiceAccount tokens authenticate. Some load balancers need a higher idle timeout. See [helm/README.md](../helm/README.md).

### On kind

kind has no load balancer, so use a port-forward and a local name. kind's default CNI, kindnet, does not enforce NetworkPolicy, so this cluster suits a trial with trusted builds only.

```sh
kind create cluster --name bk
cargo xtask image --tag buildkit-controller:dev --load-kind bk
helm install bk helm/buildkit-controller -n buildkit-system --create-namespace \
  --set fullnameOverride=buildkit-controller \
  --set image.repository=buildkit-controller --set image.tag=dev \
  --set service.type=ClusterIP --set replicaCount=1 \
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

`READY` turns `True` when the pool's sandbox objects and certificates exist. With the default `scaling.min: 0` the pool starts no pods until the first request, so the first build waits for one pod start (up to `workers.startTimeout`, 2 minutes by default). Set `scaling.min: 1` to keep one warm.

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
      url: https://bk.example.com:8443
      pools: ci/amd64
      ca-cert: ${{ vars.BK_CA }}   # the controller's CA; see below
  - run: docker buildx build .
```

Inputs are `url`, `pools` (one `namespace/pool` per entry), `ttl`, `queue-timeout`, `audience` (default `buildkit-controller`), `builder-name` and `ca-cert`. The outputs are `builder`, `platforms` and `allocation-ids`. The post step removes the builder and releases every allocation even when the job fails.

By default the edge serves a certificate issued by the controller's client CA. To trust it, read the CA from the ConfigMap the leader maintains and store it in the `BK_CA` variable that the workflow passes as `ca-cert`.

```sh
kubectl -n buildkit-system get configmap buildkit-controller-ca-bundle -o 'jsonpath={.data.ca\.crt}' \
  | gh variable set BK_CA --repo <owner>/<repo>
```

Use a certificate from a public CA, through `edge.tls.secretName`, to skip this step.

## 4. First build with curl

The script `docs/ci/buildkit-allocate.sh` needs `curl`, `jq`, `openssl` and `docker buildx`. It generates a P-256 key, sends only the public key, creates a remote builder, runs your command, and releases the allocation on exit.

On a cluster, a ServiceAccount token is the simplest credential. The pool above already allows the `builder` ServiceAccount. Run these commands.

```sh
export TOKEN=$(kubectl -n ci create token builder --audience buildkit-controller)
export API=https://bk.example.com:8443 POOL=ci/dev   # https://localhost:8443 on kind
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
