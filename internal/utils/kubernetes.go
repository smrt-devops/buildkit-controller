package utils

import (
	"context"
	"fmt"
	"maps"

	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
)

// SetOwnerReference sets the owner reference on an object and updates it.
func SetOwnerReference(ctx context.Context, k8sClient client.Client, owner, obj metav1.Object, scheme *runtime.Scheme) error {
	// Check if owner references are already set
	if len(obj.GetOwnerReferences()) > 0 {
		// Owner references are already set, skip update
		return nil
	}

	if err := controllerutil.SetControllerReference(owner, obj, scheme); err != nil {
		return fmt.Errorf("failed to set owner reference: %w", err)
	}
	clientObj, ok := obj.(client.Object)
	if !ok {
		return fmt.Errorf("object does not implement client.Object")
	}
	if err := k8sClient.Update(ctx, clientObj); err != nil {
		return fmt.Errorf("failed to update object with owner reference: %w", err)
	}
	return nil
}

// CreateOrUpdate creates or updates a Kubernetes resource.
// It only updates when the object actually changes.
// The desired object is passed in - controllerutil.CreateOrUpdate will handle the comparison.
func CreateOrUpdate(ctx context.Context, k8sClient client.Client, desiredObj client.Object) error {
	// Store desired state before controllerutil.CreateOrUpdate GETs the existing object
	desiredCopy := desiredObj.DeepCopyObject()

	// controllerutil.CreateOrUpdate will:
	// 1. GET the existing object (using key from desiredObj) - this overwrites desiredObj!
	// 2. Call mutate function - we copy desired state into the existing object
	// 3. Compare before/after and only update if changed
	operation, err := controllerutil.CreateOrUpdate(ctx, k8sClient, desiredObj, func() error {
		// At this point, desiredObj contains the existing object (or empty if new)
		// We need to copy the desired state (spec/data) from desiredCopy into it
		// while preserving metadata (resourceVersion, generation, etc.)
		return copyDesiredState(desiredObj, desiredCopy)
	})

	// Log only if something actually changed (for debugging)
	if operation == controllerutil.OperationResultUpdated {
		// Resource was updated - this is expected when changes occur
	} else if operation == controllerutil.OperationResultCreated {
		// Resource was created - this is expected for new resources
	} else if operation == controllerutil.OperationResultNone {
		// No changes - this is the desired state for idempotent operations
	}

	return err
}

// copyDesiredState copies the desired state (spec/data) from desiredCopy into obj.
// It preserves metadata (resourceVersion, generation, etc.) from obj.
func copyDesiredState(obj client.Object, desiredCopy runtime.Object) error {
	// Convert both to unstructured for generic copying
	objUnstructured, err := runtime.DefaultUnstructuredConverter.ToUnstructured(obj.(runtime.Object))
	if err != nil {
		return fmt.Errorf("failed to convert obj to unstructured: %w", err)
	}

	desiredUnstructured, err := runtime.DefaultUnstructuredConverter.ToUnstructured(desiredCopy)
	if err != nil {
		return fmt.Errorf("failed to convert desired to unstructured: %w", err)
	}

	// Preserve metadata from existing object
	metadata := objUnstructured["metadata"].(map[string]interface{})

	// Copy spec and data from desired
	if spec, ok := desiredUnstructured["spec"]; ok {
		objUnstructured["spec"] = spec
	}
	if data, ok := desiredUnstructured["data"]; ok {
		objUnstructured["data"] = data
	}
	if stringData, ok := desiredUnstructured["stringData"]; ok {
		objUnstructured["stringData"] = stringData
	}

	// Restore preserved metadata
	objUnstructured["metadata"] = metadata

	// Convert back to typed object
	if err := runtime.DefaultUnstructuredConverter.FromUnstructured(objUnstructured, obj.(runtime.Object)); err != nil {
		return fmt.Errorf("failed to convert back from unstructured: %w", err)
	}

	return nil
}

// GetSecret retrieves a secret by name and namespace.
func GetSecret(ctx context.Context, k8sClient client.Client, name, namespace string) (*corev1.Secret, error) {
	secret := &corev1.Secret{}
	key := types.NamespacedName{Name: name, Namespace: namespace}
	if err := k8sClient.Get(ctx, key, secret); err != nil {
		return nil, fmt.Errorf("failed to get secret %s/%s: %w", namespace, name, err)
	}
	return secret, nil
}

// GetConfigMap retrieves a configmap by name and namespace.
func GetConfigMap(ctx context.Context, k8sClient client.Client, name, namespace string) (*corev1.ConfigMap, error) {
	configMap := &corev1.ConfigMap{}
	key := types.NamespacedName{Name: name, Namespace: namespace}
	if err := k8sClient.Get(ctx, key, configMap); err != nil {
		return nil, fmt.Errorf("failed to get configmap %s/%s: %w", namespace, name, err)
	}
	return configMap, nil
}

// ResourceQuantity parses a resource quantity string and returns a Quantity.
// Returns zero quantity on parse error.
func ResourceQuantity(s string) resource.Quantity {
	q, err := resource.ParseQuantity(s)
	if err != nil {
		return resource.Quantity{}
	}
	return q
}

// Int32Ptr returns a pointer to the given int32 value.
func Int32Ptr(i int32) *int32 {
	return &i
}

// Int64Ptr returns a pointer to the given int64 value.
func Int64Ptr(i int64) *int64 {
	return &i
}

// StringPtr returns a pointer to the given string value.
func StringPtr(s string) *string {
	return &s
}

// BoolPtr returns a pointer to the given bool value.
func BoolPtr(b bool) *bool {
	return &b
}

// DefaultLabels returns default labels for BuildKit resources.
func DefaultLabels(component, poolName string) map[string]string {
	return map[string]string{
		"app.kubernetes.io/name":        "buildkit-controller",
		"app.kubernetes.io/component":   component,
		"app.kubernetes.io/managed-by":  "buildkit-controller",
		"buildkit.smrt-devops.net/pool": poolName,
	}
}

// MergeLabels merges multiple label maps, with later maps taking precedence.
func MergeLabels(labelMaps ...map[string]string) map[string]string {
	result := make(map[string]string)
	for _, labels := range labelMaps {
		maps.Copy(result, labels)
	}
	return result
}

// SetControllerReference sets a controller reference on an object without updating it.
// This is a convenience wrapper around controllerutil.SetControllerReference.
func SetControllerReference(owner, obj metav1.Object, scheme *runtime.Scheme) error {
	return controllerutil.SetControllerReference(owner, obj, scheme)
}

// UpdateCondition updates or appends a condition to a condition list.
// If a condition with the same type exists, it's updated. Otherwise, it's appended.
// LastTransitionTime is only updated when the status actually changes.
func UpdateCondition(conditions []metav1.Condition, condition metav1.Condition) []metav1.Condition {
	for i, c := range conditions {
		if c.Type == condition.Type {
			// Only update LastTransitionTime if the status actually changed
			if c.Status == condition.Status {
				// Status hasn't changed, preserve the original LastTransitionTime
				condition.LastTransitionTime = c.LastTransitionTime
			}
			// Update the condition
			conditions[i] = condition
			return conditions
		}
	}
	// New condition, use the provided LastTransitionTime
	return append(conditions, condition)
}

// FindCondition finds a condition by type in a condition list.
func FindCondition(conditions []metav1.Condition, conditionType string) *metav1.Condition {
	for i := range conditions {
		if conditions[i].Type == conditionType {
			return &conditions[i]
		}
	}
	return nil
}
