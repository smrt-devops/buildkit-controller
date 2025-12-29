#!/usr/bin/env bash
# Unified development environment teardown script

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/common.sh"
source "${SCRIPT_DIR}/lib/kind.sh"

CLUSTER_NAME="${CLUSTER_NAME:-${DEFAULT_CLUSTER_NAME}}"

log_step "Tearing down development environment"
echo ""

# Port-forwards are no longer used (GatewayAPI is used instead)
# Keeping this section for any legacy port-forwards that might exist
log_info "Cleaning up any legacy port-forward processes..."
kill_port_forward "kubectl port-forward.*buildkit-controller" || true
kill_port_forward "kubectl port-forward.*minimal-pool" || true

log_info "Deleting kind cluster..."
delete_kind_cluster "${CLUSTER_NAME}"

log_success "Development environment torn down"

