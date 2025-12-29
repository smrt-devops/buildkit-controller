# Release Process

This document describes the automated release process for BuildKit Controller with step-by-step instructions.

## Overview

The release process is **fully automated** and uses **PR-based gates** with approval requirements. The process supports both stable releases and pre-releases (alpha, beta, release candidates).

## Release Types

### Stable Releases

- Format: `1.0.0`, `1.1.0`, `2.0.0`
- Tagged as: `v1.0.0`
- Published to: `latest` tag (in addition to version tags)
- GitHub Release: Marked as stable release

### Pre-releases

#### Beta Releases

- Format: `1.0.0-beta.1`, `1.0.0-beta.2`
- Tagged as: `v1.0.0-beta.1`
- GitHub Release: Marked as prerelease
- Use for: Feature-complete releases ready for testing

#### Alpha Releases

- Format: `1.0.0-alpha.1`, `1.0.0-alpha.2`
- Tagged as: `v1.0.0-alpha.1`
- GitHub Release: Marked as prerelease
- Use for: Early development releases

#### Release Candidates

- Format: `1.0.0-rc.1`, `1.0.0-rc.2`
- Tagged as: `v1.0.0-rc.1`
- GitHub Release: Marked as prerelease
- Use for: Final testing before stable release

## Step-by-Step Release Process

### Step 1: Prepare for Release

Before creating a release, ensure:

- [ ] All features for this release are merged to `main`
- [ ] All tests pass on `main` branch
- [ ] Documentation is up to date
- [ ] Version number follows semantic versioning
- [ ] You have write access to the repository

### Step 2: Create Release PR

#### Option A: Using GitHub Actions UI (Recommended)

1. Navigate to **Actions** tab in GitHub
2. Select **Prepare Release** workflow from the left sidebar
3. Click **Run workflow** button (top right)
4. Fill in the workflow inputs:
   - **Version**: Enter the version number (e.g., `1.0.0`, `1.0.0-beta.1`, `1.0.0-rc.1`)
     - Do NOT include the `v` prefix (it will be added automatically)
     - Must follow semantic versioning: `X.Y.Z` or `X.Y.Z-alpha.N` or `X.Y.Z-beta.N` or `X.Y.Z-rc.N`
   - **Release type**: Select from dropdown:
     - `stable` - For stable releases (e.g., `1.0.0`)
     - `beta` - For beta releases (e.g., `1.0.0-beta.1`)
     - `alpha` - For alpha releases (e.g., `1.0.0-alpha.1`)
     - `rc` - For release candidates (e.g., `1.0.0-rc.1`)
   - **Auto-merge**: Leave unchecked (requires branch protection)
5. Click **Run workflow**

The workflow will:

- Validate the version format
- Check if the tag already exists
- Create a branch `release/v<version>`
- Optionally update `Chart.yaml` version
- Create a pull request to `main`

#### Option B: Manual PR Creation

If you prefer to create the PR manually:

1. Create a branch from `main`:

   ```bash
   git checkout main
   git pull origin main
   git checkout -b release/v1.0.0
   ```

2. Optionally update `helm/buildkit-controller/Chart.yaml`:

   ```yaml
   version: 1.0.0
   appVersion: "1.0.0"
   ```

   (Note: This will be auto-updated on release, but you can do it manually)

3. Commit and push:

   ```bash
   git add helm/buildkit-controller/Chart.yaml
   git commit -m "chore: prepare release v1.0.0"
   git push origin release/v1.0.0
   ```

4. Create a PR on GitHub:
   - Title: `Release v1.0.0` (must match this format)
   - Base: `main`
   - Head: `release/v1.0.0`

### Step 3: Automatic PR Validation

Once the PR is created, the **Release PR Checks** workflow automatically runs and:

- ✅ Validates version format (semantic versioning)
- ✅ Checks if tag already exists
- ✅ Validates Chart.yaml version (if present)
- ✅ Checks PR title format
- ✅ Adds a validation comment to the PR

**Review the validation comment** on the PR to ensure everything is correct.

### Step 4: Review and Approval

The release PR must pass all checks:

#### Required Status Checks

- ✅ **Test / Test** - All tests pass
- ✅ **Release PR Checks / Check Release PR** - Version validation passes
- ✅ Any other required checks configured in branch protection

