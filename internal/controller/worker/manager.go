package worker

import (
	"context"
	"fmt"
	"time"

	cron "github.com/robfig/cron/v3"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/controller/shared"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

type Manager struct {
	client client.Client
	scheme *runtime.Scheme
	log    utils.Logger
}

func NewManager(k8sClient client.Client, scheme *runtime.Scheme, log utils.Logger) *Manager {
	return &Manager{
		client: k8sClient,
		scheme: scheme,
		log:    log,
	}
}

func (r *Manager) Reconcile(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	return r.reconcileWorkers(ctx, pool, namespace)
}

func (r *Manager) reconcileWorkers(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	shouldScaleToZero := r.checkScaleDownSchedule(pool)

	workerList, err := shared.ListWorkersByPool(ctx, r.client, pool.Name, namespace)
	if err != nil {
		return err
	}

	categories := shared.CategorizeWorkers(ctx, r.client, workerList, shared.WorkerStuckThreshold, r.log)

	r.cleanupFailedWorkers(ctx, categories.FailedWorkers)
	provisioningWorkers := categories.ProvisioningWorkers - int32(len(categories.StuckWorkers))
	r.cleanupStuckWorkers(ctx, categories.StuckWorkers)

	if shouldScaleToZero {
		return r.scaleDownToZero(ctx, categories.IdleWorkers)
	}

	return r.ensureMinimumWorkers(ctx, pool, namespace, categories, provisioningWorkers)
}

func (r *Manager) checkScaleDownSchedule(pool *buildkitv1alpha1.BuildKitPool) bool {
	if pool.Spec.Scaling.ScaleDownSchedule == "" {
		return false
	}

	parser := cron.NewParser(cron.Minute | cron.Hour | cron.Dom | cron.Month | cron.Dow)
	schedule, err := parser.Parse(pool.Spec.Scaling.ScaleDownSchedule)
	if err != nil {
		r.log.Error(err, "Invalid scale-down schedule, ignoring", "schedule", pool.Spec.Scaling.ScaleDownSchedule, "pool", pool.Name)
		return false
	}

	now := time.Now()
	checkWindow := shared.ScaleDownScheduleCheckWindow
	nextScheduled := schedule.Next(now.Add(-checkWindow))
	shouldScaleToZero := nextScheduled.Before(now.Add(checkWindow)) && !nextScheduled.After(now)
	if shouldScaleToZero {
		r.log.Info("Scale-down schedule is active, scaling pool to zero", "pool", pool.Name, "schedule", pool.Spec.Scaling.ScaleDownSchedule, "scheduledTime", nextScheduled)
	}
	return shouldScaleToZero
}

func (r *Manager) cleanupFailedWorkers(ctx context.Context, failedWorkers []*buildkitv1alpha1.BuildKitWorker) {
	for _, worker := range failedWorkers {
		poolName := shared.GetPoolNameFromLabels(worker.Labels)
		r.log.Info("Cleaning up failed worker", "worker", worker.Name, "pool", poolName, "message", worker.Status.Message)
	}
	shared.DeleteWorkers(ctx, r.client, failedWorkers, r.log, "Deleting failed worker")
}

func (r *Manager) cleanupStuckWorkers(ctx context.Context, stuckWorkers []*buildkitv1alpha1.BuildKitWorker) {
	shared.DeleteWorkers(ctx, r.client, stuckWorkers, r.log, "Cleaning up stuck worker, will be recreated")
}

func (r *Manager) scaleDownToZero(ctx context.Context, idleWorkers []*buildkitv1alpha1.BuildKitWorker) error {
	shared.DeleteWorkers(ctx, r.client, idleWorkers, r.log, "Deleting idle worker due to scale-down schedule")
	return nil
}

func (r *Manager) ensureMinimumWorkers(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string, categories shared.WorkerCategories, provisioningWorkers int32) error {
	minIdleWorkers := int32(0)
	if pool.Spec.Scaling.Min != nil {
		minIdleWorkers = *pool.Spec.Scaling.Min
	}

	if minIdleWorkers == 0 {
		return nil
	}

	idleCount := int32(len(categories.IdleWorkers))
	idleWorkersToKeep := minIdleWorkers
	if idleCount > idleWorkersToKeep {
		if err := r.scaleDownExcessIdleWorkers(ctx, pool, categories.IdleWorkers, idleCount-idleWorkersToKeep); err != nil {
			return err
		}
		idleCount = idleWorkersToKeep
	}

	currentIdlePlusProvisioning := idleCount + provisioningWorkers
	workersToCreate := minIdleWorkers - currentIdlePlusProvisioning
	if workersToCreate > 0 {
		return r.createWorkers(ctx, pool, namespace, workersToCreate, idleCount, provisioningWorkers, categories.AllocatedWorkers)
	}

	return nil
}

func (r *Manager) scaleDownExcessIdleWorkers(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, idleWorkers []*buildkitv1alpha1.BuildKitWorker, excessCount int32) error {
	r.log.Info("Scaling down excess idle workers",
		"pool", pool.Name,
		"excess", excessCount)

	sortedIdleWorkers := make([]*buildkitv1alpha1.BuildKitWorker, len(idleWorkers))
	copy(sortedIdleWorkers, idleWorkers)
	shared.SortWorkersByCreationTime(sortedIdleWorkers)

	for i := int32(0); i < excessCount && i < int32(len(sortedIdleWorkers)); i++ {
		worker := sortedIdleWorkers[i]
		if worker.Status.Phase == buildkitv1alpha1.WorkerPhaseAllocated {
			r.log.Info("Skipping allocated worker during scale-down", "worker", worker.Name, "pool", pool.Name)
			continue
		}
		if worker.Status.Phase != buildkitv1alpha1.WorkerPhaseIdle {
			r.log.Info("Skipping non-idle worker during scale-down", "worker", worker.Name, "phase", worker.Status.Phase, "pool", pool.Name)
			continue
		}
		r.log.Info("Deleting excess idle worker", "worker", worker.Name, "pool", pool.Name)
		if err := r.client.Delete(ctx, worker); err != nil {
			r.log.Error(err, "Failed to delete excess worker", "worker", worker.Name)
		}
	}

	return nil
}

func (r *Manager) createWorkerWithOwner(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, worker *buildkitv1alpha1.BuildKitWorker) error {
	if err := utils.SetControllerReference(pool, worker, r.scheme); err != nil {
		return fmt.Errorf("failed to set owner reference on worker: %w", err)
	}

	if err := r.client.Create(ctx, worker); err != nil {
		return fmt.Errorf("failed to create worker: %w", err)
	}

	return nil
}

func (r *Manager) createWorkers(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string, workersToCreate, idleCount, provisioningWorkers, allocatedWorkers int32) error {
	r.log.Info("Creating workers to maintain minimum idle workers",
		"pool", pool.Name,
		"minIdle", *pool.Spec.Scaling.Min,
		"currentIdle", idleCount,
		"provisioning", provisioningWorkers,
		"allocated", allocatedWorkers,
		"toCreate", workersToCreate)

	for i := int32(0); i < workersToCreate; i++ {
		worker := &buildkitv1alpha1.BuildKitWorker{
			ObjectMeta: metav1.ObjectMeta{
				GenerateName: shared.GenerateResourceName(pool.Name, "worker") + "-",
				Namespace:    namespace,
				Labels:       shared.GetWorkerLabels(pool.Name),
			},
			Spec: buildkitv1alpha1.BuildKitWorkerSpec{
				PoolRef: buildkitv1alpha1.PoolReference{
					Name:      pool.Name,
					Namespace: namespace,
				},
			},
		}

		if err := r.createWorkerWithOwner(ctx, pool, worker); err != nil {
			return err
		}

		r.log.Info("Created worker for minimum pool size", "worker", worker.Name, "pool", pool.Name)
	}

	return nil
}
