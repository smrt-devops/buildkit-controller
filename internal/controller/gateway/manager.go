package gateway

import (
	"context"
	"fmt"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"
	gatewayapiv1 "sigs.k8s.io/gateway-api/apis/v1"
	gatewayapiv1alpha2 "sigs.k8s.io/gateway-api/apis/v1alpha2"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/certs"
	"github.com/smrt-devops/buildkit-controller/internal/controller/shared"
	tlshelpers "github.com/smrt-devops/buildkit-controller/internal/controller/tls"
	"github.com/smrt-devops/buildkit-controller/internal/resources"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

type Manager struct {
	client              client.Client
	scheme              *runtime.Scheme
	log                 utils.Logger
	controllerEndpoint  string
	defaultGatewayImage string
	certManager         *certs.CertificateManager
	caManager           *certs.CAManager
	allowedIngressTypes map[string]bool // Set of allowed ingress types: "ingress", "gatewayapi"
}

func NewManager(k8sClient client.Client, scheme *runtime.Scheme, log utils.Logger, controllerEndpoint, defaultGatewayImage string, certManager *certs.CertificateManager, caManager *certs.CAManager, allowedIngressTypes []string) *Manager {
	if controllerEndpoint == "" {
		controllerEndpoint = "http://buildkit-controller-api.buildkit-system.svc:8082"
	}

	allowedMap := make(map[string]bool)
	for _, t := range allowedIngressTypes {
		allowedMap[t] = true
	}

	return &Manager{
		client:              k8sClient,
		scheme:              scheme,
		log:                 log,
		controllerEndpoint:  controllerEndpoint,
		defaultGatewayImage: defaultGatewayImage,
		certManager:         certManager,
		caManager:           caManager,
		allowedIngressTypes: allowedMap,
	}
}

func (r *Manager) Reconcile(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	return r.reconcileGateway(ctx, pool, namespace)
}

func (r *Manager) reconcileGateway(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	if !pool.Spec.Gateway.Enabled {
		r.log.V(1).Info("Gateway disabled for pool", "pool", pool.Name)
		return nil
	}

	deployment, service, err := r.reconcileGatewayDeploymentAndService(ctx, pool, namespace)
	if err != nil {
		return err
	}

	if err := r.reconcileGatewayAPIResources(ctx, pool, namespace); err != nil {
		return err
	}

	if err := r.reconcileIngressResources(ctx, pool); err != nil {
		return err
	}

	pool.Status.Gateway = &buildkitv1alpha1.GatewayStatus{
		DeploymentName: deployment.Name,
		ServiceName:    service.Name,
	}

	return nil
}

func (r *Manager) needsGatewayAPICertificate(ctx context.Context, secretName, secretNamespace string) bool {
	secret := &corev1.Secret{}
	secretKey := types.NamespacedName{Name: secretName, Namespace: secretNamespace}
	if r.client.Get(ctx, secretKey, secret) != nil {
		return true
	}

	certInfo, err := r.certManager.ParseCertificateFromSecret(ctx, secretName, secretNamespace, "tls.crt")
	if err != nil || certInfo == nil {
		return true
	}

	renewBefore := shared.CertRenewalCheckWindow
	return r.certManager.ShouldRotateCertificate(certInfo, renewBefore)
}

func (r *Manager) issueAndStoreGatewayAPICertificate(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, secretName, secretNamespace, poolNamespace string) (*certs.CertificateInfo, error) {
	duration := tlshelpers.GetCertDuration(pool)
	certPEM, keyPEM, certInfo, err := r.certManager.IssueCertificate(ctx, &certs.CertificateRequest{
		CommonName:   pool.Spec.Gateway.GatewayAPI.Hostname,
		DNSNames:     []string{pool.Spec.Gateway.GatewayAPI.Hostname},
		Organization: "BuildKit Gateway API",
		Duration:     duration,
		IsServer:     true,
	})
	if err != nil {
		return nil, fmt.Errorf("failed to issue Gateway API certificate: %w", err)
	}

	caCertPEM, err := r.caManager.GetCACertPEM(ctx)
	if err != nil {
		return nil, fmt.Errorf("failed to get CA cert: %w", err)
	}

	labels := utils.DefaultLabels("gateway-api-tls", pool.Name)
	if err := r.certManager.StoreCertificate(ctx, secretName, secretNamespace, certPEM, keyPEM, caCertPEM, labels); err != nil {
		return nil, fmt.Errorf("failed to store Gateway API certificate: %w", err)
	}

	if secretNamespace == poolNamespace {
		secret := &corev1.Secret{}
		secretKey := types.NamespacedName{Name: secretName, Namespace: secretNamespace}
		if err := r.client.Get(ctx, secretKey, secret); err == nil {
			if err := utils.SetOwnerReference(ctx, r.client, pool, secret, r.scheme); err != nil {
				r.log.V(1).Info("Failed to set owner reference on Gateway API TLS secret", "error", err)
			}
		}
	}

	return certInfo, nil
}

func (r *Manager) reconcileResource(ctx context.Context, obj client.Object, resourceType string, pool *buildkitv1alpha1.BuildKitPool) error {
	if obj == nil {
		return nil
	}

	if err := utils.SetControllerReference(pool, obj, r.scheme); err != nil {
		return fmt.Errorf("failed to set owner reference on %s: %w", resourceType, err)
	}

	if err := utils.CreateOrUpdate(ctx, r.client, obj); err != nil {
		return fmt.Errorf("failed to create or update %s: %w", resourceType, err)
	}

	r.log.Info("Reconciled "+resourceType, "name", obj.GetName(), "pool", pool.Name)
	return nil
}

func (r *Manager) cleanupResource(ctx context.Context, obj client.Object, resourceType, resourceName string, namespace string) {
	if err := r.client.Get(ctx, types.NamespacedName{Name: resourceName, Namespace: namespace}, obj); err == nil {
		if err := r.client.Delete(ctx, obj); err != nil {
			r.log.V(1).Info("Failed to delete "+resourceType+" (may not exist)", resourceName, resourceName, "error", err)
		}
	}
}

func (r *Manager) reconcileGatewayDeploymentAndService(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) (*appsv1.Deployment, *corev1.Service, error) {
	gatewayImage := resources.GetGatewayImage(pool, r.defaultGatewayImage)

	deployment := resources.NewGatewayDeployment(pool, gatewayImage, r.controllerEndpoint)
	if err := r.reconcileResource(ctx, deployment, "gateway deployment", pool); err != nil {
		return nil, nil, err
	}

	service := resources.NewGatewayService(pool)
	if err := r.reconcileResource(ctx, service, "gateway service", pool); err != nil {
		return nil, nil, err
	}

	return deployment, service, nil
}

func (r *Manager) reconcileGatewayAPIResources(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	if pool.Spec.Gateway.GatewayAPI != nil && pool.Spec.Gateway.GatewayAPI.Enabled {
		return r.reconcileGatewayAPI(ctx, pool, namespace)
	}
	return r.cleanupGatewayAPI(ctx, pool)
}

func (r *Manager) reconcileGatewayAPI(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	if !r.allowedIngressTypes["gatewayapi"] {
		return fmt.Errorf("gateway API is not enabled for this controller. Pool '%s' requested Gateway API, but only the following ingress types are allowed: %v. Please update the Helm values to include 'gatewayapi' in ingress.types, or change the pool to use a different ingress type", pool.Name, r.getAllowedIngressTypesList())
	}

	if err := r.reconcileGatewayAPITLS(ctx, pool, namespace); err != nil {
		return fmt.Errorf("failed to reconcile Gateway API TLS: %w", err)
	}

	if err := r.reconcileGatewayAPIGateway(ctx, pool); err != nil {
		return err
	}

	if err := r.reconcileGatewayAPITLSRoute(ctx, pool); err != nil {
		return err
	}

	return nil
}

func (r *Manager) reconcileGatewayAPIGateway(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	gateway := resources.NewGatewayAPIGateway(pool)
	return r.reconcileResource(ctx, gateway, "Gateway API Gateway", pool)
}

func (r *Manager) reconcileGatewayAPITLSRoute(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	tlsConfig := pool.Spec.Gateway.GatewayAPI.TLS
	tlsMode := "passthrough"
	if tlsConfig != nil && tlsConfig.Mode != "" {
		tlsMode = tlsConfig.Mode
	}

	if tlsMode != "passthrough" {
		return fmt.Errorf("invalid TLS mode '%s' for Gateway API: only 'passthrough' is supported for BuildKit pools. TLS is mandatory and passthrough is required for client certificate validation", tlsMode)
	}

	tlsRoute := resources.NewGatewayAPITLSRoute(pool)
	if err := r.reconcileResource(ctx, tlsRoute, "Gateway API TLSRoute", pool); err != nil {
		return err
	}

	return r.cleanupOldTCPRoute(ctx, pool)
}

func (r *Manager) cleanupOldTCPRoute(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	tcpRouteName := shared.GenerateResourceName(pool.Name, "tcproute")
	tcpRoute := &gatewayapiv1alpha2.TCPRoute{}
	if err := r.client.Get(ctx, types.NamespacedName{Name: tcpRouteName, Namespace: pool.Namespace}, tcpRoute); err == nil {
		if err := r.client.Delete(ctx, tcpRoute); err == nil {
			r.log.Info("Deleted old Gateway API TCPRoute (TCPRoute is not supported for Gateway API)", "tcproute", tcpRouteName)
		}
	}
	return nil
}

func (r *Manager) cleanupGatewayAPI(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	gatewayName := shared.GenerateResourceName(pool.Name, "gateway")
	if pool.Spec.Gateway.GatewayAPI != nil && pool.Spec.Gateway.GatewayAPI.GatewayName != "" {
		gatewayName = pool.Spec.Gateway.GatewayAPI.GatewayName
	}

	r.cleanupResource(ctx, &gatewayapiv1.Gateway{}, "Gateway API Gateway", gatewayName, pool.Namespace)
	r.cleanupResource(ctx, &gatewayapiv1alpha2.TCPRoute{}, "Gateway API TCPRoute", shared.GenerateResourceName(pool.Name, "tcproute"), pool.Namespace)
	r.cleanupResource(ctx, &gatewayapiv1alpha2.TLSRoute{}, "Gateway API TLSRoute", shared.GenerateResourceName(pool.Name, "tlsroute"), pool.Namespace)

	return nil
}

func (r *Manager) reconcileIngressResources(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	if pool.Spec.Gateway.Ingress != nil && pool.Spec.Gateway.Ingress.Enabled {
		return r.reconcileIngress(ctx, pool)
	}
	return r.cleanupIngress(ctx, pool)
}

func (r *Manager) reconcileIngress(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	if !r.allowedIngressTypes["ingress"] {
		return fmt.Errorf("kubernetes Ingress is not enabled for this controller. Pool '%s' requested Ingress, but only the following ingress types are allowed: %v. Please update the Helm values to include 'ingress' in ingress.types, or change the pool to use a different ingress type", pool.Name, r.getAllowedIngressTypesList())
	}

	ingress := resources.NewIngress(pool)
	return r.reconcileResource(ctx, ingress, "Ingress", pool)
}

func (r *Manager) cleanupIngress(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	ingressName := shared.GenerateResourceName(pool.Name, "ingress")
	r.cleanupResource(ctx, &networkingv1.Ingress{}, "Ingress", ingressName, pool.Namespace)
	return nil
}

func (r *Manager) getAllowedIngressTypesList() []string {
	if len(r.allowedIngressTypes) == 0 {
		return []string{"none (internal cluster exposure only)"}
	}
	types := make([]string, 0, len(r.allowedIngressTypes))
	for t := range r.allowedIngressTypes {
		types = append(types, t)
	}
	return types
}

func (r *Manager) reconcileGatewayAPITLS(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	if pool.Spec.Gateway.GatewayAPI == nil || !pool.Spec.Gateway.GatewayAPI.Enabled {
		return nil
	}

	if pool.Spec.Gateway.GatewayAPI.Hostname == "" {
		return fmt.Errorf("hostname is required when Gateway API is enabled")
	}

	tlsConfig := pool.Spec.Gateway.GatewayAPI.TLS
	if tlsConfig == nil {
		tlsConfig = &buildkitv1alpha1.GatewayAPITLSConfig{
			Mode:         "passthrough",
			AutoGenerate: true,
		}
	}

	if tlsConfig.Mode == "passthrough" {
		return nil
	}

	if tlsConfig.AutoGenerate {
		secretName := tlsConfig.SecretName
		if secretName == "" {
			secretName = shared.GenerateResourceName(pool.Name, "gateway-api-tls")
		}
		secretNamespace := tlsConfig.SecretNamespace
		if secretNamespace == "" {
			secretNamespace = namespace
		}

		if r.needsGatewayAPICertificate(ctx, secretName, secretNamespace) {
			certInfo, err := r.issueAndStoreGatewayAPICertificate(ctx, pool, secretName, secretNamespace, namespace)
			if err != nil {
				return err
			}

			r.log.Info("Generated Gateway API TLS certificate",
				"secret", secretName,
				"namespace", secretNamespace,
				"hostname", pool.Spec.Gateway.GatewayAPI.Hostname,
				"expires", certInfo.NotAfter.Format(time.RFC3339))
		}

		if pool.Spec.Gateway.GatewayAPI.TLS == nil {
			pool.Spec.Gateway.GatewayAPI.TLS = tlsConfig
		}
		if pool.Spec.Gateway.GatewayAPI.TLS.SecretName == "" {
			pool.Spec.Gateway.GatewayAPI.TLS.SecretName = secretName
		}
		if pool.Spec.Gateway.GatewayAPI.TLS.SecretNamespace == "" {
			pool.Spec.Gateway.GatewayAPI.TLS.SecretNamespace = secretNamespace
		}
	}

	return nil
}
