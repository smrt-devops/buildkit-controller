# Architecture Overview

## System Design

The BuildKit Controller implements a Kubernetes operator that manages BuildKit daemon pools using an **ephemeral worker architecture** with a **pool gateway** for connection routing. This design enables true scale-to-zero, efficient resource utilization, and secure multi-tenant access.

## Core Components

### 1. Controller/Operator

The main Kubernetes operator that reconciles Custom Resource Definitions (CRDs):

- **`BuildKitPool`** - Defines pool configuration, gateway settings, and scaling policies
- **`BuildKitWorker`** - Ephemeral worker instances created on-demand for builds
- **`BuildKitOIDCConfig`** - OIDC provider configurations for authentication

**Responsibilities:**

- Watches and reconciles CRDs
- Generates and manages TLS certificates (CA, server, client)
- Creates and manages Kubernetes resources (Deployments, Services, Pods)
- Handles worker allocation and lifecycle management
- Implements auto-scaling logic based on worker activity
- Embeds an HTTP API server for worker allocation and certificate retrieval

**Location**: `cmd/controller/main.go`

### 2. Pool Gateway

A dedicated deployment that serves as the entry point for each pool:

- **TLS Termination**: Handles incoming TLS/mTLS connections from clients
- **Token Validation**: Extracts allocation tokens from client certificates
- **Connection Routing**: Routes connections to the appropriate ephemeral worker
- **Metrics**: Exposes Prometheus metrics for connection tracking

Each pool has its own gateway deployment and service. The gateway remains running (typically 1 replica) even when workers scale to zero, allowing new connection requests to trigger worker creation.

**Location**: `cmd/gateway/main.go`, `internal/gateway/gateway.go`

### 3. Ephemeral Workers

On-demand BuildKit daemon pods created as `BuildKitWorker` resources:

- **On-Demand Creation**: Workers are created when clients request allocations
- **Job Allocation**: Each worker is allocated to a specific build job
- **Automatic Termination**: Workers are terminated when idle, expired, or no longer needed
- **Isolation**: Each worker runs in its own pod with dedicated resources

Workers are managed by the `BuildKitWorkerReconciler`, which handles the full lifecycle from creation to termination.

**Location**: `internal/controller/worker_controller.go`

### 4. HTTP API Server

Embedded within the controller pod, provides REST API endpoints:

- `/api/v1/workers/allocate` - Allocate a worker and get certificates
- `/api/v1/workers/lookup` - Look up worker endpoint by allocation token
- `/api/v1/workers/release` - Release a worker allocation
- `/api/v1/certs/request` - Request certificates via OIDC or ServiceAccount token
- `/api/v1/certs/{name}` - Retrieve existing certificate by name
- `/api/v1/pools` - List available pools
- `/api/v1/health` - Health check

**Location**: `internal/api/server.go`

## Connection Flow

### Step 1: Worker Allocation

The client requests a worker allocation from the controller API:

```bash
POST /api/v1/workers/allocate
{
  "poolName": "my-pool",
  "namespace": "default",
  "ttl": "1h"
}
```

**What happens:**

1. Controller authenticates the request (OIDC token or ServiceAccount token)
2. Controller finds an idle worker or creates a new `BuildKitWorker` resource
3. Controller generates an allocation token (HMAC-signed, time-limited)
4. Controller issues a client certificate with the allocation token embedded in the CN field (`alloc:<token>`)
5. Controller returns certificates and gateway endpoint to the client

### Step 2: Client Connection

The client connects to the pool gateway using the provided certificates:

```
Client → Pool Gateway Service (<pool-name>.<namespace>.svc:1235)
       → Gateway Pod (TLS termination, token extraction)
       → Worker Lookup (via controller API)
       → Ephemeral Worker Pod (<pod-ip>:1234)
```

**What happens:**

1. Client initiates TLS connection to gateway service
2. Gateway performs TLS handshake and extracts allocation token from client certificate CN
3. Gateway calls controller API to look up worker endpoint for the token
4. Gateway establishes connection to the worker pod (plain TCP or mTLS)
5. Gateway proxies traffic bidirectionally between client and worker

### Step 3: Build Execution

The client executes builds using the allocated worker:

- BuildKit protocol traffic flows through the gateway
- Worker processes build requests and caches layers
- Gateway tracks connection metrics

