package main

import (
	"flag"
	"os"
	"strings"

	// Import all Kubernetes client auth plugins (e.g. Azure, GCP, OIDC, etc.)
	// to ensure that exec-entrypoint and run can make use of them.
	_ "k8s.io/client-go/plugin/pkg/client/auth"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	"k8s.io/apimachinery/pkg/runtime"
	utilruntime "k8s.io/apimachinery/pkg/util/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/healthz"
	"sigs.k8s.io/controller-runtime/pkg/log/zap"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"
	gatewayapiv1 "sigs.k8s.io/gateway-api/apis/v1"
	gatewayapiv1alpha2 "sigs.k8s.io/gateway-api/apis/v1alpha2"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/api"
	"github.com/smrt-devops/buildkit-controller/internal/certs"
	"github.com/smrt-devops/buildkit-controller/internal/controller"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
	//+kubebuilder:scaffold:imports
)

var (
	setupLog = ctrl.Log.WithName("setup")
)

// buildScheme creates the runtime scheme
// Manually adds only the types we need to avoid including Ingress when not allowed
// clientgoscheme.AddToScheme includes Ingress, so we can't use it when Ingress is not allowed
func buildScheme(ingressAllowed, gatewayAPIAllowed bool) *runtime.Scheme {
	scheme := runtime.NewScheme()

	if ingressAllowed {
		// If Ingress is allowed, use clientgoscheme which includes everything
		utilruntime.Must(clientgoscheme.AddToScheme(scheme))
		setupLog.Info("Added all Kubernetes types to scheme (including Ingress)")
	} else {
		// If Ingress is NOT allowed, manually add only the types we need
		// This prevents controller-runtime from watching Ingress
		utilruntime.Must(corev1.AddToScheme(scheme))
		utilruntime.Must(appsv1.AddToScheme(scheme))
		// Add metav1 types (ObjectMeta, etc.) - these are needed for all resources
		// metav1 is included via corev1, but we need to ensure it's there
		setupLog.Info("Added only required Kubernetes types to scheme (Ingress excluded)")
	}

	// Conditionally add Ingress types only if allowed (redundant if clientgoscheme was used, but safe)
	if ingressAllowed {
		utilruntime.Must(networkingv1.AddToScheme(scheme))
		setupLog.Info("Ingress types explicitly added to scheme")
	}

	// Add our CRDs
	utilruntime.Must(buildkitv1alpha1.AddToScheme(scheme))

	// Conditionally add Gateway API types only if allowed
	if gatewayAPIAllowed {
		utilruntime.Must(gatewayapiv1.Install(scheme))
		utilruntime.Must(gatewayapiv1alpha2.Install(scheme))
		setupLog.Info("Gateway API types added to scheme (gatewayapi is allowed)")
	} else {
		setupLog.Info("Gateway API types NOT added to scheme (gatewayapi is not allowed)")
	}

	//+kubebuilder:scaffold:scheme
	return scheme
}

