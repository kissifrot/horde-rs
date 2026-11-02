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
| `--json` | Print a JSON summary instead of the text report. | Disabled |
| `-L, --follow-redirects` | Follow HTTP redirects. | Disabled |
| `--max-redirects <MAX_REDIRECTS>` | Maximum redirects per request; positive and requires `-L`. | `10` |
| `-h, --help` | Show help. | — |
| `-V, --version` | Show version. | — |

## Work complete: the report

For example, `horde -X POST --body-file payload.json -H 'Content-Type: application/json' --duration 30 --json https://example.com/` runs for up to 30 seconds and prints JSON. The body file is read into memory once and reused; the HTTP method is not changed automatically. Redirects use the HTTP client's standard method and body rules (for example, a 307 preserves them).

With `--duration`, Horde stops starting new requests at the deadline and lets in-flight requests finish, so elapsed time may exceed the duration. With both `--duration` and `-n`, the first limit reached stops new requests. Without either option, Horde sends one request. A request timeout still applies separately to each request.

Each worker performs requests sequentially, up to the configured concurrency. Connections may be reused. Redirects are not followed by default; use `-L` or `--follow-redirects` to follow up to 10 redirects per request. Set a different positive limit with `-L --max-redirects 5`. Exceeding that limit counts as a failure. The timeout covers the entire request, including redirects and reading the final response body. Responses with HTTP status 4xx or 5xx, transport errors, timeouts, and body-read errors count as failures. Response bytes count body bytes received from the final response, including partial bodies. Completed requests include failures. The process exits successfully when a run completes, even if individual requests fail; invalid configuration or an internal error returns a nonzero exit code.

The report includes the total number of redirects accepted for following, including redirects in requests that subsequently fail, and the number of requests that exceeded the redirect limit. Limit failures are also included in failed requests. The count records a decision to follow, even if the next request fails. Failure details count HTTP 4xx/5xx, timeouts, connection errors (including DNS failures), body-read errors, redirect-limit errors, and other request errors. Categories can overlap: for example, a 500 response whose body times out counts in HTTP, timeout and body-read errors, but only once in failed requests. The JSON summary uses `schema_version: 1`, seconds for elapsed time and milliseconds for latency percentiles; it does not include per-request samples.

The report also includes counts by HTTP status code, wall-clock throughput, minimum, arithmetic mean, median, maximum and nearest-rank latency percentiles. For an even number of samples, the median is the midpoint of the two middle values; it can therefore differ from the nearest-rank p50. Only the final response status is counted when following redirects. A status is counted as soon as response headers are received, even if reading the body later fails; requests that receive no final response, including those exceeding the redirect limit, have no status code. Latencies include failed requests and end after the response body is read or an error occurs. Latency samples are held in memory, so memory consumption grows with the request count.

## More work? Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The library separates the test plan, local execution and report from the CLI. A distributed mode is not implemented: a future controller and agents must authenticate each other, encrypt communications, enforce target and resource limits, and never offer arbitrary command execution. No remote listener is enabled in this release.

## License

GPL-3.0-only; see [LICENSE](LICENSE).
