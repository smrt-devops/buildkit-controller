package resources

import (
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
)

// NodeSizeResources defines resource requirements for different node sizes.
type NodeSizeResources struct {
	CPURequest    resource.Quantity
	CPULimit      resource.Quantity
	MemoryRequest resource.Quantity
	MemoryLimit   resource.Quantity
}

var nodeSizeMap = map[string]NodeSizeResources{
	"sm": {
		CPURequest:    resource.MustParse("500m"),
		CPULimit:      resource.MustParse("2000m"),
		MemoryRequest: resource.MustParse("512Mi"),
		MemoryLimit:   resource.MustParse("2Gi"),
	},
	"md": {
		CPURequest:    resource.MustParse("1000m"),
		CPULimit:      resource.MustParse("4000m"),
		MemoryRequest: resource.MustParse("2Gi"),
		MemoryLimit:   resource.MustParse("4Gi"),
	},
	"lg": {
		CPURequest:    resource.MustParse("2000m"),
		CPULimit:      resource.MustParse("8000m"),
		MemoryRequest: resource.MustParse("4Gi"),
		MemoryLimit:   resource.MustParse("8Gi"),
	},
	"xl": {
		CPURequest:    resource.MustParse("4000m"),
		CPULimit:      resource.MustParse("16000m"),
		MemoryRequest: resource.MustParse("8Gi"),
		MemoryLimit:   resource.MustParse("16Gi"),
	},
}

// GetNodeSizeResources returns resource requirements for a given node size.
func GetNodeSizeResources(nodeSize string) NodeSizeResources {
	if resources, ok := nodeSizeMap[nodeSize]; ok {
		return resources
	}
	// Default to md if size not found
	return nodeSizeMap["md"]
}

// GetResourceRequirements returns a ResourceRequirements object for a node size.
func GetResourceRequirements(nodeSize string) corev1.ResourceRequirements {
	resources := GetNodeSizeResources(nodeSize)
	return corev1.ResourceRequirements{
		Requests: corev1.ResourceList{
			corev1.ResourceCPU:    resources.CPURequest,
			corev1.ResourceMemory: resources.MemoryRequest,
		},
		Limits: corev1.ResourceList{
			corev1.ResourceCPU:    resources.CPULimit,
			corev1.ResourceMemory: resources.MemoryLimit,
		},
	}
}
