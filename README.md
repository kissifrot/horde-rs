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

The installer puts `horde` in `$HOME/.local/bin`. To build from source with
stable Rust, run `cargo build --release` and use `./target/release/horde` in place
of `horde` below.

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

Build with `cargo build --release --features mcp` and configure your client
to launch `horde mcp` over standard input and output. Try asking it to draft
and validate a scenario for the local login form. You can also ask it to
explain a JSON report.

With both `mcp` and `distributed-transport` enabled, the client can manage named
Grunt inventories, check their authenticated control connections and preview a
test budget. Start the workload from the CLI.

## Run a distributed test

A controller splits a request budget across Grunts. Each Grunt runs a fixed
GET profile against an HTTP target on its own loopback interface. The
controller authenticates the Grunts over mutual TLS and merges their results.

## Documentation

| If you want to | Read |
| --- | --- |
| Set request rates, send a body, or configure CI thresholds | [CLI and reports](docs/cli.md) |
| Write a login flow with cookies, variables or CSRF extraction | [HTTP scenarios](docs/scenarios.md) |
| Connect an assistant and try example prompts | [MCP setup and examples](docs/mcp.md) |
| Run two Grunts locally or set up a private control network | [Distributed quickstart](docs/distributed-quickstart.md) |
| Check the build, run benchmarks, or fuzz the histogram decoder | [Development and release notes](docs/development.md) |

## Linux and macOS prereleases

Download the standalone executable or Debian package for your architecture
from [GitHub Releases](https://github.com/kissifrot/horde-rs/releases).
The installer accepts a release tag after `--` to select a specific version.
See the [package and checksum instructions](docs/development.md#release-packages).

## License

[GPL-3.0-only](LICENSE). The [Horde logo](docs/horde-logo.svg) is dedicated to
the public domain under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/).
