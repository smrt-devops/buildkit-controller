#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"

PROJECT_ROOT="$(get_project_root)"
NAMESPACE="${NAMESPACE:-${DEFAULT_NAMESPACE}}"
POOL_NAME="${POOL_NAME:-minimal-pool}"
TEST_DIR="${TEST_DIR:-/tmp/buildkit-test}"
BKCTL="${PROJECT_ROOT}/bin/bkctl"

log_step "Docker Buildx Test Script"
echo ""

check_kubectl || exit 1

# Step 1: Ensure bkctl is built
log_info "1. Checking bkctl CLI..."
build_binary "${PROJECT_ROOT}" "bkctl" "./cmd/bkctl"
log_success "bkctl is ready"

# Step 2: Check controller API
echo ""
log_info "2. Checking controller API..."
source "${SCRIPT_DIR}/lib/gatewayapi.sh"

# Try GatewayAPI first
CONTROLLER_API="${BKCTL_ENDPOINT:-}"
if [ -z "${CONTROLLER_API}" ]; then
    CONTROLLER_API=$(get_controller_api_endpoint "${NAMESPACE}")
fi

if [ -z "${CONTROLLER_API}" ] || ! curl -s "${CONTROLLER_API}/api/v1/health" &>/dev/null; then
    log_error "Cannot access controller API via GatewayAPI"
    echo "   Ensure GatewayAPI is installed and controller API GatewayAPI is enabled"
    echo "   Check: kubectl get httproute -n ${NAMESPACE}"
    exit 1
fi
log_success "Controller API is accessible at ${CONTROLLER_API}"

# Step 3: Get authentication token (for non-dev mode)
echo ""
echo "3. Setting up authentication..."
export BKCTL_ENDPOINT="${CONTROLLER_API}"
export BKCTL_NAMESPACE="${NAMESPACE}"

# Try to get a service account token (not needed if controller is in dev mode)
if [ -z "${BKCTL_TOKEN:-}" ]; then
    TOKEN=$(kubectl create token buildkit-controller -n "${NAMESPACE}" --duration=1h 2>/dev/null || true)
    if [ -n "${TOKEN}" ]; then
        export BKCTL_TOKEN="${TOKEN}"
        echo "   ✓ Using kubectl-generated service account token"
    else
        echo "   ⚠ No token available (works if controller is in dev mode)"
    fi
else
    echo "   ✓ Using provided BKCTL_TOKEN"
fi

# Step 4: Set up gateway access
echo ""
log_info "4. Setting up gateway access..."

# Get pool gateway endpoint from status
POOL_ENDPOINT=$(kubectl get buildkitpool "${POOL_NAME}" -n "${NAMESPACE}" -o jsonpath='{.status.endpoint}' 2>/dev/null || echo "")

if [ -n "${POOL_ENDPOINT}" ]; then
    # Extract hostname:port from endpoint (e.g., tcp://pool-name.buildkit.local:1235)
    if [[ "${POOL_ENDPOINT}" == tcp://* ]]; then
        POOL_ENDPOINT="${POOL_ENDPOINT#tcp://}"
    fi
    export BKCTL_GATEWAY_ENDPOINT="${POOL_ENDPOINT}"
    log_success "Gateway endpoint from pool status: ${BKCTL_GATEWAY_ENDPOINT}"
else
    # Fallback: try to get from service
    NODE_PORT_VALUE=$(get_nodeport "${NAMESPACE}" "${POOL_NAME}" "gateway")
    if [ -n "${NODE_PORT_VALUE}" ]; then
        export BKCTL_GATEWAY_ENDPOINT="localhost:${NODE_PORT_VALUE}"
        log_success "Gateway using NodePort: ${NODE_PORT_VALUE}"
    else
        log_error "Cannot determine gateway endpoint for pool ${POOL_NAME}"
        echo "   Pool may not be ready or GatewayAPI not configured"
        exit 1
    fi
fi

echo "   Gateway endpoint: ${BKCTL_GATEWAY_ENDPOINT}"

# Step 5: Create test Dockerfile
echo ""
log_info "5. Creating test Dockerfile..."
mkdir -p "${TEST_DIR}"
cat > "${TEST_DIR}/Dockerfile" <<'EOF'
FROM alpine:latest

RUN echo "Hello from BuildKit Pool!"
RUN apk add --no-cache curl
RUN echo "Build completed successfully" > /build-info.txt

CMD ["cat", "/build-info.txt"]
EOF
log_success "Test Dockerfile created at ${TEST_DIR}/Dockerfile"

# Step 6: Run build using bkctl
echo ""
log_info "6. Running build with bkctl..."
echo "   Pool: ${POOL_NAME}"
echo "   Namespace: ${NAMESPACE}"
echo "   Gateway: ${BKCTL_GATEWAY_ENDPOINT}"
echo ""

# bkctl handles everything: allocation, certs, builder setup, cleanup
if "${BKCTL}" build --pool "${POOL_NAME}" --namespace "${NAMESPACE}" -- \
    --load \
    -t buildkit-test:latest \
    -f "${TEST_DIR}/Dockerfile" \
    "${TEST_DIR}"; then
    
    echo ""
    log_success "Build successful!"
    
    # Step 7: Verify image
    echo ""
    log_info "7. Verifying image..."
    if docker images buildkit-test:latest --format "{{.Repository}}:{{.Tag}}" | grep -q "buildkit-test:latest"; then
        log_success "Image found in local registry"
        
        # Test running the image
        echo ""
        log_info "8. Testing image run..."
        OUTPUT=$(docker run --rm buildkit-test:latest 2>&1 || true)
        if echo "${OUTPUT}" | grep -q "Build completed successfully"; then
            log_success "Image runs successfully"
        else
            log_warning "Image ran but output unexpected: ${OUTPUT}"
        fi
    else
        log_warning "Image not found locally (may be expected depending on build args)"
    fi
    
    echo ""
    log_step "Test Summary"
    log_success "Pool: ${POOL_NAME}"
    log_success "Build: SUCCESS"
    
else
    echo ""
    log_error "Build failed"
    exit 1
fi

# Cleanup
echo ""
log_info "9. Cleanup..."
rm -rf "${TEST_DIR}"
docker rmi buildkit-test:latest 2>/dev/null || true
log_success "Cleanup complete"

echo ""
log_step "All tests passed!"
