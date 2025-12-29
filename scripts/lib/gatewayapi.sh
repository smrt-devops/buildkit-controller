#!/usr/bin/env bash
# GatewayAPI installation and management functions

set -euo pipefail

# Source common functions
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${LIB_DIR}/common.sh"

# GatewayAPI versions
ENVOY_GATEWAY_VERSION="${ENVOY_GATEWAY_VERSION:-v1.6.1}"

# Install Envoy Gateway
install_envoy_gateway() {
    local namespace="${1:-envoy-gateway-system}"
    
    log_step "Installing Envoy Gateway (v${ENVOY_GATEWAY_VERSION})..."
    
    # Check Helm version (OCI registry support requires Helm 3.8+)
    local helm_version
    helm_version=$(helm version --short 2>/dev/null | sed 's/[^0-9.]*\([0-9.]*\).*/\1/' || echo "0.0.0")
    local helm_major helm_minor
    helm_major=$(echo "${helm_version}" | cut -d. -f1)
    helm_minor=$(echo "${helm_version}" | cut -d. -f2)
    
    if [ "${helm_major}" -lt 3 ] || ([ "${helm_major}" -eq 3 ] && [ "${helm_minor}" -lt 8 ]); then
        log_error "Helm version ${helm_version} is too old. Envoy Gateway requires Helm 3.8+ for OCI registry support"
        log_info "Please upgrade Helm: https://helm.sh/docs/intro/install/"
        return 1
    fi
    
    ensure_namespace "${namespace}"
    
    # Install Envoy Gateway using OCI registry
    # Helm will install all CRDs automatically
    log_info "Installing from OCI registry: oci://docker.io/envoyproxy/gateway-helm"
    helm upgrade --install envoy-gateway \
        oci://docker.io/envoyproxy/gateway-helm \
        --namespace "${namespace}" \
        --create-namespace \
        --version "${ENVOY_GATEWAY_VERSION}" || {
        log_error "Failed to install Envoy Gateway"
        log_info "Troubleshooting:"
        log_info "  - Ensure Helm 3.8+ is installed: helm version"
        log_info "  - Check network connectivity to docker.io"
        log_info "  - Verify Envoy Gateway version exists: https://hub.docker.com/r/envoyproxy/gateway-helm/tags"
        log_info "  - Ensure CRDs are installed: kubectl get crd envoyproxies.gateway.envoyproxy.io"
        return 1
    }
    
    log_info "Waiting for Envoy Gateway to be ready..."
    kubectl wait --for=condition=available --timeout=300s \
        deployment/envoy-gateway -n "${namespace}" 2>/dev/null || {
        log_warning "Envoy Gateway may not be fully ready yet"
    }
    
    # Wait a moment for service to be created
    sleep 5
    
    # Get the NodePort (Envoy Gateway service name is typically envoy-gateway-envoy)
    local nodeport
    nodeport=$(kubectl get service -n "${namespace}" -l app.kubernetes.io/name=envoy-gateway \
        -o jsonpath='{.items[0].spec.ports[?(@.name=="http")].nodePort}' 2>/dev/null || echo "")
    
    if [ -z "${nodeport}" ]; then
        # Try alternative service name
        nodeport=$(kubectl get service envoy-gateway-envoy -n "${namespace}" \
            -o jsonpath='{.spec.ports[?(@.name=="http")].nodePort}' 2>/dev/null || echo "")
    fi
    
    if [ -n "${nodeport}" ]; then
        log_success "Envoy Gateway installed and accessible via NodePort ${nodeport}"
        echo "  Gateway API endpoint: http://localhost:${nodeport}"
    else
        log_warning "Envoy Gateway installed but NodePort not yet available"
        log_info "Check service with: kubectl get svc -n ${namespace} -l app.kubernetes.io/name=envoy-gateway"
    fi
    
    log_success "Envoy Gateway installed"
}

