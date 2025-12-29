# Building from Source

This guide explains how to build the BuildKit Controller from source.

## Prerequisites

- **Go 1.25+**: [Install Go](https://golang.org/doc/install)
- **kubectl**: [Install kubectl](https://kubernetes.io/docs/tasks/tools/)
- **Docker** (optional, for building container images)
- **make**: Usually pre-installed on Unix systems

## Quick Build

```bash
# Clone the repository
git clone https://github.com/smrt-devops/buildkit-controller.git
cd buildkit-controller

# Build both binaries
make build

# Binaries will be in bin/
ls bin/
# manager        # Controller binary
# gateway        # Gateway binary
# bkctl          # CLI tool
```

## Build Targets

### Build Binaries

```bash
# Build all binaries (controller, gateway, bkctl)
make build

# Build only controller
go build -o bin/manager ./cmd/controller

# Build only gateway
go build -o bin/gateway ./cmd/gateway

# Build only CLI tool
go build -o bin/bkctl ./cmd/bkctl
```

### Generate Code

```bash
# Generate DeepCopy methods
make generate

# Generate CRDs and RBAC manifests
make manifests
```

### Build Docker Images

By default, images are built for multiple architectures (amd64 and arm64).

**Prerequisites for Multi-Arch Builds:**

```bash
# Ensure Docker buildx is available
docker buildx version

# Create and use a buildx builder (if not already created)
docker buildx create --name multiarch --use
docker buildx inspect --bootstrap
```

**Build Commands:**

```bash
# Build controller image (multi-arch: amd64, arm64)
make docker-build IMG=ghcr.io/smrt-devops/buildkit-controller/controller:latest

# Build gateway image (multi-arch: amd64, arm64)
make docker-build-gateway GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:latest

# Build both images
make docker-build-all \
  IMG=ghcr.io/smrt-devops/buildkit-controller/controller:latest \
  GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:latest

# Build for specific architectures
make docker-build IMG=ghcr.io/smrt-devops/buildkit-controller/controller:latest PLATFORMS=linux/amd64,linux/arm64,linux/arm/v7

# Build and push to registry (for multi-arch)
make docker-push-all \
  IMG=ghcr.io/smrt-devops/buildkit-controller/controller:v1.0.0 \
  GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:v1.0.0

# Or build directly with Docker buildx
docker buildx build --platform linux/amd64,linux/arm64 \
  -t ghcr.io/smrt-devops/buildkit-controller/controller:latest \
  -f docker/Dockerfile . \
  --load
```

### Run Tests

```bash
# Run all tests
make test

# Run tests with coverage
make test-coverage
```

## Development Workflow

### 1. Set Up Development Environment

```bash
# Clone repository
git clone https://github.com/smrt-devops/buildkit-controller.git
cd buildkit-controller

# Download dependencies
go mod download

# Install development tools
make install-tools
```

### 2. Make Changes

```bash
# Edit code in your favorite editor
# ...

# Format code
make fmt

# Run linter
make vet

# Generate code (if you modified API types)
make generate
make manifests
```

### 3. Test Locally

```bash
# Run unit tests
make test

# Run controller locally (requires kubeconfig)
make run
```

### 4. Build and Deploy

```bash
# Build binaries
make build

# Build both images
make docker-build-all \
  IMG=ghcr.io/smrt-devops/buildkit-controller/controller:v1.0.0 \
  GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:v1.0.0

# Push images (or use make docker-push-all)
make docker-push-all \
  IMG=ghcr.io/smrt-devops/buildkit-controller/controller:v1.0.0 \
  GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:v1.0.0

# Deploy with Helm
helm install buildkit-controller ./helm/buildkit-controller \
  --set image.registry=ghcr.io/smrt-devops \
  --set image.name=buildkit-controller/controller \
  --set image.tag=v1.0.0 \
  --namespace buildkit-system \
  --create-namespace
```

## Dockerfile Structure

The project uses two separate Dockerfiles:

### Controller Dockerfile (`docker/Dockerfile`)

1. **Builder stage**: Compiles controller binary

   - Installs Go dependencies
   - Generates code with controller-gen
   - Builds controller binary

2. **Runtime stage**: Distroless base image (with CA certificates)
   - Uses `gcr.io/distroless/base:nonroot` (includes CA certs)
   - Copies CA certificates from builder (needed for Kubernetes API and OIDC HTTPS)
   - Copies controller binary
   - Uses distroless nonroot user (uid: 65532)

**Note:** Controller needs CA certificates for:

- HTTPS connections to Kubernetes API
- HTTPS connections to OIDC providers (e.g., GitHub Actions)

### Gateway Dockerfile (`docker/Dockerfile.gateway`)

1. **Builder stage**: Compiles gateway binary

   - Installs Go dependencies
   - Builds gateway binary

2. **Runtime stage**: Distroless static image
   - Minimal, secure base image
   - Copies gateway binary
   - Uses distroless nonroot user (uid: 65532)

**Why separate images?**

- Different update cycles (controller vs gateway)
- Better separation of concerns
- Smaller, more focused images
- Independent versioning
- Gateway runs as separate deployment per pool

### Customizing the Build

You can customize the Docker build:

```dockerfile
# Use a different Go version
FROM golang:1.26-alpine AS builder

# Build for different architectures
RUN CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build ...
```

## Cross-Compilation

Build for different platforms:

```bash
# Linux AMD64
GOOS=linux GOARCH=amd64 go build -o bin/manager-linux-amd64 ./cmd/controller
GOOS=linux GOARCH=amd64 go build -o bin/gateway-linux-amd64 ./cmd/gateway
GOOS=linux GOARCH=amd64 go build -o bin/bkctl-linux-amd64 ./cmd/bkctl

# Linux ARM64
GOOS=linux GOARCH=arm64 go build -o bin/manager-linux-arm64 ./cmd/controller
GOOS=linux GOARCH=arm64 go build -o bin/gateway-linux-arm64 ./cmd/gateway
GOOS=linux GOARCH=arm64 go build -o bin/bkctl-linux-arm64 ./cmd/bkctl

# macOS AMD64
GOOS=darwin GOARCH=amd64 go build -o bin/manager-darwin-amd64 ./cmd/controller
GOOS=darwin GOARCH=amd64 go build -o bin/gateway-darwin-amd64 ./cmd/gateway
GOOS=darwin GOARCH=amd64 go build -o bin/bkctl-darwin-amd64 ./cmd/bkctl
```

## Troubleshooting

### Build Fails with "no required module"

```bash
# Clean module cache and re-download
go clean -modcache
go mod download
```

### controller-gen Not Found

```bash
# Install controller-gen
make install-tools
```

### Docker Build Fails

```bash
# Check Docker is running
docker ps

# Check buildx is available
docker buildx version

# Create buildx builder if needed
docker buildx create --name multiarch --use
docker buildx inspect --bootstrap

# Build without cache
docker buildx build --platform linux/amd64,linux/arm64 \
  --no-cache \
  -t ghcr.io/smrt-devops/buildkit-controller/controller:latest \
  -f docker/Dockerfile . \
  --load

docker buildx build --platform linux/amd64,linux/arm64 \
  --no-cache \
  -t ghcr.io/smrt-devops/buildkit-controller/gateway:latest \
  -f docker/Dockerfile.gateway . \
  --load
```

### Import Errors

```bash
# Ensure you're in the repository root
pwd  # Should show .../buildkit-controller

# Tidy dependencies
go mod tidy

# Verify imports
go build ./...
```

## CI/CD Integration

### GitHub Actions Example (Multi-Arch)

```yaml
name: Build and Push

on:
  push:
    tags:
      - "v*"

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3

      - uses: actions/setup-go@v4
        with:
          go-version: "1.25"

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Login to Registry
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Build and Push Controller
        run: |
          make docker-push \
            IMG=ghcr.io/${{ github.repository }}/controller:${{ github.ref_name }} \
            PLATFORMS=linux/amd64,linux/arm64

      - name: Build and Push Gateway
        run: |
          make docker-push-gateway \
            GATEWAY_IMG=ghcr.io/${{ github.repository }}/gateway:${{ github.ref_name }} \
            PLATFORMS=linux/amd64,linux/arm64
```

## Release Build

For production releases with multi-architecture support:

```bash
# Set version
VERSION=v1.0.0

# Build with version info (optional, if you have version flags)
go build -ldflags "-X main.version=${VERSION}" -o bin/manager ./cmd/controller
go build -ldflags "-X main.version=${VERSION}" -o bin/gateway ./cmd/gateway
go build -ldflags "-X main.version=${VERSION}" -o bin/bkctl ./cmd/bkctl

# Build and push multi-arch Docker images
make docker-push-all \
  IMG=ghcr.io/smrt-devops/buildkit-controller/controller:${VERSION} \
  GATEWAY_IMG=ghcr.io/smrt-devops/buildkit-controller/gateway:${VERSION} \
  PLATFORMS=linux/amd64,linux/arm64

# Or manually with buildx
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t ghcr.io/smrt-devops/buildkit-controller/controller:${VERSION} \
  -f docker/Dockerfile \
  --push .

docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t ghcr.io/smrt-devops/buildkit-controller/gateway:${VERSION} \
  -f docker/Dockerfile.gateway \
  --push .
```

**Note:** For multi-arch builds, you need Docker buildx. Ensure it's set up:

```bash
# Create and use a buildx builder
docker buildx create --name multiarch --use
docker buildx inspect --bootstrap
```

## Next Steps

- [Deployment Guide](DEPLOYMENT.md) - Deploy your built image
- [Quick Start](QUICKSTART.md) - Get started quickly
- [Architecture](ARCHITECTURE.md) - Understand the system design
