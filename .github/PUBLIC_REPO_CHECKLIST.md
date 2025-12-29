# Public Repository Checklist

This document lists all the files and configurations added to make this repository ready for public release.

## ✅ Files Added

### Legal & Community

- ✅ `LICENSE` - Apache 2.0 License
- ✅ `CODE_OF_CONDUCT.md` - Contributor Covenant Code of Conduct
- ✅ `SECURITY.md` - Security policy and vulnerability reporting
- ✅ `CONTRIBUTING.md` - Contribution guidelines

### GitHub Templates

- ✅ `.github/ISSUE_TEMPLATE/bug_report.md` - Bug report template
- ✅ `.github/ISSUE_TEMPLATE/feature_request.md` - Feature request template
- ✅ `.github/pull_request_template.md` - Pull request template

### CI/CD & Automation

- ✅ `.github/workflows/test.yml` - Test workflow with explicit steps
- ✅ `.github/workflows/release-drafter.yml` - Automated release drafting (maintains draft releases)
- ✅ `.github/workflows/prepare-release.yml` - Workflow to create release PRs (supports pre-releases)
- ✅ `.github/workflows/release-pr-checks.yml` - Release PR validation (version format, tag existence, etc.)
- ✅ `.github/workflows/auto-tag-release.yml` - Auto-tagging when release PRs are merged
- ✅ `.github/workflows/release.yml` - Complete release workflow (builds, publishes, creates release)
- ✅ `.github/release-drafter.yml` - Release notes configuration
- ✅ `renovate.json` - Renovate configuration for advanced dependency management (optional)
- ✅ `.golangci.yml` - golangci-lint configuration for code quality

### Documentation

- ✅ `.github/workflows/README.md` - Workflow documentation
- ✅ `.github/RELEASE_PROCESS.md` - Complete release process documentation with step-by-step instructions
- ✅ Updated `README.md` - Added CI/CD badges (needs repository name update)

### Configuration

- ✅ Updated `.gitignore` - Added charts/ directory

## 🔧 Required Updates

### 1. Repository Name in README

Update the CI/CD badges in `README.md` to use your actual repository:

```markdown
[![Test](https://github.com/YOUR_ORG/YOUR_REPO/workflows/Test/badge.svg)](https://github.com/YOUR_ORG/YOUR_REPO/actions/workflows/test.yml)
```

Replace `YOUR_ORG/YOUR_REPO` with your actual GitHub organization and repository name.

### 2. Security Contact Email

Update the email addresses in `SECURITY.md`:

- `security@smrt-devops.net` → Your security contact email

### 3. Code of Conduct Contact

Update the email in `CODE_OF_CONDUCT.md`:

- `conduct@smrt-devops.net` → Your conduct contact email

### 4. Repository URLs

Update repository URLs in:

- `CONTRIBUTING.md` - Replace `smrt-devops/buildkit-controller` with your repo
- `SECURITY.md` - Update GitHub releases URL
- `helm/buildkit-controller/Chart.yaml` - Already has `kering-gucci-digital/gd_pe-buildkit-controller`

### 5. Enable GitHub Features

After pushing to GitHub:

