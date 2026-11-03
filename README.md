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

Usage: `horde [OPTIONS] <URL>` or `horde --scenario <FILE> [OPTIONS]`

| Argument or option | Description | Default |
| --- | --- | --- |
| `<URL>` | Target HTTP or HTTPS URL; required unless using `--scenario`. | — |
| `--scenario <FILE>` | Run a JSON scenario instead of a single URL. | Disabled |
| `--auto-referer` | Derive `Referer` between scenario steps; requires `--scenario`. | Disabled |
| `-n, --requests <REQUESTS>` | Total requests, or scenario iterations; must be positive. | `1` |
| `-c, --concurrency <CONCURRENCY>` | Maximum concurrent requests or virtual users; must be positive. | `1` |
| `-t, --timeout <TIMEOUT>` | Timeout per request, in seconds; must be positive. | `30` |
| `-X, --method <METHOD>` | HTTP method. | `GET` |
| `-H, --header <HEADERS>` | HTTP header as `Name: value`; may be repeated. | None |
| `-A, --user-agent <USER_AGENT>` | Set `User-Agent`; overrides `-H` for single-URL tests. Scenario step headers take precedence. | `Horde/<version>` (e.g. `Horde/0.1.0`) |
| `--body-file <BODY_FILE>` | Read a request body from a file and reuse it for every request. | No body |
| `--duration <DURATION>` | Stop starting new requests or scenario iterations after this many seconds; must be positive. | No duration limit |
| `--rate <RATE>` | Target starts per second for logical requests or scenario iterations, from 1 to 1,000,000,000. | Unlimited |
| `-v, --verbose` | Log the start and result of every request to stderr. | Disabled |
| `--bounded-latency` | Use bounded-memory HDR histograms for approximate percentiles. | Disabled (exact samples) |
| `--no-connection-reuse` | Disable reuse of HTTP connections. | Reuse enabled |
| `--max-failure-rate <PERCENT>` | Maximum failed request or scenario percentage, from 0 to 100. | Disabled |
| `--max-p95-ms <MILLISECONDS>` | Maximum p95 request latency for a single-URL test. | Disabled |
| `--max-step-p95-ms <MILLISECONDS>` | Maximum p95 latency for every scenario step; requires `--scenario`. | Disabled |
| `--json` | Print a JSON summary instead of the text report. | Disabled |
| `-L, --follow-redirects` | Follow HTTP redirects. | Disabled |
| `--max-redirects <MAX_REDIRECTS>` | Maximum redirects per request; positive and requires `-L`. | `10` |
| `-h, --help` | Show help. | — |
| `-V, --version` | Show version. | — |

## Load and requests

Each worker sends requests sequentially. Use `--duration` to stop starting requests after a time limit; in-flight requests finish. With both `--duration` and `-n`, the first limit reached applies. Without either, Horde sends one request. The timeout covers each request, including redirects and the response body.

For single-URL tests, `--rate` spaces logical request starts across all workers. Concurrency, response time and scheduling affect the achieved rate. Redirects are not separately rate-limited, and waiting for a rate slot is excluded from latency and timeout.

To send a body, select a method and provide a file:

```sh
horde -X POST --body-file payload.json -H 'Content-Type: application/json' https://example.com/
```

The file is read once and reused. The method is not changed automatically. Redirects follow the HTTP client's method and body rules; for example, 307 preserves them.

### User journeys

Use `--scenario` to run an ordered sequence of HTTP steps, as in [examples/journey.json](examples/journey.json):

```sh
horde --scenario examples/journey.json -n 100 -c 10 --json
```

Scenarios must declare `"version": 1` and are validated against the bundled [version 1 JSON Schema](schemas/scenario-v1.schema.json) before execution. The optional `$schema` field helps compatible editors find the schema; Horde uses its bundled copy and does not fetch the URI. Horde also validates semantics that JSON Schema cannot express here, including unique step names, variable references, HTTP methods and the expanded URLs.

Here `-n` is the total number of scenario iterations and `-c` is the maximum number of virtual users. With `--duration`, Horde stops starting iterations at the deadline but lets active iterations finish, including their waits and remaining steps. With both `-n` and `--duration`, the first limit reached applies; with `--duration` alone there is no explicit iteration limit. Without either, Horde runs one iteration. Use `--bounded-latency` to limit latency storage for long tests.

`--rate` sets a shared target for starting scenario iterations per second, not a limit on individual HTTP requests. Each user runs one iteration at a time; if all users are busy, the achieved rate can be lower. Steps and redirects are not independently rate-limited. Time waiting for a start slot is excluded from scenario and step latencies. No iteration starts after the duration limit, including one waiting for a slot.