# Create EnvoyProxy resource for NodePort service type (dev only)
create_envoy_proxy_config() {
    local namespace="${1:-envoy-gateway-system}"
    local envoy_proxy_name="${2:-envoy-proxy-nodeport}"
    
    log_step "Creating EnvoyProxy config: ${envoy_proxy_name}"
    
    # Check if EnvoyProxy already exists
    if kubectl get envoyproxy "${envoy_proxy_name}" -n "${namespace}" &>/dev/null; then
        log_info "EnvoyProxy '${envoy_proxy_name}' already exists, skipping creation"
        return 0
    fi
    
    ensure_namespace "${namespace}"
    
    # Create EnvoyProxy resource with NodePort service type
    if ! kubectl apply -f - <<EOF; then
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: EnvoyProxy
metadata:
  name: ${envoy_proxy_name}
  namespace: ${namespace}
spec:
  provider:
    type: Kubernetes
    kubernetes:
      envoyService:
        type: NodePort
EOF
        log_error "Failed to create EnvoyProxy config"
        return 1
    fi
    
    log_success "EnvoyProxy '${envoy_proxy_name}' created"
}

# Create GatewayClass for Envoy Gateway (dev only)
create_gateway_class() {
    local gateway_class_name="${1:-envoy}"
    local namespace="${2:-envoy-gateway-system}"
    local envoy_proxy_name="${3:-envoy-proxy-nodeport}"
    
    log_step "Creating GatewayClass: ${gateway_class_name}"
    
    # Check if GatewayClass already exists
    if kubectl get gatewayclass "${gateway_class_name}" &>/dev/null; then
        log_info "GatewayClass '${gateway_class_name}' already exists, checking parametersRef..."
        # Check if it already has the correct parametersRef
        if kubectl get gatewayclass "${gateway_class_name}" -o jsonpath='{.spec.parametersRef.name}' 2>/dev/null | grep -q "${envoy_proxy_name}"; then
            log_info "GatewayClass already references EnvoyProxy config"
            return 0
        fi
        # Update existing GatewayClass to reference EnvoyProxy
        log_info "Updating GatewayClass to reference EnvoyProxy config"
        kubectl patch gatewayclass "${gateway_class_name}" --type=json -p="[{\"op\": \"add\", \"path\": \"/spec/parametersRef\", \"value\": {\"group\": \"gateway.envoyproxy.io\", \"kind\": \"EnvoyProxy\", \"name\": \"${envoy_proxy_name}\", \"namespace\": \"${namespace}\"}}]" && return 0 || {
            log_warning "Failed to patch GatewayClass, will try to recreate"
            kubectl delete gatewayclass "${gateway_class_name}" 2>/dev/null || true
        }
    fi
    
    # Create GatewayClass for Envoy Gateway with EnvoyProxy reference
    if ! kubectl apply -f - <<EOF; then
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: ${gateway_class_name}
spec:
  controllerName: gateway.envoyproxy.io/gatewayclass-controller
  description: GatewayClass for BuildKit Controller (dev environment)
  parametersRef:
    group: gateway.envoyproxy.io
    kind: EnvoyProxy
    name: ${envoy_proxy_name}
    namespace: ${namespace}
EOF
        log_error "Failed to create GatewayClass"
        return 1
    fi
    
    log_success "GatewayClass '${gateway_class_name}' created with EnvoyProxy reference"
}

# Create TLS secret for controller API Gateway (dev only)
create_api_gateway_tls_secret() {
    local secret_name="${1:-buildkit-controller-api-tls}"
    local namespace="${2:-buildkit-system}"
    local hostname="${3:-api.buildkit.local}"
    
    log_step "Creating TLS secret for API Gateway: ${secret_name}"
    
    # Ensure namespace exists (should already exist from Helm install, but be safe)
    ensure_namespace "${namespace}"
    
    # Check if secret already exists
    if kubectl get secret "${secret_name}" -n "${namespace}" &>/dev/null; then
        log_info "TLS secret '${secret_name}' already exists, skipping creation"
        return 0
    fi
    
    # Create self-signed certificate using openssl
    log_info "Generating self-signed certificate for ${hostname}"
    
    local temp_dir
    temp_dir=$(mktemp -d)
    trap "rm -rf ${temp_dir}" RETURN
    
    # Generate private key
    openssl genrsa -out "${temp_dir}/tls.key" 2048 2>/dev/null || {
        log_error "Failed to generate private key (openssl required)"
        return 1
    }
    
    # Generate certificate
    openssl req -new -x509 -key "${temp_dir}/tls.key" -out "${temp_dir}/tls.crt" \
        -days 365 -subj "/CN=${hostname}" \
        -addext "subjectAltName=DNS:${hostname},DNS:*.${hostname}" 2>/dev/null || {
        log_error "Failed to generate certificate"
        return 1
    }
    
    # Create Kubernetes secret
    kubectl create secret tls "${secret_name}" \
        --cert="${temp_dir}/tls.crt" \
        --key="${temp_dir}/tls.key" \
        -n "${namespace}" || {
        log_error "Failed to create TLS secret"
        return 1
    }
    
    log_success "TLS secret '${secret_name}' created for ${hostname}"
}

