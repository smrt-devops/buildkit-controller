package resources

import (
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/util/intstr"
	gatewayapiv1 "sigs.k8s.io/gateway-api/apis/v1"
	gatewayapiv1alpha2 "sigs.k8s.io/gateway-api/apis/v1alpha2"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
)

const (
	// GatewayPort is the external TLS port for client connections
	GatewayPort = 1235
	// GatewayMetricsPort is the metrics port
	GatewayMetricsPort = 9090
)

// GetGatewayDeploymentName returns the gateway deployment name for a pool.
func GetGatewayDeploymentName(poolName string) string {
	return fmt.Sprintf("%s-gateway", poolName)
}

// GetGatewayServiceName returns the gateway service name for a pool.
func GetGatewayServiceName(poolName string) string {
	return poolName // Pool name is the service name
}

// NewGatewayDeployment creates a gateway deployment for a pool.
func NewGatewayDeployment(pool *buildkitv1alpha1.BuildKitPool, gatewayImage string, controllerEndpoint string) *appsv1.Deployment {
	replicas := int32(1)
	if pool.Spec.Gateway.Replicas != nil {
		replicas = *pool.Spec.Gateway.Replicas
	}

	deploymentName := GetGatewayDeploymentName(pool.Name)
	labels := map[string]string{
		"app.kubernetes.io/name":           "buildkit-gateway",
		"app.kubernetes.io/instance":       pool.Name,
		"app.kubernetes.io/managed-by":     "buildkit-controller",
		"buildkit.smrt-devops.net/pool":    pool.Name,
		"buildkit.smrt-devops.net/purpose": "gateway",
	}

	// Resource defaults
	cpuLimit := "200m"
	memLimit := "256Mi"
	if pool.Spec.Gateway.Resources != nil {
		if pool.Spec.Gateway.Resources.CPU != "" {
			cpuLimit = pool.Spec.Gateway.Resources.CPU
		}
		if pool.Spec.Gateway.Resources.Memory != "" {
			memLimit = pool.Spec.Gateway.Resources.Memory
		}
	}

	// TLS secret names
	serverTLSSecret := GetSecretName(pool.Name)
	workerTLSSecret := fmt.Sprintf("%s-client-certs", pool.Name)

	return &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{
			Name:      deploymentName,
			Namespace: pool.Namespace,
			Labels:    labels,
		},
		Spec: appsv1.DeploymentSpec{
			Replicas: &replicas,
			Selector: &metav1.LabelSelector{
				MatchLabels: labels,
			},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{
					Labels: labels,
				},
				Spec: corev1.PodSpec{
					Containers: []corev1.Container{
						{
							Name:  "gateway",
							Image: gatewayImage,
							Args: []string{
								"--pool-name", pool.Name,
								"--pool-namespace", pool.Namespace,
								"--listen-addr", fmt.Sprintf("0.0.0.0:%d", GatewayPort),
								"--controller-endpoint", controllerEndpoint,
								"--metrics-addr", fmt.Sprintf("0.0.0.0:%d", GatewayMetricsPort),
								// mTLS to workers is automatic and internal (mandatory, no flags needed)
							},
							Ports: []corev1.ContainerPort{
								{
									Name:          "gateway",
									ContainerPort: GatewayPort,
									Protocol:      corev1.ProtocolTCP,
								},
								{
									Name:          "metrics",
									ContainerPort: GatewayMetricsPort,
									Protocol:      corev1.ProtocolTCP,
								},
							},
							VolumeMounts: []corev1.VolumeMount{
								{
									Name:      "server-tls",
									MountPath: "/etc/gateway/tls",
									ReadOnly:  true,
								},
								{
									Name:      "worker-tls",
									MountPath: "/etc/gateway/worker-tls",
									ReadOnly:  true,
								},
							},
							Resources: corev1.ResourceRequirements{
								Limits: corev1.ResourceList{
									corev1.ResourceCPU:    resource.MustParse(cpuLimit),
									corev1.ResourceMemory: resource.MustParse(memLimit),
								},
								Requests: corev1.ResourceList{
									corev1.ResourceCPU:    resource.MustParse("50m"),
									corev1.ResourceMemory: resource.MustParse("64Mi"),
								},
							},
							ReadinessProbe: &corev1.Probe{
								ProbeHandler: corev1.ProbeHandler{
									TCPSocket: &corev1.TCPSocketAction{
										Port: intstr.FromInt(GatewayPort),
									},
								},
								InitialDelaySeconds: 2,
								PeriodSeconds:       5,
								TimeoutSeconds:      3,
							},
							LivenessProbe: &corev1.Probe{
								ProbeHandler: corev1.ProbeHandler{
									TCPSocket: &corev1.TCPSocketAction{
										Port: intstr.FromInt(GatewayPort),
									},
								},
								InitialDelaySeconds: 10,
								PeriodSeconds:       15,
								TimeoutSeconds:      5,
							},
						},
					},
					Volumes: []corev1.Volume{
						{
							Name: "server-tls",
							VolumeSource: corev1.VolumeSource{
								Secret: &corev1.SecretVolumeSource{
									SecretName: serverTLSSecret,
								},
							},
						},
						{
							Name: "worker-tls",
							VolumeSource: corev1.VolumeSource{
								Secret: &corev1.SecretVolumeSource{
									SecretName: workerTLSSecret,
								},
							},
						},
					},
				},
			},
		},
	}
}

