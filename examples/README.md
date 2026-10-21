| [claim-pod.yaml](claim-pod.yaml) | A `BuildKitClaim` and a pod that runs `buildctl` with the Secret it gets. Needs `claims.enabled: true`. |
# Examples

Every pool example uses the `ci` namespace, which you create first and label for privileged pods (`kubectl create namespace ci` and `kubectl label namespace ci pod-security.kubernetes.io/enforce=privileged`). The cluster's CNI must enforce NetworkPolicy; see the README requirements.

| File | What it shows |
|---|---|
| [pool-minimal.yaml](pool-minimal.yaml) | The smallest pool. It denies every caller until you add access rules. |
| [pool-template.yaml](pool-template.yaml) | Node selector, tolerations, resources, a weekday warm schedule and TTLs. |
| [pool-multiplatform-amd64.yaml](pool-multiplatform-amd64.yaml), [pool-multiplatform-arm64.yaml](pool-multiplatform-arm64.yaml) | Two single-platform pools that the Action combines into one builder. |
| [pool-github-oidc.yaml](pool-github-oidc.yaml) | Access rules for GitHub OIDC claims and a ServiceAccount. |
| [pool-gitlab-oidc.yaml](pool-gitlab-oidc.yaml) | Access rules for a GitLab or other generic OIDC issuer. |
| [pool-cache.yaml](pool-cache.yaml) | Both cache layers: a registry cache ref and two local volumes. |
| [pool-registry-cache.md](pool-registry-cache.md) | Passing the registry cache flags to `docker buildx build`. |
| [helm-values.yaml](helm-values.yaml) | Chart values with an OIDC issuer, load balancer annotations and a pool. |

A multi-platform build needs `edge.allocationDomain` set in the Helm values, with wildcard DNS for that domain pointing at the edge address. See [docs/operations.md](../docs/operations.md).

The field reference is [generated CRD](../helm/buildkit-controller/crds/buildkitpools.yaml) (`kubectl explain bkp.spec` prints the same text). Check them with `kubectl apply --dry-run=server` against a cluster that has the CRD installed. `pool-github-oidc.yaml` and `pool-gitlab-oidc.yaml` need a matching `auth.oidc` issuer in the Helm values, or the pool shows `Degraded=True/UnknownIssuer`.
