package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// WorkerPhase defines the lifecycle phase of a BuildKitWorker
// +kubebuilder:validation:Enum=Pending;Provisioning;Running;Idle;Allocated;Terminating;Failed
type WorkerPhase string

const (
	// WorkerPhasePending indicates the worker is waiting to be created
	WorkerPhasePending WorkerPhase = "Pending"
	// WorkerPhaseProvisioning indicates the worker pod is being created
	WorkerPhaseProvisioning WorkerPhase = "Provisioning"
	// WorkerPhaseRunning indicates the worker is running and ready
	WorkerPhaseRunning WorkerPhase = "Running"
	// WorkerPhaseIdle indicates the worker is ready but not allocated
	WorkerPhaseIdle WorkerPhase = "Idle"
	// WorkerPhaseAllocated indicates the worker is allocated to a job
	WorkerPhaseAllocated WorkerPhase = "Allocated"
	// WorkerPhaseTerminating indicates the worker is being terminated
	WorkerPhaseTerminating WorkerPhase = "Terminating"
	// WorkerPhaseFailed indicates the worker has failed
	WorkerPhaseFailed WorkerPhase = "Failed"
)

// BuildKitWorkerSpec defines the desired state of BuildKitWorker.
type BuildKitWorkerSpec struct {
	// PoolRef references the parent BuildKitPool
	PoolRef PoolReference `json:"poolRef"`

	// Allocation contains job allocation information (set when allocated)
	// +optional
	Allocation *WorkerAllocation `json:"allocation,omitempty"`
}

// PoolReference references a BuildKitPool.
type PoolReference struct {
	// Name is the name of the BuildKitPool
	Name string `json:"name"`

	// Namespace is the namespace of the BuildKitPool (defaults to same namespace)
	// +optional
	Namespace string `json:"namespace,omitempty"`
}

// WorkerAllocation contains job allocation information.
type WorkerAllocation struct {
	// JobID is the unique identifier for the allocated job
	JobID string `json:"jobId"`

	// Token is the allocation token for authentication
	Token string `json:"token"`

	// RequestedBy is the identity that requested the allocation
	// +optional
	RequestedBy string `json:"requestedBy,omitempty"`

	// AllocatedAt is when the worker was allocated
	AllocatedAt metav1.Time `json:"allocatedAt"`

	// ExpiresAt is when the allocation expires (optional timeout)
	// +optional
	ExpiresAt *metav1.Time `json:"expiresAt,omitempty"`

	// Metadata contains optional job metadata
	// +optional
	Metadata map[string]string `json:"metadata,omitempty"`
}

// BuildKitWorkerStatus defines the observed state of BuildKitWorker.
type BuildKitWorkerStatus struct {
	// Phase is the current lifecycle phase
	Phase WorkerPhase `json:"phase,omitempty"`

	// Conditions represent the latest available observations
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// PodName is the name of the worker pod
	// +optional
	PodName string `json:"podName,omitempty"`

	// PodIP is the IP address of the worker pod
	// +optional
	PodIP string `json:"podIP,omitempty"`

	// Endpoint is the internal endpoint for the gateway to reach this worker
	// Format: <pod-ip>:1234 (plain buildkitd port)
	// +optional
	Endpoint string `json:"endpoint,omitempty"`

	// CreatedAt is when the worker was created
	// +optional
	CreatedAt *metav1.Time `json:"createdAt,omitempty"`

	// ReadyAt is when the worker became ready
	// +optional
	ReadyAt *metav1.Time `json:"readyAt,omitempty"`

	// LastActivityAt is when the worker last had activity
	// +optional
	LastActivityAt *metav1.Time `json:"lastActivityAt,omitempty"`

	// AllocationCount is the number of times this worker has been allocated
	AllocationCount int32 `json:"allocationCount,omitempty"`

	// Message provides additional status information
	// +optional
	Message string `json:"message,omitempty"`
}

//+kubebuilder:object:root=true
//+kubebuilder:subresource:status
//+kubebuilder:printcolumn:name="Pool",type="string",JSONPath=".spec.poolRef.name"
//+kubebuilder:printcolumn:name="Phase",type="string",JSONPath=".status.phase"
//+kubebuilder:printcolumn:name="Pod",type="string",JSONPath=".status.podName"
//+kubebuilder:printcolumn:name="JobID",type="string",JSONPath=".spec.allocation.jobId",priority=1
//+kubebuilder:printcolumn:name="Age",type="date",JSONPath=".metadata.creationTimestamp"

// BuildKitWorker is the Schema for the buildkitworkers API.
// Represents a single ephemeral BuildKit worker instance allocated to a job.
type BuildKitWorker struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata"`

	Spec   BuildKitWorkerSpec   `json:"spec"`
	Status BuildKitWorkerStatus `json:"status,omitempty"`
}

//+kubebuilder:object:root=true

// BuildKitWorkerList contains a list of BuildKitWorker.
type BuildKitWorkerList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata"`
	Items           []BuildKitWorker `json:"items"`
}

func init() {
	SchemeBuilder.Register(&BuildKitWorker{}, &BuildKitWorkerList{})
}
