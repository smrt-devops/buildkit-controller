package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// ScalingMode defines how the pool should be scaled
// +kubebuilder:validation:Enum=auto;manual;dynamic
type ScalingMode string

const (
	ScalingModeAuto    ScalingMode = "auto"
	ScalingModeManual  ScalingMode = "manual"
	ScalingModeDynamic ScalingMode = "dynamic"
)

// TLSMode defines how TLS certificates are managed
// +kubebuilder:validation:Enum=auto;manual
type TLSMode string

const (
	TLSModeAuto   TLSMode = "auto"
	TLSModeManual TLSMode = "manual"
)

// AuthMethodType defines the type of authentication method
// +kubebuilder:validation:Enum=mtls;token;oidc
type AuthMethodType string

const (
	AuthMethodMTLS  AuthMethodType = "mtls"
	AuthMethodToken AuthMethodType = "token"
	AuthMethodOIDC  AuthMethodType = "oidc"
)

// ServiceType defines the Kubernetes service type
// +kubebuilder:validation:Enum=ClusterIP;LoadBalancer;NodePort
type ServiceType string

const (
	ServiceTypeClusterIP    ServiceType = "ClusterIP"
	ServiceTypeLoadBalancer ServiceType = "LoadBalancer"
	ServiceTypeNodePort     ServiceType = "NodePort"
)

// BuildKitPoolSpec defines the desired state of BuildKitPool.
type BuildKitPoolSpec struct {
	// Scaling behavior
	Scaling ScalingConfig `json:"scaling"`

	// Resource allocation
	Resources ResourceConfig `json:"resources"`

	// BuildKit configuration (standard buildkitd.toml)
	// If not provided, defaults will be generated
	BuildkitConfig string `json:"buildkitConfig,omitempty"`

	// Cache configuration
	Cache CacheConfig `json:"cache"`

	// TLS configuration
	TLS TLSConfig `json:"tls"`

	// Authentication
	Auth AuthConfig `json:"auth"`

	// Networking & Exposure
	Networking NetworkingConfig `json:"networking"`

	// Observability
	Observability ObservabilityConfig `json:"observability"`

	// Gateway configuration for the pool gateway
	// +optional
	Gateway GatewayConfig `json:"gateway,omitempty"`

	// BuildkitImage is the buildkit daemon image to use
	// Defaults to moby/buildkit:master-rootless
	BuildkitImage string `json:"buildkitImage,omitempty"`

	// GatewayImage is the gateway image to use
	// Defaults to ghcr.io/smrt-devops/buildkit-controller/gateway:latest
	GatewayImage string `json:"gatewayImage,omitempty"`
}

// GatewayConfig defines the pool gateway configuration.
type GatewayConfig struct {
	// Enabled enables the gateway (defaults to true)
	// +kubebuilder:default=true
	Enabled bool `json:"enabled,omitempty"`

	// Replicas is the number of gateway replicas for HA
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=1
	Replicas *int32 `json:"replicas,omitempty"`

	// Resources for the gateway pods
	// +optional
	Resources *GatewayResources `json:"resources,omitempty"`

	// TokenTTL is the default TTL for allocation tokens
	// Defaults to 1h
	TokenTTL string `json:"tokenTTL,omitempty"`

	// MaxTokenTTL is the maximum allowed TTL for allocation tokens
	// Defaults to 24h
	MaxTokenTTL string `json:"maxTokenTTL,omitempty"`

	// ServiceType is the Kubernetes service type for the gateway
	// Defaults to ClusterIP (suitable for Istio/Envoy sidecars, ingress controllers, etc.)
	// Can be set to LoadBalancer for direct external access, or NodePort for node-based access
	// +kubebuilder:default=ClusterIP
	// +optional
	ServiceType ServiceType `json:"serviceType,omitempty"`

	// Port overrides the service port for the gateway
	// Defaults to 1235
	// +optional
	Port *int32 `json:"port,omitempty"`

	// NodePort is the node port for NodePort service type
	// Only used when serviceType is NodePort
	// +optional
	// +kubebuilder:validation:Minimum=30000
	// +kubebuilder:validation:Maximum=32767
	NodePort *int32 `json:"nodePort,omitempty"`

	// LoadBalancerClass is the load balancer class for LoadBalancer service type
	// Only used when serviceType is LoadBalancer
	// +optional
	LoadBalancerClass *string `json:"loadBalancerClass,omitempty"`

	// GatewayAPI configuration for external access via Kubernetes Gateway API
	// +optional
	GatewayAPI *GatewayAPIConfig `json:"gatewayAPI,omitempty"`

	// Ingress configuration for external access via Kubernetes Ingress
	// +optional
	Ingress *IngressConfig `json:"ingress,omitempty"`
}

