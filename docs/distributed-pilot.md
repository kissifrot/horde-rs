# Two-Grunt private-network pilot

The pilot exercises the control channel across three machines. Each Grunt
runs only against an HTTP server on its own loopback address. The sample
private addresses below are placeholders for addresses assigned to the pilot.

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

If there is an existing internal CA, submit the CSRs to it using the
roles and IP SANs below. For a dedicated pilot CA, the repository's
`scripts/pilot-tls.py` helper uses OpenSSL 3. Run these commands on the
indicated machines. Generate keys locally and send only the CSR to the
machine that signs certificates.

```sh
# CA machine, once
python3 scripts/pilot-tls.py init-ca "$HOME/horde-pilot-ca"

# Controller
python3 scripts/pilot-tls.py request controller "$HOME/horde-pilot"

# Grunt A
python3 scripts/pilot-tls.py request grunt-a "$HOME/horde-pilot"

# Grunt B
python3 scripts/pilot-tls.py request grunt-b "$HOME/horde-pilot"
```

On the signer, save the three public CSRs as `controller.req.pem`,
`grunt-a.req.pem`, and `grunt-b.req.pem`. Substitute the real IPs in
`GRUNT_A_IP` and `GRUNT_B_IP`. Sign the controller for TLS client use
and each Grunt for server use with its own IP SAN.

```sh
python3 scripts/pilot-tls.py issue controller --ca-dir "$HOME/horde-pilot-ca" \
  --csr controller.req.pem --out controller.der
python3 scripts/pilot-tls.py issue grunt-a --ip "$GRUNT_A_IP" --ca-dir "$HOME/horde-pilot-ca" \
  --csr grunt-a.req.pem --out grunt-a.der
python3 scripts/pilot-tls.py issue grunt-b --ip "$GRUNT_B_IP" --ca-dir "$HOME/horde-pilot-ca" \
  --csr grunt-b.req.pem --out grunt-b.der
```

The helper verifies each issued certificate and writes DER. The pilot CA
is valid for 30 days, with leaves issued for seven days. Keep
`ca-key.pem` on the signer. Put the public `ca.der` in
`$HOME/horde-pilot` on all three machines. Place each signed leaf as
`cert.der` beside that machine's generated `key.der`. Copy the public
`controller.der` to both Grunts as `controller.der` for the explicit
allowlist. With these IP SAN certificates, use bare IP socket addresses
for `--grunt`, without an `=name` suffix.

## Start each Grunt

Start the fixed-response fixture on both Grunt machines. It serves a
two-byte file from an isolated temporary directory and logs requests
locally.

```sh
pilot_target=$(mktemp -d)
printf 'OK' > "$pilot_target/index.html"
python3 -m http.server --bind 127.0.0.1 --directory "$pilot_target" 18080
```

In another terminal on each Grunt, set `GRUNT_IP` to this machine's
address and `CONTROLLER_SOURCE_IP` to the source address shown by the
controller's route lookup. With the certificate files from the previous step:

```sh
target/release/horde grunt \
  --listen "${GRUNT_IP:?set the local Grunt IP}:7071" \
  --allow-controller-ip "${CONTROLLER_SOURCE_IP:?set the controller source IP}" \
  --target http://127.0.0.1:18080/ \
  --ca "$HOME/horde-pilot/ca.der" --cert "$HOME/horde-pilot/cert.der" \
  --key "$HOME/horde-pilot/key.der" --controller-cert "$HOME/horde-pilot/controller.der"
```

Both processes should print their private listening address. For IPv6,
enclose the IP in brackets in each socket address such as `--listen`
and `--grunt`. Leave it unbracketed when signing and setting peer IPs.

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

With `--private-network`, an omitted identity defaults to the address's
IP SAN. Set `tls_identity` when the certificate has a different IP or a
DNS SAN. The DNS name is checked against the certificate. The connection
uses the literal socket address from the inventory.

```sh
target/release/horde preflight --private-network --inventory grunts.json \
  --ca "$HOME/horde-pilot/ca.der" --cert "$HOME/horde-pilot/cert.der" \
  --key "$HOME/horde-pilot/key.der"
```

Preflight checks each peer through the authenticated protocol hello and
prints a JSON report with the address, expected identity, status and any
connection error. It exits zero when all peers are ready and one when a
peer fails. It sends no run operations and generates no HTTP traffic.
The `controller` command also accepts `--inventory grunts.json` in place
of the repeated `--grunt` flags below. Inventory order is allocation order.

From the controller machine, use a small budget first:

```sh
target/release/horde controller --private-network \
  --grunt "${GRUNT_A_IP:?set the Grunt A IP}:7071" \
  --grunt "${GRUNT_B_IP:?set the Grunt B IP}:7071" \
  --ca "$HOME/horde-pilot/ca.der" --cert "$HOME/horde-pilot/cert.der" \
  --key "$HOME/horde-pilot/key.der" \
  --requests 20 --concurrency 2 --rate 4 --deadline 30 \
  --max-failure-rate 0 > pilot-complete.json
```

With these IP SAN certificates, the controller expects the listed IPs as
the identities. If using certificates with DNS SANs, append each expected
identity as an `=name` suffix. The success criteria are exit code 0,
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
For a fresh pilot session, stop it cleanly before starting a new epoch.

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
Each Grunt's HTTP fixture listens on its own loopback interface. No
ports are published.

- Preflight checks both Grunts, then repeats with an incorrect TLS identity
  for Grunt B. It must report A as ready and B as failed. Both loopback HTTP
  fixtures must observe zero connections in both cases.
- The nominal run checks 10 requests per Grunt, 20 successful responses,
  40 body bytes and populated merged p95/p99.
- The interruption run disconnects Grunt B from the Docker network
  after its first HTTP request. It checks that B stops before spending
  its 20-request share, A completes its own 20 requests, and the
  controller reports B as missing with a partial result and an
  unevaluable failure threshold.

The script removes its containers, network, image and temporary
credentials on exit, including on failure, SIGINT or SIGTERM. It writes
the combined report to `target/pilot-docker-report.json` on success.
Use `--output` to choose another location. The `distributed-pilot`
GitHub Actions job runs the same script and uploads the JSON report
as the `horde-distributed-pilot` artifact.

This exercises separate IPs and TLS identities on a single kernel.
Routing, firewalls and physical link detection remain checks for the
multi-host pilot.

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