# Install GatewayAPI and Envoy Gateway
install_gateway_api() {
    local namespace="${1:-envoy-gateway-system}"
    local gateway_class_name="${2:-envoy}"
    local envoy_proxy_name="${3:-envoy-proxy-nodeport}"
    
    check_kubectl || return 1
    
    # Install Envoy Gateway (Helm will install all CRDs automatically)
    install_envoy_gateway "${namespace}"
    
    # Wait for CRDs to be established
    log_info "Waiting for CRDs to be established..."
    kubectl wait --for=condition=Established crd/gateways.gateway.networking.k8s.io --timeout=120s 2>/dev/null || true
    kubectl wait --for=condition=Established crd/httproutes.gateway.networking.k8s.io --timeout=120s 2>/dev/null || true
    kubectl wait --for=condition=Established crd/tcproutes.gateway.networking.k8s.io --timeout=120s 2>/dev/null || true
    kubectl wait --for=condition=Established crd/tlsroutes.gateway.networking.k8s.io --timeout=120s 2>/dev/null || true
    kubectl wait --for=condition=Established crd/envoyproxies.gateway.envoyproxy.io --timeout=120s 2>/dev/null || true
    
    # Create EnvoyProxy resource for NodePort service type (dev only)
    create_envoy_proxy_config "${namespace}" "${envoy_proxy_name}"
    
    # Create GatewayClass for dev environments (references EnvoyProxy)
    create_gateway_class "${gateway_class_name}" "${namespace}" "${envoy_proxy_name}"
    
    log_success "GatewayAPI stack installed and ready"
    echo ""
    echo "GatewayAPI is now available for use with:"
    echo "  - GatewayClassName: ${gateway_class_name}"
    echo "  - Controller API can use GatewayAPI for external access"
    echo "  - Pool gateways can use GatewayAPI for external access"
}

# Check if GatewayAPI is installed
gateway_api_installed() {
    kubectl get crd gateways.gateway.networking.k8s.io &>/dev/null && \
    kubectl get deployment envoy-gateway -n envoy-gateway-system &>/dev/null 2>/dev/null
}

