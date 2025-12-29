# OIDC Setup Guide

This guide explains how to configure OIDC authentication for the BuildKit controller.

## Overview

OIDC (OpenID Connect) is used to authenticate certificate requests via the HTTP API. Once authenticated, clients receive mTLS certificates which they use to connect to BuildKit pools.

## Architecture

```
┌─────────────┐
│   Client    │
│ (GitHub CI) │
└──────┬──────┘
       │ 1. Get OIDC Token
       ▼
┌─────────────────────┐
│  HTTP API Server    │
│  (Port 8082)        │
│                     │
│  2. Verify OIDC     │
│  3. Allocate Worker │
│  4. Issue Cert      │
│     (with token)    │
└──────┬──────────────┘
       │ 5. Return Certificates + Gateway Endpoint
       ▼
┌─────────────┐
│   Client    │
│             │
│  6. Connect │
│  with mTLS  │
└──────┬──────┘
       │
       ▼
┌─────────────────┐
│  Pool Gateway   │
│  (Routes to     │
│   Worker)       │
└──────┬──────────┘
       │
       ▼
┌─────────────────┐
│  Ephemeral      │
│  Worker Pod     │
└─────────────────┘
```

## Configuration Methods

### Method 1: BuildKitOIDCConfig CRD (Recommended)

Create a dedicated OIDC configuration resource:

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: github-actions-oidc
  namespace: buildkit-system
spec:
  issuer: https://token.actions.githubusercontent.com
  audience: buildkit-controller
  enabled: true
  claimsMapping:
    user: "actor" # Which claim contains user identity
    pools: "repository" # Which claim contains pool access (optional)
```

**Benefits:**

- Centralized configuration
- Can be managed via GitOps
- Supports multiple OIDC providers
- Easy to enable/disable

### Method 2: Per-Pool Configuration (Legacy)

Configure OIDC in individual pools:

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata:
  name: my-pool
spec:
  auth:
    methods:
      - type: oidc
        oidc:
          issuer: https://token.actions.githubusercontent.com
          audience: buildkit-controller
          claimsMapping:
            user: "actor"
            pools: "repository"
```

**Note:** This method is supported for backward compatibility but BuildKitOIDCConfig is preferred.

## GitHub Actions Setup

### 1. Configure OIDC in Controller

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: github-actions
  namespace: buildkit-system
spec:
  issuer: https://token.actions.githubusercontent.com
  audience: buildkit-controller
  enabled: true
  claimsMapping:
    user: "actor"
```

### 2. Configure GitHub Actions Workflow

```yaml
name: Build
on:
  push:
    branches: [main]

jobs:
  build:
    runs-on: ubuntu-latest
    permissions:
      id-token: write # Required for OIDC
      contents: read

    steps:
      - name: Get OIDC Token
        id: oidc
        uses: actions/github-script@v6
        with:
          script: |
            const token = await core.getIDToken('buildkit-controller')
            core.setOutput('token', token)

      - name: Allocate Worker and Get Certificates
        id: allocate
        run: |
          curl -X POST http://buildkit-controller.buildkit-system.svc:8082/api/v1/workers/allocate \
            -H "Authorization: Bearer ${{ steps.oidc.outputs.token }}" \
            -H "Content-Type: application/json" \
            -d '{"poolName": "my-pool", "namespace": "default", "ttl": "1h"}' \
            > alloc.json
          jq -r '.caCert' alloc.json | base64 -d > ca.crt
          jq -r '.clientCert' alloc.json | base64 -d > client.crt
          jq -r '.clientKey' alloc.json | base64 -d > client.key
          echo "gateway=$(jq -r '.gatewayEndpoint' alloc.json)" >> $GITHUB_OUTPUT
```

## Other OIDC Providers

### GitLab CI

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: gitlab-oidc
  namespace: buildkit-system
spec:
  issuer: https://gitlab.com
  audience: buildkit-controller
  enabled: true
  claimsMapping:
    user: "sub" # GitLab user ID
```

### Generic OIDC Provider

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: custom-oidc
  namespace: buildkit-system
spec:
  issuer: https://your-oidc-provider.com
  audience: buildkit-controller
  enabled: true
  claimsMapping:
    user: "email" # Custom claim for user identity
    pools: "groups" # Custom claim for pool access
```

## Claims Mapping

The `claimsMapping` field tells the controller which OIDC claims to use:

- **user**: Claim that contains the user identity (default: "sub")
- **pools**: Claim that contains pool access information (optional)

### Example Claims

**GitHub Actions:**

```json
{
  "sub": "repo:owner/repo:ref:refs/heads/main",
  "actor": "username",
  "repository": "owner/repo",
  "aud": "buildkit-controller"
}
```

**Generic OIDC:**

```json
{
  "sub": "user-123",
  "email": "user@example.com",
  "groups": ["developers", "ci-users"],
  "aud": "buildkit-controller"
}
```

## Verification

### Test OIDC Configuration

1. **Check OIDC Config Status:**

   ```bash
   kubectl get buildkitoidcconfig -n buildkit-system
   kubectl describe buildkitoidcconfig github-actions -n buildkit-system
   ```

2. **Check Controller Logs:**

   ```bash
   kubectl logs -l control-plane=buildkit-controller -n buildkit-system | grep oidc
   ```

3. **Test Token Verification:**

   ```bash
   # Get a test token (from your OIDC provider)
   TOKEN="your-oidc-token"

   # Test API endpoint
   curl -X POST http://buildkit-controller:8082/api/v1/certs/request \
     -H "Authorization: Bearer $TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"pools": ["test-pool"]}'
   ```

## Troubleshooting

### "No valid OIDC configuration found"

- Verify BuildKitOIDCConfig exists: `kubectl get buildkitoidcconfig`
- Check if enabled: `kubectl get buildkitoidcconfig -o yaml | grep enabled`
- Verify issuer URL is correct and accessible
- Check controller logs for OIDC errors

### "Token verification failed"

- Verify token audience matches configuration
- Check token expiry
- Verify issuer URL matches token issuer
- Check controller can reach OIDC provider (network policies, etc.)

### "Failed to create OIDC verifier"

- Verify issuer URL is accessible from controller
- Check DNS resolution
- Verify network policies allow outbound connections
- Check controller logs for detailed error

## Security Considerations

1. **Audience Validation**: Always set a specific audience to prevent token reuse
2. **Issuer Verification**: Only trust known OIDC issuers
3. **Token Expiry**: Tokens should have reasonable expiry times
4. **Network Policies**: Restrict API server access to trusted sources
5. **RBAC**: Use Kubernetes RBAC to control who can create OIDC configs

## Multiple OIDC Providers

You can configure multiple OIDC providers by creating multiple BuildKitOIDCConfig resources:

```yaml
# GitHub Actions
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: github-actions
spec:
  issuer: https://token.actions.githubusercontent.com
  audience: buildkit-controller
  # ...

---
# GitLab CI
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitOIDCConfig
metadata:
  name: gitlab-ci
spec:
  issuer: https://gitlab.com
  audience: buildkit-controller
  # ...
```

The controller will try each configured provider until one succeeds.
