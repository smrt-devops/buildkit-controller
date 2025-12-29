package controller

import (
	"context"
	"fmt"
	"time"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/controller/shared"
	"github.com/smrt-devops/buildkit-controller/internal/resources"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

const (
	workerFinalizer = "buildkit.smrt-devops.net/worker-finalizer"
)

type BuildKitWorkerReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    utils.Logger
}

//+kubebuilder:rbac:groups=buildkit.smrt-devops.net,resources=buildkitworkers,verbs=get;list;watch;create;update;patch;delete
//+kubebuilder:rbac:groups=buildkit.smrt-devops.net,resources=buildkitworkers/status,verbs=get;update;patch
//+kubebuilder:rbac:groups=buildkit.smrt-devops.net,resources=buildkitworkers/finalizers,verbs=update
//+kubebuilder:rbac:groups="",resources=pods,verbs=get;list;watch;create;update;patch;delete

func (r *BuildKitWorkerReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("worker", req.NamespacedName)

	worker := &buildkitv1alpha1.BuildKitWorker{}
	if err := r.Get(ctx, req.NamespacedName, worker); err != nil {
		if apierrors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, err
	}

	if !worker.DeletionTimestamp.IsZero() {
		return r.reconcileDelete(ctx, worker, log)
	}

	if !controllerutil.ContainsFinalizer(worker, workerFinalizer) {
		controllerutil.AddFinalizer(worker, workerFinalizer)
		if err := r.Update(ctx, worker); err != nil {
			return ctrl.Result{}, err
		}
		return ctrl.Result{Requeue: true}, nil
	}

	pool := &buildkitv1alpha1.BuildKitPool{}
	poolNamespace := worker.Spec.PoolRef.Namespace
	if poolNamespace == "" {
		poolNamespace = worker.Namespace
	}
	if err := r.Get(ctx, types.NamespacedName{Name: worker.Spec.PoolRef.Name, Namespace: poolNamespace}, pool); err != nil {
		log.Error(err, "Failed to get parent pool")
		return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhaseFailed, "Parent pool not found", log)
	}

	switch worker.Status.Phase {
	case "", buildkitv1alpha1.WorkerPhasePending:
		return r.reconcilePending(ctx, worker, pool, log)
	case buildkitv1alpha1.WorkerPhaseProvisioning:
		return r.reconcileProvisioning(ctx, worker, pool, log)
	case buildkitv1alpha1.WorkerPhaseRunning, buildkitv1alpha1.WorkerPhaseIdle, buildkitv1alpha1.WorkerPhaseAllocated:
		return r.reconcileRunning(ctx, worker, log)
	case buildkitv1alpha1.WorkerPhaseTerminating:
		return r.reconcileTerminating(ctx, worker, log)
	case buildkitv1alpha1.WorkerPhaseFailed:
		return ctrl.Result{RequeueAfter: 5 * time.Minute}, nil
	default:
		return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhasePending, "Unknown phase, resetting", log)
	}
}

func (r *BuildKitWorkerReconciler) getWorkerPod(ctx context.Context, podName, namespace string) (*corev1.Pod, error) {
	if podName == "" {
		return nil, fmt.Errorf("pod name is empty")
	}
	return shared.GetWorkerPod(ctx, r.Client, podName, namespace)
}

func (r *BuildKitWorkerReconciler) reconcilePending(ctx context.Context, worker *buildkitv1alpha1.BuildKitWorker, pool *buildkitv1alpha1.BuildKitPool, log utils.Logger) (ctrl.Result, error) {
	log.Info("Provisioning worker")

	pod := r.buildWorkerPod(worker, pool)
	if err := utils.SetControllerReference(worker, pod, r.Scheme); err != nil {
		log.Error(err, "Failed to set owner reference")
		return ctrl.Result{}, err
	}

	if err := r.Create(ctx, pod); err != nil {
		if apierrors.IsAlreadyExists(err) {
			return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhaseProvisioning, "Pod created", log)
		}
		log.Error(err, "Failed to create pod")
		return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhaseFailed, fmt.Sprintf("Failed to create pod: %v", err), log)
	}

	worker.Status.PodName = pod.Name
	worker.Status.Phase = buildkitv1alpha1.WorkerPhaseProvisioning
	worker.Status.Message = "Pod created, waiting for ready"
	now := metav1.Now()
	worker.Status.CreatedAt = &now

	if err := r.Status().Update(ctx, worker); err != nil {
		return ctrl.Result{}, err
	}

	return ctrl.Result{RequeueAfter: shared.WorkerProvisioningRequeueInterval}, nil
}

