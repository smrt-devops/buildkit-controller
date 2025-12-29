#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"

PROJECT_ROOT="$(get_project_root)"
NAMESPACE="${NAMESPACE:-${DEFAULT_NAMESPACE}}"
POOL_NAME="${POOL_NAME:-minimal-pool}"
OIDC_PORT="${OIDC_PORT:-8888}"
CONTROLLER_API="${CONTROLLER_API:-http://localhost:${DEFAULT_CONTROLLER_PORT}}"

check_kubectl || exit 1

log_step "OIDC Authentication Test"
echo ""

# Step 1: Build mock-oidc if needed
log_info "1. Building mock-oidc server..."
build_binary "${PROJECT_ROOT}" "mock-oidc" "./cmd/mock-oidc"

# Step 2: Check if mock-oidc is already running
echo ""
log_info "2. Checking mock OIDC server..."
if port_in_use "${OIDC_PORT}"; then
    log_success "Mock OIDC server already running on port ${OIDC_PORT}"
else
    log_info "Starting mock OIDC server..."
    "${PROJECT_ROOT}/bin/mock-oidc" --port "${OIDC_PORT}" &
    MOCK_OIDC_PID=$!
    sleep 2
    
    if port_in_use "${OIDC_PORT}"; then
        log_success "Mock OIDC server started (PID: ${MOCK_OIDC_PID})"
    else
        log_error "Failed to start mock OIDC server"
        exit 1
    fi
fi

MOCK_ISSUER="http://localhost:${OIDC_PORT}"

# Step 3: Create/update BuildKitOIDCConfig in cluster
echo ""
log_info "3. Creating BuildKitOIDCConfig for local testing..."

# For Kind cluster, we need to use host.docker.internal or the host network
# Check if we're in kind
CLUSTER_NAME=$(get_kind_cluster_name)
if [ -n "${CLUSTER_NAME}" ]; then
    # In Kind, use the docker bridge network IP
    HOST_IP=$(docker network inspect "${CLUSTER_NAME}" -f '{{range .IPAM.Config}}{{.Gateway}}{{end}}' 2>/dev/null || echo "host.docker.internal")
    MOCK_ISSUER_INTERNAL="http://${HOST_IP}:${OIDC_PORT}"
    log_info "Using Kind-accessible issuer: ${MOCK_ISSUER_INTERNAL}"
else
    MOCK_ISSUER_INTERNAL="${MOCK_ISSUER}"
fi

cat <<EOF | kubectl apply -f -
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: local-mock-oidc
  namespace: ${NAMESPACE}
spec:
  issuer: ${MOCK_ISSUER_INTERNAL}
  audience: buildkit-controller
  enabled: true
  claimsMapping:
    user: "actor"
    pools: "repository"
EOF
log_success "BuildKitOIDCConfig created"

# Step 4: Wait for controller to pick up config
echo ""
log_info "4. Waiting for controller to discover OIDC config..."
sleep 3
log_success "Config should be loaded"

# Step 5: Ensure bkctl is built
echo ""
log_info "5. Building bkctl..."
build_binary "${PROJECT_ROOT}" "bkctl" "./cmd/bkctl"

