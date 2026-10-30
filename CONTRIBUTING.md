# Contributing

Read [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) first. Contributions are licensed under Apache 2.0.

## Reporting bugs and requesting features

Search the [existing issues](https://github.com/smrt-devops/buildkit-controller/issues) first, then use the [bug report](.github/ISSUE_TEMPLATE/bug_report.md) or [feature request](.github/ISSUE_TEMPLATE/feature_request.md) template. A bug report needs the controller and Kubernetes versions, the steps to reproduce, the expected and actual behavior, and relevant logs. Report security problems as described in [SECURITY.md](SECURITY.md), not in public issues.

## Development setup

You need the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it on first use), Docker, `kubectl`, `kind` and `helm`. The last three are only for the cluster checks.

```sh
git clone https://github.com/smrt-devops/buildkit-controller.git
cd buildkit-controller
cargo build --workspace --locked
```

The workspace has `crates/api` (CRD and API types, and the HTTP client), `crates/server` (the `buildkit-controller` binary), and `xtask` (the task runner). The GitHub Action in `action/` is dependency-free JavaScript, and `action/dist` is its hand-written source with no build step. Edit it there and run `(cd action && node --test test/*.test.js)`.

## Checks

Run these before opening a pull request. CI also runs `cargo xtask crdgen --check`, `cargo xtask helm-test`, the release image build on amd64 and arm64, and `cargo xtask kind-all --jobs 3`.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check
```

Clippy denies `unwrap`, `expect` and `unsafe` outside tests. Use `thiserror` for errors in library crates, `anyhow` in binaries, and `tracing` with structured fields for logs.

`cargo xtask --help` lists the other tasks.

| Task | What it does |
|---|---|
| `cargo xtask crdgen` | Regenerates the CRD YAML in the Helm chart. Never edit that file by hand. `--check` fails when it is stale. |
| `cargo xtask helm-test` | Lints the chart and compares `helm template` output with golden files. `--update` rewrites them. |
| `cargo xtask kind-all` | Runs every kind suite below but soak and perf in one go, as CI does: one image build, two shared clusters (`bk-all` for most suites side by side, `bk-solo` for `runtime-verify` then `claim-verify`), each suite's output in `target/kind-all/<suite>.log` and a timing table at the end. `--only e2e,helm-verify` runs a subset, `--jobs N` limits how many run at once, `--keep` leaves the clusters running and `--down` deletes them. |
| `cargo xtask crd-verify`, `render-verify` | Apply the CRD and sample pools to a throwaway kind cluster with server-side dry run. |
| `cargo xtask alloc-verify`, `api-verify`, `reconcile-verify`, `runtime-verify`, `edge-verify`, `helm-verify` | End-to-end checks on a throwaway kind cluster. They use a private kubeconfig and never touch `~/.kube/config`. |
| `cargo xtask claim-verify` | Installs the chart with claims on kind and checks the `BuildKitClaim` lifecycle: queue order, `QueueTimeout`, a real build through a claim Secret, refusal, expiry and a lost worker. |
| `cargo xtask registry-verify` | Runs the cache registry on kind with real buildkitd workers: a read-write worker exports, a fresh read-only worker gets cached steps, and a push from the read-only worker is refused. |
| `cargo xtask soak` | Soaks the certificate lifecycle on kind: continuous builds through leaf renewals, a serving certificate swap, a leader failover, and deletion and corruption of the CA Secret. |
| `cargo xtask drain-verify` | Installs the chart with two replicas on kind and checks that stopping a replica never cuts a live build. `--long` runs the 390 s build that shows a drain past the old 300 s budget (nightly in CI). It publishes the controller on host ports 28443 and 28444, or, with `BK_KIND_SUFFIX` set, on a pair in 22000 to 23999 derived from the suffix. |
| `cargo xtask proxy-verify` | Installs the chart on kind behind the pinned Envoy Gateway with PROXY protocol on, trusting the Envoy pod addresses. Checks that the logs and the per-address caps see client pod addresses, that a build through the edge TLSRoute works, and that an untrusted pod cannot forge its address with its own PROXY header. It reaches the Gateway through `kubectl port-forward`, so it binds no fixed host port. |
| `cargo xtask e2e` | The end-to-end suite on kind: the chart behind Envoy Gateway, with real `docker buildx` builds through it. It pulls the pinned Envoy Gateway chart, Gateway API CRDs and images on the host and loads the images into kind, and publishes the Gateway on host ports 18443 and 18444. With `BK_KIND_SUFFIX` set it uses a port pair derived from the suffix (20000 to 21999) and the work directory `target/e2e-<suffix>`, so runs with different suffixes can share a host. `e2e-up` installs the same cluster, leaves it running, and writes the API URL to `api-url` in the work directory. |
| `cargo xtask perf --loopback` | Runs the in-process loopback edge benchmark. It needs no kind cluster and no Docker. Plain `perf` uses the kind rig. |
| `cargo xtask image` | Builds the controller image for your architecture. Every task tags the image by a hash of its inputs and skips the build when nothing changed. `--load-kind <cluster>` also loads it into kind. If Docker has no network, `BK_IMAGE_PREINSTALLED=1` builds with the toolchain the rust image ships instead of the pinned one; use it for local builds only. |
| `cargo xtask docs` | Builds the documentation site with mdBook into `target/book/html` and fails on a broken link to a repository file. Needs `mdbook` on the PATH. |

Every task that creates a kind cluster names it `bk-<task>`. Set `BK_KIND_SUFFIX=foo` to get `bk-<task>-foo` instead, so parallel checkouts do not share a cluster. To run a suite on a cluster that exists, such as one `kind-all --keep` left, set `BK_KIND_CLUSTER=<name>`: the task attaches to that kind cluster (`kind export kubeconfig` into its private kubeconfig), skips creating and deleting it, and skips loading images that are there already. A suite can run once per cluster that way. The tasks hand the cluster's kubeconfig to the ignored cargo tests in `BK_TEST_KUBECONFIG`, and those tests read no other kubeconfig. To run one by hand, point `BK_TEST_KUBECONFIG` at a `kind-*` context whose API server is on loopback; the test refuses anything else. A task also stops its child processes and deletes its cluster on SIGINT or SIGTERM.

The image build keeps its cache in a Docker volume named from a hash of the checkout. `cargo xtask image --prune` removes the volumes of checkouts that no longer exist.

## Pull requests

1. Branch from `main`.
2. Add a test that fails without your change.
3. Update the docs and `examples/` when behavior changes. The CRD doc comments in `crates/api` are the field reference, so regenerate the CRD with `cargo xtask crdgen` when you change them.
4. Use [Conventional Commits](https://www.conventionalcommits.org/) for messages, for example `fix(server/edge): close connections when a pod disappears`.
5. Open the pull request, fill in the template and link the issue.

A pull request needs one maintainer approval and passing CI. If you change the CRD types, run `cargo xtask crdgen` and commit the result.

## Releases

Maintainers bump the version, then tag `vX.Y.Z`. The release workflow refuses a tag whose version differs from `Cargo.toml` and `Chart.yaml`, then builds the image for amd64 and arm64, signs it, and publishes the chart and the release notes. The steps are in [.github/workflows/README.md](.github/workflows/README.md#cutting-a-release).
