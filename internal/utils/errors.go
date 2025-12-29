package utils

import (
	"fmt"
	"time"

	"k8s.io/apimachinery/pkg/api/errors"
	"sigs.k8s.io/controller-runtime/pkg/client"
)

// WrapError wraps an error with additional context.
func WrapError(err error, format string, args ...interface{}) error {
	if err == nil {
		return nil
	}
	return fmt.Errorf("%s: %w", fmt.Sprintf(format, args...), err)
}

// IgnoreNotFound returns nil if the error is a "not found" error, otherwise returns the error.
// This is a convenience wrapper around client.IgnoreNotFound.
func IgnoreNotFound(err error) error {
	return client.IgnoreNotFound(err)
}

// RetryWithBackoff retries a function with exponential backoff.
// Returns the last error if all retries fail.
func RetryWithBackoff(maxRetries int, baseDelay time.Duration, fn func() error) error {
	var lastErr error
	for i := 0; i < maxRetries; i++ {
		if err := fn(); err == nil {
			return nil
		} else if errors.IsConflict(err) {
			lastErr = err
			if i < maxRetries-1 {
				delay := baseDelay * time.Duration(1<<uint(i)) // Exponential backoff
				time.Sleep(delay)
			}
		} else {
			return err // Non-conflict errors are returned immediately
		}
	}
	return lastErr
}
