# Release Drafter Strategy

This document explains the Release Drafter configuration and best practices for managing draft releases.

## Current Strategy: Single Draft from Main

**Recommended approach** (currently active):

- **One draft release** maintained from `main` branch only
- Tracks all PRs merged to `main` (the source of truth)
- Version is auto-calculated based on PR labels
- When you create a release:
  - From `main` → Uses the draft as-is (stable release)
  - From other branches → Uses the draft version + adds pre-release suffix (e.g., `-beta.1`)

### Why This Approach?

✅ **Pros:**

- Single source of truth (what's actually merged to main)
- No confusion about which draft to use
- Pre-release versions are derived from the stable draft
- Simpler workflow

❌ **Cons:**

- Doesn't track work-in-progress on feature/develop branches
- Pre-release drafts don't show what's specifically in that branch

## Alternative Strategy: Multiple Drafts (Optional)

If you want to track pre-release work separately, you can enable multiple drafts:

### Setup for Multiple Drafts

1. **Uncomment the develop branch** in `.github/workflows/release-drafter.yml`:

   ```yaml
   on:
     push:
       branches:
         - main
         - develop # Uncomment this
   ```

2. **Uncomment the develop job** in the workflow:

   ```yaml
   update-release-draft-develop:
     name: Update Release Draft (Develop)
     runs-on: ubuntu-latest
     if: github.ref == 'refs/heads/develop'
     steps:
       - uses: release-drafter/release-drafter@v6
         env:
           GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
         with:
           config-name: release-drafter-develop.yml
   ```

3. **Use the develop config** (already created: `.github/release-drafter-develop.yml`)

### How Multiple Drafts Work

- **Main branch draft**: `v1.1.0` (stable release)
- **Develop branch draft**: `v1.1.0-beta` (pre-release)

When creating releases:

- From `main` → Uses main draft → `v1.1.0` (stable)
- From `develop` → Uses develop draft → `v1.1.0-beta.1` (pre-release)

### When to Use Multiple Drafts

Use multiple drafts if:

- You have a long-lived `develop` branch with significant work
- You want to track what's specifically in pre-release branches
- You release from `develop` frequently before merging to `main`

Don't use multiple drafts if:

- You use feature branches → PR to main directly
- Pre-releases are just for testing before stable
- You want simplicity

## Best Practice Recommendation

**For most projects**: Use **single draft from main** (current setup)

**Workflow:**

1. Work happens on feature branches
2. PRs merge to `main`
3. Release Drafter updates draft on `main` with all merged PRs
4. When ready to release:
   - From `main` → Stable release (e.g., `v1.1.0`)
   - From `develop` → Pre-release (e.g., `v1.1.0-beta.1`) using the main draft as base

This ensures:

- The draft always reflects what's actually merged
- Pre-releases are clearly derived from the stable version
- No confusion about which draft to use

## Configuration Files

- `.github/release-drafter.yml` - Main branch draft (stable releases)
- `.github/release-drafter-develop.yml` - Develop branch draft (pre-releases) - optional

## Switching Strategies

### To Enable Multiple Drafts

1. Edit `.github/workflows/release-drafter.yml`
2. Uncomment the `develop` branch and job
3. Ensure `.github/release-drafter-develop.yml` exists
4. Push changes

### To Disable Multiple Drafts

1. Edit `.github/workflows/release-drafter.yml`
2. Comment out the `develop` branch and job
3. Delete any duplicate draft releases manually
4. Push changes

## Troubleshooting

### Multiple Draft Releases Created

**Issue**: Two drafts exist (one from branch, one from main)

**Solution**:

- If using single draft strategy: Delete the branch-specific draft
- If using multiple drafts: This is expected - each branch maintains its own draft

### Wrong Draft Used for Release

**Issue**: Release workflow uses the wrong draft

**Solution**:

- Check which branch you're running the workflow from
- The workflow selects drafts based on branch:
  - `main` → Uses stable draft (no pre-release suffix)
  - Other branches → Uses first available draft and adds pre-release suffix

### Draft Not Updating

**Issue**: Draft release doesn't update when PRs are merged

**Solution**:

- Check workflow runs on pushes to the configured branches
- Verify workflow has `contents: write` permission
- Check `.github/release-drafter.yml` exists and is valid
