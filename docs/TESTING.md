# Testing Guide

This guide helps you test the BuildKit Controller in your local development environment.

## Prerequisites

- Kind cluster running (`make dev` or `make dev-pool`)
- BuildKitPool created and ready
- Controller running

## Quick Status Check

```bash
make dev-status
```

This will show you:

- Controller deployment status
- Pool status and endpoint
- API server availability

## Step 1: Verify Pool is Ready

```bash
# Check pool status
kubectl get buildkitpool -n buildkit-system

# Wait for pool to be ready
kubectl wait --for=condition=Ready buildkitpool/minimal-pool -n buildkit-system --timeout=5m

# Get pool details
kubectl get buildkitpool minimal-pool -n buildkit-system -o yaml
```

Look for:

- `status.phase: Running`
- `status.endpoint` should be set (e.g., `tcp://minimal-pool.buildkit-system.svc:1235`)
- `status.readyReplicas > 0`

## Step 2: Test the API Server

### Port-forward the API

```bash
# Get the controller pod name
POD=$(kubectl get pods -n buildkit-system -l control-plane=buildkit-controller -o jsonpath='{.items[0].metadata.name}')

# Port-forward
kubectl port-forward -n buildkit-system $POD 8082:8082
```

### Test API Endpoints

In another terminal:

```bash
# Health check (no auth required)
curl http://localhost:8082/api/v1/health

# List pools (requires auth - see below)
curl http://localhost:8082/api/v1/pools
```

## Step 3: Create a Client Certificate

Use the API to request a certificate with a ServiceAccount token:

```bash
# Create a service account
kubectl create serviceaccount test-sa -n buildkit-system

# Get the token
TOKEN=$(kubectl create token test-sa -n buildkit-system)

# Request certificate via API (with port-forward running)
curl -X POST http://localhost:8082/api/v1/certs/request \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pools": ["minimal-pool"], "duration": "24h"}' \
  | jq -r '.caCert' | base64 -d > ca.crt

curl -X POST http://localhost:8082/api/v1/certs/request \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pools": ["minimal-pool"], "duration": "24h"}' \
  | jq -r '.clientCert' | base64 -d > client.crt

curl -X POST http://localhost:8082/api/v1/certs/request \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pools": ["minimal-pool"], "duration": "24h"}' \
  | jq -r '.clientKey' | base64 -d > client.key
```

## Step 4: Test with Docker Buildx

### Get Pool Endpoint

```bash
ENDPOINT=$(kubectl get buildkitpool minimal-pool -n buildkit-system -o jsonpath='{.status.endpoint}')
echo "Pool endpoint: $ENDPOINT"
```

### Create Buildx Builder

```bash
# Create buildx builder
docker buildx create \
  --name buildkit-test \
  --driver remote \
  $ENDPOINT \
  --tls-ca-cert ca.crt \
  --tls-cert client.crt \
  --tls-key client.key \
  --use

# Verify builder
docker buildx ls
```

### Test Build

```bash
# Create a simple Dockerfile for testing
mkdir -p /tmp/buildkit-test
cat > /tmp/buildkit-test/Dockerfile <<EOF
FROM alpine:latest
RUN echo "Hello from BuildKit!"
EOF

# Build using the pool
docker buildx build \
  --builder buildkit-test \
  --platform linux/amd64 \
  --tag test:latest \
  --load \
  /tmp/buildkit-test

# Verify the image
docker images | grep test
```

## Step 5: Test Worker Allocation

Workers are allocated on-demand via the API. Test the allocation flow:

```bash
# Allocate a worker using bkctl
bkctl allocate --pool minimal-pool --namespace buildkit-system --ttl 1h

# Or use the API directly
TOKEN=$(kubectl create token default -n buildkit-system)
curl -X POST http://buildkit-controller.buildkit-system.svc:8082/api/v1/workers/allocate \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "poolName": "minimal-pool",
    "namespace": "buildkit-system",
    "ttl": "1h"
  }' | jq .

# Check worker status
kubectl get buildkitworker -n buildkit-system

# Check worker pod
kubectl get pods -n buildkit-system -l buildkit.smrt-devops.net/worker=true
```

## Troubleshooting

### Pool Not Ready

```bash
# Check pool events
kubectl describe buildkitpool minimal-pool -n buildkit-system

# Check controller logs
kubectl logs -n buildkit-system -l control-plane=buildkit-controller --tail=100

# Check pool pods
kubectl get pods -n buildkit-system | grep minimal-pool
```

### Certificate Issues

```bash
# Verify certificate validity
openssl x509 -in client.crt -text -noout

# Check certificate expiry
openssl x509 -in client.crt -noout -dates

# Verify CA matches
openssl verify -CAfile ca.crt client.crt
```

### Connection Issues

```bash
# Verify service exists
kubectl get svc -n buildkit-system | grep minimal-pool

# Check service endpoints
kubectl get endpoints -n buildkit-system | grep minimal-pool

# Test connectivity from within cluster
kubectl run -it --rm test-pod --image=curlimages/curl --restart=Never -- \
  curl -v tcp://minimal-pool.buildkit-system.svc:1235
```

### API Authentication Issues

```bash
# Check API server logs
kubectl logs -n buildkit-system -l control-plane=buildkit-controller --tail=100 | grep -i auth

# Test with verbose output
curl -v http://localhost:8082/api/v1/health
```

## Next Steps

- See `examples/docker-buildx-usage.md` for more advanced usage
- See `examples/github-actions-workflow.yaml` for CI/CD integration
- Check `docs/ARCHITECTURE.md` for system design details
