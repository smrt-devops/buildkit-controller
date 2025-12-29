#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"

PROJECT_ROOT="$(get_project_root)"
NAMESPACE="${NAMESPACE:-${DEFAULT_NAMESPACE}}"
POOL_NAME="${POOL_NAME:-minimal-pool}"
TEST_DIR="${TEST_DIR:-/tmp/buildkit-cache-test}"
BKCTL="${PROJECT_ROOT}/bin/bkctl"

check_kubectl || exit 1

log_step "BuildKit Cache Test"
echo ""

# Step 1: Ensure bkctl is built
log_info "1. Checking bkctl..."
build_binary "${PROJECT_ROOT}" "bkctl" "./cmd/bkctl"

# Step 2: Check pool exists
echo ""
log_info "2. Checking pool..."
if ! resource_exists "buildkitpool" "${POOL_NAME}" "${NAMESPACE}"; then
    log_error "Pool '${POOL_NAME}' not found in namespace '${NAMESPACE}'"
    echo "   Create a pool first: kubectl apply -f examples/pool-example.yaml"
    exit 1
fi
log_success "Pool found: ${POOL_NAME}"

# Step 3: Check cache configuration
echo ""
log_info "3. Checking cache configuration..."
CACHE_BACKENDS=$(kubectl get buildkitpool "${POOL_NAME}" -n "${NAMESPACE}" -o jsonpath='{.spec.cache.backends[*].type}' 2>/dev/null || echo "")
if [ -z "${CACHE_BACKENDS}" ]; then
    log_warning "No cache backends configured (cache will still work, just no persistent backend)"
else
    log_success "Cache backends: ${CACHE_BACKENDS}"
fi

# Step 4: Create test Dockerfile that benefits from cache
echo ""
log_info "4. Creating test Dockerfile..."
mkdir -p "${TEST_DIR}"
cat > "${TEST_DIR}/Dockerfile" <<'EOF'
FROM alpine:latest

# Install packages (these will be cached)
RUN apk add --no-cache curl wget git

# Create some files
RUN echo "Layer 1" > /layer1.txt
RUN echo "Layer 2" > /layer2.txt
RUN echo "Layer 3" > /layer3.txt

# Install Node.js (large layer, good for cache testing)
RUN apk add --no-cache nodejs npm

# Create application
RUN echo 'console.log("Hello from cached build!");' > /app.js

CMD ["node", "/app.js"]
EOF
log_success "Dockerfile created at ${TEST_DIR}/Dockerfile"

# Step 5: First build (populates cache)
echo ""
log_info "5. Running first build (populates cache)..."
echo "   This build will be slower as it downloads and builds everything"
FIRST_BUILD_START=$(date +%s)
if "${BKCTL}" build --pool "${POOL_NAME}" --namespace "${NAMESPACE}" -- \
    --load \
    -t cache-test:first \
    -f "${TEST_DIR}/Dockerfile" \
    "${TEST_DIR}" 2>&1 | tee /tmp/cache-test-first.log; then
    FIRST_BUILD_END=$(date +%s)
    FIRST_BUILD_TIME=$((FIRST_BUILD_END - FIRST_BUILD_START))
    log_success "First build completed in ${FIRST_BUILD_TIME}s"
else
    log_error "First build failed"
    exit 1
fi

# Step 6: Second build (should use cache)
echo ""
log_info "6. Running second build (should use cache)..."
echo "   This build should be faster as layers are cached"
SECOND_BUILD_START=$(date +%s)
if "${BKCTL}" build --pool "${POOL_NAME}" --namespace "${NAMESPACE}" -- \
    --load \
    -t cache-test:second \
    -f "${TEST_DIR}/Dockerfile" \
    "${TEST_DIR}" 2>&1 | tee /tmp/cache-test-second.log; then
    SECOND_BUILD_END=$(date +%s)
    SECOND_BUILD_TIME=$((SECOND_BUILD_END - SECOND_BUILD_START))
    log_success "Second build completed in ${SECOND_BUILD_TIME}s"
    
    # Compare build times
    if [ "${SECOND_BUILD_TIME}" -lt "${FIRST_BUILD_TIME}" ]; then
        SPEEDUP=$((FIRST_BUILD_TIME - SECOND_BUILD_TIME))
        PERCENT=$((SPEEDUP * 100 / FIRST_BUILD_TIME))
        log_success "Cache working! Second build was ${SPEEDUP}s faster (${PERCENT}% improvement)"
    else
        log_warning "Second build wasn't faster - cache may not be working optimally"
    fi
else
    log_error "Second build failed"
    exit 1
fi

