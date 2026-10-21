# Security policy

## Supported versions

Only the latest release receives security fixes.

## Reporting a vulnerability

Do not open a public issue. Email security@smrt-devops.net with the affected version, steps to reproduce, the impact, and a proof of concept if you have one. We reply within 48 hours and send a status update within 7 days. When the fix ships, we publish an advisory and credit you unless you ask us not to.

## What the controller protects

- Callers authenticate with an OIDC token from a configured issuer or a Kubernetes ServiceAccount token with the controller's audience. A pool's `spec.access.rules` then authorizes them, and an empty list denies everyone.
- An allocation certificate lasts as long as the allocation, at most 24 hours. The caller generates the key.
- Each worker pod serves one allocation. The pod runs rootless buildkitd as a non-root user, has its own ServiceAccount with no token mounted, and sits behind a NetworkPolicy that denies private, link-local and cloud-metadata ranges by default (`workerNetwork.deniedCIDRs`).
- Workers accept only mTLS connections from the controller's edge. The edge accepts only certificates signed by the controller's client CA for a live allocation.
- Tokens, private keys, certificate PEMs and Secret data are never logged.

The two CAs and the per-pod worker identity are described in [docs/architecture.md](docs/architecture.md#two-cas).

## What you are responsible for

- The CA Secret (`buildkit-controller-ca`) holds both CA keys. Restrict `get` on it with RBAC.
- Pool access rules. A claim pattern such as `repository_owner: *` lets every GitHub organization use your pool. Match a specific organization or repository.
- Pool `spec.network.egress.allowCIDRs` re-opens network ranges for workers. Open only what builds need.
- A template that sets `runtimeClassName` (gVisor or Kata) adds isolation. Rootless BuildKit needs Unconfined seccomp and AppArmor profiles, so consider it for untrusted builds.
- Rotate the CA before it expires, see [docs/operations.md](docs/operations.md).
