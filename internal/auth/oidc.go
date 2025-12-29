package auth

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"fmt"
	"time"

	"github.com/coreos/go-oidc/v3/oidc"
	"golang.org/x/oauth2"
)

// OIDCVerifier handles OIDC token verification.
type OIDCVerifier struct {
	provider   *oidc.Provider
	verifier   *oidc.IDTokenVerifier
	audience   string
	userClaim  string
	poolsClaim string
}

// NewOIDCVerifier creates a new OIDC verifier.
func NewOIDCVerifier(ctx context.Context, issuer, audience, userClaim, poolsClaim string) (*OIDCVerifier, error) {
	provider, err := oidc.NewProvider(ctx, issuer)
	if err != nil {
		return nil, fmt.Errorf("failed to create OIDC provider: %w", err)
	}

	verifier := provider.Verifier(&oidc.Config{
		ClientID: audience,
	})

	return &OIDCVerifier{
		provider:   provider,
		verifier:   verifier,
		audience:   audience,
		userClaim:  userClaim,
		poolsClaim: poolsClaim,
	}, nil
}

// VerifyToken verifies an OIDC token and extracts claims.
func (v *OIDCVerifier) VerifyToken(ctx context.Context, token string) (*OIDCClaims, error) {
	idToken, err := v.verifier.Verify(ctx, token)
	if err != nil {
		return nil, fmt.Errorf("failed to verify token: %w", err)
	}

	var claims map[string]interface{}
	if err := idToken.Claims(&claims); err != nil {
		return nil, fmt.Errorf("failed to extract claims: %w", err)
	}

	result := &OIDCClaims{
		Subject: idToken.Subject,
		Claims:  claims,
	}

	// Extract user identity
	if v.userClaim != "" {
		if user, ok := claims[v.userClaim].(string); ok {
			result.User = user
		} else {
			// Fallback to subject
			result.User = idToken.Subject
		}
	} else {
		result.User = idToken.Subject
	}

	// Extract pools access
	if v.poolsClaim != "" {
		if pools, ok := claims[v.poolsClaim]; ok {
			switch v := pools.(type) {
			case string:
				result.Pools = []string{v}
			case []interface{}:
				for _, p := range v {
					if pool, ok := p.(string); ok {
						result.Pools = append(result.Pools, pool)
					}
				}
			case []string:
				result.Pools = v
			}
		}
	}

	return result, nil
}

// OIDCClaims contains extracted OIDC claims.
type OIDCClaims struct {
	Subject string
	User    string
	Pools   []string
	Claims  map[string]interface{}
}

// GeneratePKCE generates PKCE code verifier and challenge.
func GeneratePKCE() (verifier, challenge string, err error) {
	// Generate random verifier
	bytes := make([]byte, 32)
	if _, err := rand.Read(bytes); err != nil {
		return "", "", err
	}
	verifier = base64.RawURLEncoding.EncodeToString(bytes)

	// Generate challenge
	hash := sha256.Sum256([]byte(verifier))
	challenge = base64.RawURLEncoding.EncodeToString(hash[:])

	return verifier, challenge, nil
}

// GetOAuth2Config returns OAuth2 configuration for token exchange.
func (v *OIDCVerifier) GetOAuth2Config(clientID, clientSecret, redirectURL string, scopes []string) *oauth2.Config {
	return &oauth2.Config{
		ClientID:     clientID,
		ClientSecret: clientSecret,
		RedirectURL:  redirectURL,
		Endpoint:     v.provider.Endpoint(),
		Scopes:       scopes,
	}
}

// RefreshToken refreshes an access token using a refresh token.
func (v *OIDCVerifier) RefreshToken(ctx context.Context, config *oauth2.Config, refreshToken string) (*oauth2.Token, error) {
	token := &oauth2.Token{
		RefreshToken: refreshToken,
		Expiry:       time.Now().Add(-time.Hour), // Force refresh
	}
	return config.TokenSource(ctx, token).Token()
}