// GatewayResources defines resource limits for gateway pods.
type GatewayResources struct {
	// CPU limit
	CPU string `json:"cpu,omitempty"`

	// Memory limit
	Memory string `json:"memory,omitempty"`
}

// GatewayAPIConfig defines Gateway API configuration for external access.
// The controller can either create Gateway resources automatically or reference existing ones.
//
// When GatewayRef is specified, the controller will not create Gateway resources
// and will only create route resources (TLSRoute/TCPRoute) that reference the existing Gateway.
// This allows customers to use their own Gateway configurations.
//
// Example with GatewayRef:
//
//	gatewayAPI:
//	  enabled: true
//	  hostname: "buildkit.example.com"
//	  gatewayRef:
//	    name: "my-existing-gateway"
//	    namespace: "gateway-system"
//
// Example without GatewayRef (controller creates Gateway):
//
//	gatewayAPI:
//	  enabled: true
//	  hostname: "buildkit.example.com"
//	  gatewayClassName: "envoy"
//	  gatewayName: "my-pool-gateway"
type GatewayAPIConfig struct {
	// Enabled enables Gateway API resources for external access
	Enabled bool `json:"enabled,omitempty"`

	// GatewayRef references an existing Gateway resource to use
	// If specified, the controller will not create a Gateway resource
	// and will use the referenced Gateway for routing. This allows
	// customers to use their own Gateway configurations.
	//
	// When GatewayRef is specified, GatewayClassName, GatewayName,
	// and Annotations are ignored since the Gateway already exists.
	//
	// Example:
	//   gatewayRef:
	//     name: "my-gateway"
	//     namespace: "gateway-system"
	// +optional
	GatewayRef *GatewayAPIRef `json:"gatewayRef,omitempty"`

	// GatewayClassName is the GatewayClass name to use
	// If not specified, defaults to "envoy" (Envoy Gateway)
	// Common values: "envoy" (Envoy Gateway), "contour", "kong", "istio"
	// Only used when GatewayRef is not specified
	// +optional
	GatewayClassName string `json:"gatewayClassName,omitempty"`

	// GatewayName is the name of the Gateway resource to create
	// If not specified, defaults to {pool-name}-gateway
	// Only used when GatewayRef is not specified
	// +optional
	GatewayName string `json:"gatewayName,omitempty"`

	// Hostname is the hostname for the Gateway
	// Required when enabled - used for TLS certificate generation
	// +optional
	Hostname string `json:"hostname,omitempty"`

	// TLS configuration for Gateway API
	// TLS is mandatory when Gateway API is enabled
	// If not specified, TLS secret will be auto-generated from the hostname
	// +optional
	TLS *GatewayAPITLSConfig `json:"tls,omitempty"`

	// Annotations to add to Gateway resources
	// Only used when GatewayRef is not specified
	// +optional
	Annotations map[string]string `json:"annotations,omitempty"`
}

