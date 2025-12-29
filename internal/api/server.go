package api

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/google/uuid"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/auth"
	"github.com/smrt-devops/buildkit-controller/internal/certs"
	"github.com/smrt-devops/buildkit-controller/internal/gateway"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

// oidcVerifierEntry tracks an OIDC verifier and its last usage time.
type oidcVerifierEntry struct {
	verifier *auth.OIDCVerifier
	lastUsed time.Time
}

// Server provides HTTP API for certificate retrieval.
type Server struct {
	client          client.Client
	certManager     *certs.CertificateManager
	caManager       *certs.CAManager
	log             utils.Logger
	port            int
	oidcVerifiersMu sync.RWMutex
	oidcVerifiers   map[string]*oidcVerifierEntry // keyed by issuer
	certConfig      *certs.Config
	rbacChecker     *RBACChecker
	saTokenVerifier *auth.ServiceAccountTokenVerifier
	tokenManager    *gateway.TokenManager
	devMode         bool // If true, skip authentication (for local development only)
}

// ServerOption is a functional option for configuring the Server.
type ServerOption func(*Server)

// WithDevMode enables dev mode (skips authentication).
func WithDevMode(enabled bool) ServerOption {
	return func(s *Server) {
		s.devMode = enabled
	}
}

// NewServer creates a new API server.
func NewServer(k8sClient client.Client, certManager *certs.CertificateManager, caManager *certs.CAManager, log utils.Logger, port int, certConfig *certs.Config, opts ...ServerOption) *Server {
	if certConfig == nil {
		certConfig = certs.LoadConfig()
	}
	s := &Server{
		client:          k8sClient,
		certManager:     certManager,
		caManager:       caManager,
		log:             log,
		port:            port,
		oidcVerifiers:   make(map[string]*oidcVerifierEntry),
		certConfig:      certConfig,
		rbacChecker:     NewRBACChecker(),
		saTokenVerifier: auth.NewServiceAccountTokenVerifier(k8sClient, log),
		tokenManager: gateway.NewTokenManager(gateway.TokenManagerConfig{
			DefaultTTL: 1 * time.Hour,
			MaxTTL:     24 * time.Hour,
		}),
	}

	// Apply options
	for _, opt := range opts {
		opt(s)
	}

	if s.devMode {
		log.Info("WARNING: Dev mode enabled - authentication is disabled!")
	}

	return s
}

func (s *Server) requireMethod(w http.ResponseWriter, r *http.Request, method string) bool {
	if r.Method != method {
		http.Error(w, "Method not allowed", http.StatusMethodNotAllowed)
		return false
	}
	return true
}

func (s *Server) authenticate(w http.ResponseWriter, r *http.Request) (string, bool) {
	identity, err := s.authenticateRequest(r)
	if err != nil {
		s.log.Error(err, "Authentication failed")
		http.Error(w, "Unauthorized", http.StatusUnauthorized)
		return "", false
	}
	return identity, true
}

func (s *Server) decodeJSON(w http.ResponseWriter, r *http.Request, v interface{}) bool {
	if err := json.NewDecoder(r.Body).Decode(v); err != nil {
		http.Error(w, fmt.Sprintf("Invalid request: %v", err), http.StatusBadRequest)
		return false
	}
	return true
}

func (s *Server) encodeJSON(w http.ResponseWriter, v interface{}) bool {
	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(v); err != nil {
		s.log.Error(err, "Failed to encode response")
		http.Error(w, "Failed to encode response", http.StatusInternalServerError)
		return false
	}
	return true
}

func (s *Server) errorResponse(w http.ResponseWriter, status int, message string, err error) {
	if err != nil {
		s.log.Error(err, message)
		http.Error(w, fmt.Sprintf("%s: %v", message, err), status)
	} else {
		http.Error(w, message, status)
	}
}

func resolveNamespace(ns string) string {
	if ns == "" {
		return "default"
	}
	return ns
}

func buildPoolMap(poolList *buildkitv1alpha1.BuildKitPoolList) map[string]*buildkitv1alpha1.BuildKitPool {
	poolMap := make(map[string]*buildkitv1alpha1.BuildKitPool)
	for i := range poolList.Items {
		pool := &poolList.Items[i]
		poolMap[pool.Name] = pool
		poolMap[fmt.Sprintf("%s/%s", pool.Namespace, pool.Name)] = pool
	}
	return poolMap
}

