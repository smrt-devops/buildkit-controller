#!/usr/bin/env bash
# Kind cluster management functions

set -euo pipefail

# Source common functions
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${LIB_DIR}/common.sh"

# Create kind cluster with NodePort mappings
# Usage: create_kind_cluster [cluster_name] [num_worker_nodes]
#   cluster_name: Name of the cluster (default: ${DEFAULT_CLUSTER_NAME})
#   num_worker_nodes: Number of worker nodes to create (default: 0)
create_kind_cluster() {
    local cluster_name="${1:-${DEFAULT_CLUSTER_NAME}}"
    local num_workers="${2:-0}"
    local config_file="${3:-/tmp/kind-config.yaml}"
    
    if kind_cluster_exists "${cluster_name}"; then
        # Check current node count (only if cluster context is available)
        if kubectl cluster-info &>/dev/null 2>&1; then
            local current_workers=0
            local current_total=0
            local current_control_planes=0
            
            # Safely get worker count
            local worker_output
            worker_output=$(kubectl get nodes --no-headers 2>/dev/null | grep -c "worker" 2>/dev/null || echo "0")
            current_workers=$(echo "${worker_output}" | head -1 | tr -d '\n' | tr -d ' ')
            # Ensure it's a number
            if ! [[ "${current_workers}" =~ ^[0-9]+$ ]]; then
                current_workers=0
            fi
            
            # Safely get total node count
            local total_output
            total_output=$(kubectl get nodes --no-headers 2>/dev/null | wc -l 2>/dev/null || echo "0")
            current_total=$(echo "${total_output}" | head -1 | tr -d '\n' | tr -d ' ')
            # Ensure it's a number
            if ! [[ "${current_total}" =~ ^[0-9]+$ ]]; then
                current_total=0
            fi
            
            # Calculate control plane nodes (safely)
            if [ "${current_total}" -ge "${current_workers}" ]; then
                current_control_planes=$((current_total - current_workers))
            fi
            
            if [ "${current_control_planes}" -gt 0 ] && [ "${current_total}" -gt 0 ]; then
                # Cluster exists, check if it matches desired configuration
                if [ "${current_workers}" -eq "${num_workers}" ]; then
                    log_info "Kind cluster '${cluster_name}' already exists with ${num_workers} worker node(s). Skipping creation."
                    return 0
                else
                    log_warning "Kind cluster '${cluster_name}' exists with ${current_workers} worker node(s), but ${num_workers} requested."
                    log_info "To recreate with ${num_workers} worker node(s), delete the cluster first:"
                    log_info "  kind delete cluster --name ${cluster_name}"
                    log_info "  make dev"
                    return 0
                fi
            fi
        fi
        log_info "Kind cluster '${cluster_name}' already exists. Skipping creation."
        return 0
    fi
    
    log_step "Creating kind cluster '${cluster_name}' with ${num_workers} worker node(s)..."
    
    # Create kind config with NodePort mappings
    # GatewayAPI/Envoy Gateway will use dynamic NodePorts, but we map common ranges
    {
        cat <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    extraPortMappings:
      # Gateway NodePort for BuildKit pools
      - containerPort: 31235
        hostPort: 31235
        protocol: TCP
      # Controller API NodePort (optional)
      - containerPort: 31082
        hostPort: 31082
        protocol: TCP
      # GatewayAPI/Envoy Gateway common NodePort range
      # Envoy Gateway will use dynamic ports, but we expose common ranges
      - containerPort: 30000
        hostPort: 30000
        protocol: TCP
      - containerPort: 30080
        hostPort: 30080
        protocol: TCP
      - containerPort: 30443
        hostPort: 30443
        protocol: TCP
EOF
        # Add worker nodes
        for ((i=1; i<=num_workers; i++)); do
            echo "  - role: worker"
        done
    } > "${config_file}"
    
    kind create cluster --name "${cluster_name}" --config "${config_file}" --wait 60s || {
        log_error "Failed to create kind cluster"
        rm -f "${config_file}"
        return 1
    }
    
    log_info "Waiting for cluster to be ready..."
    kubectl wait --for=condition=Ready nodes --all --timeout=120s || {
        log_warning "Cluster may not be fully ready"
    }
    
    rm -f "${config_file}"
    
    log_success "Kind cluster '${cluster_name}' is ready!"
    echo ""
    echo "Cluster nodes:"
    kubectl get nodes -o wide
    echo ""
    echo "NodePort mappings:"
    echo "  - Gateway: localhost:31235 (for BuildKit pools with gateway.nodePort: 31235)"
    echo "  - Controller API: localhost:31082 (if using controller.service.nodePort: 31082)"
    echo "  - GatewayAPI: Dynamic ports (check with: kubectl get svc envoy-gateway -n envoy-gateway-system)"
}

# Delete kind cluster
delete_kind_cluster() {
    local cluster_name="${1:-${DEFAULT_CLUSTER_NAME}}"
    
    if ! kind_cluster_exists "${cluster_name}"; then
        log_info "Kind cluster '${cluster_name}' does not exist. Nothing to delete."
        return 0
    fi
    
    log_step "Deleting kind cluster '${cluster_name}'..."
    kind delete cluster --name "${cluster_name}" || {
        log_error "Failed to delete kind cluster"
        return 1
    }
    
    log_success "Kind cluster '${cluster_name}' has been deleted!"
}

# Load Docker image into kind cluster
# By default, kind load docker-image loads to all nodes, but we verify this
load_image_to_kind() {
    local image="$1"
    local cluster_name="${2:-${DEFAULT_CLUSTER_NAME}}"
    
    if ! kind_cluster_exists "${cluster_name}"; then
        log_error "Kind cluster '${cluster_name}' does not exist"
        return 1
    fi
    
    # Get all nodes in the cluster
    local nodes
    nodes=$(kind get nodes --name "${cluster_name}" 2>/dev/null || echo "")
    
    if [ -z "${nodes}" ]; then
        log_error "Failed to get nodes for cluster '${cluster_name}'"
        return 1
    fi
    
    # Load image to all nodes explicitly
    # kind load docker-image loads to all nodes by default, but we can also specify nodes
    # This ensures the image is available on all nodes, including control-plane and workers
    local node_list
    node_list=$(echo "${nodes}" | tr '\n' ',' | sed 's/,$//')
    
    kind load docker-image "${image}" --name "${cluster_name}" --nodes "${node_list}" || {
        log_error "Failed to load image ${image} into kind cluster"
        return 1
    }
    
    log_success "Image ${image} loaded into kind cluster (all nodes)"
}

# Get kind cluster name from current context
get_kind_cluster_name() {
    local context
    context=$(kubectl config current-context 2>/dev/null || echo "")
    
    if [[ "${context}" == kind-* ]]; then
        echo "${context#kind-}"
    else
        echo ""
    fi
}

