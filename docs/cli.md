# Horde CLI reference

See the [README](../README.md#quick-start) for a first run and the [distributed guide](distributed-quickstart.md) for the controller and Grunt commands.

## Options

Usage: `horde [OPTIONS] <URL>` or `horde --scenario <FILE> [OPTIONS]`

| Argument or option | Description | Default |
| --- | --- | --- |
| `<URL>` | Target HTTP or HTTPS URL; required unless using `--scenario`. | — |
| `--scenario <FILE>` | Run a JSON scenario instead of a single URL. | Disabled |
| `--validate` | Validate a scenario without sending requests; requires `--scenario`. | Disabled |
| `--auto-referer` | Derive `Referer` between scenario steps; requires `--scenario`. | Disabled |
| `-n, --requests <REQUESTS>` | Total requests, or scenario iterations; must be positive. | `1` without `--duration` |
| `-c, --concurrency <CONCURRENCY>` | Maximum concurrent requests or virtual users; must be positive. | `1` |
| `-t, --timeout <TIMEOUT>` | Timeout per request, in seconds; must be positive. | `30` |
| `-X, --method <METHOD>` | HTTP method for single-URL tests. | `GET` |
| `-H, --header <HEADERS>` | HTTP header as `Name: value`; may be repeated; single-URL tests only. | None |
| `-A, --user-agent <USER_AGENT>` | Set `User-Agent`; overrides `-H` for single-URL tests. Scenario step headers take precedence. | `Horde/<version>` (e.g. `Horde/0.1.0`) |
| `--body-file <BODY_FILE>` | Read a request body from a file and reuse it for every request; single-URL tests only. | No body |
| `--duration <DURATION>` | Stop starting new requests or scenario iterations after this many seconds; must be positive. | No duration limit |
| `--rate <RATE>` | Target starts per second for logical requests or scenario iterations, from 1 to 1,000,000,000. | Unlimited |
| `-v, --verbose` | Log the start and result of every request to stderr. | Disabled |
| `--bounded-latency` | Use bounded-memory HDR histograms for approximate percentiles. | Disabled (exact samples) |
| `--no-connection-reuse` | Disable reuse of HTTP connections for single-URL tests. | Reuse enabled |
| `--max-failure-rate <PERCENT>` | Maximum failed request or scenario percentage, from 0 to 100. | Disabled |
| `--max-p95-ms <MILLISECONDS>` | Maximum p95 request latency for a single-URL test. | Disabled |
| `--max-step-p95-ms <MILLISECONDS>` | Maximum p95 latency for every scenario step; requires `--scenario`. | Disabled |
| `--json` | Print a JSON summary instead of the text report. | Disabled |
| `-L, --follow-redirects` | Follow HTTP redirects. | Disabled |
| `--max-redirects <MAX_REDIRECTS>` | Maximum redirects per request; positive and requires `-L`. | `10` |
| `-h, --help` | Show help. | — |
| `-V, --version` | Show version. | — |

## Load and requests

Each worker sends requests sequentially. Use `-n` to set a request count,
`--duration` to set the window for starting requests, or both to stop at
the first limit reached. In-flight requests finish. If you omit both
options, Horde sends one request. The timeout covers redirects and the
response body.

For single-URL tests, `--rate` sets the target request starts per second
across all workers. The achieved rate depends on concurrency, response time
and scheduling. Redirects have no separate rate limit. Waiting for a rate
slot is excluded from latency and timeout. See [pacing internals](development.md#pacing-internals).

### Progress and interruption

Interactive terminals show completed requests or iterations, failures and
average throughput once per second. Verbose runs show per-request logs.

Press Ctrl+C to cancel a run and close active requests. On Unix, SIGTERM
behaves likewise. The partial text report includes completed and failed
counts, elapsed time, average throughput and p95/p99. Horde calculates
percentiles after the workers stop, using only completed work.

Horde exits with 130 for SIGINT or 143 for SIGTERM. With `--json`, it
returns an object with `type: "interrupted"` and
`thresholds.status: "not_evaluable"`. A second signal or a shutdown
taking more than five seconds forces immediate exit and may prevent
the report from being printed.

### Send a request body

Select a method and provide a payload file. To post JSON, point Horde at
an endpoint that accepts it and provide the content type.

```sh
horde -X POST --body-file payload.json -H 'Content-Type: application/json' http://127.0.0.1:8000/api
```

Horde reads the file once and uses the selected method for each initial
request. When following redirects, 307 and 308 preserve the method and body.
301 and 302 change POST to GET, and 303 uses GET except for HEAD. Horde drops
the body when switching to GET.

## Connections and redirects

Connections may be reused by default. `--no-connection-reuse` disables the idle pool and sends `Connection: close`, overriding a custom `Connection` header. The server may close connections even when reuse is enabled.

Redirects are not followed by default. Use `-L` to follow up to 10 per request, or `-L --max-redirects 5` to set another limit. Exceeding it counts as a failure.

When following a redirect, Horde sets `Referer` from the previous URL: the full URL without credentials or fragment on the same origin, only the origin across origins, and nothing on HTTPS-to-HTTP transitions. This applies to each redirect in both modes, replacing any explicit `Referer` from the previous request. Cross-origin redirects also remove sensitive request headers such as `Authorization` and explicit cookies. The timeout applies to the complete chain and final response body.

For separate scenario steps, `--auto-referer` derives the header from the previous final response URL for each virtual user, including across iterations, using the same policy. An explicit step `Referer` takes precedence for that step's initial request. Horde does not interpret `Referrer-Policy` response headers or HTML attributes, so it does not fully emulate browser behavior.

## Reports

For single-URL tests, the report shows completed and failed requests, throughput, response bytes, HTTP status counts and latency statistics. Completed requests include failures. Response bytes include partial bodies; latency ends when the body is read or an error occurs. Scenario reports provide iteration and per-step statistics.

Standalone runs print a text report by default. Set `NO_COLOR=1` to disable
terminal color. Use `--json` for scripts. Scenario reports include completed,
failed and skipped counts for each step. See the [scenario guide](scenarios.md#iteration-pace-and-reports).

For single-URL tests, failures include HTTP 4xx/5xx, timeouts, connection errors, body-read errors and redirect-limit errors. Failure categories can overlap, but each failed request is counted only once. A completed run exits successfully by default even if requests or scenarios fail; invalid configuration and internal errors return a nonzero code.

Status counts use the final response when redirects are followed. A status is counted when its headers arrive, even if the body later fails. A request with no final response has no status. In single-URL reports, redirect counts include decisions to follow redirects even if a later request fails.

In single-URL reports, `Connections opened` counts successful connections throughout the run, including those for redirects or subsequently failed requests. It is not the number still open at the end; failed connection and TLS setup attempts are excluded. Proxy connections are counted at the connector level.

Latencies include failures. Percentiles use the nearest-rank method; for an even number of samples the median is the midpoint of the two central values, so it can differ from p50. Exact mode stores every sample, and its memory use grows with the request count.

Use `--json` for a machine-readable summary (schema version 1, elapsed time in seconds and latencies in milliseconds). Use `-v` for per-request events on stderr. Verbose timestamps are relative to the start of the test; URLs, headers and bodies are omitted. Logging can affect performance.

### Bounded-memory latency statistics

For long tests, `--bounded-latency` uses bounded-memory HDR histograms per active worker. Scenarios use one per executed step and one for iteration duration. Each scenario histogram grows to cover the recorded latencies, with a bounded range. Percentiles and the median are approximate upper bounds, with about 0.1% precision; minimum, maximum and mean use observed latencies. Memory depends on concurrency and the number of steps, and may exceed exact mode for small tests. The report and JSON identify the method.

A sample beyond the histogram's range (about 584 years) fails the run rather than being clipped.

### CI thresholds

Use `--max-failure-rate 1 --max-p95-ms 250` to set a maximum failure percentage and p95 in milliseconds. Failure rate is `failed / completed * 100`; p95 includes failed requests. Values equal to the limits pass, and comparisons use unrounded values.

If a configured threshold is exceeded or cannot be evaluated because no requests completed, Horde prints the report and exits with code 2. JSON includes the limits and results under `thresholds`. In bounded mode, the approximate p95 upper bound is compared with the limit, so a true p95 just below it may fail.

For scenarios, `--max-failure-rate` applies to iterations and `--max-step-p95-ms` applies to every step. A step with no samples, including one skipped after an earlier failure, fails its p95 threshold. Scenario p95 thresholds use step latencies, including failed steps but excluding waits. JSON reports each step's observed value and result. `--max-p95-ms` applies only to single-URL tests.