// Start starts the HTTP server.
func (s *Server) Start(ctx context.Context) error {
	mux := http.NewServeMux()

	// Certificate request endpoint (OIDC/ServiceAccount token)
	mux.HandleFunc("/api/v1/certs/request", s.handleCertRequest)

	// List available pools
	mux.HandleFunc("/api/v1/pools", s.handleListPools)

	// Wake up a pool (ensure it's ready) - handles /api/v1/pools/{name}/wake
	mux.HandleFunc("/api/v1/pools/", s.handleWakePool)

	// Allocate a pool (with optional cert issuance)
	mux.HandleFunc("/api/v1/allocate", s.handleAllocate)

	// Worker allocation (new architecture - allocate a specific worker)
	mux.HandleFunc("/api/v1/workers/allocate", s.handleWorkerAllocate)

	// Worker lookup by token (for gateway)
	mux.HandleFunc("/api/v1/workers/lookup", s.handleWorkerLookup)

	// Release a worker
	mux.HandleFunc("/api/v1/workers/release", s.handleWorkerRelease)

	// Health check
	mux.HandleFunc("/api/v1/health", s.handleHealth)

	server := &http.Server{
		Addr:         fmt.Sprintf(":%d", s.port),
		Handler:      mux,
		ReadTimeout:  15 * time.Second,
		WriteTimeout: 15 * time.Second,
		IdleTimeout:  60 * time.Second,
	}

	s.log.Info("Starting API server", "port", s.port)

	// Start cleanup goroutine for stale OIDC verifiers
	go s.cleanupStaleVerifiers(ctx)

	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := server.Shutdown(shutdownCtx); err != nil {
			s.log.Error(err, "Failed to shutdown server gracefully")
		}
	}()

	if err := server.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		return fmt.Errorf("failed to start API server: %w", err)
	}

	return nil
}

// CertRequest represents a certificate request.
type CertRequest struct {
	Pools    []string `json:"pools"`
	Duration string   `json:"duration,omitempty"`
}

// CertResponse represents a certificate response.
type CertResponse struct {
	CACert     string            `json:"caCert"`
	ClientCert string            `json:"clientCert"`
	ClientKey  string            `json:"clientKey"`
	Endpoints  map[string]string `json:"endpoints"`
}

// PoolInfo represents pool information.
type PoolInfo struct {
	Name     string `json:"name"`
	Endpoint string `json:"endpoint"`
	Status   string `json:"status"`
}

// AllocationRequest represents a pool allocation request.
type AllocationRequest struct {
	// PoolSelector selects an existing pool by labels
	PoolSelector map[string]string `json:"poolSelector,omitempty"`
	// PoolName directly specifies a pool name (alternative to selector)
	PoolName string `json:"poolName,omitempty"`
	// UseExistingCerts if true, skips cert issuance and uses existing certs
	UseExistingCerts bool `json:"useExistingCerts,omitempty"`
	// CertSecretName specifies existing cert secret to use (if useExistingCerts is true)
	CertSecretName string `json:"certSecretName,omitempty"`
	// Duration for certificate (if issuing new certs)
	Duration string `json:"duration,omitempty"`
	// Namespace for the pool (defaults to "default")
	Namespace string `json:"namespace,omitempty"`
	// JobID is a unique identifier for this build job (optional)
	JobID string `json:"jobId,omitempty"`
	// Metadata for the allocation (optional)
	Metadata map[string]string `json:"metadata,omitempty"`
}

// AllocationResponse represents an allocation response.
type AllocationResponse struct {
	Endpoint string `json:"endpoint"`
	// Certificates (only if useExistingCerts is false)
	CACert     string `json:"caCert,omitempty"`
	ClientCert string `json:"clientCert,omitempty"`
	ClientKey  string `json:"clientKey,omitempty"`
	// Pool information
	PoolName string `json:"poolName"`
	Ready    bool   `json:"ready"`
	// Worker information (new architecture)
	WorkerName      string `json:"workerName,omitempty"`
	AllocationToken string `json:"allocationToken,omitempty"`
}

// WakeResponse represents a wake-up response.
type WakeResponse struct {
	Endpoint string `json:"endpoint"`
	PoolName string `json:"poolName"`
	Ready    bool   `json:"ready"`
}

