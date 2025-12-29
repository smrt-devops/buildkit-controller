#!/usr/bin/env bash
# Unified test runner script
# Usage: ./scripts/test.sh [test-type] [options]
# Test types: buildx, oidc, cache, setup, all

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"

PROJECT_ROOT="$(get_project_root)"
NAMESPACE="${NAMESPACE:-${DEFAULT_NAMESPACE}}"
POOL_NAME="${POOL_NAME:-minimal-pool}"

TEST_TYPE="${1:-all}"

# Run a specific test
run_test() {
    local test_name="$1"
    shift
    
    log_step "Running ${test_name} test"
    echo ""
    
    case "${test_name}" in
        buildx)
            "${SCRIPT_DIR}/tests/test-docker-buildx.sh" "$@"
            ;;
        oidc)
            "${SCRIPT_DIR}/tests/test-oidc.sh" "$@"
            ;;
        cache)
            "${SCRIPT_DIR}/tests/test-cache.sh" "$@"
            ;;
        setup|status)
            run_test_setup
            ;;
        *)
            log_error "Unknown test type: ${test_name}"
            echo ""
            echo "Available test types:"
            echo "  buildx  - Docker Buildx integration test"
            echo "  oidc    - OIDC authentication test"
            echo "  cache   - BuildKit cache test"
            echo "  setup   - Check development environment status"
            echo "  all     - Run all tests (default)"
            exit 1
            ;;
    esac
}

# Run test setup/status check
run_test_setup() {
    log_step "BuildKit Controller Test Setup"
    echo ""
    
    log_info "1. Checking controller status..."
    if resource_exists "deployment" "buildkit-controller" "${NAMESPACE}"; then
        kubectl get deployment buildkit-controller -n "${NAMESPACE}"
        echo ""
        kubectl get pods -n "${NAMESPACE}" -l control-plane=buildkit-controller
    else
        log_error "Controller deployment not found"
    fi
    
    echo ""
    log_info "2. Checking BuildKitPool status..."
    kubectl get buildkitpool -A
    
    echo ""
    log_info "3. Checking pool details..."
    local pool_name
    pool_name=$(kubectl get buildkitpool -n "${NAMESPACE}" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
    if [ -n "${pool_name}" ]; then
        echo "Pool: ${pool_name}"
        kubectl get buildkitpool "${pool_name}" -n "${NAMESPACE}" -o yaml | grep -A 10 "status:" || echo "  Status not yet available"
        
        echo ""
        log_info "4. Checking pool resources..."
        kubectl get statefulset,deployment,svc -n "${NAMESPACE}" | grep -E "(buildkit|${pool_name})" || echo "  Pool resources not yet created"
        
        echo ""
        log_info "5. Getting pool endpoint..."
        local endpoint
        endpoint=$(kubectl get buildkitpool "${pool_name}" -n "${NAMESPACE}" -o jsonpath='{.status.endpoint}' 2>/dev/null || echo "")
        if [ -n "${endpoint}" ]; then
            echo "  Endpoint: ${endpoint}"
        else
            echo "  Endpoint not yet available (pool may still be initializing)"
        fi
    else
        log_error "No BuildKitPool found"
    fi
    
    echo ""
    log_info "6. Checking API server..."
    local api_pod
    api_pod=$(get_pod_by_label "${NAMESPACE}" "control-plane=buildkit-controller")
    if [ -n "${api_pod}" ]; then
        echo "  API Pod: ${api_pod}"
        
        # Check GatewayAPI access
        source "${SCRIPT_DIR}/lib/gatewayapi.sh"
        local controller_api
        controller_api=$(get_controller_api_endpoint "${NAMESPACE}")
        
        if [ -n "${controller_api}" ]; then
            log_info "  Controller API via GatewayAPI: ${controller_api}"
            # Use -k to skip SSL verification (self-signed certs in dev)
            if curl -skf "${controller_api}/api/v1/health" 2>/dev/null | grep -qi "OK"; then
                log_success "API accessible via GatewayAPI at ${controller_api}"
            else
                log_warning "GatewayAPI configured but API not responding"
                log_info "  Try: curl -sk ${controller_api}/api/v1/health"
            fi
        else
            log_warning "Controller API GatewayAPI not configured"
        fi
        
        # Test health endpoint via ClusterIP service (pod may not have wget/curl)
        echo "  Testing health endpoint via ClusterIP service..."
        if kubectl run -n "${NAMESPACE}" --rm -i --restart=Never test-api-health --image=curlimages/curl:latest -- \
            curl -sf "http://buildkit-controller-api.${NAMESPACE}.svc:8082/api/v1/health" 2>/dev/null | grep -qi "OK"; then
            log_success "API is responding (internal check)"
        else
            log_warning "API not responding via ClusterIP (pod may still be starting)"
        fi
        
        echo ""
        echo "  Test endpoints:"
        if [ -n "${controller_api}" ]; then
            echo "  curl -sk ${controller_api}/api/v1/health"
        fi
        echo "  curl http://buildkit-controller-api.${NAMESPACE}.svc:8082/api/v1/health (from within cluster)"
    else
        log_error "Controller pod not found"
    fi
    
    echo ""
    log_info "7. Checking GatewayAPI configuration..."
    source "${SCRIPT_DIR}/lib/gatewayapi.sh"
    # Get NodePort for controller API Gateway (pass namespace to find the right service)
    local gateway_port
    gateway_port=$(get_envoy_gateway_nodeport "envoy-gateway-system" "${NAMESPACE}")
    if [ -n "${gateway_port}" ]; then
        log_success "GatewayAPI NodePort: ${gateway_port}"
        # Get Kind node IP for connection info
        local kind_node_ip
        kind_node_ip=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || echo "localhost")
        log_info "  Access via: https://api.buildkit.local:${gateway_port} (or https://${kind_node_ip}:${gateway_port})"
    else
        log_warning "GatewayAPI NodePort not found (GatewayAPI may not be installed or Gateway not yet programmed)"
    fi
    
    echo ""
    log_step "Next Steps"
    echo ""
    if [ -n "${pool_name}" ]; then
        echo "1. Wait for pool to be ready:"
        echo "   kubectl wait --for=condition=Ready buildkitpool/${pool_name} -n ${NAMESPACE} --timeout=5m"
    else
        echo "1. Create a pool: kubectl apply -f examples/pool-dev.yaml"
    fi
    echo ""
    echo "2. Create a client certificate (see examples/client-cert-example.yaml)"
    echo ""
    echo "3. Test with docker buildx: make dev-test-buildx"
    echo ""
}

# Run all tests
run_all_tests() {
    log_step "Running all tests"
    echo ""
    
    local failed_tests=()
    
    for test in setup buildx cache; do
        echo ""
        if ! run_test "${test}"; then
            failed_tests+=("${test}")
            log_warning "${test} test failed, continuing..."
        fi
    done
    
    echo ""
    if [ ${#failed_tests[@]} -eq 0 ]; then
        log_success "All tests passed!"
        return 0
    else
        log_error "Some tests failed: ${failed_tests[*]}"
        return 1
    fi
}

# Main
check_kubectl || exit 1

case "${TEST_TYPE}" in
    all)
        run_all_tests
        ;;
    *)
        run_test "${TEST_TYPE}" "${@:2}"
        ;;
esac

