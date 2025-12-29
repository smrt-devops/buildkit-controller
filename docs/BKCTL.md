# bkctl - BuildKit Controller CLI

`bkctl` is the command-line interface for the BuildKit Controller. It simplifies building container images using BuildKit pools by automatically handling worker allocation, certificate management, and Docker Buildx integration.

## Table of Contents

- [Installation](#installation)
- [Quick Start](#quick-start)
- [Commands](#commands)
  - [build](#build)
  - [allocate](#allocate)
  - [release](#release)
  - [status](#status)
  - [oidc-token](#oidc-token)
  - [version](#version)
- [Authentication](#authentication)
- [Environment Variables](#environment-variables)
- [Examples](#examples)
- [Troubleshooting](#troubleshooting)

## Installation

### From GitHub Releases

Download the appropriate binary for your platform from the [GitHub Releases](https://github.com/smrt-devops/buildkit-controller/releases) page:

```bash
# Linux (amd64)
curl -LO https://github.com/smrt-devops/buildkit-controller/releases/latest/download/bkctl-linux-amd64
chmod +x bkctl-linux-amd64
sudo mv bkctl-linux-amd64 /usr/local/bin/bkctl

# macOS (Apple Silicon)
curl -LO https://github.com/smrt-devops/buildkit-controller/releases/latest/download/bkctl-darwin-arm64
chmod +x bkctl-darwin-arm64
sudo mv bkctl-darwin-arm64 /usr/local/bin/bkctl

# Windows
# Download bkctl-windows-amd64.exe and add to PATH
```

### From Source

```bash
git clone https://github.com/smrt-devops/buildkit-controller.git
cd buildkit-controller
go build -o bkctl ./cmd/bkctl
sudo mv bkctl /usr/local/bin/
```

### Verify Installation

```bash
bkctl version
# Output: bkctl v0.1.0
```

## Quick Start

The simplest way to build an image:

```bash
bkctl build --pool my-pool -- -t myimage:latest .
```

This command will:

1. Allocate a worker from the specified pool
2. Set up TLS certificates automatically
3. Create a Docker Buildx builder
4. Run the build
5. Clean up the builder and release the worker

## Commands

### build

Build a container image using a BuildKit pool. This is the most common command and handles the entire workflow automatically.

**Syntax:**

```bash
bkctl build --pool <pool-name> [options] [--] [docker buildx build args...]
```

**Options:**

- `--pool`, `-p`: (Required) Name of the BuildKit pool to use
- `--namespace`, `-n`: Kubernetes namespace (default: `buildkit-system`)
- `--ttl`: Time-to-live for worker allocation (default: `1h`)
- `--oidc-actor`: OIDC actor/identity for authentication
- `--oidc-repository`: OIDC repository claim for authentication
- `--oidc-subject`: OIDC subject claim (defaults to actor if not specified)
- `--`: Separator between bkctl options and docker buildx arguments

**Examples:**

```bash
# Simple build
bkctl build --pool prod-pool -- -t myapp:latest .

# Build with specific namespace
bkctl build --pool dev-pool --namespace default -- -t myapp:v1.0.0 .

# Build with OIDC authentication
bkctl build --pool prod-pool \
  --oidc-actor my-user \
  --oidc-repository my-org/my-repo \
  -- -t myapp:latest .

# Build with cache
bkctl build --pool prod-pool -- \
  --cache-from type=registry,ref=ghcr.io/my-org/myapp:buildcache \
  --cache-to type=registry,ref=ghcr.io/my-org/myapp:buildcache,mode=max \
  -t myapp:latest \
  --push \
  .

# Multi-platform build
bkctl build --pool prod-pool -- \
  --platform linux/amd64,linux/arm64 \
  -t myapp:latest \
  --push \
  .
```

**What it does:**

1. Allocates a worker from the specified pool
2. Downloads and configures TLS certificates
3. Creates a temporary Docker Buildx builder with TLS
4. Executes `docker buildx build` with your arguments
5. Automatically cleans up the builder and releases the worker on completion or error

### allocate

Allocate a worker from a BuildKit pool and get connection details. Useful for manual setup or scripting.

**Syntax:**

```bash
bkctl allocate --pool <pool-name> [options]
```

**Options:**

- `--pool`, `-p`: (Required) Name of the BuildKit pool
- `--namespace`, `-n`: Kubernetes namespace (default: `buildkit-system`)
- `--ttl`: Time-to-live for the allocation (default: `1h`)
- `--json`: Output response as JSON
- `--oidc-actor`: OIDC actor/identity for authentication
- `--oidc-repository`: OIDC repository claim for authentication
- `--oidc-subject`: OIDC subject claim

**Examples:**

```bash
# Allocate a worker
bkctl allocate --pool prod-pool

# Allocate with custom TTL
bkctl allocate --pool prod-pool --ttl 2h

# Get JSON output
bkctl allocate --pool prod-pool --json

# Allocate with OIDC authentication
bkctl allocate --pool prod-pool \
  --oidc-actor my-user \
  --oidc-repository my-org/my-repo
```

**Output:**

```
Worker allocated successfully!

Worker:    prod-pool-worker-abc123
Token:     alloc-token-xyz789
Endpoint:  tcp://prod-pool-gateway.default.svc:1235
Expires:   2024-01-15T10:30:00Z

To release: bkctl release --token alloc-token-xyz789
```

The response includes:

- `WorkerName`: Name of the allocated worker
- `Token`: Allocation token (needed for release)
- `Endpoint`: Direct worker endpoint (if available)
- `GatewayEndpoint`: Gateway endpoint (recommended)
- `ExpiresAt`: When the allocation expires
- `CACert`: Base64-encoded CA certificate
- `ClientCert`: Base64-encoded client certificate
- `ClientKey`: Base64-encoded client private key

### release

Release an allocated worker back to the pool. Workers are automatically released when their TTL expires, but you can release them early.

**Syntax:**

```bash
bkctl release --token <token> [options]
```

**Options:**

- `--token`, `-t`: (Required) Allocation token from `bkctl allocate`
- `--oidc-actor`: OIDC actor/identity for authentication
- `--oidc-repository`: OIDC repository claim for authentication
- `--oidc-subject`: OIDC subject claim

**Examples:**

```bash
# Release a worker
bkctl release --token alloc-token-xyz789

# Release with OIDC authentication
bkctl release --token alloc-token-xyz789 \
  --oidc-actor my-user \
  --oidc-repository my-org/my-repo
```

**Note:** The `bkctl build` command automatically releases workers, so you typically only need this for manually allocated workers.

### status

Get the current status of a BuildKit pool, including worker counts, capacity, and health.

**Syntax:**

```bash
bkctl status --pool <pool-name> [options]
```

**Options:**

- `--pool`, `-p`: (Required) Name of the BuildKit pool
- `--namespace`, `-n`: Kubernetes namespace (default: `buildkit-system`)
- `--oidc-actor`: OIDC actor/identity for authentication
- `--oidc-repository`: OIDC repository claim for authentication
- `--oidc-subject`: OIDC subject claim

**Examples:**

```bash
# Check pool status
bkctl status --pool prod-pool

# Check status in specific namespace
bkctl status --pool dev-pool --namespace default
```

**Output:**
The command outputs JSON with pool information including:

- Worker counts (allocated, available, total)
- Pool capacity and limits
- Worker health status
- Gateway endpoints
- Configuration details

### oidc-token

Generate an OIDC token for authentication. Useful for testing or when using the API directly.

**Syntax:**

```bash
bkctl oidc-token [options]
```

**Options:**

- `--issuer`: OIDC issuer URL (default: auto-detected from cluster)
- `--actor`, `--oidc-actor`: Actor/identity claim
- `--repository`, `--oidc-repository`: Repository claim
- `--subject`, `--oidc-subject`: Subject claim
- `--audience`: Audience claim (default: `buildkit-controller`)

**Examples:**

```bash
# Generate token with defaults
bkctl oidc-token

# Generate token with specific identity
bkctl oidc-token --actor my-user --repository my-org/my-repo

# Use token in environment variable
export BKCTL_TOKEN=$(bkctl oidc-token --actor my-user)
bkctl status --pool prod-pool
```

**Note:** This command requires `mock-oidc` to be running in the cluster or locally. For production, use your actual OIDC provider.

### version

Display the version of bkctl.

**Syntax:**

```bash
bkctl version
```

**Output:**

```
bkctl v0.1.0
```

## Authentication

`bkctl` supports multiple authentication methods:

### 1. OIDC Token (Automatic)

If `BKCTL_TOKEN` is not set and OIDC flags are provided, `bkctl` will automatically generate an OIDC token using the mock-oidc service in the cluster:

```bash
bkctl build --pool prod-pool \
  --oidc-actor my-user \
  --oidc-repository my-org/my-repo \
  -- -t myapp:latest .
```

### 2. Explicit Token

Set `BKCTL_TOKEN` environment variable:

```bash
export BKCTL_TOKEN=$(bkctl oidc-token --actor my-user)
bkctl build --pool prod-pool -- -t myapp:latest .
```

Or use a token from your OIDC provider:

```bash
export BKCTL_TOKEN="eyJhbGciOiJSUzI1NiIs..."
bkctl build --pool prod-pool -- -t myapp:latest .
```

### 3. Service Account Token

For Kubernetes service accounts:

```bash
export BKCTL_TOKEN=$(cat /var/run/secrets/kubernetes.io/serviceaccount/token)
bkctl build --pool prod-pool -- -t myapp:latest .
```

### 4. No Authentication (Dev Only)

For development/testing only:

```bash
export BKCTL_ALLOW_NO_AUTH=true
bkctl build --pool dev-pool -- -t myapp:latest .
```

**Warning:** This only works if the controller is configured to allow unauthenticated requests (dev mode).

## Environment Variables

| Variable                 | Description                                      | Default                 |
| ------------------------ | ------------------------------------------------ | ----------------------- |
| `BKCTL_ENDPOINT`         | Controller API endpoint                          | `http://localhost:8082` |
| `BKCTL_TOKEN`            | Bearer token for authentication                  | (none)                  |
| `BKCTL_NAMESPACE`        | Default Kubernetes namespace                     | `buildkit-system`       |
| `BKCTL_TLS_SKIP_VERIFY`  | Skip TLS certificate verification                | `false`                 |
| `BKCTL_TLS_CA_CERT`      | Path to CA certificate file                      | (none)                  |
| `BKCTL_GATEWAY_ENDPOINT` | Override gateway endpoint (for testing)          | (none)                  |
| `BKCTL_ALLOW_NO_AUTH`    | Allow requests without authentication (dev only) | `false`                 |

**Examples:**

```bash
# Use custom controller endpoint
export BKCTL_ENDPOINT="https://buildkit-controller.example.com:8082"
bkctl status --pool prod-pool

# Skip TLS verification (dev only)
export BKCTL_TLS_SKIP_VERIFY=true
bkctl build --pool dev-pool -- -t myapp:latest .

# Use custom CA certificate
export BKCTL_TLS_CA_CERT="/path/to/ca.crt"
bkctl build --pool prod-pool -- -t myapp:latest .

# Set default namespace
export BKCTL_NAMESPACE="my-namespace"
bkctl build --pool my-pool -- -t myapp:latest .
```

## Examples

### Basic Build

```bash
# Build and tag an image
bkctl build --pool prod-pool -- -t myapp:latest .

# Build and push to registry
bkctl build --pool prod-pool -- \
  -t registry.example.com/myapp:latest \
  --push \
  .
```

### Multi-Platform Build

```bash
bkctl build --pool prod-pool -- \
  --platform linux/amd64,linux/arm64 \
  -t myapp:latest \
  --push \
  .
```

### Build with Cache

```bash
# Registry cache
bkctl build --pool prod-pool -- \
  --cache-from type=registry,ref=ghcr.io/my-org/myapp:buildcache \
  --cache-to type=registry,ref=ghcr.io/my-org/myapp:buildcache,mode=max \
  -t myapp:latest \
  --push \
  .

# Inline cache
bkctl build --pool prod-pool -- \
  --cache-from type=registry,ref=myapp:buildcache \
  --cache-to type=inline \
  -t myapp:latest \
  --push \
  .
```

### CI/CD Pipeline (GitHub Actions)

```yaml
name: Build with BuildKit

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

      - name: Install bkctl
        run: |
          curl -LO https://github.com/smrt-devops/buildkit-controller/releases/latest/download/bkctl-linux-amd64
          chmod +x bkctl-linux-amd64
          sudo mv bkctl-linux-amd64 /usr/local/bin/bkctl

      - name: Build and Push
        env:
          BKCTL_TOKEN: ${{ steps.oidc.outputs.token }}
          BKCTL_ENDPOINT: http://buildkit-controller.buildkit-system.svc:8082
        run: |
          bkctl build --pool ci-pool -- \
            --platform linux/amd64,linux/arm64 \
            --cache-from type=registry,ref=ghcr.io/${{ github.repository }}:buildcache \
            --cache-to type=registry,ref=ghcr.io/${{ github.repository }}:buildcache,mode=max \
            -t ghcr.io/${{ github.repository }}:${{ github.sha }} \
            -t ghcr.io/${{ github.repository }}:latest \
            --push \
            .
```

### Manual Worker Management

```bash
# Allocate a worker
ALLOC=$(bkctl allocate --pool prod-pool --json)

# Extract endpoint and certificates
ENDPOINT=$(echo $ALLOC | jq -r '.gatewayEndpoint')
echo $ALLOC | jq -r '.caCert' | base64 -d > ca.crt
echo $ALLOC | jq -r '.clientCert' | base64 -d > client.crt
echo $ALLOC | jq -r '.clientKey' | base64 -d > client.key
TOKEN=$(echo $ALLOC | jq -r '.token')

# Create buildx builder manually
docker buildx create \
  --name my-builder \
  --driver remote \
  --driver-opt "cacert=ca.crt,cert=client.crt,key=client.key" \
  $ENDPOINT \
  --use

# Build
docker buildx build -t myapp:latest --push .

# Cleanup
docker buildx rm my-builder
bkctl release --token $TOKEN
```

## Troubleshooting

### "Error allocating worker"

**Possible causes:**

- Pool doesn't exist or is in wrong namespace
- No available workers in the pool
- Authentication failure
- Network connectivity issues

**Solutions:**

```bash
# Check pool exists
kubectl get buildkitpool -n buildkit-system

# Check pool status
bkctl status --pool my-pool

# Verify authentication
bkctl oidc-token --actor my-user

# Check controller is accessible
curl $BKCTL_ENDPOINT/health
```

### "Connection refused" or "Connection timeout"

**Possible causes:**

- Controller not running
- Wrong endpoint URL
- Network/firewall issues
- TLS certificate problems

**Solutions:**

```bash
# Verify controller is running
kubectl get pods -n buildkit-system -l app=buildkit-controller

# Check endpoint
echo $BKCTL_ENDPOINT

# Test connectivity
curl -k $BKCTL_ENDPOINT/health

# For TLS issues, try skipping verification (dev only)
export BKCTL_TLS_SKIP_VERIFY=true
```

### "Build failed" or Docker Buildx errors

**Possible causes:**

- Worker allocation expired
- TLS certificate issues
- Buildx builder configuration problems
- Network issues between client and worker

**Solutions:**

```bash
# Check worker is still allocated
kubectl get buildkitworkers -n buildkit-system

# Verify certificates
openssl x509 -in client.crt -text -noout

# Check buildx builder
docker buildx inspect my-builder

# Try manual allocation to debug
bkctl allocate --pool my-pool --json
```

### "Authentication failed"

**Possible causes:**

- Missing or invalid token
- OIDC provider not accessible
- Wrong OIDC claims
- Pool requires authentication but none provided

**Solutions:**

```bash
# Verify token is set
echo $BKCTL_TOKEN

# Generate new token
export BKCTL_TOKEN=$(bkctl oidc-token --actor my-user)

# Check OIDC provider is accessible
kubectl get pods -n buildkit-system -l app=mock-oidc

# Verify OIDC claims match pool requirements
bkctl oidc-token --actor my-user --repository my-org/my-repo
```

### Certificate errors

**Possible causes:**

- Expired certificates
- Wrong CA certificate
- Certificate doesn't include allocation token
- TLS configuration mismatch

**Solutions:**

```bash
# Check certificate expiry
openssl x509 -in client.crt -noout -dates

# Verify allocation token in certificate
openssl x509 -in client.crt -noout -subject | grep alloc

# Re-allocate to get fresh certificates
bkctl allocate --pool my-pool

# Use custom CA if needed
export BKCTL_TLS_CA_CERT="/path/to/ca.crt"
```

### Worker not released after build

**Possible causes:**

- Build was interrupted (Ctrl+C)
- Process crashed
- Network error during cleanup

**Solutions:**

```bash
# Check allocated workers
kubectl get buildkitworkers -n buildkit-system

# Workers will auto-release when TTL expires, but you can manually check
# The TTL is typically 1 hour, so wait or check the ExpiresAt timestamp

# If you have the token, manually release
bkctl release --token <allocation-token>
```

### Getting help

```bash
# Show usage
bkctl help

# Show version
bkctl version

# Check command syntax
bkctl build --help  # (if supported)
```

For more information, see:

- [Architecture Documentation](./ARCHITECTURE.md)
- [Deployment Guide](./DEPLOYMENT.md)
- [OIDC Setup](./OIDC-SETUP.md)
- [Testing Guide](./TESTING.md)
