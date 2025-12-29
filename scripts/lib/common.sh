#!/usr/bin/env bash
# Common utility functions for buildkit-controller scripts

set -euo pipefail

# Colors for output (only set if not already set to allow multiple sourcing)
if [ -z "${RED:-}" ]; then
    readonly RED='\033[0;31m'
    readonly GREEN='\033[0;32m'
    readonly YELLOW='\033[1;33m'
    readonly BLUE='\033[0;34m'
    readonly NC='\033[0m' # No Color
fi

# Default values (only set if not already set to allow multiple sourcing)
if [ -z "${DEFAULT_NAMESPACE:-}" ]; then
    readonly DEFAULT_NAMESPACE="${NAMESPACE:-buildkit-system}"
    readonly DEFAULT_CLUSTER_NAME="${CLUSTER_NAME:-buildkit-dev}"
    readonly DEFAULT_CONTROLLER_PORT="${CONTROLLER_PORT:-8082}"
    readonly DEFAULT_POOL_PORT="${POOL_PORT:-1235}"
fi

# Get script directory (works when sourced)
get_script_dir() {
    # When sourced from a script, BASH_SOURCE[1] is the calling script
    # When executed directly, BASH_SOURCE[0] is the script
    local source_file="${BASH_SOURCE[1]:-${BASH_SOURCE[0]:-$0}}"
    dirname "$(readlink -f "${source_file}" 2>/dev/null || echo "${source_file}")"
}

# Get project root directory
get_project_root() {
    # Get the directory of the script that called this
    # When sourced from a script, BASH_SOURCE[1] is the calling script
    # When executed directly, BASH_SOURCE[0] is the script
    local source_file="${BASH_SOURCE[1]:-${BASH_SOURCE[0]:-$0}}"
    local caller_dir
    caller_dir="$(dirname "$(readlink -f "${source_file}" 2>/dev/null || echo "${source_file}")")"
    
    # Handle different script locations
    if [[ "${caller_dir}" == */scripts/lib ]]; then
        # scripts/lib -> go up two levels
        dirname "$(dirname "${caller_dir}")"
    elif [[ "${caller_dir}" == */scripts/utils ]] || [[ "${caller_dir}" == */scripts/tests ]]; then
        # scripts/utils or scripts/tests -> go up one level to scripts, then one more to project root
        dirname "$(dirname "${caller_dir}")"
    elif [[ "${caller_dir}" == */scripts ]]; then
        # scripts/ -> go up one level
        dirname "${caller_dir}"
    else
        # Fallback: assume we're in project root
        echo "${caller_dir}"
    fi
}

# Logging functions
log_info() {
    echo -e "${BLUE}ℹ${NC} $*"
}

log_success() {
    echo -e "${GREEN}✓${NC} $*"
}

log_warning() {
    echo -e "${YELLOW}⚠${NC} $*"
}

log_error() {
    echo -e "${RED}✗${NC} $*" >&2
}

log_step() {
    echo ""
    echo -e "${BLUE}==>${NC} $*"
}

# Check if command exists
command_exists() {
    command -v "$1" >/dev/null 2>&1
}

# Check if kubectl is available and cluster is accessible
check_kubectl() {
    if ! command_exists kubectl; then
        log_error "kubectl is not installed or not in PATH"
        return 1
    fi
    
    if ! kubectl cluster-info &>/dev/null; then
        log_error "Not connected to a Kubernetes cluster"
        return 1
    fi
    
    return 0
}

# Check if kind cluster exists
kind_cluster_exists() {
    local cluster_name="${1:-${DEFAULT_CLUSTER_NAME}}"
    kind get clusters 2>/dev/null | grep -q "^${cluster_name}$"
}

# Check if namespace exists, create if it doesn't
ensure_namespace() {
    local namespace="${1:-${DEFAULT_NAMESPACE}}"
    kubectl create namespace "${namespace}" --dry-run=client -o yaml | kubectl apply -f - >/dev/null 2>&1
}

# Check if a port is in use
port_in_use() {
    local port="$1"
    lsof -ti:"${port}" &>/dev/null || nc -z localhost "${port}" &>/dev/null
}

