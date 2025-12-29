#!/usr/bin/env bash
# Create a BuildKitPool (applies the manifest as-is, like customers would)
# Usage: ./scripts/create-pool.sh [pool-name] [namespace] [options]
# Options:
#   --file FILE          Pool manifest file (default: examples/pool-dev.yaml)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"
source "${SCRIPT_DIR}/lib/gatewayapi.sh"
source "${SCRIPT_DIR}/lib/hosts.sh"

PROJECT_ROOT="$(get_project_root)"
NAMESPACE="${NAMESPACE:-${DEFAULT_NAMESPACE}}"

# Parse arguments
POOL_NAME=""
POOL_FILE=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --file)
            POOL_FILE="$2"
            shift 2
            ;;
        --namespace)
            NAMESPACE="$2"
            shift 2
            ;;
        -*)
            log_error "Unknown option: $1"
            exit 1
            ;;
        *)
            if [ -z "${POOL_NAME}" ]; then
                POOL_NAME="$1"
            elif [ -z "${NAMESPACE}" ] || [ "${NAMESPACE}" = "${DEFAULT_NAMESPACE}" ]; then
                NAMESPACE="$1"
            fi
            shift
            ;;
    esac
done

# Default pool file if not provided
if [ -z "${POOL_FILE}" ]; then
    POOL_FILE="${PROJECT_ROOT}/examples/pool-dev.yaml"
fi

check_kubectl || exit 1

# Read pool name and namespace from file if not provided
if [ -z "${POOL_NAME}" ] || [ "${NAMESPACE}" = "${DEFAULT_NAMESPACE}" ]; then
    if command_exists yq; then
        FILE_NAME=$(yq eval '.metadata.name' "${POOL_FILE}" 2>/dev/null || echo "")
        FILE_NAMESPACE=$(yq eval '.metadata.namespace' "${POOL_FILE}" 2>/dev/null || echo "")
    else
        FILE_NAME=$(grep -E "^  name:" "${POOL_FILE}" | head -1 | awk '{print $2}' || echo "")
        FILE_NAMESPACE=$(grep -E "^  namespace:" "${POOL_FILE}" | head -1 | awk '{print $2}' || echo "")
    fi
    
    if [ -z "${POOL_NAME}" ] && [ -n "${FILE_NAME}" ]; then
        POOL_NAME="${FILE_NAME}"
    fi
    if [ "${NAMESPACE}" = "${DEFAULT_NAMESPACE}" ] && [ -n "${FILE_NAMESPACE}" ]; then
        NAMESPACE="${FILE_NAMESPACE}"
    fi
fi

# Default pool name if still not provided
if [ -z "${POOL_NAME}" ]; then
    POOL_NAME="minimal-pool"
fi

log_step "Creating BuildKitPool: ${POOL_NAME}"
echo ""

# Check if GatewayAPI is installed
if gateway_api_installed; then
    log_success "GatewayAPI is available"
else
    log_warning "GatewayAPI is not installed"
fi

# Apply pool manifest (only update name/namespace if they differ from file)
TEMP_POOL_FILE=""
FILE_NAME=""
FILE_NAMESPACE=""

if command_exists yq; then
    FILE_NAME=$(yq eval '.metadata.name' "${POOL_FILE}" 2>/dev/null || echo "")
    FILE_NAMESPACE=$(yq eval '.metadata.namespace' "${POOL_FILE}" 2>/dev/null || echo "")
else
    FILE_NAME=$(grep -E "^  name:" "${POOL_FILE}" | head -1 | awk '{print $2}' || echo "")
    FILE_NAMESPACE=$(grep -E "^  namespace:" "${POOL_FILE}" | head -1 | awk '{print $2}' || echo "")
fi

# Only create temp file if we need to update name or namespace
if [ "${FILE_NAME}" != "${POOL_NAME}" ] || [ "${FILE_NAMESPACE}" != "${NAMESPACE}" ]; then
    TEMP_POOL_FILE=$(mktemp /tmp/pool-XXXXXX.yaml)
    if command_exists yq; then
        yq eval "
            .metadata.name = \"${POOL_NAME}\" |
            .metadata.namespace = \"${NAMESPACE}\"
        " "${POOL_FILE}" > "${TEMP_POOL_FILE}" || {
            log_error "Failed to update pool file with yq"
            exit 1
        }
    else
        sed "s/^  name:.*/  name: ${POOL_NAME}/" "${POOL_FILE}" | \
        sed "s/^  namespace:.*/  namespace: ${NAMESPACE}/" > "${TEMP_POOL_FILE}"
    fi
    POOL_FILE="${TEMP_POOL_FILE}"
fi

