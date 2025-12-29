package gateway

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"sync"
	"time"
)

// TokenManager manages allocation tokens.
type TokenManager struct {
	secret     []byte
	tokens     map[string]*TokenData
	mu         sync.RWMutex
	defaultTTL time.Duration
	maxTTL     time.Duration
}

// TokenData contains token metadata.
type TokenData struct {
	Token          string            `json:"token"`
	PoolName       string            `json:"poolName"`
	Namespace      string            `json:"namespace"`
	WorkerName     string            `json:"workerName"`
	WorkerEndpoint string            `json:"workerEndpoint"`
	JobID          string            `json:"jobId,omitempty"`
	RequestedBy    string            `json:"requestedBy,omitempty"`
	IssuedAt       time.Time         `json:"issuedAt"`
	ExpiresAt      time.Time         `json:"expiresAt"`
	Metadata       map[string]string `json:"metadata,omitempty"`
}

// TokenManagerConfig configures the token manager.
type TokenManagerConfig struct {
	Secret     []byte
	DefaultTTL time.Duration
	MaxTTL     time.Duration
}

// NewTokenManager creates a new token manager.
func NewTokenManager(cfg TokenManagerConfig) *TokenManager {
	if len(cfg.Secret) == 0 {
		// Generate random secret if not provided
		cfg.Secret = make([]byte, 32)
		rand.Read(cfg.Secret)
	}
	if cfg.DefaultTTL == 0 {
		cfg.DefaultTTL = 1 * time.Hour
	}
	if cfg.MaxTTL == 0 {
		cfg.MaxTTL = 24 * time.Hour
	}

	return &TokenManager{
		secret:     cfg.Secret,
		tokens:     make(map[string]*TokenData),
		defaultTTL: cfg.DefaultTTL,
		maxTTL:     cfg.MaxTTL,
	}
}

// IssueToken creates a new allocation token.
func (tm *TokenManager) IssueToken(poolName, namespace, workerName, workerEndpoint, jobID, requestedBy string, ttl time.Duration, metadata map[string]string) (*TokenData, error) {
	if ttl == 0 {
		ttl = tm.defaultTTL
	}
	if ttl > tm.maxTTL {
		ttl = tm.maxTTL
	}

	// Generate random token
	tokenBytes := make([]byte, 32)
	if _, err := rand.Read(tokenBytes); err != nil {
		return nil, fmt.Errorf("failed to generate token: %w", err)
	}

	// Add HMAC signature
	token := tm.signToken(tokenBytes)

	now := time.Now()
	data := &TokenData{
		Token:          token,
		PoolName:       poolName,
		Namespace:      namespace,
		WorkerName:     workerName,
		WorkerEndpoint: workerEndpoint,
		JobID:          jobID,
		RequestedBy:    requestedBy,
		IssuedAt:       now,
		ExpiresAt:      now.Add(ttl),
		Metadata:       metadata,
	}

	tm.mu.Lock()
	tm.tokens[token] = data
	tm.mu.Unlock()

	return data, nil
}

// ValidateToken validates a token and returns its data.
func (tm *TokenManager) ValidateToken(token string) (*TokenData, error) {
	tm.mu.RLock()
	data, exists := tm.tokens[token]
	tm.mu.RUnlock()

	if !exists {
		return nil, fmt.Errorf("token not found")
	}

	if time.Now().After(data.ExpiresAt) {
		// Clean up expired token
		tm.RevokeToken(token)
		return nil, fmt.Errorf("token expired")
	}

	return data, nil
}

// RevokeToken revokes a token.
func (tm *TokenManager) RevokeToken(token string) {
	tm.mu.Lock()
	delete(tm.tokens, token)
	tm.mu.Unlock()
}

// GetWorkerEndpoint returns the worker endpoint for a token.
func (tm *TokenManager) GetWorkerEndpoint(token string) (string, error) {
	data, err := tm.ValidateToken(token)
	if err != nil {
		return "", err
	}
	return data.WorkerEndpoint, nil
}

// UpdateWorkerEndpoint updates the worker endpoint for a token.
func (tm *TokenManager) UpdateWorkerEndpoint(token, endpoint string) error {
	tm.mu.Lock()
	defer tm.mu.Unlock()

	data, exists := tm.tokens[token]
	if !exists {
		return fmt.Errorf("token not found")
	}

	data.WorkerEndpoint = endpoint
	return nil
}

// RefreshToken extends the expiry of a token.
func (tm *TokenManager) RefreshToken(token string, extension time.Duration) error {
	tm.mu.Lock()
	defer tm.mu.Unlock()

	data, exists := tm.tokens[token]
	if !exists {
		return fmt.Errorf("token not found")
	}

	newExpiry := time.Now().Add(extension)
	if newExpiry.Sub(data.IssuedAt) > tm.maxTTL {
		newExpiry = data.IssuedAt.Add(tm.maxTTL)
	}
	data.ExpiresAt = newExpiry

	return nil
}

// CleanupExpired removes all expired tokens.
func (tm *TokenManager) CleanupExpired() int {
	tm.mu.Lock()
	defer tm.mu.Unlock()

	now := time.Now()
	expired := 0

	for token, data := range tm.tokens {
		if now.After(data.ExpiresAt) {
			delete(tm.tokens, token)
			expired++
		}
	}

	return expired
}

// ListTokens returns all active tokens (for debugging/admin).
func (tm *TokenManager) ListTokens() []*TokenData {
	tm.mu.RLock()
	defer tm.mu.RUnlock()

	result := make([]*TokenData, 0, len(tm.tokens))
	for _, data := range tm.tokens {
		result = append(result, data)
	}
	return result
}

// signToken creates an HMAC-signed token string.
func (tm *TokenManager) signToken(tokenBytes []byte) string {
	mac := hmac.New(sha256.New, tm.secret)
	mac.Write(tokenBytes)
	signature := mac.Sum(nil)

	// Combine token and signature
	combined := append(tokenBytes, signature...)
	return base64.RawURLEncoding.EncodeToString(combined)
}

// Encode encodes token data to JSON for storage.
func (td *TokenData) Encode() ([]byte, error) {
	return json.Marshal(td)
}

// DecodeTokenData decodes token data from JSON.
func DecodeTokenData(data []byte) (*TokenData, error) {
	var td TokenData
	if err := json.Unmarshal(data, &td); err != nil {
		return nil, err
	}
	return &td, nil
}
