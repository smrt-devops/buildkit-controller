# Performance

This page records how the Rust build measures against its efficiency targets
and how to repeat the measurement.

## Results

Two runs of `cargo xtask perf` on 2026-10-20, each on a fresh kind cluster. The
measured column is the median of the two runs. With two runs the median is the
mean.

| Target | Measured, median of 2 | Run 1 | Run 2 | Verdict |
|---|---|---|---|---|
| Allocation p50 with a warm pod under 150 ms | p50 8.5 ms, p95 15.1 ms, p99 21.6 ms | 8.5 / 15.2 / 26.5 ms | 8.5 / 15.0 / 16.6 ms | Pass |
| Edge connect overhead p99 under 5 ms, 1 connection at a time | p99 +0.0 ms, p50 +0.3 ms | +0.3 / +0.3 ms | -0.3 / +0.2 ms | p99 not resolved on kind; p50 overhead +0.2 to +0.3 ms. Pass on loopback, see below |
| Edge connect overhead p99 under 5 ms, 8 connections at a time | p99 -2.0 ms, p50 +0.5 ms | -2.6 / +0.5 ms | -1.4 / +0.5 ms | p99 not resolved on kind; p50 overhead +0.5 ms in both runs. Pass on loopback, see below |
| Edge throughput at least 1 GiB/s on loopback-class links | edge 310 MB/s, direct 349 MB/s | 343 / 364 MB/s | 276 / 333 MB/s | Not testable on kind. Pass on loopback at 1.6 GiB/s with one stream, see below |
| Idle replica RSS under 40 MiB | 12.9 to 14.3 MiB per replica | 14.3, 13.3 MiB | 12.9, 13.8 MiB | Pass |
| No API-server polling while idle | 0 get or list calls in 300 s | 0 | 0 | Pass |

Each run made 100 allocations and 1000 connects per path and concurrency
level. A p99 over 100 allocations is the second slowest sample, so it moves a
lot between runs. The p50 is stable.

The rig is a single-node kind cluster, kind v0.32.0, in Docker Desktop on an
arm64 Mac with 10 CPUs and 12 GiB given to the Docker VM. The controller runs
two replicas from the edge-verify fixture: restricted PodSecurity, read-only
root filesystem, and the image from `cargo xtask image`.

### Allocation latency

The client is `curl` in a pod in the pool namespace. It sends one HTTP POST to
`/v1/allocations` over a new TLS connection, with a projected `ServiceAccount`
token. The clock stops when the 201 response with the client certificate
has arrived. That is curl's `time_total`, so it includes DNS for the API
Service name, the TCP connect, the TLS handshake, the token check, the claim
on the pod and the certificate issue. Before each batch of 4 allocations the
task waits until at least 4 free worker pods are Ready, so every allocation
finds a warm pod. The task releases the batch with DELETE afterwards.

### Edge connect overhead

Each connect is a fresh `curl --http2` with a client certificate that sends one
request and stops at the first response byte, curl's `time_starttransfer`.
buildkitd answers a plain HTTP/2 GET with 415, and every one of the 8000
connects in the two runs got that 415, so every request reached buildkitd.

- Direct: from a pod in the controller namespace that the worker
  `NetworkPolicy` admits, straight to the worker pod IP on port 1234, with an
  edge client certificate from the internal CA.
- Through the edge: from the client pod in the pool namespace, to the edge
  Service ClusterIP on port 8444, with the allocation's client certificate.

Both paths reach the same worker pod and use an IP address, so neither pays for
a DNS lookup. The overhead is the edge path's percentile minus the direct
path's percentile at the same concurrency. That figure includes the edge's
second TLS handshake to the worker, so it is stricter than a target of "on top
of the TLS handshakes". The p99 difference is negative at concurrency 8
because the direct handshake to buildkitd has a longer tail than the handshake
to the edge. The direct handshake p99 was 6.9 to 9.9 ms against 3.9 to 4.9 ms
for the edge. The p50 difference, +0.2 to +0.3 ms at 1 connection and +0.5 ms at 8, is the better estimate of what
the edge adds.

This is a gap, as throughput is. A negative p99 difference does not show that
the edge meets the p99 target. It shows that the direct path's tail on this
setup is wider than any overhead the edge adds, so the p99 overhead is not
resolved here. `cargo xtask perf` reports such a row as inconclusive, not as a
pass. The loopback benchmark below resolves it with a TLS sink in place of
buildkitd.

### Edge throughput

The task copies a 512 MiB random file into a `FROM scratch` image with
`buildctl build --no-cache`, once through the edge and once direct to the same
worker, as `edge-verify` does. MB/s is 512 MiB over the wall time of the
build, so it includes buildkit's context transfer and snapshot write.

The edge ran at 83 to 94 percent of the direct path. The direct path, which
has no edge in it, peaked at 364 MB/s. On this rig the ceiling is the build
path and the Docker Desktop VM, not the edge. All pods share the VM's CPUs,
every byte crosses the kind node's pod network, buildkitd writes the file
through rootless overlay storage, and through the edge every byte is encrypted
and decrypted twice. A kind cluster cannot show whether the edge reaches
1 GiB/s.

