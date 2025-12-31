# Release Artifacts Flow - Pre-release vs Stable

This document explains exactly where and how the release process handles pre-release vs stable releases and their artifacts.

## The Decision Point: Tag Name

**The tag name determines everything.** The release workflow extracts the version from the tag and checks if it contains `-alpha`, `-beta`, or `-rc`.

## Complete Flow

### Step 1: Create Release PR

**Workflows**: `prepare-release.yml` or `create-release-from-draft.yml`

**Determines**:

- Version number (e.g., `0.0.2` or `0.0.2-beta.1`)
- Release type based on branch:
  - `main` branch → Stable (no suffix added)
  - Other branches → Pre-release (adds `-beta.1`, `-alpha.1`, or `-rc.1`)

**Output**: Creates PR with branch `release/v0.0.2` or `release/v0.0.2-beta.1`

### Step 2: Merge Release PR

**Workflow**: `auto-tag-release.yml`

**Action**: Creates and pushes tag `v0.0.2` or `v0.0.2-beta.1`

**This tag is the source of truth** - it contains the pre-release suffix if applicable.

### Step 3: Release Workflow Triggers

**Workflow**: `release.yml` (triggered by tag push)

**Line 218**: **THE KEY DECISION POINT**

```bash
IS_PRERELEASE=$(echo "$VERSION" | grep -qE '-(alpha|beta|rc)' && echo "true" || echo "false")
```

This checks the tag name:

- `v0.0.2` → `IS_PRERELEASE=false` (stable)
- `v0.0.2-beta.1` → `IS_PRERELEASE=true` (pre-release)

## Artifact Handling Differences

### Docker Images (Lines 84-120)

**Location**: `build-images` job

**Difference**:

- **Stable releases** (`v0.0.2`):
  - Tags: `v0.0.2`, `v0.0`, `v0`, **`latest`** ✅
  - Line 92: `type=raw,value=latest,enable=${{ !contains(github.ref_name, '-') }}`
- **Pre-releases** (`v0.0.2-beta.1`):
  - Tags: `v0.0.2-beta.1` only ❌ (no `latest`, no major/minor tags)
  - Line 92: `latest` tag is **disabled** because tag contains `-`

**Same artifacts built, different tags applied.**

### CLI Binaries (Lines 128-166)

**Location**: `build-cli` job

**No difference** - Same binaries built for both:

- `bkctl-linux-amd64`
- `bkctl-linux-arm64`
- `bkctl-darwin-amd64`
- `bkctl-darwin-arm64`

**All binaries are identical regardless of pre-release status.**

### Helm Chart (Lines 168-201)

**Location**: `publish-helm` job

**No difference** - Same chart published:

- Chart version: `0.0.2` or `0.0.2-beta.1` (matches tag)
- Chart content: **Identical**

**Only the version number in Chart.yaml differs.**

### GitHub Release (Lines 333-367)

**Location**: `create-release` job

**Differences**:

1. **Pre-release flag** (Line 340, 348, 366):

   ```javascript
   prerelease: isPrerelease; // true for pre-releases, false for stable
   ```

2. **Make latest flag** (Line 349, 367):

   ```javascript
   make_latest: !isPrerelease; // false for pre-releases, true for stable
   ```

3. **Release body**:
   - Uses draft release notes if available (same for both)
   - Or generates default notes (same for both)

**Same artifacts attached, different flags set.**

## Summary: What's Different?

| Artifact           | Stable Release                           | Pre-release                              | Where Determined                           |
| ------------------ | ---------------------------------------- | ---------------------------------------- | ------------------------------------------ |
| **Docker Images**  | Tags: `v0.0.2`, `v0.0`, `v0`, `latest`   | Tags: `v0.0.2-beta.1` only               | Line 92: `!contains(github.ref_name, '-')` |
| **CLI Binaries**   | Same binaries                            | Same binaries                            | No difference                              |
| **Helm Chart**     | Version: `0.0.2`                         | Version: `0.0.2-beta.1`                  | Chart.yaml version matches tag             |
| **GitHub Release** | `prerelease: false`, `make_latest: true` | `prerelease: true`, `make_latest: false` | Lines 340, 348, 366-367                    |

## The Critical Line

**Line 218 in `release.yml`**:

```bash
IS_PRERELEASE=$(echo "$VERSION" | grep -qE '-(alpha|beta|rc)' && echo "true" || echo "false")
```

This single line determines:

- ✅ Whether `latest` Docker tag is created
- ✅ Whether GitHub release is marked as pre-release
- ✅ Whether release is set as latest

**Everything else is the same** - same binaries, same chart, same process.

## Visual Flow

```
Tag Created: v0.0.2-beta.1
     ↓
Release Workflow Triggers
     ↓
Line 218: Extract version → "0.0.2-beta.1"
     ↓
Line 218: Check for -(alpha|beta|rc) → IS_PRERELEASE=true
     ↓
┌─────────────────────────────────────┐
│  Build Images (Line 92)              │
│  latest tag: DISABLED (contains '-') │
│  Result: Only v0.0.2-beta.1 tag     │
└─────────────────────────────────────┘
     ↓
┌─────────────────────────────────────┐
│  Build CLI (Lines 128-166)           │
│  Same binaries for all releases     │
└─────────────────────────────────────┘
     ↓
┌─────────────────────────────────────┐
│  Publish Helm (Lines 168-201)       │
│  Chart version: 0.0.2-beta.1        │
└─────────────────────────────────────┘
     ↓
┌─────────────────────────────────────┐
│  Create Release (Lines 333-367)    │
│  prerelease: true                  │
│  make_latest: false                │
└─────────────────────────────────────┘
```

## Key Takeaway

**The tag name is everything.** The version string in the tag (`v0.0.2` vs `v0.0.2-beta.1`) determines:

1. Docker image tagging strategy
2. GitHub release flags
3. Chart version

**All artifacts are built the same way** - the difference is only in:

- Which Docker tags are applied
- Which GitHub release flags are set