// GatewayAPITLSConfig defines TLS configuration for Gateway API.
type GatewayAPITLSConfig struct {
	// Mode is the TLS mode (terminate, passthrough)
	// Defaults to "passthrough" - required because pool gateway needs to validate client certificates
	// With passthrough, TLS is terminated at the pool gateway, which can extract allocation tokens
	// from client certificates. The pool gateway's certificate (which includes the Gateway API
	// hostname) will be presented to clients.
	// +kubebuilder:validation:Enum=terminate;passthrough
	// +kubebuilder:default=passthrough
	Mode string `json:"mode,omitempty"`

	// SecretName is the name of the TLS secret for terminate mode
	// If not specified, will be auto-generated as {pool-name}-gateway-api-tls
	// The secret will be created in the same namespace as the pool
	// +optional
	SecretName string `json:"secretName,omitempty"`

	// SecretNamespace is the namespace for the TLS secret
	// If not specified, uses the pool's namespace
	// +optional
	SecretNamespace string `json:"secretNamespace,omitempty"`

	// AutoGenerate enables automatic TLS secret generation
	// When true (default), the controller will generate a TLS secret
	// with a certificate valid for the Gateway API hostname
	// +kubebuilder:default=true
	AutoGenerate bool `json:"autoGenerate,omitempty"`
}

// GatewayAPIRef defines a reference to an existing Gateway resource.
type GatewayAPIRef struct {
	// Name is the name of the existing Gateway resource
	// +optional
	Name string `json:"name,omitempty"`

	// Namespace is the namespace of the Gateway resource
	// If not specified, uses the pool's namespace
	// +optional
	Namespace string `json:"namespace,omitempty"`
}

// IngressConfig defines Ingress configuration for external access.
type IngressConfig struct {
	// Enabled enables Ingress resources for external access
	Enabled bool `json:"enabled,omitempty"`

	// IngressClassName is the IngressClass name to use
	// If not specified, uses the cluster's default IngressClass
	// +optional
	IngressClassName string `json:"ingressClassName,omitempty"`

	// Hostname is the hostname for the Ingress
	// Required when enabled
	// +optional
	Hostname string `json:"hostname,omitempty"`

	// TLS configuration for Ingress
	// +optional
	TLS *IngressTLSConfig `json:"tls,omitempty"`

	// Annotations to add to Ingress resources
	// +optional
	Annotations map[string]string `json:"annotations,omitempty"`
}

// IngressTLSConfig defines TLS configuration for Ingress.
type IngressTLSConfig struct {
	// Enabled enables TLS
	Enabled bool `json:"enabled,omitempty"`

	// SecretName is the name of the TLS secret
	// Required when enabled
	// +optional
	SecretName string `json:"secretName,omitempty"`
}

// ScalingConfig defines scaling behavior.
type ScalingConfig struct {
	// Mode is the scaling mode (auto, manual, dynamic)
	// +kubebuilder:default=auto
	Mode ScalingMode `json:"mode,omitempty"`

	// Min is the minimum number of idle/available workers to maintain in the pool
	// Similar to GitHub Actions ARC: this is the number of available workers that should always be ready.
	// When workers are allocated, the desired total becomes min + allocated workers.
	// Example: if min=4 and 1 worker is allocated, desired=5 (4 idle + 1 allocated).
	// Set to 0 for scale-to-zero (no idle workers maintained).
	// +kubebuilder:default=0
	// +kubebuilder:validation:Minimum=0
	Min *int32 `json:"min,omitempty"`

	// Max is the maximum number of replicas
	// +kubebuilder:default=10
	// +kubebuilder:validation:Minimum=1
	Max *int32 `json:"max,omitempty"`

	// ScaleDownDelay is the delay before scaling down when idle
	// Defaults to 15m
	ScaleDownDelay string `json:"scaleDownDelay,omitempty"`

	// TargetCPUUtilization is the target CPU utilization percentage for HPA
	// +kubebuilder:default=70
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=100
	TargetCPUUtilization *int32 `json:"targetCPUUtilization,omitempty"`

	// TargetMemoryUtilization is the target memory utilization percentage for HPA
	// +kubebuilder:default=80
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=100
	TargetMemoryUtilization *int32 `json:"targetMemoryUtilization,omitempty"`

	// TargetActiveConnections is the target number of active connections per pod before scaling up
	// +kubebuilder:default=10
	// +kubebuilder:validation:Minimum=1
	TargetActiveConnections *int32 `json:"targetActiveConnections,omitempty"`

	// ScaleDownSchedule is a cron expression that defines when to scale the pool to zero
	// even if min > 0. This allows pools to scale down during off-hours (e.g., nights/weekends).
	// When the current time matches the cron schedule, the pool will scale to 0 workers
	// regardless of the min setting. The cron expression uses standard 5-field format:
	// "minute hour day-of-month month day-of-week"
	// Examples:
	//   "0 0 * * *" - Every day at midnight
	//   "0 18 * * 1-5" - Every weekday at 6 PM
	//   "0 0 * * 0,6" - Every Saturday and Sunday at midnight
	// If not specified, the pool will always respect the min setting.
	// +optional
	ScaleDownSchedule string `json:"scaleDownSchedule,omitempty"`
}