func (s *Server) handleCertRequest(w http.ResponseWriter, r *http.Request) {
	if !s.requireMethod(w, r, http.MethodPost) {
		return
	}

	identity, ok := s.authenticate(w, r)
	if !ok {
		return
	}

	var req CertRequest
	if !s.decodeJSON(w, r, &req) {
		return
	}

	if len(req.Pools) == 0 {
		http.Error(w, "At least one pool must be specified", http.StatusBadRequest)
		return
	}

	// Validate pools exist and user has access
	// Batch fetch pools from all namespaces to reduce API calls
	poolList := &buildkitv1alpha1.BuildKitPoolList{}
	if err := s.client.List(r.Context(), poolList); err != nil {
		s.errorResponse(w, http.StatusInternalServerError, "Failed to list pools", err)
		return
	}

	poolMap := buildPoolMap(poolList)

	if err := s.rbacChecker.CheckPoolAccess(identity, req.Pools, poolMap); err != nil {
		s.log.Error(err, "RBAC check failed", "identity", identity, "pools", req.Pools)
		http.Error(w, err.Error(), http.StatusForbidden)
		return
	}

	duration := s.certConfig.DefaultClientCertDuration
	if req.Duration != "" {
		if parsed, err := time.ParseDuration(req.Duration); err == nil {
			duration = parsed
		}
	}

	certPEM, keyPEM, _, err := s.certManager.IssueCertificate(r.Context(), &certs.CertificateRequest{
		CommonName:   identity,
		Organization: "BuildKit Client",
		Duration:     duration,
		IsClient:     true,
	})
	if err != nil {
		s.errorResponse(w, http.StatusInternalServerError, "Failed to issue certificate", err)
		return
	}

	caCertPEM, err := s.caManager.GetCACertPEM(r.Context())
	if err != nil {
		s.errorResponse(w, http.StatusInternalServerError, "Failed to get CA certificate", err)
		return
	}

	endpoints := make(map[string]string)
	for _, poolName := range req.Pools {
		if pool, exists := poolMap[poolName]; exists && pool.Status.Endpoint != "" {
			endpoints[poolName] = pool.Status.Endpoint
		}
	}

	response := CertResponse{
		CACert:     string(caCertPEM),
		ClientCert: string(certPEM),
		ClientKey:  string(keyPEM),
		Endpoints:  endpoints,
	}

	s.encodeJSON(w, response)
}

func (s *Server) handleListPools(w http.ResponseWriter, r *http.Request) {
	if !s.requireMethod(w, r, http.MethodGet) {
		return
	}

	if _, ok := s.authenticate(w, r); !ok {
		return
	}

	poolList := &buildkitv1alpha1.BuildKitPoolList{}
	if err := s.client.List(r.Context(), poolList); err != nil {
		s.errorResponse(w, http.StatusInternalServerError, "Failed to list pools", err)
		return
	}

	pools := make([]PoolInfo, 0, len(poolList.Items))
	for i := range poolList.Items {
		pool := &poolList.Items[i]
		status := "Unknown"
		if pool.Status.Phase != "" {
			status = pool.Status.Phase
		}

		pools = append(pools, PoolInfo{
			Name:     pool.Name,
			Endpoint: pool.Status.Endpoint,
			Status:   status,
		})
	}

	s.encodeJSON(w, map[string]interface{}{"pools": pools})
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	w.WriteHeader(http.StatusOK)
	fmt.Fprintf(w, "OK")
}

// authenticateRequest authenticates the incoming request.
// Supports:
// 1. OIDC token (Bearer token)
// 2. Kubernetes ServiceAccount token.
// In dev mode, authentication is skipped.
func (s *Server) authenticateRequest(r *http.Request) (string, error) {
	// In dev mode, skip authentication
	if s.devMode {
		return "dev-user", nil
	}

	authHeader := r.Header.Get("Authorization")
	if authHeader == "" {
		return "", fmt.Errorf("no authorization header")
	}

	parts := strings.SplitN(authHeader, " ", 2)
	if len(parts) != 2 || parts[0] != "Bearer" {
		return "", fmt.Errorf("invalid authorization header format")
	}

	token := parts[1]

	// Try OIDC token first
	if identity, err := s.verifyOIDCToken(r.Context(), token); err == nil {
		return identity, nil
	}

	// Try ServiceAccount token
	if identity, err := s.verifyServiceAccountToken(r.Context(), token); err == nil {
		return identity, nil
	}

	return "", fmt.Errorf("token verification failed")
}

// verifyOIDCToken verifies an OIDC token.
func (s *Server) verifyOIDCToken(ctx context.Context, token string) (string, error) {
	// First, try to find OIDC configuration from BuildKitOIDCConfig CRDs
	oidcConfigList := &buildkitv1alpha1.BuildKitOIDCConfigList{}
	if err := s.client.List(ctx, oidcConfigList); err == nil {
		for i := range oidcConfigList.Items {
			oidcConfig := &oidcConfigList.Items[i]
			if !oidcConfig.Spec.Enabled {
				continue
			}

			verifier, err := s.getOrCreateVerifier(ctx, oidcConfig.Spec.Issuer, oidcConfig.Spec.Audience, oidcConfig.Spec.ClaimsMapping)
			if err != nil {
				s.log.V(1).Info("Failed to create OIDC verifier", "issuer", oidcConfig.Spec.Issuer, "error", err)
				continue
			}

			// Verify token
			claims, err := verifier.VerifyToken(ctx, token)
			if err != nil {
				s.log.V(1).Info("OIDC token verification failed", "issuer", oidcConfig.Spec.Issuer, "error", err)
				continue
			}

			// Return user identity
			return claims.User, nil
		}
	}

	// Fallback: Try to find OIDC configuration from pools (for backward compatibility)
	poolList := &buildkitv1alpha1.BuildKitPoolList{}
	if err := s.client.List(ctx, poolList); err == nil {
		for i := range poolList.Items {
			pool := &poolList.Items[i]
			for _, method := range pool.Spec.Auth.Methods {
				if method.Type != buildkitv1alpha1.AuthMethodOIDC || method.OIDC == nil {
					continue
				}
				oidcConfig := method.OIDC

				verifier, err := s.getOrCreateVerifier(ctx, oidcConfig.Issuer, oidcConfig.Audience, oidcConfig.ClaimsMapping)
				if err != nil {
					s.log.V(1).Info("Failed to create OIDC verifier", "issuer", oidcConfig.Issuer, "error", err)
					continue
				}

				// Verify token
				claims, err := verifier.VerifyToken(ctx, token)
				if err != nil {
					s.log.V(1).Info("OIDC token verification failed", "issuer", oidcConfig.Issuer, "error", err)
					continue
				}

				// Return user identity
				return claims.User, nil
			}
		}
	}

	return "", fmt.Errorf("no valid OIDC configuration found or token verification failed")
}

