package tls

import (
	"context"
	"fmt"
	"net"
	"time"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"
	clientutil "sigs.k8s.io/controller-runtime/pkg/client"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/certs"
	"github.com/smrt-devops/buildkit-controller/internal/controller/shared"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

type Manager struct {
	client      client.Client
	scheme      *runtime.Scheme
	certManager *certs.CertificateManager
	caManager   *certs.CAManager
	log         utils.Logger
}

func NewManager(k8sClient client.Client, scheme *runtime.Scheme, certManager *certs.CertificateManager, caManager *certs.CAManager, log utils.Logger) *Manager {
	return &Manager{
		client:      k8sClient,
		scheme:      scheme,
		certManager: certManager,
		caManager:   caManager,
		log:         log,
	}
}

func (r *Manager) Reconcile(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	return r.reconcileTLS(ctx, pool, namespace)
}

func (r *Manager) reconcileTLS(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	if !pool.Spec.TLS.Enabled {
		return nil
	}

	needsRotation, err := CheckCertificateRotation(ctx, r.certManager, pool, namespace)
	if err != nil {
		return fmt.Errorf("failed to check certificate rotation: %w", err)
	}

	if needsRotation {
		if err := r.issueServerCertificate(ctx, pool, namespace); err != nil {
			return fmt.Errorf("failed to issue server certificate: %w", err)
		}
		if err := r.issueClientCertificate(ctx, pool, namespace); err != nil {
			return fmt.Errorf("failed to issue client certificate: %w", err)
		}
		return nil
	}

	if pool.Status.TLSSecretName == "" {
		r.updateStatusFromExistingCert(ctx, pool, namespace)
	}

	workerSecretName := shared.GenerateResourceName(pool.Name, "worker-tls")
	workerNeedsRotation, err := CheckWorkerCertificateRotation(ctx, r.certManager, workerSecretName, namespace, pool)
	if err != nil {
		return fmt.Errorf("failed to check worker certificate rotation: %w", err)
	}
	if workerNeedsRotation {
		if err := r.issueWorkerServerCertificate(ctx, pool, namespace); err != nil {
			return fmt.Errorf("failed to issue worker server certificate: %w", err)
		}
	}

	return nil
}

func (r *Manager) updateStatusFromExistingCert(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) {
	secretName := shared.GenerateResourceName(pool.Name, "tls")
	existingInfo, err := r.certManager.ParseCertificateFromSecret(ctx, secretName, namespace, "tls.crt")
	if err != nil || existingInfo == nil {
		return
	}
	pool.Status.TLSSecretName = secretName
	pool.Status.ServerCert = &buildkitv1alpha1.CertificateInfo{
		NotBefore:   &metav1.Time{Time: existingInfo.NotBefore},
		NotAfter:    &metav1.Time{Time: existingInfo.NotAfter},
		RenewalTime: &metav1.Time{Time: existingInfo.RenewalTime},
	}
}

func (r *Manager) setOwnerReference(ctx context.Context, secretName, namespace string, owner client.Object) error {
	secret := &corev1.Secret{}
	if err := r.client.Get(ctx, types.NamespacedName{Name: secretName, Namespace: namespace}, secret); err != nil {
		return clientutil.IgnoreNotFound(err)
	}
	return utils.SetOwnerReference(ctx, r.client, owner, secret, r.scheme)
}

func (r *Manager) storeCertificateWithCA(ctx context.Context, secretName, namespace string, certPEM, keyPEM []byte, labelPrefix string, pool *buildkitv1alpha1.BuildKitPool) error {
	caCertPEM, err := r.caManager.GetCACertPEM(ctx)
	if err != nil {
		return fmt.Errorf("failed to get CA cert: %w", err)
	}

	labels := utils.DefaultLabels(labelPrefix, pool.Name)
	if err := r.certManager.StoreCertificate(ctx, secretName, namespace, certPEM, keyPEM, caCertPEM, labels); err != nil {
		return fmt.Errorf("failed to store certificate: %w", err)
	}

	return r.setOwnerReference(ctx, secretName, namespace, pool)
}

func (r *Manager) storeClientCertificateWithCA(ctx context.Context, secretName, namespace string, certPEM, keyPEM []byte, labelPrefix string, pool *buildkitv1alpha1.BuildKitPool) error {
	caCertPEM, err := r.caManager.GetCACertPEM(ctx)
	if err != nil {
		return fmt.Errorf("failed to get CA cert: %w", err)
	}

	labels := utils.DefaultLabels(labelPrefix, pool.Name)
	if err := r.certManager.StoreClientCertificate(ctx, secretName, namespace, certPEM, keyPEM, caCertPEM, labels); err != nil {
		return fmt.Errorf("failed to store client certificate: %w", err)
	}

	return r.setOwnerReference(ctx, secretName, namespace, pool)
}

func (r *Manager) issueServerCertificate(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	secretName := shared.GenerateResourceName(pool.Name, "tls")
	serviceName := pool.Name
	externalHostname := GetExternalHostname(pool)
	san := GenerateServiceDNSNames(serviceName, namespace, externalHostname)
	duration := GetCertDuration(pool)

	commonName := fmt.Sprintf("%s.%s.svc.cluster.local", serviceName, namespace)
	certPEM, keyPEM, certInfo, err := r.certManager.IssueCertificate(ctx, &certs.CertificateRequest{
		CommonName:   commonName,
		DNSNames:     san,
		IPAddresses:  []net.IP{net.ParseIP("127.0.0.1")},
		Organization: "BuildKit Pool",
		Duration:     duration,
		IsServer:     true,
	})
	if err != nil {
		return fmt.Errorf("failed to issue server certificate: %w", err)
	}

	if err := r.storeCertificateWithCA(ctx, secretName, namespace, certPEM, keyPEM, "pool-tls", pool); err != nil {
		return err
	}

	pool.Status.TLSSecretName = secretName
	pool.Status.ServerCert = &buildkitv1alpha1.CertificateInfo{
		NotBefore:   &metav1.Time{Time: certInfo.NotBefore},
		NotAfter:    &metav1.Time{Time: certInfo.NotAfter},
		RenewalTime: &metav1.Time{Time: certInfo.RenewalTime},
	}

	r.log.Info("Issued server certificate", "secret", secretName)
	return nil
}

func (r *Manager) issueClientCertificate(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	clientSecretName := shared.GenerateResourceName(pool.Name, "client-certs")
	duration := GetCertDuration(pool)

	clientCertPEM, clientKeyPEM, _, err := r.certManager.IssueCertificate(ctx, &certs.CertificateRequest{
		CommonName:   shared.GenerateClientCertCommonName(pool.Name),
		Organization: "BuildKit Client",
		Duration:     duration,
		IsClient:     true,
	})
	if err != nil {
		return fmt.Errorf("failed to issue client certificate: %w", err)
	}

	if err := r.storeClientCertificateWithCA(ctx, clientSecretName, namespace, clientCertPEM, clientKeyPEM, "client-certs", pool); err != nil {
		return err
	}

	r.log.Info("Issued client certificate", "secret", clientSecretName)
	return nil
}

func (r *Manager) issueWorkerServerCertificate(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	workerSecretName := shared.GenerateResourceName(pool.Name, "worker-tls")
	duration := GetCertDuration(pool)

	commonName := fmt.Sprintf("buildkit-worker.%s.svc.cluster.local", namespace)
	ipAddresses := []net.IP{
		net.ParseIP("10.244.0.0"),
		net.ParseIP("10.96.0.0"),
		net.ParseIP("172.16.0.0"),
	}

	certPEM, keyPEM, certInfo, err := r.certManager.IssueCertificate(ctx, &certs.CertificateRequest{
		CommonName:   commonName,
		DNSNames:     []string{commonName, fmt.Sprintf("*.%s.svc.cluster.local", namespace)},
		IPAddresses:  ipAddresses,
		Organization: "BuildKit Worker",
		Duration:     duration,
		IsServer:     true,
	})
	if err != nil {
		return fmt.Errorf("failed to issue worker server certificate: %w", err)
	}

	if err := r.storeCertificateWithCA(ctx, workerSecretName, namespace, certPEM, keyPEM, "worker-tls", pool); err != nil {
		return err
	}

	r.log.Info("Issued worker server certificate", "secret", workerSecretName, "expires", certInfo.NotAfter.Format(time.RFC3339))
	return nil
}
