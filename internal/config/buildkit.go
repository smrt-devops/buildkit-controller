package config

import (
	"fmt"
	"strings"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
)

// GenerateBuildkitConfig generates buildkitd.toml from pool spec.
func GenerateBuildkitConfig(pool *buildkitv1alpha1.BuildKitPool) string {
	var config strings.Builder

	// Basic configuration
	config.WriteString("debug = false\n\n")

	// gRPC configuration
	config.WriteString("[grpc]\n")
	config.WriteString("  address = [\"tcp://0.0.0.0:1234\"]\n\n")

	// Worker configuration
	config.WriteString("[worker.oci]\n")
	config.WriteString("  enabled = true\n")
	config.WriteString("  max-parallelism = 4\n")
	config.WriteString("  gc = true\n")
	config.WriteString("  gckeepstorage = 10000\n\n")

	config.WriteString("[worker.containerd]\n")
	config.WriteString("  enabled = false\n\n")

	// Registry configuration
	if len(pool.Spec.Cache.Backends) > 0 {
		for _, backend := range pool.Spec.Cache.Backends {
			if backend.Type == "registry" && backend.Registry != nil {
				config.WriteString(fmt.Sprintf("[registry.%q]\n", backend.Registry.Endpoint))
				if backend.Registry.Insecure {
					config.WriteString("  http = true\n")
				} else {
					config.WriteString("  http = false\n")
				}
				if backend.Registry.CredentialsSecret != "" {
					// Note: Credentials would be mounted and configured separately
					config.WriteString(fmt.Sprintf("  # credentials from secret: %s\n", backend.Registry.CredentialsSecret))
				}
				config.WriteString("\n")
			}
		}
	}

	// Garbage collection configuration
	if pool.Spec.Cache.GC.Enabled {
		config.WriteString("# Garbage collection configured via pool spec\n")
		config.WriteString("# Schedule: " + pool.Spec.Cache.GC.Schedule + "\n")
		if pool.Spec.Cache.GC.KeepStorage != "" {
			config.WriteString("# Keep storage: " + pool.Spec.Cache.GC.KeepStorage + "\n")
		}
		if pool.Spec.Cache.GC.KeepDuration != "" {
			config.WriteString("# Keep duration: " + pool.Spec.Cache.GC.KeepDuration + "\n")
		}
		config.WriteString("\n")
	}

	return config.String()
}