func (r *BuildKitWorkerReconciler) reconcileProvisioning(ctx context.Context, worker *buildkitv1alpha1.BuildKitWorker, pool *buildkitv1alpha1.BuildKitPool, log utils.Logger) (ctrl.Result, error) {
	pod, err := r.getWorkerPod(ctx, worker.Status.PodName, worker.Namespace)
	if err != nil {
		if apierrors.IsNotFound(err) {
			return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhasePending, "Pod not found, recreating", log)
		}
		return ctrl.Result{}, err
	}

	if isPodReady(pod) {
		log.Info("Worker pod is ready")

		worker.Status.Phase = buildkitv1alpha1.WorkerPhaseIdle
		worker.Status.PodIP = pod.Status.PodIP
		worker.Status.Endpoint = shared.GenerateEndpoint(pod.Status.PodIP, resources.DefaultBuildkitPort)
		worker.Status.Message = "Worker ready"
		now := metav1.Now()
		worker.Status.ReadyAt = &now
		worker.Status.LastActivityAt = &now

		if err := r.Status().Update(ctx, worker); err != nil {
			return ctrl.Result{}, err
		}

		return ctrl.Result{}, nil
	}

	if pod.Status.Phase == corev1.PodFailed {
		return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhaseFailed, "Pod failed", log)
	}

	if pod.Status.Phase == corev1.PodPending {
		creationTime := pod.CreationTimestamp.Time
		pendingDuration := time.Since(creationTime)
		maxPendingDuration := 10 * time.Minute

		if pendingDuration > maxPendingDuration {
			for _, cond := range pod.Status.Conditions {
				if cond.Type == corev1.PodScheduled && cond.Status == corev1.ConditionFalse && cond.Reason == "Unschedulable" {
					return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhaseFailed,
						fmt.Sprintf("Pod unschedulable after %v: %s", pendingDuration.Round(time.Minute), cond.Message), log)
				}
			}
			return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhaseFailed,
				fmt.Sprintf("Pod still pending after %v", pendingDuration.Round(time.Minute)), log)
		}

		log.V(1).Info("Pod still pending, waiting for cluster autoscaler", "pendingDuration", pendingDuration.Round(time.Second))
		return ctrl.Result{RequeueAfter: 30 * time.Second}, nil
	}

	// Pod is in another state (ContainerCreating, etc.), requeue normally
	return ctrl.Result{RequeueAfter: shared.WorkerProvisioningRequeueInterval}, nil
}

func (r *BuildKitWorkerReconciler) isAllocationExpired(worker *buildkitv1alpha1.BuildKitWorker) bool {
	return worker.Spec.Allocation != nil &&
		worker.Spec.Allocation.ExpiresAt != nil &&
		time.Now().After(worker.Spec.Allocation.ExpiresAt.Time)
}

func (r *BuildKitWorkerReconciler) calculateRequeueInterval(worker *buildkitv1alpha1.BuildKitWorker) time.Duration {
	requeueAfter := shared.StatusUpdateInterval
	if worker.Spec.Allocation != nil && worker.Spec.Allocation.ExpiresAt != nil {
		timeUntilExpiry := time.Until(worker.Spec.Allocation.ExpiresAt.Time)
		if timeUntilExpiry > 0 && timeUntilExpiry < requeueAfter {
			requeueAfter = timeUntilExpiry + time.Second
		}
	}
	return requeueAfter
}

func (r *BuildKitWorkerReconciler) reconcileRunning(ctx context.Context, worker *buildkitv1alpha1.BuildKitWorker, log utils.Logger) (ctrl.Result, error) {
	if r.isAllocationExpired(worker) {
		log.Info("Worker allocation expired, deleting", "worker", worker.Name, "expiresAt", worker.Spec.Allocation.ExpiresAt.Time)
		if err := r.Delete(ctx, worker); err != nil && !apierrors.IsNotFound(err) {
			return ctrl.Result{}, err
		}
		return ctrl.Result{}, nil
	}

	pod, err := r.getWorkerPod(ctx, worker.Status.PodName, worker.Namespace)
	if err != nil {
		if apierrors.IsNotFound(err) {
			return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhaseFailed, "Pod disappeared", log)
		}
		return ctrl.Result{}, err
	}

	if !isPodReady(pod) {
		return r.updateStatus(ctx, worker, buildkitv1alpha1.WorkerPhaseFailed, "Pod no longer ready", log)
	}

	desiredPhase := buildkitv1alpha1.WorkerPhaseIdle
	if worker.Spec.Allocation != nil {
		desiredPhase = buildkitv1alpha1.WorkerPhaseAllocated
	}
	if worker.Status.Phase != desiredPhase {
		message := "Idle"
		if desiredPhase == buildkitv1alpha1.WorkerPhaseAllocated {
			message = "Allocated to job"
		}
		return r.updateStatus(ctx, worker, desiredPhase, message, log)
	}

	return ctrl.Result{RequeueAfter: r.calculateRequeueInterval(worker)}, nil
}

