# Code Review: BuildKit Controller

## Executive Summary

This is a comprehensive code review of the BuildKit Kubernetes Controller project. The codebase is generally well-structured but has several areas that need attention for production readiness, particularly around memory management, resource cleanup, and concurrency safety.

## Status: Fixes Applied

The following critical issues have been **FIXED** in the codebase:

✅ **Fixed**: Memory leak in `oidcVerifiers` map - Added cleanup goroutine with 24-hour TTL  
✅ **Fixed**: Memory leak in `lastActivity` map - Added mutex protection and cleanup method  
✅ **Fixed**: Goroutine leak in metrics server - Added context cancellation support  
✅ **Fixed**: API server context issue - Fixed context lifecycle management  
✅ **Fixed**: Missing mutex protection in scale manager - Added `lastActivityMu` mutex  
✅ **Fixed**: HTTP client connection pooling - Added proper transport configuration  
✅ **Fixed**: Connection limit in proxy - Added bounds checking (10,000 max connections)  
✅ **Fixed**: Error handling - Fixed swallowed errors in owner reference setting  
✅ **Fixed**: API call optimization - Batch pool fetching to reduce K8s API calls  
✅ **Fixed**: Magic numbers - Extracted to constants in `internal/controller/constants.go`  
✅ **Fixed**: SRP violation - Refactored reconciler into specialized components:

- `TLSReconciler` - Handles TLS certificate management
- `ConfigMapReconciler` - Handles ConfigMap reconciliation
- `WorkloadReconciler` - Handles Deployment/StatefulSet/Job reconciliation
- `ServiceReconciler` - Handles Service reconciliation
- `StatusUpdater` - Handles status updates
  ✅ **Fixed**: Periodic cleanup - Added cleanup for stale pools in scale manager

The following issues still need attention (see details below):

⚠️ **TODO**: Implement proper JWT verification for ServiceAccount tokens (security)  
⚠️ **TODO**: Implement RBAC checks in API server (security)  
⚠️ **TODO**: Complete StatefulSet/Job reconciliation (features)  
⚠️ **TODO**: Add comprehensive unit tests (testing)

## Critical Issues

### 1. Memory Leaks

#### Issue: Unbounded Map Growth in `internal/api/server.go`

**Location**: `Server.oidcVerifiers` map (line 30, 45, 453)

**Problem**: The `oidcVerifiers` map grows unbounded as new OIDC issuers are encountered. There's no cleanup mechanism for stale verifiers.

**Impact**: Memory leak over time, especially in multi-tenant environments.

**Fix**:

```go
// Add cleanup mechanism
type Server struct {
    // ... existing fields ...
    oidcVerifiers     map[string]*oidcVerifierEntry
    verifierCleanupMu sync.RWMutex
}

type oidcVerifierEntry struct {
    verifier *auth.OIDCVerifier
    lastUsed time.Time
}

// Add cleanup goroutine or periodic cleanup
func (s *Server) cleanupStaleVerifiers() {
    s.verifierCleanupMu.Lock()
    defer s.verifierCleanupMu.Unlock()

    now := time.Now()
    for issuer, entry := range s.oidcVerifiers {
        if now.Sub(entry.lastUsed) > 24*time.Hour {
            delete(s.oidcVerifiers, issuer)
        }
    }
}
```

#### Issue: Unbounded Map Growth in `internal/scale/manager.go`

**Location**: `Manager.lastActivity` map (line 23, 32, 74, 82)

**Problem**: The `lastActivity` map stores activity timestamps for pools but never cleans up entries for deleted pools.

**Impact**: Memory leak when pools are deleted.

**Fix**:

```go
// Add cleanup on pool deletion or periodic cleanup
func (m *Manager) cleanupStalePools(ctx context.Context) error {
    // Get list of existing pools
    poolList := &buildkitv1alpha1.BuildKitPoolList{}
    if err := m.client.List(ctx, poolList); err != nil {
        return err
    }

    existingPools := make(map[string]bool)
    for i := range poolList.Items {
        existingPools[poolList.Items[i].Name] = true
    }

    // Clean up entries for non-existent pools
    for poolName := range m.lastActivity {
        if !existingPools[poolName] {
            delete(m.lastActivity, poolName)
        }
    }
    return nil
}
```

#### Issue: Connection Map in `internal/auth/proxy.go`