# Step 6: Generate test token using bkctl
echo ""
log_info "6. Generating test OIDC token using bkctl..."
TOKEN=$("${PROJECT_ROOT}/bin/bkctl" oidc-token \
    --issuer "http://mock-oidc.${NAMESPACE}.svc:8888" \
    --actor "test-user" \
    --repository "test-org/test-repo" 2>/dev/null || \
    kubectl exec -n "${NAMESPACE}" deploy/mock-oidc -- wget -qO- \
        --post-data='{"sub":"test-user","aud":"buildkit-controller","claims":{"actor":"test-user","repository":"test-org/test-repo"}}' \
        --header='Content-Type: application/json' \
        http://localhost:8888/token 2>/dev/null | jq -r .id_token)
if [ -z "${TOKEN}" ]; then
    log_error "Failed to generate token"
    exit 1
fi
log_success "Token generated (length: ${#TOKEN} chars)"

# Step 7: Test using bkctl with OIDC token
echo ""
log_info "7. Testing bkctl with OIDC token..."

# Check if controller API is accessible via GatewayAPI
source "${SCRIPT_DIR}/lib/gatewayapi.sh"
if [ -z "${CONTROLLER_API}" ]; then
    CONTROLLER_API=$(get_controller_api_endpoint "${NAMESPACE}")
fi

if [ -z "${CONTROLLER_API}" ] || ! curl -s "${CONTROLLER_API}/api/v1/health" &>/dev/null; then
    log_error "Controller API not accessible via GatewayAPI"
    echo "   Ensure GatewayAPI is installed and controller API GatewayAPI is enabled"
    exit 1
fi

echo ""
echo "   Testing bkctl status..."
export BKCTL_TOKEN="${TOKEN}"
export BKCTL_ENDPOINT="${CONTROLLER_API}"
export BKCTL_NAMESPACE="${NAMESPACE}"

if "${PROJECT_ROOT}/bin/bkctl" status --pool "${POOL_NAME}" --namespace "${NAMESPACE}" &>/dev/null; then
    log_success "bkctl status command succeeded"
    echo ""
    echo "   Pool status:"
    "${PROJECT_ROOT}/bin/bkctl" status --pool "${POOL_NAME}" --namespace "${NAMESPACE}" | head -10
else
    log_error "bkctl status failed"
    echo ""
    echo "   This might mean:"
    echo "   - Controller cannot reach the mock OIDC server (network issue)"
    echo "   - OIDC config hasn't been loaded yet"
    echo "   - Token format issue"
    echo ""
    echo "   Debug: Check controller logs:"
    echo "   kubectl logs -n ${NAMESPACE} -l control-plane=buildkit-controller --tail=50"
fi

# Step 8: Test worker allocation (if pool exists)
echo ""
log_info "8. Testing worker allocation with OIDC token..."
if "${PROJECT_ROOT}/bin/bkctl" allocate --pool "${POOL_NAME}" --namespace "${NAMESPACE}" --ttl "5m" &>/tmp/bkctl-allocate.log; then
    log_success "Worker allocated successfully"
    ALLOC_TOKEN=$(grep -oP 'Token:\s+\K\S+' /tmp/bkctl-allocate.log || echo "")
    if [ -n "${ALLOC_TOKEN}" ]; then
        log_success "Allocation token received"
        # Clean up
        "${PROJECT_ROOT}/bin/bkctl" release --token "${ALLOC_TOKEN}" &>/dev/null || true
        log_success "Worker released"
    fi
else
    log_warning "Worker allocation failed (check logs in /tmp/bkctl-allocate.log)"
    if grep -q "Pool.*not found" /tmp/bkctl-allocate.log; then
        echo "   ⚠ Pool '${POOL_NAME}' not found - create a pool first"
    fi
fi

echo ""
log_step "OIDC Test Complete"
echo ""
echo "Next steps:"
echo "1. Use bkctl with OIDC token:"
echo "   export BKCTL_TOKEN=\$(${PROJECT_ROOT}/bin/bkctl oidc-token --actor my-user --repository my-org/my-repo)"
echo "   ${PROJECT_ROOT}/bin/bkctl allocate --pool ${POOL_NAME}"
echo ""
echo "2. If tests failed, check controller logs:"
echo "   kubectl logs -n ${NAMESPACE} -l control-plane=buildkit-controller -f"
echo ""
echo "3. Verify OIDC config:"
echo "   kubectl get buildkitoidcconfig -n ${NAMESPACE} -o yaml"
echo ""
echo "4. To generate tokens manually:"
echo "   ${PROJECT_ROOT}/bin/bkctl oidc-token --actor my-user --repository my-org/my-repo"

