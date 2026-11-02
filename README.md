# Horde

For the Horde! Horde is a standalone HTTP load-testing tool written in Rust.

## Ready to work: install and run

Install the current stable Rust toolchain, then run:

```sh
cargo build --release
./target/release/horde -n 1000 -c 10 https://example.com/
./target/release/horde -n 100 -c 5 -t 10 -X GET -H 'Accept: application/json' https://example.com/
```

Use `horde --help` for all options. Only test services you are authorized to load test.

## Assemble the Horde: command-line options

Usage: `horde [OPTIONS] <URL>`

| Argument or option | Description | Default |
| --- | --- | --- |
| `<URL>` | Target HTTP or HTTPS URL (required). | — |
| `-n, --requests <REQUESTS>` | Total number of requests; must be positive. | `1` |
| `-c, --concurrency <CONCURRENCY>` | Maximum concurrent requests; must be positive. | `1` |
| `-t, --timeout <TIMEOUT>` | Timeout per request, in seconds; must be positive. | `30` |
| `-X, --method <METHOD>` | HTTP method. | `GET` |
| `-H, --header <HEADERS>` | HTTP header as `Name: value`; may be repeated. | None |
| `-A, --user-agent <USER_AGENT>` | Set the `User-Agent` header; overrides a `User-Agent` set with `-H`. | `Horde/<version>` (e.g. `Horde/0.1.0`) |
| `--body-file <BODY_FILE>` | Read a request body from a file and reuse it for every request. | No body |
| `--duration <DURATION>` | Stop starting new requests after this many seconds; must be positive. | No duration limit |
| `--rate <RATE>` | Target rate of logical requests per second, from 1 to 1,000,000,000. | Unlimited |
| `--no-connection-reuse` | Disable reuse of HTTP connections. | Reuse enabled |
| `--max-failure-rate <PERCENT>` | Maximum failed request percentage, from 0 to 100. | Disabled |
| `--max-p95-ms <MILLISECONDS>` | Maximum p95 latency in milliseconds; must be non-negative. | Disabled |
| `--json` | Print a JSON summary instead of the text report. | Disabled |
| `-L, --follow-redirects` | Follow HTTP redirects. | Disabled |
| `--max-redirects <MAX_REDIRECTS>` | Maximum redirects per request; positive and requires `-L`. | `10` |
| `-h, --help` | Show help. | — |
| `-V, --version` | Show version. | — |

## Work complete: the report

For example, `horde -X POST --body-file payload.json -H 'Content-Type: application/json' --duration 30 --json https://example.com/` runs for up to 30 seconds and prints JSON. The body file is read into memory once and reused; the HTTP method is not changed automatically. Redirects use the HTTP client's standard method and body rules (for example, a 307 preserves them).

With `--duration`, Horde stops starting new requests at the deadline and lets in-flight requests finish, so elapsed time may exceed the duration. With both `--duration` and `-n`, the first limit reached stops new requests. Without either option, Horde sends one request. A request timeout still applies separately to each request.

`--rate` spaces the starts of logical requests across all workers; for example, `--rate 100 -c 10` targets 100 starts per second. Concurrency and response times can reduce the achieved rate, and operating-system scheduling can affect spacing and cause short bursts. Redirects are part of a logical request and are not separately rate-limited. Time spent waiting for a rate slot is excluded from request latency and timeout; this closed-loop limit does not account for latency from requests that could not be started while workers were busy.

For CI, use e.g. `--max-failure-rate 1 --max-p95-ms 250`. Failure rate is `failed / completed * 100`; p95 uses the same nearest-rank latency samples as the report, including failed requests. A value equal to the limit passes. The full, unrounded values are compared. If a configured threshold is exceeded or cannot be evaluated because no requests completed, Horde prints the report and exits with code 2. JSON output includes the configured limits, observed values and pass/fail results under `thresholds`; an unconfigured threshold is `null`. Without thresholds, individual request failures do not change the exit code.

Each worker performs requests sequentially, up to the configured concurrency. Connections may be reused. Redirects are not followed by default; use `-L` or `--follow-redirects` to follow up to 10 redirects per request. Set a different positive limit with `-L --max-redirects 5`. Exceeding that limit counts as a failure. The timeout covers the entire request, including redirects and reading the final response body. Responses with HTTP status 4xx or 5xx, transport errors, timeouts, and body-read errors count as failures. Response bytes count body bytes received from the final response, including partial bodies. Completed requests include failures. Invalid configuration or an internal error returns a nonzero exit code.

