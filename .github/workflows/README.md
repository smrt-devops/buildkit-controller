# Workflows

- `rust.yml` (Rust): on every pull request and push to main, runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, `cargo deny check`, the chart checks (`cargo xtask crdgen --check` and `cargo xtask helm-test`), and the release compile (`cargo auditable build` in the pinned builder image) plus an image build that does not push, on amd64 and on arm64.
- `action.yml` (Action): on pushes to main and on pull requests that touch `action/` or `docs/ci/`, tests the JavaScript GitHub Action (including `action/action.yml` as YAML) and runs shellcheck on `docs/ci/buildkit-allocate.sh`.
- `e2e.yml` (E2E): on every pull request and push to main, every kind suite but soak and perf in one job (`cargo xtask kind-all --jobs 3`: one image build, two shared kind clusters, the suites side by side, their logs kept as an artifact), a dry run of `examples/` against the CRDs on a kind cluster, and the real Action job with the job's OIDC token (same-repository pull requests only, `id-token: write`). Nightly and on demand, it runs the long jobs: the certificate soak, the performance rig and `drain-verify --long` (an allocation queued past five minutes). Each nightly leg has its own runner, so none sets `BK_KIND_SUFFIX`, and keeps its results as the `nightly-<n>` artifact. The perf table is a report: a target it misses shows as MISS in the table and does not fail the job.
- `audit.yml` (Audit): nightly `cargo deny check advisories`, so a new RUSTSEC advisory shows up without a code change.
- `docs.yml` (Docs): builds the documentation site (`cargo xtask docs`) on every pull request, which also catches broken links, and deploys it to GitHub Pages from main.
- `release.yml` (Release): on `vX.Y.Z` tags. A gate job requires the tag commit to be on main with green Rust, E2E and Action push runs, requires `Cargo.toml` and `Chart.yaml` to name the tag's version (`v0.2.0-rc1` needs 0.2.0), and reruns `cargo deny check advisories`. Builds linux/amd64 and linux/arm64 natively, pushes the multi-arch index by digest, signs it and the per-arch manifests with cosign (keyless), attaches an SPDX SBOM that lists the Rust crates (built with `cargo auditable`) and SLSA provenance, scans the unsigned digest with a checksum-pinned Trivy binary in a job with read-only package access, signs only after the scan passes, and only then adds the version and `sha-` tags, packages the Helm chart with its own provenance onto the GitHub release, and moves the `vN` tag that the GitHub Action is published under, only when the new tag is the highest stable version in its major.

The release notes start with the image digest and the commands that verify the image and the chart, then link `CHANGELOG.md` at the tag, then list the merged pull requests GitHub finds. No workflow needs a repository secret beyond the built-in `GITHUB_TOKEN`. Every third-party action is pinned to a full commit SHA with its version in a comment, and Renovate keeps the pins current (see [RENOVATE.md](../RENOVATE.md)).

## Cutting a release

1. Bump the version in one commit: `version` in `Cargo.toml` and the four path-dependency pins in `crates/api/Cargo.toml`, `crates/server/Cargo.toml` and `xtask/Cargo.toml`, then `cargo update -w`, `version` and `appVersion` in `helm/buildkit-controller/Chart.yaml`, then `cargo xtask helm-test --update`. `cargo test` fails while `Chart.yaml` and `Cargo.toml` disagree. Move the `CHANGELOG.md` entries under the new version.
2. Push to main and wait for green push runs of Rust, E2E and Action on that commit.
3. Optionally tag `vX.Y.Z-rc1` first: a pre-release with a signed image and the chart, which does not move `vN`.
4. Tag `vX.Y.Z` on that commit and push the tag.

## Where nightly failures show

A failed nightly run is a failed job and nothing more. Look under Actions, in the `E2E` and `Audit` workflows, filtered with `event:schedule`. GitHub emails a failed scheduled run only to the account that last changed its `cron` line, so that account needs "Actions" notifications on (see below). The nightly artifacts hold the soak result and client log, the perf table and the `drain-verify --long` log.

## Repository settings

These are set in the GitHub UI. The workflows behave safely without them, but each item says what goes wrong until it is set.

### Workflow permissions

Settings, Actions, General, Workflow permissions: "Read repository contents and packages permissions". Each workflow requests the extra permissions it needs (`packages: write`, `id-token: write`, `attestations: write`, `contents: write`) in its own `permissions` block.

### Branch protection for main

Settings, Branches, rule for `main`: require a pull request with one approval, dismiss stale approvals, require status checks to pass with the branch up to date. Required checks, named exactly as a finished run shows them:

- Rust: `fmt`, `clippy`, `test`, `deny`, `chart`, `Image build amd64 (no push)`, `Image build arm64 (no push)`.
- E2E: `kind-all`, `examples`.

Remove the old `Image build (no push)` check, or every pull request waits for it. Do not require `action`, which is skipped on fork pull requests, or the `Action` workflow, which runs only when `action/` or `docs/ci/` change. With `kind-all` required, a Renovate or Dependabot pull request cannot automerge until the kind suites pass. Until this is set, the release gate still refuses any commit whose push runs are red, but main can hold a commit that cannot be released.

### Tag rulesets

Settings, Rules, Rulesets, new tag ruleset "release tags", enforcement Active:

- Target `refs/tags/v*`. That covers the release tags and the `v0` tag that users of `smrt-devops/buildkit-controller/action@v0` run.
- Turn on "Restrict creations", "Restrict updates", "Restrict deletions" and "Block force pushes".
- Bypass list: the Repository admin role and, when the repository belongs to an organization, the GitHub Actions app, so the `action-tag` job can move `v0`.

On a personal account the GitHub Actions app cannot be a bypass actor. Then the `action-tag` job's push is refused and the job fails after everything else is published. Move the tag yourself once the release run is otherwise green: `git tag -fa v0 -m "Release vX.Y.Z" vX.Y.Z^{} && git push --force origin refs/tags/v0`. Until `v0` is in a ruleset, anyone with push access can move it, and every `@v0` user runs what it points at; pinning the Action to `@vX.Y.Z` or a commit SHA avoids that.

### GitHub Pages

Settings, Pages, Source: "GitHub Actions". Until then the `deploy` job of every Docs push run fails at `configure-pages`. It is not part of the release gate.

### Package visibility

The release workflow pushes `ghcr.io/smrt-devops/buildkit-controller`. After the first release, set the package visibility to public and link it to the repository under the package's settings, so users can pull without credentials. If the package already exists, grant this repository write access under the package's "Manage Actions access", or the release fails at the registry push.

### Notifications and security

- Renovate: install the [Renovate app](https://github.com/apps/renovate) for the repository. Without it, nothing updates the pinned actions.
- Your own Settings, Notifications, System, Actions: on, so failed scheduled runs reach you by email.
- Settings, Security: Dependabot alerts, Dependabot security updates and private vulnerability reporting on.

### Test the setup

1. Open a pull request. Branch protection must block the merge until the required checks above pass.
2. Push a pre-release tag for the version in the tree, such as `v0.2.0-test1`. The Release workflow must publish a GitHub pre-release with the chart attached and must not move `v0`. Delete the tag, the release and the test image afterwards.
