# Workflows

CI stays small so it fits on free GitHub runners. The full kind sweep, the soak and the perf rig run locally when a release is cut.

- `rust.yml` (Rust): on every pull request and push to main. Jobs `fmt`, `clippy`, `test`, `deny` (`cargo deny check`) and `chart` (`cargo xtask crdgen --check` and `cargo xtask helm-test` with a checksum-pinned helm).
- `e2e.yml` (E2E): on every pull request and push to main. Job `e2e` runs `cargo xtask kind-all --only e2e`: one kind cluster and one end-to-end build through the chart, with the suite log kept as an artifact. Job `examples` dry-runs `examples/` against the CRDs on a kind cluster.
- `action.yml` (Action): on pushes to main and on pull requests that touch `action/` or `docs/ci/`. Tests the JavaScript GitHub Action (including `action/action.yml` as YAML) and runs shellcheck on `docs/ci/buildkit-allocate.sh`.
- `docs.yml` (Docs): builds the documentation site (`cargo xtask docs`) on every pull request, which also catches broken links, and deploys it to GitHub Pages from main.
- `release.yml` (Release): on `vX.Y.Z` tags. A gate job requires the tag commit to be on main with green Rust, E2E and Action push runs, requires `Cargo.toml` and `Chart.yaml` to name the tag's version (`v0.2.0-rc1` needs 0.2.0), and reruns `cargo deny check advisories`. Then it builds linux/amd64 and linux/arm64 natively, pushes the multi-arch index by digest, scans the unsigned digest with a checksum-pinned Trivy binary in a job with read-only package access, signs the index and the per-arch manifests with cosign (keyless) only after the scan passes, attaches an SPDX SBOM that lists the Rust crates (built with `cargo auditable`) and SLSA provenance, and only then adds the version and `sha-` tags. It packages the Helm chart with its own provenance onto the GitHub release and pushes it to GHCR as a signed OCI chart. The version tag is pullable before the signing steps finish, so if they fail, rerun the failed Release run: it re-pushes and re-signs the chart. Last, it moves the `vN` tag that the GitHub Action is published under, only when the new tag is the highest stable version in its major.

Rust, E2E and Action cancel their older in-progress run when a newer push or pull request update lands on the same ref. The release gate counts only successful runs on the tag commit, so a commit whose runs were cancelled must be rerun before it is tagged.

The release notes start with the image digest and the commands that verify the image and the chart, then link `CHANGELOG.md` at the tag, then list the merged pull requests GitHub finds. No workflow needs a repository secret beyond the built-in `GITHUB_TOKEN`. Every third-party action is pinned to a full commit SHA with its version in a comment, and Renovate keeps the pins current (see [RENOVATE.md](../RENOVATE.md)).

## Cutting a release

1. Bump the version in one commit: `version` in `Cargo.toml` and the four path-dependency pins in `crates/api/Cargo.toml`, `crates/server/Cargo.toml` and `xtask/Cargo.toml`, then `cargo update -w`, `version` and `appVersion` in `helm/buildkit-controller/Chart.yaml`, then `cargo xtask helm-test --update`. `cargo test` fails while `Chart.yaml` and `Cargo.toml` disagree. Move the `CHANGELOG.md` entries under the new version.
2. Push to main and wait for green push runs of Rust, E2E and Action on that commit.
3. On that commit, run the full kind sweep locally: `cargo xtask kind-all`. For a release that touches the edge or the certificates, also run `cargo xtask kind-all --only soak,perf`. For one that touches shutdown or draining, run `cargo xtask drain-verify --long`.
4. Tag `vX.Y.Z-rc1` first: a pre-release with a signed image and the chart, which does not move `vN`. Pull requests build no release image, so this is the first run of the arm64 and `cargo auditable` builds.
5. Tag `vX.Y.Z` on that commit and push the tag.
