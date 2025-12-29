package certs

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"fmt"
	"math/big"
	"net"
	"time"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"sigs.k8s.io/controller-runtime/pkg/client"

	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

// CertificateInfo contains certificate validity information.
type CertificateInfo struct {
	NotBefore   time.Time
	NotAfter    time.Time
	RenewalTime time.Time
}

// CertificateRequest defines a certificate request.
type CertificateRequest struct {
	CommonName   string
	DNSNames     []string
	IPAddresses  []net.IP
	Organization string
	Duration     time.Duration
	IsServer     bool
	IsClient     bool
}

// CertificateManager manages certificate generation and rotation.
type CertificateManager struct {
	client    client.Client
	caManager *CAManager
	log       utils.Logger
	config    *Config
}

// NewCertificateManager creates a new certificate manager.
func NewCertificateManager(k8sClient client.Client, caManager *CAManager, log utils.Logger, config *Config) *CertificateManager {
	if config == nil {
		config = LoadConfig()
	}
	return &CertificateManager{
		client:    k8sClient,
		caManager: caManager,
		log:       log,
		config:    config,
	}
}

// IssueCertificate issues a new certificate from the CA.
// All certificates use ECDSA P-256 (elliptic-curve) keys - this is mandatory.
func (m *CertificateManager) IssueCertificate(ctx context.Context, req *CertificateRequest) (certPEM, keyPEM []byte, info *CertificateInfo, err error) {
	// Get CA
	ca, err := m.caManager.GetCA(ctx)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("failed to get CA: %w", err)
	}

	// Generate private key using ECDSA P-256 (elliptic-curve is mandatory)
	privateKey, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("failed to generate ECDSA private key: %w", err)
	}

	// Determine duration
	duration := req.Duration
	if duration == 0 {
		// Use configurable default based on certificate type
		if req.IsServer {
			duration = m.config.DefaultServerCertDuration
		} else {
			duration = m.config.DefaultClientCertDuration
		}
	}

	// Create certificate template
	template := &x509.Certificate{
		SerialNumber: big.NewInt(time.Now().Unix()),
		Subject: pkix.Name{
			CommonName:   req.CommonName,
			Organization: []string{req.Organization},
		},
		NotBefore:             time.Now(),
		NotAfter:              time.Now().Add(duration),
		DNSNames:              req.DNSNames,
		IPAddresses:           req.IPAddresses,
		KeyUsage:              x509.KeyUsageDigitalSignature | x509.KeyUsageKeyEncipherment,
		ExtKeyUsage:           []x509.ExtKeyUsage{},
		BasicConstraintsValid: true,
	}

	if req.IsServer {
		template.ExtKeyUsage = append(template.ExtKeyUsage, x509.ExtKeyUsageServerAuth)
	}
	if req.IsClient {
		template.ExtKeyUsage = append(template.ExtKeyUsage, x509.ExtKeyUsageClientAuth)
	}

	// Create certificate
	certDER, err := x509.CreateCertificate(rand.Reader, template, ca.Cert, &privateKey.PublicKey, ca.Key)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("failed to create certificate: %w", err)
	}

	// Parse certificate to get validity info
	cert, err := x509.ParseCertificate(certDER)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("failed to parse certificate: %w", err)
	}

	// Encode to PEM
	certPEM = pem.EncodeToMemory(&pem.Block{
		Type:  "CERTIFICATE",
		Bytes: certDER,
	})

	// Encode private key as ECDSA (elliptic-curve is mandatory)
	keyDER, err := x509.MarshalECPrivateKey(privateKey)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("failed to marshal ECDSA private key: %w", err)
	}
	keyPEM = pem.EncodeToMemory(&pem.Block{
		Type:  "EC PRIVATE KEY",
		Bytes: keyDER,
	})

	// Calculate renewal time
	// Use the configured default renewal time, but if certificate duration is shorter,
	// use 80% of the duration to ensure renewal time is in the future
	certDuration := cert.NotAfter.Sub(cert.NotBefore)
	defaultRenewalWindow := m.config.DefaultRenewalTime
	if certDuration < defaultRenewalWindow {
		// For short-lived certificates, renew at 80% of duration
		renewalTime := cert.NotAfter.Add(-certDuration * 80 / 100)
		info = &CertificateInfo{
			NotBefore:   cert.NotBefore,
			NotAfter:    cert.NotAfter,
			RenewalTime: renewalTime,
		}
	} else {
		// For long-lived certificates, use the default renewal window
		renewalTime := cert.NotAfter.Add(-defaultRenewalWindow)
		info = &CertificateInfo{
			NotBefore:   cert.NotBefore,
			NotAfter:    cert.NotAfter,
			RenewalTime: renewalTime,
		}
	}

	return certPEM, keyPEM, info, nil
}

