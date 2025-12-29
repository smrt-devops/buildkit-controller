---
name: Bug Report
about: Create a report to help us improve
title: "[BUG] "
labels: bug
assignees: ""
---

## Bug Description

A clear and concise description of what the bug is.

## Steps to Reproduce

1. Go to '...'
2. Click on '....'
3. Scroll down to '....'
4. See error

## Expected Behavior

A clear and concise description of what you expected to happen.

## Actual Behavior

A clear and concise description of what actually happened.

## Environment

- **Kubernetes Version**:
- **Controller Version**:
- **Helm Chart Version** (if applicable):
- **Cloud Provider**:
- **OS**:

## Configuration

If applicable, provide relevant configuration:

```yaml
# Your BuildKitPool or BuildKitClientCert configuration
```

## Logs

Please provide relevant logs:

```
# Controller logs
kubectl logs -n buildkit-system -l control-plane=buildkit-controller

# Auth proxy logs (if applicable)
kubectl logs -n buildkit-system -l app=buildkit-pool-<name>
```

## Additional Context

Add any other context about the problem here.

## Screenshots

If applicable, add screenshots to help explain your problem.

## Checklist

- [ ] I have searched existing issues to ensure this bug hasn't been reported
- [ ] I have provided all relevant environment information
- [ ] I have included relevant logs
- [ ] I have tested with the latest version (if applicable)