func (r *BuildKitWorkerReconciler) deleteWorkerPod(ctx context.Context, podName, namespace string, log utils.Logger) error {
	if podName == "" {
		return nil
	}
	pod, err := r.getWorkerPod(ctx, podName, namespace)
	if err != nil {
		return client.IgnoreNotFound(err)
	}
	log.Info("Deleting worker pod", "pod", pod.Name)
	if err := r.Delete(ctx, pod); err != nil {
		return client.IgnoreNotFound(err)
	}
	return nil
}

func (r *BuildKitWorkerReconciler) reconcileTerminating(ctx context.Context, worker *buildkitv1alpha1.BuildKitWorker, log utils.Logger) (ctrl.Result, error) {
	if err := r.deleteWorkerPod(ctx, worker.Status.PodName, worker.Namespace, log); err != nil {
		return ctrl.Result{}, err
	}
	return ctrl.Result{RequeueAfter: shared.WorkerProvisioningRequeueInterval}, nil
}

func (r *BuildKitWorkerReconciler) reconcileDelete(ctx context.Context, worker *buildkitv1alpha1.BuildKitWorker, log utils.Logger) (ctrl.Result, error) {
	log.Info("Deleting worker")

	if err := r.deleteWorkerPod(ctx, worker.Status.PodName, worker.Namespace, log); err != nil {
		return ctrl.Result{}, err
	}

	controllerutil.RemoveFinalizer(worker, workerFinalizer)
	if err := r.Update(ctx, worker); err != nil {
		if apierrors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, err
	}

	return ctrl.Result{}, nil
}

func (r *BuildKitWorkerReconciler) updateStatus(ctx context.Context, worker *buildkitv1alpha1.BuildKitWorker, phase buildkitv1alpha1.WorkerPhase, message string, log utils.Logger) (ctrl.Result, error) {
	worker.Status.Phase = phase
	worker.Status.Message = message

	if err := r.Status().Update(ctx, worker); err != nil {
		log.Error(err, "Failed to update status")
		return ctrl.Result{}, err
	}

	return ctrl.Result{Requeue: true}, nil
}

func (r *BuildKitWorkerReconciler) buildWorkerPod(worker *buildkitv1alpha1.BuildKitWorker, pool *buildkitv1alpha1.BuildKitPool) *corev1.Pod {
	buildkitImage := resources.GetBuildkitImage(pool, "")
	configMapName := resources.GetConfigMapName(pool.Name)

	labels := utils.MergeLabels(
		shared.GetWorkerLabels(pool.Name),
		map[string]string{
			"app.kubernetes.io/name":           "buildkit-worker",
			"app.kubernetes.io/instance":       worker.Name,
			"app.kubernetes.io/managed-by":     "buildkit-controller",
			"buildkit.smrt-devops.net/worker":  worker.Name,
			"buildkit.smrt-devops.net/purpose": "worker",
		},
	)

	workerTLSSecretName := shared.GenerateResourceName(pool.Name, "worker-tls")
	buildkitdContainer := resources.NewBuildkitdContainer(&resources.BuildkitdContainerSpec{
		Image:            buildkitImage,
		ConfigMapName:    configMapName,
		SecretName:       workerTLSSecretName,
		TLSEnabled:       true,
		Resources:        pool.Spec.Resources.Buildkit,
		DefaultResources: "md",
	})

	volumes := resources.BuildVolumes(configMapName, workerTLSSecretName, true)

	return &corev1.Pod{
		ObjectMeta: metav1.ObjectMeta{
			Name:      worker.Name,
			Namespace: worker.Namespace,
			Labels:    labels,
		},
		Spec: corev1.PodSpec{
			Containers:    []corev1.Container{buildkitdContainer},
			Volumes:       volumes,
			RestartPolicy: corev1.RestartPolicyNever,
		},
	}
}

func isPodReady(pod *corev1.Pod) bool {
	if pod.Status.Phase != corev1.PodRunning {
		return false
	}
	for _, cond := range pod.Status.Conditions {
		if cond.Type == corev1.PodReady && cond.Status == corev1.ConditionTrue {
			return true
		}
	}
	return false
}

func (r *BuildKitWorkerReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&buildkitv1alpha1.BuildKitWorker{}).
		Owns(&corev1.Pod{}).
		Complete(r)
}
