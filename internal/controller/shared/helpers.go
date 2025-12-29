package shared

import (
	"context"
	"fmt"
	"sort"
	"strings"
	"time"

	corev1 "k8s.io/api/core/v1"
	"sigs.k8s.io/controller-runtime/pkg/client"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

// CalculateRequeueInterval calculates the requeue interval based on certificate renewal needs and status update requirements.
func CalculateRequeueInterval(pool *buildkitv1alpha1.BuildKitPool) time.Duration {
	requeueAfter := StatusUpdateInterval

	if pool.Spec.TLS.Enabled && pool.Status.ServerCert != nil && pool.Status.ServerCert.RenewalTime != nil {
		// Requeue when certificate needs rotation
		renewalTime := pool.Status.ServerCert.RenewalTime.Time
		now := time.Now()
		if renewalTime.After(now) {
			timeUntilRenewal := time.Until(renewalTime)
			if timeUntilRenewal > MinRequeueInterval {
				if timeUntilRenewal > MaxRequeueInterval {
					// Use the shorter of: cert renewal time or worker status update interval
					requeueAfter = minDuration(requeueAfter, MaxRequeueInterval)
				} else {
					certRequeueTime := timeUntilRenewal - MinRequeueInterval
					requeueAfter = minDuration(requeueAfter, certRequeueTime)
				}
			} else {
				requeueAfter = MinRequeueInterval
			}
		} else {
			// Certificate needs rotation soon, check more frequently
			requeueAfter = MinRequeueInterval
		}
	}

	return requeueAfter
}

// minDuration returns the minimum of two durations.
func minDuration(a, b time.Duration) time.Duration {
	if a < b {
		return a
	}
	return b
}

// MinInt32 returns the minimum of two int32 values.
func MinInt32(a, b int32) int32 {
	if a < b {
		return a
	}
	return b
}

// MaxInt32 returns the maximum of two int32 values.
func MaxInt32(a, b int32) int32 {
	if a > b {
		return a
	}
	return b
}

// WorkerCategories categorizes workers by their phase and health status.
type WorkerCategories struct {
	ReadyWorkers        int32
	ProvisioningWorkers int32
	IdleWorkers         []*buildkitv1alpha1.BuildKitWorker
	AllocatedWorkers    int32
	FailedWorkers       []*buildkitv1alpha1.BuildKitWorker
	StuckWorkers        []*buildkitv1alpha1.BuildKitWorker
}

// CategorizeWorkers categorizes workers by their phase and health status.
func CategorizeWorkers(ctx context.Context, k8sClient client.Client, workerList *buildkitv1alpha1.BuildKitWorkerList, stuckThreshold time.Duration, log utils.Logger) WorkerCategories {
	categories := WorkerCategories{
		IdleWorkers:   []*buildkitv1alpha1.BuildKitWorker{},
		FailedWorkers: []*buildkitv1alpha1.BuildKitWorker{},
		StuckWorkers:  []*buildkitv1alpha1.BuildKitWorker{},
	}

	now := time.Now()

	for i := range workerList.Items {
		worker := &workerList.Items[i]

		switch worker.Status.Phase {
		case buildkitv1alpha1.WorkerPhaseIdle:
			categories.ReadyWorkers++
			categories.IdleWorkers = append(categories.IdleWorkers, worker)
		case buildkitv1alpha1.WorkerPhaseRunning:
			categories.ReadyWorkers++
		case buildkitv1alpha1.WorkerPhaseAllocated:
			categories.ReadyWorkers++
			categories.AllocatedWorkers++
		case buildkitv1alpha1.WorkerPhasePending, buildkitv1alpha1.WorkerPhaseProvisioning:
			categories.ProvisioningWorkers++
			if IsWorkerStuck(ctx, k8sClient, worker, stuckThreshold, now, log) {
				categories.StuckWorkers = append(categories.StuckWorkers, worker)
			}
		case buildkitv1alpha1.WorkerPhaseFailed:
			categories.FailedWorkers = append(categories.FailedWorkers, worker)
		case buildkitv1alpha1.WorkerPhaseTerminating:
			// Ignore terminating workers, they're being cleaned up
		}
	}

	return categories
}

// IsWorkerStuck checks if a worker is stuck in provisioning.
func IsWorkerStuck(ctx context.Context, k8sClient client.Client, worker *buildkitv1alpha1.BuildKitWorker, stuckThreshold time.Duration, now time.Time, log utils.Logger) bool {
	// Check if worker is stuck (in provisioning too long)
	if worker.Status.CreatedAt != nil {
		timeInProvisioning := now.Sub(worker.Status.CreatedAt.Time)
		if timeInProvisioning > stuckThreshold {
			return true
		}
	}

	// Check pod status for unschedulable conditions
	if worker.Status.PodName == "" {
		return false
	}

	pod := &corev1.Pod{}
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: worker.Status.PodName, Namespace: worker.Namespace}, pod); err != nil {
		return false
	}

	// Check for unschedulable condition
	for _, cond := range pod.Status.Conditions {
		if cond.Type == corev1.PodScheduled && cond.Status == corev1.ConditionFalse {
			if cond.Reason == corev1.PodReasonUnschedulable {
				log.Info("Worker pod is unschedulable",
					"worker", worker.Name,
					"reason", cond.Reason,
					"message", cond.Message)
				return true
			}
		}
	}

	// Check if pod has been in Pending for too long
	if pod.Status.Phase == corev1.PodPending {
		if pod.CreationTimestamp.Time.Add(stuckThreshold).Before(now) {
			return true
		}
	}

	// Check if pod failed
	if pod.Status.Phase == corev1.PodFailed {
		log.Info("Worker pod failed", "worker", worker.Name)
		return true
	}

	return false
}

