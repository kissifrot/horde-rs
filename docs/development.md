# Horde development guide

## Build from source

From the repository root, build with stable Rust. Choose the feature set for
the binary you need.

| Capabilities | Build command |
| --- | --- |
| Single-URL tests and local scenarios | `cargo build --release --locked` |
| Distributed CLI with Grunts, controller and preflight | `cargo build --release --locked --features distributed-transport` |
| Local MCP server | `cargo build --release --locked --features mcp` |
| Local MCP server with distributed inventory tools | `cargo build --release --locked --features mcp,distributed-transport` |

Cargo writes the binary to `target/release/horde`. To follow the usage
guides with this binary, add its directory to the current shell's PATH from
the repository root.

```sh
export PATH="$PWD/target/release:$PATH"
horde --version
```

For prebuilt packages and the installer, see the [installation guide](installation.md).

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The library separates the test plan, local execution and report from the CLI. The optional `distributed-offline` feature provides experimental Grunt execution and aggregation. The optional `distributed-transport` feature adds an experimental mutual TLS protocol for loopback or explicitly authorized private control connections. The `grunt` command still runs its HTTP workload on loopback. `--all-features` includes the loopback protocol tests. See [Distributed mode](distributed.md) for the protocol and its scope.

## Local performance benchmark

```sh
cargo build --release --locked
python3 benchmarks/local.py --requests 10000 --repeats 5 --output /tmp/horde-benchmark.json
```

The Python harness compiles a Rust HTTP/1.1 loopback server, warms up, then measures serial, concurrent, no-reuse, rate-limited and same-origin redirect workloads (one or three redirects per request). It also measures one-step and three-step scenarios, including assertions and header extraction, with exact and bounded latency statistics. Single-URL throughput is in logical requests/s; scenario throughput is in iterations/s. Each run, median throughput, binary hashes and environment metadata are recorded; on Linux, the harness also records server CPU and, when GNU `time` is available, client CPU and peak RSS. Use `--binary` or `--server-binary` to select prebuilt binaries, or `python3 benchmarks/local.py --server` to run the server separately. A custom server must implement the same redirect paths and responses.

The server handles up to 128 connections and shares resources with Horde. Either can become a bottleneck; CPU frequency, scheduling and background activity also affect results. Compare repeated runs on the same machine with the same binaries and settings, then profile suspected bottlenecks separately.

GitHub Actions runs five repetitions per workload on pushes and pull requests. The job summary shows medians and ranges; detailed JSON is retained for 90 days as `horde-benchmark-results`. For pull requests, the exact base commit is measured sequentially on the same runner with the pull request's harness. The summary compares compatible workloads; a positive percentage means higher throughput in the pull request. Scenario definitions are hashed to avoid comparing different workloads. `binary_revision` identifies each binary and `git_commit` identifies the harness checkout. If the base cannot run a scenario workload, that workload is marked not comparable; the pull request benchmark must still succeed.

CI comparisons are informational and have no regression threshold. Shared runner variability and run order can affect results; confirm suspected regressions on controlled hardware. Do not use this workflow with privileged pull request events or secrets: it runs pull request code.

## Docker pilot validation

From the repository root on Linux, use a local Docker daemon, Rust, Python 3,
OpenSSL and `ldd`.

```sh
cargo build --release --locked --features distributed-transport
python3 scripts/pilot-docker.py
```

The script builds a temporary image from the local release binary and
HTTP fixture. It allocates private IPs on an internal Docker network,
issues fresh TLS certificates, and starts two Grunts and a controller.
It launches the commands with per-role JSON configuration and passes
the budget and rate directly to the controller.
Each Grunt's HTTP fixture listens on its own loopback interface. No
ports are published.

- Preflight checks both Grunts, then repeats with an incorrect TLS identity
  for Grunt B. It must report A as ready and B as failed. Both loopback HTTP
  fixtures must observe zero connections in both cases.
- The source IP check repeats the successful preflight with the same
  controller certificate and inventory from an unlisted private Docker
  IP. Both Grunts must close the control connection before the TLS
  handshake completes. Neither Grunt sends a `hello` or reaches its HTTP
  target. The JSON report records the source and both failures.
- Rotation uses two CAs, an IP SAN and a DNS SAN. During overlap, both
  controller leaves must reach both Grunts, one with the new certificate
  and one with the old. Removing the old controller leaf must deny it
  even with both roots installed. With only the new root and leaf
  configured, the new credentials must work and the old credentials and
  old-root-only trust must fail. Each preflight checks for zero HTTP
  connections. The pilot stages credentials in separate role directories
  on the Docker host and copies only public CSRs to the signer directory.
- The nominal run checks 10 requests per Grunt, 20 successful responses,
  40 body bytes and populated merged p95/p99. Both HTTP targets record the
  User-Agent on every request. The report checks `Horde-Docker-Pilot/1.0`
  from Grunt A's configuration and the default `Horde/<version>` from Grunt B.
- The interruption run disconnects Grunt B from the Docker network
  after its first HTTP request. It checks that B stops before spending
  its 20-request share, A completes its own 20 requests, and the
  controller reports B as missing with a partial result and an
  unevaluable failure threshold.
