# Two-Grunt private-network pilot

The pilot exercises the control channel across three machines. Each Grunt
runs only against an HTTP server on its own loopback address. The sample
private addresses below are placeholders for addresses assigned to the pilot.

See the [illustrated overview](distributed.md#how-distributed-mode-works)
for the control channel, workload flow and partial reports.

| Role | Private address | Control identity | HTTP workload |
| --- | --- | --- | --- |
| Controller | `10.20.0.10` | Dedicated client leaf, explicitly allowed on both Grunts | None |
| Grunt A | `10.20.0.21:7071` | `grunt-a.pilot.test` DNS SAN or its private IP SAN | Local HTTP fixture |
| Grunt B | `10.20.0.22:7071` | `grunt-b.pilot.test` DNS SAN or its private IP SAN | Local HTTP fixture |

## Before the run

- Build the same revision on all three machines with
  `cargo build --release --locked --features distributed-transport`.
- Provision an internal CA trust root, one controller client certificate
  and distinct server certificates for the Grunts. Each process needs
  its own leaf certificate in DER, its own PKCS#8 DER private key and
  the CA certificate used to verify its peer. The Grunts also need the
  exact controller leaf as a public certificate. Keep each private key on
  its own machine. The certificates under `tests/fixtures` are test data.
- Verify the expected SAN, validity dates, certificate chain and intended
  client/server usage, and check the clocks of all three machines.
  If a Grunt has no hostname, an IP SAN for its stable private address
  works. A DNS SAN used with an explicit socket address and identity
  needs no DNS lookup.
- Allow control traffic to TCP 7071 from the controller's source IP
  only. Confirm the address the Grunt will actually see. Keep the
  workload HTTP port bound to loopback and the control interface private.
- Set aside a bounded test window. Keep the controller output and
  locally observed target requests for each run. Both Grunts have local
  deadlines even if the controller becomes unreachable.

## Find the private addresses and issue certificates

On each Linux machine, run `ip -br -4 addr show up`. Pick the private
address on the LAN or VPN interface the machines share. Docker bridges,
virtual Ethernet links and loopback are separate from that LAN. Once
the Grunt addresses are known, run `ip -4 route get GRUNT_IP` on the
controller and use the reported `src` value for that Grunt's
`--allow-controller-ip`. Check both routes.

Prepare the [JSON inventory](#run-and-verify) first. Copy it to each
Grunt and to the signer. The repository's `scripts/pilot-tls.py` helper
uses OpenSSL 3. Run the request command on each endpoint so its private
key stays there. Send only the resulting `request.pem` to the signer
over an authenticated channel. An internal CA can sign these CSRs using
its existing enrollment process.

```sh
# CA machine, once
python3 scripts/pilot-tls.py init-ca "$HOME/horde-pilot-ca"

# Controller
python3 scripts/pilot-tls.py request controller "$HOME/horde-pilot"

# On each Grunt, select its exact address from the inventory
python3 scripts/pilot-tls.py request-grunt "$HOME/horde-pilot" \
  --inventory grunts.json --address "${GRUNT_ADDRESS:?set the local Grunt socket address}"
```

On the pilot signer, save the three public CSRs as `controller.req.pem`,
`grunt-a.req.pem`, and `grunt-b.req.pem`. Verify which authenticated
endpoint submitted each CSR. Set `GRUNT_A_ADDRESS` and
`GRUNT_B_ADDRESS` to their inventory addresses, including the ports.
The signer takes the permitted IP or DNS SAN from its local inventory.

```sh
python3 scripts/pilot-tls.py issue controller --ca-dir "$HOME/horde-pilot-ca" \
  --csr controller.req.pem --out controller.der
python3 scripts/pilot-tls.py issue-grunt --inventory grunts.json \
  --address "$GRUNT_A_ADDRESS" --ca-dir "$HOME/horde-pilot-ca" \
  --csr grunt-a.req.pem --out grunt-a.der
python3 scripts/pilot-tls.py issue-grunt --inventory grunts.json \
  --address "$GRUNT_B_ADDRESS" --ca-dir "$HOME/horde-pilot-ca" \
  --csr grunt-b.req.pem --out grunt-b.der
```

The helper verifies each issued certificate and writes DER. The pilot CA
is valid for 30 days, with leaves issued for seven days. Keep
`ca-key.pem` on the signer. Put the public `ca.der` in
`$HOME/horde-pilot` on all three machines. Place each signed leaf as
`cert.der` beside that machine's generated `key.der`. Copy the public
`controller.der` to both Grunts as `controller.der` for the explicit
allowlist. Use the same inventory for issuance and control so the
expected identities match the certificates.

## Start each Grunt

Start the fixed-response fixture on both Grunt machines. It serves a
two-byte file from an isolated temporary directory and logs requests
locally.

```sh
pilot_target=$(mktemp -d)
printf 'OK' > "$pilot_target/index.html"
python3 -m http.server --bind 127.0.0.1 --directory "$pilot_target" 18080
```

Save a local `grunt.json` beside the certificate files. Replace the
listen address with this Grunt's inventory address and the allowed
controller IP with the source address from the controller's route lookup.

```json
{
  "listen": "10.20.0.21:7071",
  "allow_controller_ip": ["10.20.0.10"],
  "target": "http://127.0.0.1:18080/",
  "controller_cert": ["controller.der"],
  "tls": {"ca": ["ca.der"], "cert": "cert.der", "key": "key.der"}
}
```

```sh
target/release/horde grunt --config "$HOME/horde-pilot/grunt.json"
```

Both processes should print their private listening address. For IPv6,
enclose the IP in brackets in socket addresses. Leave it unbracketed
when setting peer IPs.

## Run and verify

For repeated runs, put the peers in a JSON inventory such as `grunts.json`.
Use the actual private socket addresses for your Grunts.

```json
{
  "grunts": [
    {"address": "10.0.0.12:7071"},
    {"address": "10.0.0.13:7071", "tls_identity": "grunt-b.example.internal"}
  ]
}
```

In private network mode, an omitted identity defaults to the address's
IP SAN. Set `tls_identity` when the certificate has a different IP or a
DNS SAN. The DNS name is checked against the certificate. The connection
uses the literal socket address from the inventory.

Save `controller.json` beside the controller's credential files and
`grunts.json`. Both commands use this same configuration.

```json
{
  "inventory": "grunts.json",
  "private_network": true,
  "tls": {"ca": ["ca.der"], "cert": "cert.der", "key": "key.der"}
}
```

Relative paths in either configuration are resolved from the directory
containing that file. Absolute paths also work. The files reference the
local private keys by path. `private_network` defaults to false and
`server_name` is an optional fallback TLS identity.

```sh
target/release/horde preflight --config "$HOME/horde-pilot/controller.json"
```

Preflight checks each peer through the authenticated protocol hello and
prints a JSON report with the address, expected identity, status and any
connection error. It exits zero when all peers are ready and one when a
peer fails. It sends no run operations and generates no HTTP traffic.
Inventory order is allocation order. The file-based settings and their
equivalent flags are mutually exclusive. The controller's request
budget, concurrency, rate, deadline and thresholds are CLI options.

From the controller machine, use a small budget first:

```sh
target/release/horde controller --config "$HOME/horde-pilot/controller.json" \
  --requests 20 --concurrency 2 --rate 4 --deadline 30 \
  --max-failure-rate 0 > pilot-complete.json
```

When using repeated `--grunt` flags, append an explicit identity as an
`=name` suffix. The success criteria are exit code 0,
two `complete` Grunts, `partial: false`, 20 started and completed requests,
zero failures, 20 HTTP 200s, 40 body bytes from the fixture and populated
merged p95/p99. Each Grunt should execute 10 requests. Count successes in each fixture’s access log to confirm the split. The aggregate report gives the combined counts and per-Grunt
states.

For each fault drill, save a separate controller report and target log.
Use a 40-request budget at 4 requests/s so there is time to act while a
run is active. Keep the same two loopback targets.

| Drill | Expected result |
| --- | --- |
| Wrong expected certificate identity for one Grunt | That Grunt receives no submission. The other keeps its assigned share. The report is partial and its configured thresholds are unevaluable. |
| Interrupt the controller once while both Grunts are reachable | It sends cancellation to active Grunts. The report is partial, retains only validated completed results, and exits 130 for SIGINT on Unix. A Grunt is confirmed cancelled only after its terminal reply. |
| Interrupt one control link while its workload is active | The controller reports the unreachable Grunt as missing, without resubmitting or reallocating its budget. Measure stop time locally. After the Grunt detects the disconnect, it cancels when its local reconnection grace expires. A silent link failure may instead be detected by an I/O timeout. The independent run deadline remains in force. |
| Stop the controller process abruptly | The controller cannot emit a final report. Check directly on both Grunt machines that workload connections and new starts stop after disconnect detection and grace. |

Stop after any unexpected continued load, duplicate submission, acceptance
of a wrong certificate or partial report passing a configured threshold.
End the test by stopping the controller, Grunts and local HTTP fixtures.
The current Grunt retains terminal records for its process lifetime.
See [controlled restart](#restart-a-grunt-after-saturation) for how to
start a fresh epoch.

A one-machine rehearsal over a private interface completed 20 requests
with ephemeral pilot certificates, 10 at each local target, and no
failures. This procedure has not been executed across hosts. Assigned
private addresses, matching pilot certificates and network access are
the operational prerequisites for doing so.

## Single-host alternative using containers

On Linux with a local Docker daemon, Rust, Python 3, OpenSSL and
`ldd`, run:

```sh
cargo build --release --locked --features distributed-transport
python3 scripts/pilot-docker.py
```

The script builds a temporary image from the local release binary and
HTTP fixture. It allocates private IPs on an internal Docker network,
issues fresh TLS certificates, and starts two Grunts and a controller.
It launches the commands with per-role JSON configuration and passes
the budget and rate directly to the controller.
Each Grunt's HTTP fixture listens on its own loopback interface. No
ports are published.

- Preflight checks both Grunts, then repeats with an incorrect TLS identity
  for Grunt B. It must report A as ready and B as failed. Both loopback HTTP
  fixtures must observe zero connections in both cases.
- The source IP check repeats the successful preflight with the same
  controller certificate and inventory from an unlisted private Docker
  IP. Both Grunts must close the control connection before the TLS
  handshake completes. Neither Grunt sends a `hello` or reaches its HTTP
  target. The JSON report records the source and both failures.
- Rotation uses two CAs, an IP SAN and a DNS SAN. During overlap, both
  controller leaves must reach both Grunts, one with the new certificate
  and one with the old. Removing the old controller leaf must deny it
  even with both roots installed. With only the new root and leaf
  configured, the new credentials must work and the old credentials and
  old-root-only trust must fail. Each preflight checks for zero HTTP
  connections. The pilot stages credentials in separate role directories
  on the Docker host and copies only public CSRs to the signer directory.
- The nominal run checks 10 requests per Grunt, 20 successful responses,
  40 body bytes and populated merged p95/p99.
- The interruption run disconnects Grunt B from the Docker network
  after its first HTTP request. It checks that B stops before spending
  its 20-request share, A completes its own 20 requests, and the
  controller reports B as missing with a partial result and an
  unevaluable failure threshold.
- The silent failure run routes B's TLS control stream through a
  test-only TCP relay on the internal network. After B's first workload
  request, the relay pauses both directions while keeping both sockets
  open. The controller must report an I/O timeout and mark B missing
  while A completes exactly its 20-request share. The failure threshold
  remains unevaluable. Docker log timestamps measure the controller
  detection time and the last HTTP connection at B's fixture. The pilot
  allows 0.5–2.5 seconds for controller detection and 3–6.5 seconds until
  the last observed connection at B, with at least two more seconds
  between B's last connection and A's last connection. These windows
  allow for observation and scheduling around the one-second controller
  timeout, three-second Grunt frame timeout and two-second reconnection
  grace.
- The maintenance run lowers both retention limits on Grunt B. It checks
  both saturation reasons and their HTTP effects, stops B with SIGTERM,
  and verifies that only B's epoch changes. A fresh two-request run then
  completes with one request on each Grunt.

The script removes its containers, network, image and temporary
credentials on exit, including on failure, SIGINT or SIGTERM. It writes
the combined report to `target/pilot-docker-report.json` on success.
Use `--output` to choose another location. The `distributed-pilot`
GitHub Actions job runs the same script and uploads the JSON report
as the `horde-distributed-pilot` artifact.

This exercises separate IPs and TLS identities on a single kernel.
Routing, firewalls and physical link detection remain checks for the
multi-host pilot.

## Restart a Grunt after saturation

Each Grunt keeps up to 64 run records and accounts for up to 16 MiB of
retained results by default. A result is charged for its encoded histogram
and a 32 KiB metadata allowance. For a maintenance drill, set
`max_records` and `max_retained_result_bytes` in the Grunt JSON to lower
these limits. Both accept positive integers up to their default values.

| Reason in the controller's per-Grunt JSON | Effect |
| --- | --- |
| `run_record_capacity` | The Grunt refuses the new submission before it starts HTTP work. |
| `result_storage_capacity` | The workload can already have reached the target. The Grunt cannot retain the new result, and previously stored results remain available. |

The Grunt writes each diagnostic to stderr the first time it hits that
limit in an epoch. The controller reports the affected Grunt as `failed`
with the corresponding `reason`, keeps completed metrics from healthy
Grunts, and marks the report partial. Configured thresholds are
`not_evaluable`.

1. Pause new submissions targeting the Grunt and let active runs finish.
   Save the controller reports and Grunt logs. Capture a preflight
   report and note the affected peer's `epoch`.
2. Send SIGTERM to that Grunt through its service manager or directly
   with `kill -TERM "$GRUNT_PID"`. Wait for the process to exit before
   restarting it with the same local configuration. The Grunt stops
   accepting control connections and cancels any work still active.
3. Run `horde preflight --config controller.json` and check that the
   restarted peer is ready and has a new epoch. Resume with a small,
   explicitly budgeted fresh run.

Restart clears the Grunt's in-memory records and retained results.
Requests bearing the old epoch are rejected. Keep budgets already
submitted out of the recovery run, since their HTTP effects may already
have reached a target. Preflight checks the control channel and reports
the epoch even while either retention limit is saturated.

## Rotate a pilot CA

Issue new leaves using new directories on each endpoint, starting with the
same inventory. Distribute the new public CA while keeping the old one.
List both trust roots in `tls.ca` in the Grunt and controller
configurations. On each Grunt, list the old and new public controller
leaves in `controller_cert`.
Schedule Grunt restarts between runs because trust and leaves are read at
startup. Replace the Grunt leaves in stages and use `preflight` with each
controller credential to verify the overlap.

Once the new credentials work everywhere, remove the old controller leaf
from every Grunt's allowlist and verify that it cannot reach `hello` with
both roots still installed. Then remove the old CA everywhere and check
again with the new credentials. Retire the old private material on the
endpoints and on the signer according to your local CA procedure.

## Evidence before the pilot

The Rust suite, Clippy and release build passed. On loopback, the
controller kept the healthy share after a Grunt was stopped abruptly.
When the controller was killed, both Grunts closed their active workload
connections after disconnect and stopped issuing work. Tests also cover
explicit interruption, reconnect within grace, duplicate submissions,
session saturation and recovery, stalled TLS handshakes, incomplete and
oversized frames, run admission, histogram collector reservations and
result retention. The retention test fills a smaller configured allowance
with a completed result and verifies that the next result is refused
while the first stays retrievable. These are bounds on the configured
accounting, not an end-to-end bound on process RSS.

Both histogram fuzz targets ran for about three minutes per build with
cargo-fuzz 0.13.2, Rust 1.102.0-nightly and AddressSanitizer. The campaigns
reused the prior corpus and checked each input within a 15-second timeout
and a 1 GiB process limit. Leak detection was disabled for the local
environment.

| Build | Arbitrary histogram envelopes | Generated valid histograms |
| --- | ---: | ---: |
| Debug | 859,981 | 221 |
| Optimized | 6,666,966 | 2,797 |

All four campaigns completed without a reported failure. The corpus and
logs are local build artifacts under `fuzz/target/pilot-campaign`. Actual
link loss and its detection time across separate machines remain pilot
checks.
