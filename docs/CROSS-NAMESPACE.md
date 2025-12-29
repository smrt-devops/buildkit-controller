# Cross-Namespace Support

BuildKit Controller supports cross-namespace pools, workers, and certificates for multi-tenant scenarios.

## Pool References

### Workers

Workers can reference pools in different namespaces using the `PoolReference`:

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitWorker
metadata:
  name: worker-1
  namespace: tenant-a
spec:
  poolRef:
    name: shared-pool
    namespace: buildkit-system # Cross-namespace reference
```

### API Requests

When requesting certificates via the API, use the same format:

```bash
curl -X POST http://localhost:8082/api/v1/certs/request \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "pools": [
      "my-pool",
      "buildkit-system/shared-pool",
      "tenant-b/private-pool"
    ]
  }'
```

## Status Information

The `BuildKitPool` status now includes detailed information:

### Workers Status

```yaml
status:
  workers:
    total: 5 # Total number of workers
    ready: 4 # Ready workers (idle + allocated)
    idle: 3 # Idle (unallocated) workers
    allocated: 1 # Currently allocated workers
    provisioning: 1 # Workers being provisioned
    failed: 0 # Failed workers
```

### Connections Status

```yaml
status:
  connections:
    active: 2 # Current active connections
    total: 150 # Total connections (from metrics)
    lastConnectionTime: "2025-12-18T23:00:00Z"
```

### Gateway Status

```yaml
status:
  gateway:
    ready: true
    replicas: 2
    readyReplicas: 2
    deploymentName: my-pool-gateway
    serviceName: my-pool
```

## kubectl Output

The CRD includes improved print columns:

```bash
$ kubectl get buildkitpool

NAME           PHASE    GATEWAY   WORKERS   READY   ALLOCATED   CONNECTIONS   AGE
minimal-pool   Running  true      5         4       1          2             1h
shared-pool    Running  true      10        8       2          5             2h
```

## Cross-Namespace Considerations

### RBAC

Ensure proper RBAC permissions for cross-namespace access:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: buildkit-pool-reader
rules:
  - apiGroups: ["buildkit.smrt-devops.net"]
    resources: ["buildkitpools"]
    verbs: ["get", "list"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: buildkit-cross-namespace-access
subjects:
  - kind: ServiceAccount
    name: buildkit-controller
    namespace: buildkit-system
roleRef:
  kind: ClusterRole
  name: buildkit-pool-reader
  apiGroup: rbac.authorization.k8s.io
```

### OIDC Configuration

OIDC configs are namespace-scoped but can be referenced by pools in any namespace. The controller will search for OIDC configs across namespaces when verifying tokens.

### Network Policies

If using NetworkPolicies, ensure they allow cross-namespace communication:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: allow-cross-namespace-pools
  namespace: buildkit-system
spec:
  podSelector:
    matchLabels:
      buildkit.smrt-devops.net/pool: shared-pool
  policyTypes:
    - Ingress
  ingress:
    - from:
        - namespaceSelector:
            matchLabels:
              name: tenant-a
        - namespaceSelector:
            matchLabels:
              name: tenant-b
```

## Examples

### Multi-Tenant Setup

```yaml
# Shared pool in buildkit-system
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata:
  name: shared-pool
  namespace: buildkit-system
spec:
  # ... pool config

---
# Tenant-specific pool
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata:
  name: tenant-pool
  namespace: tenant-a
spec:
  # ... pool config

---
# API request accessing both pools
# Use the API to request certificates for cross-namespace pools:
curl -X POST http://localhost:8082/api/v1/certs/request \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "pools": [
      "tenant-pool",
      "buildkit-system/shared-pool"
    ]
  }'
```
