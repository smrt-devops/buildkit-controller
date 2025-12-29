#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"
source "${SCRIPT_DIR}/lib/kind.sh"

PROJECT_ROOT="$(get_project_root)"
NAMESPACE="${NAMESPACE:-${DEFAULT_NAMESPACE}}"

check_kubectl || exit 1

log_step "Deploy Mock OIDC Server"
echo ""

# Step 1: Build the mock-oidc binary
log_info "1. Building mock-oidc..."
build_binary "${PROJECT_ROOT}" "mock-oidc" "./cmd/mock-oidc"

# Step 2: Build Docker image
echo ""
log_info "2. Building Docker image..."
docker build -t mock-oidc:dev -f "${PROJECT_ROOT}/docker/Dockerfile.mock-oidc" "${PROJECT_ROOT}" || {
    log_error "Failed to build Docker image"
    exit 1
}
log_success "Image built"

# Step 3: Load into Kind
echo ""
log_info "3. Loading image into Kind cluster..."
CLUSTER_NAME=$(get_kind_cluster_name)
if [ -n "${CLUSTER_NAME}" ]; then
    load_image_to_kind "mock-oidc:dev" "${CLUSTER_NAME}"
else
    log_warning "Not a Kind cluster, skipping image load"
fi

# Step 4: Ensure namespace exists
echo ""
log_info "4. Ensuring namespace exists..."
ensure_namespace "${NAMESPACE}"
log_success "Namespace ready"

# Step 5: Deploy mock-oidc
echo ""
log_info "5. Deploying mock-oidc..."
kubectl apply -f "${PROJECT_ROOT}/examples/mock-oidc-deployment.yaml" || {
    log_error "Failed to deploy mock-oidc"
    exit 1
}
log_success "Deployment created"

# Step 6: Wait for pod to be ready
echo ""
log_info "6. Waiting for mock-oidc to be ready..."
wait_for_deployment "${NAMESPACE}" "mock-oidc" "60s"
log_success "Mock OIDC server is ready"

# Step 7: Create/update the OIDC config
echo ""
log_info "7. Creating BuildKitOIDCConfig..."
cat <<EOF | kubectl apply -f -
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: local-mock-oidc
  namespace: ${NAMESPACE}
spec:
  issuer: http://mock-oidc.${NAMESPACE}.svc:8888
  audience: buildkit-controller
  enabled: true
  claimsMapping:
    user: "actor"
    pools: "repository"
EOF
log_success "OIDC config created"

# Step 8: Show status
echo ""
log_step "Mock OIDC Server Deployed"
echo ""
echo "Service: mock-oidc.${NAMESPACE}.svc:8888"
echo ""
echo "To generate a test token:"
echo "  kubectl exec -n ${NAMESPACE} deploy/mock-oidc -- /mock-oidc --print-token --actor my-user --repository my-org/my-repo"
echo ""
echo "Or port-forward and use local binary:"
echo "  kubectl port-forward -n ${NAMESPACE} svc/mock-oidc 8888:8888 &"
echo "  ./bin/mock-oidc --print-token --issuer http://mock-oidc.${NAMESPACE}.svc:8888 --actor my-user"
echo ""
echo "To test the full OIDC flow:"
echo "  ./scripts/test.sh oidc"
echo "  or: ./scripts/tests/test-oidc.sh"

