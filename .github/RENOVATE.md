# Renovate Configuration

This repository uses [Renovate](https://github.com/renovatebot/renovate) for automated dependency management.

## Features

- **Automated dependency updates** for Go modules, GitHub Actions, Docker images, and Helm charts
- **Grouped updates** by ecosystem (Go, GitHub Actions, Docker, etc.)
- **Semantic versioning** support
- **Vulnerability alerts** with security labels
- **Auto-merge** for minor and patch updates (configurable)
- **Weekly schedule** to reduce PR noise

## Configuration

The configuration is in `renovate.json` at the repository root. Key features:

### Update Groups

- **Go dependencies**: Grouped together, updates on Mondays
- **GitHub Actions**: Separate group for CI/CD updates
- **Docker images**: Base image updates grouped
- **Helm charts**: Chart dependency updates
- **Kubernetes dependencies**: Special grouping for k8s.io and sigs.k8s.io packages

### Auto-merge

Minor and patch updates are automatically merged after CI passes. Major updates require manual review.

### Vulnerability Alerts

Security vulnerabilities are automatically flagged with the `security` label and require manual review.

## Setup

1. Install the [Renovate App](https://github.com/apps/renovate) from GitHub Marketplace
2. Grant access to this repository
3. Renovate will automatically create a PR to enable itself (if not already configured)
4. The configuration in `renovate.json` will be used automatically

## Dependabot vs Renovate

This repository is configured for Renovate, which offers:

- More flexible configuration options
- Better grouping and scheduling
- Support for more package managers
- Advanced auto-merge rules
- Better integration with semantic versioning

If you prefer Dependabot, you can:

1. Disable Renovate in repository settings
2. Keep the `.github/dependabot.yml` configuration
3. Enable Dependabot in repository settings

Both can coexist, but it's recommended to use one to avoid duplicate PRs.

## Customization

To customize Renovate behavior, edit `renovate.json`. Common customizations:

- Change update schedule: Modify the `schedule` field
- Disable auto-merge: Set `automerge: false`
- Add more package managers: Add entries to `packageRules`
- Change PR limits: Adjust `prConcurrentLimit` and `prHourlyLimit`

See [Renovate documentation](https://docs.renovatebot.com/) for full configuration options.