# Get Envoy Gateway NodePort
get_envoy_gateway_nodeport() {
    local namespace="${1:-envoy-gateway-system}"
    local target_namespace="${2:-}"
    local gateway_name="${3:-}"
    
    # If gateway_name is provided, look specifically for that gateway
    if [ -n "${gateway_name}" ] && [ -n "${target_namespace}" ]; then
        # Look for service matching this specific gateway
        local service_name
        service_name=$(kubectl get service -n "${namespace}" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | \
            grep -E "^envoy-${target_namespace}-${gateway_name}-.*" | head -1 || echo "")
        
        if [ -n "${service_name}" ]; then
            # Get nodePort for port 1235 (pool gateway) first, then fallback to any port
            local nodeport
            nodeport=$(kubectl get service "${service_name}" -n "${namespace}" \
                -o jsonpath='{.spec.ports[?(@.port==1235)].nodePort}' 2>/dev/null || echo "")
            
            if [ -z "${nodeport}" ]; then
                # Fallback to any nodePort in this service
                nodeport=$(kubectl get service "${service_name}" -n "${namespace}" \
                    -o jsonpath='{.spec.ports[?(@.nodePort)].nodePort}' 2>/dev/null | head -1 || echo "")
            fi
            
            if [ -n "${nodeport}" ]; then
                echo "${nodeport}"
                return 0
            fi
        fi
    fi
    
    # If target_namespace is provided, look for Gateway service for that namespace
    # Service name pattern: envoy-<namespace>-<gateway-name>-<hash>
    if [ -n "${target_namespace}" ]; then
        # Get all Gateway resources in the target namespace and find matching services
        # We need to check each Gateway to find the right service
        local gateway_count
        if command_exists yq; then
            gateway_count=$(kubectl get gateway -n "${target_namespace}" -o yaml 2>/dev/null | \
                yq eval '.items | length' - 2>/dev/null || echo "0")
        else
            gateway_count=$(kubectl get gateway -n "${target_namespace}" -o jsonpath='{.items[*].metadata.name}' 2>/dev/null | \
                wc -w | tr -d ' ' || echo "0")
        fi
        
        # Try each Gateway to find the matching service
        # Prefer pool gateways (port 1235) over controller API (port 443) when multiple exist
        local found_nodeport=""
        local i=0
        while [ "${i}" -lt "${gateway_count}" ]; do
            local gateway_name gateway_port
            if command_exists yq; then
                gateway_name=$(kubectl get gateway -n "${target_namespace}" -o yaml 2>/dev/null | \
                    yq eval ".items[${i}].metadata.name" - 2>/dev/null || echo "")
                gateway_port=$(kubectl get gateway -n "${target_namespace}" -o yaml 2>/dev/null | \
                    yq eval ".items[${i}].spec.listeners[0].port" - 2>/dev/null || echo "")
            else
                gateway_name=$(kubectl get gateway -n "${target_namespace}" -o jsonpath="{.items[${i}].metadata.name}" 2>/dev/null || echo "")
                gateway_port=$(kubectl get gateway -n "${target_namespace}" -o jsonpath="{.items[${i}].spec.listeners[0].port}" 2>/dev/null || echo "")
            fi
            
            if [ -n "${gateway_name}" ] && [ -n "${gateway_port}" ]; then
                # Look for service matching this gateway (service name includes gateway name)
                local service_name
                service_name=$(kubectl get service -n "${namespace}" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | \
                    grep -E "^envoy-${target_namespace}-${gateway_name}-.*" | head -1 || echo "")
                
                if [ -n "${service_name}" ]; then
                    # Get nodePort for the port that matches the Gateway port
                    local nodeport
                    nodeport=$(kubectl get service "${service_name}" -n "${namespace}" \
                        -o jsonpath="{.spec.ports[?(@.port==${gateway_port})].nodePort}" 2>/dev/null || echo "")
                    
                    if [ -n "${nodeport}" ]; then
                        # Prefer pool gateway (port 1235) over controller API (port 443)
                        if [ "${gateway_port}" = "1235" ]; then
                            echo "${nodeport}"
                            return 0
                        elif [ "${gateway_port}" = "443" ] && [ -z "${found_nodeport}" ]; then
                            # Store controller API port as fallback
                            found_nodeport="${nodeport}"
                        elif [ -z "${found_nodeport}" ]; then
                            # Store any other port as fallback
                            found_nodeport="${nodeport}"
                        fi
                    fi
                fi
            fi
            i=$((i + 1))
        done
        
        # Return the found nodePort if we found one
        if [ -n "${found_nodeport}" ]; then
            echo "${found_nodeport}"
            return 0
        fi
        
        # Fallback: Look for any nodePort in services matching the pattern
        # (useful if Gateway resource isn't ready yet or port info isn't available)
        # Check all matching services and prefer pool gateway (port 1235) over controller API (port 443)
        local all_services
        all_services=$(kubectl get service -n "${namespace}" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | \
            grep -E "^envoy-${target_namespace}-.*" || echo "")
        
        if [ -n "${all_services}" ]; then
            local pool_nodeport="" controller_nodeport="" any_nodeport=""
            
            # Check each service for different port types
            while IFS= read -r service_name; do
                [ -z "${service_name}" ] && continue
                
                # Check for pool gateway port (1235)
                if [ -z "${pool_nodeport}" ]; then
                    pool_nodeport=$(kubectl get service "${service_name}" -n "${namespace}" \
                        -o jsonpath='{.spec.ports[?(@.port==1235)].nodePort}' 2>/dev/null || echo "")
                fi
                
                # Check for controller API port (443)
                if [ -z "${controller_nodeport}" ]; then
                    controller_nodeport=$(kubectl get service "${service_name}" -n "${namespace}" \
                        -o jsonpath='{.spec.ports[?(@.port==443)].nodePort}' 2>/dev/null || echo "")
                fi
                
                # Get any nodePort as last resort
                if [ -z "${any_nodeport}" ]; then
                    any_nodeport=$(kubectl get service "${service_name}" -n "${namespace}" \
                        -o jsonpath='{.spec.ports[?(@.nodePort)].nodePort}' 2>/dev/null | head -1 || echo "")
                fi
            done <<< "${all_services}"
            
            # Prefer pool gateway, then controller API, then any nodePort
            if [ -n "${pool_nodeport}" ]; then
                echo "${pool_nodeport}"
                return 0
            elif [ -n "${controller_nodeport}" ]; then
                echo "${controller_nodeport}"
                return 0
            elif [ -n "${any_nodeport}" ]; then
                echo "${any_nodeport}"
                return 0
            fi
        fi
    fi
    
    # Fallback: Try to find the service by label (Envoy Gateway control plane service)
    local nodeport
    nodeport=$(kubectl get service -n "${namespace}" -l app.kubernetes.io/name=envoy-gateway \
        -o jsonpath='{.items[0].spec.ports[?(@.name=="http" || @.port==80)].nodePort}' 2>/dev/null || echo "")
    
    if [ -z "${nodeport}" ]; then
        # Try alternative service name
        nodeport=$(kubectl get service envoy-gateway-envoy -n "${namespace}" \
            -o jsonpath='{.spec.ports[?(@.name=="http" || @.port==80)].nodePort}' 2>/dev/null || echo "")
    fi
    
    echo "${nodeport}"
}

