# Run a distributed test

The controller divides a test budget across Grunts. Each Grunt runs a configured
GET profile or a named HTTP scenario against its own loopback targets. Grunts authenticate
the controller over mutual TLS, and the controller verifies each Grunt's identity.
See the [workload diagram](distributed.md#how-distributed-mode-works).

## Try two Grunts on one machine

Put a Horde binary with distributed support on your PATH and run the
commands from the repository root. See [feature availability](installation.md#feature-availability).
The bundled TLS fixture keys are public test data for this loopback demo. Use separately
provisioned certificates for the [private-network pilot](distributed-pilot.md).

Start the HTTP demo in its own terminal.

```sh
python3 examples/login_server.py
```

Start one Grunt in each of two other terminals. Run the same command in both,
using `7072` in place of `7071` in the second terminal.

```sh
tls=tests/fixtures/grunt-tls
horde grunt --listen 127.0.0.1:7071 --target http://127.0.0.1:8080/ \
  --ca "$tls/ca.der" --cert "$tls/server.der" --key "$tls/server-key.der" \
  --controller-cert "$tls/controller.der"
```

Each Grunt prints its listening address. Port zero lets the OS pick an
available port. Use the printed addresses in the inventory if you change ports.

## Set up the inventory

Create `grunts.json` in the repository root.

```json
{
  "grunts": [
    {"address": "127.0.0.1:7071"},
    {"address": "127.0.0.1:7072", "tls_identity": "localhost"}
  ]
}
```

Create `controller.json` beside it. TLS paths and the inventory path are
relative to the configuration file.

```json
{
  "inventory": "grunts.json",
  "tls": {
    "ca": ["tests/fixtures/grunt-tls/ca.der"],
    "cert": "tests/fixtures/grunt-tls/controller.der",
    "key": "tests/fixtures/grunt-tls/controller-key.der"
  }
}
```

Use the CLI to inspect the inventory and verify the control connection.

```sh
horde inventory --config controller.json list
horde preflight --config controller.json
```

Preflight succeeds when each Grunt responds with an authenticated hello.
Preflight sends no HTTP workload. Once both Grunts are ready, run the test.

```sh
horde controller --config controller.json \
  --requests 41 --concurrency 2 --rate 20
```

The first Grunt receives 21 requests and the second 20. Each gets one
worker and a target of 10 starts per second.

## Run a named scenario with the inventory

Start the repository's HTTP demo as above and use the same controller
configuration and inventory. Save the following as `grunt-scenario.json`
at the repository root. This example uses the bundled login flow.

```json
{
  "listen": "127.0.0.1:7071",
  "scenarios": {"login": "examples/login.json"},
  "controller_cert": ["tests/fixtures/grunt-tls/controller.der"],
  "tls": {
    "ca": ["tests/fixtures/grunt-tls/ca.der"],
    "cert": "tests/fixtures/grunt-tls/server.der",
    "key": "tests/fixtures/grunt-tls/server-key.der"
  }
}
```

Stop the demo Grunts if they are still running. Start them with
`horde grunt --config grunt-scenario.json`, using a copy of
the config with port `7072` for the second Grunt. Relative scenario paths
are resolved from the Grunt configuration file. The same Grunt can serve
both kinds of profile if its configuration includes `target` and `scenarios`.

```sh
horde preflight --config controller.json --scenario login
horde controller --config controller.json \
  --scenario login --iterations 5 --concurrency 2 --rate 4
```

For a remote fleet, use the [Ansible deployment guide](scenario-deployment.md).

The controller checks the installed scenario and its budget on each
assigned Grunt before starting any iterations. Install the same JSON file
on each Grunt. Environment variables referenced by the scenario are read
locally when the Grunt starts. Step URLs must use static literal loopback
IPs after substitution. Steps can use forms, cookies and response data in
later headers or bodies. Automatic redirects and HTTP proxies are disabled
for distributed scenarios. A profile supports up to eight steps.

Here the first Grunt receives three iterations and the second receives two.
`--rate` counts iteration starts per second. The report includes per-step
completed, failed and skipped counts and merged latency percentiles.
`--max-failure-rate` applies to iterations and `--max-p95-ms` applies to
overall iteration duration.

## Manage peers

Use `inventory add` and `inventory remove` to update the list of peers for
the next run. For example, remove the second Grunt and add it back.

```sh
horde inventory --config controller.json remove 127.0.0.1:7072
horde inventory --config controller.json add 127.0.0.1:7072=localhost
```

Both `controller` and `preflight` use the same `--config` file. To supply
TLS settings on the command line, use `--inventory grunts.json` to load the
peer list or repeat `--grunt` with each address. Pass the request budget
and rate as controller options. See the [private-network configuration](distributed-pilot.md#run-and-verify).

The Grunt profile uses a fixed HTTP(S) target with a literal loopback IP
and disables redirects. Certificates are DER and keys are PKCS#8 DER.
Each Grunt authorizes the controller leaf certificate it was configured with.

## Interpret the result

The controller emits JSON by default. Add `--format text` for a terminal
summary of the completed budget, merged percentiles, per-Grunt states and CI thresholds.
It returns a nonzero exit code for partial runs or failed configured thresholds.
On interruption it requests cancellation concurrently and keeps metrics from
validated completed Grunts. Unconfirmed Grunts are reported as missing.

## Use a private control network

To opt into private control connections, set `grunt --listen` to the exact
private address of the Grunt and repeat `--allow-controller-ip` for each allowed
controller source IP. Start the controller with `--private-network` and the
explicit private Grunt addresses in `--grunt`. IPv4 RFC 1918 addresses and IPv6
ULA addresses are accepted. A wildcard bind or an empty private peer list is
rejected. The IP check applies before TLS, and each peer still needs an
authorized controller certificate. Use certificates issued for the pilot
machines. Set the expected TLS identity per Grunt with
`--grunt 10.0.0.21:7071=grunt-a.example.test`. The controller connects to the
socket address and checks the certificate's DNS SAN against the identity.
DNS resolution is unnecessary for that identity. In private mode, omitting
the suffix defaults to the socket's IP. The certificate must then contain
that address in an IP SAN, including for IPv6. An IP string in a DNS SAN is
insufficient. A bare loopback address defaults to `localhost`.
`--server-name` sets a fallback for entries without an explicit identity.
Private entries must resolve to distinct expected identities. Issue each
Grunt a leaf certificate for its own identity.

For a complete setup across three machines and certificate rotation, see the [private-network pilot](distributed-pilot.md). For protocol details and design notes, see [Distributed mode](distributed.md).
