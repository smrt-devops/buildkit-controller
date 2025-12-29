# Release Process

This document describes the automated release process for BuildKit Controller.

## Overview

The release process is **fully automated** and uses **PR-based gates** with approval requirements. No manual tagging is required.

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

#### Alpha Releases

- Format: `1.0.0-alpha.1`, `1.0.0-alpha.2`
- Tagged as: `v1.0.0-alpha.1`
- GitHub Release: Marked as prerelease

#### Release Candidates

- Format: `1.0.0-rc.1`, `1.0.0-rc.2`
- Tagged as: `v1.0.0-rc.1`
- GitHub Release: Marked as prerelease

## Release Workflow

### Step 1: Create Release PR

#### Option A: Using GitHub Actions UI (Recommended)

1. Go to **Actions** → **Release** workflow
2. Click **Run workflow**
3. Fill in:
   - **Version**: `1.0.0` (or `1.0.0-beta.1` for pre-release)
   - **Release type**: `stable`, `beta`, `alpha`, or `rc`
4. Click **Run workflow**

This will automatically create a PR with branch `release/v1.0.0`.

#### Option B: Manual PR Creation

1. Create a branch: `release/v1.0.0` (or `release/v1.0.0-beta.1`)
2. Optionally update `helm/buildkit-controller/Chart.yaml` version (will be auto-updated on merge)
3. Create a PR to `main` with title: `Release v1.0.0`

### Step 2: PR Validation

The release PR will automatically:

- ✅ Validate version format
- ✅ Check if tag already exists
- ✅ Run all CI checks
- ✅ Validate Chart.yaml version
- ✅ Add validation comment to PR

### Step 3: Review and Approval

Required checks must pass:

- ✅ CI workflow (tests, linting, builds)
- ✅ Security scan workflow
- ✅ Release PR validation
- ✅ Required number of approvals (configure in branch protection)

### Step 4: Merge PR

Once the PR is merged:

1. **Automatically creates git tag** `v1.0.0`
2. **Updates Chart.yaml** version
3. **Builds and pushes Docker images** with tags:
   - `v1.0.0` (exact version)
   - `1.0` (major.minor)
   - `1` (major)
   - `latest` (only for stable releases)
4. **Publishes Helm chart** to OCI registry
5. **Creates GitHub Release** with:
   - Release notes (auto-generated from PRs)
   - Installation instructions
   - Links to Docker images and Helm chart

## Branch Protection Requirements

Configure branch protection for `main`:

1. Go to **Settings** → **Branches** → **Add rule**
2. Branch name pattern: `main`
3. Enable:
   - ✅ Require a pull request before merging
   - ✅ Require approvals (recommended: 1-2)
   - ✅ Require status checks to pass
   - ✅ Require branches to be up to date
   - ✅ Include administrators

### Required Status Checks

Add these required checks:

- `CI / test`
- `CI / build`
- `Release PR Checks / Check Release PR` (for release PRs)
- `Security Scan / gosec`
- `Security Scan / trivy-scan`

## Environment Protection

The release workflow uses a protected environment called `release`:

1. Go to **Settings** → **Environments** → **New environment**
2. Name: `release`
3. Configure:
   - **Required reviewers**: Add maintainers who can approve releases
   - **Wait timer**: Optional delay before deployment (0 minutes recommended)
   - **Deployment branches**: `main` only

## Version Numbering

### Semantic Versioning

Follow [Semantic Versioning](https://semver.org/):

- **MAJOR**: Breaking changes
- **MINOR**: New features (backward compatible)
- **PATCH**: Bug fixes (backward compatible)

### Pre-release Examples

- `1.0.0-alpha.1` - Early development release
- `1.0.0-beta.1` - Feature complete, testing phase
- `1.0.0-rc.1` - Release candidate, final testing
- `1.0.0` - Stable release

## Release Checklist

Before creating a release PR:

- [ ] All features for this release are merged to `main`
- [ ] All tests pass
- [ ] Documentation is up to date
- [ ] CHANGELOG is updated (if maintained separately)
- [ ] Version number follows semantic versioning
- [ ] Release notes are reviewed (auto-generated from PRs)

## Troubleshooting

### Tag Already Exists

If you see "Tag already exists" error:

- Check if the version was already released
- Use a new version number or increment the pre-release suffix

### Release PR Not Triggering

- Ensure branch name starts with `release/`
- Check workflow permissions
- Verify branch protection settings

### Build/Publish Fails After Merge

- Check workflow logs in Actions tab
- Verify Docker registry permissions
- Check Helm chart linting errors
- Ensure all required secrets are configured

## Manual Override (Emergency Only)

If you need to manually create a tag (not recommended):

```bash
git tag -a v1.0.0 -m "Release v1.0.0"
git push origin v1.0.0
```

**Note:** This will trigger the build workflows, but the release workflow expects a merged PR. Manual tags should only be used for emergency hotfixes.

## Examples

### Creating a Stable Release

1. Workflow dispatch: Version `1.0.0`, Type `stable`
2. PR created: `release/v1.0.0`
3. After approval and merge:
   - Tag: `v1.0.0`
   - Images: `v1.0.0`, `1.0`, `1`, `latest`
   - Chart: `1.0.0`
   - Release: Stable

### Creating a Beta Release

1. Workflow dispatch: Version `1.0.0-beta.1`, Type `beta`
2. PR created: `release/v1.0.0-beta.1`
3. After approval and merge:
   - Tag: `v1.0.0-beta.1`
   - Images: `v1.0.0-beta.1` (no `latest` tag)
   - Chart: `1.0.0-beta.1`
   - Release: Prerelease

## Best Practices

1. **Always use PR-based releases** - Never tag manually
2. **Test pre-releases** - Use beta/rc releases for testing
3. **Review release notes** - Check auto-generated notes before merging
4. **Follow semantic versioning** - Maintain clear versioning strategy
5. **Use protected environments** - Require approvals for releases
6. **Monitor release workflow** - Check Actions tab after merge

## Support

For issues with the release process:

- Check workflow logs in Actions tab
- Review this documentation
- Open an issue with `release` label