### Step 4: Worker Release

When the build completes or allocation expires:

1. Client can explicitly release the worker via API
2. Worker becomes idle and may be reused for new allocations
3. Idle workers are terminated after a configurable timeout
4. Allocation token expires and becomes invalid

## Allocation Token System

Allocation tokens are HMAC-signed, time-limited identifiers that link client connections to specific workers.

### Token Format

- **Generation**: Random 32-byte value + HMAC signature
- **Storage**: Base64 URL-encoded string
- **Embedding**: Token embedded in client certificate CN as `alloc:<token>`
- **Validation**: Gateway extracts token from certificate and validates with controller

### Token Lifecycle

1. **Issuance**: Created when worker is allocated, includes worker endpoint and metadata
2. **Validation**: Gateway validates token on each connection
3. **Expiration**: Tokens expire based on TTL (default 1h, max 24h)
4. **Revocation**: Tokens can be explicitly revoked or expire naturally

**Location**: `internal/gateway/token.go`

## TLS Certificate Management

### Certificate Hierarchy

1. **CA Certificate**: Root certificate authority (10-year validity, stored in cluster-wide Secret)
2. **Server Certificate**: Used by pool gateway (default 1-year validity, auto-rotated)
3. **Client Certificates**: Used by build clients, with allocation tokens embedded in CN

### Certificate Storage

For each `BuildKitPool`, the operator creates:

1. **`<pool-name>-tls`** Secret: Gateway TLS secret containing:

   - `ca.crt`: CA certificate
   - `tls.crt`: Server certificate
   - `tls.key`: Server private key

2. **`<pool-name>-client-certs`** Secret (optional): Client certificates for gateway-to-worker mTLS:
   - `ca.crt`: Worker CA certificate
   - `client.crt`: Gateway client certificate
   - `client.key`: Gateway client private key

### Certificate Rotation

- Server certificates are automatically rotated before expiry (default: 30 days before)
- Client certificates are automatically renewed based on the `renewBefore` field
- The operator checks certificate expiry on each reconciliation loop

## Pool Lifecycle

### 1. Pool Creation

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata:
  name: my-pool
spec:
  scaling:
    mode: auto
    min: 0
    max: 10
  gateway:
    enabled: true
  tls:
    enabled: true
```

**What happens:**

1. Operator ensures CA certificate exists
2. Operator generates server TLS certificate and stores in Secret
3. Operator creates gateway Deployment and Service
4. Operator updates pool status with endpoint: `tcp://my-pool.default.svc:1235`
5. No workers are created initially (scale-to-zero)

### 2. Worker Allocation

**What happens:**

1. Client requests worker allocation via HTTP API
2. Controller finds idle worker or creates new `BuildKitWorker` resource
3. `BuildKitWorkerReconciler` creates worker pod
4. Worker pod becomes ready and reports endpoint
5. Controller issues allocation token and client certificate
6. Client receives certificates and gateway endpoint

### 3. Client Connection

**What happens:**

1. Client connects to gateway service with certificates
2. Gateway extracts allocation token from certificate
3. Gateway looks up worker endpoint via controller API
4. Gateway proxies connection to worker pod
5. BuildKit protocol traffic flows through gateway

### 4. Auto-Scaling

The operator's scaling logic:

- Tracks worker count and phase (idle, allocated, running)
- Creates new workers when allocation requests arrive (up to `max`)
- Terminates idle workers after timeout (down to `min`)
- Supports scale-to-zero when `min: 0`

## Worker Lifecycle

### Phases

1. **Pending**: Worker resource created, waiting for pod creation
2. **Provisioning**: Pod created, waiting for readiness
3. **Idle**: Worker ready but not allocated
4. **Allocated**: Worker assigned to a job
5. **Running**: Worker actively processing builds
6. **Terminating**: Worker being cleaned up
7. **Failed**: Worker encountered an error

### State Transitions

```
Pending → Provisioning → Idle → Allocated → Running
                                    ↓
                              Terminating → (deleted)
```

Workers transition to `Terminating` when:

- Allocation expires
- Worker becomes idle after timeout
- Pool scales down
- Worker resource is deleted

## Authentication Methods

### mTLS (Mutual TLS)

Default authentication method. Each client requires a certificate issued by the operator's CA.

**Flow:**

