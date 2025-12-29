# Using BuildKit Pools with Docker Buildx

This guide shows how to use BuildKit pools with `docker buildx` for building container images.

## Prerequisites

1. Docker with Buildx plugin installed
2. Client certificates for mTLS authentication
3. Access to a BuildKit pool

## Setup

### 1. Allocate a Worker and Get Certificates

Workers are allocated on-demand. Each allocation includes certificates with an embedded allocation token.

#### Option A: Using bkctl (Recommended)

```bash
# Allocate a worker and get certificates
bkctl allocate --pool my-buildkit-pool --namespace default --ttl 1h > alloc.json

# Extract certificates
jq -r '.caCert' alloc.json | base64 -d > ca.crt
jq -r '.clientCert' alloc.json | base64 -d > client.crt
jq -r '.clientKey' alloc.json | base64 -d > client.key

# Get gateway endpoint
GATEWAY_ENDPOINT=$(jq -r '.gatewayEndpoint' alloc.json)
```

#### Option B: Via HTTP API (OIDC)

```bash
# Get OIDC token (e.g., from GitHub Actions)
TOKEN=$(curl -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
  "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=buildkit-controller" | jq -r .value)

# Allocate worker and get certificates
curl -X POST http://buildkit-controller.buildkit-system.svc:8082/api/v1/workers/allocate \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"poolName": "my-buildkit-pool", "namespace": "default", "ttl": "1h"}' \
  > alloc.json

# Extract certificates
jq -r '.caCert' alloc.json | base64 -d > ca.crt
jq -r '.clientCert' alloc.json | base64 -d > client.crt
jq -r '.clientKey' alloc.json | base64 -d > client.key
GATEWAY_ENDPOINT=$(jq -r '.gatewayEndpoint' alloc.json)
```

#### Option C: Via Kubernetes Secret (Long-lived certificates)

For long-lived certificates, you can store the certificates obtained from the API in a Kubernetes Secret:

```bash
# Store certificates from API response in a secret
kubectl create secret generic my-client-cert -n default \
  --from-file=ca.crt=ca.crt \
  --from-file=client.crt=client.crt \
  --from-file=client.key=client.key

# Later, retrieve them:
kubectl get secret my-client-cert -n default \
  -o jsonpath='{.data.ca\.crt}' | base64 -d > ca.crt

kubectl get secret my-client-cert -n default \
  -o jsonpath='{.data.client\.crt}' | base64 -d > client.crt

kubectl get secret my-client-cert -n default \
  -o jsonpath='{.data.client\.key}' | base64 -d > client.key

# Note: These certificates don't have allocation tokens and won't work with the gateway
# They're only for direct access (if enabled) or administrative purposes
```

### 2. Create Buildx Builder

```bash
# Create a new buildx builder using the pool gateway with TLS
docker buildx create \
  --name my-builder \
  --driver remote \
  --driver-opt "cacert=ca.crt,cert=client.crt,key=client.key" \
  $GATEWAY_ENDPOINT \
  --use

# Or if using the service name directly:
docker buildx create \
  --name my-builder \
  --driver remote \
  --driver-opt "cacert=ca.crt,cert=client.crt,key=client.key" \
  tcp://my-buildkit-pool.default.svc:1235 \
  --use
```

**Important**: The certificates must include an allocation token (from worker allocation). The gateway uses this token to route connections to the correct worker.

The `--driver-opt` flag accepts comma-separated key=value pairs:

- `cacert`: Path to CA certificate
- `cert`: Path to client certificate
- `key`: Path to client private key

### 3. Build Images

```bash
# Build using the builder
docker buildx build \
  --builder my-builder \
  --platform linux/amd64,linux/arm64 \
  --tag myapp:latest \
  --push \
  .
```

Or with the builder set as default:

```bash
# Set as default builder
docker buildx use my-builder

# Build
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --tag myapp:latest \
  --push \
  .
```

## Using with Cache

### Registry Cache

```bash
docker buildx build \
  --builder my-builder \
  --cache-from type=registry,ref=registry.example.com/cache/myapp \
  --cache-to type=registry,ref=registry.example.com/cache/myapp,mode=max \
  --tag myapp:latest \
  --push \
  .
```

### Inline Cache

```bash
docker buildx build \
  --builder my-builder \
  --cache-from type=registry,ref=myapp:buildcache \
  --cache-to type=inline \
  --tag myapp:latest \
  --push \
  .
```

## GitHub Actions Example

```yaml
name: Build with BuildKit Pool

on:
  push:
    branches: [main]

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3

      - name: Get OIDC Token
        id: oidc
        uses: actions/github-script@v6
        with:
          script: |
            const token = await core.getIDToken('buildkit-controller')
            core.setOutput('token', token)

      - name: Allocate Worker and Get Certificates
        id: allocate
        run: |
          curl -X POST http://buildkit-controller.buildkit-system.svc:8082/api/v1/workers/allocate \
            -H "Authorization: Bearer ${{ steps.oidc.outputs.token }}" \
            -H "Content-Type: application/json" \
            -d '{"poolName": "ci-pool", "namespace": "default", "ttl": "1h"}' \
            > alloc.json
          jq -r '.caCert' alloc.json | base64 -d > ca.crt
          jq -r '.clientCert' alloc.json | base64 -d > client.crt
          jq -r '.clientKey' alloc.json | base64 -d > client.key
          echo "gateway=$(jq -r '.gatewayEndpoint' alloc.json)" >> $GITHUB_OUTPUT

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v2

      - name: Create Buildx Builder
        run: |
          docker buildx create \
            --name buildkit-builder \
            --driver remote \
            --driver-opt "cacert=ca.crt,cert=client.crt,key=client.key" \
            ${{ steps.allocate.outputs.gateway }} \
            --use

      - name: Build and Push
        run: |
          docker buildx build \
            --builder buildkit-builder \
            --platform linux/amd64,linux/arm64 \
            --cache-from type=registry,ref=ghcr.io/${{ github.repository }}:buildcache \
            --cache-to type=registry,ref=ghcr.io/${{ github.repository }}:buildcache,mode=max \
            --tag ghcr.io/${{ github.repository }}:${{ github.sha }} \
            --tag ghcr.io/${{ github.repository }}:latest \
            --push \
            .
```

## Troubleshooting

### Connection Refused

- Verify the pool is running: `kubectl get buildkitpool`
- Check gateway service: `kubectl get svc my-buildkit-pool`
- Check gateway pods: `kubectl get pods -l buildkit.smrt-devops.net/purpose=gateway`
- Ensure port is 1235 (gateway port, not 1234)
- Verify a worker is allocated: `kubectl get buildkitworkers`

### Certificate Errors

- Verify certificates are valid: `openssl x509 -in client.crt -text -noout`
- Check certificate expiry: `openssl x509 -in client.crt -noout -dates`
- Verify allocation token in CN: `openssl x509 -in client.crt -noout -subject` (should contain `alloc:`)
- Ensure CA certificate matches pool's CA
- Check if allocation token expired (default TTL is 1h)

### Authentication Failures

- Verify mTLS is enabled on the pool
- Check that certificates include allocation token (from worker allocation)
- Verify worker is still allocated: `kubectl get buildkitworkers`
- Check gateway logs: `kubectl logs -l buildkit.smrt-devops.net/purpose=gateway`
- Ensure certificates match the pool's CA
- Verify allocation token hasn't expired (re-allocate if needed)
