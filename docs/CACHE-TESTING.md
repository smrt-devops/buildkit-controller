# Cache Testing Guide

This guide explains how to test and verify BuildKit cache functionality with different cache backends.

## Cache Backend Types

BuildKit supports three types of cache backends:

1. **Registry Cache** - Stores cache in container registries (most common)
2. **S3 Cache** - Stores cache in S3-compatible object storage
3. **Local Cache** - Stores cache in persistent volumes (PVC)

## Quick Test

Run the automated cache test script:

```bash
./scripts/test-cache.sh
```

This script will:

1. Create a test Dockerfile with multiple layers
2. Run a first build (populates cache)
3. Run a second build (should use cache - faster)
4. Test cache invalidation
5. Compare build times to verify cache effectiveness

## Manual Testing

### 1. Create Pool with Registry Cache

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata:
  name: cache-test-pool
  namespace: buildkit-system
spec:
  cache:
    backends:
      - type: registry
        registry:
          endpoint: ghcr.io
          mode: max
          compression: zstd
    gc:
      enabled: true
      schedule: "0 2 * * *"
      keepStorage: "10GB"
      keepDuration: "168h"
  # ... rest of spec
```

Apply it:

```bash
kubectl apply -f examples/pool-with-registry-cache.yaml
```

### 2. Test Cache with Docker Buildx

#### First Build (Populates Cache)

```bash
# Build with cache export
bkctl build --pool cache-test-pool -- \
  --cache-to type=registry,ref=ghcr.io/your-org/cache:test,mode=max \
  -t myapp:latest \
  .
```

#### Second Build (Uses Cache)

```bash
# Build with cache import
bkctl build --pool cache-test-pool -- \
  --cache-from type=registry,ref=ghcr.io/your-org/cache:test \
  --cache-to type=registry,ref=ghcr.io/your-org/cache:test,mode=max \
  -t myapp:latest \
  .
```

The second build should be significantly faster and show `CACHED` for layers.

### 3. Verify Cache is Working

#### Check Build Output

Look for `CACHED` in the build output:

```
#2 [internal] load build definition from Dockerfile
#2 CACHED
#3 [internal] load .dockerignore
#3 CACHED
#4 [1/4] FROM docker.io/library/alpine:latest
#4 CACHED
```

#### Compare Build Times

```bash
# First build
time bkctl build --pool cache-test-pool -- -t test:first .

# Second build (should be faster)
time bkctl build --pool cache-test-pool -- -t test:second .
```

#### Check Cache Metrics

If Prometheus is enabled, check cache-related metrics:

```bash
kubectl port-forward -n buildkit-system deploy/cache-test-pool-gateway 9090:9090
curl http://localhost:9090/metrics | grep cache
```

### 4. Test Different Cache Backends

#### Registry Cache

**Pros:**

- No additional infrastructure needed
- Works across clusters
- Easy to share between teams

**Example:**

```yaml
cache:
  backends:
    - type: registry
      registry:
        endpoint: ghcr.io
        mode: max
        compression: zstd
```

**Test:**

```bash
bkctl build --pool pool-name -- \
  --cache-from type=registry,ref=ghcr.io/org/cache:tag \
  --cache-to type=registry,ref=ghcr.io/org/cache:tag,mode=max \
  -t app:latest .
```

#### S3 Cache

**Pros:**

- Scalable
- Cost-effective for large caches
- Works with any S3-compatible storage

**Example:**

```yaml
cache:
  backends:
    - type: s3
      s3:
        bucket: buildkit-cache
        region: us-east-1
        endpoint: s3.amazonaws.com
        credentialsSecret: aws-credentials
```

**Test:**

```bash
bkctl build --pool pool-name -- \
  --cache-from type=s3,region=us-east-1,bucket=buildkit-cache,prefix=myapp \
  --cache-to type=s3,region=us-east-1,bucket=buildkit-cache,prefix=myapp,mode=max \
  -t app:latest .
