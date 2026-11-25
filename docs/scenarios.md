# HTTP scenarios

A Horde scenario describes an ordered sequence of HTTP requests, such as
signing in and opening a protected page.

## Run a local example

With `horde` on your PATH, start the local demo server from the repository
root to try [examples/journey.json](../examples/journey.json). See the
[installation guide](installation.md) if you need the binary.

```sh
python3 examples/login_server.py
```

Run the scenario in another terminal. If port 8080 is in use, choose
another with `--port` and update `base_url` in the scenario.

```sh
horde --scenario examples/journey.json -n 100 -c 10 --json
```

## Scenario format

Scenarios must declare `"version": 1` and are validated against the bundled [version 1 JSON Schema](../schemas/scenario-v1.schema.json) before execution. The optional `$schema` field helps compatible editors find the schema; Horde uses its bundled copy and does not fetch the URI. Horde also validates semantics that JSON Schema cannot express here, including unique step names, variable references, HTTP methods and the expanded URLs.

Each user has its own cookie jar, retained across its iterations. Each step has a unique `name`, an absolute HTTP(S) `url`, an optional `method` (default `GET`), `headers`, request body, and `wait_ms` before the step. By default, a step requires a 2xx response. A failed request or assertion stops that iteration; later steps are skipped. Redirects are not followed unless `-L` is set; `--max-redirects` sets the per-step limit. The report shows iteration duration and per-step counts, final HTTP statuses, failure categories and latency percentiles. Iteration duration includes waits; step latency excludes waits but includes assertions and extraction. Error categories can overlap. Skipped steps have no latency sample or HTTP status. Individual failures do not change the exit code unless a CI threshold is set.

With `-v`, stderr shows each user's scenario start, waits, step results and final outcome with relative timestamps. URLs, headers, bodies, cookies and variable values are omitted; step names are shown. JSON remains on stdout. Logging can affect timing and throughput.

Invalid scenario files are rejected before the run. Errors identify the file and, when available, a JSON path or step and field; JSON syntax errors include a line and column. With `-v`, variable preparation and extraction failures also identify the affected field or extraction variable and a reason without printing its value or the response body. File paths, step names, field names and variable names may appear in diagnostics and must not contain secrets.

## Validate before running

Use `horde --scenario examples/journey.json --validate` to check a scenario without sending HTTP requests. Horde performs the same loading and validation as a normal run, including resolving required environment variables. Success exits with code 0; errors are reported on stderr with a nonzero exit code. With `--json`, success prints `{"schema_version":1,"type":"scenario_validation","valid":true}`. Execution options such as concurrency, duration and thresholds cannot be combined with `--validate`. Validation does not check server responses, assertions against live responses or dynamic extraction values.

## Variables and request bodies

Define values under `variables` and use `{{name}}` in step URLs, header
values and bodies. In this example, the base URL is literal and the
credentials come from the process environment.

```json
{
  "variables": {
    "base_url": "http://127.0.0.1:8080",
    "username": {"env": "HORDE_USERNAME"},
    "password": {"env": "HORDE_PASSWORD"}
  }
}
```

Horde resolves environment variables once when loading the scenario; missing or non-UTF-8 values stop the run before any requests. Environment variable names must start with a letter or underscore and contain only letters, digits and underscores. Scenario variable names contain letters, digits and underscores. Values are omitted from reports and verbose logs. Avoid committing secrets in scenario files; environment variables are not a secret store and their visibility depends on the host and CI system.

Substitution in URLs, headers and raw bodies is literal; use the structured body options below for form or JSON encoding. Unknown variables or malformed placeholders reject the scenario before any requests.

For a request body, choose one of `body` (a raw string), `form` (an object of string fields) or `json_body` (an object or array). Form values are URL-encoded after substitution; JSON string values are escaped during serialization and other JSON types are preserved. Field and object names are literal. Horde sets the appropriate content type for `form` and `json_body` unless the step specifies one. Static structured bodies are prepared once.

## Assertions and extraction

An optional `expect` object checks the final response: `status` lists accepted codes (default: any 2xx), `final_url` matches the final URL, `headers` checks exact header values, and `body_contains` looks for literal UTF-8 text. All specified checks must pass. Static variables may be used in `final_url`, `headers` and `body_contains`.

An optional `extract` object defines variables for later steps, for example `"csrf": {"from": "html", "selector": "input[name=csrf]", "attribute": "value"}`. Sources are response headers (`from: "header"`, `name`), JSON strings (`from: "json"`, JSON `pointer`), and HTML attributes (`from: "html"`, CSS `selector` and `attribute`). HTML uses the first match. Response bodies used for JSON or HTML extraction are limited to 2 MiB; missing values, invalid content or exceeding the limit fail the step. Header extraction does not buffer the body.

Extracted values belong to one user and iteration, and are discarded before the next iteration. Names must be unique and cannot replace static variables; only later steps may reference them. They are not logged in verbose output. See [examples/federated-login.json](../examples/federated-login.json) for a form with a per-session CSRF token.

## Login examples

With the local demo server running, try the login scenario.

```sh
horde --scenario examples/login.json -n 10 -c 2 -v
```

The example opens a form, submits demo credentials and accesses a protected page using the session cookie. The server listens only on loopback and is for demonstration, not production. Its raw form body uses literal substitution; `form` can be used to encode special characters automatically.

For a redirect between a site and a separate authentication service, start the two-service demo in one terminal.

```sh
python3 examples/federated_login_server.py
```

Then run the scenario in another terminal.

```sh
horde --scenario examples/federated-login.json -n 10 -c 2 -L -v
```

The services use different loopback hostnames to demonstrate separate cookies. The scenario extracts a per-session CSRF token from the login form before submitting it. They use demo credentials and a simplified one-time ticket; this is not an OAuth or OpenID Connect implementation. `-L` is required to follow the redirects.

## Iteration pace and reports

Here `-n` is the total number of scenario iterations and `-c` is the maximum number of virtual users. With `--duration`, Horde stops starting iterations at the deadline but lets active iterations finish, including their waits and remaining steps. With both `-n` and `--duration`, the first limit reached applies; with `--duration` alone there is no explicit iteration limit. Without either, Horde runs one iteration. Use `--bounded-latency` to limit latency storage for long tests.

`--rate` sets a shared target for starting scenario iterations per second, not a limit on individual HTTP requests. Each user runs one iteration at a time; if all users are busy, the achieved rate can be lower. Steps and redirects are not independently rate-limited. Time waiting for a start slot is excluded from scenario and step latencies. No iteration starts after the duration limit, including one waiting for a slot.

With a fixed number of users, a slow service can reduce the number of iterations started. Reported latencies cover executed iterations and steps; they do not measure delay for iterations that could not start while all users were busy.

The report shows the target start rate when set, the observed start rate and completed iterations per second. The start rate includes failed iterations and uses one of two windows:

- If the duration limit is reached first, divide the number started by the configured duration. This is the full allowed start window, even when no further slot can be used and the run finishes earlier.
- If the iteration limit is reached first, divide the number of intervals between starts (`started - 1`) by the elapsed time between the first and last starts. With fewer than two starts or a zero-length interval, the rate is unavailable.

The completed rate divides completed iterations by total elapsed time, including time spent finishing active iterations after the duration limit. JSON reports the rates under `scenarios.target_starts_per_second`, `scenarios.started_per_second` and `scenarios.completed_per_second`. `scenarios.start_measurement` includes the number started, `window_seconds` and `method` (`duration` or `iteration_limit`).

See the [CLI reference](cli.md#ci-thresholds) for pass criteria and exit codes.