# Check if a port-forward process is running
port_forward_running() {
    local pattern="$1"
    pgrep -f "${pattern}" &>/dev/null
}

# Kill port-forward processes matching pattern
kill_port_forward() {
    local pattern="$1"
    pkill -f "${pattern}" 2>/dev/null || true
}

# Start port-forward in background
start_port_forward() {
    local namespace="$1"
    local target="$2"
    local local_port="$3"
    local remote_port="${4:-${local_port}}"
    local log_file="${5:-/tmp/buildkit-port-forward.log}"
    local pattern="${6:-kubectl port-forward.*${target}.*${local_port}}"
    
    # Check if already running
    if port_forward_running "${pattern}"; then
        if port_in_use "${local_port}"; then
            log_info "Port-forward already running on port ${local_port}"
            return 0
        else
            log_warning "Port-forward process found but port not responding, restarting..."
            kill_port_forward "${pattern}"
            sleep 1
        fi
    fi
    
    # Start port-forward
    kubectl port-forward -n "${namespace}" "${target}" "${local_port}:${remote_port}" > "${log_file}" 2>&1 &
    local pf_pid=$!
    
    # Wait a moment to verify it started
    sleep 2
    if ! kill -0 "${pf_pid}" 2>/dev/null; then
        log_error "Port-forward failed to start. Check logs: ${log_file}"
        cat "${log_file}" 2>/dev/null || true
        return 1
    fi
    
    log_success "Port-forward started (PID: ${pf_pid})"
    echo "  Logs: ${log_file}"
    echo "  To stop: kill ${pf_pid} or pkill -f '${pattern}'"
    
    return 0
}

# Wait for pod to be ready
wait_for_pod() {
    local namespace="$1"
    local selector="$2"
    local timeout="${3:-120s}"
    
    kubectl wait --for=condition=Ready pod -l "${selector}" -n "${namespace}" --timeout="${timeout}" 2>/dev/null || {
        log_warning "Pod may not be ready yet"
        return 1
    }
}

# Wait for deployment to be available
wait_for_deployment() {
    local namespace="$1"
    local deployment="$2"
    local timeout="${3:-120s}"
    
    kubectl wait --for=condition=available "deployment/${deployment}" -n "${namespace}" --timeout="${timeout}" 2>/dev/null || {
        log_warning "Deployment may not be available yet"
        return 1
    }
}

# Get pod name by label selector
get_pod_by_label() {
    local namespace="$1"
    local selector="$2"
    kubectl get pods -n "${namespace}" -l "${selector}" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo ""
}

# Build Go binary
build_binary() {
    local project_root="$1"
    local binary_name="$2"
    local source_path="$3"
    local output_path="${project_root}/bin/${binary_name}"
    
    if [ -x "${output_path}" ]; then
        return 0
    fi
    
    log_info "Building ${binary_name}..."
    (cd "${project_root}" && go build -o "${output_path}" "${source_path}") || {
        log_error "Failed to build ${binary_name}"
        return 1
    }
    
    log_success "${binary_name} built"
    return 0
}

# Check if resource exists
resource_exists() {
    local resource_type="$1"
    local resource_name="$2"
    local namespace="${3:-}"
    
    if [ -n "${namespace}" ]; then
        kubectl get "${resource_type}" "${resource_name}" -n "${namespace}" &>/dev/null
    else
        kubectl get "${resource_type}" "${resource_name}" &>/dev/null
    fi
}

# Parse image reference (name:tag format)
parse_image() {
    local image="$1"
    local var_prefix="$2"
    
    if [[ "${image}" == *":"* ]]; then
        export "${var_prefix}_NAME"="${image%%:*}"
        export "${var_prefix}_TAG"="${image##*:}"
    else
        export "${var_prefix}_NAME"="${image}"
        export "${var_prefix}_TAG"="latest"
    fi
}

# Check if service is NodePort and get the port
get_nodeport() {
    local namespace="$1"
    local service="$2"
    local port_name="${3:-gateway}"
    
    local service_type
    service_type=$(kubectl get svc "${service}" -n "${namespace}" -o jsonpath='{.spec.type}' 2>/dev/null || echo "ClusterIP")
    
    if [ "${service_type}" = "NodePort" ]; then
        kubectl get svc "${service}" -n "${namespace}" -o jsonpath="{.spec.ports[?(@.name==\"${port_name}\")].nodePort}" 2>/dev/null || echo ""
    else
        echo ""
    fi
}