// NewGatewayService creates a service for the gateway.
func NewGatewayService(pool *buildkitv1alpha1.BuildKitPool) *corev1.Service {
	serviceName := GetGatewayServiceName(pool.Name)
	labels := map[string]string{
		"app.kubernetes.io/name":           "buildkit-gateway",
		"app.kubernetes.io/instance":       pool.Name,
		"app.kubernetes.io/managed-by":     "buildkit-controller",
		"buildkit.smrt-devops.net/pool":    pool.Name,
		"buildkit.smrt-devops.net/purpose": "gateway",
	}

	// Gateway service type - defaults to ClusterIP if not specified
	// Supports various setups: ClusterIP (for Istio/Envoy sidecars), LoadBalancer (direct access), NodePort, etc.
	serviceType := corev1.ServiceTypeClusterIP
	if pool.Spec.Gateway.ServiceType != "" {
		serviceType = corev1.ServiceType(pool.Spec.Gateway.ServiceType)
	}

	port := int32(GatewayPort)
	if pool.Spec.Networking.Port != nil {
		port = *pool.Spec.Networking.Port
	}
	// Allow gateway-specific port override
	if pool.Spec.Gateway.Port != nil {
		port = *pool.Spec.Gateway.Port
	}

	gatewayServicePort := corev1.ServicePort{
		Name:       "gateway",
		Port:       port,
		TargetPort: intstr.FromInt(GatewayPort),
		Protocol:   corev1.ProtocolTCP,
	}

	// Set NodePort if specified and service type is NodePort
	if serviceType == corev1.ServiceTypeNodePort && pool.Spec.Gateway.NodePort != nil {
		gatewayServicePort.NodePort = *pool.Spec.Gateway.NodePort
	}

	serviceSpec := corev1.ServiceSpec{
		Type: serviceType,
		Selector: map[string]string{
			"buildkit.smrt-devops.net/pool":    pool.Name,
			"buildkit.smrt-devops.net/purpose": "gateway",
		},
		Ports: []corev1.ServicePort{
			gatewayServicePort,
			{
				Name:       "metrics",
				Port:       GatewayMetricsPort,
				TargetPort: intstr.FromInt(GatewayMetricsPort),
				Protocol:   corev1.ProtocolTCP,
			},
		},
	}

	// Set LoadBalancerClass if specified and service type is LoadBalancer
	if serviceType == corev1.ServiceTypeLoadBalancer && pool.Spec.Gateway.LoadBalancerClass != nil {
		serviceSpec.LoadBalancerClass = pool.Spec.Gateway.LoadBalancerClass
	}

	service := &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:        serviceName,
			Namespace:   pool.Namespace,
			Labels:      labels,
			Annotations: pool.Spec.Networking.Annotations,
		},
		Spec: serviceSpec,
	}

	return service
}