The report shows the target start rate when set and completed iterations per second. The latter is completed iterations divided by total elapsed time, including time spent finishing active iterations after the duration limit; it is not the observed start rate. JSON reports these values as `scenarios.target_starts_per_second` and `scenarios.completed_per_second`.

Each user has its own cookie jar, retained across its iterations. Each step has a unique `name`, an absolute HTTP(S) `url`, an optional `method` (default `GET`), `headers`, request body, and `wait_ms` before the step. By default, a step requires a 2xx response. A failed request or assertion stops that iteration; later steps are skipped. Redirects are not followed unless `-L` is set; `--max-redirects` sets the per-step limit. The report shows iteration duration and per-step counts, final HTTP statuses, failure categories and latency percentiles. Iteration duration includes waits; step latency excludes waits but includes assertions and extraction. Error categories can overlap. Skipped steps have no latency sample or HTTP status. Individual failures do not change the exit code unless a CI threshold is set.

With `-v`, stderr shows each user's scenario start, waits, step results and final outcome with relative timestamps. URLs, headers, bodies, cookies and variable values are omitted; step names are shown. JSON remains on stdout. Logging can affect timing and throughput.

Define string values under `variables` and refer to them as `{{name}}` in step URLs, header values and bodies. Names contain letters, digits and underscores. Substitution in URLs, headers and raw bodies is literal; use the structured body options below for form or JSON encoding. Unknown variables or malformed placeholders reject the scenario before any requests. Avoid committing secrets in scenario files.

For a request body, choose one of `body` (a raw string), `form` (an object of string fields) or `json_body` (an object or array). Form values are URL-encoded after substitution; JSON string values are escaped during serialization and other JSON types are preserved. Field and object names are literal. Horde sets the appropriate content type for `form` and `json_body` unless the step specifies one. Static structured bodies are prepared once.

### Assertions and extraction

An optional `expect` object checks the final response: `status` lists accepted codes (default: any 2xx), `final_url` matches the final URL, `headers` checks exact header values, and `body_contains` looks for literal UTF-8 text. All specified checks must pass. Static variables may be used in `final_url`, `headers` and `body_contains`.

An optional `extract` object defines variables for later steps, for example `"csrf": {"from": "html", "selector": "input[name=csrf]", "attribute": "value"}`. Sources are response headers (`from: "header"`, `name`), JSON strings (`from: "json"`, JSON `pointer`), and HTML attributes (`from: "html"`, CSS `selector` and `attribute`). HTML uses the first match. Response bodies used for JSON or HTML extraction are limited to 2 MiB; missing values, invalid content or exceeding the limit fail the step. Header extraction does not buffer the body.

Extracted values belong to one user and iteration, and are discarded before the next iteration. Names must be unique and cannot replace static variables; only later steps may reference them. They are not logged in verbose output. See [examples/federated-login.json](examples/federated-login.json) for a form with a per-session CSRF token.

For a runnable login example, start the local demo server in one terminal and run the scenario in another:

```sh
python3 examples/login_server.py
./target/release/horde --scenario examples/login.json -n 10 -c 2 -v
```

The example opens a form, submits demo credentials and accesses a protected page using the session cookie. The server listens only on loopback and is for demonstration, not production. Its raw form body uses literal substitution; `form` can be used to encode special characters automatically.

For a redirect between a site and a separate authentication service, use the two-service demo:

```sh
python3 examples/federated_login_server.py
./target/release/horde --scenario examples/federated-login.json -n 10 -c 2 -L -v
```

The services use different loopback hostnames to demonstrate separate cookies. The scenario extracts a per-session CSRF token from the login form before submitting it. They use demo credentials and a simplified one-time ticket; this is not an OAuth or OpenID Connect implementation. `-L` is required to follow the redirects.

### Connections and redirects

Connections may be reused by default. `--no-connection-reuse` disables the idle pool and sends `Connection: close`, overriding a custom `Connection` header. The server may close connections even when reuse is enabled.

Redirects are not followed by default. Use `-L` to follow up to 10 per request, or `-L --max-redirects 5` to set another limit. Exceeding it counts as a failure.

When following a redirect, Horde sets `Referer` from the previous URL: the full URL without credentials or fragment on the same origin, only the origin across origins, and nothing on HTTPS-to-HTTP transitions. This applies to each redirect in both modes, replacing any explicit `Referer` from the previous request. Cross-origin redirects also remove sensitive request headers such as `Authorization` and explicit cookies. The timeout applies to the complete chain and final response body.

For separate scenario steps, `--auto-referer` derives the header from the previous final response URL for each virtual user, including across iterations, using the same policy. An explicit step `Referer` takes precedence for that step's initial request. Horde does not interpret `Referrer-Policy` response headers or HTML attributes, so it does not fully emulate browser behavior.

## Work complete: the report

