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
| `-v, --verbose` | Log the start and result of every request to stderr. | Disabled |
| `--bounded-latency` | Use a bounded-memory HDR histogram for approximate percentiles. | Disabled (exact samples) |
| `--no-connection-reuse` | Disable reuse of HTTP connections. | Reuse enabled |
| `--max-failure-rate <PERCENT>` | Maximum failed request percentage, from 0 to 100. | Disabled |
| `--max-p95-ms <MILLISECONDS>` | Maximum p95 latency in milliseconds; must be non-negative. | Disabled |
| `--json` | Print a JSON summary instead of the text report. | Disabled |
| `-L, --follow-redirects` | Follow HTTP redirects. | Disabled |
| `--max-redirects <MAX_REDIRECTS>` | Maximum redirects per request; positive and requires `-L`. | `10` |
| `-h, --help` | Show help. | — |
| `-V, --version` | Show version. | — |

## Load and requests

Each worker sends requests sequentially. Use `--duration` to stop starting requests after a time limit; in-flight requests finish. With both `--duration` and `-n`, the first limit reached applies. Without either, Horde sends one request. The timeout covers each request, including redirects and the response body.

`--rate` spaces logical request starts across all workers. Concurrency, response time and scheduling affect the achieved rate. Redirects are not separately rate-limited, and waiting for a rate slot is excluded from latency and timeout.

To send a body, select a method and provide a file:

```sh
horde -X POST --body-file payload.json -H 'Content-Type: application/json' https://example.com/
```

The file is read once and reused. The method is not changed automatically. Redirects follow the HTTP client's method and body rules; for example, 307 preserves them.

### Connections and redirects

Connections may be reused by default. `--no-connection-reuse` disables the idle pool and sends `Connection: close`, overriding a custom `Connection` header. The server may close connections even when reuse is enabled.

Redirects are not followed by default. Use `-L` to follow up to 10 per request, or `-L --max-redirects 5` to set another limit. Exceeding it counts as a failure.

## Work complete: the report

The report shows completed and failed requests, throughput, response bytes, HTTP status counts and latency statistics. Completed requests include failures. Response bytes include partial bodies; latency ends when the body is read or an error occurs.

Failures include HTTP 4xx/5xx, timeouts, connection errors, body-read errors and redirect-limit errors. Failure categories can overlap, but each failed request is counted only once. A completed run exits successfully by default even if requests fail; invalid configuration and internal errors return a nonzero code.

Status counts use the final response when redirects are followed. A status is counted when its headers arrive, even if the body later fails. A request with no final response has no status. Redirect counts include decisions to follow redirects even if a later request fails.

`Connections opened` counts successful connections throughout the run, including those for redirects or subsequently failed requests. It is not the number still open at the end; failed connection and TLS setup attempts are excluded. Proxy connections are counted at the connector level.

Latencies include failures. Percentiles use the nearest-rank method; for an even number of samples the median is the midpoint of the two central values, so it can differ from p50. Exact mode stores every sample, and its memory use grows with the request count.

Use `--json` for a machine-readable summary (schema version 1, elapsed time in seconds and latencies in milliseconds). Use `-v` for per-request events on stderr. Verbose timestamps are relative to the start of the test; URLs, headers and bodies are omitted. Logging can affect performance.

### Bounded-memory latency statistics

For long tests, `--bounded-latency` uses a fixed-size HDR histogram per active worker. Percentiles and the median are approximate upper bounds, with about 0.1% precision; minimum, maximum and mean use observed latencies. Memory depends on concurrency and may exceed exact mode for small tests. The report and JSON identify the method.

A sample beyond the histogram's range (about 584 years) fails the run rather than being clipped.

### CI thresholds

Use `--max-failure-rate 1 --max-p95-ms 250` to set a maximum failure percentage and p95 in milliseconds. Failure rate is `failed / completed * 100`; p95 includes failed requests. Values equal to the limits pass, and comparisons use unrounded values.

If a configured threshold is exceeded or cannot be evaluated because no requests completed, Horde prints the report and exits with code 2. JSON includes the limits and results under `thresholds`. In bounded mode, the approximate p95 upper bound is compared with the limit, so a true p95 just below it may fail.

## More work? Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The library separates the test plan, local execution and report from the CLI. Distributed mode is not implemented. Future agents must authenticate and encrypt communications, enforce target and resource limits, and never offer arbitrary command execution. No remote listener is enabled.

### Local performance benchmark

```sh
cargo build --release --locked
python3 benchmarks/local.py --requests 10000 --repeats 5 --output /tmp/horde-benchmark.json
```

The Python harness compiles a Rust HTTP/1.1 loopback server, warms up, then measures serial, concurrent, no-reuse and rate-limited workloads. It records each run, median throughput, binary hashes and environment metadata; on Linux, it also records server CPU and, when GNU `time` is available, client CPU and peak RSS. Use `--binary` or `--server-binary` to select prebuilt binaries, or `python3 benchmarks/local.py --server` to run the server separately.

The server handles up to 128 connections and shares resources with Horde. Either can become a bottleneck; CPU frequency, scheduling and background activity also affect results. Compare repeated runs on the same machine with the same binaries and settings, then profile suspected bottlenecks separately. Results from the former Python server are not directly comparable.

GitHub Actions runs five repetitions per scenario on pushes and pull requests. The job summary shows medians and ranges; detailed JSON is retained for 90 days as `horde-benchmark-results`. For pull requests, the exact base commit is measured sequentially on the same runner with the pull request's harness. The summary compares medians; a positive percentage means higher throughput in the pull request. `binary_revision` identifies each binary and `git_commit` identifies the harness checkout. If the base cannot be measured, the comparison is marked unavailable; the pull request benchmark must still succeed.

CI comparisons are informational and have no regression threshold. Shared runner variability and run order can affect results; confirm suspected regressions on controlled hardware. Do not use this workflow with privileged pull request events or secrets: it runs pull request code.

## License

GPL-3.0-only; see [LICENSE](LICENSE).
