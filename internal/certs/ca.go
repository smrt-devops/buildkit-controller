package certs

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"fmt"
	"math/big"
	"time"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"sigs.k8s.io/controller-runtime/pkg/client"

	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

const (
	// DefaultCAName is the default name for the CA secret.
	DefaultCAName = "buildkit-ca"
	// DefaultCANamespace is the default namespace for the CA secret.
	DefaultCANamespace = "buildkit-system"
	// CADuration is the CA certificate duration (10 years).
	CADuration = 10 * 365 * 24 * time.Hour
)

// CA represents a Certificate Authority.
// The CA key is always ECDSA (elliptic-curve) - this is mandatory.
type CA struct {
	Cert *x509.Certificate
	Key  *ecdsa.PrivateKey // Always ECDSA P-256 (elliptic-curve)
}

// CAManager manages the CA certificate.
type CAManager struct {
	client    client.Client
	caName    string
	namespace string
	log       utils.Logger
}

// NewCAManager creates a new CA manager.
func NewCAManager(k8sClient client.Client, caName, namespace string, log utils.Logger) *CAManager {
	if caName == "" {
		caName = DefaultCAName
	}
	if namespace == "" {
		namespace = DefaultCANamespace
	}
	return &CAManager{
		client:    k8sClient,
		caName:    caName,
		namespace: namespace,
		log:       log,
	}
}

// EnsureCA ensures the CA exists, creating it if necessary.
func (m *CAManager) EnsureCA(ctx context.Context) (*CA, error) {
	// Try to get existing CA
	secret := &corev1.Secret{}
	err := m.client.Get(ctx, client.ObjectKey{Name: m.caName, Namespace: m.namespace}, secret)
	if err == nil {
		// CA exists, parse it
		ca, parseErr := parseCAFromSecret(secret)
		if parseErr != nil {
			m.log.Info("Failed to parse existing CA, regenerating", "error", parseErr)
		} else {
			m.log.Info("Using existing CA", "secret", m.caName)
			return ca, nil
		}
	}

	// CA doesn't exist or is invalid, create new one
	m.log.Info("Creating new CA", "secret", m.caName)
	ca, err := m.generateCA()
	if err != nil {
		return nil, fmt.Errorf("failed to generate CA: %w", err)
	}

	// Store CA in secret
	if err := m.storeCA(ctx, ca); err != nil {
		return nil, fmt.Errorf("failed to store CA: %w", err)
	}

	return ca, nil
}

// GetCA retrieves the CA from the secret.
func (m *CAManager) GetCA(ctx context.Context) (*CA, error) {
	secret := &corev1.Secret{}
	err := m.client.Get(ctx, client.ObjectKey{Name: m.caName, Namespace: m.namespace}, secret)
	if err != nil {
		return nil, fmt.Errorf("failed to get CA secret: %w", err)
	}

	ca, err := parseCAFromSecret(secret)
	if err != nil {
		return nil, fmt.Errorf("failed to parse CA: %w", err)
	}

	return ca, nil
}

// GetCACertPEM returns the CA certificate as PEM.
func (m *CAManager) GetCACertPEM(ctx context.Context) ([]byte, error) {
	ca, err := m.GetCA(ctx)
	if err != nil {
		return nil, err
	}

	return pem.EncodeToMemory(&pem.Block{
		Type:  "CERTIFICATE",
		Bytes: ca.Cert.Raw,
	}), nil
}

// generateCA generates a new CA certificate and key.
// The CA key is always ECDSA P-256 (elliptic-curve) - this is mandatory.
func (m *CAManager) generateCA() (*CA, error) {
	// Generate CA private key using ECDSA P-256 (elliptic-curve is mandatory)
	caKey, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, fmt.Errorf("failed to generate ECDSA CA key: %w", err)
	}

	// Create CA certificate template
	caTemplate := &x509.Certificate{
		SerialNumber: big.NewInt(1),
		Subject: pkix.Name{
			Organization:  []string{"BuildKit Controller"},
			CommonName:    "BuildKit CA",
			Country:       []string{"US"},
			Province:      []string{""},
			Locality:      []string{""},
			StreetAddress: []string{""},
			PostalCode:    []string{""},
		},
		NotBefore:             time.Now(),
		NotAfter:              time.Now().Add(CADuration),
		IsCA:                  true,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth, x509.ExtKeyUsageServerAuth},
		KeyUsage:              x509.KeyUsageDigitalSignature | x509.KeyUsageCertSign,
		BasicConstraintsValid: true,
	}

	// Create CA certificate
	caCertDER, err := x509.CreateCertificate(rand.Reader, caTemplate, caTemplate, &caKey.PublicKey, caKey)
	if err != nil {
		return nil, err
	}

	// Parse the certificate
	caCert, err := x509.ParseCertificate(caCertDER)
	if err != nil {
		return nil, err
	}

	return &CA{
		Cert: caCert,
		Key:  caKey,
	}, nil
}

