#!/usr/bin/env bash
# Unified development environment setup script
# Handles: kind cluster, GatewayAPI, images, Helm (with CRDs), and mock OIDC

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"
source "${SCRIPT_DIR}/lib/kind.sh"
source "${SCRIPT_DIR}/lib/helm.sh"
source "${SCRIPT_DIR}/lib/gatewayapi.sh"

PROJECT_ROOT="$(get_project_root)"

# Configuration
CLUSTER_NAME="${CLUSTER_NAME:-${DEFAULT_CLUSTER_NAME}}"
NAMESPACE="${NAMESPACE:-${DEFAULT_NAMESPACE}}"
INSTALL_GATEWAY_API="${INSTALL_GATEWAY_API:-true}"
INSTALL_MOCK_OIDC="${INSTALL_MOCK_OIDC:-true}"
NUM_WORKER_NODES="${NUM_WORKER_NODES:-3}"

log_step "BuildKit Controller Development Setup"
echo ""

# Step 1: Create kind cluster
log_info "Step 1: Setting up kind cluster with ${NUM_WORKER_NODES} worker node(s)..."
create_kind_cluster "${CLUSTER_NAME}" "${NUM_WORKER_NODES}"

# Step 1.5: Ensure metrics-server is installed and working
echo ""
log_info "Step 1.5: Ensuring metrics-server is installed and working..."
ensure_metrics_server

# Step 2: Install GatewayAPI (if requested)
if [ "${INSTALL_GATEWAY_API}" = "true" ]; then
    echo ""
    log_info "Step 2: Installing GatewayAPI stack..."
    if gateway_api_installed; then
        log_info "GatewayAPI already installed, ensuring GatewayClass exists..."
    else
        install_gateway_api
    fi
    # Always ensure GatewayClass exists (even if GatewayAPI was already installed)
    # This is needed for dev environments
    create_gateway_class "envoy"
else
    log_info "Step 2: Skipping GatewayAPI installation (set INSTALL_GATEWAY_API=false to disable)"
fi

# Step 3: Build and load images
echo ""
log_info "Step 3: Building and loading images..."
if ! kind_cluster_exists "${CLUSTER_NAME}"; then
    log_error "Kind cluster '${CLUSTER_NAME}' does not exist"
    exit 1
fi

CONTROLLER_IMAGE="${CONTROLLER_IMAGE:-buildkit-controller:dev}"
GATEWAY_IMAGE="${GATEWAY_IMAGE:-buildkit-controller-gateway:dev}"

log_info "Building controller image..."
docker build -t "${CONTROLLER_IMAGE}" -f "${PROJECT_ROOT}/docker/Dockerfile" "${PROJECT_ROOT}" || {
    log_error "Failed to build controller image"
    exit 1
}

log_info "Building gateway image..."
docker build -t "${GATEWAY_IMAGE}" -f "${PROJECT_ROOT}/docker/Dockerfile.gateway" "${PROJECT_ROOT}" || {
    log_error "Failed to build gateway image"
    exit 1
}

load_image_to_kind "${CONTROLLER_IMAGE}" "${CLUSTER_NAME}"
load_image_to_kind "${GATEWAY_IMAGE}" "${CLUSTER_NAME}"

# Step 4: Install Helm chart (CRDs are automatically installed by Helm)
echo ""
log_info "Step 4: Installing Helm chart (CRDs will be installed automatically)..."
HELM_CHART="${PROJECT_ROOT}/helm/buildkit-controller"
VALUES_FILE="${VALUES_FILE:-${HELM_CHART}/values-dev.yaml}"

# Build helm extra args from image environment variables
read -r -a HELM_EXTRA_ARGS <<< "$(build_helm_image_args)"

install_helm_chart "${HELM_CHART}" "buildkit-controller" "${NAMESPACE}" "${VALUES_FILE}" "${HELM_EXTRA_ARGS[@]}"

# Wait for CRDs to be established (Helm installs them but doesn't wait)
log_info "Waiting for CRDs to be established..."
kubectl wait --for=condition=Established crd/buildkitpools.buildkit.smrt-devops.net --timeout=60s 2>/dev/null || true
kubectl wait --for=condition=Established crd/buildkitworkers.buildkit.smrt-devops.net --timeout=60s 2>/dev/null || true
kubectl wait --for=condition=Established crd/buildkitoidcconfigs.buildkit.smrt-devops.net --timeout=60s 2>/dev/null || true

