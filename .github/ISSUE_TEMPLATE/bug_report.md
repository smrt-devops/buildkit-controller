---
name: Bug report
about: Report a problem with the controller
title: "[BUG] "
labels: bug
assignees: ""
---

## Description

Describe the bug.

## Steps to reproduce

1. The command or workflow you ran.
2. The next step.
3. The error you saw.

## Expected behavior

Describe what you expected to happen.

## Actual behavior

Describe what happened.

## Environment

- Kubernetes version:
- Controller version:
- Helm chart version (if used):
- Cloud provider:
- OS:

## Configuration

Paste the relevant `BuildKitPool` spec and Helm values.

```yaml
# Your BuildKitPool configuration
```

## Logs

Paste the controller logs. Remove tokens and certificates first.

```
kubectl logs -n buildkit-system -l app.kubernetes.io/name=buildkit-controller

# Worker pod logs, if a worker is involved
kubectl logs -n <namespace> <pool>-<slot>
```

## Additional context

Add anything else that helps, such as screenshots.

## Checklist

- [ ] I searched existing issues and found no duplicate
- [ ] I filled in the environment section
- [ ] I included logs
- [ ] I tested the latest release
