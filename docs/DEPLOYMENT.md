# Deployment Guide

This guide covers deploying the BuildKit Controller to your Kubernetes cluster using Helm (recommended) or manual installation.

## Prerequisites

- Kubernetes cluster 1.24+
- kubectl configured
- Helm 3.0+ (for Helm installation)

## Quick Start with Helm

```bash
# Install with default values
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace

# Verify installation
kubectl get pods -n buildkit-system
kubectl get crds | grep buildkit
```

## Helm Installation

### Basic Installation

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace
```

### Custom Images

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace \
  --set image.registry=ghcr.io/smrt-devops \
  --set image.name=buildkit-controller/controller \
  --set image.tag=v1.0.0 \
  --set gateway.image.registry=ghcr.io/smrt-devops \
  --set gateway.image.name=buildkit-controller/gateway \
  --set gateway.image.tag=v1.0.0
```

### Production Deployment

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  -f helm/buildkit-controller/values-production.yaml \
  --namespace buildkit-system \
  --create-namespace
```

### With OIDC Configuration

```bash
# Create values file with OIDC config
cat > my-values.yaml <<EOF
oidc:
  configs:
    - name: github-actions
      issuer: https://token.actions.githubusercontent.com
      audience: buildkit-controller
      enabled: true
      claimsMapping:
        user: "actor"
EOF

helm install buildkit-controller ./helm/buildkit-controller \
  -f my-values.yaml \
  --namespace buildkit-system \
  --create-namespace
```

## Manual Installation

If you prefer not to use Helm, you can install CRDs manually:

```bash
# 1. Generate manifests
make manifests

# 2. Install CRDs
kubectl apply -f helm/buildkit-controller/crds

# 3. Build and push Docker images
make docker-push-all \
  IMG=ghcr.io/smrt-devops/buildkit-controller/controller:latest \
  GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:latest

# 4. Deploy controller (you'll need to create RBAC and deployment manually)
```

**Note:** Helm is the recommended deployment method as it handles CRDs, RBAC, and deployment automatically.

## Verification

### Check Controller Status

```bash
# Check pods
kubectl get pods -n buildkit-system

# Check logs
kubectl logs -n buildkit-system -l control-plane=buildkit-controller

# Check CRDs
kubectl get crds | grep buildkit
```

### Test API Endpoint

```bash
# Port forward to API server
kubectl port-forward -n buildkit-system \
  deployment/buildkit-controller 8082:8082

# Test health endpoint
curl http://localhost:8082/api/v1/health
```

## Post-Installation

### 1. Create a BuildKitPool

```bash
kubectl apply -f examples/pool-example.yaml
```

The pool will automatically create a gateway deployment. Verify:

```bash
kubectl get deployment -l buildkit.smrt-devops.net/purpose=gateway
kubectl get svc -l buildkit.smrt-devops.net/purpose=gateway
```

### 2. Configure OIDC (Optional)

```bash
kubectl apply -f examples/oidc-config-example.yaml
```

### 3. Test Worker Allocation

```bash
# Using bkctl
bkctl allocate --pool minimal-pool --namespace buildkit-system

# Or via API
TOKEN=$(kubectl create token default -n buildkit-system)
curl -X POST http://buildkit-controller.buildkit-system.svc:8082/api/v1/workers/allocate \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"poolName": "minimal-pool", "namespace": "buildkit-system"}'
```

## Upgrading

### Helm Upgrade

```bash
# Upgrade with new values
helm upgrade buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --set image.tag=v1.1.0

# Upgrade with values file
helm upgrade buildkit-controller ./helm/buildkit-controller \
  -f values.yaml \
  --namespace buildkit-system
```

### Manual Upgrade

```bash
# Update CRDs
kubectl apply -f helm/buildkit-controller/crds

# Update controller (if not using Helm)
helm upgrade buildkit-controller ./helm/buildkit-controller --namespace buildkit-system
```

## Uninstalling

### Helm Uninstall

```bash
# Uninstall (CRDs removed by default)
helm uninstall buildkit-controller --namespace buildkit-system

# Uninstall but keep CRDs
helm uninstall buildkit-controller --namespace buildkit-system \
  --set crds.keep=true
```

### Manual Uninstall

```bash
# Delete controller (if using Helm)
helm uninstall buildkit-controller --namespace buildkit-system

# Delete CRDs (optional)
kubectl delete crd buildkitpools.buildkit.smrt-devops.net
kubectl delete crd buildkitworkers.buildkit.smrt-devops.net
kubectl delete crd buildkitoidcconfigs.buildkit.smrt-devops.net

# Note: Deleting pools will also delete associated gateways and workers

# Delete namespace
kubectl delete namespace buildkit-system
```

## Troubleshooting

### Controller Not Starting

```bash
# Check pod status
kubectl describe pod -n buildkit-system -l control-plane=buildkit-controller

# Check logs
kubectl logs -n buildkit-system -l control-plane=buildkit-controller

# Check events
kubectl get events -n buildkit-system --sort-by='.lastTimestamp'
```

### CRDs Not Found

```bash
# Verify CRDs are installed
kubectl get crds | grep buildkit

# Reinstall CRDs
kubectl apply -f helm/buildkit-controller/crds
```

### API Server Not Responding

```bash
# Check service
kubectl get svc -n buildkit-system

# Check endpoints
kubectl get endpoints -n buildkit-system

# Test connectivity
kubectl run -it --rm debug --image=curlimages/curl --restart=Never -- \
  curl http://buildkit-controller.buildkit-system.svc:8082/api/v1/health
```

### Gateway Not Ready

```bash
# Check gateway deployment
kubectl get deployment -l buildkit.smrt-devops.net/purpose=gateway

# Check gateway pods
kubectl get pods -l buildkit.smrt-devops.net/purpose=gateway

# Check gateway logs
kubectl logs -l buildkit.smrt-devops.net/purpose=gateway

# Verify gateway service
kubectl get svc -l buildkit.smrt-devops.net/purpose=gateway
```

## Configuration Reference

See [helm/buildkit-controller/README.md](helm/buildkit-controller/README.md) for complete configuration options.