if kubectl get deployment buildkit-controller -n "${NAMESPACE}" &>/dev/null; then
    log_info "Restarting controller deployment to pick up new image..."
    kubectl rollout restart deployment/buildkit-controller -n "${NAMESPACE}" || true
    for gateway in $(kubectl get deployment -n "${NAMESPACE}" -l buildkit.smrt-devops.net/purpose=gateway -o jsonpath='{.items[*].metadata.name}' 2>/dev/null || echo ""); do
        if [ -n "${gateway}" ]; then
            log_info "Restarting gateway deployment ${gateway}..."
            kubectl rollout restart deployment/"${gateway}" -n "${NAMESPACE}" || true
        fi
    done
fi

# Wait for controller pod to be ready
log_info "Waiting for controller pod to be ready..."
wait_for_pod "${NAMESPACE}" "control-plane=buildkit-controller" "2m" || true

# Create TLS secret for controller API Gateway if GatewayAPI is enabled
# This must be done after Helm install since the namespace is created by Helm
if [ "${INSTALL_GATEWAY_API}" = "true" ]; then
    create_api_gateway_tls_secret "buildkit-controller-api-tls" "${NAMESPACE}" "api.buildkit.local"
fi

# Add controller API hostname to /etc/hosts if GatewayAPI is enabled
if [ "${INSTALL_GATEWAY_API}" = "true" ]; then
    source "${SCRIPT_DIR}/lib/hosts.sh"
    # Wait a bit for Gateway service to be created by Envoy Gateway
    log_info "Waiting for Gateway service to be created..."
    sleep 10
    
    # Find the Gateway service (Envoy Gateway creates it in envoy-gateway-system namespace)
    # Service name pattern: envoy-<namespace>-<gateway-name>-<hash>
    GATEWAY_SVC=$(kubectl get svc -n envoy-gateway-system -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | grep "buildkit-controller-api-gateway" | head -1 || echo "")
    
    if [ -n "${GATEWAY_SVC}" ]; then
        # Get NodePort (service should be NodePort due to annotation in Gateway resource)
        GATEWAY_PORT=$(kubectl get svc "${GATEWAY_SVC}" -n envoy-gateway-system -o jsonpath='{.spec.ports[?(@.port==443)].nodePort}' 2>/dev/null || echo "")
        if [ -n "${GATEWAY_PORT}" ]; then
            log_info "Found Gateway service NodePort: ${GATEWAY_PORT}"
            # Gateway API uses dynamic NodePorts that aren't mapped in Kind config
            # So we need to use a node IP instead of localhost
            # Get the first node IP (any node will work since NodePorts are accessible on all nodes)
            node_ip=$(get_first_node_ip)
            if [ -n "${node_ip}" ] && [ "${node_ip}" != "127.0.0.1" ]; then
                add_hostname "api.buildkit.local" "${node_ip}" || true
                log_info "Access API via: https://api.buildkit.local:${GATEWAY_PORT}"
                log_info "Note: Using node IP ${node_ip} (NodePort ${GATEWAY_PORT} not mapped to localhost)"
            else
                # Fallback to localhost (won't work for unmapped ports, but at least the entry exists)
                add_hostname_for_nodeport "api.buildkit.local" "true" || true
                log_warning "Using localhost (may not work - NodePort ${GATEWAY_PORT} not mapped in Kind config)"
                log_info "You may need to manually update /etc/hosts to use a node IP"
            fi
        else
            log_warning "Gateway service exists but no NodePort found yet, adding hostname to /etc/hosts anyway"
            add_hostname_for_nodeport "api.buildkit.local" "false" || true
        fi
    else
        log_warning "Could not find Gateway service, adding hostname to /etc/hosts anyway"
        add_hostname_for_nodeport "api.buildkit.local" "false" || true
    fi
fi