// NewGatewayAPIGateway creates a Gateway API Gateway resource for the BuildKit gateway.
// Returns nil if GatewayRef is specified (controller should not create Gateway).
func NewGatewayAPIGateway(pool *buildkitv1alpha1.BuildKitPool) *gatewayapiv1.Gateway {
	if pool.Spec.Gateway.GatewayAPI == nil || !pool.Spec.Gateway.GatewayAPI.Enabled {
		return nil
	}

	// If GatewayRef is specified, don't create a Gateway resource
	if pool.Spec.Gateway.GatewayAPI.GatewayRef != nil {
		return nil
	}

	gatewayName := pool.Spec.Gateway.GatewayAPI.GatewayName
	if gatewayName == "" {
		gatewayName = fmt.Sprintf("%s-gateway", pool.Name)
	}

	gatewayClassName := pool.Spec.Gateway.GatewayAPI.GatewayClassName
	if gatewayClassName == "" {
		gatewayClassName = "envoy" // Default to envoy (Envoy Gateway), simpler than istio
	}

	labels := map[string]string{
		"app.kubernetes.io/name":           "buildkit-gateway",
		"app.kubernetes.io/instance":       pool.Name,
		"app.kubernetes.io/managed-by":     "buildkit-controller",
		"buildkit.smrt-devops.net/pool":    pool.Name,
		"buildkit.smrt-devops.net/purpose": "gateway",
	}

	port := int32(GatewayPort)
	if pool.Spec.Networking.Port != nil {
		port = *pool.Spec.Networking.Port
	}
	if pool.Spec.Gateway.Port != nil {
		port = *pool.Spec.Gateway.Port
	}

	gateway := &gatewayapiv1.Gateway{
		ObjectMeta: metav1.ObjectMeta{
			Name:        gatewayName,
			Namespace:   pool.Namespace,
			Labels:      labels,
			Annotations: pool.Spec.Gateway.GatewayAPI.Annotations,
		},
		Spec: gatewayapiv1.GatewaySpec{
			GatewayClassName: gatewayapiv1.ObjectName(gatewayClassName),
			Listeners: []gatewayapiv1.Listener{
				func() gatewayapiv1.Listener {
					// TLS is mandatory for Gateway API
					tlsConfig := pool.Spec.Gateway.GatewayAPI.TLS
					if tlsConfig == nil {
						// Default to passthrough mode - pool gateway needs to validate client certificates
						tlsConfig = &buildkitv1alpha1.GatewayAPITLSConfig{
							Mode:         "passthrough",
							AutoGenerate: true,
						}
					}

					tlsMode := tlsConfig.Mode
					if tlsMode == "" {
						tlsMode = "passthrough" // Passthrough required for client cert validation
					}

					// For TLS passthrough, use TLS protocol (not TCP)
					// For TLS terminate, use HTTPS protocol
					// TCP protocol cannot have TLS or hostname
					var protocol gatewayapiv1.ProtocolType
					var hostname *gatewayapiv1.Hostname
					var tls *gatewayapiv1.ListenerTLSConfig

					switch tlsMode {
					case "passthrough":
						// TLS passthrough: use TLS protocol, hostname required, TLS mode passthrough
						protocol = gatewayapiv1.TLSProtocolType
						if pool.Spec.Gateway.GatewayAPI.Hostname != "" {
							h := gatewayapiv1.Hostname(pool.Spec.Gateway.GatewayAPI.Hostname)
							hostname = &h
						}
						mode := gatewayapiv1.TLSModePassthrough
						tls = &gatewayapiv1.ListenerTLSConfig{
							Mode: &mode,
						}
					case "terminate":
						// TLS terminate mode is not supported for BuildKit pools
						// Terminate mode would require HTTPS protocol and HTTPRoute,
						// but BuildKit uses TCP traffic, so only passthrough is valid
						// This should be caught by the reconciler, but handle gracefully here
						mode := gatewayapiv1.TLSModePassthrough
						return gatewayapiv1.Listener{
							Name:     gatewayapiv1.SectionName("buildkit"),
							Protocol: gatewayapiv1.TLSProtocolType, // Fallback to TLS
							Port:     gatewayapiv1.PortNumber(port),
							Hostname: func() *gatewayapiv1.Hostname {
								if pool.Spec.Gateway.GatewayAPI.Hostname != "" {
									h := gatewayapiv1.Hostname(pool.Spec.Gateway.GatewayAPI.Hostname)
									return &h
								}
								return nil
							}(),
							TLS: &gatewayapiv1.ListenerTLSConfig{
								Mode: &mode,
							},
						}
					default:
						// Invalid TLS mode - TLS is mandatory for Gateway API
						// Default to passthrough (will be caught by reconciler validation)
						mode := gatewayapiv1.TLSModePassthrough
						return gatewayapiv1.Listener{
							Name:     gatewayapiv1.SectionName("buildkit"),
							Protocol: gatewayapiv1.TLSProtocolType,
							Port:     gatewayapiv1.PortNumber(port),
							Hostname: func() *gatewayapiv1.Hostname {
								if pool.Spec.Gateway.GatewayAPI.Hostname != "" {
									h := gatewayapiv1.Hostname(pool.Spec.Gateway.GatewayAPI.Hostname)
									return &h
								}
								return nil
							}(),
							TLS: &gatewayapiv1.ListenerTLSConfig{
								Mode: &mode,
							},
						}
					}

					return gatewayapiv1.Listener{
						Name:     gatewayapiv1.SectionName("buildkit"),
						Protocol: protocol,
						Port:     gatewayapiv1.PortNumber(port),
						Hostname: hostname,
						TLS:      tls,
					}
				}(),
			},
		},
	}

	return gateway
}