The edge copies with `tokio::io::copy_bidirectional_with_sizes` and 64 KiB
buffers per direction in `crates/server/src/edge/proxy.rs`. Nothing in that
path looks like a limit below 1 GiB/s. The loopback benchmark below measures
the proxy without buildkit or a cluster. No product change was needed.

## Loopback

`cargo xtask perf --loopback` resolves the two targets that kind could not. It
needs no kind and no Docker. The benchmark is
`crates/server/benches/loopback.rs`, built in release mode, and it uses only
the public `edge` API. One process on 127.0.0.1 holds:

- an upstream that stands in for buildkitd: a TLS server with an internal-CA
  serving cert for `<pool>.<namespace>.worker.buildkit.internal` and mandatory
  internal-CA client auth, which sinks what it reads and sends back a requested
  number of bytes;
- the real listener and `ProxyHandler`, with a client cert signed by the client
  CA whose SAN names the allocation, resolved to the upstream through a
  one-pod reflector store;
- a client that talks to either the edge or the upstream directly. Direct uses
  the edge client cert, as the kind rig does.

Client, edge and upstream share one multi-threaded Tokio runtime and the same
cores, so the edge competes with both ends, as it would not in production.
That makes the throughput figure conservative. Connect latency is a full TCP
and mTLS connect plus the first response byte, 2000 samples per path and
concurrency level after a 200-connect warm-up. Throughput moves 512 MiB per run
in total, split across 1 or 8 streams, after one warm-up run.

Three runs on 2026-10-20 on an arm64 Mac, medians:

| Target | Measured, median of 3 | Run 1 / 2 / 3 | Verdict |
|---|---|---|---|
| Edge throughput at least 1 GiB/s | 1 stream: upload 1.62, download 1.60 GiB/s. 8 streams: upload 2.82, download 2.25 GiB/s | upload 1 stream 1.57 / 1.62 / 1.62 | Pass |
| Edge connect overhead p99 under 5 ms, 1 at a time | +0.42 ms (edge p99 0.70 ms, direct 0.28 ms) | +0.47 / +0.42 / +0.42 | Pass |
| Edge connect overhead p99 under 5 ms, 8 at a time | +0.72 ms (edge p99 1.21 ms, direct 0.48 ms) | +0.71 / +0.72 / +0.74 | Pass |

Direct to the upstream, the same runs give 2.74 GiB/s upload and 1.98 GiB/s
download with one stream, and 5.23 and 4.76 GiB/s with eight. The edge keeps
55 to 87 percent of the direct rate with one stream and about half with eight.
Both ends of the direct path run on the same cores as the edge, so the edge's
share of the CPU is what bounds the 8-stream figure.

Connect overhead medians, edge minus direct:

| Concurrency | p50 | p95 | p99 |
|---|---|---|---|
| 1 | +0.38 ms | +0.44 ms | +0.42 ms |
| 8 | +0.63 ms | +0.70 ms | +0.72 ms |

The loopback direct tail is tight (p99 under 0.5 ms), so unlike on kind the
p99 overhead is a real positive number. It is the edge's second handshake to
the upstream plus the proxy hop, and it sits an order of magnitude under the
5 ms target. The throughput runs fall in a 1.57 to 1.62 GiB/s band for one
stream upload, so no profiling or change to the 64 KiB copy buffers in
`crates/server/src/edge/proxy.rs` was needed.

### Idle replica RSS

After the measurements the task releases every allocation, waits until the
pool is back at 5 warm pods, waits one more minute and then runs the idle
window. At the end of the window it reads `VmRSS` from `/proc/<pid>/status` of
each controller container on the kind node, finding the PID with `crictl`.
Both replicas, leader and follower, stayed between 12.9 and 14.3 MiB.

### API requests while idle

The kind cluster runs with an API-server audit policy that logs every request
from the controller's `ServiceAccount` at the `Metadata` level and nothing
else. The task counts the audit events written during a 300-second idle window.
A watch counts once when it starts and every other request counts once when it
completes. Both runs logged the same 159 requests:

| Request | Count | Reason |
|---|---|---|
| update leases | 150 | Leader-election renewals, one every 2 s on average across both replicas. |
| watch buildkitpools, pods, secrets, leases | 9 | Watches that restart when the API server closes them after its watch timeout. |
| get or list, any resource | 0 | |

The renewals are the cost of `Lease`-based leader election and are not
polling. A get or list outside leader election would be polling, and the task
fails the target if it sees one.

## Running it

```bash
RUSTC_WRAPPER="" SCCACHE_DISABLE=1 cargo xtask perf
```

The task needs Docker, kind and kubectl, and the `moby/buildkit:rootless`
image that `edge-verify` also uses. A run takes about 15 minutes, 5 of them in
the idle window. Options:

- `--batches N` sets the number of allocation batches, 4 allocations each.
  The default is 25.
- `--idle-secs S` sets the idle window. The default is 300.

The task creates the kind cluster `bk-perf` with a private kubeconfig under
`target/perf`, never touches `~/.kube/config`, and deletes the cluster on every
exit path. It prints the table and writes it to `target/perf/result-<unix
time>.md`, with the raw allocation samples in `target/perf/alloc-<unix
time>.txt`. The method is in `xtask/src/perf.rs`.

For the loopback benchmark alone, run `cargo xtask perf --loopback`. It takes
about two minutes after the first build and prints the tables above.