#### Required Approvals

- Get the required number of approvals (configured in branch protection, typically 1-2)
- Ensure all reviewers are maintainers with release permissions

#### Review Checklist

Before approving, verify:

- [ ] Version number is correct
- [ ] Release type matches the version (stable vs pre-release)
- [ ] All CI checks pass
- [ ] PR title is correct: `Release v<version>`
- [ ] No blocking issues in validation comment

### Step 5: Merge PR

Once all checks pass and approvals are obtained:

1. Click **Merge pull request**
2. Confirm the merge
3. Optionally delete the release branch (recommended)

### Step 6: Automatic Tag Creation

After merging, the **Auto Tag Release** workflow automatically:

1. Extracts version from the merged branch name
2. Creates an annotated git tag: `v<version>`
3. Pushes the tag to the repository
4. Comments on the PR confirming tag creation

**Example**: For branch `release/v1.0.0`, creates tag `v1.0.0`

### Step 7: Release Workflow Execution

The tag push triggers the **Release** workflow, which:

1. **Runs tests** to ensure code quality
2. **Builds Docker images** (controller and gateway):
   - Multi-arch: `linux/amd64`, `linux/arm64`
   - Tags created:
     - `v1.0.0` (exact version)
     - `v1.0` (major.minor)
     - `v1` (major)
     - `latest` (only for stable releases, not pre-releases)
3. **Builds CLI binaries** for all platforms:
   - `bkctl-linux-amd64`
   - `bkctl-linux-arm64`
   - `bkctl-darwin-amd64`
   - `bkctl-darwin-arm64`
4. **Publishes Helm chart** to OCI registry:
   - Updates Chart.yaml version automatically
   - Lints the chart
   - Packages and pushes to `ghcr.io/<repo>/charts/buildkit-controller`
5. **Creates GitHub Release**:
   - Uses release notes from Release Drafter (if available)
   - Otherwise generates release notes automatically
   - Attaches CLI binaries and Helm chart
   - Includes installation instructions
   - Marks as pre-release if version contains `-alpha`, `-beta`, or `-rc`

### Step 8: Environment Protection (If Configured)

If the `release` environment is configured with required reviewers:

1. The workflow will pause at the `build-images` and `create-release` jobs
2. Required reviewers will receive a notification
3. Reviewers must approve the deployment in the **Actions** tab
4. After approval, the workflow continues

### Step 9: Verify Release

After the workflow completes:

1. **Check GitHub Release**:

   - Navigate to **Releases** page
   - Verify release notes are correct
   - Verify all assets are attached
   - Verify pre-release flag (if applicable)

2. **Verify Docker Images**:

   ```bash
   docker pull ghcr.io/<org>/<repo>/controller:v1.0.0
   docker pull ghcr.io/<org>/<repo>/gateway:v1.0.0
   ```

3. **Verify Helm Chart**:

   ```bash
   helm pull oci://ghcr.io/<org>/<repo>/charts/buildkit-controller --version 1.0.0
   ```

4. **Verify CLI Binaries**:
   - Download from GitHub release assets
   - Test on your platform

## Release Drafter

The **Release Drafter** workflow automatically maintains a draft release as PRs are merged to `main`. The draft release:

- Categorizes changes by labels (features, bug fixes, docs, etc.)
- **Auto-calculates version** based on PR labels (`major`, `minor`, `patch`)
- Updates automatically when PRs are merged
- Can be used directly to create releases via **Create Release from Draft** workflow

### How Version is Auto-Calculated

Release Drafter analyzes all PRs merged since the last release and determines the version bump:

- If any PR has `major` or `breaking` label → Major version bump (1.0.0 → 2.0.0)
- If any PR has `minor` or `feature` label → Minor version bump (1.0.0 → 1.1.0)
- Otherwise → Patch version bump (1.0.0 → 1.0.1)

The draft release shows the calculated version in its tag name (e.g., `v1.1.0`).

### Release Drafter Labels

To ensure proper categorization, label PRs with:

- `feature` or `enhancement` → Features section
- `fix`, `bugfix`, or `bug` → Bug Fixes section
- `documentation` or `docs` → Documentation section
- `chore`, `maintenance`, or `dependencies` → Maintenance section
- `refactor` → Refactoring section
- `performance` → Performance section
- `test` or `testing` → Testing section