// ShouldRotateCertificate checks if a certificate should be rotated.
func (m *CertificateManager) ShouldRotateCertificate(certInfo *CertificateInfo, rotateBefore time.Duration) bool {
	if certInfo == nil {
		return true
	}

	now := time.Now()
	// Use the stored RenewalTime if it's set and valid
	var renewalTime time.Time
	if !certInfo.RenewalTime.IsZero() {
		renewalTime = certInfo.RenewalTime
	} else {
		// Fallback: calculate renewal time based on rotateBefore
		// But ensure it's not in the past (for short-lived certs)
		certDuration := certInfo.NotAfter.Sub(certInfo.NotBefore)
		if certDuration < rotateBefore {
			// For short-lived certificates, renew at 80% of duration
			renewalTime = certInfo.NotAfter.Add(-certDuration * 80 / 100)
		} else {
			renewalTime = certInfo.NotAfter.Add(-rotateBefore)
		}
	}

	return now.After(renewalTime)
}

// ParseCertificateFromSecret parses a certificate from a Kubernetes secret.
func (m *CertificateManager) ParseCertificateFromSecret(ctx context.Context, secretName, namespace, certKey string) (*CertificateInfo, error) {
	secret := &corev1.Secret{}
	err := m.client.Get(ctx, client.ObjectKey{Name: secretName, Namespace: namespace}, secret)
	if err != nil {
		return nil, fmt.Errorf("failed to get secret: %w", err)
	}

	certPEM, ok := secret.Data[certKey]
	if !ok {
		return nil, fmt.Errorf("certificate key %s not found in secret", certKey)
	}

	// Parse certificate
	block, _ := pem.Decode(certPEM)
	if block == nil {
		return nil, fmt.Errorf("failed to decode certificate PEM")
	}

	cert, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		return nil, fmt.Errorf("failed to parse certificate: %w", err)
	}

	// Calculate renewal time
	// Use the configured default renewal time, but if certificate duration is shorter,
	// use 80% of the duration to ensure renewal time is in the future
	certDuration := cert.NotAfter.Sub(cert.NotBefore)
	defaultRenewalWindow := m.config.DefaultRenewalTime
	var renewalTime time.Time
	if certDuration < defaultRenewalWindow {
		// For short-lived certificates, renew at 80% of duration
		renewalTime = cert.NotAfter.Add(-certDuration * 80 / 100)
	} else {
		// For long-lived certificates, use the default renewal window
		renewalTime = cert.NotAfter.Add(-defaultRenewalWindow)
	}

	return &CertificateInfo{
		NotBefore:   cert.NotBefore,
		NotAfter:    cert.NotAfter,
		RenewalTime: renewalTime,
	}, nil
}