// getOrCreateVerifier gets or creates an OIDC verifier for the given issuer.
func (s *Server) getOrCreateVerifier(ctx context.Context, issuer, audience string, claimsMapping buildkitv1alpha1.ClaimsMapping) (*auth.OIDCVerifier, error) {
	// Get or create verifier for this issuer
	s.oidcVerifiersMu.RLock()
	entry, exists := s.oidcVerifiers[issuer]
	s.oidcVerifiersMu.RUnlock()

	if exists {
		// Update last used time
		s.oidcVerifiersMu.Lock()
		entry.lastUsed = time.Now()
		s.oidcVerifiersMu.Unlock()
		return entry.verifier, nil
	}

	// Create new verifier
	userClaim := claimsMapping.User
	if userClaim == "" {
		userClaim = "sub"
	}
	poolsClaim := claimsMapping.Pools

	verifier, err := auth.NewOIDCVerifier(
		ctx,
		issuer,
		audience,
		userClaim,
		poolsClaim,
	)
	if err != nil {
		return nil, fmt.Errorf("failed to create OIDC verifier: %w", err)
	}

	// Store verifier with timestamp
	s.oidcVerifiersMu.Lock()
	s.oidcVerifiers[issuer] = &oidcVerifierEntry{
		verifier: verifier,
		lastUsed: time.Now(),
	}
	s.oidcVerifiersMu.Unlock()

	return verifier, nil
}

// cleanupStaleVerifiers periodically removes OIDC verifiers that haven't been used recently.
func (s *Server) cleanupStaleVerifiers(ctx context.Context) {
	ticker := time.NewTicker(1 * time.Hour)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			s.oidcVerifiersMu.Lock()
			now := time.Now()
			for issuer, entry := range s.oidcVerifiers {
				// Remove verifiers not used in the last 24 hours
				if now.Sub(entry.lastUsed) > 24*time.Hour {
					delete(s.oidcVerifiers, issuer)
					s.log.V(1).Info("Cleaned up stale OIDC verifier", "issuer", issuer)
				}
			}
			s.oidcVerifiersMu.Unlock()
		}
	}
}

// verifyServiceAccountToken verifies a Kubernetes ServiceAccount token.
func (s *Server) verifyServiceAccountToken(ctx context.Context, token string) (string, error) {
	// Use the ServiceAccount token verifier
	identity, err := s.saTokenVerifier.VerifyToken(ctx, token)
	if err != nil {
		return "", fmt.Errorf("failed to verify ServiceAccount token: %w", err)
	}
	return identity, nil
}

// handleWakePool handles waking up a pool (ensuring it's ready).
func (s *Server) handleWakePool(w http.ResponseWriter, r *http.Request) {
	// Check if this is a wake request (ends with /wake) - must check BEFORE authentication
	if !strings.HasSuffix(r.URL.Path, "/wake") {
		// This is not a wake request, return 404 (might be /api/v1/pools/ which should go to list)
		http.Error(w, "Not found", http.StatusNotFound)
		return
	}

	if !s.requireMethod(w, r, http.MethodPost) {
		return
	}

	if _, ok := s.authenticate(w, r); !ok {
		return
	}

	path := strings.TrimPrefix(r.URL.Path, "/api/v1/pools/")
	poolName := strings.TrimSuffix(path, "/wake")
	poolName = strings.TrimSuffix(poolName, "/")
	if poolName == "" {
		http.Error(w, "Pool name required", http.StatusBadRequest)
		return
	}

	namespace := resolveNamespace(r.URL.Query().Get("namespace"))

	pool := &buildkitv1alpha1.BuildKitPool{}
	if err := s.client.Get(r.Context(), types.NamespacedName{Name: poolName, Namespace: namespace}, pool); err != nil {
		s.errorResponse(w, http.StatusNotFound, "Pool not found", err)
		return
	}

	// Wait for pool to be ready (with timeout)
	ctx, cancel := context.WithTimeout(r.Context(), 5*time.Minute)
	defer cancel()

	ready, endpoint, err := s.waitForPoolReady(ctx, poolName, namespace)
	if err != nil {
		s.errorResponse(w, http.StatusInternalServerError, "Failed to wait for pool readiness", err)
		return
	}

	s.encodeJSON(w, WakeResponse{
		Endpoint: endpoint,
		PoolName: poolName,
		Ready:    ready,
	})
}