- The silent failure run routes B's TLS control stream through a
  test-only TCP relay on the internal network. After B's first workload
  request, the relay pauses both directions while keeping both sockets
  open. The controller must report an I/O timeout and mark B missing
  while A completes exactly its 20-request share. The failure threshold
  remains unevaluable. Docker log timestamps measure the controller
  detection time and the last HTTP connection at B's fixture. The pilot
  allows 0.5–2.5 seconds for controller detection and 3–6.5 seconds until
  the last observed connection at B, with at least two more seconds
  between B's last connection and A's last connection. These windows
  allow for observation and scheduling around the one-second controller
  timeout, three-second Grunt frame timeout and two-second reconnection
  grace.
- The maintenance run lowers both retention limits on Grunt B. It checks
  both saturation reasons and their HTTP effects, stops B with SIGTERM,
  and verifies that only B's epoch changes. A fresh two-request run then
  completes with one request on each Grunt.
- The controller stop scenarios send `SIGINT` and `SIGKILL` with both
  Grunts running. `SIGINT` must exit 130 with a partial report confirming
  both cancellations. `SIGKILL` must exit 137 with no controller report.
  For both signals, the Docker pilot observes the HTTP fixtures for seven
  seconds after sending the signal. Each Grunt must leave part of its
  20-request share unused and remain quiet for at least three seconds.
  The last HTTP connection must occur by 1.5 seconds after `SIGINT` or
  four seconds after `SIGKILL`. The pilot finishes with an authenticated
  preflight to verify that both Grunts are still reachable.
- The inventory scenario adds a second Grunt during a one-peer run,
  verifies that the next run uses both peers, and removes the original
  Grunt before the following run.

The script removes its containers, network, image and temporary
credentials on exit, including on failure, SIGINT or SIGTERM. It writes
the combined report to `target/pilot-docker-report.json` on success.
Use `--output` to choose another location. The `distributed-pilot`
GitHub Actions job runs the same script and uploads the JSON report
as the `horde-distributed-pilot` artifact.

This exercises separate IPs and TLS identities on a single kernel.
Routing, firewalls and physical link detection remain checks for the
multi-host pilot.

## Pacing internals

The scheduler normally waits on Tokio’s timer. The timer cutoff is the larger of 1,000 starts/s per process and a worker-based allowance. The allowance is 600 starts/s per worker for up to 2 local workers, 550 for 3–4, 525 for 5–7, and 500 for 8 or more. Above the cutoff, it uses a shared admission clock and yields until the next start. Each Grunt applies the cutoff to its own share. This avoids millisecond rounding at high rates, at a higher CPU cost. Missed slots do not accumulate in this path.

## Fuzzing the offline histogram decoder

The `fuzz/` project exercises the experimental distributed histogram decoder with
coverage-guided libFuzzer. Install `cargo-fuzz`, a nightly Rust toolchain and a C++
compiler. From the repository root, for example:

```sh
cargo install cargo-fuzz --locked
rustup toolchain install nightly --profile minimal
mkdir -p /tmp/horde-fuzz-decode /tmp/horde-fuzz-generated
cargo +nightly fuzz run histogram_decode /tmp/horde-fuzz-decode fuzz/corpus/histogram_decode -- -max_len=506964 -max_total_time=3600 -timeout=15 -rss_limit_mb=2048
cargo +nightly fuzz run histogram_generated /tmp/horde-fuzz-generated fuzz/corpus/histogram_generated -- -max_len=1024 -max_total_time=3600 -timeout=15 -rss_limit_mb=2048
```

On systems where LeakSanitizer cannot inspect processes, prefix the commands with `ASAN_OPTIONS=detect_leaks=0`.

The first corpus directory receives new inputs; retain it across runs. The second
contains the checked-in seeds. Use `--dev` before the target name for a debug
build; the default uses optimizations, debug assertions, overflow checks and
AddressSanitizer. Crash inputs are saved under `fuzz/artifacts/`; reproduce one
with `cargo +nightly fuzz run histogram_decode path/to/artifact` (substitute the
other target when appropriate). Preserve and minimize findings, then add a
regression test. The raw target checks arbitrary envelopes and receiver limits;
the generated target constructs valid histograms and checks decoded results
against the pinned HDR library. These campaigns do not prove bounded memory or
replace the security and transport work required before exposing a listener.

## Release packages

The [release workflow](../.github/workflows/linux-packages.yml) builds
Horde on Debian 13 with static musl for AMD64 and ARM64. It checks package
installation and the distributed CLI on Debian 13 and Ubuntu 24.04
on both architectures. Each build includes `grunt`, `preflight` and
`controller`.

The release workflow also builds and runs the macOS binary on a native
Apple Silicon runner. It uses ad-hoc signing without Apple notarization.

To prepare a prerelease, create and push a `vX.Y.Z-rc.N` tag on the
commit to ship. Its base version must match `Cargo.toml`. For example:

```sh
git tag -a v0.1.0-rc.1 -m "Horde 0.1.0 release candidate 1"
git push origin v0.1.0-rc.1
```

After the install checks, GitHub Actions publishes the two Linux packages,
three standalone binaries and `SHA256SUMS` as a prerelease. The workflow
also supports manual runs and keeps snapshot outputs as workflow
artifacts. The build pins the Rust container and musl toolchain versions,
uses `Cargo.lock` and sets package timestamps from the commit.