// StoreCertificate stores a certificate in a Kubernetes secret.
func (m *CertificateManager) StoreCertificate(ctx context.Context, secretName, namespace string, certPEM, keyPEM, caCertPEM []byte, labels map[string]string) error {
	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{
			Name:      secretName,
			Namespace: namespace,
			Labels:    labels,
		},
		Type: corev1.SecretTypeOpaque,
		Data: map[string][]byte{
			"tls.crt": certPEM,
			"tls.key": keyPEM,
		},
	}

	if caCertPEM != nil {
		secret.Data["ca.crt"] = caCertPEM
	}

	// Create or update secret
	existingSecret := &corev1.Secret{}
	err := m.client.Get(ctx, client.ObjectKey{Name: secretName, Namespace: namespace}, existingSecret)
	if err != nil {
		// Secret doesn't exist, create it
		if err := m.client.Create(ctx, secret); err != nil {
			return fmt.Errorf("failed to create certificate secret: %w", err)
		}
		m.log.Info("Created certificate secret", "secret", secretName)
	} else {
		// Secret exists, check if data has changed
		dataChanged := false
		if !bytes.Equal(existingSecret.Data["tls.crt"], secret.Data["tls.crt"]) ||
			!bytes.Equal(existingSecret.Data["tls.key"], secret.Data["tls.key"]) {
			dataChanged = true
		}
		if caCertPEM != nil {
			if !bytes.Equal(existingSecret.Data["ca.crt"], secret.Data["ca.crt"]) {
				dataChanged = true
			}
		} else if existingSecret.Data["ca.crt"] != nil {
			// New secret doesn't have CA cert but existing does
			dataChanged = true
		}

		// Check if labels changed
		labelsChanged := false
		if existingSecret.Labels == nil {
			existingSecret.Labels = make(map[string]string)
			labelsChanged = true
		}
		for k, v := range labels {
			if existingSecret.Labels[k] != v {
				labelsChanged = true
				existingSecret.Labels[k] = v
			}
		}

		// Only update if data or labels changed
		if dataChanged || labelsChanged {
			if dataChanged {
				existingSecret.Data = secret.Data
			}
			if err := m.client.Update(ctx, existingSecret); err != nil {
				return fmt.Errorf("failed to update certificate secret: %w", err)
			}
			m.log.Info("Updated certificate secret", "secret", secretName)
		}
		// If nothing changed, skip update to avoid triggering watch events
	}

	return nil
}

// StoreClientCertificate stores a client certificate in a Kubernetes secret.
func (m *CertificateManager) StoreClientCertificate(ctx context.Context, secretName, namespace string, certPEM, keyPEM, caCertPEM []byte, labels map[string]string) error {
	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{
			Name:      secretName,
			Namespace: namespace,
			Labels:    labels,
		},
		Type: corev1.SecretTypeOpaque,
		Data: map[string][]byte{
			"client.crt": certPEM,
			"client.key": keyPEM,
			"ca.crt":     caCertPEM,
		},
	}

	// Create or update secret
	existingSecret := &corev1.Secret{}
	err := m.client.Get(ctx, client.ObjectKey{Name: secretName, Namespace: namespace}, existingSecret)
	if err != nil {
		// Secret doesn't exist, create it
		if err := m.client.Create(ctx, secret); err != nil {
			return fmt.Errorf("failed to create client certificate secret: %w", err)
		}
		m.log.Info("Created client certificate secret", "secret", secretName)
	} else {
		// Secret exists, check if data has changed
		dataChanged := false
		if !bytes.Equal(existingSecret.Data["client.crt"], secret.Data["client.crt"]) ||
			!bytes.Equal(existingSecret.Data["client.key"], secret.Data["client.key"]) ||
			!bytes.Equal(existingSecret.Data["ca.crt"], secret.Data["ca.crt"]) {
			dataChanged = true
		}

		// Check if labels changed
		labelsChanged := false
		if existingSecret.Labels == nil {
			existingSecret.Labels = make(map[string]string)
			labelsChanged = true
		}
		for k, v := range labels {
			if existingSecret.Labels[k] != v {
				labelsChanged = true
				existingSecret.Labels[k] = v
			}
		}

		// Only update if data or labels changed
		if dataChanged || labelsChanged {
			if dataChanged {
				existingSecret.Data = secret.Data
			}
			if err := m.client.Update(ctx, existingSecret); err != nil {
				return fmt.Errorf("failed to update client certificate secret: %w", err)
			}
			m.log.Info("Updated client certificate secret", "secret", secretName)
		}
		// If nothing changed, skip update to avoid triggering watch events
	}

	return nil
}
