package resources

import (
	"fmt"

	corev1 "k8s.io/api/core/v1"

	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

const (
	// DefaultBuildkitUserID is the default user ID for buildkitd container.
	DefaultBuildkitUserID = int64(1000)
	// DefaultBuildkitGroupID is the default group ID for buildkitd container.
	DefaultBuildkitGroupID = int64(1000)
	// DefaultBuildkitPort is the default port for buildkitd.
	DefaultBuildkitPort = int32(1234)
)

// BuildkitdContainerSpec defines the specification for a buildkitd container.
type BuildkitdContainerSpec struct {
	Image            string
	ConfigMapName    string
	SecretName       string
	TLSEnabled       bool
	Resources        corev1.ResourceRequirements
	DefaultResources string
}

// NewBuildkitdContainer creates a buildkitd container specification.
func NewBuildkitdContainer(spec *BuildkitdContainerSpec) corev1.Container {
	args := []string{
		"--addr", "unix:///run/user/1000/buildkit/buildkitd.sock",
		"--addr", fmt.Sprintf("tcp://0.0.0.0:%d", DefaultBuildkitPort),
		"--config", "/etc/buildkit/buildkitd.toml",
		"--oci-worker-no-process-sandbox",
	}

	// Add TLS configuration if enabled
	if spec.TLSEnabled {
		args = append(args,
			"--tlscacert", "/certs/ca.crt",
			"--tlscert", "/certs/tls.crt",
			"--tlskey", "/certs/tls.key",
		)
	}

	// Determine resources
	resources := spec.Resources
	if resources.Requests == nil && resources.Limits == nil && spec.DefaultResources != "" {
		resources = GetResourceRequirements(spec.DefaultResources)
	}

	volumeMounts := []corev1.VolumeMount{
		{
			Name:      "config",
			ReadOnly:  true,
			MountPath: "/etc/buildkit",
		},
		{
			Name:      "buildkitd",
			MountPath: "/home/user/.local/share/buildkit",
		},
	}

	// Add TLS volume mount if enabled
	if spec.TLSEnabled {
		volumeMounts = append(volumeMounts, corev1.VolumeMount{
			Name:      "certs",
			ReadOnly:  true,
			MountPath: "/certs",
		})
	}

	return corev1.Container{
		Name:      "buildkitd",
		Image:     spec.Image,
		Args:      args,
		Resources: resources,
		ReadinessProbe: &corev1.Probe{
			ProbeHandler: corev1.ProbeHandler{
				Exec: &corev1.ExecAction{
					Command: []string{"buildctl", "--addr", "unix:///run/user/1000/buildkit/buildkitd.sock", "debug", "workers"},
				},
			},
			InitialDelaySeconds: 2,
			PeriodSeconds:       5,
			TimeoutSeconds:      3,
			SuccessThreshold:    1,
			FailureThreshold:    3,
		},
		LivenessProbe: &corev1.Probe{
			ProbeHandler: corev1.ProbeHandler{
				Exec: &corev1.ExecAction{
					Command: []string{"buildctl", "--addr", "unix:///run/user/1000/buildkit/buildkitd.sock", "debug", "workers"},
				},
			},
			InitialDelaySeconds: 10,
			PeriodSeconds:       15,
			TimeoutSeconds:      5,
			SuccessThreshold:    1,
			FailureThreshold:    3,
		},
		SecurityContext: &corev1.SecurityContext{
			SeccompProfile: &corev1.SeccompProfile{
				Type: corev1.SeccompProfileTypeUnconfined,
			},
			RunAsUser:  utils.Int64Ptr(DefaultBuildkitUserID),
			RunAsGroup: utils.Int64Ptr(DefaultBuildkitGroupID),
		},
		Ports: []corev1.ContainerPort{
			{
				Name:          "buildkit",
				ContainerPort: DefaultBuildkitPort,
				Protocol:      corev1.ProtocolTCP,
			},
		},
		VolumeMounts: volumeMounts,
	}
}

// BuildVolumes creates the volumes for a BuildKit worker pod.
func BuildVolumes(configMapName, secretName string, tlsEnabled bool) []corev1.Volume {
	volumes := []corev1.Volume{
		{
			Name: "config",
			VolumeSource: corev1.VolumeSource{
				ConfigMap: &corev1.ConfigMapVolumeSource{
					LocalObjectReference: corev1.LocalObjectReference{
						Name: configMapName,
					},
				},
			},
		},
		{
			Name: "buildkitd",
			VolumeSource: corev1.VolumeSource{
				EmptyDir: &corev1.EmptyDirVolumeSource{},
			},
		},
	}

	// Add TLS volume if enabled (for direct TLS on buildkitd, not used in gateway mode)
	if tlsEnabled {
		volumes = append(volumes, corev1.Volume{
			Name: "certs",
			VolumeSource: corev1.VolumeSource{
				Secret: &corev1.SecretVolumeSource{
					SecretName: secretName,
				},
			},
		})
	}

	return volumes
}