// ResourceConfig defines resource allocation.
type ResourceConfig struct {
	// Buildkit resources for the buildkitd container
	Buildkit corev1.ResourceRequirements `json:"buildkit"`

	// Sidecar resources for the auth-proxy container
	Sidecar corev1.ResourceRequirements `json:"sidecar"`
}

// CacheConfig defines cache backend configuration.
type CacheConfig struct {
	// Backends is a list of cache backends
	Backends []CacheBackend `json:"backends,omitempty"`

	// GC is garbage collection configuration
	GC GarbageCollectionConfig `json:"gc"`
}

// CacheBackend defines a cache backend.
type CacheBackend struct {
	// Type is the cache backend type (registry, s3, local)
	// +kubebuilder:validation:Enum=registry;s3;local
	Type string `json:"type"`

	// Registry configuration (when type is registry)
	Registry *RegistryCacheConfig `json:"registry,omitempty"`

	// S3 configuration (when type is s3)
	S3 *S3CacheConfig `json:"s3,omitempty"`

	// Local configuration (when type is local)
	Local *LocalCacheConfig `json:"local,omitempty"`
}

// RegistryCacheConfig defines registry cache configuration.
type RegistryCacheConfig struct {
	// Endpoint is the registry endpoint
	Endpoint string `json:"endpoint"`

	// Mode is the cache mode (min, max)
	// +kubebuilder:default=max
	Mode string `json:"mode,omitempty"`

	// Compression is the compression algorithm (zstd, gzip)
	// +kubebuilder:default=zstd
	Compression string `json:"compression,omitempty"`

	// Insecure allows insecure registry connections
	Insecure bool `json:"insecure,omitempty"`

	// CredentialsSecret is the name of the secret containing registry credentials
	CredentialsSecret string `json:"credentialsSecret,omitempty"`
}

// S3CacheConfig defines S3 cache configuration.
type S3CacheConfig struct {
	// Bucket is the S3 bucket name
	Bucket string `json:"bucket"`

	// Region is the AWS region
	Region string `json:"region"`

	// Endpoint is the S3 endpoint (defaults to s3.amazonaws.com)
	Endpoint string `json:"endpoint,omitempty"`

	// CredentialsSecret is the name of the secret containing AWS credentials
	CredentialsSecret string `json:"credentialsSecret,omitempty"`
}

// LocalCacheConfig defines local cache configuration.
type LocalCacheConfig struct {
	// StorageClass is the storage class for the PVC
	StorageClass string `json:"storageClass"`

	// Size is the size of the cache volume
	Size string `json:"size"`
}

// GarbageCollectionConfig defines garbage collection configuration.
type GarbageCollectionConfig struct {
	// Enabled enables garbage collection
	// +kubebuilder:default=true
	Enabled bool `json:"enabled,omitempty"`

	// Schedule is the cron schedule for GC (defaults to daily at 2 AM: "0 2 * * *")
	Schedule string `json:"schedule,omitempty"`

	// KeepStorage is the amount of storage to keep
	KeepStorage string `json:"keepStorage,omitempty"`

	// KeepDuration is the duration to keep cache entries
	KeepDuration string `json:"keepDuration,omitempty"`
}

