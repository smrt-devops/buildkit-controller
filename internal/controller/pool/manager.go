package pool

import (
	"context"
	"fmt"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	buildkitv1alpha1 "github.com/smrt-devops/buildkit-controller/api/v1alpha1"
	"github.com/smrt-devops/buildkit-controller/internal/config"
	"github.com/smrt-devops/buildkit-controller/internal/resources"
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

func (r *Manager) reconcileResource(ctx context.Context, obj client.Object, resourceType string, pool *buildkitv1alpha1.BuildKitPool) error {
	if err := utils.SetControllerReference(pool, obj, r.scheme); err != nil {
		return fmt.Errorf("failed to set owner reference on %s: %w", resourceType, err)
	}

	if err := utils.CreateOrUpdate(ctx, r.client, obj); err != nil {
		return fmt.Errorf("failed to create or update %s: %w", resourceType, err)
	}

	return nil
}

func (r *Manager) Reconcile(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	return r.reconcileConfigMap(ctx, pool, namespace)
}

func (r *Manager) reconcileConfigMap(ctx context.Context, pool *buildkitv1alpha1.BuildKitPool, namespace string) error {
	configMapName := resources.GetConfigMapName(pool.Name)

	buildkitConfig := pool.Spec.BuildkitConfig
	if buildkitConfig == "" {
		buildkitConfig = config.GenerateBuildkitConfig(pool)
	}

	configMap := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{
			Name:      configMapName,
			Namespace: namespace,
			Labels:    resources.GetConfigMapLabels(pool.Name),
		},
		Data: map[string]string{
			"buildkitd.toml": buildkitConfig,
		},
	}

	if err := r.reconcileResource(ctx, configMap, "ConfigMap", pool); err != nil {
		return err
	}

	r.log.Info("Reconciled ConfigMap", "configmap", configMapName)
	return nil
}
