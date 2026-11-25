# Development and release notes

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
`controller`. Installation places `horde` in `/usr/bin` and starts no service.

The release workflow also builds and runs the macOS binary on a native
Apple Silicon runner. It uses ad-hoc signing without Apple notarization.

Get the package or standalone executable for your architecture from
[GitHub Releases](https://github.com/kissifrot/horde-rs/releases). Use
`dpkg --print-architecture` to check whether to select `amd64` or `arm64`.
Download `SHA256SUMS` alongside the asset and verify it before installing.
For the immutable `v0.1.0-rc.1` release, GitHub changed the `~` in
the package asset names to a dot. Normalize those two names in the
checksum input when verifying this release. For example, on AMD64:

```sh
sed 's/~rc\./.rc./g' SHA256SUMS | sha256sum --ignore-missing -c -
sudo apt install './horde-rs_0.1.0.rc.1_amd64.deb'
horde --version
```

For later prereleases, verify with `sha256sum --ignore-missing -c SHA256SUMS`.

The standalone file contains the same executable as the package.
It can be run directly with `chmod +x horde-linux-amd64` followed by
`./horde-linux-amd64 --version`.

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
