# Use Horde with an MCP client

Horde's experimental MCP server runs as a local subprocess over standard input
and output. It exposes scenario references, offline validation and report
summaries. With the distributed features enabled, it can also edit configured
inventories and check Grunt control connections. Workloads run from the CLI.

## Connect your client

Use a Horde binary with MCP support. See
[feature availability](installation.md#feature-availability) for the available
builds. Configure your client to launch that binary with the argument `mcp`.
For clients that use an `mcpServers` configuration, replace the path below
with the absolute path to your binary.

```json
{
  "mcpServers": {
    "horde": {
      "command": "/absolute/path/to/horde",
      "args": ["mcp"]
    }
  }
}
```

The shell examples assume the same `horde` binary is on your PATH.

## Draft and validate a login scenario

The MCP server exposes the [scenario schema](../schemas/scenario-v1.schema.json)
and the journey, login and federated login examples. Give the assistant the
form's method, action and field names. For the bundled local demo, try this
prompt.

> Draft a Horde scenario for the demo at http://127.0.0.1:8080. Open
> /login, POST the fields username and password to /login, then open
> /protected and check for “welcome”. Use the demo username grunt and
> reference HORDE_TEST_PASSWORD for the password. Validate the JSON offline
> using a dummy password.

The assistant can use `validate_scenario` with the complete scenario document
as a JSON string in `scenario_json`. For a scenario variable declared as
`{"env":"HORDE_TEST_PASSWORD"}`, pass
`{"HORDE_TEST_PASSWORD":"dummy"}` as `sample_env`. A successful call returns
`{"valid":true,"validation":"offline"}`. The MCP validator uses the supplied
values for interpolation and performs the same offline checks as the CLI.
Actual form submission and assertions are checked when you run the scenario.

To try the draft, save the JSON as `login-draft.json` and start
`python3 examples/login_server.py` from the repository root. In another
terminal, validate and run with the bundled demo password.

<details>
<summary>Example login-draft.json</summary>

```json
{
  "version": 1,
  "name": "Demo login",
  "variables": {
    "base_url": "http://127.0.0.1:8080",
    "password": {"env": "HORDE_TEST_PASSWORD"}
  },
  "steps": [
    {"name": "Open form", "url": "{{base_url}}/login"},
    {
      "name": "Sign in",
      "url": "{{base_url}}/login",
      "method": "POST",
      "form": {"username": "grunt", "password": "{{password}}"}
    },
    {
      "name": "Open protected page",
      "url": "{{base_url}}/protected",
      "expect": {"body_contains": "welcome"}
    }
  ]
}
```

</details>

```sh
HORDE_TEST_PASSWORD=zugzug horde --scenario login-draft.json --validate
HORDE_TEST_PASSWORD=zugzug horde --scenario login-draft.json -n 4 -c 2 --json > login-report.json
```

Ask the assistant to summarize the report and explain any failed steps.
Provide `login-report.json` through the client, or paste its contents. The
`summarize_report` tool takes the parsed JSON object as `report`.

## Configure distributed inventories

Use a Horde binary with MCP and distributed support on the controller.
Follow the [two-Grunt loopback setup](distributed-quickstart.md)
to create `controller.json` and start the two Grunts. In the MCP client
configuration, set the server arguments to the following.

```json
["mcp", "--controller", "local=/absolute/path/to/horde-rs/controller.json"]
```

Repeat `--controller NAME=CONFIG_PATH` to expose more named inventories.
Each name selects the inventory and TLS settings in a local controller
configuration. Tool calls select a name configured at startup.

### Manage the inventory and run preflight

> List the Grunts configured in the local inventory. Add 127.0.0.1:7072
> with TLS identity localhost, then run preflight and tell me which Grunts
> are ready.

`list_inventories` shows available names. `list_grunts` reads the selected
inventory. Here are the corresponding `add_grunt` arguments.

```json
{"inventory":"local","address":"127.0.0.1:7072","tls_identity":"localhost"}
```

Use a literal IP and port as the address. The optional TLS identity can be a
DNS name or IP and is checked against the certificate. The example returns
`changed: false` if you kept the entry from the loopback setup. To remove
it with `remove_grunt`, supply `inventory` and `address`.

`preflight_grunts` takes `{"inventory":"local"}` and checks that each Grunt
responds with an authenticated control hello. The report gives each peer a
status of `ready` or `failed` and includes an error for each failure.
Preflight sends no HTTP workload.

### Preview a budget and read the report

> Prepare a distributed test on local with 41 requests, concurrency 2 and
> 20 starts per second. Show me the split and the CLI arguments.

The arguments to `prepare_distributed_test` are as follows.

```json
{"inventory":"local","requests":41,"concurrency":2,"rate_per_second":20}
```

For the inventory in the loopback setup, the resulting split is as follows.

| Grunt | Requests | Workers | Starts/s |
| --- | ---: | ---: | ---: |
| `127.0.0.1:7071` | 21 | 1 | 10 |
| `127.0.0.1:7072` | 20 | 1 | 10 |

The preview uses the built-in GET profile. It reads the inventory without
opening a control connection. The response includes the controller's
argument list in `argv`, the default 30-second deadline and any unused
Grunts. The controller reads the inventory again at launch. To run the
previewed plan manually from the repository root, use the following command.

```sh
horde controller --config controller.json \
  --requests 41 --concurrency 2 --rate 20 --deadline 30 > distributed-report.json
```

> Summarize this distributed report. Show which Grunts completed and tell
> me whether the configured thresholds could be evaluated.

`summarize_report` shows the per-Grunt status alongside aggregate metrics.
If a Grunt is missing in a partial report, the aggregate covers confirmed
results and any configured thresholds are reported as `not_evaluable`.

### Preview a named scenario

Once the scenario is [installed on the Grunts](distributed-quickstart.md#run-a-named-scenario-with-the-inventory),
try a prompt like this.

> Prepare the login scenario on local for five iterations, with two
> concurrent users and four iteration starts per second.

Use `scenario` and `iterations` in the call to `prepare_distributed_test`.

```json
{"inventory":"local","scenario":"login","iterations":5,"concurrency":2,"rate_per_second":4}
```

The response labels the budget in iterations and includes the controller
arguments for manual execution. The controller checks the installed profile
when it starts. You can pass the resulting JSON to `summarize_report` to
see the iteration counts, step results and Grunt status together.
