#!/usr/bin/env bash
# /etc/hosts management for local development

set -euo pipefail

# Source common functions
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${LIB_DIR}/common.sh"

# Local domain for kind clusters
LOCAL_DOMAIN="${LOCAL_DOMAIN:-buildkit.local}"

# Add hostname to /etc/hosts
# If the hostname already exists with a different IP, it will be updated
add_hostname() {
    local hostname="$1"
    local ip="${2:-127.0.0.1}"
    
    if [ "$(uname)" = "Darwin" ]; then
        # macOS
        if grep -q ".*${hostname}" /etc/hosts 2>/dev/null; then
            # Hostname exists, check if IP needs updating
            if grep -q "^${ip}.*${hostname}" /etc/hosts 2>/dev/null; then
                log_info "${hostname} already in /etc/hosts with correct IP ${ip}"
            else
                # Update existing entry
                log_info "Updating ${hostname} in /etc/hosts to ${ip} (requires sudo)"
                sudo sed -i '' "/.*${hostname}/d" /etc/hosts 2>/dev/null || {
                    log_warning "Failed to remove old entry from /etc/hosts"
                }
                echo "${ip} ${hostname}" | sudo tee -a /etc/hosts >/dev/null || {
                    log_warning "Failed to add to /etc/hosts (may need manual entry)"
                    echo "  Add this line to /etc/hosts: ${ip} ${hostname}"
                    return 1
                }
                log_success "Updated ${hostname} -> ${ip} in /etc/hosts"
            fi
        else
            # Hostname doesn't exist, add it
            log_info "Adding ${hostname} to /etc/hosts (requires sudo)"
            echo "${ip} ${hostname}" | sudo tee -a /etc/hosts >/dev/null || {
                log_warning "Failed to add to /etc/hosts (may need manual entry)"
                echo "  Add this line to /etc/hosts: ${ip} ${hostname}"
                return 1
            }
            log_success "Added ${hostname} -> ${ip} to /etc/hosts"
        fi
    else
        # Linux
        if grep -q ".*${hostname}" /etc/hosts 2>/dev/null; then
            # Hostname exists, check if IP needs updating
            if grep -q "^${ip}.*${hostname}" /etc/hosts 2>/dev/null; then
                log_info "${hostname} already in /etc/hosts with correct IP ${ip}"
            else
                # Update existing entry
                log_info "Updating ${hostname} in /etc/hosts to ${ip} (requires sudo)"
                sudo sed -i "/.*${hostname}/d" /etc/hosts 2>/dev/null || {
                    log_warning "Failed to remove old entry from /etc/hosts"
                }
                echo "${ip} ${hostname}" | sudo tee -a /etc/hosts >/dev/null || {
                    log_warning "Failed to add to /etc/hosts (may need manual entry)"
                    echo "  Add this line to /etc/hosts: ${ip} ${hostname}"
                    return 1
                }
                log_success "Updated ${hostname} -> ${ip} in /etc/hosts"
            fi
        else
            # Hostname doesn't exist, add it
            log_info "Adding ${hostname} to /etc/hosts (requires sudo)"
            echo "${ip} ${hostname}" | sudo tee -a /etc/hosts >/dev/null || {
                log_warning "Failed to add to /etc/hosts (may need manual entry)"
                echo "  Add this line to /etc/hosts: ${ip} ${hostname}"
                return 1
            }
            log_success "Added ${hostname} -> ${ip} to /etc/hosts"
        fi
    fi
}

# Remove hostname from /etc/hosts
remove_hostname() {
    local hostname="$1"
    
    if [ "$(uname)" = "Darwin" ]; then
        # macOS
        if grep -q ".*${hostname}" /etc/hosts 2>/dev/null; then
            log_info "Removing ${hostname} from /etc/hosts (requires sudo)"
            sudo sed -i '' "/.*${hostname}/d" /etc/hosts 2>/dev/null || {
                log_warning "Failed to remove from /etc/hosts (may need manual removal)"
                return 1
            }
            log_success "Removed ${hostname} from /etc/hosts"
        fi
    else
        # Linux
        if grep -q ".*${hostname}" /etc/hosts 2>/dev/null; then
            log_info "Removing ${hostname} from /etc/hosts (requires sudo)"
            sudo sed -i "/.*${hostname}/d" /etc/hosts 2>/dev/null || {
                log_warning "Failed to remove from /etc/hosts (may need manual removal)"
                return 1
            }
            log_success "Removed ${hostname} from /etc/hosts"
        fi
    fi
}

# Generate a local hostname for a pool
generate_pool_hostname() {
    local pool_name="$1"
    local namespace="${2:-${DEFAULT_NAMESPACE}}"
    
    if [ "${namespace}" = "${DEFAULT_NAMESPACE}" ]; then
        echo "${pool_name}.${LOCAL_DOMAIN}"
    else
        echo "${pool_name}.${namespace}.${LOCAL_DOMAIN}"
    fi
}

# Get Envoy Gateway IP (localhost for kind)
get_gateway_ip() {
    echo "127.0.0.1"
}

# Get all Kind node IPs (for NodePort access via any node)
# Returns space-separated list of node IPs
get_all_node_ips() {
    kubectl get nodes -o jsonpath='{range .items[*]}{.status.addresses[?(@.type=="InternalIP")].address}{" "}{end}' 2>/dev/null | tr -s ' ' | sed 's/^ *//;s/ *$//' || echo ""
}

# Get first Kind node IP (for backward compatibility)
get_first_node_ip() {
    kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || echo "127.0.0.1"
}

# Add hostname for NodePort access (handles multiple nodes)
# For kind clusters, NodePorts are accessible via localhost due to port mappings,
# but this function can also add entries for all node IPs if needed
add_hostname_for_nodeport() {
    local hostname="$1"
    local use_localhost="${2:-true}"  # Default to localhost for kind
    
    if [ "${use_localhost}" = "true" ]; then
        # Use localhost - works for all nodes since kind maps NodePorts to localhost
        add_hostname "${hostname}" "127.0.0.1"
    else
        # Add entries for all node IPs (one entry per IP)
        local node_ips
        node_ips=$(get_all_node_ips)
        if [ -n "${node_ips}" ]; then
            # Use first node IP (can't have multiple IPs for same hostname in /etc/hosts)
            local first_ip
            first_ip=$(echo "${node_ips}" | cut -d' ' -f1)
            add_hostname "${hostname}" "${first_ip}"
            log_info "NodePort accessible via any node IP: ${node_ips}"
        else
            # Fallback to localhost
            add_hostname "${hostname}" "127.0.0.1"
        fi
    fi
}
