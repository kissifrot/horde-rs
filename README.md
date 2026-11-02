# Horde

For the Horde! Horde is a standalone HTTP load-testing tool written in Rust.

## Install and run

Install the current stable Rust toolchain, then run:

```sh
cargo build --release
./target/release/horde -n 1000 -c 10 https://example.com/
./target/release/horde -n 100 -c 5 -t 10 -X GET -H 'Accept: application/json' https://example.com/
```

Use `horde --help` for all options. Only test services you are authorized to load test.

Each worker performs requests sequentially, up to the configured concurrency. Connections may be reused. Redirects are not followed. The timeout covers the entire request, including reading the body. Responses with HTTP status 4xx or 5xx, transport errors, timeouts, and body-read errors count as failures. Response bytes count body bytes received, including partial bodies. Completed requests include failures. The process exits successfully when a run completes, even if individual requests fail; invalid configuration or an internal error returns a nonzero exit code.

The report includes wall-clock throughput and nearest-rank latency percentiles. Latencies include failed requests and end after the response body is read or an error occurs. Percentile samples are held in memory, so memory consumption grows with the request count. No request body support is included yet.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The library separates the test plan, local execution and report from the CLI. A distributed mode is not implemented: a future controller and agents must authenticate each other, encrypt communications, enforce target and resource limits, and never offer arbitrary command execution. No remote listener is enabled in this release.

## License

GPL-3.0-only; see [LICENSE](LICENSE).