1. Client obtains certificate via API
2. Allocation token embedded in certificate CN
3. Client connects with mTLS to gateway
4. Gateway verifies client certificate and extracts token
5. Connection is routed to allocated worker

### OIDC (OpenID Connect)

For certificate requests via HTTP API:

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

**Flow:**

1. Client presents OIDC token to controller API
2. Controller verifies token with OIDC provider
3. Controller allocates worker and issues certificate with allocation token
4. Client uses certificate for mTLS connection to gateway

### ServiceAccount Token

Kubernetes ServiceAccount tokens can be used to authenticate with the controller API for worker allocation and certificate requests.

## Resource Management

### Pool Resources

Each ephemeral worker pod can be configured with different resource sizes:

- **sm**: 500m CPU / 512Mi memory (request), 2 CPU / 2Gi memory (limit)
- **md**: 1 CPU / 2Gi memory (request), 4 CPU / 4Gi memory (limit)
- **lg**: 2 CPU / 4Gi memory (request), 8 CPU / 8Gi memory (limit)
- **xl**: 4 CPU / 8Gi memory (request), 16 CPU / 16Gi memory (limit)

Or custom resources via `spec.resources.buildkit`.

### Gateway Resources

The pool gateway has minimal resource requirements:

- Default: 50m CPU / 64Mi memory (request), 200m CPU / 256Mi memory (limit)
- Can be customized per pool via `spec.gateway.resources`

## Networking

### Service Types

Pools can be exposed via different Kubernetes Service types:

- **ClusterIP** (default): Only accessible within cluster
- **LoadBalancer**: Exposed via cloud load balancer
- **NodePort**: Exposed on cluster nodes

### Gateway API Support

Pools can be exposed via Kubernetes Gateway API:

```yaml
spec:
  gateway:
    gatewayAPI:
      enabled: true
      gatewayClassName: envoy
      hostname: buildkit.example.com
```

### Endpoints

Pool endpoints are automatically generated:

- **Internal**: `tcp://<pool-name>.<namespace>.svc:<port>`
- **External**: `tcp://<hostname>:<port>` (if Gateway API or LoadBalancer configured)

Default port is 1235 (gateway port), configurable via `spec.gateway.port`.

## Observability

### Metrics

Pool gateway exposes Prometheus metrics on port 9090:

- `buildkit_gateway_active_connections{pool="pool-name"}` - Current active connections
- `buildkit_gateway_connections_total{pool="pool-name",status="success|error"}` - Total connections
- `buildkit_gateway_connection_duration_seconds{pool="pool-name"}` - Connection duration histogram

Controller tracks worker metrics:

- `buildkit_workers_total{pool="pool-name",phase="idle|allocated|running"}` - Worker count by phase
- `buildkit_worker_allocations_total{pool="pool-name"}` - Total allocations
- `buildkit_worker_allocation_duration_seconds{pool="pool-name"}` - Allocation duration

### Logging

- Controller logs: Structured logging via controller-runtime
- Gateway logs: Connection details and routing decisions
- Worker logs: Standard BuildKit daemon logging

## Security Considerations

1. **TLS Certificates**: Automatically generated, rotated on expiry
2. **Allocation Tokens**: Time-limited, HMAC-signed, embedded in certificates
3. **mTLS**: All client connections require mutual TLS authentication
4. **Worker Isolation**: Each worker runs in its own pod with dedicated resources
5. **Network Policies**: Workers are only accessible via gateway (no direct access)
6. **RBAC**: Controller requires extensive RBAC permissions to manage resources
7. **Secrets**: Certificates stored in Kubernetes Secrets with proper labels and owner references

## Performance Optimizations

1. **Scale to Zero**: Pools can scale to 0 workers when idle (saves resources)
2. **On-Demand Scaling**: Workers created only when needed
3. **Worker Reuse**: Idle workers can be reused for new allocations
4. **Connection Tracking**: Metrics-based scaling decisions
5. **Resource Sizing**: Different worker sizes for different workloads
6. **Cache Backends**: Support for registry, S3, and local cache backends

## Architecture (Pool Gateway + Ephemeral Workers)

- Dedicated gateway deployment per pool
- Ephemeral workers created on-demand
- True scale-to-zero (gateway stays, workers terminate)
- Better resource utilization and isolation
- Allocation token system for secure routing