```

#### Local Cache (PVC)

**Pros:**

- Fast (no network overhead)
- No external dependencies
- Good for single-cluster setups

**Example:**

```yaml
cache:
  backends:
    - type: local
      local:
        storageClass: standard
        size: "50Gi"
```

**Test:**

```bash
# Local cache is automatic - no special flags needed
bkctl build --pool pool-name -- -t app:latest .
```

### 5. Test Cache Invalidation

Create a Dockerfile that changes:

```dockerfile
FROM alpine:latest
RUN apk add --no-cache curl  # This will be cached
RUN echo "Version 1" > /version.txt  # Change this line
RUN apk add --no-cache git  # This should use cache
```

Build twice:

1. First build - all layers built
2. Modify the version.txt line
3. Second build - only changed layer rebuilt, rest cached

### 6. Test Garbage Collection

If GC is enabled, verify it runs:

```bash
# Check GC job/CronJob
kubectl get cronjob -n buildkit-system | grep gc

# Check GC logs
kubectl logs -n buildkit-system -l app=buildkit-gc --tail=50
```

### 7. Monitor Cache Usage

#### Check PVC Size (for local cache)

```bash
kubectl get pvc -n buildkit-system | grep cache
```

#### Check Registry Cache Size

Use registry API or UI to check cache image sizes.

#### Check BuildKit Metrics

```bash
# Port-forward to gateway
kubectl port-forward -n buildkit-system deploy/pool-name-gateway 9090:9090

# Query metrics
curl http://localhost:9090/metrics | grep -i cache
```

## Troubleshooting

### Cache Not Working

1. **Check cache backend configuration:**

   ```bash
   kubectl get buildkitpool pool-name -o yaml | grep -A 10 cache
   ```

2. **Verify buildkitd.toml:**

   ```bash
   kubectl exec -n buildkit-system deploy/pool-name-worker-0 -- \
     cat /etc/buildkit/buildkitd.toml
   ```

3. **Check build logs for cache errors:**
   ```bash
   bkctl build --pool pool-name -- -t test . 2>&1 | grep -i cache
   ```

### Registry Cache Issues

- **Authentication:** Ensure credentials are configured
- **Permissions:** Registry must support cache manifest format
- **Network:** Check if registry is accessible from workers

### S3 Cache Issues

- **Credentials:** Verify AWS credentials secret exists
- **Bucket:** Ensure bucket exists and is accessible
- **Region:** Match the region in config

### Local Cache Issues

- **StorageClass:** Verify storage class exists
- **PVC:** Check if PVC was created
- **Size:** Ensure sufficient storage available

## Best Practices

1. **Use Registry Cache for CI/CD** - Easy to share across runners
2. **Use S3 Cache for Large Caches** - More cost-effective
3. **Use Local Cache for Development** - Fastest, no network overhead
4. **Enable GC** - Prevents cache from growing indefinitely
5. **Monitor Cache Hit Rates** - Track cache effectiveness

## Example: Full Cache Test

```bash
# 1. Create pool with registry cache
kubectl apply -f examples/pool-with-registry-cache.yaml

# 2. Wait for pool to be ready
kubectl wait --for=condition=Ready buildkitpool/pool-with-registry-cache -n buildkit-system

# 3. Run automated test
./scripts/test-cache.sh --pool pool-with-registry-cache

# 4. Check results
# Look for "CACHED" in build output and compare build times
```

## Advanced: Multi-Stage Cache

For complex builds, use cache mounts:

```dockerfile
FROM golang:1.21 AS builder
RUN --mount=type=cache,target=/go/pkg/mod \
    go mod download
RUN --mount=type=cache,target=/root/.cache/go-build \
    go build -o app .

FROM alpine:latest
COPY --from=builder /app /app
CMD ["/app"]
```

This caches Go modules and build cache separately.