// TLSConfig defines TLS configuration.
type TLSConfig struct {
	// Enabled enables TLS
	// +kubebuilder:default=true
	Enabled bool `json:"enabled,omitempty"`

	// Mode is the TLS mode (auto, manual)
	// +kubebuilder:default=auto
	Mode TLSMode `json:"mode,omitempty"`

	// Auto configuration (when mode is auto)
	Auto *TLSAutoConfig `json:"auto,omitempty"`

	// Manual configuration (when mode is manual)
	Manual *TLSManualConfig `json:"manual,omitempty"`
}

// TLSAutoConfig defines automatic TLS configuration.
type TLSAutoConfig struct {
	// ServerCertDuration is the duration for server certificates
	// Defaults to 8760h (1 year)
	ServerCertDuration string `json:"serverCertDuration,omitempty"`

	// RotateBeforeExpiry is when to rotate certificates before expiry
	// Defaults to 720h (30 days)
	RotateBeforeExpiry string `json:"rotateBeforeExpiry,omitempty"`

	// Organization is the organization name for certificates
	Organization string `json:"organization,omitempty"`
}

// TLSManualConfig defines manual TLS configuration.
type TLSManualConfig struct {
	// ServerCertSecret is the name of the secret containing server certificate
	ServerCertSecret string `json:"serverCertSecret"`

	// CASecret is the name of the secret containing CA certificate
	CASecret string `json:"caSecret"`
}

// AuthConfig defines authentication configuration.
type AuthConfig struct {
	// Methods is a list of authentication methods
	Methods []AuthMethod `json:"methods,omitempty"`

	// RBAC is RBAC configuration
	RBAC *RBACConfig `json:"rbac,omitempty"`
}

// AuthMethod defines an authentication method.
type AuthMethod struct {
	// Type is the auth method type (mtls, token, oidc)
	Type AuthMethodType `json:"type"`

	// MTLS configuration (when type is mtls)
	MTLS *MTLSConfig `json:"mtls,omitempty"`

	// Token configuration (when type is token)
	Token *TokenConfig `json:"token,omitempty"`

	// OIDC configuration (when type is oidc)
	OIDC *OIDCConfig `json:"oidc,omitempty"`
}

// MTLSConfig defines mTLS configuration.
type MTLSConfig struct {
	// Required requires mTLS (client certificates)
	// +kubebuilder:default=true
	Required bool `json:"required,omitempty"`

	// ClientCASecret is the name of the secret containing client CA certificate
	ClientCASecret string `json:"clientCASecret,omitempty"`
}

// TokenConfig defines token-based authentication.
type TokenConfig struct {
	// SecretRef is the name of the secret containing tokens
	// Format: key = token, value = JSON with user and pools
	SecretRef string `json:"secretRef"`
}

// OIDCConfig defines OIDC authentication.
type OIDCConfig struct {
	// Issuer is the OIDC issuer URL
	Issuer string `json:"issuer"`

	// Audience is the expected audience
	Audience string `json:"audience"`

	// ClaimsMapping maps OIDC claims to user/pool information
	ClaimsMapping ClaimsMapping `json:"claimsMapping"`
}

// ClaimsMapping maps OIDC claims.
type ClaimsMapping struct {
	// User is the claim name for user identity
	User string `json:"user,omitempty"`

	// Pools is the claim name for pool access list
	Pools string `json:"pools,omitempty"`
}

// RBACConfig defines RBAC configuration.
type RBACConfig struct {
	// Enabled enables RBAC
	// +kubebuilder:default=true
	Enabled bool `json:"enabled,omitempty"`

	// Rules are RBAC rules
	Rules []RBACRule `json:"rules,omitempty"`
}

// RBACRule defines an RBAC rule.
type RBACRule struct {
	// Users is a list of user patterns (supports wildcards)
	Users []string `json:"users"`

	// Pools is a list of pool names (supports wildcards)
	Pools []string `json:"pools"`
}

