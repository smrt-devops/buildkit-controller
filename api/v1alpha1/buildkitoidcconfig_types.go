package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// BuildKitOIDCConfigSpec defines the desired state of BuildKitOIDCConfig.
type BuildKitOIDCConfigSpec struct {
	// Issuer is the OIDC issuer URL
	// +kubebuilder:validation:Required
	Issuer string `json:"issuer"`

	// Audience is the expected audience for tokens
	// +kubebuilder:validation:Required
	Audience string `json:"audience"`

	// ClaimsMapping maps OIDC claims to user/pool information
	ClaimsMapping ClaimsMapping `json:"claimsMapping,omitempty"`

	// ClientID is the OAuth2 client ID (optional, for token exchange)
	ClientID string `json:"clientID,omitempty"`

	// ClientSecretRef is a reference to a secret containing the OAuth2 client secret
	ClientSecretRef *SecretReference `json:"clientSecretRef,omitempty"`

	// Enabled controls whether this OIDC configuration is active
	// +kubebuilder:default=true
	Enabled bool `json:"enabled,omitempty"`
}

// SecretReference references a Kubernetes secret.
type SecretReference struct {
	// Name is the name of the secret
	Name string `json:"name"`

	// Namespace is the namespace of the secret (defaults to same namespace as config)
	Namespace string `json:"namespace,omitempty"`

	// Key is the key in the secret (defaults to "client-secret")
	Key string `json:"key,omitempty"`
}

// BuildKitOIDCConfigStatus defines the observed state of BuildKitOIDCConfig.
type BuildKitOIDCConfigStatus struct {
	// Ready indicates whether the OIDC configuration is ready
	Ready bool `json:"ready,omitempty"`

	// Message is a human-readable status message
	Message string `json:"message,omitempty"`

	// Conditions represent the latest available observations
	Conditions []metav1.Condition `json:"conditions,omitempty"`
}

//+kubebuilder:object:root=true
//+kubebuilder:subresource:status
//+kubebuilder:printcolumn:name="Issuer",type="string",JSONPath=".spec.issuer"
//+kubebuilder:printcolumn:name="Audience",type="string",JSONPath=".spec.audience"
//+kubebuilder:printcolumn:name="Enabled",type="boolean",JSONPath=".spec.enabled"
//+kubebuilder:printcolumn:name="Ready",type="boolean",JSONPath=".status.ready"
//+kubebuilder:printcolumn:name="Age",type="date",JSONPath=".metadata.creationTimestamp"

// BuildKitOIDCConfig is the Schema for the buildkitoidcconfigs API.
// This is a cluster-scoped resource for configuring OIDC authentication.
type BuildKitOIDCConfig struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   BuildKitOIDCConfigSpec   `json:"spec,omitempty"`
	Status BuildKitOIDCConfigStatus `json:"status,omitempty"`
}

//+kubebuilder:object:root=true

// BuildKitOIDCConfigList contains a list of BuildKitOIDCConfig.
type BuildKitOIDCConfigList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []BuildKitOIDCConfig `json:"items"`
}

func init() {
	SchemeBuilder.Register(&BuildKitOIDCConfig{}, &BuildKitOIDCConfigList{})
}