**Location**: `Proxy.connections` map (line 54, 73, 81)

**Problem**: While connections are cleaned up in defer, if a connection fails very early (before defer executes), it might not be removed. Also, no bounds checking.

**Impact**: Potential memory leak under error conditions.

**Fix**: Already handled with defer, but add bounds checking and ensure cleanup:

```go
func (p *Proxy) HandleConnection(ctx context.Context, downstream net.Conn) {
    startTime := time.Now()

    // Ensure cleanup even on early return
    cleaned := false
    defer func() {
        if !cleaned {
            p.mu.Lock()
            delete(p.connections, downstream)
            p.mu.Unlock()
            activeConnections.WithLabelValues(p.poolName).Dec()
        }
        downstream.Close()
    }()

    p.mu.Lock()
    // Add bounds check
    if len(p.connections) > 10000 {
        p.mu.Unlock()
        p.logger.Printf("Too many connections, rejecting")
        return
    }
    p.connections[downstream] = true
    p.mu.Unlock()
    cleaned = true

    // ... rest of function
}
```

### 2. Goroutine Leaks

#### Issue: Metrics Server Goroutine in `cmd/auth-proxy/main.go`

**Location**: Lines 49-53

**Problem**: The metrics server goroutine doesn't respect context cancellation and may not shut down gracefully.

**Impact**: Goroutine leak on shutdown.

**Fix**:

```go
// Start metrics server
metricsCtx, metricsCancel := context.WithCancel(ctx)
defer metricsCancel()

go func() {
    defer metricsCancel()
    if startErr := auth.StartMetricsServer(*metricsAddr, logger); startErr != nil {
        logger.Printf("Failed to start metrics server: %v", startErr)
    }
}()

// In auth/metrics.go, update StartMetricsServer to accept context
func StartMetricsServer(ctx context.Context, addr string, logger *log.Logger) error {
    // ... existing code ...

    go func() {
        <-ctx.Done()
        shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
        defer cancel()
        server.Shutdown(shutdownCtx)
    }()

    return server.ListenAndServe()
}
```

#### Issue: API Server Context in `cmd/controller/main.go`

**Location**: Lines 87-97

**Problem**: The context is created with `context.WithCancel(context.Background())` but the cancel is deferred immediately, which means the context is cancelled as soon as the goroutine starts, not when the main function exits.

**Impact**: API server may shut down prematurely.

**Fix**:

```go
// Create context tied to manager's context
apiCtx, apiCancel := context.WithCancel(ctrl.SetupSignalHandler())
defer apiCancel()

go func() {
    defer apiCancel()
    // Wait for manager to be elected (if leader election enabled)
    if enableLeaderElection {
        <-mgr.Elected()
    }
    if startErr := apiServer.Start(apiCtx); startErr != nil {
        setupLog.Error(startErr, "unable to start API server")
    }
}()
```

### 3. Resource Cleanup

#### Issue: HTTP Client in `internal/metrics/client.go`

**Location**: Line 24-26

**Problem**: HTTP client is created but never closed. While `http.Client` doesn't need explicit closing, connection pooling should be configured.

**Impact**: Potential connection exhaustion.

**Fix**:

```go
func NewClient(log logr.Logger) *Client {
    transport := &http.Transport{
        MaxIdleConns:        100,
        MaxIdleConnsPerHost: 10,
        IdleConnTimeout:     90 * time.Second,
    }

    return &Client{
        httpClient: &http.Client{
            Transport: transport,
            Timeout:   5 * time.Second,
        },
        log: log,
    }
}
```

### 4. Concurrency Issues

#### Issue: Race Condition in `internal/auth/proxy.go`

**Location**: Lines 72-74, 80-82

**Problem**: The connection map is accessed with locks, but the metric increment/decrement happens outside the lock, which could cause race conditions.

**Impact**: Potential race condition in metrics tracking.

**Fix**: Already mostly safe, but ensure metric operations are atomic (they are with Prometheus).

#### Issue: Missing Mutex Protection in `internal/scale/manager.go`

**Location**: `lastActivity` map access (lines 74, 82)

**Problem**: The `lastActivity` map is accessed without mutex protection in a concurrent environment.

**Impact**: Race condition, potential data corruption.

**Fix**:

```go
type Manager struct {
    // ... existing fields ...
    lastActivityMu sync.RWMutex
    lastActivity   map[string]time.Time
}

func (m *Manager) ShouldScaleDown(...) (bool, error) {
    // ... existing code ...

    m.lastActivityMu.Lock()
    m.lastActivity[pool.Name] = time.Now()
    m.lastActivityMu.Unlock()

    // ... later ...
    m.lastActivityMu.RLock()
    lastActivity, exists := m.lastActivity[pool.Name]
    m.lastActivityMu.RUnlock()
}
```

### 5. Performance Issues

#### Issue: Inefficient Certificate Parsing

**Location**: `internal/controller/pool_controller.go`, lines 208-230

**Problem**: Certificate is parsed from secret on every reconciliation, even when not needed.

**Impact**: Unnecessary CPU usage and API calls.

**Fix**: Cache parsed certificates or only parse when rotation is needed:

```go
// Only parse if we're checking rotation
if pool.Status.TLSSecretName != "" && needsRotation {
    existingInfo, err := r.CertManager.ParseCertificateFromSecret(...)
    // ...
}
```

#### Issue: Multiple K8s API Calls in `internal/api/server.go`

**Location**: Lines 140-189

**Problem**: Multiple sequential `Get` calls for pools that could be batched.

**Impact**: Increased latency.

**Fix**: Use a single list operation and filter:

```go
poolList := &buildkitv1alpha1.BuildKitPoolList{}
if err := s.client.List(ctx, poolList, client.InNamespace("default")); err != nil {
    http.Error(w, "Failed to list pools", http.StatusInternalServerError)
    return
}

poolMap := make(map[string]*buildkitv1alpha1.BuildKitPool)
for i := range poolList.Items {
    poolMap[poolList.Items[i].Name] = &poolList.Items[i]
}

// Validate pools exist
for _, poolName := range req.Pools {
    pool, exists := poolMap[poolName]
    if !exists {
        http.Error(w, fmt.Sprintf("Pool %s not found", poolName), http.StatusNotFound)
        return
    }
    // ... rest of validation
}
```

#### Issue: No Connection Pooling for Metrics Queries

**Location**: `internal/metrics/client.go`

**Problem**: Each metrics query creates a new connection.

**Impact**: High latency and connection overhead.

**Fix**: Already addressed above with HTTP client configuration.

### 6. SOLID Principle Violations

#### Issue: Single Responsibility Principle

**Location**: `internal/controller/pool_controller.go`

**Problem**: `BuildKitPoolReconciler` handles too many responsibilities:

- TLS certificate management
- ConfigMap management
- Deployment/StatefulSet/Job management
- Service management
- Status updates
- Auto-scaling

**Impact**: Hard to test, maintain, and extend.

**Recommendation**: Split into separate reconcilers or use composition:

```go
type BuildKitPoolReconciler struct {
    client.Client
    Scheme      *runtime.Scheme
    Log         logr.Logger
    TLSReconciler *TLSReconciler
    WorkloadReconciler *WorkloadReconciler
    ServiceReconciler *ServiceReconciler
    StatusUpdater *StatusUpdater
    ScaleManager *scale.Manager
}
```

#### Issue: Dependency Inversion

**Location**: Multiple files

**Problem**: Direct dependencies on concrete types rather than interfaces.

**Impact**: Hard to test and mock.

**Recommendation**: Define interfaces for key dependencies:

```go
type CertificateManager interface {
    IssueCertificate(ctx context.Context, req *CertificateRequest) (certPEM, keyPEM []byte, info *CertificateInfo, err error)
    ShouldRotateCertificate(certInfo *CertificateInfo, rotateBefore time.Duration) bool
    // ... other methods
}
```

### 7. Security Issues

#### Issue: Insecure ServiceAccount Token Verification

**Location**: `internal/api/server.go`, lines 457-491

**Problem**: The `verifyServiceAccountToken` function doesn't actually verify JWT tokens - it just accepts any token and returns a generic identity.

**Impact**: Security vulnerability - any token is accepted.

**Fix**: Implement proper JWT verification:

```go
func (s *Server) verifyServiceAccountToken(ctx context.Context, token string) (string, error) {
    // Parse and verify JWT token
    claims := &jwt.MapClaims{}
    jwtToken, err := jwt.ParseWithClaims(token, claims, func(token *jwt.Token) (interface{}, error) {
        // Verify signing method and get public key from Kubernetes API
        // This is a simplified example - full implementation needed
        return s.getServiceAccountPublicKey(ctx, claims)
    })

    if err != nil || !jwtToken.Valid {
        return "", fmt.Errorf("invalid token: %w", err)
    }

    // Extract ServiceAccount from claims
    sub, ok := (*claims)["sub"].(string)
    if !ok {
        return "", fmt.Errorf("invalid token subject")
    }

    return sub, nil
}
```

#### Issue: Missing RBAC Checks

**Location**: `internal/api/server.go`, lines 148, 236

**Problem**: TODO comments indicate RBAC checks are not implemented.

**Impact**: Users can access pools/certificates they shouldn't have access to.

**Fix**: Implement RBAC checks before returning certificates.

#### Issue: Hardcoded Namespace

**Location**: Multiple files use "default" namespace

**Problem**: Hardcoded "default" namespace limits multi-tenancy.

**Impact**: Cannot work across namespaces properly.

**Fix**: Make namespace configurable or extract from request context.

### 8. Error Handling Issues

#### Issue: Swallowed Errors

**Location**: `internal/controller/pool_controller.go`, lines 276-282

**Problem**: Errors when setting owner references are logged but not returned, causing silent failures.

**Impact**: Resources may not be properly owned, leading to orphaned resources.

**Fix**: Return errors or at least log them at error level:

```go
if refErr := controllerutil.SetControllerReference(pool, secret, r.Scheme); refErr != nil {
    log.Error(refErr, "Failed to set owner reference on TLS secret")
    return fmt.Errorf("failed to set owner reference: %w", refErr)
}
```

#### Issue: Error Context Loss

**Location**: Multiple locations

**Problem**: Some errors don't include sufficient context.

**Impact**: Hard to debug issues in production.

**Fix**: Use `fmt.Errorf` with `%w` verb to wrap errors with context.

### 9. Code Quality Issues

#### Issue: Magic Numbers

**Location**: Multiple files

**Problem**: Hardcoded values like `10000`, `30 * time.Second`, `720 * time.Hour`, etc.

**Impact**: Hard to maintain and configure.

**Fix**: Extract to constants or configuration:

```go
const (
    DefaultRequeueInterval = 30 * time.Second
    DefaultCertDuration = 8760 * time.Hour // 1 year
    DefaultRenewalTime = 720 * time.Hour   // 30 days
    MaxConnections = 10000
)
```

#### Issue: Incomplete Implementations

**Location**: `internal/controller/pool_controller.go`, lines 427, 432

**Problem**: StatefulSet and Job reconciliation return errors instead of being implemented.

**Impact**: Feature incomplete.

**Fix**: Implement these methods or remove the workload types from the API.

#### Issue: TODO Comments

**Location**: Multiple files

**Problem**: Several TODO comments indicate incomplete features.

**Impact**: Technical debt.

**Fix**: Either implement the features or document why they're deferred.

### 10. Testing Concerns

#### Issue: No Unit Tests Visible

**Problem**: No test files found in the codebase.

**Impact**: No confidence in code correctness, hard to refactor.

**Recommendation**: Add comprehensive unit tests, especially for:

- Certificate generation and rotation logic
- Connection handling in proxy
- Scaling decisions
- Error handling paths

## Recommendations Summary

### High Priority

1. **Fix memory leaks** in `oidcVerifiers` and `lastActivity` maps
2. **Fix goroutine leaks** in metrics server and API server
3. **Implement proper JWT verification** for ServiceAccount tokens
4. **Add mutex protection** for `lastActivity` map
5. **Implement RBAC checks** in API server

### Medium Priority

1. **Refactor reconciler** to follow Single Responsibility Principle
2. **Add connection pooling** for HTTP clients
3. **Optimize K8s API calls** (batch operations)
4. **Extract magic numbers** to constants
5. **Improve error handling** (don't swallow errors)

### Low Priority

1. **Complete StatefulSet/Job reconciliation**
2. **Add comprehensive unit tests**
3. **Document design decisions**
4. **Add performance benchmarks**

## Conclusion

The codebase is well-structured and follows Kubernetes controller patterns correctly. However, there are several production-readiness issues that need to be addressed, particularly around memory management, resource cleanup, and security. The most critical issues are the memory leaks and the insecure token verification. Once these are fixed, the codebase will be in good shape for public release.
