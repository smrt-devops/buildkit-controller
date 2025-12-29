package api

import (
	"fmt"
	"strings"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
)

// RBACChecker checks if a user has access to pools based on RBAC rules.
type RBACChecker struct{}

// NewRBACChecker creates a new RBAC checker.
func NewRBACChecker() *RBACChecker {
	return &RBACChecker{}
}

// CheckPoolAccess checks if the given identity has access to the specified pools.
// It checks against the pool's RBAC configuration.
func (c *RBACChecker) CheckPoolAccess(identity string, poolNames []string, pools map[string]*buildkitv1alpha1.BuildKitPool) error {
	for _, poolName := range poolNames {
		pool, exists := pools[poolName]
		if !exists {
			return fmt.Errorf("pool %s not found", poolName)
		}

		// Check if RBAC is enabled for this pool
		if pool.Spec.Auth.RBAC == nil || !pool.Spec.Auth.RBAC.Enabled {
			// RBAC not enabled, allow access
			continue
		}

		// Check RBAC rules
		hasAccess := false
		for _, rule := range pool.Spec.Auth.RBAC.Rules {
			if c.matchesUser(identity, rule.Users) && c.matchesPool(poolName, rule.Pools) {
				hasAccess = true
				break
			}
		}

		if !hasAccess {
			return fmt.Errorf("access denied: user %s does not have access to pool %s", identity, poolName)
		}
	}

	return nil
}

// matchesUser checks if the identity matches any of the user patterns.
func (c *RBACChecker) matchesUser(identity string, patterns []string) bool {
	for _, pattern := range patterns {
		if c.matchPattern(identity, pattern) {
			return true
		}
	}
	return false
}

// matchesPool checks if the pool name matches any of the pool patterns.
func (c *RBACChecker) matchesPool(poolName string, patterns []string) bool {
	for _, pattern := range patterns {
		if c.matchPattern(poolName, pattern) {
			return true
		}
	}
	return false
}

// matchPattern matches a value against a pattern.
// Supports:
// - Exact match: "system:serviceaccount:default:my-sa"
// - Wildcard suffix: "system:serviceaccount:*"
// - Full wildcard: "*".
func (c *RBACChecker) matchPattern(value, pattern string) bool {
	if pattern == "*" {
		return true
	}

	// Simple wildcard matching (can be enhanced)
	if strings.HasSuffix(pattern, "*") {
		prefix := pattern[:len(pattern)-1]
		return strings.HasPrefix(value, prefix)
	}

	return value == pattern
}
