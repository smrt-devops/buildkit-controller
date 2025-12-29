package status

import (
	"context"
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/controller/shared"
	"github.com/smrt-devops/buildkit-controller/internal/resources"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

type Updater struct {
	client client.Client
	log    utils.Logger
}

func NewUpdater(k8sClient client.Client, log utils.Logger) *Updater {
	return &Updater{
		client: k8sClient,
		log:    log,
	}
}

func (r *Updater) Update(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	return r.updateStatus(ctx, pool, namespace)
}

func (r *Updater) updateStatus(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	latestPool := &buildkitv1alpha1.BuildKitPool{}
	if err := r.client.Get(ctx, types.NamespacedName{Name: pool.Name, Namespace: namespace}, latestPool); err != nil {
		return fmt.Errorf("failed to fetch latest pool: %w", err)
	}

	r.initializeStatusFields(latestPool)
	originalStatus := latestPool.Status.DeepCopy()

	if err := r.updateGatewayStatus(ctx, latestPool, namespace); err != nil {
		return err
	}

	if err := r.updateWorkerStatus(ctx, latestPool, namespace); err != nil {
		return err
	}

	r.updateEndpoint(latestPool, namespace)
	r.updateConnectionsStatus(latestPool)
	r.updateWorkerScalingMetrics(latestPool)
	r.updateReadyCondition(latestPool)

	if r.statusChanged(*originalStatus, latestPool.Status) {
		if err := r.updateStatusWithRetry(ctx, latestPool, namespace); err != nil {
			return err
		}

		r.log.V(1).Info("Pool status updated",
			"pool", latestPool.Name,
			"phase", latestPool.Status.Phase,
			"workers", latestPool.Status.Workers.Total,
			"connections", latestPool.Status.Connections.Active)
	}

	return nil
}

func (r *Updater) updateStatusWithRetry(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	if err := r.client.Status().Update(ctx, pool); err != nil {
		r.log.V(1).Info("Status update conflict, will retry on next reconcile",
			"pool", pool.Name,
			"namespace", namespace,
			"error", err)
		return fmt.Errorf("failed to update status: %w", err)
	}
	return nil
}

func (r *Updater) statusChanged(old, new buildkitv1alpha1.BuildKitPoolStatus) bool {
	if old.Phase != new.Phase || old.Endpoint != new.Endpoint {
		return true
	}

	if old.Workers.Total != new.Workers.Total ||
		old.Workers.Ready != new.Workers.Ready ||
		old.Workers.Idle != new.Workers.Idle ||
		old.Workers.Allocated != new.Workers.Allocated ||
		old.Workers.Provisioning != new.Workers.Provisioning ||
		old.Workers.Failed != new.Workers.Failed ||
		old.Workers.Desired != new.Workers.Desired ||
		old.Workers.Needed != new.Workers.Needed {
		return true
	}

	if !r.gatewayStatusEqual(old.Gateway, new.Gateway) {
		return true
	}

	if !r.connectionsStatusEqual(old.Connections, new.Connections) {
		return true
	}

	return r.conditionsChanged(old.Conditions, new.Conditions)
}

func (r *Updater) gatewayStatusEqual(old, new *buildkitv1alpha1.GatewayStatus) bool {
	if (old == nil) != (new == nil) {
		return false
	}
	if old == nil {
		return true
	}
	return old.Ready == new.Ready && old.Replicas == new.Replicas && old.ReadyReplicas == new.ReadyReplicas
}

func (r *Updater) connectionsStatusEqual(old, new *buildkitv1alpha1.ConnectionsStatus) bool {
	if (old == nil) != (new == nil) {
		return false
	}
	if old == nil {
		return true
	}
	return old.Active == new.Active && old.Total == new.Total
}

func (r *Updater) conditionsChanged(old, new []metav1.Condition) bool {
	if len(old) != len(new) {
		return true
	}
	for i := range old {
		if i >= len(new) {
			return true
		}
		oc, nc := old[i], new[i]
		if oc.Type != nc.Type || oc.Status != nc.Status || oc.Reason != nc.Reason ||
			oc.Message != nc.Message || oc.ObservedGeneration != nc.ObservedGeneration {
			return true
		}
	}
	return false
}

func (r *Updater) zeroWorkersStatus() buildkitv1alpha1.WorkersStatus {
	return buildkitv1alpha1.WorkersStatus{
		Total:        0,
		Ready:        0,
		Idle:         0,
		Allocated:    0,
		Provisioning: 0,
		Failed:       0,
	}
}

func (r *Updater) initializeStatusFields(pool *buildkitv1alpha1.BuildKitPool) {
	if pool.Status.Workers.Total == 0 && pool.Status.Workers.Ready == 0 {
		pool.Status.Workers = r.zeroWorkersStatus()
	}
	if pool.Status.Connections == nil {
		pool.Status.Connections = &buildkitv1alpha1.ConnectionsStatus{
			Active: 0,
			Total:  0,
		}
	}
}

func (r *Updater) getGatewayDeployment(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) (*appsv1.Deployment, error) {
	gatewayDeploymentName := resources.GetGatewayDeploymentName(pool.Name)
	deployment := &appsv1.Deployment{}
	err := r.client.Get(ctx, types.NamespacedName{Name: gatewayDeploymentName, Namespace: namespace}, deployment)
	if err != nil {
		return nil, client.IgnoreNotFound(err)
	}
	return deployment, nil
}

func (r *Updater) updateGatewayStatus(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	deployment, err := r.getGatewayDeployment(ctx, pool, namespace)
	if err != nil {
		return err
	}

	if deployment == nil {
		pool.Status.Phase = "Pending"
		pool.Status.Gateway = nil
		return nil
	}

	gatewayDeploymentName := resources.GetGatewayDeploymentName(pool.Name)
	pool.Status.Gateway = &buildkitv1alpha1.GatewayStatus{
		DeploymentName: gatewayDeploymentName,
		ServiceName:    pool.Name,
		Replicas:       deployment.Status.Replicas,
		ReadyReplicas:  deployment.Status.ReadyReplicas,
		Ready:          deployment.Status.ReadyReplicas > 0,
	}

	if deployment.Status.ReadyReplicas > 0 {
		pool.Status.Phase = "Running"
	} else {
		pool.Status.Phase = "Pending"
	}

	return nil
}

func (r *Updater) updateWorkerStatus(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	workerList, listErr := shared.ListWorkersByPool(ctx, r.client, pool.Name, namespace)

	if listErr != nil {
		r.log.V(1).Info("Failed to list workers for status update", "error", listErr, "pool", pool.Name, "namespace", namespace)
		pool.Status.Workers = r.zeroWorkersStatus()
		return nil
	}

	pool.Status.Workers = buildkitv1alpha1.WorkersStatus{
		Total:        int32(len(workerList.Items)),
		Ready:        0,
		Idle:         0,
		Allocated:    0,
		Provisioning: 0,
		Failed:       0,
		Desired:      0,
		Needed:       0,
	}

	r.countWorkersByPhase(workerList, pool)

	if pool.Status.Workers.Allocated > 0 {
		now := metav1.Now()
		pool.Status.LastActivityTime = &now
	}

	return nil
}

func (r *Updater) countWorkersByPhase(workerList *buildkitv1alpha1.BuildKitWorkerList, pool *buildkitv1alpha1.BuildKitPool) {
	for i := range workerList.Items {
		worker := &workerList.Items[i]
		switch worker.Status.Phase {
		case buildkitv1alpha1.WorkerPhaseIdle:
			pool.Status.Workers.Ready++
			pool.Status.Workers.Idle++
		case buildkitv1alpha1.WorkerPhaseRunning:
			pool.Status.Workers.Ready++
		case buildkitv1alpha1.WorkerPhaseAllocated:
			pool.Status.Workers.Ready++
			pool.Status.Workers.Allocated++
		case buildkitv1alpha1.WorkerPhaseFailed:
			pool.Status.Workers.Failed++
		case buildkitv1alpha1.WorkerPhasePending, buildkitv1alpha1.WorkerPhaseProvisioning:
			pool.Status.Workers.Provisioning++
		}
	}
}

func (r *Updater) updateEndpoint(pool *buildkitv1alpha1.BuildKitPool, namespace string) {
	port := shared.DefaultGatewayPort
	if pool.Spec.Gateway.Port != nil {
		port = *pool.Spec.Gateway.Port
	} else if pool.Spec.Networking.Port != nil {
		port = *pool.Spec.Networking.Port
	}

	if pool.Spec.Networking.External != nil && pool.Spec.Networking.External.Hostname != "" {
		pool.Status.Endpoint = shared.GenerateEndpoint(pool.Spec.Networking.External.Hostname, port)
	} else {
		serviceHostname := shared.GenerateServiceHostname(pool.Name, namespace)
		pool.Status.Endpoint = shared.GenerateEndpoint(serviceHostname, port)
	}
}

// updateConnectionsStatus updates the connections status.
// Estimates active connections from allocated workers (each allocated worker typically has at least one connection).
func (r *Updater) updateConnectionsStatus(pool *buildkitv1alpha1.BuildKitPool) {
	activeConnections := int32(0)
	if pool.Status.Workers.Allocated > 0 {
		activeConnections = pool.Status.Workers.Allocated
	}

	pool.Status.Connections = &buildkitv1alpha1.ConnectionsStatus{
		Active: activeConnections,
		Total:  0,
	}

	if activeConnections > 0 {
		now := metav1.Now()
		pool.Status.Connections.LastConnectionTime = &now
	}
}

func (r *Updater) updateWorkerScalingMetrics(pool *buildkitv1alpha1.BuildKitPool) {
	desiredWorkers := r.calculateDesiredWorkers(pool, pool.Status.Workers.Allocated)
	pool.Status.Workers.Desired = desiredWorkers

	currentWorkers := pool.Status.Workers.Ready + pool.Status.Workers.Provisioning
	neededWorkers := shared.MaxInt32(desiredWorkers-currentWorkers, 0)
	pool.Status.Workers.Needed = neededWorkers

	r.log.V(1).Info("Updated worker status",
		"pool", pool.Name,
		"total", pool.Status.Workers.Total,
		"ready", pool.Status.Workers.Ready,
		"idle", pool.Status.Workers.Idle,
		"allocated", pool.Status.Workers.Allocated,
		"provisioning", pool.Status.Workers.Provisioning,
		"failed", pool.Status.Workers.Failed,
		"desired", pool.Status.Workers.Desired,
		"needed", pool.Status.Workers.Needed,
		"activeConnections", pool.Status.Connections.Active)
}

func (r *Updater) updateReadyCondition(pool *buildkitv1alpha1.BuildKitPool) {
	condition := metav1.Condition{
		Type:               "Ready",
		Status:             metav1.ConditionFalse,
		ObservedGeneration: pool.Generation,
		LastTransitionTime: metav1.Now(),
	}

	if pool.Status.Gateway != nil && pool.Status.Gateway.Ready {
		condition.Status = metav1.ConditionTrue
		condition.Reason = "GatewayReady"
		condition.Message = "Gateway is ready, pool can accept requests"
	} else {
		condition.Reason = "GatewayNotReady"
		condition.Message = "Gateway is not ready"
	}

	pool.Status.Conditions = utils.UpdateCondition(pool.Status.Conditions, condition)
}

func (r *Updater) calculateDesiredWorkers(pool *buildkitv1alpha1.BuildKitPool, allocatedWorkers int32) int32 {
	minIdleWorkers := int32(0)
	if pool.Spec.Scaling.Min != nil {
		minIdleWorkers = *pool.Spec.Scaling.Min
	}

	maxWorkers := shared.DefaultMaxWorkers
	if pool.Spec.Scaling.Max != nil {
		maxWorkers = *pool.Spec.Scaling.Max
	}

	// Desired = min idle workers + allocated workers
	// This ensures we always maintain min idle workers available
	desired := shared.MinInt32(minIdleWorkers+allocatedWorkers, maxWorkers)

	return desired
}