// handleAllocate handles pool allocation requests.
func (s *Server) handleAllocate(w http.ResponseWriter, r *http.Request) {
	if !s.requireMethod(w, r, http.MethodPost) {
		return
	}

	identity, ok := s.authenticate(w, r)
	if !ok {
		return
	}

	var req AllocationRequest
	if !s.decodeJSON(w, r, &req) {
		return
	}

	namespace := resolveNamespace(req.Namespace)

	var pool *buildkitv1alpha1.BuildKitPool
	if req.PoolName != "" {
		pool = &buildkitv1alpha1.BuildKitPool{}
		if err := s.client.Get(r.Context(), types.NamespacedName{Name: req.PoolName, Namespace: namespace}, pool); err != nil {
			s.errorResponse(w, http.StatusNotFound, "Pool not found", err)
			return
		}
	} else if len(req.PoolSelector) > 0 {
		poolList := &buildkitv1alpha1.BuildKitPoolList{}
		selector := labels.SelectorFromSet(req.PoolSelector)
		if err := s.client.List(r.Context(), poolList, client.InNamespace(namespace), client.MatchingLabelsSelector{Selector: selector}); err != nil {
			s.errorResponse(w, http.StatusInternalServerError, "Failed to list pools", err)
			return
		}
		if len(poolList.Items) == 0 {
			http.Error(w, "No pools found matching selector", http.StatusNotFound)
			return
		}
		pool = &poolList.Items[0]
	} else {
		http.Error(w, "Either poolName or poolSelector must be specified", http.StatusBadRequest)
		return
	}

	// Wait for pool to be ready (with timeout)
	ctx, cancel := context.WithTimeout(r.Context(), 5*time.Minute)
	defer cancel()

	ready, endpoint, err := s.waitForPoolReady(ctx, pool.Name, namespace)
	if err != nil {
		s.errorResponse(w, http.StatusInternalServerError, "Failed to wait for pool readiness", err)
		return
	}

	response := AllocationResponse{
		Endpoint: endpoint,
		PoolName: pool.Name,
		Ready:    ready,
	}

	if !req.UseExistingCerts {
		duration := s.certConfig.DefaultClientCertDuration
		if req.Duration != "" {
			if parsed, err := time.ParseDuration(req.Duration); err == nil {
				duration = parsed
			}
		}

		certPEM, keyPEM, _, err := s.certManager.IssueCertificate(r.Context(), &certs.CertificateRequest{
			CommonName:   identity,
			Organization: "BuildKit Client",
			Duration:     duration,
			IsClient:     true,
		})
		if err != nil {
			s.errorResponse(w, http.StatusInternalServerError, "Failed to issue certificate", err)
			return
		}

		caCertPEM, err := s.caManager.GetCACertPEM(r.Context())
		if err != nil {
			s.errorResponse(w, http.StatusInternalServerError, "Failed to get CA certificate", err)
			return
		}

		// Base64 encode for safe JSON transport
		response.CACert = base64.StdEncoding.EncodeToString(caCertPEM)
		response.ClientCert = base64.StdEncoding.EncodeToString(certPEM)
		response.ClientKey = base64.StdEncoding.EncodeToString(keyPEM)
	} else if req.CertSecretName != "" {
		// Use existing cert secret
		secret := &corev1.Secret{}
		if err := s.client.Get(r.Context(), types.NamespacedName{Name: req.CertSecretName, Namespace: namespace}, secret); err != nil {
			s.log.Error(err, "Failed to get cert secret", "secret", req.CertSecretName)
			http.Error(w, fmt.Sprintf("Cert secret not found: %v", err), http.StatusNotFound)
			return
		}
		// Extract certs from secret (if present) and base64 encode for JSON transport
		if caCert, ok := secret.Data["ca.crt"]; ok {
			response.CACert = base64.StdEncoding.EncodeToString(caCert)
		}
		if clientCert, ok := secret.Data["client.crt"]; ok {
			response.ClientCert = base64.StdEncoding.EncodeToString(clientCert)
		}
		if clientKey, ok := secret.Data["client.key"]; ok {
			response.ClientKey = base64.StdEncoding.EncodeToString(clientKey)
		}
	}

	s.encodeJSON(w, response)
}