// NetworkingConfig defines networking configuration.
type NetworkingConfig struct {
	// ServiceType is the Kubernetes service type
	// +kubebuilder:default=ClusterIP
	ServiceType ServiceType `json:"serviceType,omitempty"`

	// External configuration for external access
	External *ExternalConfig `json:"external,omitempty"`

	// Port is the service port (defaults to 1235 for sidecar)
	// +kubebuilder:default=1235
	Port *int32 `json:"port,omitempty"`

	// AllowedCIDRs is a list of allowed CIDR blocks
	AllowedCIDRs []string `json:"allowedCIDRs,omitempty"`

	// Annotations are annotations to add to the service
	Annotations map[string]string `json:"annotations,omitempty"`
}

// ExternalConfig defines external access configuration.
type ExternalConfig struct {
	// Enabled enables external access
	Enabled bool `json:"enabled,omitempty"`

	// Hostname is the external hostname
	Hostname string `json:"hostname,omitempty"`

	// Annotations are annotations for external-dns, load balancer, etc.
	Annotations map[string]string `json:"annotations,omitempty"`
}

// ObservabilityConfig defines observability configuration.
type ObservabilityConfig struct {
	// Metrics configuration
	Metrics MetricsConfig `json:"metrics"`

	// Logging configuration
	Logging LoggingConfig `json:"logging"`
}

// MetricsConfig defines metrics configuration.
type MetricsConfig struct {
	// Enabled enables metrics
	// +kubebuilder:default=true
	Enabled bool `json:"enabled,omitempty"`

	// Port is the metrics port
	// +kubebuilder:default=9090
	Port *int32 `json:"port,omitempty"`
}

// LoggingConfig defines logging configuration.
type LoggingConfig struct {
	// Level is the log level (debug, info, warn, error)
	// +kubebuilder:default=info
	Level string `json:"level,omitempty"`

	// Format is the log format (json, text)
	// +kubebuilder:default=json
	Format string `json:"format,omitempty"`
}

// BuildKitPoolStatus defines the observed state of BuildKitPool.
type BuildKitPoolStatus struct {
	// Conditions represent the latest available observations of the pool's state
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// Endpoint is the gateway endpoint for client connections
	Endpoint string `json:"endpoint,omitempty"`

	// Phase is the current phase (Pending, Running, ScaledToZero, Failed)
	// +kubebuilder:validation:Enum=Pending;Running;ScaledToZero;Failed
	Phase string `json:"phase,omitempty"`

	// Gateway status
	Gateway *GatewayStatus `json:"gateway,omitempty"`

	// Workers status
	Workers WorkersStatus `json:"workers,omitempty"`

	// LastActivityTime is the last time there was activity
	LastActivityTime *metav1.Time `json:"lastActivityTime,omitempty"`

	// ServerCert contains server certificate information (for gateway)
	ServerCert *CertificateInfo `json:"serverCert,omitempty"`

	// TLSSecretName is the name of the secret containing gateway TLS certificates
	TLSSecretName string `json:"tlsSecretName,omitempty"`

	// WorkerTLSSecretName is the name of the secret for worker mTLS
	WorkerTLSSecretName string `json:"workerTLSSecretName,omitempty"`

	// LastScaleTime is the last time workers were scaled
	LastScaleTime *metav1.Time `json:"lastScaleTime,omitempty"`

	// Connections contains connection statistics
	Connections *ConnectionsStatus `json:"connections,omitempty"`
}

// ConnectionsStatus contains connection statistics for the pool.
type ConnectionsStatus struct {
	// Active is the current number of active connections
	Active int32 `json:"active,omitempty"`

	// Total is the total number of connections since pool creation
	Total int64 `json:"total,omitempty"`

	// LastConnectionTime is when the last connection was established
	LastConnectionTime *metav1.Time `json:"lastConnectionTime,omitempty"`
}