# Step 5: Test controller API endpoint
echo ""
log_info "Step 5: Testing controller API endpoint..."
API_POD=$(kubectl get pod -n "${NAMESPACE}" -l control-plane=buildkit-controller -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")

if [ -n "${API_POD}" ]; then
    # Wait for controller deployment to be available
    log_info "Waiting for controller deployment to be available..."
    if kubectl wait --for=condition=available deployment/buildkit-controller -n "${NAMESPACE}" --timeout=120s 2>/dev/null; then
        log_info "Controller deployment is available"
    else
        log_warning "Controller deployment may not be fully available yet"
    fi
    
    # Wait for controller pod to be ready
    log_info "Waiting for controller pod to be ready..."
    if kubectl wait --for=condition=ready pod -l control-plane=buildkit-controller -n "${NAMESPACE}" --timeout=60s 2>/dev/null; then
        log_info "Controller pod is ready"
    else
        log_warning "Controller pod may not be fully ready yet"
    fi
    
    # Test internal health endpoint via service
    log_info "Testing internal health endpoint..."
    # Test via ClusterIP service (no port-forward needed)
    # Use unique pod name to avoid conflicts
    TEST_POD_NAME="test-api-health-$(date +%s)"
    if kubectl run -n "${NAMESPACE}" --rm -i --restart=Never "${TEST_POD_NAME}" --image=curlimages/curl:latest -- \
        curl -sf "http://buildkit-controller-api.${NAMESPACE}.svc:8082/api/v1/health" 2>/dev/null | grep -qi "OK"; then
        log_success "Controller API is responding (internal check)"
        API_INTERNAL_OK=true
    else
        log_warning "Controller API health check failed (pod may still be starting)"
        API_INTERNAL_OK=false
    fi
    
    # Test external endpoint if GatewayAPI is enabled
    if [ "${INSTALL_GATEWAY_API}" = "true" ]; then
        source "${SCRIPT_DIR}/lib/gatewayapi.sh"
        CONTROLLER_API=$(get_controller_api_endpoint "${NAMESPACE}")
        # Get NodePort for controller API Gateway (port 443)
        GATEWAY_PORT=$(get_envoy_gateway_nodeport "envoy-gateway-system" "${NAMESPACE}" "buildkit-controller-api-gateway")
        
        if [ -n "${CONTROLLER_API}" ] || [ -n "${GATEWAY_PORT}" ]; then
            log_info "Testing external API endpoint via GatewayAPI..."
            # Wait for Gateway to be fully ready and programmed
            # Gateway programming can take 10-30 seconds after creation
            log_info "Waiting for Gateway to program routes (this may take up to 30 seconds)..."
            
            # Try multiple endpoints (GatewayAPI might return different formats)
            # Health endpoint returns "OK" (uppercase)
            API_REACHABLE=false
            MAX_RETRIES=12
            RETRY_DELAY=5
            
            for i in $(seq 1 ${MAX_RETRIES}); do
                # Try via CONTROLLER_API endpoint first
                if [ -n "${CONTROLLER_API}" ]; then
                    if curl -skf "${CONTROLLER_API}/api/v1/health" 2>/dev/null | grep -qi "OK"; then
                        API_REACHABLE=true
                        break
                    fi
                fi
                
                # Try via NodePort with hostname (use --resolve to point to node IP for Kind)
                # In Kind, NodePorts are accessible via node IP, not localhost
                if [ "${API_REACHABLE}" = "false" ] && [ -n "${GATEWAY_PORT}" ]; then
                    KIND_NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || echo "")
                    if [ -n "${KIND_NODE_IP}" ]; then
                        # Use --resolve to force DNS resolution to node IP (works with TLS cert for api.buildkit.local)
                        if curl -skf --resolve "api.buildkit.local:${GATEWAY_PORT}:${KIND_NODE_IP}" "https://api.buildkit.local:${GATEWAY_PORT}/api/v1/health" 2>/dev/null | grep -qi "OK"; then
                            API_REACHABLE=true
                            CONTROLLER_API="https://api.buildkit.local:${GATEWAY_PORT}"
                            break
                        fi
                    fi
                    # Fallback: try without --resolve (may fail TLS validation but works if cert allows IP)
                    if [ "${API_REACHABLE}" = "false" ]; then
                        if curl -skf "https://api.buildkit.local:${GATEWAY_PORT}/api/v1/health" 2>/dev/null | grep -qi "OK"; then
                            API_REACHABLE=true
                            CONTROLLER_API="https://api.buildkit.local:${GATEWAY_PORT}"
                            break
                        fi
                    fi
                fi
                
                # Try via localhost (works if port is mapped in Kind config, but 30949 is dynamic)
                if [ "${API_REACHABLE}" = "false" ] && [ -n "${GATEWAY_PORT}" ]; then
                    if curl -skf "https://localhost:${GATEWAY_PORT}/api/v1/health" 2>/dev/null | grep -qi "OK"; then
                        API_REACHABLE=true
                        CONTROLLER_API="https://localhost:${GATEWAY_PORT}"
                        break
                    fi
                fi
                
                # Try via Kind node IP directly (works but may fail TLS validation)
                if [ "${API_REACHABLE}" = "false" ] && [ -n "${GATEWAY_PORT}" ]; then
                    if [ -z "${KIND_NODE_IP}" ]; then
                        KIND_NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || echo "")
                    fi
                    if [ -n "${KIND_NODE_IP}" ]; then
                        # Skip TLS verification for IP access (cert is for hostname)
                        if curl -skf --insecure "https://${KIND_NODE_IP}:${GATEWAY_PORT}/api/v1/health" 2>/dev/null | grep -qi "OK"; then
                            API_REACHABLE=true
                            CONTROLLER_API="https://${KIND_NODE_IP}:${GATEWAY_PORT}"
                            break
                        fi
                    fi
                fi
                
                # If not reachable yet, wait and retry
                if [ "${API_REACHABLE}" = "false" ] && [ ${i} -lt ${MAX_RETRIES} ]; then
                    if [ ${i} -eq 1 ]; then
                        log_info "Gateway not ready yet, retrying (attempt ${i}/${MAX_RETRIES})..."
                    else
                        echo -n "."
                    fi
                    sleep ${RETRY_DELAY}
                fi
            done
            
            if [ ${i} -gt 1 ] && [ "${API_REACHABLE}" = "false" ]; then
                echo ""  # New line after dots
            fi
            
            if [ "${API_REACHABLE}" = "true" ]; then
                if [ -n "${CONTROLLER_API}" ]; then
                    log_success "Controller API is accessible via GatewayAPI at ${CONTROLLER_API}"
                elif [ -n "${GATEWAY_PORT}" ]; then
                    log_success "Controller API is accessible via GatewayAPI at https://localhost:${GATEWAY_PORT}"
                fi
            else
                log_warning "Controller API not yet accessible via GatewayAPI after ${MAX_RETRIES} attempts"
                log_info "Gateway may still be programming routes. This is normal and can take 30-60 seconds."
                if [ -n "${GATEWAY_PORT}" ]; then
                    log_info "You can test manually with: curl -sk https://api.buildkit.local:${GATEWAY_PORT}/api/v1/health"
                    log_info "Or check Gateway status: kubectl get gateway buildkit-controller-api-gateway -n ${NAMESPACE}"
                fi
            fi
        else
            log_warning "Could not determine GatewayAPI endpoint for controller API"
        fi
    else
        log_info "GatewayAPI not enabled, skipping external endpoint test"
        log_info "Controller API is available internally at: buildkit-controller.${NAMESPACE}.svc:8082"
    fi
