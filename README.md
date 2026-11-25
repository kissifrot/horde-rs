# Horde

<img src="docs/horde-logo.svg" width="120" height="120" alt="Horde logo">

For the Horde! Horde is an HTTP load-testing tool written in Rust. Test a
single endpoint, replay a multi-step scenario, or split a request budget
across Grunts.

## Quick start

Install Horde on Linux (AMD64 or ARM64) or macOS 15+ (Apple Silicon).

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/kissifrot/horde-rs/main/install.sh)"
```

The installer puts `horde` in `$HOME/.local/bin`. Add that directory to your
PATH if needed. See the [installation guide](docs/installation.md) for
packages and version selection.

From the repository root, start the local HTTP demo with
`python3 examples/login_server.py`. In a second terminal, send 20 requests
with up to two concurrent workers and a target of five starts per second.

```sh
horde -n 20 -c 2 --rate 5 -A 'Horde-Demo/1.0' http://127.0.0.1:8080/
```

Add `--json` for a machine-readable report. Use `horde --help` to see all
options. Only send load to services you are authorized to test.

## Run a scenario

With the same demo server running, try the login scenario. It opens a form,
submits demo credentials and uses the session cookie to open a protected page.

```sh
horde --scenario examples/login.json --validate
horde --scenario examples/login.json -n 4 -c 2 --json
```

`--validate` checks the JSON without sending HTTP requests. For a scenario,
`-n` counts iterations and `-c` sets the maximum number of virtual users.

## Local MCP server

Configure your client to launch an MCP-enabled Horde binary over standard
input and output with `horde mcp`. Try asking it to draft and validate a
scenario for the local login form or explain a JSON report.

With distributed support enabled, the client can manage named Grunt
inventories, check their authenticated control connections and preview a test
budget. Start the workload from the CLI.

## Run a distributed test

A controller authenticates Grunts over mutual TLS, splits a budget and merges
their results. Each Grunt runs a configured GET profile or a named HTTP
scenario against targets on its own loopback interface.

## Usage guides

| If you want to | Read |
| --- | --- |
| Set request rates, send a body, or configure CI thresholds | [CLI and reports](docs/cli.md) |
| Write a login flow with cookies, variables or CSRF extraction | [HTTP scenarios](docs/scenarios.md) |
| Connect an assistant and try example prompts | [MCP setup and examples](docs/mcp.md) |
| Run two Grunts locally or set up a private control network | [Distributed quickstart](docs/distributed-quickstart.md) |
| Publish a scenario to a fleet of Grunts | [Remote scenario deployment](docs/scenario-deployment.md) |

## Linux and macOS prereleases

See the [package and checksum instructions](docs/installation.md#linux-and-macos-prereleases).

## Development

See the [development guide](docs/development.md) for source builds,
feature flags, tests, benchmarks and release packaging.

## License

[GPL-3.0-only](LICENSE). The [Horde logo](docs/horde-logo.svg) is dedicated to
the public domain under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/).
