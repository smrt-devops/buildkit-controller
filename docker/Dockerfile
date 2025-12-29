# Builder stage
FROM golang:1.25-alpine AS builder

WORKDIR /workspace

# Install build dependencies
RUN apk add --no-cache git bash make

# Copy go mod files
COPY go.mod go.mod
COPY go.sum go.sum

# Download dependencies
RUN --mount=type=cache,target=/go/pkg/mod go mod download

# Install controller-gen
RUN --mount=type=cache,target=/go/pkg/mod --mount=type=cache,target=/root/.cache/go-build \
    go install sigs.k8s.io/controller-tools/cmd/controller-gen@v0.19.0

# Copy source code
COPY cmd/ cmd/
COPY api/ api/
COPY internal/ internal/
COPY hack/ hack/

# Generate code
RUN controller-gen object:headerFile="hack/boilerplate.go.txt" paths="./..."

# Build the binary
RUN --mount=type=cache,target=/go/pkg/mod --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 GOOS=linux GOARCH=$TARGETARCH \
    go build -trimpath -ldflags="-w -s" -o manager ./cmd/controller

# Final stage - distroless with CA certificates
FROM gcr.io/distroless/base:nonroot

WORKDIR /

# Copy CA certificates (needed for Kubernetes API and OIDC HTTPS requests)
COPY --from=gcr.io/distroless/base:nonroot /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Copy the binary from builder
COPY --from=builder /workspace/manager /manager

# Use distroless nonroot user (uid: 65532, gid: 65532)
USER nonroot:nonroot

ENTRYPOINT ["/manager"]
