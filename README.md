# BuildKit Kubernetes Controller

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Go Version](https://img.shields.io/badge/go-1.25+-00ADD8.svg)](https://golang.org)
[![Kubernetes](https://img.shields.io/badge/kubernetes-1.24+-326CE5.svg)](https://kubernetes.io/)
[![CI](https://github.com/smrt-devops/buildkit-controller/workflows/CI/badge.svg)](https://github.com/smrt-devops/buildkit-controller/actions/workflows/ci.yml)

A production-ready Kubernetes operator for managing scalable [BuildKit](https://github.com/moby/buildkit) daemon pools with enterprise-grade security, auto-scaling, and scale-to-zero capabilities.

> **Note**: This project is built using [Kubebuilder](https://kubebuilder.io/) and [controller-runtime](https://github.com/kubernetes-sigs/controller-runtime) to provide a robust Kubernetes operator framework. The **controller** (implemented in `cmd/controller/`) performs all operator tasks including resource reconciliation, certificate management, and auto-scaling. In Kubernetes terminology, an operator is a controller that manages custom resources - this controller implements that pattern.

## 🚀 Quick Start

```bash
# Install with Helm (recommended)
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace

# Create your first BuildKit pool
kubectl apply -f examples/pool-example.yaml
```

## ✨ Features

- **🔒 Enterprise Security**: Automatic ECDSA certificate generation with mTLS support
- **📈 Auto-Scaling**: Intelligent scale-to-zero and scale-up based on connection activity
- **🔐 Multiple Auth Methods**: mTLS, OIDC, and token-based authentication
- **🌐 HTTP API**: RESTful API for certificate retrieval and pool management
- **💾 Cache Backends**: Support for registry, S3, and local cache backends
- **📊 Observability**: Prometheus metrics and connection tracking
- **⚡ Lightweight**: Minimal resource footprint with efficient sidecar architecture

## 📖 Documentation

- **[Quick Start Guide](docs/QUICKSTART.md)** - Get up and running in minutes
- **[Deployment Guide](docs/DEPLOYMENT.md)** - Detailed installation instructions
- **[Architecture](docs/ARCHITECTURE.md)** - System design and components
- **[OIDC Setup](docs/OIDC-SETUP.md)** - Configure OIDC authentication
- **[Cache Testing](docs/CACHE-TESTING.md)** - Test and verify cache functionality
- **[Cross-Namespace Support](docs/CROSS-NAMESPACE.md)** - Multi-tenant pool management
- **[Build Guide](docs/BUILD.md)** - Building from source

## 🏗️ Architecture

The operator manages [BuildKit](https://github.com/moby/buildkit) daemon pools through two main Custom Resources:

1. **`BuildKitPool`** - Defines a pool with gateway and scaling configuration
2. **`BuildKitWorker`** - Ephemeral worker instances created on-demand for builds

### Components

- **Controller/Operator**: The main Kubernetes operator that reconciles `BuildKitPool` and `BuildKitWorker` resources. Built with [controller-runtime](https://github.com/kubernetes-sigs/controller-runtime), it handles resource lifecycle management, certificate generation, worker allocation, and auto-scaling.
- **Pool Gateway**: A dedicated deployment that terminates TLS connections, validates allocation tokens from client certificates, and routes connections to ephemeral workers. Each pool has its own gateway service.
- **Ephemeral Workers**: On-demand BuildKit daemon pods created as `BuildKitWorker` resources. Workers are allocated to jobs, used for builds, and automatically terminated when idle or expired.
- **HTTP API Server**: Embedded in the controller pod, provides REST APIs for worker allocation, certificate retrieval, and pool management via OIDC or ServiceAccount token authentication.

## 📦 Installation

### Prerequisites

- Kubernetes cluster (1.24+)
- `kubectl` configured to access your cluster
- Helm 3.0+ (recommended)

### Helm Installation (Recommended)

```bash
# Basic installation
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace

# Production installation
helm install buildkit-controller ./helm/buildkit-controller \
  -f helm/buildkit-controller/values-production.yaml \
  --namespace buildkit-system \
  --create-namespace
```

See the [Helm chart README](helm/buildkit-controller/README.md) for detailed configuration options.

### Building from Source

```bash
# Clone the repository
git clone https://github.com/smrt-devops/buildkit-controller.git
cd buildkit-controller

# Build binaries
make build

# Build Docker images (multi-arch: amd64, arm64)
make docker-build-all \
  IMG=ghcr.io/smrt-devops/buildkit-controller/controller:latest \
  GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:latest

# Build and push to registry
make docker-build-push-all \
  IMG=ghcr.io/smrt-devops/buildkit-controller/controller:v1.0.0 \
  GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:v1.0.0

# Generate manifests
make manifests
```

See [docs/BUILD.md](docs/BUILD.md) for detailed build instructions.

## 🎯 Usage

### 1. Create a BuildKit Pool

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata:
  name: my-buildkit-pool
spec:
  scaling:
    mode: auto
    min: 0 # Enable scale-to-zero
    max: 10
  resources:
    buildkit:
      requests:
        cpu: 1
        memory: 2Gi
      limits:
        cpu: 4
        memory: 4Gi
  tls:
    enabled: true
  auth:
    methods:
      - mtls
  gateway:
    enabled: true # Gateway is enabled by default
```

### 2. Allocate a Worker and Get Certificates

Workers are allocated on-demand via the HTTP API. Use the `bkctl` CLI tool:

```bash
# Allocate a worker and get certificates
bkctl allocate --pool my-buildkit-pool --ttl 1h

# This returns:
# - Worker allocation token
# - Client certificates with embedded allocation token
# - Gateway endpoint
```

Or use the HTTP API directly:

```bash
# Request worker allocation
curl -X POST http://buildkit-controller.buildkit-system.svc:8082/api/v1/workers/allocate \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"poolName": "my-buildkit-pool", "ttl": "1h"}'
```

### 3. Connect with Docker Buildx

```bash
# Use bkctl to allocate and build in one command
bkctl build --pool my-buildkit-pool -- \
  --platform linux/amd64,linux/arm64 \
  --tag myapp:latest \
  --push \
  .

# Or manually: allocate, then use certificates
bkctl allocate --pool my-buildkit-pool > certs.json
export CA_CERT=$(jq -r '.caCert' certs.json | base64 -d)
export CLIENT_CERT=$(jq -r '.clientCert' certs.json | base64 -d)
export CLIENT_KEY=$(jq -r '.clientKey' certs.json | base64 -d)

docker buildx create \
  --name my-builder \
  --driver remote \
  --driver-opt "cacert=$CA_CERT,cert=$CLIENT_CERT,key=$CLIENT_KEY" \
  tcp://my-buildkit-pool.default.svc:1235 \
  --use

docker buildx build \
  --builder my-builder \
  --platform linux/amd64,linux/arm64 \
  --tag myapp:latest \
  --push \
  .
```

See [examples/docker-buildx-usage.md](examples/docker-buildx-usage.md) for detailed usage examples.

> **Learn more**: [Docker Buildx Documentation](https://docs.docker.com/build/building/multi-platform/)

## 🔐 Authentication

### mTLS (Mutual TLS)

The default authentication method. Each client requires a certificate issued by the controller's CA.

### OIDC (OpenID Connect)

Configure OIDC for certificate requests via the HTTP API:

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: github-actions-oidc
spec:
  issuer: https://token.actions.githubusercontent.com
  audience: buildkit-controller
  enabled: true
  claimsMapping:
    user: "actor"
```

See [docs/OIDC-SETUP.md](docs/OIDC-SETUP.md) for complete setup instructions.

## 📊 Monitoring

The pool gateway exposes [Prometheus](https://prometheus.io/) metrics on port 9090:

- `buildkit_gateway_active_connections{pool="pool-name"}` - Current active gateway connections
- `buildkit_gateway_connections_total{pool="pool-name",status="success|error"}` - Total gateway connections
- `buildkit_gateway_connection_duration_seconds{pool="pool-name"}` - Gateway connection duration

The controller also tracks worker lifecycle metrics:

- `buildkit_workers_total{pool="pool-name",phase="idle|allocated|running"}` - Worker count by phase
- `buildkit_worker_allocations_total{pool="pool-name"}` - Total worker allocations

These metrics can be scraped by Prometheus and visualized in [Grafana](https://grafana.com/) or other monitoring tools.

## 🔧 Configuration

### Resource Sizes

- **sm**: 500m CPU / 512Mi memory (request), 2 CPU / 2Gi memory (limit)
- **md**: 1 CPU / 2Gi memory (request), 4 CPU / 4Gi memory (limit)
- **lg**: 2 CPU / 4Gi memory (request), 8 CPU / 8Gi memory (limit)
- **xl**: 4 CPU / 8Gi memory (request), 16 CPU / 16Gi memory (limit)

### Scale-to-Zero

Pools support true scale-to-zero with ephemeral workers:

```yaml
spec:
  scaling:
    mode: auto
    min: 0 # No workers when idle
    max: 10 # Maximum concurrent workers
```

Workers are created on-demand when clients request allocations and automatically terminated when:

- The allocation expires (based on TTL)
- The worker becomes idle after the build completes
- The pool scales down due to inactivity

The gateway remains running (typically 1 replica) to handle incoming connection requests.

### Cache Backends

```yaml
spec:
  cache:
    backends:
      - type: registry
        registry:
          endpoint: "registry.example.com/cache"
      - type: s3
        s3:
          bucket: my-cache-bucket
          region: us-west-2
```

## 📚 Examples

The `examples/` directory contains:

- `pool-example.yaml` - Basic pool configuration with gateway
- `pool-with-oidc.yaml` - Pool with OIDC authentication
- `pool-with-registry-cache.yaml` - Pool with registry cache backend
- `pool-with-local-cache.yaml` - Pool with local cache configuration
- `scale-to-zero-example.yaml` - Scale-to-zero configuration
- `oidc-config-example.yaml` - OIDC configuration
- `github-actions-workflow.yaml` - Complete CI/CD workflow
- `docker-buildx-usage.md` - Detailed usage guide

## 🛠️ Development

### Prerequisites

- Go 1.25+
- kubectl
- Kubernetes cluster (or kind/minikube)
- Docker (for building images)

### Running Locally

```bash
# Install dependencies
go mod download

# Generate code
make generate
make manifests

# Run controller locally
make run
```

### Testing

```bash
# Run tests
make test

# Run with coverage
make test-coverage
```

### Building

```bash
# Build binaries
make build

# Build Docker image
make docker-build IMG=ghcr.io/smrt-devops/buildkit-controller/controller:latest
```

## 🤝 Contributing

Contributions are welcome! Please see our [Contributing Guide](CONTRIBUTING.md) for details.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add some amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## 📄 License

Copyright 2024 SMRT DevOps.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.

## 🔗 Links & Resources

### Core Technologies

- **[BuildKit](https://github.com/moby/buildkit)** - The underlying build engine
- **[Docker Buildx](https://docs.docker.com/build/building/multi-platform/)** - Multi-platform builds
- **[Kubernetes](https://kubernetes.io/)** - Container orchestration platform
- **[Helm](https://helm.sh/)** - Package manager for Kubernetes

### Kubernetes & Operators

- **[Kubernetes Operators](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/)** - Operator pattern documentation
- **[Controller Runtime](https://github.com/kubernetes-sigs/controller-runtime)** - Framework for building Kubernetes controllers
- **[Kubebuilder](https://kubebuilder.io/)** - SDK for building Kubernetes APIs using CRDs
- **[Custom Resources](https://kubernetes.io/docs/concepts/extend-kubernetes/api-extension/custom-resources/)** - Kubernetes CRD documentation

### Authentication & Security

- **[OIDC](https://openid.net/connect/)** - OpenID Connect specification
- **[GitHub Actions OIDC](https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/about-security-hardening-with-openid-connect)** - OIDC with GitHub Actions
- **[mTLS](https://en.wikipedia.org/wiki/Mutual_authentication)** - Mutual TLS authentication

### Observability

- **[Prometheus](https://prometheus.io/)** - Monitoring and alerting toolkit
- **[Prometheus Client Go](https://github.com/prometheus/client_golang)** - Prometheus Go client library

### Container Images

- **[Distroless](https://github.com/GoogleContainerTools/distroless)** - Language-focused container images

## 🙏 Acknowledgments

This project builds upon and integrates with several excellent open-source projects:

- **[BuildKit](https://github.com/moby/buildkit)** by Docker/Moby - The powerful build engine
- **[Kubernetes Controller Runtime](https://github.com/kubernetes-sigs/controller-runtime)** - Controller framework
- **[Kubebuilder](https://kubebuilder.io/)** - CRD and controller scaffolding
- **[Prometheus](https://prometheus.io/)** - Metrics collection
- **[go-oidc](https://github.com/coreos/go-oidc)** - OIDC client library
- **[Distroless](https://github.com/GoogleContainerTools/distroless)** - Secure container base images

## 💬 Support

- **Issues**: [GitHub Issues](https://github.com/smrt-devops/buildkit-controller/issues)
- **Discussions**: [GitHub Discussions](https://github.com/smrt-devops/buildkit-controller/discussions)
- **Documentation**: [Full Documentation](docs/README.md)

---

Made with ❤️ by the SMRT DevOps team
