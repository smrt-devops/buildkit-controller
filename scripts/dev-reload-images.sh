#!/usr/bin/env bash
# Rebuild and reload images into kind cluster

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"
source "${SCRIPT_DIR}/lib/kind.sh"

PROJECT_ROOT="$(get_project_root)"
CLUSTER_NAME="${CLUSTER_NAME:-${DEFAULT_CLUSTER_NAME}}"
NAMESPACE="${NAMESPACE:-${DEFAULT_NAMESPACE}}"
CONTROLLER_IMAGE="${CONTROLLER_IMAGE:-buildkit-controller:dev}"
GATEWAY_IMAGE="${GATEWAY_IMAGE:-buildkit-controller-gateway:dev}"

log_step "Rebuilding and reloading images"

if ! kind_cluster_exists "${CLUSTER_NAME}"; then
    log_error "Kind cluster '${CLUSTER_NAME}' does not exist"
    exit 1
fi

log_info "Building controller image..."
docker build -t "${CONTROLLER_IMAGE}" -f "${PROJECT_ROOT}/docker/Dockerfile" "${PROJECT_ROOT}"

log_info "Building gateway image..."
docker build -t "${GATEWAY_IMAGE}" -f "${PROJECT_ROOT}/docker/Dockerfile.gateway" "${PROJECT_ROOT}"

log_info "Loading images into kind cluster..."
load_image_to_kind "${CONTROLLER_IMAGE}" "${CLUSTER_NAME}"
load_image_to_kind "${GATEWAY_IMAGE}" "${CLUSTER_NAME}"

log_info "Restarting deployments..."
kubectl rollout restart deployment/buildkit-controller -n "${NAMESPACE}" || true

for gateway in $(kubectl get deployment -n "${NAMESPACE}" -l buildkit.smrt-devops.net/purpose=gateway -o jsonpath='{.items[*].metadata.name}' 2>/dev/null || echo ""); do
    if [ -n "${gateway}" ]; then
        log_info "Restarting ${gateway}..."
        kubectl rollout restart deployment/"${gateway}" -n "${NAMESPACE}" || true
    fi
done

log_info "Waiting for deployments to be available..."
kubectl wait --for=condition=available --timeout=120s deployment/buildkit-controller -n "${NAMESPACE}" || true

for gateway in $(kubectl get deployment -n "${NAMESPACE}" -l buildkit.smrt-devops.net/purpose=gateway -o jsonpath='{.items[*].metadata.name}' 2>/dev/null || echo ""); do
    if [ -n "${gateway}" ]; then
        kubectl wait --for=condition=available --timeout=120s deployment/"${gateway}" -n "${NAMESPACE}" || true
    fi
done

log_success "Images reloaded and pods restarted"