else
    log_warning "Controller pod not found, skipping API endpoint test"
fi

# Step 6: Install mock OIDC (if requested)
if [ "${INSTALL_MOCK_OIDC}" = "true" ]; then
    echo ""
    log_info "Step 6: Installing mock OIDC server..."
    "${SCRIPT_DIR}/utils/deploy-mock-oidc.sh" || {
        log_warning "Failed to install mock OIDC (may already be installed)"
    }
fi

# Summary
echo ""
log_step "Setup Complete!"
echo ""
echo "Cluster: ${CLUSTER_NAME}"
echo "Namespace: ${NAMESPACE}"
echo ""

if [ "${INSTALL_GATEWAY_API}" = "true" ]; then
    GATEWAY_PORT=$(get_envoy_gateway_nodeport)
    if [ -n "${GATEWAY_PORT}" ]; then
        echo "GatewayAPI:"
        echo "  - Envoy Gateway NodePort: ${GATEWAY_PORT}"
        echo "  - Controller API: https://api.buildkit.local:${GATEWAY_PORT} (or https://localhost:${GATEWAY_PORT})"
    fi
fi

echo ""
echo "Controller API:"
echo "  - Service: buildkit-controller.${NAMESPACE}.svc:8082"
if [ "${INSTALL_GATEWAY_API}" = "true" ]; then
    GATEWAY_PORT=$(get_envoy_gateway_nodeport)
    if [ -n "${GATEWAY_PORT}" ]; then
        echo "  - Via GatewayAPI: https://api.buildkit.local:${GATEWAY_PORT} (hostname added to /etc/hosts)"
        echo "    Note: Uses self-signed certificate (use curl -k or curl --insecure)"
    else
        echo "  - Via GatewayAPI: https://api.buildkit.local (hostname added to /etc/hosts, port not yet available)"
    fi
fi

echo ""
echo "Mock OIDC:"
if [ "${INSTALL_MOCK_OIDC}" = "true" ]; then
    echo "  - Service: mock-oidc.${NAMESPACE}.svc:8888"
    echo "  - Config: local-mock-oidc (BuildKitOIDCConfig)"
else
    echo "  - Not installed (set INSTALL_MOCK_OIDC=true to install)"
fi

echo ""
echo "Next steps:"
echo "  - Check status: make dev-status"
echo "  - Create a pool: ./scripts/create-pool.sh [pool-name] [namespace]"
echo "    Example: ./scripts/create-pool.sh my-pool buildkit-system"
echo "  - Create pool with custom hostname: ./scripts/create-pool.sh my-pool --hostname my-pool.example.com"
echo "  - Run tests: make dev-test-buildx"
echo "  - Tear down: make dev-down"

