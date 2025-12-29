# BuildKit Controller Scripts

This directory contains scripts for managing the BuildKit Controller development environment and testing.

## Directory Structure

```
scripts/
├── dev-setup.sh          # Main setup entrypoint
├── dev-teardown.sh       # Main teardown entrypoint
├── create-pool.sh        # Pool creation entrypoint
├── test.sh               # Test runner entrypoint
├── lib/                  # Shared library functions
│   ├── common.sh
│   ├── kind.sh
│   ├── helm.sh
│   ├── gatewayapi.sh
│   └── hosts.sh
├── tests/                # Test scripts
│   ├── test-docker-buildx.sh
│   ├── test-oidc.sh
│   └── test-cache.sh
└── utils/                # Utility scripts
    └── deploy-mock-oidc.sh
```

## Entrypoint Scripts

All main entrypoints are in the root `scripts/` directory:

### `dev-setup.sh`

Unified development environment setup script. Handles:

- Kind cluster creation
- GatewayAPI installation (Envoy Gateway)
- Docker image building and loading
- CRD installation
- Helm chart installation
- Mock OIDC server installation (always installed by default)

**Note**: Pools are NOT created during setup. Use `create-pool.sh` to create pools with different configurations.

**Usage:**

```bash
# Basic setup (includes mock OIDC)
./scripts/dev-setup.sh

# Skip GatewayAPI installation
INSTALL_GATEWAY_API=false ./scripts/dev-setup.sh

# Skip mock OIDC installation
INSTALL_MOCK_OIDC=false ./scripts/dev-setup.sh
```

### `create-pool.sh`

Create BuildKitPool with automatic GatewayAPI configuration and hostname management.

**Usage:**

```bash
# Create pool with auto-generated hostname
./scripts/create-pool.sh [pool-name] [namespace]

# Examples:
./scripts/create-pool.sh my-pool buildkit-system
./scripts/create-pool.sh test-pool default

# Custom hostname
./scripts/create-pool.sh my-pool buildkit-system --hostname my-pool.example.com

# Disable GatewayAPI (use NodePort)
./scripts/create-pool.sh my-pool buildkit-system --no-gateway-api

# TLS terminate mode
./scripts/create-pool.sh my-pool buildkit-system --tls-mode terminate

# Use custom pool file
./scripts/create-pool.sh my-pool buildkit-system --file examples/pool-example.yaml
```

### `dev-teardown.sh`

Unified teardown script that cleans up the development environment.

**Usage:**

```bash
./scripts/dev-teardown.sh
```

### `test.sh`

Unified test runner that can run individual tests or all tests.

**Usage:**

```bash
# Run all tests
./scripts/test.sh all

# Run specific test
./scripts/test.sh buildx
./scripts/test.sh oidc
./scripts/test.sh cache
./scripts/test.sh setup  # Check environment status
```

## Subdirectories

### `lib/` - Shared Library Functions

Shared utility functions organized by purpose:

- **`common.sh`** - Common utilities (logging, kubectl checks, port-forward management)
- **`kind.sh`** - Kind cluster management
- **`helm.sh`** - Helm chart installation and management
- **`gatewayapi.sh`** - GatewayAPI and Envoy Gateway installation
- **`hosts.sh`** - `/etc/hosts` management for local hostname resolution

### `tests/` - Test Scripts

Individual test scripts that can be run directly or via `test.sh`:

- **`test-docker-buildx.sh`** - Docker Buildx integration test
- **`test-oidc.sh`** - OIDC authentication test
- **`test-cache.sh`** - BuildKit cache test

```bash
# Via test runner (recommended)
./scripts/test.sh buildx
./scripts/test.sh oidc
./scripts/test.sh cache

# Or directly
./scripts/tests/test-docker-buildx.sh
./scripts/tests/test-oidc.sh
./scripts/tests/test-cache.sh
```

### `utils/` - Utility Scripts

Utility scripts for specific tasks:

- **`deploy-mock-oidc.sh`** - Deploy mock OIDC server for testing

```bash
./scripts/utils/deploy-mock-oidc.sh
```

## GatewayAPI Support

The development environment supports GatewayAPI for exposing services without port-forwarding:

1. **Automatic Installation**: GatewayAPI (Envoy Gateway) is installed by default during `dev-setup.sh`
2. **NodePort Access**: Envoy Gateway service is configured as NodePort for kind cluster access
3. **Hostname Management**: Pool creation script automatically:

   - Generates hostnames (e.g., `pool-name.buildkit.local`)
   - Adds entries to `/etc/hosts` for local resolution
   - Configures TLS passthrough (default) or terminate mode

4. **Creating Pools with GatewayAPI**:

   ```bash
   # Auto-generate hostname
   ./scripts/create-pool.sh my-pool buildkit-system

   # Custom hostname
   ./scripts/create-pool.sh my-pool buildkit-system --hostname my-pool.example.com

   # Disable GatewayAPI (use NodePort instead)
   ./scripts/create-pool.sh my-pool buildkit-system --no-gateway-api

   # TLS terminate mode
   ./scripts/create-pool.sh my-pool buildkit-system --tls-mode terminate
   ```

5. **Hostname Resolution**: The script automatically adds hostnames to `/etc/hosts` pointing to `127.0.0.1`. For kind clusters, Envoy Gateway NodePort handles routing.

## Environment Variables

Common environment variables used across scripts:

- `CLUSTER_NAME` - Kind cluster name (default: `buildkit-dev`)
- `NAMESPACE` - Kubernetes namespace (default: `buildkit-system`)
- `CONTROLLER_IMAGE` - Controller image name (default: `buildkit-controller:dev`)
- `GATEWAY_IMAGE` - Gateway image name (default: `buildkit-controller-gateway:dev`)
- `INSTALL_GATEWAY_API` - Install GatewayAPI (default: `true`)
- `INSTALL_MOCK_OIDC` - Install mock OIDC server (default: `true`)
- `LOCAL_DOMAIN` - Local domain for hostname generation (default: `buildkit.local`)

## Makefile Integration

The Makefile provides convenient targets:

```bash
make dev              # Full setup (no pools created)
make dev-pool         # Create default pool
make dev-down         # Teardown
make dev-status       # Check status
make dev-test         # Run all tests
make dev-test-buildx  # Run buildx test
make dev-reload-images # Reload images after code changes
```