### Version Resolution

The release drafter determines version increment based on PR labels:

- `major` or `breaking` → Major version bump
- `minor` or `feature` → Minor version bump
- `patch`, `fix`, `bugfix`, or `bug` → Patch version bump

## Configuration

### Branch Protection

Configure branch protection for `main`:

1. Go to **Settings** → **Branches** → **Add rule**
2. Branch name pattern: `main`
3. Enable:
   - ✅ Require a pull request before merging
   - ✅ Require approvals (recommended: 1-2)
   - ✅ Require status checks to pass
   - ✅ Require branches to be up to date
   - ✅ Include administrators

#### Required Status Checks

Add these required checks:

- `Test / Test`
- `Release PR Checks / Check Release PR` (for release PRs)

### Release Environment Protection

Set up the `release` environment for additional control:

1. Go to **Settings** → **Environments** → **New environment**
2. Name: `release`
3. Configure:
   - **Required reviewers**: Add maintainers who can approve releases
   - **Wait timer**: Optional delay before deployment (0 minutes recommended)
   - **Deployment branches**: `main` only
4. Save

**Note**: Environment protection is optional but recommended for production releases.

## Version Numbering

### Semantic Versioning

Follow [Semantic Versioning](https://semver.org/):

- **MAJOR** (X.0.0): Breaking changes
- **MINOR** (0.X.0): New features (backward compatible)
- **PATCH** (0.0.X): Bug fixes (backward compatible)

### Pre-release Examples

- `1.0.0-alpha.1` - Early development release
- `1.0.0-alpha.2` - Next alpha release
- `1.0.0-beta.1` - Feature complete, testing phase
- `1.0.0-beta.2` - Next beta release
- `1.0.0-rc.1` - Release candidate, final testing
- `1.0.0-rc.2` - Next release candidate
- `1.0.0` - Stable release

### Version Increment Strategy

- **Alpha** → **Beta**: When features are complete
- **Beta** → **RC**: When ready for final testing
- **RC** → **Stable**: When all tests pass and ready for production
- **Stable** → **Next Alpha**: Start next development cycle

## Examples

### Example 1: Creating a Stable Release from Draft (Easiest)

**Goal**: Release the version auto-calculated by Release Drafter as a stable release

1. Ensure you're on the `main` branch
2. Go to **Actions** → **Create Release from Draft**
3. Click **Run workflow** (no inputs needed - version auto-detected, stable because on `main`)
4. Review the created PR (version will be from Release Drafter, e.g., `v1.1.0`)
5. Wait for all checks to pass
6. Get required approvals
7. Merge the PR
8. Monitor the release workflow in **Actions**
9. Verify the release on **Releases** page

**Result**:

- Tag: `v1.1.0` (or whatever Release Drafter calculated)
- Images: `v1.1.0`, `v1.1`, `v1`, `latest`
- Chart: `1.1.0`
- Release: Stable

### Example 2: Creating a Beta Release from Development Branch

**Goal**: Create a beta release from a development branch

1. Switch to your development branch (e.g., `develop`, `next`)
2. Go to **Actions** → **Create Release from Draft**
3. Pre-release suffix: `beta` (default)
4. Click **Run workflow**
5. Review the created PR (version auto-detected from draft + `-beta.1` suffix)
6. Wait for all checks to pass
7. Get required approvals
8. Merge the PR
9. Monitor the release workflow in **Actions**
10. Verify the release on **Releases** page

**Result**:

- Tag: `v1.1.0-beta.1` (base version from draft + `-beta.1`)
- Images: `v1.1.0-beta.1` (no `latest` tag)
- Chart: `1.1.0-beta.1`
- Release: Pre-release

**Note**: Running this again from the same branch will auto-increment to `v1.1.0-beta.2`, etc.

### Example 3: Creating a Release Candidate

**Goal**: Create an RC release from a release branch

1. Switch to your release branch (e.g., `release/1.0.0`)
2. Go to **Actions** → **Create Release from Draft**
3. Pre-release suffix: `rc`
4. Click **Run workflow**
5. Review the created PR
6. Wait for all checks to pass
7. Get required approvals
8. Merge the PR
9. Monitor the release workflow in **Actions**
10. Verify the release on **Releases** page

**Result**:

- Tag: `v1.1.0-rc.1` (base version from draft + `-rc.1`)
- Images: `v1.1.0-rc.1` (no `latest` tag)
- Chart: `1.1.0-rc.1`
- Release: Pre-release

### Branch-Based Release Strategy

The workflows automatically determine release type based on the branch:

- **`main` branch** → Always creates **stable releases**
- **Other branches** (e.g., `develop`, `next`, `release/*`) → Creates **pre-releases**

This means:

- Work on `main` → Stable releases only
- Work on `develop` → Beta/alpha releases
- Work on `release/*` → RC releases

You can override this by manually specifying the release type in the workflow inputs.

## Troubleshooting

### Tag Already Exists

**Error**: "Tag v1.0.0 already exists!"

**Solution**:

- Check if the version was already released
- Use a new version number or increment the pre-release suffix
- For pre-releases: increment the number (e.g., `1.0.0-beta.1` → `1.0.0-beta.2`)

### Invalid Version Format

**Error**: "Invalid version format"

**Solution**:

- Ensure version follows semantic versioning: `X.Y.Z` or `X.Y.Z-alpha.N` or `X.Y.Z-beta.N` or `X.Y.Z-rc.N`
- Do NOT include `v` prefix in the workflow input
- Examples of valid versions:
  - `1.0.0` ✅
  - `1.0.0-beta.1` ✅
  - `v1.0.0` ❌ (don't include `v`)
  - `1.0` ❌ (must be X.Y.Z format)

### Release PR Not Triggering Checks

**Issue**: Release PR Checks workflow doesn't run

**Solution**:

- Ensure branch name starts with `release/`
- Check workflow permissions in repository settings
- Verify the workflow file exists: `.github/workflows/release-pr-checks.yml`

### Build/Publish Fails After Merge

**Issue**: Release workflow fails after tag creation

**Solution**:

1. Check workflow logs in **Actions** tab
2. Verify Docker registry permissions:
   - `vars.DOCKER_USER` is set
   - `secrets.DOCKER_PAT` is configured
3. Check Helm chart linting errors
4. Ensure all required secrets are configured
5. Verify environment protection settings (if configured)

### Environment Protection Blocking Release

**Issue**: Workflow is waiting for environment approval

**Solution**:

1. Go to **Actions** tab
2. Find the running workflow
3. Click on the job that's waiting
4. Click **Review deployments**
5. Approve the deployment
6. Workflow will continue automatically

### Release Notes Not Generated

**Issue**: Release notes are empty or generic

**Solution**:

- Ensure PRs are properly labeled
- Check Release Drafter configuration: `.github/release-drafter.yml`
- Verify Release Drafter workflow runs on PR merges
- Manually edit release notes after release creation if needed

## Manual Override (Emergency Only)

If you need to manually create a tag (not recommended):

```bash
git tag -a v1.0.0 -m "Release v1.0.0"
git push origin v1.0.0
```

**Note**: This will trigger the release workflow, but:

- No release PR validation
- No automatic Chart.yaml updates
- Should only be used for emergency hotfixes

## Best Practices

1. **Always use PR-based releases** - Never tag manually
2. **Test pre-releases** - Use beta/rc releases for testing before stable
3. **Review release notes** - Check auto-generated notes before merging
4. **Follow semantic versioning** - Maintain clear versioning strategy
5. **Use protected environments** - Require approvals for releases
6. **Monitor release workflow** - Check Actions tab after merge
7. **Label PRs properly** - Helps Release Drafter categorize changes
8. **Increment pre-release numbers** - Use `-beta.1`, `-beta.2`, etc.
9. **Verify releases** - Always check Docker images, Helm chart, and CLI binaries
10. **Document breaking changes** - Use `major` or `breaking` labels

## Support

For issues with the release process:

- Check workflow logs in **Actions** tab
- Review this documentation
- Open an issue with `release` label
- Contact maintainers

## Workflow Summary

```
┌─────────────────────┐
│  Prepare Release    │
│  (workflow_dispatch)│
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│  Create Release PR  │
│  (release/v1.0.0)   │
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
│  (build & publish)  │
└─────────────────────┘
```