# Apply pool manifest
log_info "Applying pool manifest..."
kubectl apply -f "${POOL_FILE}" || {
    log_error "Failed to create pool"
    [ -n "${TEMP_POOL_FILE}" ] && rm -f "${TEMP_POOL_FILE}"
    exit 1
}

# Clean up temp file
[ -n "${TEMP_POOL_FILE}" ] && rm -f "${TEMP_POOL_FILE}"

# Extract GatewayAPI hostname from the applied pool and add to /etc/hosts
HOSTNAME=""
if command_exists yq; then
    HOSTNAME=$(kubectl get buildkitpool "${POOL_NAME}" -n "${NAMESPACE}" -o yaml 2>/dev/null | \
        yq eval '.spec.gateway.gatewayAPI.hostname' - 2>/dev/null || echo "")
else
    HOSTNAME=$(kubectl get buildkitpool "${POOL_NAME}" -n "${NAMESPACE}" -o jsonpath='{.spec.gateway.gatewayAPI.hostname}' 2>/dev/null || echo "")
fi

if [ -n "${HOSTNAME}" ]; then
    echo ""
    log_info "Configuring hostname resolution..."
    source "${SCRIPT_DIR}/lib/hosts.sh"
    
    # Gateway API uses dynamic NodePorts that aren't mapped in Kind config
    # So we need to use a node IP instead of localhost
    # Get the first node IP (any node will work since NodePorts are accessible on all nodes)
    node_ip=$(get_first_node_ip)
    if [ -n "${node_ip}" ] && [ "${node_ip}" != "127.0.0.1" ]; then
        add_hostname "${HOSTNAME}" "${node_ip}" || true
        log_info "Using node IP ${node_ip} for Gateway API endpoint (NodePort not mapped to localhost)"
    else
        # Fallback to localhost (won't work for unmapped ports, but at least the entry exists)
        add_hostname_for_nodeport "${HOSTNAME}" "true" || true
        log_warning "Using localhost (may not work - Gateway API NodePort not mapped in Kind config)"
        log_info "You may need to manually update /etc/hosts to use a node IP"
    fi
fi

# Wait for pool to be ready
echo ""
log_info "Waiting for pool to be ready..."

# Check if Ready condition exists first (to avoid waiting if it doesn't)
CONDITION_EXISTS=$(kubectl get buildkitpool "${POOL_NAME}" -n "${NAMESPACE}" -o jsonpath='{.status.conditions[?(@.type=="Ready")]}' 2>/dev/null || echo "")

if [ -n "${CONDITION_EXISTS}" ]; then
    # Condition exists, wait for it to be True
    if kubectl wait --for=condition=Ready "buildkitpool/${POOL_NAME}" -n "${NAMESPACE}" --timeout=300s 2>/dev/null; then
        log_success "Pool is ready"
    else
        log_warning "Pool Ready condition is not True yet"
    fi
else
    # Condition doesn't exist yet, wait for gateway deployment instead
    # (The Ready condition depends on gateway being ready, so this is more reliable)
    log_info "Ready condition not set yet, checking gateway deployment..."
    GATEWAY_DEPLOYMENT="${POOL_NAME}-gateway"
    
    # Wait for gateway deployment to be available (with shorter timeout since condition doesn't exist)
    if kubectl wait --for=condition=available "deployment/${GATEWAY_DEPLOYMENT}" -n "${NAMESPACE}" --timeout=300s 2>/dev/null; then
        log_success "Gateway deployment is ready"
    else
        # Check if deployment exists
        if kubectl get deployment "${GATEWAY_DEPLOYMENT}" -n "${NAMESPACE}" &>/dev/null; then
            log_warning "Gateway deployment exists but not yet ready"
            log_info "This is normal - the controller is still provisioning resources"
        else
            log_warning "Gateway deployment not found yet (controller may still be creating it)"
        fi
        log_info "Continuing anyway - gateway will be ready shortly"
    fi
fi

