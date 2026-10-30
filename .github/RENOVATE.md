# Renovate configuration

Renovate opens pull requests for dependency updates. The configuration is `renovate.json` at the repository root.

## What Renovate updates

Renovate groups updates by manager, and each group runs before 10am UTC on Mondays.

- Cargo crates form the "Rust dependencies" group. The `kube`, `kube-*` and `k8s-openapi` crates form their own "Kubernetes dependencies" group, and `jsonwebtoken` and `openidconnect` form "OIDC dependencies".
- GitHub Actions form one group, and Renovate keeps the commit-SHA pins current.
- Dockerfile base images form one group with digest pinning.
- Helm chart dependencies form one group.
- Lock files are refreshed on the first day of each month.

## Merging

Renovate merges minor and patch updates by squash only after the required status checks pass, and only once a release is three days old (`minimumReleaseAge`). GitHub Action pins and Docker digests never automerge, because they run with release credentials. Major updates need a manual review. Security vulnerability alerts get the `security` label.

## Changing the behavior

Edit `renovate.json`.

- To change the update schedule, edit the `schedule` field.
- To stop auto-merge, set `automerge` to `false` in the minor and patch rule.
- To handle another package manager, add an entry to `packageRules`.
- To change pull request limits, edit `prConcurrentLimit` and `prHourlyLimit`.

The [Renovate documentation](https://docs.renovatebot.com/) lists every option.
