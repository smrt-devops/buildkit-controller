package tls

import (
	"context"
	"fmt"
	"time"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/certs"
	"github.com/smrt-devops/buildkit-controller/internal/controller/shared"
)

// GenerateServiceDNSNames generates DNS names for a Kubernetes service.
// Includes localhost for local development/port-forwarding scenarios.
func GenerateServiceDNSNames(serviceName, namespace, externalHostname string) []string {
	san := []string{
		serviceName,
		fmt.Sprintf("%s.%s", serviceName, namespace),
		shared.GenerateServiceHostname(serviceName, namespace),
		fmt.Sprintf("%s.%s.svc.cluster.local", serviceName, namespace),
		// Include localhost for port-forward/local development
		"localhost",
	}

	if externalHostname != "" {
		san = append(san, externalHostname)
	}

	return san
}

// CheckCertificateRotation determines if a certificate needs rotation.
func CheckCertificateRotation(
	ctx context.Context,
	certManager *certs.CertificateManager,
	pool *buildkitv1alpha1.BuildKitPool,
	namespace string,
) (bool, error) {
	// Determine secret name - use status if available, otherwise use expected name
	secretName := pool.Status.TLSSecretName
	if secretName == "" {
		// Status not set yet, but secret might exist - check by expected name
		secretName = shared.GenerateResourceName(pool.Name, "tls")
	}

	// Try to get existing certificate info from secret
	existingInfo, err := certManager.ParseCertificateFromSecret(ctx, secretName, namespace, "tls.crt")
	if err == nil && existingInfo != nil {
		// Certificate exists and is valid, check if rotation is needed
		renewBefore := GetRenewBeforeDuration(pool)
		return certManager.ShouldRotateCertificate(existingInfo, renewBefore), nil
	}

	// Secret doesn't exist or parsing failed
	// If status has cert info, use it as fallback
	if pool.Status.ServerCert != nil {
		renewBefore := GetRenewBeforeDuration(pool)
		certInfo := &certs.CertificateInfo{
			NotAfter:    pool.Status.ServerCert.NotAfter.Time,
			RenewalTime: pool.Status.ServerCert.RenewalTime.Time,
		}
		return certManager.ShouldRotateCertificate(certInfo, renewBefore), nil
	}

	// No certificate exists (secret doesn't exist and status is empty)
	return true, nil
}

// GetRenewBeforeDuration extracts the renew before duration from pool spec.
func GetRenewBeforeDuration(pool *buildkitv1alpha1.BuildKitPool) time.Duration {
	if pool.Spec.TLS.Auto != nil && pool.Spec.TLS.Auto.RotateBeforeExpiry != "" {
		return shared.ParseDurationWithDefault(pool.Spec.TLS.Auto.RotateBeforeExpiry, shared.DefaultCertRenewalTime)
	}
	return shared.DefaultCertRenewalTime
}

// GetCertDuration extracts the certificate duration from pool spec.
func GetCertDuration(pool *buildkitv1alpha1.BuildKitPool) time.Duration {
	if pool.Spec.TLS.Auto != nil && pool.Spec.TLS.Auto.ServerCertDuration != "" {
		return shared.ParseDurationWithDefault(pool.Spec.TLS.Auto.ServerCertDuration, shared.DefaultCertDuration)
	}
	return shared.DefaultCertDuration
}

// GetExternalHostname extracts the external hostname from pool spec.
// Checks Gateway API hostname first (preferred), then networking.external.hostname.
func GetExternalHostname(pool *buildkitv1alpha1.BuildKitPool) string {
	// Gateway API hostname takes precedence
	if pool.Spec.Gateway.GatewayAPI != nil && pool.Spec.Gateway.GatewayAPI.Enabled && pool.Spec.Gateway.GatewayAPI.Hostname != "" {
		return pool.Spec.Gateway.GatewayAPI.Hostname
	}
	// Fall back to networking.external.hostname
	if pool.Spec.Networking.External != nil {
		return pool.Spec.Networking.External.Hostname
	}
	return ""
}

// CheckWorkerCertificateRotation determines if a worker certificate needs rotation.
func CheckWorkerCertificateRotation(
	ctx context.Context,
	certManager *certs.CertificateManager,
	workerSecretName, namespace string,
	pool *buildkitv1alpha1.BuildKitPool,
) (bool, error) {
	// Check if secret exists
	existingInfo, err := certManager.ParseCertificateFromSecret(ctx, workerSecretName, namespace, "tls.crt")
	if err != nil {
		// Secret doesn't exist or can't be parsed, needs to be created
		return true, nil
	}

	if existingInfo != nil {
		renewBefore := GetRenewBeforeDuration(pool)
		return certManager.ShouldRotateCertificate(existingInfo, renewBefore), nil
	}

	return false, nil
}