The report shows completed and failed requests, throughput, response bytes, HTTP status counts and latency statistics. Completed requests include failures. Response bytes include partial bodies; latency ends when the body is read or an error occurs.

Failures include HTTP 4xx/5xx, timeouts, connection errors, body-read errors and redirect-limit errors. Failure categories can overlap, but each failed request is counted only once. A completed run exits successfully by default even if requests fail; invalid configuration and internal errors return a nonzero code.

Status counts use the final response when redirects are followed. A status is counted when its headers arrive, even if the body later fails. A request with no final response has no status. Redirect counts include decisions to follow redirects even if a later request fails.

`Connections opened` counts successful connections throughout the run, including those for redirects or subsequently failed requests. It is not the number still open at the end; failed connection and TLS setup attempts are excluded. Proxy connections are counted at the connector level.

Latencies include failures. Percentiles use the nearest-rank method; for an even number of samples the median is the midpoint of the two central values, so it can differ from p50. Exact mode stores every sample, and its memory use grows with the request count.

Use `--json` for a machine-readable summary (schema version 1, elapsed time in seconds and latencies in milliseconds). Use `-v` for per-request events on stderr. Verbose timestamps are relative to the start of the test; URLs, headers and bodies are omitted. Logging can affect performance.

### Bounded-memory latency statistics

For long tests, `--bounded-latency` uses fixed-size HDR histograms per active worker; scenarios use one per executed step as well as one for iteration duration. Percentiles and the median are approximate upper bounds, with about 0.1% precision; minimum, maximum and mean use observed latencies. Memory depends on concurrency and the number of steps, and may exceed exact mode for small tests. The report and JSON identify the method.

A sample beyond the histogram's range (about 584 years) fails the run rather than being clipped.

### CI thresholds

Use `--max-failure-rate 1 --max-p95-ms 250` to set a maximum failure percentage and p95 in milliseconds. Failure rate is `failed / completed * 100`; p95 includes failed requests. Values equal to the limits pass, and comparisons use unrounded values.

If a configured threshold is exceeded or cannot be evaluated because no requests completed, Horde prints the report and exits with code 2. JSON includes the limits and results under `thresholds`. In bounded mode, the approximate p95 upper bound is compared with the limit, so a true p95 just below it may fail.

For scenarios, `--max-failure-rate` applies to iterations and `--max-step-p95-ms` applies to every step. A step with no samples, including one skipped after an earlier failure, fails its p95 threshold. Scenario p95 thresholds use step latencies, including failed steps but excluding waits. JSON reports each step's observed value and result. `--max-p95-ms` applies only to single-URL tests.

## More work? Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The library separates the test plan, local execution and report from the CLI. Distributed mode is not implemented and no remote listener is enabled. See the [proposed distributed design](docs/distributed.md) for agent security, result aggregation and implementation steps.

### Local performance benchmark

```sh
cargo build --release --locked
python3 benchmarks/local.py --requests 10000 --repeats 5 --output /tmp/horde-benchmark.json
```

The Python harness compiles a Rust HTTP/1.1 loopback server, warms up, then measures serial, concurrent, no-reuse, rate-limited and same-origin redirect workloads (one or three redirects per request). It also measures one-step and three-step scenarios, including assertions and header extraction, with exact and bounded latency statistics. Single-URL throughput is in logical requests/s; scenario throughput is in iterations/s. Each run, median throughput, binary hashes and environment metadata are recorded; on Linux, the harness also records server CPU and, when GNU `time` is available, client CPU and peak RSS. Use `--binary` or `--server-binary` to select prebuilt binaries, or `python3 benchmarks/local.py --server` to run the server separately. A custom server must implement the same redirect paths and responses.

The server handles up to 128 connections and shares resources with Horde. Either can become a bottleneck; CPU frequency, scheduling and background activity also affect results. Compare repeated runs on the same machine with the same binaries and settings, then profile suspected bottlenecks separately. Results from different server binaries, including the former Python server, are not directly comparable.

GitHub Actions runs five repetitions per workload on pushes and pull requests. The job summary shows medians and ranges; detailed JSON is retained for 90 days as `horde-benchmark-results`. For pull requests, the exact base commit is measured sequentially on the same runner with the pull request's harness. The summary compares compatible workloads; a positive percentage means higher throughput in the pull request. Scenario definitions are hashed to avoid comparing different workloads. `binary_revision` identifies each binary and `git_commit` identifies the harness checkout. If the base cannot run a scenario workload, that workload is marked not comparable; the pull request benchmark must still succeed.

CI comparisons are informational and have no regression threshold. Shared runner variability and run order can affect results; confirm suspected regressions on controlled hardware. Do not use this workflow with privileged pull request events or secrets: it runs pull request code.

## License

GPL-3.0-only; see [LICENSE](LICENSE).