# Get Envoy Gateway NodePort for the pool's Gateway (after pool is ready)
if [ -n "${HOSTNAME}" ]; then
    # Wait a bit for the gateway to be created by the controller
    sleep 3
    
    # Get the pool gateway name from the pool resource (defaults to {pool-name}-gateway)
    POOL_GATEWAY_NAME="${POOL_NAME}-gateway"
    if command_exists yq; then
        CUSTOM_GATEWAY_NAME=$(kubectl get buildkitpool "${POOL_NAME}" -n "${NAMESPACE}" -o yaml 2>/dev/null | \
            yq eval '.spec.gateway.gatewayAPI.gatewayName' - 2>/dev/null || echo "")
    else
        CUSTOM_GATEWAY_NAME=$(kubectl get buildkitpool "${POOL_NAME}" -n "${NAMESPACE}" -o jsonpath='{.spec.gateway.gatewayAPI.gatewayName}' 2>/dev/null || echo "")
    fi
    if [ -n "${CUSTOM_GATEWAY_NAME}" ]; then
        POOL_GATEWAY_NAME="${CUSTOM_GATEWAY_NAME}"
    fi
    
    # Get Envoy Gateway NodePort for the pool's Gateway (pass gateway name to find the right service)
    GATEWAY_PORT=$(get_envoy_gateway_nodeport "envoy-gateway-system" "${NAMESPACE}" "${POOL_GATEWAY_NAME}")
    
    # If not found by name, try without name (fallback to port-based lookup)
    if [ -z "${GATEWAY_PORT}" ]; then
        GATEWAY_PORT=$(get_envoy_gateway_nodeport "envoy-gateway-system" "${NAMESPACE}")
    fi
    
    if [ -n "${GATEWAY_PORT}" ]; then
        echo ""
        log_success "Pool GatewayAPI configured"
        echo "  Hostname: ${HOSTNAME}"
        echo "  Endpoint: tcp://${HOSTNAME}:${GATEWAY_PORT}"
        echo ""
        echo "To connect:"
        echo "  BKCTL_GATEWAY_ENDPOINT=tcp://${HOSTNAME}:${GATEWAY_PORT} bin/bkctl build --pool ${POOL_NAME} --namespace ${NAMESPACE} -- ..."
    else
        log_warning "Gateway NodePort not yet available (Gateway may still be programming)"
        log_info "You can check gateway status with: kubectl get gateway ${POOL_GATEWAY_NAME} -n ${NAMESPACE}"
    fi
fi

# Verify Gateway API resources if enabled
if gateway_api_installed; then
    echo ""
    log_info "Verifying Gateway API resources..."
    
    # Wait a bit for controller to create Gateway and TCPRoute
    sleep 3
    
    # Check Gateway
    GATEWAY_NAME="${POOL_NAME}-gateway"
    if kubectl get gateway "${GATEWAY_NAME}" -n "${NAMESPACE}" &>/dev/null; then
        GATEWAY_STATUS=$(kubectl get gateway "${GATEWAY_NAME}" -n "${NAMESPACE}" -o jsonpath='{.status.conditions[?(@.type=="Programmed")].status}' 2>/dev/null || echo "")
        if [ "${GATEWAY_STATUS}" = "True" ]; then
            log_success "Gateway '${GATEWAY_NAME}' is programmed"
        else
            log_warning "Gateway '${GATEWAY_NAME}' exists but not yet programmed"
        fi
    else
        log_warning "Gateway '${GATEWAY_NAME}' not found (controller may still be creating it)"
    fi
    
    # Check TCPRoute or TLSRoute (depending on TLS mode)
    TCPROUTE_NAME="${POOL_NAME}-tcproute"
    TLSROUTE_NAME="${POOL_NAME}-tlsroute"
    
    # Check for TCPRoute (used for TCP mode, no TLS)
    if kubectl get tcproute "${TCPROUTE_NAME}" -n "${NAMESPACE}" &>/dev/null; then
        TCPROUTE_STATUS=$(kubectl get tcproute "${TCPROUTE_NAME}" -n "${NAMESPACE}" -o jsonpath='{.status.parents[0].conditions[?(@.type=="Accepted")].status}' 2>/dev/null || echo "")
        if [ "${TCPROUTE_STATUS}" = "True" ]; then
            log_success "TCPRoute '${TCPROUTE_NAME}' is accepted"
        else
            log_warning "TCPRoute '${TCPROUTE_NAME}' exists but not yet accepted"
        fi
    # Check for TLSRoute (used for TLS passthrough mode)
    elif kubectl get tlsroute "${TLSROUTE_NAME}" -n "${NAMESPACE}" &>/dev/null; then
        TLSROUTE_STATUS=$(kubectl get tlsroute "${TLSROUTE_NAME}" -n "${NAMESPACE}" -o jsonpath='{.status.parents[0].conditions[?(@.type=="Accepted")].status}' 2>/dev/null || echo "")
        if [ "${TLSROUTE_STATUS}" = "True" ]; then
            log_success "TLSRoute '${TLSROUTE_NAME}' is accepted"
        else
            log_warning "TLSRoute '${TLSROUTE_NAME}' exists but not yet accepted"
        fi
    else
        log_warning "TCPRoute/TLSRoute not found (controller may still be creating it)"
    fi
fi

log_success "Pool created: ${POOL_NAME} in namespace ${NAMESPACE}"