# Get controller API endpoint via GatewayAPI
get_controller_api_endpoint() {
    local namespace="${1:-buildkit-system}"
    local gateway_namespace="${2:-envoy-gateway-system}"
    
    # Look specifically for the controller API gateway (port 443)
    # Service name pattern: envoy-<namespace>-<gateway-name>-<hash>
    # Controller API gateway name is typically: buildkit-controller-api-gateway
    local gateway_port=""
    
    # First, try to find the controller API gateway by name
    local gateway_name="buildkit-controller-api-gateway"
    local service_name
    service_name=$(kubectl get service -n "${gateway_namespace}" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | \
        grep -E "^envoy-${namespace}-${gateway_name}-.*" | head -1 || echo "")
    
    if [ -n "${service_name}" ]; then
        gateway_port=$(kubectl get service "${service_name}" -n "${gateway_namespace}" \
            -o jsonpath='{.spec.ports[?(@.port==443)].nodePort}' 2>/dev/null || echo "")
    fi
    
    # Fallback: look for any service with port 443 in envoy-gateway-system
    if [ -z "${gateway_port}" ]; then
        gateway_port=$(kubectl get service -n "${gateway_namespace}" -o jsonpath='{range .items[*]}{.spec.ports[?(@.port==443)].nodePort}{"\n"}{end}' 2>/dev/null | head -1 || echo "")
    fi
    
    if [ -n "${gateway_port}" ]; then
        # Check if controller API GatewayAPI is configured
        local hostname
        hostname=$(kubectl get httproute -n "${namespace}" -l app.kubernetes.io/name=buildkit-controller \
            -o jsonpath='{.items[0].spec.hostnames[0]}' 2>/dev/null || echo "")
        
        if [ -n "${hostname}" ]; then
            # Use hostname if configured (requires /etc/hosts entry)
            # TLS is enabled, so use https
            echo "https://${hostname}:${gateway_port}"
        else
            # Fallback to NodePort directly (TLS is enabled)
            echo "https://localhost:${gateway_port}"
        fi
    else
        echo ""
    fi
}

