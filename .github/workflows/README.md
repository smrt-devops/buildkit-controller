# GitHub Actions Workflows

This directory contains GitHub Actions workflows for CI/CD and releases.

## Workflows Overview

### Continuous Integration

#### `test.yml` - Test Suite

**Triggers:**

- Push to `main` branch
- Pull requests to `main`

**Steps:**

1. **Checkout code** - Checks out the repository
2. **Set up Go** - Installs Go using version from `go.mod`
3. **Install controller-gen** - Installs controller-tools for code generation
4. **Generate CRDs** - Generates CustomResourceDefinition objects
5. **Generate DeepCopy methods** - Generates DeepCopy methods for Kubernetes objects
6. **Check code formatting** - Verifies code is formatted with `go fmt`
7. **Run go vet** - Runs static analysis with `go vet`
8. **Run tests** - Executes all Go tests with coverage
9. **Upload coverage report** - Uploads test coverage report as artifact

**Purpose:** Fast feedback on code changes. Runs all code quality checks and tests to ensure code quality before merging.

### Release Management

The release process is fully automated with multiple workflows working together:

#### `release-drafter.yml` - Release Drafter

**Triggers:**

- Push to `main` branch
- Pull request events (opened, reopened, synchronize, closed) to `main`

**Purpose:** Automatically maintains a draft release as PRs are merged. Categorizes changes by labels and resolves version based on PR labels.

**Configuration:** `.github/release-drafter.yml`

#### `prepare-release.yml` - Prepare Release

**Triggers:**

- Manual workflow dispatch

**Inputs:**

- `version` - Release version (e.g., `1.0.0`, `1.0.0-beta.1`)
- `release_type` - Release type (`stable`, `beta`, `alpha`, `rc`)
- `auto_merge` - Auto-merge after checks (optional)

**Purpose:** Creates a release PR with the specified version. Validates version format and checks if tag exists.

**Output:** Creates a pull request with branch `release/v<version>`

#### `release-pr-checks.yml` - Release PR Validation

**Triggers:**

- Pull request events (opened, synchronize, reopened, edited) to `main`
- Only runs on branches starting with `release/`

**Purpose:** Validates release PRs:

- ✅ Version format (semantic versioning)
- ✅ Tag existence check
- ✅ Chart.yaml version validation
- ✅ PR title format check
- ✅ Adds validation comment to PR

#### `auto-tag-release.yml` - Auto Tag Release

**Triggers:**

- Pull request closed (merged) to `main`
- Only runs when PR branch starts with `release/`

**Purpose:** Automatically creates and pushes a git tag when a release PR is merged. Extracts version from branch name and creates tag `v<version>`.

#### `release.yml` - Release Workflow

**Triggers:**

- Push of tags matching `v*` pattern

**Jobs:**

1. `test` - Runs tests before building
2. `build-images` - Builds and pushes Docker images (controller + gateway)
   - Uses Docker Cloud workers for faster builds
   - Multi-arch: `linux/amd64`, `linux/arm64`
   - Creates tags: `v<version>`, `v<major>.<minor>`, `v<major>`, `latest` (stable only)
   - Protected by `release` environment (requires approval)
3. `build-cli` - Builds `bkctl` CLI binaries for Linux and macOS
4. `publish-helm` - Packages and publishes Helm chart to OCI registry
5. `create-release` - Creates GitHub release with all artifacts
   - Uses release notes from Release Drafter (if available)
   - Attaches CLI binaries and Helm chart
   - Protected by `release` environment (requires approval)

**Purpose:** Complete release pipeline that builds, tests, and publishes all artifacts.

## Workflow Flow

```
┌─────────────────────┐
│  Push/PR to main    │
└──────┬──────────────┘
       │
       ├─────────────────┐
       │                 │
       ▼                 ▼
┌─────────────┐  ┌──────────────────┐
│  test.yml   │  │ release-drafter  │
│  (tests)    │  │ (draft release)  │
└─────────────┘  └──────────────────┘

┌─────────────────────┐
│  Prepare Release    │
│  (workflow_dispatch)│
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│  Create Release PR  │
│  (release/v1.0.0)    │
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│  Release PR Checks  │
│  (validation)       │
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│  Review & Approve   │
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│  Merge PR           │
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│  Auto Tag Release   │
│  (creates tag)      │
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│  Release Workflow   │
│                     │
│  test →             │
│  build-images →     │
│  build-cli →        │
│  publish-helm →     │
│  create-release     │
└─────────────────────┘
```

## Creating a Release

### Using the Prepare Release Workflow (Recommended)

1. Go to **Actions** → **Prepare Release**
2. Click **Run workflow**
3. Fill in:
   - **Version**: `1.0.0` (or `1.0.0-beta.1` for pre-release)
   - **Release type**: `stable`, `beta`, `alpha`, or `rc`
4. Click **Run workflow**

This creates a release PR that will be validated automatically.

### Manual Release PR Creation

1. Create a branch: `release/v1.0.0`
2. Create a PR to `main` with title: `Release v1.0.0`
3. The Release PR Checks workflow will validate it automatically

### After Merging Release PR

Once the release PR is merged:

1. Auto Tag Release workflow creates tag `v1.0.0`
2. Release workflow is triggered by the tag push
3. All artifacts are built and published
4. GitHub release is created

## Pre-releases

Pre-releases (alpha, beta, rc) are fully supported:

```bash
# Beta release
Version: 1.0.0-beta.1
Release type: beta

# Release candidate
Version: 1.0.0-rc.1
Release type: rc
```

Pre-releases:

- Are marked as pre-release in GitHub
- Do NOT get `latest` tag on Docker images
- Follow the same process as stable releases

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

## Environment Protection

The release workflow uses a protected environment called `release`:

- Requires approval before building images and creating releases
- Configure in: **Settings** → **Environments** → **release**
- Add required reviewers (maintainers who can approve releases)
- Restrict to `main` branch only

## Permissions

- **CI workflow**: `contents: read` (for checkout)
- **Release workflows**: `contents: write`, `packages: write`, `pull-requests: write`
- These are automatically granted via `GITHUB_TOKEN` for public repositories

## Release Drafter

The Release Drafter automatically maintains draft releases:

- Categorizes changes by PR labels
- Resolves version based on PR labels (`major`, `minor`, `patch`)
- Updates automatically when PRs are merged
- Draft release is used when creating the final release

### Labeling PRs

Label PRs appropriately for proper categorization:

- `feature` or `enhancement` → Features
- `fix`, `bugfix`, or `bug` → Bug Fixes
- `documentation` or `docs` → Documentation
- `chore`, `maintenance`, or `dependencies` → Maintenance
- `refactor` → Refactoring
- `performance` → Performance
- `test` or `testing` → Testing

## Documentation

For detailed step-by-step instructions, see:

- [Release Process Documentation](../RELEASE_PROCESS.md)

## Troubleshooting

### Release PR Checks Not Running

- Ensure branch name starts with `release/`
- Check workflow permissions in repository settings

### Tag Already Exists

- Check if version was already released
- Use a new version number or increment pre-release suffix

### Environment Protection Blocking

- Go to **Actions** tab
- Find the waiting workflow
- Click **Review deployments**
- Approve the deployment

For more troubleshooting tips, see [RELEASE_PROCESS.md](../RELEASE_PROCESS.md#troubleshooting).
