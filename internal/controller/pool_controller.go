package controller

import (
	"context"
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/handler"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/certs"
	"github.com/smrt-devops/buildkit-controller/internal/controller/gateway"
	poolmanager "github.com/smrt-devops/buildkit-controller/internal/controller/pool"
	"github.com/smrt-devops/buildkit-controller/internal/controller/shared"
	statusupdater "github.com/smrt-devops/buildkit-controller/internal/controller/status"
	tlsmanager "github.com/smrt-devops/buildkit-controller/internal/controller/tls"
	workermanager "github.com/smrt-devops/buildkit-controller/internal/controller/worker"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

// BuildKitPoolReconciler reconciles a BuildKitPool object.
// It delegates to specialized reconcilers to follow Single Responsibility Principle.
type BuildKitPoolReconciler struct {
	client.Client
	Scheme              *runtime.Scheme
	Log                 utils.Logger
	CertManager         *certs.CertificateManager
	CAManager           *certs.CAManager
	DefaultGatewayImage string
	AllowedIngressTypes []string

	// Domain managers (NOT Kubernetes controllers - helpers used by this controller)
	tlsManager       *tlsmanager.Manager
	configMapManager *poolmanager.Manager
	gatewayManager   *gateway.Manager
	workerManager    *workermanager.Manager
	statusUpdater    *statusupdater.Updater
}

//+kubebuilder:rbac:groups=buildkit.smrt-devops.net,resources=buildkitpools,verbs=get;list;watch;create;update;patch;delete
//+kubebuilder:rbac:groups=buildkit.smrt-devops.net,resources=buildkitpools/status,verbs=get;update;patch
//+kubebuilder:rbac:groups=buildkit.smrt-devops.net,resources=buildkitpools/finalizers,verbs=update
//+kubebuilder:rbac:groups=buildkit.smrt-devops.net,resources=buildkitworkers,verbs=get;list;watch
//+kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;list;watch;create;update;patch;delete
//+kubebuilder:rbac:groups=core,resources=services,verbs=get;list;watch;create;update;patch;delete
//+kubebuilder:rbac:groups=core,resources=secrets,verbs=get;list;watch;create;update;patch;delete
//+kubebuilder:rbac:groups=core,resources=configmaps,verbs=get;list;watch;create;update;patch;delete

// Reconcile is part of the main kubernetes reconciliation loop.
func (r *BuildKitPoolReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("buildkitpool", req.NamespacedName)

	// Fetch the BuildKitPool instance
	pool := &buildkitv1alpha1.BuildKitPool{}
	if err := r.Get(ctx, req.NamespacedName, pool); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	// Set defaults on a copy to avoid modifying the original object
	// This prevents conflicts when updating status
	poolWithDefaults := pool.DeepCopy()
	r.setDefaults(poolWithDefaults)

	// Initialize domain managers if not already done
	r.ensureManagers(log)

	// Ensure CA exists
	if _, err := r.CAManager.EnsureCA(ctx); err != nil {
		return ctrl.Result{}, fmt.Errorf("failed to ensure CA: %w", err)
	}

	// Manage TLS certificates
	if err := r.tlsManager.Reconcile(ctx, poolWithDefaults, req.Namespace); err != nil {
		return ctrl.Result{}, fmt.Errorf("failed to manage TLS: %w", err)
	}

	// Manage ConfigMap for buildkitd.toml
	if err := r.configMapManager.Reconcile(ctx, poolWithDefaults, req.Namespace); err != nil {
		return ctrl.Result{}, fmt.Errorf("failed to manage ConfigMap: %w", err)
	}

	// Manage gateway (creates gateway deployment and service)
	if err := r.gatewayManager.Reconcile(ctx, poolWithDefaults, req.Namespace); err != nil {
		return ctrl.Result{}, fmt.Errorf("failed to manage gateway: %w", err)
	}

	// Manage workers (ensures minimum workers exist)
	if err := r.workerManager.Reconcile(ctx, poolWithDefaults, req.Namespace); err != nil {
		return ctrl.Result{}, fmt.Errorf("failed to manage workers: %w", err)
	}

	// Update status
	if err := r.statusUpdater.Update(ctx, pool, req.Namespace); err != nil {
		return ctrl.Result{}, fmt.Errorf("failed to update status: %w", err)
	}

	// Calculate requeue time based on certificate rotation needs
	requeueAfter := shared.CalculateRequeueInterval(poolWithDefaults)

	return ctrl.Result{RequeueAfter: requeueAfter}, nil
}

// ensureManagers initializes the domain managers if they haven't been created yet.
func (r *BuildKitPoolReconciler) ensureManagers(log utils.Logger) {
	if r.tlsManager == nil {
		r.tlsManager = tlsmanager.NewManager(r.Client, r.Scheme, r.CertManager, r.CAManager, log)
	}
	if r.configMapManager == nil {
		r.configMapManager = poolmanager.NewManager(r.Client, r.Scheme, log)
	}
	if r.gatewayManager == nil {
		r.gatewayManager = gateway.NewManager(r.Client, r.Scheme, log, "", r.DefaultGatewayImage, r.CertManager, r.CAManager, r.AllowedIngressTypes)
	}
	if r.workerManager == nil {
		r.workerManager = workermanager.NewManager(r.Client, r.Scheme, log)
	}
	if r.statusUpdater == nil {
		r.statusUpdater = statusupdater.NewUpdater(r.Client, log)
	}
}

func (r *BuildKitPoolReconciler) setDefaults(pool *buildkitv1alpha1.BuildKitPool) {
	// Set TLS defaults
	if pool.Spec.TLS.Mode == "" {
		pool.Spec.TLS.Mode = buildkitv1alpha1.TLSModeAuto
	}
	if !pool.Spec.TLS.Enabled {
		pool.Spec.TLS.Enabled = true
	}

	// Set networking defaults
	if pool.Spec.Networking.ServiceType == "" {
		pool.Spec.Networking.ServiceType = buildkitv1alpha1.ServiceTypeClusterIP
	}
	if pool.Spec.Networking.Port == nil {
		port := shared.DefaultGatewayPort
		pool.Spec.Networking.Port = &port
	}

	// Set gateway defaults
	// Gateway.Enabled defaults to true per the API definition (+kubebuilder:default=true)
	// Since bool zero value is false, we need to check if gateway config was provided
	// If any gateway field is set (other than Enabled), we assume the user provided gateway config
	// Otherwise, we default Enabled to true
	gatewayConfigProvided := pool.Spec.Gateway.Replicas != nil ||
		pool.Spec.Gateway.Resources != nil ||
		pool.Spec.Gateway.TokenTTL != "" ||
		pool.Spec.Gateway.MaxTokenTTL != "" ||
		pool.Spec.Gateway.ServiceType != "" ||
		pool.Spec.Gateway.Port != nil ||
		pool.Spec.Gateway.NodePort != nil ||
		pool.Spec.Gateway.GatewayAPI != nil ||
		pool.Spec.Gateway.Ingress != nil

	// If no gateway config was provided, default Enabled to true
	// This handles the case where pool is created with empty gateway: {} or no gateway field
	// Note: We can't distinguish between "not set" and "explicitly false" for a bool,
	// but if user sets enabled: false, they likely also set other fields, making gatewayConfigProvided true
	if !gatewayConfigProvided {
		pool.Spec.Gateway.Enabled = true
	}

	if pool.Spec.Gateway.Replicas == nil {
		replicas := int32(1)
		pool.Spec.Gateway.Replicas = &replicas
	}

	// Set image defaults
	if pool.Spec.BuildkitImage == "" {
		pool.Spec.BuildkitImage = "moby/buildkit:master-rootless"
	}
	if pool.Spec.GatewayImage == "" {
		if r.DefaultGatewayImage != "" {
			pool.Spec.GatewayImage = r.DefaultGatewayImage
		} else {
			pool.Spec.GatewayImage = "ghcr.io/smrt-devops/buildkit-controller/gateway:latest"
		}
	}
}

// SetupWithManager sets up the controller with the Manager.
func (r *BuildKitPoolReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&buildkitv1alpha1.BuildKitPool{}).
		Owns(&appsv1.Deployment{}).
		Owns(&corev1.Service{}).
		Owns(&corev1.Secret{}).
		Owns(&corev1.ConfigMap{}).
		Watches(
			&buildkitv1alpha1.BuildKitWorker{},
			handler.EnqueueRequestsFromMapFunc(r.workerToPoolMapper),
		).
		Complete(r)
}

// workerToPoolMapper maps a BuildKitWorker to its parent BuildKitPool for reconciliation.
func (r *BuildKitPoolReconciler) workerToPoolMapper(ctx context.Context, obj client.Object) []reconcile.Request {
	worker, ok := obj.(*buildkitv1alpha1.BuildKitWorker)
	if !ok {
		return nil
	}

	// Get pool name from worker's pool label or spec
	poolName := shared.GetPoolNameFromLabels(worker.Labels)
	if poolName == "" && worker.Spec.PoolRef.Name != "" {
		poolName = worker.Spec.PoolRef.Name
	}

	if poolName == "" {
		return nil
	}

	// Determine namespace
	namespace := worker.Namespace
	if worker.Spec.PoolRef.Namespace != "" {
		namespace = worker.Spec.PoolRef.Namespace
	}

	return []reconcile.Request{
		{
			NamespacedName: types.NamespacedName{
				Name:      poolName,
				Namespace: namespace,
			},
		},
	}
}
