package auth

import (
	"context"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"net/http"
	"sync"
	"time"

	"github.com/go-jose/go-jose/v4"
	"github.com/go-jose/go-jose/v4/jwt"
)

// MockOIDCServer provides a local OIDC server for testing.
// This should NEVER be used in production.
type MockOIDCServer struct {
	privateKey *rsa.PrivateKey
	publicKey  *rsa.PublicKey
	keyID      string
	issuer     string
	server     *http.Server
	mu         sync.RWMutex
}

// NewMockOIDCServer creates a new mock OIDC server.
func NewMockOIDCServer(issuer string, port int) (*MockOIDCServer, error) {
	// Generate RSA key pair
	privateKey, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return nil, fmt.Errorf("failed to generate RSA key: %w", err)
	}

	m := &MockOIDCServer{
		privateKey: privateKey,
		publicKey:  &privateKey.PublicKey,
		keyID:      "mock-key-1",
		issuer:     issuer,
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/.well-known/openid-configuration", m.handleDiscovery)
	mux.HandleFunc("/.well-known/jwks.json", m.handleJWKS)
	mux.HandleFunc("/token", m.handleToken)

	m.server = &http.Server{
		Addr:    fmt.Sprintf(":%d", port),
		Handler: mux,
	}

	return m, nil
}

// Start starts the mock OIDC server.
func (m *MockOIDCServer) Start() error {
	return m.server.ListenAndServe()
}

// Stop stops the mock OIDC server.
func (m *MockOIDCServer) Stop(ctx context.Context) error {
	return m.server.Shutdown(ctx)
}

// handleDiscovery returns OIDC discovery document.
func (m *MockOIDCServer) handleDiscovery(w http.ResponseWriter, _ *http.Request) {
	discovery := map[string]interface{}{
		"issuer":                 m.issuer,
		"authorization_endpoint": m.issuer + "/authorize",
		"token_endpoint":         m.issuer + "/token",
		"jwks_uri":               m.issuer + "/.well-known/jwks.json",
		"response_types_supported": []string{
			"id_token",
			"token id_token",
		},
		"subject_types_supported": []string{
			"public",
		},
		"id_token_signing_alg_values_supported": []string{
			"RS256",
		},
		"claims_supported": []string{
			"sub",
			"aud",
			"iss",
			"exp",
			"iat",
			"actor",
			"repository",
			"ref",
		},
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(discovery)
}

// handleJWKS returns the JWKS.
func (m *MockOIDCServer) handleJWKS(w http.ResponseWriter, _ *http.Request) {
	jwk := jose.JSONWebKey{
		Key:       m.publicKey,
		KeyID:     m.keyID,
		Algorithm: string(jose.RS256),
		Use:       "sig",
	}

	jwks := jose.JSONWebKeySet{
		Keys: []jose.JSONWebKey{jwk},
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(jwks)
}

// MockTokenRequest represents a token generation request.
type MockTokenRequest struct {
	Subject  string            `json:"sub"`
	Audience string            `json:"aud"`
	Duration time.Duration     `json:"duration,omitempty"`
	Claims   map[string]string `json:"claims,omitempty"`
}

// handleToken generates a mock token (for testing only).
func (m *MockOIDCServer) handleToken(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "Method not allowed", http.StatusMethodNotAllowed)
		return
	}

	var req MockTokenRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, fmt.Sprintf("Invalid request: %v", err), http.StatusBadRequest)
		return
	}

	// Default duration
	if req.Duration == 0 {
		req.Duration = 1 * time.Hour
	}

	// Default subject
	if req.Subject == "" {
		req.Subject = "test-user"
	}

	// Default audience
	if req.Audience == "" {
		req.Audience = "buildkit-controller"
	}

	token, err := m.GenerateToken(req.Subject, req.Audience, req.Duration, req.Claims)
	if err != nil {
		http.Error(w, fmt.Sprintf("Failed to generate token: %v", err), http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]string{
		"id_token": token,
	})
}

// GenerateToken creates a signed JWT token for testing.
func (m *MockOIDCServer) GenerateToken(subject, audience string, duration time.Duration, extraClaims map[string]string) (string, error) {
	// Create signer
	signerOpts := jose.SignerOptions{}
	signerOpts.WithType("JWT")
	signerOpts.WithHeader("kid", m.keyID)

	signer, err := jose.NewSigner(jose.SigningKey{Algorithm: jose.RS256, Key: m.privateKey}, &signerOpts)
	if err != nil {
		return "", fmt.Errorf("failed to create signer: %w", err)
	}

	// Build claims
	now := time.Now()
	claims := jwt.Claims{
		Issuer:    m.issuer,
		Subject:   subject,
		Audience:  jwt.Audience{audience},
		IssuedAt:  jwt.NewNumericDate(now),
		Expiry:    jwt.NewNumericDate(now.Add(duration)),
		NotBefore: jwt.NewNumericDate(now),
	}

	// Add extra claims
	privateClaims := make(map[string]interface{})
	for k, v := range extraClaims {
		privateClaims[k] = v
	}

	// Sign token
	builder := jwt.Signed(signer).Claims(claims).Claims(privateClaims)
	token, err := builder.Serialize()
	if err != nil {
		return "", fmt.Errorf("failed to sign token: %w", err)
	}

	return token, nil
}

// GetPublicKeyPEM returns the public key in PEM format.
func (m *MockOIDCServer) GetPublicKeyPEM() (string, error) {
	pubASN1, err := x509.MarshalPKIXPublicKey(m.publicKey)
	if err != nil {
		return "", err
	}

	pubPEM := pem.EncodeToMemory(&pem.Block{
		Type:  "PUBLIC KEY",
		Bytes: pubASN1,
	})

	return string(pubPEM), nil
}

// Issuer returns the issuer URL.
func (m *MockOIDCServer) Issuer() string {
	return m.issuer
}
