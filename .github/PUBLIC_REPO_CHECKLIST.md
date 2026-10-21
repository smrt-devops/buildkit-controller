# Public repository checklist

Items to confirm before making the repository public.

## Files that must exist

- `LICENSE` (Apache 2.0), `CODE_OF_CONDUCT.md`, `SECURITY.md`, `CONTRIBUTING.md`.
- Issue templates and `pull_request_template.md` under `.github/`.
- Workflows `rust.yml`, `action.yml` and `release.yml` under `.github/workflows/`.
- `renovate.json`, `deny.toml`, `rust-toolchain.toml`.

## Check before the switch

1. `SECURITY.md` lists a mailbox you read (`security@smrt-devops.net`), and `CODE_OF_CONDUCT.md` names a working contact.
2. The README links resolve and the example hostnames use `example.com`.
3. No credentials in history. Run a scanner such as `gitleaks detect` over the full history and review any hit.
4. `cargo deny check` passes, so every dependency license and advisory is accepted.
5. The checks in [GITHUB_SETUP.md](GITHUB_SETUP.md) are done, covering workflow permissions, branch protection and package visibility.
6. `cargo xtask crdgen --check` and `cargo xtask helm-test` pass, so the chart in the repository matches the code.
7. A pre-release tag has produced a signed image, an SBOM and a chart on the GitHub release. Verify the signature with `cosign verify`.

## After the switch

- Enable private vulnerability reporting and Dependabot alerts.
- Enable Discussions if you want questions kept out of issues.
- Pin the repository description: "Ephemeral BuildKit builders for CI on Kubernetes".
