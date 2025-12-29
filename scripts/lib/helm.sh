#!/usr/bin/env bash
# Helm chart management functions

set -euo pipefail

# Source common functions
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${LIB_DIR}/common.sh"

# Install or upgrade Helm chart
install_helm_chart() {
    local chart_path="$1"
    local release_name="${2:-buildkit-controller}"
    local namespace="${3:-${DEFAULT_NAMESPACE}}"
    local values_file="${4:-}"
    local extra_args=("${@:5}")
    
    local helm_args=(
        --namespace "${namespace}"
        --create-namespace
    )
    
    if [ -n "${values_file}" ]; then
        helm_args+=(--values "${values_file}")
    fi
    
    helm_args+=("${extra_args[@]}")
    
    # Check if release already exists
    if helm list --namespace "${namespace}" --short 2>/dev/null | grep -q "^${release_name}$"; then
        log_info "Helm release '${release_name}' already exists. Upgrading..."
        helm upgrade "${release_name}" "${chart_path}" \
            "${helm_args[@]}" \
            --wait \
            --timeout 5m || {
            log_error "Failed to upgrade Helm chart"
            return 1
        }
    else
        log_info "Installing Helm chart '${release_name}'..."
        helm install "${release_name}" "${chart_path}" \
            "${helm_args[@]}" \
            --wait \
            --timeout 5m || {
            log_error "Failed to install Helm chart"
            return 1
        }
    fi
    
    log_success "Helm chart '${release_name}' installed successfully!"
}

# Build Helm extra args from image environment variables
build_helm_image_args() {
    local args=()
    
    # Controller image
    if [ -n "${CONTROLLER_IMAGE:-}" ]; then
        parse_image "${CONTROLLER_IMAGE}" "CONTROLLER"
        args+=(--set "image.name=${CONTROLLER_NAME}" --set "image.tag=${CONTROLLER_TAG}" --set "image.registry=")
    fi
    
    # Auth proxy image
    if [ -n "${AUTH_PROXY_IMAGE:-}" ]; then
        parse_image "${AUTH_PROXY_IMAGE}" "AUTH_PROXY"
        args+=(--set "authProxy.image.name=${AUTH_PROXY_NAME}" --set "authProxy.image.tag=${AUTH_PROXY_TAG}" --set "authProxy.image.registry=")
    fi
    
    # Gateway image
    if [ -n "${GATEWAY_IMAGE:-}" ]; then
        parse_image "${GATEWAY_IMAGE}" "GATEWAY"
        args+=(--set "gateway.image.name=${GATEWAY_NAME}" --set "gateway.image.tag=${GATEWAY_TAG}" --set "gateway.image.registry=")
    fi
    
    echo "${args[@]}"
}

