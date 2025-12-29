# BuildKit Controller Helm Chart

This Helm chart deploys the BuildKit Kubernetes Operator to your cluster.

## Prerequisites

- Kubernetes 1.24+
- Helm 3.0+
- kubectl configured to access your cluster

## Installation

### Quick Start

```bash
# Add the repository (if using a chart repository)
helm repo add buildkit-controller https://charts.example.com
helm repo update

# Install with default values
helm install buildkit-controller buildkit-controller/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace
```

### Install from Local Chart

```bash
# Install from local chart directory
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace
```

### Install with Custom Values

```bash
# Install with custom image
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace \
  --set image.registry=my-registry \
  --set image.name=buildkit-controller/controller \
  --set image.tag=v1.0.0
```

## Configuration

### Values File

Create a `values.yaml` file to customize the installation:

```yaml
# values.yaml
image:
  registry: my-registry
  name: buildkit-controller/controller
  tag: v1.0.0

controller:
  replicas: 1
  resources:
    limits:
      cpu: 500m
      memory: 128Mi
    requests:
      cpu: 10m
      memory: 64Mi

oidc:
  configs:
    - name: github-actions
      issuer: https://token.actions.githubusercontent.com
      audience: buildkit-controller
      enabled: true
      claimsMapping:
        user: "actor"
```

Then install:

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  -f values.yaml \
  --namespace buildkit-system \
  --create-namespace
```

## Configuration Reference

### Image Configuration

```yaml
image:
  registry: ghcr.io/smrt-devops
  name: buildkit-controller/controller
  tag: latest
  pullPolicy: IfNotPresent

gateway:
  image:
    registry: ghcr.io/smrt-devops
    name: buildkit-controller/gateway
    tag: latest
    pullPolicy: IfNotPresent
```

**Note:** The `gateway.image` configuration is used as the default gateway image for BuildKitPools that don't specify their own gateway image.

### Controller Configuration

```yaml
controller:
  replicas: 1
  resources:
    limits:
      cpu: 500m
      memory: 128Mi
    requests:
      cpu: 10m
      memory: 64Mi
  metrics:
    enabled: true
    port: 8080
  healthProbe:
    enabled: true
    port: 8081
  api:
    enabled: true
    port: 8082
  leaderElection:
    enabled: true
```

### Gateway API Configuration

The controller API can be exposed using Kubernetes Gateway API. **Important:** The Helm chart creates HTTPRoute resources but does **not** create Gateway resources. You must create and manage your own Gateway resource separately.

```yaml
controller:
  api:
    gatewayAPI:
      enabled: true
      hostname: "api.buildkit.example.com"
      # Reference to your existing Gateway resource
      gatewayRef:
        name: "my-gateway" # Name of your Gateway resource
        namespace: "gateway-system" # Namespace of your Gateway (optional, defaults to same namespace)
```

**Example Gateway resource you need to create:**

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: my-gateway
  namespace: gateway-system
spec:
  gatewayClassName: envoy # or your preferred GatewayClass
  listeners:
    - name: https
      hostname: "api.buildkit.example.com"
      port: 443
      protocol: HTTPS
      tls:
        mode: Terminate
        certificateRefs:
          - name: api-tls-cert
            namespace: gateway-system
```

The HTTPRoute will automatically attach to your Gateway and route traffic to the controller API.

### OIDC Configuration

```yaml
oidc:
  configs:
    - name: github-actions
      issuer: https://token.actions.githubusercontent.com
      audience: buildkit-controller
      enabled: true
      claimsMapping:
        user: "actor"
        pools: "repository"
    - name: gitlab-ci
      issuer: https://gitlab.com
      audience: buildkit-controller
      enabled: true
      claimsMapping:
        user: "sub"
```

### CRD Management

```yaml
crds:
  install: true # Install CRDs with Helm
  keep: false # Keep CRDs when uninstalling
```

**Note:** If `crds.install: false`, you must install CRDs manually:

```bash
kubectl apply -f helm/buildkit-controller/crds
```

## Upgrading

```bash
# Upgrade with new values
helm upgrade buildkit-controller ./helm/buildkit-controller \
  -f values.yaml \
  --namespace buildkit-system

# Upgrade with new image
helm upgrade buildkit-controller ./helm/buildkit-controller \
  --set image.tag=v1.1.0 \
  --set gateway.image.tag=v1.1.0 \
  --namespace buildkit-system
```

## Uninstalling

```bash
# Uninstall the chart
helm uninstall buildkit-controller --namespace buildkit-system

# If crds.keep: false (default), CRDs will be removed
# If crds.keep: true, CRDs will remain
```

## Examples

### Production Deployment

```yaml
# production-values.yaml
image:
  registry: my-registry
  name: buildkit-controller/controller
  tag: v1.0.0
  pullPolicy: Always

gateway:
  image:
    registry: my-registry
    name: buildkit-controller/gateway
    tag: v1.0.0
    pullPolicy: Always

controller:
  replicas: 2
  resources:
    limits:
      cpu: 1000m
      memory: 256Mi
    requests:
      cpu: 100m
      memory: 128Mi
  leaderElection:
    enabled: true

podDisruptionBudget:
  enabled: true
  minAvailable: 1

nodeSelector:
  node-type: control-plane

tolerations:
  - key: "control-plane"
    operator: "Exists"
    effect: "NoSchedule"
```

### Development Deployment

```yaml
# dev-values.yaml
image:
  tag: dev

controller:
  replicas: 1
  resources:
    limits:
      cpu: 200m
      memory: 64Mi
    requests:
      cpu: 10m
      memory: 32Mi
  leaderElection:
    enabled: false

crds:
  install: true
```

## Troubleshooting

### Check Controller Status

```bash
kubectl get pods -n buildkit-system
kubectl logs -n buildkit-system -l app.kubernetes.io/name=buildkit-controller
```

### Check CRDs

```bash
kubectl get crds | grep buildkit
```

### Check OIDC Configuration

```bash
kubectl get buildkitoidcconfig -n buildkit-system
kubectl describe buildkitoidcconfig github-actions -n buildkit-system
```

### Verify Installation

```bash
# Check all resources
helm status buildkit-controller --namespace buildkit-system

# List all resources
helm get manifest buildkit-controller --namespace buildkit-system
```

## Next Steps

After installation:

1. **Create a BuildKitPool:**

   ```bash
   kubectl apply -f examples/pool-example.yaml
   ```

2. **Configure OIDC (if needed):**

   ```bash
   kubectl apply -f examples/oidc-config-example.yaml
   ```

3. **Create a client certificate:**
   ```bash
   kubectl apply -f examples/client-cert-example.yaml
   ```

See the main [README.md](../README.md) for usage examples.
