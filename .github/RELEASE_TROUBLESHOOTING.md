# Release Workflow Troubleshooting

## Release Workflow Not Running

If the release workflow doesn't run after merging a release PR, check the following:

### 1. Check if Auto-Tag-Release Workflow Ran

1. Go to **Actions** tab
2. Look for **Auto Tag Release** workflow
3. Check if it ran when you merged the release PR
4. Check the logs to see if:
   - The tag was created
   - The tag was pushed successfully
   - Any errors occurred

### 2. Check if Tag Was Created

1. Go to **Releases** page
2. Click **Tags** tab
3. Look for the tag (e.g., `v0.0.2`)
4. If tag exists, check which commit it points to

**Or via command line:**

```bash
git fetch --tags
git tag -l "v*"
git show v0.0.2  # Check what commit the tag points to
```

### 3. Check Release Workflow Triggers

The release workflow triggers on:

- Push of tags matching `v*` pattern

**Verify the tag format:**

- ✅ Correct: `v0.0.2`, `v1.0.0`, `v1.0.0-beta.1`
- ❌ Wrong: `0.0.2` (missing `v` prefix)

### 4. Check Workflow Permissions

1. Go to **Settings** → **Actions** → **General**
2. Check **Workflow permissions**:
   - Should be: **Read and write permissions**
   - ✅ **Allow GitHub Actions to create and approve pull requests**

### 5. Check if Tag Was Pushed

The auto-tag-release workflow should push the tag. Check the workflow logs for:

```
✅ Created and pushed tag: v0.0.2
Tag points to: <commit-sha>
```

If you see an error pushing the tag, check:

- Repository permissions
- Token permissions
- Network issues

### 6. Manual Tag Creation (Emergency)

If the auto-tag workflow failed, you can manually create the tag:

```bash
# Checkout main
git checkout main
git pull origin main

# Create and push tag
git tag -a v0.0.2 -m "Release v0.0.2"
git push origin v0.0.2
```

This will trigger the release workflow.

### 7. Check Release Workflow Status

1. Go to **Actions** tab
2. Look for **Release** workflow
3. Check if it's:
   - ✅ Running
   - ⏸️ Waiting for environment approval
   - ❌ Failed
   - ⚠️ Not triggered

### 8. Common Issues

#### Issue: Tag Already Exists

**Symptom**: Auto-tag-release workflow exits early with "Tag already exists"

**Solution**:

- Check if the tag was created in a previous attempt
- Delete the tag if it's pointing to the wrong commit:
  ```bash
  git push origin --delete v0.0.2  # Delete remote tag
  git tag -d v0.0.2                # Delete local tag (if exists)
  ```
- Re-run the workflow or create a new release PR

#### Issue: Workflow Waiting for Approval

**Symptom**: Release workflow is paused at `build-images` or `create-release` job

**Solution**:

1. Go to **Actions** tab
2. Find the running workflow
3. Click on the paused job
4. Click **Review deployments**
5. Approve the deployment
6. Workflow will continue

#### Issue: Tag Points to Wrong Commit

**Symptom**: Tag exists but points to an old commit

**Solution**:

1. Delete the tag (see above)
2. Re-run auto-tag-release workflow or create tag manually pointing to correct commit

#### Issue: Release Workflow Not Triggered

**Symptom**: Tag exists but release workflow didn't run

**Possible causes**:

- Tag format doesn't match `v*` pattern
- Workflow file has syntax errors
- Repository settings blocking workflow runs

**Solution**:

1. Check tag format: `git tag -l "v*"`
2. Check workflow file syntax
3. Verify workflow is enabled in repository settings

### 9. Debugging Steps

1. **Check Auto-Tag-Release logs**:

   - Did it extract the version correctly?
   - Did it create the tag?
   - Did it push the tag?
   - Any errors?

2. **Check tag exists**:

   ```bash
   git ls-remote --tags origin | grep v0.0.2
   ```

3. **Check release workflow**:

   - Is it enabled?
   - Does it have the correct trigger?
   - Are there any syntax errors?

4. **Check permissions**:
   - Does the workflow have `contents: write` permission?
   - Is the GITHUB_TOKEN valid?

### 10. Getting Help

If none of the above resolves the issue:

1. Check the workflow logs in detail
2. Verify all prerequisites from `.github/GITHUB_SETUP.md`
3. Check GitHub Actions status page for outages
4. Review `.github/RELEASE_PROCESS.md` for the expected flow