// waitForPoolReady waits for a pool to be ready, scaling it up if needed.
func (s *Server) waitForPoolReady(ctx context.Context, poolName, namespace string) (bool, string, error) {
	// Check if pool is scaled to zero and scale up if needed
	deployment := &appsv1.Deployment{}
	if err := s.client.Get(ctx, types.NamespacedName{Name: poolName, Namespace: namespace}, deployment); err == nil {
		if deployment.Spec.Replicas != nil && *deployment.Spec.Replicas == 0 {
			// Scale up to 1 replica
			replicas := int32(1)
			deployment.Spec.Replicas = &replicas
			if err := s.client.Update(ctx, deployment); err != nil {
				return false, "", fmt.Errorf("failed to scale up pool: %w", err)
			}
			s.log.Info("Scaled up pool from zero", "pool", poolName, "namespace", namespace)
		}
	}

	// Wait for pool to be ready
	ticker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return false, "", fmt.Errorf("timeout waiting for pool to be ready")
		case <-ticker.C:
			pool := &buildkitv1alpha1.BuildKitPool{}
			if err := s.client.Get(ctx, types.NamespacedName{Name: poolName, Namespace: namespace}, pool); err != nil {
				continue
			}

			// Check Ready condition
			for _, condition := range pool.Status.Conditions {
				if condition.Type == "Ready" {
					if condition.Status == metav1.ConditionTrue {
						endpoint := pool.Status.Endpoint
						if endpoint == "" {
							// Generate endpoint if not set
							port := int32(1235)
							if pool.Spec.Networking.Port != nil {
								port = *pool.Spec.Networking.Port
							}
							endpoint = fmt.Sprintf("tcp://%s.%s.svc:%d", poolName, namespace, port)
						}
						return true, endpoint, nil
					}
				}
			}

			// Also check if gateway or workers are ready
			gatewayReady := pool.Status.Gateway != nil && pool.Status.Gateway.Ready
			if gatewayReady || pool.Status.Workers.Ready > 0 {
				endpoint := pool.Status.Endpoint
				if endpoint == "" {
					port := int32(1235)
					if pool.Spec.Networking.Port != nil {
						port = *pool.Spec.Networking.Port
					}
					endpoint = fmt.Sprintf("tcp://%s.%s.svc:%d", poolName, namespace, port)
				}
				return true, endpoint, nil
			}
		}
	}
}

// WorkerAllocateRequest represents a worker allocation request.
type WorkerAllocateRequest struct {
	PoolName  string            `json:"poolName"`
	Namespace string            `json:"namespace,omitempty"`
	JobID     string            `json:"jobId,omitempty"`
	TTL       string            `json:"ttl,omitempty"`
	Metadata  map[string]string `json:"metadata,omitempty"`
}

// WorkerAllocateResponse represents a worker allocation response.
type WorkerAllocateResponse struct {
	WorkerName      string `json:"workerName"`
	Token           string `json:"token"`
	Endpoint        string `json:"endpoint"`
	GatewayEndpoint string `json:"gatewayEndpoint"`
	ExpiresAt       string `json:"expiresAt"`
	// Certificates for client auth
	CACert     string `json:"caCert,omitempty"`
	ClientCert string `json:"clientCert,omitempty"`
	ClientKey  string `json:"clientKey,omitempty"`
}

// WorkerLookupRequest represents a worker lookup request (used by gateway).
type WorkerLookupRequest struct {
	Token string `json:"token"`
}

// WorkerLookupResponse represents a worker lookup response.
type WorkerLookupResponse struct {
	WorkerEndpoint string `json:"workerEndpoint"`
	WorkerName     string `json:"workerName"`
	PoolName       string `json:"poolName"`
}