func (m *CAManager) storeCA(ctx context.Context, ca *CA) error {
	// Encode CA certificate
	caCertPEM := pem.EncodeToMemory(&pem.Block{
		Type:  "CERTIFICATE",
		Bytes: ca.Cert.Raw,
	})

	// Encode CA key as ECDSA (elliptic-curve is mandatory)
	caKeyDER, marshalErr := x509.MarshalECPrivateKey(ca.Key)
	if marshalErr != nil {
		return fmt.Errorf("failed to marshal ECDSA CA key: %w", marshalErr)
	}
	caKeyPEM := pem.EncodeToMemory(&pem.Block{
		Type:  "EC PRIVATE KEY",
		Bytes: caKeyDER,
	})

	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{
			Name:      m.caName,
			Namespace: m.namespace,
			Labels: map[string]string{
				"app.kubernetes.io/name":       "buildkit-controller",
				"app.kubernetes.io/component":  "ca",
				"app.kubernetes.io/managed-by": "buildkit-controller",
			},
		},
		Type: corev1.SecretTypeOpaque,
		Data: map[string][]byte{
			"ca.crt": caCertPEM,
			"ca.key": caKeyPEM,
		},
	}

	// Create or update secret
	existingSecret := &corev1.Secret{}
	getErr := m.client.Get(ctx, client.ObjectKey{Name: m.caName, Namespace: m.namespace}, existingSecret)
	if getErr != nil {
		// Secret doesn't exist, create it
		if createErr := m.client.Create(ctx, secret); createErr != nil {
			return fmt.Errorf("failed to create CA secret: %w", createErr)
		}
		m.log.Info("Created CA secret", "secret", m.caName)
	} else {
		// Secret exists, update it
		existingSecret.Data = secret.Data
		if updateErr := m.client.Update(ctx, existingSecret); updateErr != nil {
			return fmt.Errorf("failed to update CA secret: %w", updateErr)
		}
		m.log.Info("Updated CA secret", "secret", m.caName)
	}

	return nil
}

func parseCAFromSecret(secret *corev1.Secret) (*CA, error) {
	caCertPEM, ok := secret.Data["ca.crt"]
	if !ok {
		return nil, fmt.Errorf("ca.crt not found in secret")
	}

	caKeyPEM, ok := secret.Data["ca.key"]
	if !ok {
		return nil, fmt.Errorf("ca.key not found in secret")
	}

	// Parse CA certificate
	caBlock, _ := pem.Decode(caCertPEM)
	if caBlock == nil {
		return nil, fmt.Errorf("failed to decode CA certificate PEM")
	}

	caCert, err := x509.ParseCertificate(caBlock.Bytes)
	if err != nil {
		return nil, fmt.Errorf("failed to parse CA certificate: %w", err)
	}

	// Parse CA key - only ECDSA (elliptic-curve) keys are supported
	caKeyBlock, _ := pem.Decode(caKeyPEM)
	if caKeyBlock == nil {
		return nil, fmt.Errorf("failed to decode CA key PEM")
	}

	var caKey *ecdsa.PrivateKey
	if caKeyBlock.Type == "EC PRIVATE KEY" {
		// Parse as ECDSA private key (preferred format)
		parsedKey, parseErr := x509.ParseECPrivateKey(caKeyBlock.Bytes)
		if parseErr != nil {
			return nil, fmt.Errorf("failed to parse ECDSA CA key: %w", parseErr)
		}
		caKey = parsedKey
	} else {
		// Try parsing as PKCS8 format, but must be ECDSA (elliptic-curve is mandatory)
		key, parseErr := x509.ParsePKCS8PrivateKey(caKeyBlock.Bytes)
		if parseErr != nil {
			return nil, fmt.Errorf("failed to parse CA key (only ECDSA/elliptic-curve keys are supported): %w", parseErr)
		}
		var ok bool
		caKey, ok = key.(*ecdsa.PrivateKey)
		if !ok {
			return nil, fmt.Errorf("CA key is not ECDSA - only elliptic-curve keys are supported, got %T", key)
		}
	}

	return &CA{
		Cert: caCert,
		Key:  caKey,
	}, nil
}