// NewGatewayAPITLSRoute creates a Gateway API TLSRoute resource for the BuildKit gateway (TLS passthrough mode).
func NewGatewayAPITLSRoute(pool *buildkitv1alpha1.BuildKitPool) *gatewayapiv1alpha2.TLSRoute {
	if pool.Spec.Gateway.GatewayAPI == nil || !pool.Spec.Gateway.GatewayAPI.Enabled {
		return nil
	}

	gatewayName := pool.Spec.Gateway.GatewayAPI.GatewayName
	if gatewayName == "" {
		gatewayName = fmt.Sprintf("%s-gateway", pool.Name)
	}

	// Use GatewayRef if specified
	if pool.Spec.Gateway.GatewayAPI.GatewayRef != nil {
		gatewayName = pool.Spec.Gateway.GatewayAPI.GatewayRef.Name
		if gatewayName == "" {
			gatewayName = fmt.Sprintf("%s-gateway", pool.Name)
		}
	}

	serviceName := GetGatewayServiceName(pool.Name)

	labels := map[string]string{
		"app.kubernetes.io/name":           "buildkit-gateway",
		"app.kubernetes.io/instance":       pool.Name,
		"app.kubernetes.io/managed-by":     "buildkit-controller",
		"buildkit.smrt-devops.net/pool":    pool.Name,
		"buildkit.smrt-devops.net/purpose": "gateway",
	}

	port := int32(GatewayPort)
	if pool.Spec.Networking.Port != nil {
		port = *pool.Spec.Networking.Port
	}
	if pool.Spec.Gateway.Port != nil {
		port = *pool.Spec.Gateway.Port
	}

	tlsRoute := &gatewayapiv1alpha2.TLSRoute{
		ObjectMeta: metav1.ObjectMeta{
			Name:      fmt.Sprintf("%s-tlsroute", pool.Name),
			Namespace: pool.Namespace,
			Labels:    labels,
		},
		Spec: gatewayapiv1alpha2.TLSRouteSpec{
			CommonRouteSpec: gatewayapiv1.CommonRouteSpec{
				ParentRefs: []gatewayapiv1.ParentReference{
					{
						Name: gatewayapiv1.ObjectName(gatewayName),
						Namespace: func() *gatewayapiv1.Namespace {
							var ns gatewayapiv1.Namespace
							if pool.Spec.Gateway.GatewayAPI.GatewayRef != nil && pool.Spec.Gateway.GatewayAPI.GatewayRef.Namespace != "" {
								ns = gatewayapiv1.Namespace(pool.Spec.Gateway.GatewayAPI.GatewayRef.Namespace)
							} else {
								ns = gatewayapiv1.Namespace(pool.Namespace)
							}
							return &ns
						}(),
					},
				},
			},
			Hostnames: func() []gatewayapiv1.Hostname {
				if pool.Spec.Gateway.GatewayAPI.Hostname != "" {
					return []gatewayapiv1.Hostname{gatewayapiv1.Hostname(pool.Spec.Gateway.GatewayAPI.Hostname)}
				}
				return nil
			}(),
			Rules: []gatewayapiv1alpha2.TLSRouteRule{
				{
					BackendRefs: []gatewayapiv1.BackendRef{
						{
							BackendObjectReference: gatewayapiv1.BackendObjectReference{
								Name: gatewayapiv1.ObjectName(serviceName),
								Port: func() *gatewayapiv1.PortNumber {
									p := gatewayapiv1.PortNumber(port)
									return &p
								}(),
							},
						},
					},
				},
			},
		},
	}

	return tlsRoute
}