# Ensure metrics-server is installed and working
# Metrics-server is required for HPA and resource metrics
ensure_metrics_server() {
    # Check if metrics-server already exists and is working
    if kubectl get deployment metrics-server -n kube-system &>/dev/null; then
        log_info "metrics-server already exists, checking if it's working..."
        
        # Wait for metrics-server to be ready
        if kubectl wait --for=condition=available --timeout=60s deployment/metrics-server -n kube-system &>/dev/null; then
            # Give it a moment to start collecting metrics
            sleep 3
            
            # Test if metrics-server is responding
            if kubectl top nodes &>/dev/null 2>&1; then
                log_success "metrics-server is installed and working"
                return 0
            else
                log_warning "metrics-server exists but not responding, checking configuration..."
                # Check if it has the kind-specific flag
                if kubectl get deployment metrics-server -n kube-system -o jsonpath='{.spec.template.spec.containers[0].args}' 2>/dev/null | grep -q "kubelet-insecure-tls"; then
                    log_info "metrics-server has correct configuration, waiting a bit longer..."
                    sleep 10
                    if kubectl top nodes &>/dev/null 2>&1; then
                        log_success "metrics-server is now working"
                        return 0
                    fi
                fi
                log_warning "Reinstalling metrics-server with correct configuration..."
                kubectl delete deployment,service,apiservice -n kube-system metrics-server v1beta1.metrics.k8s.io --ignore-not-found=true 2>/dev/null || true
            fi
        else
            log_warning "metrics-server exists but not ready, reinstalling..."
            kubectl delete deployment,service,apiservice -n kube-system metrics-server v1beta1.metrics.k8s.io --ignore-not-found=true 2>/dev/null || true
        fi
    fi
    
    log_info "Installing metrics-server..."
    
    # Install metrics-server with kind-specific configuration
    # Kind requires --kubelet-insecure-tls flag because it uses self-signed certificates
    if ! kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml 2>/dev/null; then
        log_error "Failed to download metrics-server manifest"
        return 1
    fi
    
    # Patch metrics-server deployment for kind (disable TLS verification)
    # This is required because kind uses self-signed certificates
    # Check if the flag already exists before patching
    if ! kubectl get deployment metrics-server -n kube-system -o jsonpath='{.spec.template.spec.containers[0].args}' 2>/dev/null | grep -q "kubelet-insecure-tls"; then
        log_info "Patching metrics-server for kind (adding --kubelet-insecure-tls flag)..."
        kubectl patch deployment metrics-server -n kube-system --type='json' \
            -p='[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--kubelet-insecure-tls"}]' 2>/dev/null || {
            log_warning "Failed to patch metrics-server (may need manual configuration)"
        }
    else
        log_info "metrics-server already has --kubelet-insecure-tls flag"
    fi
    
    # Wait for metrics-server to be ready
    log_info "Waiting for metrics-server to be ready..."
    if kubectl wait --for=condition=available --timeout=120s deployment/metrics-server -n kube-system &>/dev/null; then
        # Give it a moment to start collecting metrics
        log_info "Waiting for metrics-server to start collecting metrics..."
        sleep 10
        
        # Test if metrics-server is responding
        local retries=0
        local max_retries=6
        while [ $retries -lt $max_retries ]; do
            if kubectl top nodes &>/dev/null 2>&1; then
                log_success "metrics-server is installed and working"
                return 0
            fi
            retries=$((retries + 1))
            if [ $retries -lt $max_retries ]; then
                log_info "Waiting for metrics-server to respond... (attempt $retries/$max_retries)"
                sleep 5
            fi
        done
        
        log_warning "metrics-server installed but not yet responding (may need more time to collect metrics)"
        log_info "You can test manually with: kubectl top nodes"
        return 0
    else
        log_warning "metrics-server installation may have issues, but continuing..."
        log_info "You can check status with: kubectl get deployment metrics-server -n kube-system"
        return 0
    fi
}

# Source this file to use the functions
# Usage: source "$(dirname "$0")/lib/common.sh"