// GatewayStatus contains gateway deployment status.
type GatewayStatus struct {
	// Ready indicates if the gateway is ready
	Ready bool `json:"ready,omitempty"`

	// Replicas is the number of gateway replicas
	Replicas int32 `json:"replicas,omitempty"`

	// ReadyReplicas is the number of ready gateway replicas
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// DeploymentName is the name of the gateway deployment
	DeploymentName string `json:"deploymentName,omitempty"`

	// ServiceName is the name of the gateway service
	ServiceName string `json:"serviceName,omitempty"`
}

// WorkersStatus contains aggregated worker status.
type WorkersStatus struct {
	// Total is the total number of workers
	Total int32 `json:"total,omitempty"`

	// Ready is the number of ready workers
	Ready int32 `json:"ready,omitempty"`

	// Idle is the number of idle (unallocated) workers
	Idle int32 `json:"idle,omitempty"`

	// Allocated is the number of allocated workers
	Allocated int32 `json:"allocated,omitempty"`

	// Provisioning is the number of workers being provisioned
	Provisioning int32 `json:"provisioning,omitempty"`

	// Failed is the number of failed workers
	Failed int32 `json:"failed,omitempty"`

	// Desired is the desired total number of workers (min idle + allocated workers)
	// This ensures we always maintain min idle workers available, similar to GitHub Actions ARC.
	// Formula: min (idle workers to maintain) + allocated workers
	// Example: if min=4 and 1 worker is allocated, desired=5 (4 idle + 1 allocated)
	Desired int32 `json:"desired,omitempty"`

	// Needed is the number of additional workers needed to meet desired count
	// Calculated as: max(0, desired - (ready + provisioning))
	// This helps with scheduling decisions and scaling actions.
	Needed int32 `json:"needed,omitempty"`
}

// CertificateInfo contains certificate validity information.
type CertificateInfo struct {
	// NotBefore is when the certificate is valid from
	NotBefore *metav1.Time `json:"notBefore,omitempty"`

	// NotAfter is when the certificate expires
	NotAfter *metav1.Time `json:"notAfter,omitempty"`

	// RenewalTime is when the certificate should be renewed
	RenewalTime *metav1.Time `json:"renewalTime,omitempty"`
}

//+kubebuilder:object:root=true
//+kubebuilder:subresource:status
//+kubebuilder:printcolumn:name="Phase",type="string",JSONPath=".status.phase"
//+kubebuilder:printcolumn:name="Gateway",type="string",JSONPath=".status.gateway.ready",priority=1
//+kubebuilder:printcolumn:name="Workers",type="integer",JSONPath=".status.workers.total"
//+kubebuilder:printcolumn:name="Ready",type="integer",JSONPath=".status.workers.ready"
//+kubebuilder:printcolumn:name="Desired",type="integer",JSONPath=".status.workers.desired",priority=1
//+kubebuilder:printcolumn:name="Needed",type="integer",JSONPath=".status.workers.needed",priority=1
//+kubebuilder:printcolumn:name="Idle",type="integer",JSONPath=".status.workers.idle",priority=1
//+kubebuilder:printcolumn:name="Allocated",type="integer",JSONPath=".status.workers.allocated"
//+kubebuilder:printcolumn:name="Connections",type="integer",JSONPath=".status.connections.active"
//+kubebuilder:printcolumn:name="Endpoint",type="string",JSONPath=".status.endpoint",priority=1
//+kubebuilder:printcolumn:name="Age",type="date",JSONPath=".metadata.creationTimestamp"

// BuildKitPool is the Schema for the buildkitpools API.
type BuildKitPool struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata"`

	Spec   BuildKitPoolSpec   `json:"spec"`
	Status BuildKitPoolStatus `json:"status,omitempty"`
}

//+kubebuilder:object:root=true

// BuildKitPoolList contains a list of BuildKitPool.
type BuildKitPoolList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata"`
	Items           []BuildKitPool `json:"items"`
}

func init() {
	SchemeBuilder.Register(&BuildKitPool{}, &BuildKitPoolList{})
}