# Step 7: Check cache export/import (if registry cache configured)
if echo "${CACHE_BACKENDS}" | grep -q "registry"; then
    echo ""
    log_info "7. Testing cache export/import..."
    REGISTRY_ENDPOINT=$(kubectl get buildkitpool "${POOL_NAME}" -n "${NAMESPACE}" -o jsonpath='{.spec.cache.backends[?(@.type=="registry")].registry.endpoint}' 2>/dev/null || echo "")
    if [ -n "${REGISTRY_ENDPOINT}" ]; then
        echo "   Registry cache endpoint: ${REGISTRY_ENDPOINT}"
        echo "   Testing cache export..."
        
        # Build with cache export
        if "${BKCTL}" build --pool "${POOL_NAME}" --namespace "${NAMESPACE}" -- \
            --cache-from type=registry,ref=${REGISTRY_ENDPOINT}/cache-test:buildcache \
            --cache-to type=registry,ref=${REGISTRY_ENDPOINT}/cache-test:buildcache,mode=max \
            -t cache-test:exported \
            -f "${TEST_DIR}/Dockerfile" \
            "${TEST_DIR}" 2>&1 | grep -q "CACHED\|EXPORTING"; then
            echo "   ✓ Cache export/import working"
        else
            echo "   ⚠ Cache export/import may need credentials or different configuration"
        fi
    fi
fi

# Step 8: Test cache invalidation
echo ""
log_info "8. Testing cache invalidation..."
cat > "${TEST_DIR}/Dockerfile" <<'EOF'
FROM alpine:latest

# Install packages (should use cache)
RUN apk add --no-cache curl wget git

# Change this layer (should invalidate cache from here)
RUN echo "Layer 1 - MODIFIED" > /layer1.txt
RUN echo "Layer 2" > /layer2.txt
RUN echo "Layer 3" > /layer3.txt

# Install Node.js (should use cache if previous layers cached)
RUN apk add --no-cache nodejs npm

CMD ["node", "--version"]
EOF

THIRD_BUILD_START=$(date +%s)
if "${BKCTL}" build --pool "${POOL_NAME}" --namespace "${NAMESPACE}" -- \
    --load \
    -t cache-test:invalidated \
    -f "${TEST_DIR}/Dockerfile" \
    "${TEST_DIR}" 2>&1 | tee /tmp/cache-test-third.log; then
    THIRD_BUILD_END=$(date +%s)
    THIRD_BUILD_TIME=$((THIRD_BUILD_END - THIRD_BUILD_START))
    log_success "Third build (with modification) completed in ${THIRD_BUILD_TIME}s"
    
    # Check if cache was used for unchanged layers
    if grep -q "CACHED" /tmp/cache-test-third.log; then
        CACHED_LAYERS=$(grep -c "CACHED" /tmp/cache-test-third.log || echo "0")
        log_success "Cache working! ${CACHED_LAYERS} layers used from cache"
    fi
else
    log_error "Third build failed"
fi

# Step 9: Check cache metrics (if available)
echo ""
log_info "9. Checking cache metrics..."
if kubectl get pods -n "${NAMESPACE}" -l "buildkit.smrt-devops.net/pool=${POOL_NAME}" &>/dev/null; then
    WORKER_POD=$(kubectl get pods -n "${NAMESPACE}" -l "buildkit.smrt-devops.net/pool=${POOL_NAME}" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
    if [ -n "${WORKER_POD}" ]; then
        echo "   Worker pod: ${WORKER_POD}"
        echo "   Checking buildkitd config..."
        kubectl exec -n "${NAMESPACE}" "${WORKER_POD}" -- cat /etc/buildkit/buildkitd.toml 2>/dev/null | grep -A 5 "registry\|cache" || echo "   (Config not accessible or using defaults)"
    fi
fi

# Summary
echo ""
log_step "Cache Test Summary"
echo ""
echo "Build times:"
echo "  First build:  ${FIRST_BUILD_TIME}s (populates cache)"
echo "  Second build: ${SECOND_BUILD_TIME}s (should use cache)"
if [ -n "${THIRD_BUILD_TIME:-}" ]; then
    echo "  Third build:  ${THIRD_BUILD_TIME}s (with modification)"
fi
echo ""
echo "Cache backends: ${CACHE_BACKENDS:-none}"
echo ""
echo "To inspect cache:"
echo "  kubectl get buildkitpool ${POOL_NAME} -n ${NAMESPACE} -o yaml | grep -A 20 cache"
echo ""
echo "To test with different cache backends:"
echo "  kubectl apply -f examples/pool-with-registry-cache.yaml"
echo "  kubectl apply -f examples/pool-with-local-cache.yaml"

# Cleanup
echo ""
log_info "10. Cleanup..."
rm -rf "${TEST_DIR}"
docker rmi cache-test:first cache-test:second cache-test:exported cache-test:invalidated 2>/dev/null || true
log_success "Cleanup complete"

echo ""
log_step "Cache test complete!"

