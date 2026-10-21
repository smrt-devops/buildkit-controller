# GitHub repository setup

The settings the workflows in `.github/workflows/` need. See [workflows/README.md](workflows/README.md) for what each workflow does.

## Required

### Workflow permissions

Under Settings, Actions, General, Workflow permissions, choose "Read repository contents and packages permissions". Each workflow requests the extra permissions it needs (`packages: write`, `id-token: write`, `attestations: write`, `contents: write`) in its own `permissions` block.

### Branch protection for main

Under Settings, Branches, add a rule for `main` that requires a pull request with one approval, requires status checks to pass with the branch up to date, and dismisses stale approvals. Add these required checks from the `Rust` workflow, `fmt`, `clippy`, `test`, `deny` and `Image build (no push)`. The `Action` workflow runs only when `action/` or `docs/ci/` change, so do not require it.

### Tag ruleset for releases

Under Settings, Rules, Rulesets, add a tag ruleset that targets `v*.*.*`, with enforcement active. Turn on "Restrict creations", "Restrict updates", "Restrict deletions" and "Block force pushes", and put the Repository admin role in the bypass list. The pattern covers release tags such as `v0.1.0` and `v0.1.0-rc1`, and leaves the major tags (`v0`, `v1`) out, because the `action-tag` job force-moves them with the workflow token. GitHub accepts the GitHub Actions app as a bypass actor only on organization repositories, so on a personal account the major tags have to stay outside the ruleset. The release workflow also checks that the tagged commit is on main and has a green `Rust` run, but only the ruleset stops a stray release tag from being created.

### Package visibility

The release workflow pushes `ghcr.io/smrt-devops/buildkit-controller`. The package takes the repository's visibility when the workflow creates it, so on a public repository users can pull without credentials. If the first release ran while the repository was private, set the package to public under the package's settings.

## Optional

- Renovate. Install the [Renovate app](https://github.com/apps/renovate) for the repository. The configuration is `renovate.json`, described in [RENOVATE.md](RENOVATE.md).
- Dependabot alerts and security updates. Enable them under Settings, Security.
- Private vulnerability reporting. Enable it under Settings, Security, so reporters can also use GitHub's advisory form beside the address in [SECURITY.md](../SECURITY.md).

## No secrets needed

The workflows use only the built-in `GITHUB_TOKEN`. Cosign signs keylessly with the workflow's OIDC identity, so there are no signing keys or registry passwords to store.

## Test the setup

1. Open a pull request. The `Rust` workflow must run `fmt`, `clippy`, `test`, `deny` and the image build, and branch protection must block the merge until they pass.
2. Push a pre-release tag such as `v0.0.0-test1`. The `Release` workflow must publish a GitHub pre-release with the chart attached and must not move the `v0` tag, because pre-release tags are skipped by the `action-tag` job. Delete the tag, the release and the test image afterwards.

## Troubleshooting

- A required check never reports. Branch protection matches job names exactly, so copy them from a finished run.
- The release fails at the registry push. Check that the package, if it already exists, grants this repository write access under the package's "Manage Actions access".
- The `Rust` workflow fails on formatting. Run `cargo fmt --all` and commit.
