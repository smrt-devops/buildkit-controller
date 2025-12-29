# GitHub Actions Workflows

This directory contains GitHub Actions workflows for CI/CD and releases.

## Workflows

### `ci.yml` - Continuous Integration

**Triggers:**

- Push to `main` branch
- Pull requests to `main`

**Jobs:**

- `test` - Runs Go tests, linting, and code generation

**Purpose:** Fast feedback on code changes. Runs only tests to keep CI fast.

### `release.yml` - Release Process

**Triggers:**

- Tags matching `v*` pattern (e.g., `v1.0.0`, `v1.2.3-beta.1`)

**Jobs:**

1. `test` - Runs tests before building
2. `build-images` - Builds and pushes Docker images (controller + gateway) using Docker Cloud workers
3. `build-cli` - Builds `bkctl` CLI binaries for Linux and macOS
4. `publish-helm` - Packages and publishes Helm chart to OCI registry
5. `create-release` - Creates GitHub release with all artifacts

**Purpose:** Complete release pipeline that builds, tests, and publishes all artifacts.

## Workflow Structure

```
┌─────────────┐
│   Push/PR   │
└──────┬──────┘
       │
       ▼
┌─────────────┐
│   ci.yml    │
│  (tests)    │
└─────────────┘

┌─────────────┐
│  Tag (v*)   │
└──────┬──────┘
       │
       ▼
┌─────────────────┐
│  release.yml     │
│                  │
│  test →          │
│  build-images →  │
│  build-cli →     │
│  publish-helm →  │
│  create-release  │
└─────────────────┘
```

## Creating a Release

### Stable Release

```bash
# 1. Update version in code/docs if needed
# 2. Create and push tag
git tag v1.0.0
git push origin v1.0.0
```

This will:

- Run tests
- Build and push Docker images with version tags
- Build CLI binaries
- Publish Helm chart
- Create GitHub release

### Pre-release (Alpha/Beta/RC)

```bash
git tag v1.0.0-beta.1
git push origin v1.0.0-beta.1
```

Pre-releases are automatically detected and marked in the GitHub release.

## Image Tags

Images are tagged with:

- `v1.2.3` - Exact version from tag
- `v1.2` - Major.minor version
- `v1` - Major version
- `latest` - Only for stable releases (not pre-releases)

## Registry

- **Images**: `ghcr.io/<repository>/controller` and `ghcr.io/<repository>/gateway`
- **Helm Chart**: `oci://ghcr.io/<repository>/charts/buildkit-controller`

## Docker Cloud Workers

The release workflow uses Docker Cloud workers for faster builds. Requires:

- `vars.DOCKER_USER` - Docker Hub username
- `secrets.DOCKER_PAT` - Docker Personal Access Token
- Docker Cloud builder endpoint: `${{ vars.DOCKER_USER }}/builder-1`

## Permissions

- **CI workflow**: `contents: read` (for checkout)
- **Release workflow**: `contents: read`, `packages: write`, `contents: write` (for releases)

These are automatically granted via `GITHUB_TOKEN` for public repositories.