// SortWorkersByCreationTime sorts workers by creation time (oldest first).
// Uses sort.Slice for efficient sorting.
func SortWorkersByCreationTime(workers []*buildkitv1alpha1.BuildKitWorker) {
	if len(workers) <= 1 {
		return
	}
	// Use sort.Slice for O(n log n) performance instead of O(n²) bubble sort
	sort.Slice(workers, func(i, j int) bool {
		return workers[i].CreationTimestamp.Time.Before(workers[j].CreationTimestamp.Time)
	})
}

// DeleteWorkers deletes a list of workers, logging errors but continuing on failure.
// Returns the number of successfully deleted workers.
func DeleteWorkers(ctx context.Context, k8sClient client.Client, workers []*buildkitv1alpha1.BuildKitWorker, log utils.Logger, action string) int {
	deleted := 0
	for _, worker := range workers {
		poolName := GetPoolNameFromLabels(worker.Labels)
		log.Info(action, "worker", worker.Name, "pool", poolName)
		if err := k8sClient.Delete(ctx, worker); err != nil {
			log.Error(err, "Failed to delete worker", "worker", worker.Name, "pool", poolName)
		} else {
			deleted++
		}
	}
	return deleted
}

// GetPoolNameFromLabels extracts the pool name from worker labels.
func GetPoolNameFromLabels(labels map[string]string) string {
	if poolName, exists := labels["buildkit.smrt-devops.net/pool"]; exists {
		return poolName
	}
	return ""
}

// GetWorkerPod fetches a worker's pod by name and namespace.
// Returns the pod and an error. If the pod is not found, returns nil and the error.
func GetWorkerPod(ctx context.Context, k8sClient client.Client, podName, namespace string) (*corev1.Pod, error) {
	if podName == "" {
		return nil, fmt.Errorf("pod name is empty")
	}
	pod := &corev1.Pod{}
	key := client.ObjectKey{Name: podName, Namespace: namespace}
	if err := k8sClient.Get(ctx, key, pod); err != nil {
		return nil, err
	}
	return pod, nil
}

// GenerateEndpoint generates a BuildKit endpoint string.
func GenerateEndpoint(hostname string, port int32) string {
	return fmt.Sprintf("tcp://%s:%d", hostname, port)
}

// GenerateServiceHostname generates a Kubernetes service hostname.
func GenerateServiceHostname(serviceName, namespace string) string {
	return fmt.Sprintf("%s.%s.svc", serviceName, namespace)
}

// GenerateResourceName generates a resource name with a suffix.
func GenerateResourceName(baseName, suffix string) string {
	return fmt.Sprintf("%s-%s", baseName, suffix)
}

// GenerateClientCertCommonName generates a common name for a client certificate.
func GenerateClientCertCommonName(poolName string) string {
	return fmt.Sprintf("client@%s", poolName)
}

// ListWorkersByPool lists all workers for a given pool in a namespace.
func ListWorkersByPool(ctx context.Context, k8sClient client.Client, poolName, namespace string) (*buildkitv1alpha1.BuildKitWorkerList, error) {
	workerList := &buildkitv1alpha1.BuildKitWorkerList{}
	if err := k8sClient.List(ctx, workerList,
		client.InNamespace(namespace),
		client.MatchingLabels{"buildkit.smrt-devops.net/pool": poolName}); err != nil {
		return nil, fmt.Errorf("failed to list workers for pool %s: %w", poolName, err)
	}
	return workerList, nil
}

// GetWorkerLabels returns standard labels for a worker belonging to a pool.
func GetWorkerLabels(poolName string) map[string]string {
	return map[string]string{
		"buildkit.smrt-devops.net/pool":   poolName,
		"buildkit.smrt-devops.net/worker": "true",
	}
}

// ParsePoolReference parses a pool reference string that supports "name" or "namespace/name" format.
// Returns the pool name and namespace (empty if not specified).
func ParsePoolReference(poolRef string) (poolName, poolNamespace string) {
	if strings.Contains(poolRef, "/") {
		parts := strings.SplitN(poolRef, "/", 2)
		if len(parts) == 2 {
			return parts[1], parts[0]
		}
	}
	return poolRef, ""
}

// GetSearchNamespaces determines the list of namespaces to search for a pool.
func GetSearchNamespaces(explicitNamespace, currentNamespace, defaultNamespace string) []string {
	if explicitNamespace != "" {
		return []string{explicitNamespace}
	}
	namespaces := []string{currentNamespace}
	if defaultNamespace != "" && defaultNamespace != currentNamespace {
		namespaces = append(namespaces, defaultNamespace)
	}
	return namespaces
}

// ParseDurationWithDefault parses a duration string, returning the default if parsing fails.
func ParseDurationWithDefault(durationStr string, defaultDuration time.Duration) time.Duration {
	if durationStr == "" {
		return defaultDuration
	}
	parsed, err := time.ParseDuration(durationStr)
	if err != nil {
		return defaultDuration
	}
	return parsed
}
