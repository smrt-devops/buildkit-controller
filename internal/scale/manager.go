package scale

import (
	"context"
	"fmt"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

// Manager handles scaling operations for BuildKit pools.
type Manager struct {
	client client.Client
	log    utils.Logger
}

// NewManager creates a new scale manager.
func NewManager(c client.Client, log utils.Logger) *Manager {
	return &Manager{
		client: c,
		log:    log,
	}
}

// ScaleToZero scales a pool's deployment to zero replicas.
func (m *Manager) ScaleToZero(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	deployment := &appsv1.Deployment{}
	if err := m.client.Get(ctx, types.NamespacedName{Name: pool.Name, Namespace: pool.Namespace}, deployment); err != nil {
		return fmt.Errorf("failed to get deployment: %w", err)
	}

	if deployment.Spec.Replicas != nil && *deployment.Spec.Replicas == 0 {
		// Already at zero
		return nil
	}

	replicas := int32(0)
	deployment.Spec.Replicas = &replicas

	if err := m.client.Update(ctx, deployment); err != nil {
		return fmt.Errorf("failed to scale to zero: %w", err)
	}

	m.log.Info("Scaled pool to zero", "pool", pool.Name, "namespace", pool.Namespace)
	return nil
}

// ScaleUp scales a pool's deployment to the specified number of replicas.
func (m *Manager) ScaleUp(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, replicas int32) error {
	deployment := &appsv1.Deployment{}
	if err := m.client.Get(ctx, types.NamespacedName{Name: pool.Name, Namespace: pool.Namespace}, deployment); err != nil {
		return fmt.Errorf("failed to get deployment: %w", err)
	}

	if deployment.Spec.Replicas != nil && *deployment.Spec.Replicas >= replicas {
		// Already at or above target
		return nil
	}

	deployment.Spec.Replicas = &replicas

	if err := m.client.Update(ctx, deployment); err != nil {
		return fmt.Errorf("failed to scale up: %w", err)
	}

	m.log.Info("Scaled pool up", "pool", pool.Name, "namespace", pool.Namespace, "replicas", replicas)
	return nil
}

// EnsureMinReplicas ensures the pool has at least the specified number of replicas.
func (m *Manager) EnsureMinReplicas(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, minReplicas int32) error {
	deployment := &appsv1.Deployment{}
	if err := m.client.Get(ctx, types.NamespacedName{Name: pool.Name, Namespace: pool.Namespace}, deployment); err != nil {
		return fmt.Errorf("failed to get deployment: %w", err)
	}

	current := int32(0)
	if deployment.Spec.Replicas != nil {
		current = *deployment.Spec.Replicas
	}

	if current >= minReplicas {
		return nil
	}

	deployment.Spec.Replicas = &minReplicas

	if err := m.client.Update(ctx, deployment); err != nil {
		return fmt.Errorf("failed to ensure min replicas: %w", err)
	}

	m.log.Info("Ensured minimum replicas", "pool", pool.Name, "namespace", pool.Namespace, "replicas", minReplicas)
	return nil
}

// ShouldScaleToZero determines if a pool should be scaled to zero based on idle timeout.
func (m *Manager) ShouldScaleToZero(pool *buildkitv1alpha1.BuildKitPool) bool {
	// Check if scale-to-zero is enabled (Min must be 0)
	if pool.Spec.Scaling.Min != nil && *pool.Spec.Scaling.Min > 0 {
		return false
	}

	// Check if pool has been idle long enough
	idleTimeout := 5 * time.Minute
	if pool.Spec.Scaling.ScaleDownDelay != "" {
		if parsed, err := time.ParseDuration(pool.Spec.Scaling.ScaleDownDelay); err == nil {
			idleTimeout = parsed
		}
	}

	if pool.Status.LastActivityTime == nil {
		return false
	}

	return time.Since(pool.Status.LastActivityTime.Time) > idleTimeout
}

// RecordActivity updates the pool's last activity time.
func (m *Manager) RecordActivity(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool) error {
	// This would update the pool status with the current time
	// Implementation depends on how status updates are handled
	return nil
}
