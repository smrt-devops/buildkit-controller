# Workflows

- `rust.yml` (Rust): on every pull request and push to main, runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, `cargo deny check`, the chart checks (`cargo xtask crdgen --check` and `cargo xtask helm-test`), and the release compile (`cargo auditable build` in the pinned builder image) plus an image build that does not push, on amd64 and on arm64.
- `action.yml` (Action): on pushes to main and on pull requests that touch `action/` or `docs/ci/`, tests the JavaScript GitHub Action (including `action/action.yml` as YAML) and runs shellcheck on `docs/ci/buildkit-allocate.sh`.
- `e2e.yml` (E2E): on every pull request and push to main, every kind suite but soak and perf in one job (`cargo xtask kind-all --jobs 3`: one image build, two shared kind clusters, the suites side by side, their logs kept as an artifact), a dry run of `examples/` against the CRDs on a kind cluster, and the real Action job with the job's OIDC token (same-repository pull requests only, `id-token: write`). Nightly and on demand, it runs the long jobs: the certificate soak, the performance rig and `drain-verify --long` (an allocation queued past five minutes). Each nightly leg has its own runner, so none sets `BK_KIND_SUFFIX`, and keeps its results as the `nightly-<n>` artifact. The perf table is a report: a target it misses shows as MISS in the table and does not fail the job.
- `audit.yml` (Audit): nightly `cargo deny check advisories`, so a new RUSTSEC advisory shows up without a code change.
- `docs.yml` (Docs): builds the documentation site (`cargo xtask docs`) on every pull request, which also catches broken links, and deploys it to GitHub Pages from main.
- `release.yml` (Release): on `vX.Y.Z` tags. A gate job requires the tag commit to be on main with green Rust, E2E and Action push runs, requires `Cargo.toml` and `Chart.yaml` to name the tag's version (`v0.2.0-rc1` needs 0.2.0), and reruns `cargo deny check advisories`. Builds linux/amd64 and linux/arm64 natively, pushes the multi-arch index by digest, signs it and the per-arch manifests with cosign (keyless), attaches an SPDX SBOM that lists the Rust crates (built with `cargo auditable`) and SLSA provenance, scans the unsigned digest with a checksum-pinned Trivy binary in a job with read-only package access, signs only after the scan passes, and only then adds the version and `sha-` tags, packages the Helm chart with its own provenance onto the GitHub release, pushes it to GHCR as a signed OCI chart (the version tag is pullable before the signing steps finish, so if they fail, rerun the failed Release run: it re-pushes and re-signs the chart), and moves the `vN` tag that the GitHub Action is published under, only when the new tag is the highest stable version in its major.

The release notes start with the image digest and the commands that verify the image and the chart, then link `CHANGELOG.md` at the tag, then list the merged pull requests GitHub finds. No workflow needs a repository secret beyond the built-in `GITHUB_TOKEN`. Every third-party action is pinned to a full commit SHA with its version in a comment, and Renovate keeps the pins current (see [RENOVATE.md](../RENOVATE.md)).

## Cutting a release

1. Bump the version in one commit: `version` in `Cargo.toml` and the four path-dependency pins in `crates/api/Cargo.toml`, `crates/server/Cargo.toml` and `xtask/Cargo.toml`, then `cargo update -w`, `version` and `appVersion` in `helm/buildkit-controller/Chart.yaml`, then `cargo xtask helm-test --update`. `cargo test` fails while `Chart.yaml` and `Cargo.toml` disagree. Move the `CHANGELOG.md` entries under the new version.
2. Push to main and wait for green push runs of Rust, E2E and Action on that commit.
3. Optionally tag `vX.Y.Z-rc1` first: a pre-release with a signed image and the chart, which does not move `vN`.
4. Tag `vX.Y.Z` on that commit and push the tag.

## Where nightly failures show

A failed nightly run is a failed job and nothing more. Look under Actions, in the `E2E` and `Audit` workflows, filtered with `event:schedule`. The nightly artifacts hold the soak result and client log, the perf table and the `drain-verify --long` log.

