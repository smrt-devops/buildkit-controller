# Naming Conventions

This document defines the naming conventions used throughout the BuildKit Controller project.

## Standard Naming

**Primary Name:** `buildkit-controller`

All resources use this consistent naming:

- **Chart Name:** `buildkit-controller`
- **Deployment Name:** `buildkit-controller`
- **ServiceAccount Name:** `buildkit-controller`
- **ClusterRole Name:** `buildkit-controller`
- **ClusterRoleBinding Name:** `buildkit-controller`
- **Container Name:** `manager` (standard kubebuilder convention)
- **Label:** `control-plane: buildkit-controller`
- **Leader Election ID:** `buildkit-controller.smrt-devops.net`
- **Image Name:** `buildkit-controller` (default)

## Resource Naming Pattern

### Kubernetes Resources

- **Deployment:** `buildkit-controller`
- **Service (Metrics):** `buildkit-controller-metrics`
- **Service (API):** `buildkit-controller-api`
- **ServiceAccount:** `buildkit-controller`
- **ClusterRole:** `buildkit-controller`
- **ClusterRoleBinding:** `buildkit-controller`
- **Namespace:** `buildkit-system` (configurable via Helm)

### Labels

Standard labels applied to all resources:

```yaml
app.kubernetes.io/name: buildkit-controller
app.kubernetes.io/instance: <release-name>
app.kubernetes.io/version: <chart-version>
app.kubernetes.io/managed-by: Helm
control-plane: buildkit-controller
```

### CRDs

- **API Group:** `buildkit.smrt-devops.net`
- **Resources:**
  - `buildkitpools`
  - `buildkitworkers`
  - `buildkitoidcconfigs`

## Deprecated Names

The following names are deprecated and should not be used:

- ❌ `buildkit-operator`
- ❌ `controller-manager`
- ❌ `manager-role`
- ❌ `manager-rolebinding`

## Helm Chart

The Helm chart uses consistent naming via helpers:

- `{{ include "buildkit-controller.fullname" . }}` - Full resource name
- `{{ include "buildkit-controller.name" . }}` - Base name
- `{{ include "buildkit-controller.namespace" . }}` - Namespace

## Container Image

Default image naming:

- **Controller:** `ghcr.io/smrt-devops/buildkit-controller/controller:latest`
- **Gateway:** `ghcr.io/smrt-devops/buildkit-controller/gateway:latest`

Both are configurable via Helm values.