By default Horde can reuse connections; the server may still close them. `--no-connection-reuse` disables the idle connection pool and sends `Connection: close`, overriding a conflicting custom Connection header. The report and JSON `connections_opened` field count connections successfully established by the HTTP connector throughout the run, including connections for redirects or requests that later fail. This is a cumulative count, not the number still open at the end. Failed connection or TLS setup attempts are not counted. Proxy connections, if used, are counted at the connector level.

The report includes the total number of redirects accepted for following, including redirects in requests that subsequently fail, and the number of requests that exceeded the redirect limit. Limit failures are also included in failed requests. The count records a decision to follow, even if the next request fails. Failure details count HTTP 4xx/5xx, timeouts, connection errors (including DNS failures), body-read errors, redirect-limit errors, and other request errors. Categories can overlap: for example, a 500 response whose body times out counts in HTTP, timeout and body-read errors, but only once in failed requests. The JSON summary uses `schema_version: 1`, seconds for elapsed time and milliseconds for latency percentiles; it does not include per-request samples.

The report also includes counts by HTTP status code, wall-clock throughput, minimum, arithmetic mean, median, maximum and nearest-rank latency percentiles. For an even number of samples, the median is the midpoint of the two middle values; it can therefore differ from the nearest-rank p50. Only the final response status is counted when following redirects. A status is counted as soon as response headers are received, even if reading the body later fails; requests that receive no final response, including those exceeding the redirect limit, have no status code. Latencies include failed requests and end after the response body is read or an error occurs. Latency samples are held in memory, so memory consumption grows with the request count.

## More work? Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The library separates the test plan, local execution and report from the CLI. A distributed mode is not implemented: a future controller and agents must authenticate each other, encrypt communications, enforce target and resource limits, and never offer arbitrary command execution. No remote listener is enabled in this release.

### Local performance benchmark

Build an optimized binary and run the standard-library Python 3 harness:

```sh
cargo build --release --locked
python3 benchmarks/local.py --requests 10000 --repeats 3 --output /tmp/horde-benchmark.json
```

The Python harness compiles and starts a small Rust HTTP/1.1 server with 128 blocking connection workers. It listens only on loopback, returns a two-byte body, and supports persistent connections. After a short warm-up, the harness measures serial, concurrent (16 and 64), no-reuse and rate-limited workloads. Each scenario runs three times by default. The JSON contains each Horde report, median throughput, elapsed wall time, client and server binary hashes, revision and environment metadata. On Linux it also records server CPU time via `/proc` and client CPU time and peak RSS when GNU `/usr/bin/time` is available. The `--binary` option lets you measure a previously built Horde version; `--server-binary` uses a prebuilt server. `python3 benchmarks/local.py --server` runs the server separately and prints its port, for example when profiling Horde.

Client and server use the same machine and compete for resources. The server has a finite worker pool and can itself become saturated; at most 128 connections are handled simultaneously. CPU frequency, loopback, thread scheduling, background activity and operating-system limits can also constrain results. Server CPU time divided by wall time can exceed one because the server uses multiple cores; it alone does not prove whether the server is the bottleneck. This harness is a repeatable baseline, not a measurement of Horde's absolute capacity. Results from the older Python server are not directly comparable with results from the Rust server.

For comparisons, use the same machine and load, build settings, server binary, Python version, request count and repeat count. Compare the individual samples and median, not just one run. Client and server CPU or memory growth can indicate where to profile, but does not by itself prove a bottleneck. To identify a cause, profile the release binary separately (for example with Linux `perf`) under the same workload, then repeat the benchmark after a change. Avoid treating noisy shared CI runners as a strict performance gate.

GitHub Actions runs the benchmark with five repetitions per scenario for pushes and pull requests. The benchmark job summary shows the median and range for each scenario; the `horde-benchmark-results` artifact contains the full JSON for 90 days. For pull requests, the job also builds the exact base commit and runs it with the pull request's benchmark harness on the same runner, sequentially after the pull request's merge commit. The summary compares median throughput for matching workloads, and the artifact includes both reports. A positive percentage means higher throughput in the pull request. The `binary_revision` metadata identifies each binary; `git_commit` identifies the checkout containing the harness. If the base cannot be built or does not support the harness, the comparison is marked unavailable and its logs explain why. The pull request benchmark itself must still succeed.

The comparison is informational: there is no regression threshold, and shared runner variability, workload order and server contention can affect results even on the same runner. Confirm suspected regressions on controlled hardware. Do not use this workflow with privileged pull request events or secrets: it builds and runs pull request code.

## License

GPL-3.0-only; see [LICENSE](LICENSE).
