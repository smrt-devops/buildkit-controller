# GitHub Settings Configuration Guide

This document outlines all the GitHub settings and configurations needed for the workflows to function properly.

## ✅ Required Configuration

### 1. Repository Variables

**Location**: Settings → Secrets and variables → Actions → Variables tab

Configure these variables:

| Variable Name | Description              | Example                   | Required For                             |
| ------------- | ------------------------ | ------------------------- | ---------------------------------------- |
| `DOCKER_USER` | Your Docker Hub username | `your-dockerhub-username` | Release workflow (Docker Cloud builders) |

**How to set:**

1. Go to **Settings** → **Secrets and variables** → **Actions**
2. Click **Variables** tab
3. Click **New repository variable**
4. Add `DOCKER_USER` with your Docker Hub username
5. Click **Add variable**

### 2. Repository Secrets

**Location**: Settings → Secrets and variables → Actions → Secrets tab

Configure these secrets:

| Secret Name  | Description                      | How to Create | Required For                             |
| ------------ | -------------------------------- | ------------- | ---------------------------------------- |
| `DOCKER_PAT` | Docker Hub Personal Access Token | See below     | Release workflow (Docker Cloud builders) |

**How to create Docker Hub PAT:**

1. Go to [Docker Hub](https://hub.docker.com/) → **Account Settings** → **Security**
2. Click **New Access Token**
3. Give it a name (e.g., "GitHub Actions")
4. Set permissions: **Read & Write**
5. Copy the token (you won't see it again!)
6. In GitHub: **Settings** → **Secrets and variables** → **Actions** → **Secrets** tab
7. Click **New repository secret**
8. Name: `DOCKER_PAT`, Value: paste your token
9. Click **Add secret**

**Note**: `GITHUB_TOKEN` is automatically provided by GitHub Actions - you don't need to create it.

### 3. Release Environment (Recommended)

**Location**: Settings → Environments → New environment

The `release` environment adds an approval gate before building images and creating releases.

**How to set up:**

1. Go to **Settings** → **Environments**
2. Click **New environment**
3. Name: `release` (must match exactly)
4. Configure:
   - **Required reviewers**: Add maintainers who can approve releases
     - Click **Add reviewer** and select team members
   - **Wait timer**: `0` minutes (or set a delay if desired)
   - **Deployment branches**: Select **Selected branches** → Add `main`
5. Click **Save protection rules**

**What this does:**

- When a release tag is pushed, the workflow will pause at `build-images` and `create-release` jobs
- Required reviewers receive a notification
- They must approve in the **Actions** tab before the workflow continues
- This provides an extra safety check before publishing releases

### 4. Branch Protection Rules

**Location**: Settings → Branches → Add rule

Protect the `main` branch to ensure quality:

**How to set up:**

1. Go to **Settings** → **Branches**
2. Click **Add rule** (or edit existing rule for `main`)
3. Branch name pattern: `main`
4. Enable these settings:
   - ✅ **Require a pull request before merging**
     - ✅ Require approvals: `1` or `2` (recommended)
     - ✅ Dismiss stale pull request approvals when new commits are pushed
   - ✅ **Require status checks to pass before merging**
     - ✅ Require branches to be up to date before merging
     - Add required checks:
       - `Test / Test` (from test.yml workflow)
       - `Release PR Checks / Check Release PR` (for release PRs only)
   - ✅ **Include administrators** (optional, but recommended)
5. Click **Create** (or **Save changes**)

### 5. Workflow Permissions

**Location**: Settings → Actions → General → Workflow permissions

Ensure workflows have the correct permissions:

1. Go to **Settings** → **Actions** → **General**
2. Scroll to **Workflow permissions**
3. Select: **Read and write permissions**
4. ✅ Check **Allow GitHub Actions to create and approve pull requests**
5. Click **Save**

**Why this is needed:**

- Release workflows need to create PRs, tags, and releases
- Auto-tag-release needs to push tags
- Release drafter needs to create draft releases

## 🔧 Optional Configuration

### 6. Release Drafter (Automatic)

**Status**: ✅ Already configured via `.github/release-drafter.yml`

The Release Drafter workflow runs automatically. No additional setup needed, but ensure:

- PRs are labeled appropriately (see `.github/RELEASE_PROCESS.md`)
- The workflow has `contents: write` and `pull-requests: write` permissions (already set)

### 7. Renovate (Optional)

**Location**: Install from [GitHub Marketplace](https://github.com/marketplace/renovate)

If you want automated dependency updates:

1. Go to [Renovate App](https://github.com/apps/renovate)
2. Click **Install**
3. Select your repository
4. Configuration is already in `renovate.json`

**Alternative**: Use Dependabot (built into GitHub)

- Go to **Settings** → **Security** → **Dependabot**
- Enable **Dependabot alerts** and **Dependabot security updates**

### 8. Security Scanning (Optional)

**Location**: Settings → Security → Code security and analysis

Enable additional security features:

1. Go to **Settings** → **Security**
2. Scroll to **Code security and analysis**
3. Enable:
   - ✅ **Dependency graph** (recommended)
   - ✅ **Dependabot alerts** (if using Dependabot)
   - ✅ **Dependabot security updates** (if using Dependabot)
   - ✅ **Code scanning** (optional, requires additional setup)

### 9. GitHub Discussions (Optional)

**Location**: Settings → General → Features → Discussions

Enable if you want community discussions:

1. Go to **Settings** → **General**
2. Scroll to **Features**
3. Enable **Discussions**
4. Click **Set up discussions**

## 📋 Configuration Checklist

Use this checklist to ensure everything is configured:

### Required

- [ ] Repository variable `DOCKER_USER` is set
- [ ] Repository secret `DOCKER_PAT` is set
- [ ] `release` environment is created with required reviewers
- [ ] Branch protection rule for `main` is configured
- [ ] Required status checks are added to branch protection
- [ ] Workflow permissions are set to "Read and write"

### Optional

- [ ] Renovate or Dependabot is enabled
- [ ] Security scanning features are enabled
- [ ] GitHub Discussions is enabled (if desired)

## 🧪 Testing Your Configuration

After configuring everything, test the workflows:

### 1. Test the Test Workflow

1. Create a test PR to `main`
2. Verify the **Test** workflow runs
3. Check that all steps pass:
   - Checkout code ✅
   - Set up Go ✅
   - Install controller-gen ✅
   - Generate CRDs ✅
   - Generate DeepCopy methods ✅
   - Check code formatting ✅
   - Run go vet ✅
   - Run tests ✅
   - Upload coverage report ✅

### 2. Test Release Drafter

1. Merge a PR to `main` with a label (e.g., `feature`, `fix`)
2. Go to **Releases** page
3. Verify a draft release was created/updated
4. Check that version is calculated correctly

### 3. Test Release Process

1. Go to **Actions** → **Create Release from Draft**
2. Click **Run workflow** (on `main` branch)
3. Verify a release PR is created
4. Check that **Release PR Checks** workflow runs and validates
5. (Optional) Merge the PR to test full release flow

### 4. Test Environment Protection

1. Create a release tag manually (for testing): `git tag v0.0.0-test && git push origin v0.0.0-test`
2. Go to **Actions** tab
3. Find the **Release** workflow run
4. Verify it pauses at `build-images` job
5. Check that you can see "Review deployments" button
6. Approve the deployment
7. Verify workflow continues

## ❌ Troubleshooting

### Workflow Fails: "DOCKER_USER not found"

- **Solution**: Add `DOCKER_USER` variable in Settings → Secrets and variables → Actions → Variables

### Workflow Fails: "DOCKER_PAT not found"

- **Solution**: Add `DOCKER_PAT` secret in Settings → Secrets and variables → Actions → Secrets

### Release Workflow Stuck Waiting

- **Solution**: Check if `release` environment is configured with reviewers. Approve in Actions tab.

### Test Workflow Fails on Formatting

- **Solution**: Run `go fmt ./...` locally and commit the changes

### Release Drafter Not Creating Drafts

- **Solution**:
  - Check workflow permissions (needs `contents: write`)
  - Verify `.github/release-drafter.yml` exists
  - Check workflow runs on pushes to `main` (triggers automatically when PRs are merged)

### Multiple Draft Releases Created

- **Issue**: Release Drafter created multiple draft releases (one from branch, one from main)
- **Solution**:
  - The workflow has been updated to only trigger on pushes to `main` (which happens when PRs are merged)
  - Delete the duplicate draft releases manually:
    1. Go to **Releases** page
    2. Find draft releases
    3. Delete the one that's not from `main` (or keep the one with the correct version)
  - Future drafts will only be created/updated when PRs are merged to `main`

### Branch Protection Blocking Merges

- **Solution**: Ensure required status checks are passing. Check that workflow names match exactly.

## 📚 Additional Resources

- [GitHub Actions Documentation](https://docs.github.com/en/actions)
- [GitHub Environments](https://docs.github.com/en/actions/deployment/targeting-different-environments/using-environments-for-deployment)
- [Branch Protection Rules](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches)
- [Release Drafter](https://github.com/release-drafter/release-drafter)
