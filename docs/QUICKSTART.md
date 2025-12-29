# Quick Start Guide

Get up and running with the BuildKit Controller in 5 minutes.

## Prerequisites

- Kubernetes cluster (1.24+) with `kubectl` configured
- Helm 3.0+ installed
- Docker with Buildx (for building images)

## Installation

### Step 1: Install the Controller

```bash
# Install with Helm
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace

# Verify installation
kubectl get pods -n buildkit-system
```

You should see the controller pod running:

```
NAME                                  READY   STATUS    RESTARTS   AGE
buildkit-controller-xxxxxxxxxx-xxxxx   1/1     Running   0          30s
```

### Step 2: Create a BuildKit Pool

```bash
# Apply the example pool
kubectl apply -f examples/pool-example.yaml
```

This creates a BuildKit pool with:

- Gateway enabled (default)
- TLS enabled
- mTLS authentication
- Medium resource size (1 CPU / 2Gi memory per worker)
- Scale-to-zero enabled (min: 0, max: 10)

Wait for the pool to be ready:

```bash
kubectl get buildkitpool minimal-pool -o jsonpath='{.status.phase}'
# Should output: Running
```

### Step 3: Allocate a Worker

Workers are allocated on-demand. Use the `bkctl` CLI tool:

```bash
# Install bkctl (or use from bin/ if built locally)
# Allocate a worker and get certificates
bkctl allocate --pool minimal-pool --namespace buildkit-system --ttl 1h
```

This will output JSON with:

- `workerName`: The allocated worker name
- `token`: Allocation token
- `caCert`, `clientCert`, `clientKey`: Base64-encoded certificates
- `gatewayEndpoint`: Gateway service endpoint

Or use the HTTP API directly:

```bash
# Get ServiceAccount token (for in-cluster access)
TOKEN=$(kubectl create token default -n buildkit-system)

# Allocate worker
curl -X POST http://buildkit-controller.buildkit-system.svc:8082/api/v1/workers/allocate \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "poolName": "minimal-pool",
    "namespace": "buildkit-system",
    "ttl": "1h"
  }' | jq .
```

### Step 4: Connect with Docker Buildx

#### Option A: Using bkctl (Recommended)

```bash
# Allocate and build in one command
bkctl build --pool minimal-pool --namespace buildkit-system -- \
  --platform linux/amd64 \
  --tag myapp:latest \
  --load \
  .
```

#### Option B: Manual Setup

```bash
# Save certificates from allocation response
ALLOC_RESPONSE=$(bkctl allocate --pool minimal-pool --namespace buildkit-system --ttl 1h)

# Extract and decode certificates
echo $ALLOC_RESPONSE | jq -r '.caCert' | base64 -d > ca.crt
echo $ALLOC_RESPONSE | jq -r '.clientCert' | base64 -d > client.crt
echo $ALLOC_RESPONSE | jq -r '.clientKey' | base64 -d > client.key

# Get gateway endpoint
GATEWAY_ENDPOINT=$(echo $ALLOC_RESPONSE | jq -r '.gatewayEndpoint')

# Create buildx builder
docker buildx create \
  --name my-builder \
  --driver remote \
  --driver-opt "cacert=ca.crt,cert=client.crt,key=client.key" \
  $GATEWAY_ENDPOINT \
  --use

# Verify connection
docker buildx inspect my-builder

# Build a test image
docker buildx build \
  --builder my-builder \
  --platform linux/amd64 \
  --tag myapp:latest \
  --load \
  .
```

## Verify Everything Works

### Check Pool Status

```bash
kubectl get buildkitpool minimal-pool -o yaml
```

Look for:

- `status.phase: Running`
- `status.gateway.ready: true`
- `status.workers.total: 1` (after allocation)

### Check Gateway Status

