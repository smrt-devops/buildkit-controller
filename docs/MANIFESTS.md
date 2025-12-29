# Manifest Generation

This document explains how manifests are generated and managed in the BuildKit Controller project.

## Generated Manifests

The following manifests are **automatically generated** by `make manifests`:

### CRDs (Custom Resource Definitions)

- Location: `helm/buildkit-controller/crds/`
- Generated from: Go type definitions in `api/v1alpha1/`
- Command: `make manifests`
- Files:
  - `buildkit.smrt-devops.net_buildkitpools.yaml`
  - `buildkit.smrt-devops.net_buildkitworkers.yaml`
  - `buildkit.smrt-devops.net_buildkitoidcconfigs.yaml`

**Note:** These files are generated and should not be edited manually. Changes should be made in the Go source code with appropriate Kubebuilder markers. Helm automatically installs CRDs from the `crds/` directory.

### RBAC (Role-Based Access Control)

- Location: `helm/buildkit-controller/templates/`
- Manually maintained: RBAC is handled by Helm chart templates
- Files:
  - `clusterrole.yaml` - ClusterRole with permissions
  - `clusterrolebinding.yaml` - ClusterRoleBinding
  - `serviceaccount.yaml` - ServiceAccount

## Static Manifests

The following manifests are **manually maintained**:

### Manager Deployment

- Location: `config/manager/manager.yaml`
- Purpose: Base deployment configuration for Kustomize
- Used by: `make deploy` (Kustomize-based deployment)

### Kustomization Files

- Location: `config/*/kustomization.yaml`
- Purpose: Kustomize configuration for building manifests
- Used by: `make deploy`

## Helm Chart

The **Helm chart** (recommended deployment method) uses:

- **CRDs:** Generated directly to `helm/buildkit-controller/crds/` (Helm automatically installs CRDs from this directory)
- **Templates:** Manually maintained in `helm/buildkit-controller/templates/`
- **Values:** Manually maintained in `helm/buildkit-controller/values*.yaml`

### Updating Helm Chart CRDs

When CRDs are regenerated, they are automatically generated to the Helm chart:

```bash
# Regenerate CRDs (generates directly to helm/buildkit-controller/crds/)
make manifests
```

## Workflow

### Development Workflow

1. **Modify Go types** in `api/v1alpha1/`
2. **Regenerate CRDs:**
   ```bash
   make manifests
   ```
   This generates CRDs directly to `helm/buildkit-controller/crds/`
3. **Commit changes** (both generated and source files)

### Deployment Workflow

**Recommended:** Use Helm chart

```bash
helm install buildkit-controller ./helm/buildkit-controller
```

**Alternative:** Use Kustomize

```bash
make deploy
```

## What Gets Generated

### From Go Code

- **CRD schemas** - From struct definitions and Kubebuilder markers
- **RBAC rules** - From `//+kubebuilder:rbac` markers
- **DeepCopy methods** - From `//+kubebuilder:object:root` markers

### Kubebuilder Markers

Common markers used:

```go
//+kubebuilder:object:root=true
//+kubebuilder:subresource:status
//+kubebuilder:rbac:groups=buildkit.smrt-devops.net,resources=buildkitpools,verbs=get;list;watch
//+kubebuilder:printcolumn:name="Phase",type="string",JSONPath=".status.phase"
```

## Best Practices

1. **Never edit generated files manually** - They will be overwritten
2. **Commit generated files** - They're part of the source of truth
3. **Use Helm for deployment** - More flexible and user-friendly
4. **Keep CRDs in sync** - Update Helm chart CRDs after regeneration
5. **Review generated RBAC** - Ensure permissions are correct

## Troubleshooting

### Manifests Not Updating

```bash
# Clean and regenerate
rm -rf helm/buildkit-controller/crds/*.yaml
make manifests
```

### RBAC Not Generated

Check for Kubebuilder markers in controller code:

```bash
grep -r "kubebuilder:rbac" internal/controller/
```

### CRD Validation Errors

Check Go type definitions:

```bash
make generate  # Generates DeepCopy methods
make manifests # Generates CRDs
```