// NewIngress creates a Kubernetes Ingress resource for the BuildKit gateway.
func NewIngress(pool *buildkitv1alpha1.BuildKitPool) *networkingv1.Ingress {
	if pool.Spec.Gateway.Ingress == nil || !pool.Spec.Gateway.Ingress.Enabled {
		return nil
	}

	serviceName := GetGatewayServiceName(pool.Name)

	port := int32(GatewayPort)
	if pool.Spec.Networking.Port != nil {
		port = *pool.Spec.Networking.Port
	}
	if pool.Spec.Gateway.Port != nil {
		port = *pool.Spec.Gateway.Port
	}

	labels := map[string]string{
		"app.kubernetes.io/name":           "buildkit-gateway",
		"app.kubernetes.io/instance":       pool.Name,
		"app.kubernetes.io/managed-by":     "buildkit-controller",
		"buildkit.smrt-devops.net/pool":    pool.Name,
		"buildkit.smrt-devops.net/purpose": "gateway",
	}

	ingress := &networkingv1.Ingress{
		ObjectMeta: metav1.ObjectMeta{
			Name:        fmt.Sprintf("%s-ingress", pool.Name),
			Namespace:   pool.Namespace,
			Labels:      labels,
			Annotations: pool.Spec.Gateway.Ingress.Annotations,
		},
		Spec: networkingv1.IngressSpec{
			IngressClassName: func() *string {
				if pool.Spec.Gateway.Ingress.IngressClassName != "" {
					className := pool.Spec.Gateway.Ingress.IngressClassName
					return &className
				}
				return nil
			}(),
			Rules: []networkingv1.IngressRule{
				{
					Host: pool.Spec.Gateway.Ingress.Hostname,
					IngressRuleValue: networkingv1.IngressRuleValue{
						HTTP: &networkingv1.HTTPIngressRuleValue{
							Paths: []networkingv1.HTTPIngressPath{
								{
									Path: "/",
									PathType: func() *networkingv1.PathType {
										pt := networkingv1.PathTypePrefix
										return &pt
									}(),
									Backend: networkingv1.IngressBackend{
										Service: &networkingv1.IngressServiceBackend{
											Name: serviceName,
											Port: networkingv1.ServiceBackendPort{
												Number: port,
											},
										},
									},
								},
							},
						},
					},
				},
			},
		},
	}

	// Add TLS configuration if enabled
	if pool.Spec.Gateway.Ingress.TLS != nil && pool.Spec.Gateway.Ingress.TLS.Enabled && pool.Spec.Gateway.Ingress.TLS.SecretName != "" {
		ingress.Spec.TLS = []networkingv1.IngressTLS{
			{
				Hosts:      []string{pool.Spec.Gateway.Ingress.Hostname},
				SecretName: pool.Spec.Gateway.Ingress.TLS.SecretName,
			},
		}
	}

	return ingress
}