// handleWorkerAllocate allocates a worker from a pool.
func (s *Server) handleWorkerAllocate(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "Method not allowed", http.StatusMethodNotAllowed)
		return
	}

	// Authenticate request
	identity, err := s.authenticateRequest(r)
	if err != nil {
		s.log.Error(err, "Authentication failed")
		http.Error(w, "Unauthorized", http.StatusUnauthorized)
		return
	}

	// Parse request
	var req WorkerAllocateRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, fmt.Sprintf("Invalid request: %v", err), http.StatusBadRequest)
		return
	}

	if req.PoolName == "" {
		http.Error(w, "poolName is required", http.StatusBadRequest)
		return
	}

	namespace := req.Namespace
	if namespace == "" {
		namespace = "default"
	}

	// Get the pool
	pool := &buildkitv1alpha1.BuildKitPool{}
	if err := s.client.Get(r.Context(), types.NamespacedName{Name: req.PoolName, Namespace: namespace}, pool); err != nil {
		s.log.Error(err, "Failed to get pool", "pool", req.PoolName)
		http.Error(w, fmt.Sprintf("Pool not found: %v", err), http.StatusNotFound)
		return
	}

	worker, err := s.findOrCreateWorker(r.Context(), pool)
	if err != nil {
		s.errorResponse(w, http.StatusServiceUnavailable, "Failed to allocate worker", err)
		return
	}

	jobID := req.JobID
	if jobID == "" {
		jobID = uuid.New().String()
	}

	ttl := 1 * time.Hour
	if req.TTL != "" {
		if parsed, err := time.ParseDuration(req.TTL); err == nil {
			ttl = parsed
		}
	}

	tokenData, err := s.tokenManager.IssueToken(
		pool.Name,
		namespace,
		worker.Name,
		worker.Status.Endpoint,
		jobID,
		identity,
		ttl,
		req.Metadata,
	)
	if err != nil {
		s.errorResponse(w, http.StatusInternalServerError, "Failed to issue allocation token", err)
		return
	}

	// Update worker with allocation
	worker.Spec.Allocation = &buildkitv1alpha1.WorkerAllocation{
		JobID:       jobID,
		Token:       tokenData.Token,
		RequestedBy: identity,
		AllocatedAt: metav1.Now(),
		ExpiresAt:   &metav1.Time{Time: tokenData.ExpiresAt},
		Metadata:    req.Metadata,
	}
	if err := s.client.Update(r.Context(), worker); err != nil {
		s.log.Error(err, "Failed to update worker allocation")
		// Don't fail the request, token is still valid
	}

	// Get gateway endpoint
	gatewayEndpoint := pool.Status.Endpoint
	if gatewayEndpoint == "" {
		port := int32(1235)
		if pool.Spec.Networking.Port != nil {
			port = *pool.Spec.Networking.Port
		}
		gatewayEndpoint = fmt.Sprintf("tcp://%s.%s.svc:%d", pool.Name, namespace, port)
	}

	// Issue client certificate with token embedded in CN
	certPEM, keyPEM, _, issueErr := s.certManager.IssueCertificate(r.Context(), &certs.CertificateRequest{
		CommonName:   fmt.Sprintf("alloc:%s", tokenData.Token),
		Organization: "BuildKit Client",
		Duration:     ttl,
		IsClient:     true,
	})
	if issueErr != nil {
		s.log.Error(issueErr, "Failed to issue certificate")
		http.Error(w, "Failed to issue certificate", http.StatusInternalServerError)
		return
	}

	caCertPEM, err := s.caManager.GetCACertPEM(r.Context())
	if err != nil {
		s.errorResponse(w, http.StatusInternalServerError, "Failed to get CA certificate", err)
		return
	}

	response := WorkerAllocateResponse{
		WorkerName:      worker.Name,
		Token:           tokenData.Token,
		Endpoint:        worker.Status.Endpoint,
		GatewayEndpoint: gatewayEndpoint,
		ExpiresAt:       tokenData.ExpiresAt.Format(time.RFC3339),
		CACert:          base64.StdEncoding.EncodeToString(caCertPEM),
		ClientCert:      base64.StdEncoding.EncodeToString(certPEM),
		ClientKey:       base64.StdEncoding.EncodeToString(keyPEM),
	}

	s.log.Info("Worker allocated", "worker", worker.Name, "pool", pool.Name, "identity", identity)
	s.encodeJSON(w, response)
}

// findOrCreateWorker finds an idle worker or creates a new one.
func (s *Server) findOrCreateWorker(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) (*buildkitv1alpha1.BuildKitWorker, error) {
	// List workers for this pool
	workerList := &buildkitv1alpha1.BuildKitWorkerList{}
	if err := s.client.List(ctx, workerList,
		client.InNamespace(pool.Namespace),
		client.MatchingLabels{"buildkit.smrt-devops.net/pool": pool.Name}); err != nil {
		return nil, fmt.Errorf("failed to list workers: %w", err)
	}

	// Find an idle worker
	for i := range workerList.Items {
		worker := &workerList.Items[i]
		if worker.Status.Phase == buildkitv1alpha1.WorkerPhaseIdle {
			return worker, nil
		}
	}

	// Check if we can create a new worker (respect pool max)
	maxWorkers := int32(10)
	if pool.Spec.Scaling.Max != nil {
		maxWorkers = *pool.Spec.Scaling.Max
	}
	if int32(len(workerList.Items)) >= maxWorkers {
		// Wait for an idle worker
		return s.waitForIdleWorker(ctx, pool)
	}

	// Create a new worker
	worker := &buildkitv1alpha1.BuildKitWorker{
		ObjectMeta: metav1.ObjectMeta{
			GenerateName: fmt.Sprintf("%s-worker-", pool.Name),
			Namespace:    pool.Namespace,
			Labels: map[string]string{
				"buildkit.smrt-devops.net/pool":   pool.Name,
				"buildkit.smrt-devops.net/worker": "true",
			},
		},
		Spec: buildkitv1alpha1.BuildKitWorkerSpec{
			PoolRef: buildkitv1alpha1.PoolReference{
				Name:      pool.Name,
				Namespace: pool.Namespace,
			},
		},
	}

	if err := s.client.Create(ctx, worker); err != nil {
		return nil, fmt.Errorf("failed to create worker: %w", err)
	}

	// Wait for worker to be ready
	return s.waitForWorkerReady(ctx, worker.Name, pool.Namespace)
}

