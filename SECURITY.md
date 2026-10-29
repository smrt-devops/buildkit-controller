# Security policy

## Supported versions

Only the latest release receives security fixes.

## Reporting a vulnerability

Do not open a public issue. Email security@smrt-devops.net with the affected version, steps to reproduce, the impact, and a proof of concept if you have one. We reply within 48 hours and send a status update within 7 days. When the fix ships, we publish an advisory and credit you unless you ask us not to.

## What the controller protects

- Callers authenticate with an OIDC token from a configured issuer or a Kubernetes ServiceAccount token with the controller's audience. A pool's `spec.access.rules` then authorizes them, and an empty list denies everyone.
- An allocation certificate lasts as long as the allocation, at most 24 hours. The caller generates the key. A BuildKitClaim is the exception: the controller generates its key and writes it to the claim's Secret in the claim's namespace.
- Each worker pod serves one allocation. The pod runs rootless buildkitd as a non-root user, has its own ServiceAccount with no token mounted, and sits behind a NetworkPolicy that denies private ranges by default (`workerNetwork.deniedCIDRs`) and the link-local and cloud-metadata ranges always.
- Workers accept only mTLS connections from the controller's edge. The edge accepts only certificates signed by the controller's client CA for a live allocation. The edge checks that the certificate's pod exists and still holds the allocation while it verifies the certificate, and rustls does that before it checks the client's proof of the private key. Someone with a copy of an allocation certificate but not its key can therefore learn from the TLS alert whether that allocation is still live, and can add to its refusal metrics. The handshake still fails without the key, so this gives no access.
- The build cache is reachable only by worker pods, and mTLS is the boundary (a NetworkPolicy adds a layer). Each worker has a clientAuth-only registry leaf from the client CA, separate from its serverAuth serving certificate, and no certificate a worker holds opens a buildkitd. The cache registry reads the calling pod's live allocation and allows a pull from that caller's own cache scope and a push only when the pool trusts the caller. A build cannot read or write another scope's cache, and a build from `pull_request`, `pull_request_target`, `workflow_run`, `issue_comment`, `pull_request_review`, `pull_request_review_comment`, `deployment` or `deployment_status`, or a GitLab merge request or child pipeline, can read its repository's cache but never writes it, whatever the pool's `writers` say.
- Tokens, private keys, certificate PEMs and Secret data are never logged.

The two CAs and the per-pod worker identity are described in [docs/architecture.md](docs/architecture.md#two-cas).

## What you are responsible for

- The CA Secret (`buildkit-controller-ca`) holds both CA keys, and every controller replica loads them. Restrict `get` on the Secret with RBAC, and treat a compromise of any replica as a compromise of both CAs: rotate them.
- Pool access rules. A claim pattern such as `repository_owner: *` lets every GitHub organization use your pool. Match a specific organization or repository.
- Who may write the cache. By default pushes, schedules and manual runs on a protected ref, ServiceAccounts and claims write, and pull requests only read. Anyone who may patch pods in a pool namespace can write any cache of that pool. A pool's `spec.workers.cache.writers` changes that, and `spec.workers.cache.scope: pool` makes all its callers share one cache. Widen either only for callers you trust equally, because a trusted build can plant what an untrusted build of the same cache then reads.
- Pool `spec.network.egress.allowCIDRs` re-opens network ranges for workers. Open only what builds need.
- Untrusted builds, such as fork pull requests. Workers run with seccomp and AppArmor Unconfined and no process sandbox, so each `RUN` step has a larger kernel attack surface on a shared node. Use a dedicated tainted node pool or a `runtimeClassName` in the pool template, as described in [Running untrusted builds](docs/operations.md#running-untrusted-builds).
- Who may create or edit a `BuildKitPool`. A pool author is as trusted as anyone who can create pods in its namespace, see [Who may write a pool](docs/operations.md#who-may-write-a-pool).
- The ops port (8080: metrics and health) has no authentication. Restrict it with a NetworkPolicy.
- Rotate the CA before it expires, see [docs/operations.md](docs/operations.md).