1. **Enable Renovate**:
   - Install the [Renovate App](https://github.com/apps/renovate) from GitHub Marketplace
   - Or enable it via repository settings (if available in your organization)
   - Note: You can keep Dependabot or disable it in favor of Renovate (Renovate is more feature-rich)
2. **Enable Dependabot** (optional, if not using Renovate): Go to Settings → Security → Dependabot alerts
3. **Enable Discussions**: Go to Settings → General → Features → Discussions
4. **Set up Release Drafter**: The workflow will run automatically on pushes to main and PR merges
5. **Configure Branch Protection**: Set up branch protection rules for `main` branch
   - Require pull request reviews (recommended: 1-2 approvals)
   - Require status checks to pass
   - Add required checks:
     - `Test / Test`
     - `Release PR Checks / Check Release PR` (for release PRs)
6. **Set up Release Environment** (Recommended): Go to Settings → Environments → New environment
   - Name: `release`
   - Add required reviewers (maintainers who can approve releases)
   - Restrict to `main` branch only
   - This adds an approval gate before building images and creating releases
7. **Enable Security Scanning**: Go to Settings → Security → Code security and analysis
   - Enable "Dependency graph"
   - Enable "Dependabot alerts" (if using Dependabot)
   - Enable "Code scanning" (optional, for additional security tools)

## 📋 Pre-Publication Checklist

- [ ] Update all repository references in documentation
- [ ] Update contact emails in SECURITY.md and CODE_OF_CONDUCT.md
- [ ] Update CI/CD badge URLs in README.md
- [ ] Verify all workflows use correct repository paths
- [ ] Test workflows on a test branch/PR
- [ ] Ensure LICENSE copyright matches your organization
- [ ] Review and customize CONTRIBUTING.md for your project
- [ ] Review and customize `.golangci.yml` for your linting preferences
- [ ] Review and customize `renovate.json` for your dependency update preferences
- [ ] Decide whether to use Renovate, Dependabot, or both
- [ ] Set up branch protection rules
- [ ] Enable GitHub Discussions (if desired)
- [ ] Configure repository topics and description
- [ ] Add repository to appropriate GitHub topics
- [ ] Enable security scanning features in GitHub settings
- [ ] Set up release environment with required reviewers
- [ ] Configure branch protection with required checks
- [ ] Review release process documentation (`.github/RELEASE_PROCESS.md`)
- [ ] Test release workflow with a beta release (e.g., `1.0.0-beta.1`)
  - Use the "Prepare Release" workflow to create a release PR
  - Verify all validation checks pass
  - Merge the PR and verify auto-tagging works
  - Verify the release workflow completes successfully
- [ ] Create initial release (v0.1.0 or similar) using the release workflow
- [ ] Verify Release Drafter is creating draft releases automatically

## 🚀 Post-Publication

After making the repository public:

1. **Monitor Issues**: Verify issue templates are working
2. **Test Workflows**: Verify CI/CD pipelines run successfully
3. **Test Release Process**:
   - Create a test pre-release (e.g., `0.1.0-beta.1`) using the Prepare Release workflow
   - Verify all steps work: PR creation, validation, auto-tagging, and release
4. **Create First Release**: Create the first stable release (e.g., `v0.1.0`) using the release workflow
5. **Verify Release Drafter**: Check that draft releases are being maintained automatically
6. **Announce**: Share the repository with your community

## 📝 Release Workflow Overview

The release process follows these steps:

1. **Prepare Release** (`.github/workflows/prepare-release.yml`)

   - Triggered manually via workflow dispatch
   - Creates a release PR with branch `release/v<version>`
   - Supports stable, alpha, beta, and rc release types

2. **Release PR Checks** (`.github/workflows/release-pr-checks.yml`)

   - Automatically validates release PRs
   - Checks version format, tag existence, Chart.yaml
   - Adds validation comments to PRs

3. **Auto Tag Release** (`.github/workflows/auto-tag-release.yml`)

   - Automatically creates and pushes tags when release PRs are merged
   - Extracts version from branch name

4. **Release** (`.github/workflows/release.yml`)

   - Triggered by tag pushes
   - Builds Docker images, CLI binaries, publishes Helm chart
   - Creates GitHub release with artifacts
   - Uses environment protection for approval gates

5. **Release Drafter** (`.github/workflows/release-drafter.yml`)
   - Maintains draft releases automatically
   - Categorizes changes by PR labels
   - Resolves version based on PR labels

For detailed step-by-step instructions, see `.github/RELEASE_PROCESS.md`.

## 📚 Additional Resources

- [GitHub Community Standards](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions)
- [Open Source Guides](https://opensource.guide/)
- [Semantic Versioning](https://semver.org/)