func main() {
	var metricsAddr string
	var enableLeaderElection bool
	var probeAddr string
	var apiAddr string
	var devMode bool
	flag.StringVar(&metricsAddr, "metrics-bind-address", ":8080", "The address the metric endpoint binds to.")
	flag.StringVar(&probeAddr, "health-probe-bind-address", ":8081", "The address the probe endpoint binds to.")
	flag.StringVar(&apiAddr, "api-bind-address", ":8082", "The address the API server binds to.")
	flag.BoolVar(&enableLeaderElection, "leader-elect", false,
		"Enable leader election for controller manager. "+
			"Enabling this will ensure there is only one active controller manager.")
	flag.BoolVar(&devMode, "dev-mode", false,
		"Enable dev mode (disables API authentication). FOR LOCAL DEVELOPMENT ONLY.")
	// Configure logger - allow flags to override environment variables
	loggerConfig := utils.LoadLoggerConfigFromEnv()
	opts := zap.Options{
		Development: loggerConfig.Development,
	}
	opts.BindFlags(flag.CommandLine)
	flag.Parse()

	// Use flag-based configuration (flags take precedence over env vars)
	ctrl.SetLogger(zap.New(zap.UseFlagOptions(&opts)))

	// Parse allowed ingress types from environment (needed before manager setup)
	// Format: comma-separated list, e.g., "ingress,gatewayapi" or "gatewayapi" or ""
	allowedIngressTypesEnv := os.Getenv("ALLOWED_INGRESS_TYPES")
	var allowedIngressTypes []string
	if allowedIngressTypesEnv != "" {
		types := strings.Split(allowedIngressTypesEnv, ",")
		for _, t := range types {
			trimmed := strings.TrimSpace(t)
			if trimmed != "" {
				allowedIngressTypes = append(allowedIngressTypes, trimmed)
			}
		}
	}
	// If not specified, default to allowing both (backward compatibility)
	// In production, this should be explicitly set via Helm values
	if len(allowedIngressTypes) == 0 {
		setupLog.Info("ALLOWED_INGRESS_TYPES not set, defaulting to allow all ingress types (ingress, gatewayapi)")
		allowedIngressTypes = []string{"ingress", "gatewayapi"}
	} else {
		setupLog.Info("Allowed ingress types", "types", allowedIngressTypes)
	}

	// Check which ingress types are allowed
	ingressAllowed := false
	gatewayAPIAllowed := false
	for _, t := range allowedIngressTypes {
		if strings.EqualFold(t, "ingress") {
			ingressAllowed = true
		}
		if strings.EqualFold(t, "gatewayapi") {
			gatewayAPIAllowed = true
		}
	}

	// Build scheme (only add types that are explicitly allowed)
	scheme := buildScheme(ingressAllowed, gatewayAPIAllowed)

	// Configure manager options
	managerOpts := ctrl.Options{
		Scheme: scheme,
		Metrics: metricsserver.Options{
			BindAddress: metricsAddr,
		},
		HealthProbeBindAddress: probeAddr,
		LeaderElection:         enableLeaderElection,
		LeaderElectionID:       "buildkit-controller.smrt-devops.net",
	}

	// No need to configure cache exclusions - types not in the scheme won't be watched

	mgr, err := ctrl.NewManager(ctrl.GetConfigOrDie(), managerOpts)
	if err != nil {
		setupLog.Error(err, "unable to start manager")
		os.Exit(1)
	}

	// Load certificate configuration
	certConfig := certs.LoadConfig()

	// Initialize CA manager
	caManager := certs.NewCAManager(mgr.GetClient(), "", "", setupLog)

	// Initialize certificate manager with configuration
	certManager := certs.NewCertificateManager(mgr.GetClient(), caManager, setupLog, certConfig)

	// Start API server
	var apiOpts []api.ServerOption
	if devMode {
		apiOpts = append(apiOpts, api.WithDevMode(true))
	}
	apiServer := api.NewServer(mgr.GetClient(), certManager, caManager, setupLog, 8082, certConfig, apiOpts...)
	// Use the manager's context for proper lifecycle management
	managerCtx := ctrl.SetupSignalHandler()
	go func() {
		// Wait for manager to be elected (if leader election enabled)
		if enableLeaderElection {
			<-mgr.Elected()
		}
		if startErr := apiServer.Start(managerCtx); startErr != nil {
			setupLog.Error(startErr, "unable to start API server")
		}
	}()

	// Get default gateway image from environment
	defaultGatewayImage := os.Getenv("GATEWAY_IMAGE")
	if defaultGatewayImage == "" {
		defaultGatewayImage = "ghcr.io/smrt-devops/buildkit-controller/gateway:latest"
	}

	// Register BuildKitPool controller
	if err = (&controller.BuildKitPoolReconciler{
		Client:              mgr.GetClient(),
		Scheme:              mgr.GetScheme(),
		Log:                 ctrl.Log.WithName("controller").WithName("BuildKitPool"),
		CertManager:         certManager,
		CAManager:           caManager,
		DefaultGatewayImage: defaultGatewayImage,
		AllowedIngressTypes: allowedIngressTypes,
	}).SetupWithManager(mgr); err != nil {
		setupLog.Error(err, "unable to create controller", "controller", "BuildKitPool")
		os.Exit(1)
	}

	// Register BuildKitWorker controller
	if err = (&controller.BuildKitWorkerReconciler{
		Client: mgr.GetClient(),
		Scheme: mgr.GetScheme(),
		Log:    ctrl.Log.WithName("controller").WithName("BuildKitWorker"),
	}).SetupWithManager(mgr); err != nil {
		setupLog.Error(err, "unable to create controller", "controller", "BuildKitWorker")
		os.Exit(1)
	}
	//+kubebuilder:scaffold:builder

	if err := mgr.AddHealthzCheck("healthz", healthz.Ping); err != nil {
		setupLog.Error(err, "unable to set up health check")
		os.Exit(1)
	}
	if err := mgr.AddReadyzCheck("readyz", healthz.Ping); err != nil {
		setupLog.Error(err, "unable to set up ready check")
		os.Exit(1)
	}

	setupLog.Info("starting manager")
	if err := mgr.Start(managerCtx); err != nil {
		setupLog.Error(err, "problem running manager")
		os.Exit(1)
	}
}