// waitForIdleWorker waits for an idle worker to become available.
func (s *Server) waitForIdleWorker(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) (*buildkitv1alpha1.BuildKitWorker, error) {
	ticker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()

	timeout := time.After(2 * time.Minute)

	for {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-timeout:
			return nil, fmt.Errorf("timeout waiting for idle worker")
		case <-ticker.C:
			workerList := &buildkitv1alpha1.BuildKitWorkerList{}
			if err := s.client.List(ctx, workerList,
				client.InNamespace(pool.Namespace),
				client.MatchingLabels{"buildkit.smrt-devops.net/pool": pool.Name}); err != nil {
				continue
			}

			for i := range workerList.Items {
				worker := &workerList.Items[i]
				if worker.Status.Phase == buildkitv1alpha1.WorkerPhaseIdle {
					return worker, nil
				}
			}
		}
	}
}

// waitForWorkerReady waits for a worker to become ready.
func (s *Server) waitForWorkerReady(ctx context.Context, workerName, namespace string) (*buildkitv1alpha1.BuildKitWorker, error) {
	ticker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()

	timeout := time.After(5 * time.Minute)

	for {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-timeout:
			return nil, fmt.Errorf("timeout waiting for worker to be ready")
		case <-ticker.C:
			worker := &buildkitv1alpha1.BuildKitWorker{}
			if err := s.client.Get(ctx, types.NamespacedName{Name: workerName, Namespace: namespace}, worker); err != nil {
				continue
			}

			if worker.Status.Phase == buildkitv1alpha1.WorkerPhaseIdle || worker.Status.Phase == buildkitv1alpha1.WorkerPhaseRunning {
				return worker, nil
			}

			if worker.Status.Phase == buildkitv1alpha1.WorkerPhaseFailed {
				return nil, fmt.Errorf("worker failed: %s", worker.Status.Message)
			}
		}
	}
}

// handleWorkerLookup looks up a worker by allocation token (for gateway use).
func (s *Server) handleWorkerLookup(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "Method not allowed", http.StatusMethodNotAllowed)
		return
	}

	// This endpoint is called by the gateway internally, so we use a different auth
	// For now, require a special gateway token or internal call
	// In production, this should be protected by network policy

	var req WorkerLookupRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, fmt.Sprintf("Invalid request: %v", err), http.StatusBadRequest)
		return
	}

	if req.Token == "" {
		http.Error(w, "token is required", http.StatusBadRequest)
		return
	}

	// Look up the token
	tokenData, err := s.tokenManager.ValidateToken(req.Token)
	if err != nil {
		s.log.V(1).Info("Token lookup failed", "error", err)
		http.Error(w, "Token not found or expired", http.StatusNotFound)
		return
	}

	response := WorkerLookupResponse{
		WorkerEndpoint: tokenData.WorkerEndpoint,
		WorkerName:     tokenData.WorkerName,
		PoolName:       tokenData.PoolName,
	}

	s.encodeJSON(w, response)
}

func (s *Server) handleWorkerRelease(w http.ResponseWriter, r *http.Request) {
	if !s.requireMethod(w, r, http.MethodPost) {
		return
	}

	if _, ok := s.authenticate(w, r); !ok {
		return
	}

	var req struct {
		Token string `json:"token"`
	}
	if !s.decodeJSON(w, r, &req) {
		return
	}

	if req.Token == "" {
		http.Error(w, "token is required", http.StatusBadRequest)
		return
	}

	// Get token data before revoking
	tokenData, err := s.tokenManager.ValidateToken(req.Token)
	if err != nil {
		http.Error(w, "Token not found or expired", http.StatusNotFound)
		return
	}

	// Delete the worker (ephemeral workers are cleaned up after use)
	worker := &buildkitv1alpha1.BuildKitWorker{}
	workerNamespace := tokenData.Namespace
	if workerNamespace == "" {
		workerNamespace = "buildkit-system" // Default namespace fallback
	}
	if err := s.client.Get(r.Context(), types.NamespacedName{Name: tokenData.WorkerName, Namespace: workerNamespace}, worker); err == nil {
		if deleteErr := s.client.Delete(r.Context(), worker); deleteErr != nil {
			s.log.Error(deleteErr, "Failed to delete worker", "worker", worker.Name, "namespace", workerNamespace)
		} else {
			s.log.Info("Worker deleted", "worker", worker.Name, "namespace", workerNamespace)
		}
	} else {
		s.log.V(1).Info("Worker not found, may have been deleted already", "worker", tokenData.WorkerName, "namespace", workerNamespace)
	}

	// Revoke the token
	s.tokenManager.RevokeToken(req.Token)

	s.log.Info("Worker released", "worker", tokenData.WorkerName, "pool", tokenData.PoolName)
	s.encodeJSON(w, map[string]string{"status": "released"})
}
