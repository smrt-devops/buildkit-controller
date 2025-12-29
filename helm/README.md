# BuildKit Controller Helm Chart

This directory contains the Helm chart for deploying the BuildKit Controller.

## Chart Structure

```
helm/buildkit-controller/
├── Chart.yaml              # Chart metadata
├── values.yaml             # Default values
├── values-production.yaml  # Production values
├── values-dev.yaml         # Development values
├── README.md               # Chart documentation
├── crds-source/            # CRD source files (used by templates)
│   ├── buildkit.smrt-devops.net_buildkitpools.yaml
│   ├── buildkit.smrt-devops.net_buildkitworkers.yaml
│   └── buildkit.smrt-devops.net_buildkitoidcconfigs.yaml
└── templates/              # Kubernetes manifests
    ├── namespace.yaml
    ├── serviceaccount.yaml
    ├── clusterrole.yaml
    ├── clusterrolebinding.yaml
    ├── deployment.yaml
    ├── service.yaml
    ├── crds.yaml
    ├── oidc-configs.yaml
    ├── NOTES.txt
    └── _helpers.tpl
```

## Quick Install

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace
```

## Features

- ✅ Automatic CRD installation
- ✅ Configurable OIDC setup
- ✅ Production-ready defaults
- ✅ Resource limits and requests
- ✅ Health probes
- ✅ Leader election
- ✅ Service accounts and RBAC
- ✅ Namespace management

## Configuration

See [values.yaml](buildkit-controller/values.yaml) for all configuration options.

Key settings:

- `image.repository` and `image.tag` - Controller image
- `controller.replicas` - Number of replicas
- `controller.resources` - CPU/memory limits
- `oidc.configs` - OIDC provider configurations
- `crds.install` - Whether to install CRDs

## Examples

### Basic Installation

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace
```

### Custom Image

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace \
  --set image.repository=my-registry/buildkit-controller \
  --set image.tag=v1.0.0
```

### With OIDC

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --create-namespace \
  --set oidc.configs[0].name=github-actions \
  --set oidc.configs[0].issuer=https://token.actions.githubusercontent.com \
  --set oidc.configs[0].audience=buildkit-controller \
  --set oidc.configs[0].enabled=true
```

### Production Deployment

```bash
helm install buildkit-controller ./helm/buildkit-controller \
  -f helm/buildkit-controller/values-production.yaml \
  --namespace buildkit-system \
  --create-namespace
```

## Upgrading

```bash
helm upgrade buildkit-controller ./helm/buildkit-controller \
  --namespace buildkit-system \
  --set image.tag=v1.1.0
```

## Uninstalling

```bash
# Remove controller (CRDs removed by default)
helm uninstall buildkit-controller --namespace buildkit-system

# Keep CRDs when uninstalling
helm uninstall buildkit-controller --namespace buildkit-system \
  --set crds.keep=true
```

## Documentation

- [Chart README](buildkit-controller/README.md) - Detailed chart documentation
- [Main README](../README.md) - Project documentation
- [Deployment Guide](../DEPLOYMENT.md) - Deployment instructions
- [Quick Start](../QUICKSTART.md) - Quick start guide
