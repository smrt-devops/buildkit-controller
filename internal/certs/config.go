package certs

import (
	"os"
	"time"
)

// Config holds certificate configuration.
type Config struct {
	// DefaultServerCertDuration is the default duration for server certificates.
	DefaultServerCertDuration time.Duration

	// DefaultClientCertDuration is the default duration for client certificates.
	DefaultClientCertDuration time.Duration

	// DefaultRotateBeforeExpiry is the default time before expiry to rotate certificates.
	DefaultRotateBeforeExpiry time.Duration

	// DefaultRenewalTime is the default time before expiry to set renewal time.
	DefaultRenewalTime time.Duration
}

// LoadConfig loads certificate configuration from environment variables.
func LoadConfig() *Config {
	config := &Config{
		// Defaults
		DefaultServerCertDuration: 8760 * time.Hour, // 1 year
		DefaultClientCertDuration: 8760 * time.Hour, // 1 year
		DefaultRotateBeforeExpiry: 720 * time.Hour,  // 30 days
		DefaultRenewalTime:        720 * time.Hour,  // 30 days
	}

	// Load from environment variables
	if val := os.Getenv("CERT_DEFAULT_SERVER_DURATION"); val != "" {
		if parsed, err := time.ParseDuration(val); err == nil {
			config.DefaultServerCertDuration = parsed
		}
	}

	if val := os.Getenv("CERT_DEFAULT_CLIENT_DURATION"); val != "" {
		if parsed, err := time.ParseDuration(val); err == nil {
			config.DefaultClientCertDuration = parsed
		}
	}

	if val := os.Getenv("CERT_DEFAULT_ROTATE_BEFORE_EXPIRY"); val != "" {
		if parsed, err := time.ParseDuration(val); err == nil {
			config.DefaultRotateBeforeExpiry = parsed
		}
	}

	if val := os.Getenv("CERT_DEFAULT_RENEWAL_TIME"); val != "" {
		if parsed, err := time.ParseDuration(val); err == nil {
			config.DefaultRenewalTime = parsed
		}
	}

	return config
}
