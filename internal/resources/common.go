package resources

import (
	"fmt"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

const (
	// DefaultBuildkitImage is the default buildkit daemon image.
	DefaultBuildkitImage = "moby/buildkit:master-rootless"
	// DefaultGatewayImage is the default gateway image.
	DefaultGatewayImage = "ghcr.io/smrt-devops/buildkit-controller/gateway:latest"
)

// GetBuildkitImage returns the buildkit image, using defaults if not specified.
func GetBuildkitImage(pool *buildkitv1alpha1.BuildKitPool, defaultImage string) string {
	if pool.Spec.BuildkitImage != "" {
		return pool.Spec.BuildkitImage
	}
	if defaultImage != "" {
		return defaultImage
	}
	return DefaultBuildkitImage
}

// GetGatewayImage returns the gateway image, using defaults if not specified.
func GetGatewayImage(pool *buildkitv1alpha1.BuildKitPool, defaultImage string) string {
	if pool.Spec.GatewayImage != "" {
		return pool.Spec.GatewayImage
	}
	if defaultImage != "" {
		return defaultImage
	}
	return DefaultGatewayImage
}

// GetSecretName returns the TLS secret name for a pool.
func GetSecretName(poolName string) string {
	return fmt.Sprintf("%s-tls", poolName)
}

// GetConfigMapName returns the configmap name for a pool.
func GetConfigMapName(poolName string) string {
	return fmt.Sprintf("%s-config", poolName)
}

// GetClientSecretName returns the client certificate secret name for a pool.
func GetClientSecretName(poolName string) string {
	return fmt.Sprintf("%s-client-certs", poolName)
}

// GetPoolLabels returns the standard labels for a BuildKit pool.
func GetPoolLabels(poolName string) map[string]string {
	return map[string]string{
		"app.kubernetes.io/name":        "buildkitd",
		"app.kubernetes.io/component":   "pool",
		"app.kubernetes.io/managed-by":  "buildkit-controller",
		"buildkit.smrt-devops.net/pool": poolName,
	}
}

// GetTLSSecretLabels returns the labels for a TLS secret.
func GetTLSSecretLabels(poolName string) map[string]string {
	return utils.MergeLabels(
		utils.DefaultLabels("pool-tls", poolName),
	)
}

// GetClientCertSecretLabels returns the labels for a client certificate secret.
func GetClientCertSecretLabels(poolName string) map[string]string {
	return utils.MergeLabels(
		utils.DefaultLabels("client-certs", poolName),
	)
}

// GetConfigMapLabels returns the labels for a ConfigMap.
func GetConfigMapLabels(poolName string) map[string]string {
	return utils.MergeLabels(
		utils.DefaultLabels("pool-config", poolName),
	)
}