```bash
# Check gateway deployment
kubectl get deployment minimal-pool-gateway -n buildkit-system

# Check gateway service
kubectl get svc minimal-pool -n buildkit-system

# Check gateway pods
kubectl get pods -l buildkit.smrt-devops.net/purpose=gateway -n buildkit-system
```

### Check Worker Status

```bash
# List workers
kubectl get buildkitworkers -n buildkit-system

# Check worker details
kubectl get buildkitworker <worker-name> -n buildkit-system -o yaml
```

Look for:

- `status.phase: Allocated` or `Running`
- `status.endpoint`: Worker pod IP and port
- `spec.allocation.jobId`: Job identifier

### Test Build

```bash
# Create a simple Dockerfile
cat > Dockerfile <<EOF
FROM alpine:latest
RUN echo "Hello from BuildKit!"
CMD echo "Build successful!"
EOF

# Build using the remote builder
docker buildx build \
  --builder my-builder \
  --platform linux/amd64 \
  --tag test:latest \
  --load \
  .

# Verify the image
docker run --rm test:latest
# Should output: Build successful!
```

## Next Steps

- **[Deployment Guide](DEPLOYMENT.md)** - Production deployment configuration
- **[OIDC Setup](OIDC-SETUP.md)** - Configure OIDC authentication for CI/CD
- **[Examples](../examples/)** - More usage examples
- **[Architecture](ARCHITECTURE.md)** - Understand the system design

## Troubleshooting

### Controller Not Starting

```bash
# Check controller logs
kubectl logs -n buildkit-system -l control-plane=buildkit-controller

# Check for errors
kubectl describe pod -n buildkit-system -l control-plane=buildkit-controller
```

### Pool Not Ready

```bash
# Check pool status
kubectl describe buildkitpool minimal-pool -n buildkit-system

# Check gateway deployment
kubectl describe deployment minimal-pool-gateway -n buildkit-system

# Check gateway pods
kubectl logs -l buildkit.smrt-devops.net/purpose=gateway -n buildkit-system
```

### Worker Allocation Fails

```bash
# Check controller logs for allocation errors
kubectl logs -n buildkit-system -l control-plane=buildkit-controller | grep allocate

# Check worker resources
kubectl get buildkitworkers -n buildkit-system

# Check worker pod status
kubectl get pods -l buildkit.smrt-devops.net/worker=true -n buildkit-system
```

### Connection Issues

```bash
# Verify gateway service exists
kubectl get svc minimal-pool -n buildkit-system

# Test connectivity from within cluster
kubectl run test-pod --image=curlimages/curl --rm -it -- \
  curl -v tcp://minimal-pool.buildkit-system.svc:1235

# Check gateway endpoint
kubectl get buildkitpool minimal-pool -n buildkit-system \
  -o jsonpath='{.status.endpoint}'
```

### Certificate Errors

```bash
# Verify certificates are valid
openssl x509 -in client.crt -text -noout

# Check certificate CN (should contain allocation token)
openssl x509 -in client.crt -noout -subject

# Verify CA certificate matches
openssl verify -CAfile ca.crt client.crt
```

## Common Issues

### "certificate signed by unknown authority"

Make sure you're using the correct CA certificate from the allocation response:

```bash
# Re-extract CA certificate
echo $ALLOC_RESPONSE | jq -r '.caCert' | base64 -d > ca.crt
```

### "connection refused"

Check that the gateway is running:

```bash
kubectl get pods -l buildkit.smrt-devops.net/purpose=gateway -n buildkit-system
```

### "worker lookup failed"

The allocation token may have expired. Allocate a new worker:

```bash
bkctl allocate --pool minimal-pool --namespace buildkit-system --ttl 1h
```

### "builder not found"

Make sure you created the builder:

```bash
docker buildx ls
docker buildx create --name my-builder ...
```

## Getting Help

- Check the [full documentation](../README.md)
- Review [examples](../examples/)
- Open an [issue](https://github.com/smrt-devops/buildkit-controller/issues)
