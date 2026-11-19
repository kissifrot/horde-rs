> **Historical measurements:** Temporary benchmark scripts, prototypes, raw results and profiles mentioned below were removed during cleanup. The summaries and measurements remain here; file paths and reproduction commands referencing those artifacts are historical.

# Distributed mode: proposed design

This document describes the planned distributed mode and the current offline and loopback implementations. The CLI does not open an agent listener or execute remote tests.

## Roles and trust

A controller submits a constrained test plan to explicitly configured agents. Each agent runs the local load generator and returns a result; it never accepts shell commands, executable code, arbitrary file paths or configuration changes from the controller.

Agents are security boundaries, not trusted relays. A controller credential authorizes only the operations and target ranges configured by the agent administrator. Network access to an agent alone grants no authority. A compromised authorized controller is still restricted by local policy. A compromised agent can falsify its own results; the controller must not treat those results as proof of target behavior.

## Transport and authorization

- Use mutually authenticated TLS with certificate validation in both directions. Agents authorize an explicit set of controller identities; controllers verify the intended agent identity. Fail closed on invalid, expired or untrusted credentials.
- Configure certificates, trust roots, identities and agent policy locally, outside the remote protocol. Document rotation and revocation before deployment. Never offer a plaintext or unauthenticated fallback.
- Bind the listener only when agent mode is explicitly started, to a locally configured address and port. Set connection, handshake and idle timeouts, and bound concurrent sessions and message sizes.
- Version the protocol and validate every field before allocating resources or starting work. Do not log credentials, request bodies or authorization headers.

### Experimental loopback protocol

The `distributed-transport` feature handles TCP streams accepted by the library caller. It checks that both endpoints are loopback addresses and requires TLS with ALPN `horde-grunt/1`. The Grunt validates the client certificate chain and authorizes the exact client leaf certificate against its local list. The controller must validate the Grunt certificate and expected server name. Each listed controller may submit any of the locally configured profiles. Test certificates are only for loopback fixtures.

Each message is a JSON object preceded by a four-byte unsigned length in network byte order. Frames must contain between 1 and 4,096 bytes. After authentication the Grunt sends a `hello` with `version` and a random 16-byte `epoch`. Requests carry `version`, `epoch`, a nonzero 16-byte `run_id` chosen by the controller, and an `operation` object. IDs are JSON arrays of bytes.

| Operation type | Extra field | Behavior |
| --- | --- | --- |
| `submit` | `profile` | Starts a locally configured plan and budget. The same run ID and profile return existing state. A different profile for that ID returns a conflict. |
| `inspect` | | Returns the current state. For an active run it attaches the current session. |
| `cancel` | | Requests cancellation. Repeats return the current state and never restart work. |

Replies are `state` with a `status`, or `error` with a `code`. Run IDs are scoped to the authenticated controller certificate. A reconnect with the same certificate can reattach before the configured grace expires. Once it expires, the Grunt cancels the run and keeps its terminal state. The run's own deadline still applies. A restarted endpoint rejects requests bearing the old epoch. Controllers must preserve the original epoch for retries. Terminal records remain for that epoch. When the bounded record table is full, new runs are refused while existing records remain available. Session count, TLS handshake and frame operations also have local limits.

This slice exposes state only. Result and histogram transfer, listener and process management, certificate rotation and remote deployment remain separate work.

## Target and resource policy

Each agent has local allowlists for target schemes, hostnames, ports and resolved IP ranges. Validate every resolved address and connect only to an approved address; retain the intended hostname for HTTP Host and TLS verification. Reapply policy to every redirect and DNS resolution. Reject the request if a permitted destination cannot be established safely. Disable implicit proxy use in agent mode; an eventual proxy feature must be explicitly configured and enforce equivalent destination policy.

Set finite local limits for concurrent runs, requests, concurrency, request rate, duration, per-request timeout, redirect count, body and header sizes, result size and memory. The agent rejects plans exceeding its limits; the controller cannot raise them. Each run has a finite duration and is cancellable. An agent must not execute arbitrary local file reads: request bodies, if supported, are bounded data in the plan.

### In-process Grunt policy

With `distributed-offline`, `run_local` requires a trusted `GruntPolicy`. Its allowed origins are exact scheme, hostname and port tuples. Literal IP addresses must also match an allowed CIDR. For a hostname, the HTTP connector receives the checked OS resolver answers directly and rejects the whole answer set if any address is outside policy. It keeps the URL hostname for HTTP Host and TLS verification. Every redirect is checked before it is followed. A reused connection already uses a checked destination. The Grunt client disables environment and system proxies.

The local caller supplies `AgentLimits` for samples, concurrency, rate and duration. The policy also checks request timeout, redirects, explicit headers, request body and worst-case encoded histogram size before starting. Clones of the same policy share admission limits for active runs and histogram count-array memory. The memory reservation covers active collectors and their merge array. Retained results, connection buffers and other process allocations are outside this accounting, and a future transport still needs its own framing, session and queued-result limits.

## Run lifecycle and results

The protocol needs explicit versioned operations to submit, inspect and cancel a run. Give each run an unpredictable identifier scoped to the authenticated controller. Define duplicate submissions, disconnects, cancellation, expiry and restart behavior before implementation; retries must not accidentally execute the same load twice. A controller cannot inspect or cancel another identity's run without local authorization.

Distribute the request budget, concurrency and rate explicitly across agents. These are aggregate limits, not limits independently repeated on every agent. Start times may differ: report per-agent timing and controller-observed wall time, and do not imply synchronized clocks. Define timeout and cancellation behavior for in-flight requests.

Return completed and failed counts, failure categories, status counts, bytes, connections and redirect counts per agent. Compute aggregate throughput from the controller-observed run window, not by averaging agent throughputs. Report missing or failed agents and partial results explicitly; never silently retry or present a partial run as complete.

Do not average percentiles. For bounded-memory mode, merge compatible histogram counts, then compute percentiles from the merged distribution. Reject incompatible histogram settings and version them in the protocol. Preserve the distinction between exact and approximate results and the conservative p95 threshold behavior. Exact distributed percentiles require transferring all samples or a separately specified exact approach; do not label an approximation exact.

## Implementation order

1. Define typed plans, results, limits and a versioned protocol independently of networking. Test validation, run identity and histogram aggregation.
2. Implement local policy enforcement, including address pinning and redirect checks, with adversarial tests for DNS changes, prohibited addresses, proxies and oversized inputs.
3. Add the authenticated transport and explicit agent lifecycle. Test invalid and unauthorized identities, certificate expiry, disconnects, cancellation and resource exhaustion.
4. Add controller scheduling, partial-result handling and functional tests with multiple local agents. Benchmark controller and agent overhead before claiming scalability.

Do not enable a network listener until these controls and tests are implemented together.

## Offline contract proposal — review draft

This section defines the first single-URL slice for discussion. It does not freeze a wire format or enable an agent. Scenario execution, credentials from the agent environment, scheduling over a network and remote execution are outside this slice.

### Plan and budgets

Define a versioned controller plan independently of `TestPlan` and HTTP client types. It describes one absolute HTTP(S) target, method, ordered headers, optional body bytes, connection reuse, redirect policy, timeout, request budget, concurrency, optional duration and optional target rate. Header and body data are values in the plan, never paths to files on the agent. Preserve repeated headers and request bytes. Do not log the plan, credentials, body or target query; diagnostics identify the field and reason without its value.

All numeric values have explicit units and checked bounds. Reject zero or out-of-range values, unsupported versions, invalid methods, headers and URLs, excessive headers or body size, and unsupported configurations. A plan passing structural validation is not authorized to run: an agent separately applies its administrator's limits and target policy, including DNS and redirect checks. Message and decoded-object sizes must be limited before allocation and deserialization; ordinary post-deserialization validation is insufficient.

The controller's request count, concurrency and rate are **global** budgets. For a fixed set of active agents, split each integer budget deterministically: divide evenly and give the first agents one additional unit until the remainder is exhausted. The sums must equal the global budgets. For example, 100 requests across three active agents become 34, 33 and 33. Never send a zero budget interpreted as unlimited. If a finite budget is smaller than the number of available agents, select fewer active agents and report which ones were not used. A duration is a shared maximum start window, not divided across agents. Agent deadlines and elapsed durations use local monotonic clocks; no assumption of synchronized clocks is made.

The existing local runner allows an omitted `-n`, `--duration` or `--rate`. The distributed agent still needs finite administrator-enforced resource limits. Before implementation, decide whether omitted values are rejected or explicitly constrained by agent policy; never silently present a capped run as an uncapped one. Likewise, decide how the controller handles unequal agent capacities: a simple equal split may be rejected by an agent even when another distribution would fit. Initial support can reject the plan and report the limit rather than silently redistribute.

### Identity and lifecycle

A controller creates an unpredictable run identifier using a cryptographically secure generator. The identity and authorization of a controller or agent come from the future authenticated transport, not from a self-declared ID in the payload. Specify the scope of run IDs, how submission retries are deduplicated, and whether run state survives restarts before implementing remote submission. In the offline aggregation contract, reject results for the wrong run, duplicate agent results and unrecognized agents. A missing or failed agent is explicit; never silently retry or treat a partial result as complete.

### Results and time

Each agent result carries its run identity, status, locally measured elapsed duration, completed and failed counts, failure categories, final HTTP status counts, bytes, opened connections, redirects followed and latency data. Failure categories may overlap. Validate count consistency and use checked addition; reject malformed, oversized, incompatible or overflowing results. An authenticated agent can still provide false data: these checks do not establish truthfulness.

The aggregate preserves individual agent status and reports missing and failed agents. Its throughput is total completed requests divided by the **controller-observed wall-clock run window**, not a mean of agent throughputs. Define the exact start and end events, setup time, cancellation and timeout semantics before transport implementation. A partial aggregate must be marked partial, including when it has a nonzero throughput. Do not derive a global elapsed duration by comparing agent clocks.

### Latency representations

The current local bounded collector uses `hdrhistogram` 7.6.0 with nanoseconds, lowest discernible value 1, highest trackable value `u64::MAX`, three significant digits and `u64` counts. It separately stores exact observed minimum, maximum and a floating-point sum for the mean. A transferable representation must preserve those extra values if aggregate min, max and mean are to retain their present semantics. It must also carry a format/version and the histogram configuration; reject incompatible inputs. Merge counts first, then calculate percentiles. Exact mode would require transferring every sample or a separately specified method, so the initial distributed slice should use bounded mode and clearly label it approximate.

Two candidates need a prototype and measurements before a decision:

| Candidate | Benefits | Costs and checks |
| --- | --- | --- |
| HDR V2 binary | Existing histogram serializer and compact zero-run encoding; avoids inventing bucket encoding. | The crate's `serialization` feature also enables `flate2`, `nom` and `base64`. V2 cannot encode a single count above `i64::MAX`. Its deserializer constructs a histogram from header parameters and resizes a buffer from a declared payload length before returning: cap and validate the header, configuration and lengths **before** invoking it. Do not accept compressed V2 without separately bounded decompression. |
| Explicit sparse buckets | A simple, inspectable contract can fix configuration and bound bucket and count totals. | Horde must define canonical bucket indices or bounds, validate ordering, uniqueness, count sums and overflow, and maintain interoperability itself. A JSON array has additional size and parsing cost; limits must apply before allocation. |

The V2 safety observations above come from the locally installed `hdrhistogram` 7.6.0 source, not a measured benchmark. Benchmark encoding, decoding, size, allocations and merge cost on representative distributions, including empty, sparse and dense histograms. Measure separately from request execution. Confirm round trips preserve bucket counts and extra statistics, and that malformed metadata cannot trigger unbounded allocation. The existing `BoundedLatency::merge` is internal and assumes compatible local collectors; it is not yet a validated wire decoder.

#### Isolated prototype measurements — 2026-11-03

An isolated Rust prototype compared the crate's **uncompressed HDR V2** serializer with an explicit sparse-bucket JSON representation. Both carry the observed min, max and exact floating-point bit pattern of the accumulated sum. HDR uses a small binary envelope; JSON encodes the sum bits as a hexadecimal string. Both decoders enforce a 1 MiB input limit and a 10 million sample ceiling for this experiment. These are prototype values, not proposed production limits. The V2 prototype checks the fixed configuration, header, payload length and encoded counts before calling the library; the JSON prototype validates format, ordered unique canonical buckets, counts, extrema and sum. Compressed input is rejected.

Measured locally on an Intel Core Ultra 7 155U, release build, `hdrhistogram` 7.6.0: five separate process launches; each reports the median of five batches of 100 operations after warm-up. Times are microseconds per operation. The current prototype forbids `unsafe` in its own code and uses the default allocator; dependencies are outside that guarantee. Results are illustrative, not production thresholds; no networking or concurrent agents were measured.

| Distribution | HDR V2 bytes | Sparse JSON bytes | HDR encode / decode µs | JSON encode / decode µs |
| --- | ---: | ---: | ---: | ---: |
| Empty | 69 | 110 | 0.13 / 7.67 | 0.16 / 7.92 |
| 100 samples, 5 occupied buckets | 82 | 174 | 13.02 / 7.72 | 29.60 / 8.13 |
| 100,000 samples, 2,170 occupied buckets | 2,803 | 28,759 | 12.91 / 15.16 | 62.89 / 76.08 |
| 80,000 samples, 40,000 occupied buckets | 40,068 | 564,305 | 51.36 / 125.72 | 775.71 / 1,376.18 |

An earlier, separate prototype used an `unsafe` counting allocator to estimate allocations; that instrumentation has been removed and was not used for the timings above. In that earlier run, the 2,170-bucket case allocated 453,295 bytes cumulatively for V2 decoding and 581,593 for JSON. Both instantiate a fixed-size HDR collector of approximately 440 KiB, even for an empty histogram. On the 40,000-bucket case, JSON encoding allocated approximately 4.0 MiB cumulatively versus 391 KiB for V2. These counts are not peak live memory or RSS, and reuse or streaming could change them. Future allocation measurements can use an external profiler. A test occupying all 56,320 buckets with two samples each produced 56,388 bytes in V2 and 929,524 bytes in JSON; this is one distribution, not a universal maximum for JSON payloads.

After decoding, both candidates produce the same HDR structure. Separately, creating an empty collector and adding two copies of the distribution took a median of 10.26 µs for empty histograms, 26.31 µs for sparse histograms, 26.45 µs for the 2,170-bucket case and 41.72 µs for the 40,000-bucket case. These measurements exclude decoding; they use the same five-launch, five-batch procedure.

Round trips and a two-histogram merge preserved bucket counts and p50/p90/p95/p99/p100, extrema and the stored sum bits in the tested cases; separate round trips covered zero and `u64::MAX` nanoseconds. With the project's default `serde_json` configuration, a preliminary JSON-number round trip changed one bit of a floating-point sum; the measured JSON prototype therefore uses explicit bits. This preserves the stored floating-point value, not mathematical exactness of the original sum. JSON interoperability for 64-bit bucket values and counts also needs an explicit rule for consumers without exact 64-bit integers.

The prototype rejected selected oversized frames, malformed V2 lengths and configuration, compressed V2, truncated payloads, an excessive zero run and duplicate or zero-count JSON buckets. These checks are examples, not a security audit or a proof of bounded decoding. In particular, the JSON implementation builds its bucket vector before applying the bucket-count limit, although the input frame is capped. The crate's V2 decoder also has payload-level edge cases beyond its header; a production decoder requires an independently reviewed preflight or another bounded implementation, adversarial tests and fuzzing. Neither candidate is ready to expose to an untrusted peer.

### Proposed bounded HDR decoder — for review

This proposal is an offline format and validation contract, not an implemented or approved protocol. It covers one agent's single-URL latency histogram. It does not define transport framing, peer authentication, authorization or retry behavior. Our implementation should use safe Rust; the dependency boundary is reviewed separately. The `/tmp` prototype is evidence for discussion, not production-ready code.

#### Envelope

Propose a binary envelope, with all integers in big-endian order. It contains exactly one **uncompressed** HDR V2 histogram. No credentials, request data or target URL belong in this envelope.

| Offset | Bytes | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `HLDH`, identifying this proposal's envelope |
| 4 | 2 | version | `1`; other versions are rejected |
| 6 | 2 | flags | bit 0 means samples are present; all other bits must be zero |
| 8 | 8 | sample count | Number of observations; must match the V2 counts and the corresponding agent result's completed count |
| 16 | 8 | minimum | Exact observed minimum, in nanoseconds |
| 24 | 8 | maximum | Exact observed maximum, in nanoseconds |
| 32 | 8 | sum | IEEE 754 binary64 bits of the locally accumulated latency sum, in seconds |
| 40 | 4 | V2 length | Byte length of the following complete HDR V2 data, including its 40-byte header |
| 44 | variable | V2 data | Uncompressed V2 header and counts payload; no trailing bytes |

For an empty histogram, the presence flag, count, minimum, maximum and sum bits are all zero; the V2 data must also decode to zero observations. A nonempty histogram sets the presence flag and has a positive count. Observed minimum and maximum are separate from the HDR bucket bounds: a real sample can be zero or `u64::MAX`. The sum preserves the agent's stored floating-point bits, not a mathematically exact sum. The V2 data alone is therefore insufficient to preserve the existing exact-extrema and mean semantics.

The containing result binds the envelope to a run and an authenticated agent and supplies the completed count. Envelope validation checks consistency; it cannot prove the agent measured honestly. Integrity and replay protection belong to the future authenticated transport and run lifecycle, not to this byte layout.

#### Limits and validation order

Limits are set by the receiving side's trusted configuration, never by the received envelope. Before allocating a buffer for the payload, the future transport must check the declared whole-message length against its local message limit and an aggregate memory/concurrency budget. The offline decoder accepts an already bounded byte slice; it does not read an unbounded stream or allocate a buffer based on an untrusted length. The 1 MiB and 10 million sample limits used in the prototype are **not** production defaults.

For envelope version 1, only the local collector's configuration is accepted: nanoseconds, `low = 1`, `high = u64::MAX`, three significant digits, integer-to-double ratio 1.0 and normalizing offset 0. This configuration has 56,320 count slots in the tested `hdrhistogram` 7.6.0 implementation. Confirm that number against the pinned dependency and golden fixtures during implementation; changing configuration or encoding requires an explicit compatibility decision. With at most nine encoded bytes per slot, 40 bytes of V2 header and 44 bytes of envelope, a conservative format bound is **506,964 bytes**. The effective bound is the smaller of this format bound and the receiver's configured bound. Do not use a peer-provided value as a limit.

Proposed fail-closed sequence:

1. Require at least 44 envelope bytes, supported magic and version, known flags, a length within the effective bounds, and an exact match between declared length and remaining bytes. Reject truncation, extra bytes and arithmetic overflow. Require at least the full 40-byte V2 header before reading it.
2. Check the **uncompressed** V2 cookie (`0x1c849313`), the V2 payload length against the actual V2 length, and every fixed configuration field above before calling the library. Reject the compressed cookie and all unsupported formats. This avoids letting the library allocate from untrusted header parameters or an unchecked payload length.
3. Preflight the entire counts payload on the bounded slice, without allocating a vector of decoded buckets. Parse the HDR V2 zigzag/LEB128-64b9B representation exactly: at most nine bytes per integer, with the ninth byte carrying eight bits. Positive decoded values are counts; negative values skip that many zero slots; zero consumes one empty slot. Reject truncated encodings, overflow, an excessive zero run, a slot index beyond the validated slot count, and counts or checked sums above the receiver's sample limit. In particular, reject the encoding of `i64::MIN` before any signed negation. A V2 encoder cannot encode an individual bucket count above `i64::MAX`; fail rather than truncate.
4. Compare the preflight count with the envelope count and the enclosing result's completed count. Apply the smallest applicable trusted ceiling, including the agent's assigned request budget when present. Check the presence flag and empty/nonempty rules. If a rate-only or duration-only run lacks an explicit request budget, an independently configured finite sample ceiling is still required.
5. Only then call the pinned HDR library decoder on the validated V2 slice. Require full consumption, the expected configuration and the same count. Reject errors rather than attempting repair. Check that min and max are ordered, correspond to the first and last occupied buckets, and that the sum is finite and nonnegative. For an empty histogram, reject nonzero extrema or sum bits. Additional checks relating the floating-point sum to extrema need a rounding analysis so that valid long runs are not spuriously rejected; they cannot prove the source is honest.
6. Merge only fully validated, compatible histograms. **Before** adding, use checked arithmetic to ensure the aggregate sample count and every affected bucket fit; the current `hdrhistogram` 7.6.0 `add` implementation uses saturating addition for matching buckets, so a successful return alone does not prove there was no overflow. Check floating-point totals for non-finite results, preserve global extrema, and compute percentiles from merged buckets. An invalid histogram invalidates its agent result; partial-result policy must make that failure visible rather than treating it as an empty histogram.

The bounded preflight must match the library's interpretation, including zero counts, zero runs and the special ninth varint byte. The protocol does not yet require rejecting every non-minimal encoding that the library accepts; decide that interoperability rule with fixtures before freezing version 1. Pin and review changes to the HDR dependency: a library upgrade must rerun the differential and adversarial tests. A second parser is additional maintenance and security surface; it is justified only if the equivalence and bounds are tested rather than assumed.

#### Review and test cases before integration

- Golden round trips from the actual V2 serializer: empty, one zero-duration sample, `u64::MAX` nanoseconds, sparse and fully occupied histograms; retain exact min/max and sum bits. Check all bucket counts and representative percentiles after a multi-agent merge.
- Each envelope and V2 header field: unknown magic, version, flags, compressed cookie, different bounds or precision, nonzero normalizing offset, invalid ratio, bad lengths, truncated and trailing input. Exercise every byte boundary and the effective size limit without allocating from claimed lengths.
- Counts: single zero, legal zero runs, ninth-byte values, `i64::MIN`, truncated varints, slot boundary and one beyond it, per-bucket and aggregate count overflow, sample ceilings and mismatches with envelope or completed counts. Compare accepted preflight results with the pinned library decoder.
- Metadata: empty/nonempty disagreement, reversed or unoccupied extrema, zero and maximum extrema, NaN/infinite/negative sums, and aggregate sum overflow. Include long runs to check any rounding tolerance before enforcing it.
- Fuzz the envelope and payload with bounded input, asserting no panic, bounded resources and differential agreement for accepted payloads. Test in both debug and release builds; the library contains signed arithmetic on untrusted decoded values that must never receive `i64::MIN` from the preflight.
- Resource tests for many concurrent decodes and interrupted reads belong with the future transport. Test local memory budget enforcement before allocation and avoid sharing mutable decoder state unsafely.
- Test compatible merge against all original observations, reject incompatible histograms and preserve explicit partial-agent status. Benchmark preflight, decode, merge and peak memory separately from request execution.

Outstanding decisions for review: envelope layout and whether version 1 should require canonical V2 encodings; the configured memory and sample-limit model; the treatment of a missing result after cancellation or timeout; and whether a two-pass preflight plus library decoder remains preferable to a separately reviewed single-pass bounded decoder. No implementation choice above establishes destination authorization or makes an agent safe to expose.

#### Recommendations for review — not yet approved

**Decoder.** Prefer a safe-Rust bounded preflight followed by the existing `hdrhistogram` V2 decoder. A completely independent single-pass decoder could avoid parsing twice, but would make Horde responsible for implementing and maintaining the full V2 mapping. The preflight still duplicates sensitive parsing logic, so it is not automatically safe: require differential tests against the pinned library, adversarial tests and fuzzing before exposing it to peers. The prototype's checks are a starting point, not this implementation. The local measurements include preflight and show that this cost was acceptable for the tested distributions; they do not measure networked agents or production load.

**Envelope and encodings.** Keep the proposed fixed versioned envelope, fixed histogram configuration and uncompressed V2 only. Reject unknown versions, flags and compression. Require producers to use the standard serializer, but initially accept non-minimal V2 encodings only if the complete bounded preflight accepts them and their interpretation agrees with the pinned decoder. This avoids making an additional canonicalization rule an accidental interoperability requirement. If differential tests show a safety or ambiguity problem, reject the problematic encoding explicitly before freezing the format. The exact envelope and compatibility rules remain review proposals.

**Format and memory bounds.** Apply the derived 506,964-byte ceiling to an individual version-1 envelope, additionally limited by a potentially smaller locally configured cap. This is distinct from a cap on the complete future result message, whose framing and encoding are not yet chosen. Read only the fixed header before accepting a declared length; reserve budget before allocating or queuing the bounded payload. Separately bound the number of concurrent decodes and total retained result/aggregation memory. As a starting operational proposal, allow at most two concurrent decodes by default, configurable down or up within a separate local memory budget. A decode can temporarily involve an input buffer, the library's payload buffer and an approximately 440 KiB histogram; a retained aggregate histogram and any queued results must also be accounted for. Derive reservations from the actual implementation, measure peak memory and test saturation before adopting numeric byte defaults. A bookkeeping budget is not by itself a guarantee of total process RSS or immunity to allocation failure.

**Sample limits.** The receiver enforces the smaller of its locally configured finite sample ceiling and the agent's assigned request budget when one exists; the payload cannot raise either. Do not adopt the prototype's 10 million samples as a universal default. Check counts before deserializing and again against the agent result. Runs with only a duration still require an explicit finite local resource ceiling; reaching it must be reported as a limit/partial outcome rather than silently counted as a completed run. Define enforcement and suitable administrator-facing configuration with the agent lifecycle.

**Omitted run limits and agent capacities.** Preserve the meaning of explicitly requested budgets; never silently reduce `-n`, duration or rate. For the initial distributed mode, recommend requiring an explicit finite target rate rather than implicitly throttling an unlimited request. An `-n`-only run may be subject to an agent's separately disclosed operational deadline; hitting that deadline is a partial/limited result, not successful completion. When duration is supplied without `-n`, enforce and report the local sample ceiling as above. Begin with deterministic equal budget splitting; validate each share against each agent's configured limits and reject an infeasible plan rather than silently reassigning work. Capacity-aware scheduling can be added later. These are UX and lifecycle choices for approval, not current behavior.

**Missing or invalid results.** Preserve each agent's terminal status. A timeout, cancellation, invalid histogram or missing response makes the aggregate explicitly partial; do not substitute zeros, silently retry or present thresholds as passing on a partial result. A partial snapshot may contribute only if its provenance, identity and completeness are explicitly defined and verified by the future protocol; otherwise exclude it. Use controller-observed time for aggregate throughput and report the window separately. Exact start/end events and retry/idempotency rules still require lifecycle design before networking.

**Tests before network exposure.** The acceptance list above is a starting point. Add golden fixtures, differential preflight/library tests, malformed-length and count-overflow cases, debug and release fuzzing, bounded allocation and concurrent-budget tests, plus regression benchmarks. Reject a release criterion based solely on the existing prototype or performance measurements. No future listener should be enabled until transport authentication, target authorization and local resource controls are also implemented and tested.

### Acceptance cases for the offline slice

- Version, run ID, URL, method, duplicate headers, body bytes and every resource field round-trip without exposing sensitive values in diagnostics or debug output.
- Invalid or oversized fields and unknown versions fail before execution; local policy rejection remains distinct from structural validity. Cover zero, maximum and overflow boundaries.
- Partition uneven budgets, fewer agents than budgets, budgets smaller than available agents, and optional limits; assert exact global sums and no implicit unlimited zero values.
- Aggregate zero, one and multiple agent results, including an explicitly missing agent, failures, duplicate identities, unknown agents, wrong run IDs and checked counter overflow.
- Merge compatible histograms with deliberately skewed distributions; compare percentiles with a collector containing all original samples. Cover empty results, count mismatches, different configurations, invalid buckets and corrupt or oversized payloads.
- Confirm throughput uses controller-observed elapsed time and partial status is visible; do not use the mean of per-agent rates.
- Confirm the work adds no listener and does not change the local request hot path. Benchmark any encoding and aggregation overhead independently.

## Offline implementation review notes — 2026-11-03

The offline implementation now rejects an individual decoded histogram when its mean in milliseconds would be non-finite. It divides the sum by the count before converting to milliseconds, avoiding an unnecessary intermediate overflow. Merging also checks the sum and resulting mean before mutating the aggregate. This is a numerical validity check, not proof that the reported sum matches the measured observations; stricter plausibility bounds still need a rounding analysis.

A known agent's structurally invalid or overflowing result is recorded as `Invalid`, while an unknown agent, duplicate result or wrong run does not change aggregate state. `add_encoded` ties histogram decoding to this transition; callers that decode separately must still explicitly record a decoding error. Threshold evaluation has four outcomes: not configured, not evaluable, passed, failed. Configured thresholds are not evaluable for partial or empty aggregates. There is no distributed CLI or CI exit code yet.

The first decoded histogram is moved into the aggregate rather than cloned. The differential tests now compare every HDR bucket and representative percentiles for generated distributions, and include a nine-byte non-minimal integer. These are regression tests, not coverage-guided fuzzing, a security audit, or proof of equivalence for every accepted byte sequence. Separate fuzzing, stable golden fixtures, and review remain required before networking.

#### In-process Grunt aggregation

`Aggregate::add_local_outcome` accepts the terminal outcome of `run_local` with the Grunt's assigned budget and receiver limits. For a complete run it uses the actual start count and the report counters, encodes the bounded histogram in memory, and submits both to the existing result validator. Partial runs contribute only a terminal status. The aggregate retains metrics from completed Grunts and leaves configured thresholds unevaluable when any Grunt is incomplete.

Loopback tests run two Grunts concurrently and cover budget splitting, combined counters, p95 from the merged histogram and cancellation. The Grunts send HTTP requests to local fixtures and hand their results to the aggregate in the same process.

## Local peak RSS experiment

Measured on Linux 7.0.0-42 x86_64, Intel Core Ultra 7 155U, Rust 1.99.1, release build. The untracked harness at `docs/offline-memory` creates one 56,404-byte envelope with one count in each of the 56,320 histogram slots. It runs in three separate processes: fixture only (`baseline`), one decode (`decode`), or two decodes followed by one merge (`merge`). Each line below is a separate invocation; GNU time reports process peak RSS in KiB.

```text
baseline: 3180 3272 3288 3232 3228   median 3232
 decode: 3340 3628 3668 3500 3472   median 3500
  merge: 3880 3852 3728 3764 3616   median 3764
```

Reproduce from the repository root (the harness and its own Cargo.lock remain untracked):

```sh
CARGO_TARGET_DIR="$PWD/target" cargo build --offline --release --manifest-path docs/offline-memory/Cargo.toml
for mode in baseline decode merge; do
  for run in 1 2 3 4 5; do
    /usr/bin/time -f "$mode %M" target/release/horde-offline-memory "$mode"
  done
done
```

The differences of medians, about 268 KiB for one decode and 532 KiB for two decodes plus merge relative to baseline, are indicative only. RSS includes the process, allocator, fixture construction and touched pages; its peak can happen at different times in each process. Subtracting independently measured peaks does not measure exact incremental peak allocation, and the baseline itself constructs and drops an empty histogram. Allocator reuse, demand paging, sample size, order and system noise affect the numbers. The harness tests a dense uncompressed histogram, not worst-case encoded length, interrupted reads, simultaneous decoding, queued buffers or multiple aggregations. It does not measure allocation counts, CPU overhead or a memory budget's enforcement. Repeat with an allocation profiler and concurrency/saturation tests before setting reservations or exposing a listener.

## Follow-up offline checks — 2026-11-03

`AgentLimits::validate` now rejects an individual agent rate above the existing local runner's 1,000,000,000 starts/s limit and counts/concurrency that cannot be represented as `usize`. A global distributed rate may exceed this number if each assigned share is acceptable. This validation is separate from actual scheduling.

The offline `StartGate` exercises start limits: request and duration targets are normal stops; a locally imposed deadline or sample ceiling makes the outcome partial. It accounts for in-flight starts and does not expose a terminal outcome until they drain. Duration-only runs are subject to the local sample ceiling; request-only runs are subject to the local deadline. It uses monotonic instants and rejects backwards time. A future executor must actually invoke this gate on each start and completion, schedule the rate independently, and translate the outcome into its result. It is not connected to the existing runner, does not cancel hung in-flight requests, and is not a security boundary. Decide timeout/cancellation behavior before integration.

The tests include a literal version-1 binary golden fixture and 20,000 reproducible envelope mutations. Coverage-guided libFuzzer targets now exercise arbitrary envelopes and generated valid histograms. In the first 90-second optimized AddressSanitizer campaigns, the decoder target completed 2,609,242 inputs and the generated target completed 3,403 inputs without a decoder failure. Two initial generated-target failures were harness mistakes, involving the floating-point representation of an empty sum and the percentile expectation for the maximum HDR bucket. Both were corrected and the inputs replayed successfully. The campaigns used `ASAN_OPTIONS=detect_leaks=0` because LeakSanitizer fails under this environment’s ptrace restrictions. Continue persistent fuzzing, include a debug build and review resource bounds before exposing a network listener.

### Concurrent decode RSS

The untracked `docs/offline-memory` harness was extended with `parallel-2`, `parallel-4` and `parallel-8` modes. Threads start decoding together and retain their decoded histograms until every thread has completed; the same dense 56,404-byte input is shared. Measured with the same machine, release build and GNU time as above, five separate processes per mode, in KiB:

```text
baseline:   3348 3364 3268 3284 3120  median 3284
parallel-2: 3992 4060 3812 3852 4012  median 3992
parallel-4: 4700 4824 4552 4764 4816  median 4764
parallel-8: 6600 6596 6764 6572 6572  median 6596
```

Use the command in the earlier section, substituting `baseline parallel-2 parallel-4 parallel-8` for the list of modes. The peak RSS includes thread stacks, allocator behavior, and process overhead. Peaks of independent processes cannot be subtracted to derive exact incremental reservations; shared input also omits per-message input buffers. Neither allocator-level live/peak allocations nor a bounded concurrent admission mechanism was measured. Do not derive a production byte limit from these measurements.

### Result and lifecycle design still to validate

The current aggregate carries only completed/failed counts and the histogram. A future complete per-agent result needs elapsed duration, received final status counts, failure categories, bytes, opened connections and followed redirects, as in the local `Report`. Proposed validation: failed <= completed; each overlapping failure category <= failed; checked sum of HTTP status counts <= completed; checked addition of every counter and every status bucket during aggregation; a finite status-key domain and limits before parsing. HTTP categories may overlap and status counts may be lower than completed when headers were not received. The result should retain per-agent terminal state and timing as well as aggregate totals. These rules still need review against all runner error paths and serialization design; they are not implemented yet.

Before transport, define the controller's exact throughput window and run-ID scope, deduplication, disconnect/cancellation/deadline and restart semantics. The controller must authenticate identity independently of payload IDs; agent destination policy and bounded admission must be enforced before execution and allocation. No listener has been implemented.

## Complete single-URL result contract — proposal for review

This is a logical offline contract, not a selected wire serialization or an implemented protocol. Version 1 covers one target and bounded HDR latencies. It does not cover scenarios or expose an agent. All fields below are required for a completed agent result unless explicitly optional. Integer counters are unsigned 64-bit, durations are monotonic elapsed durations, and their eventual representation must state units without floating-point loss. Reject unsupported versions and unknown enum variants; decide unknown-field evolution with the eventual encoding.

### Identity and outcome

- `version`: logical result version 1. `run_id`: the controller's 128-bit run identifier. The receiving controller looks up an expected, authenticated transport identity and the run's assigned budget; payload values do not authenticate an agent.
- `outcome`: `completed` with a stop reason of `request_budget` or `duration`; or a non-complete outcome: `sample_limit`, `agent_deadline`, `cancelled`, `execution_error`, `invalid`, `timed_out` or `missing`. `invalid`, `timed_out` and `missing` may be controller-observed rather than agent messages. Preserve the actual source of each state. A configured local cap reached before a requested stopping condition is non-complete, never silently reported as completed.
- For version 1, only `completed` results contribute metrics. An agent reporting a non-complete outcome has its outcome retained but contributes no metrics, even if it supplies a snapshot. An invalid result cannot be repaired into a completed result. A future protocol may define authenticated partial snapshots and their timing/completeness rules separately.
- A completed result contains `started`, `completed`, and `failed`. `started` counts logical requests actually started, not slots reserved while waiting for a rate limiter; `completed` counts those whose final response body finished or that failed. To emit a completed result all started requests must have terminated, so `started == completed`. Each logical request is counted once even across redirects. `failed <= completed`.
- For a plan with an explicit assigned request count, require `started <= assigned_requests`. If the reason is `request_budget`, an assigned count must exist and `started == assigned_requests`. If the reason is `duration`, an assigned duration must exist; a smaller request count is valid. If both stop conditions become true together, define deterministic precedence in the execution contract and test it; both explanations must be consistent with the assigned plan. A duration ending with zero starts is valid but thresholds are not evaluable. An agent with no assigned request count still has a locally configured finite sample ceiling; use it when decoding.
- `elapsed` is the agent's locally measured duration from its load generation start to completion of its started requests and collection of its result. It is descriptive only: do not compare clocks or use it as the aggregate wall time. The exact event boundaries and maximum/zero rules must be fixed alongside the executor; never trust peer timing to prove duration enforcement. Failure to drain requests under the agent's operational policy is non-complete, not a fabricated completed result.

### Metrics and consistency

- `status_codes`: counts by final HTTP status whose headers were received, including responses whose body later fails. Redirect intermediate responses are not included. Accept numeric status codes 100 through 999, matching the current Rust HTTP status representation, rather than silently discarding unfamiliar codes; bound the number of distinct keys to 900 before constructing an unbounded map. Reject duplicate codes in an encoding that permits them. Require each represented count to be positive and the checked sum `statuses <= completed`. The difference `completed - statuses` must be <= `failed`: the current runner marks every request without a received final status as failed. Do not require a status for every failure.
- Failure categories are `http`, `timeout`, `connection`, `body`, `redirect_limit` and `other`, matching the local runner. Each count is <= `failed`; categories can overlap (for example HTTP 500 followed by a body timeout), so their sum may exceed `failed`. Every failed request has at least one category in the present runner, so the mathematical sum of the categories must be >= `failed`; compute that comparison without overflow. The HTTP failure count equals the checked sum of final status counts in 400..=599, because those statuses are marked failed even if the body later fails. Recheck these invariants if local error classification changes.
- `response_bytes` counts received body bytes, including partial bodies. `connections_opened` counts successfully established HTTP connections; `redirects_followed` counts redirects accepted for following, including those before a later failure. None is bounded by `completed`: redirects and reconnects may generate multiple connections or redirects per logical request, and byte counts depend on responses. Each counter and aggregate addition must be checked, including per-status and per-category additions; overflow rejects the affected result/aggregate, never wraps or silently saturates.
- At the time of the contract review, local code used saturating additions for body bytes and relaxed atomic `fetch_add` for connections and redirects, which could respectively saturate or wrap. The follow-up below adds overflow detection. Before constructing a completed distributed result, make overflow detectable with a measured performance cost, or explicitly design a different representation with an overflow indication; do not certify the present counters as exact merely by checking the received result. Likewise ensure `started` reflects actual starts rather than the current scheduler's reserved request IDs.
- `latency`: exactly one version-1 bounded HDR envelope, including the empty histogram for zero completions. Its sample count must equal `completed` and pass the existing bounded preflight, configured sample ceiling, optional assigned request ceiling, and checked histogram merge. Latencies cover each completed logical request, including failures and body consumption. Percentiles are approximate upper bounds; min/max are the separately stored observed extrema. Do not average agent percentiles. Exact latencies are out of scope for this version.

### Controller aggregation

The controller creates the expected set of authenticated agents before accepting results. Reject unknown agents, wrong run IDs and duplicate terminal results. Decode only after enforcing trusted frame, concurrency and memory bounds. A malformed, incompatible or overflowing result marks that expected agent invalid; no part of it contributes to the aggregate. Aggregation must be transactional: first validate the entire result and check every sum and histogram merge, then commit all metrics and terminal state. Do not lose an earlier valid agent's contribution when rejecting a later one.

Retain terminal state and local elapsed duration per agent; retain individual metrics only if the product needs them and their memory is budgeted. Any missing, timed-out, cancelled, limited, failed or invalid expected agent makes the whole run partial. The aggregate can display totals from fully completed agents, clearly labeled partial, but configured CI thresholds are non-evaluable. A complete aggregate with zero samples is also non-evaluable. Aggregate throughput is completed logical requests divided by a controller-observed monotonic wall-clock window; do not average agent rates. Exact window boundaries, a zero window, and whether coordination/setup time is included are open lifecycle decisions, not implied by this contract.

### Offline acceptance cases before implementation review

1. Empty completed result with duration stop; nonempty request-budget stop; duration stop before the assigned count; both boundaries coincident; started/completed mismatch; count over assigned or local sample ceiling.
2. 200 followed by failed body; 500 followed by a body timeout; connection failure without a status; redirects followed before redirect-limit or connection failure. Confirm category overlap, missing-status and HTTP-failure invariants against the actual runner.
3. Reject duplicate/out-of-range/zero status entries, a status total above completed, impossible missing-status and HTTP-failure counts, missing error category, and arithmetic overflow. Include a valid category sum greater than failed and valid multiple redirects/connections per request.
4. Empty, sparse and dense histograms; mismatch with completed, incompatible configuration, sample/byte ceilings and merge overflow. Ensure a rejected agent leaves prior accepted metrics unchanged and marks the run partial.
5. One or more complete agents, missing agent, timeout, cancellation, local cap, invalid result, duplicate agent, wrong run, and non-complete agent with an unsolicited snapshot. Confirm partial status and non-evaluable thresholds, not a zero-valued success.
6. Simulate counters at their integer boundaries without running huge tests; verify overflow is surfaced before an eligible result is emitted. Independently measure the hot-path impact of any added accounting.

### Decisions requested

Recommend the model above: exclude all partial snapshots initially; expose `started` and require it to equal `completed` for a completed agent result; use logical version 1 without yet choosing JSON or binary framing; reject overflow rather than presenting saturated counts as exact. Confirm those choices and review the consistency rules before implementing. Authentication, target authorization, bounded admission and lifecycle semantics remain separate prerequisites for transport.

### Implementation notes after contract approval

The proposed logical result contract was approved and its offline validation is implemented behind `distributed-offline`. `CompletedResult` and `ResultMetrics` describe the version, reason, local elapsed time, starts/completions/failures, failure categories, status buckets and other counts. `Aggregate::add_completed` checks the assigned budget and local sample cap, validates metric relationships, checks aggregate counter arithmetic, decodes the bounded histogram and only then commits the metrics and agent state. Other terminal outcomes are recorded without accepting snapshots. An invalid known agent makes the aggregate partial; wrong run, unknown or duplicate agents leave it unchanged. The logical version is not a wire encoding or authenticated identity.

This is receiver-side validation only. A trusted caller must supply the actual assigned budget and trusted limits, and future framing must bound allocations before constructing the status map. There is intentionally no conversion from the current local `Report` to an accepted distributed `CompletedResult`. Overflow detection and its measurement were subsequently added as described below; the conversion and actual-start accounting still need implementation and validation. Per-agent local elapsed values are retained for completed results; controller window boundaries and the timing of incomplete outcomes remain lifecycle decisions. Result serialization, target authorization, execution integration and transport remain unimplemented.

## Local counter overflow checks and benchmark — 2026-11-03

Local single-URL runs now remember overflow when accumulating received body bytes and when incrementing the shared connection and redirect counts. The worker merge also checks byte totals. A run with an overflow returns a counter-specific error rather than a report with a wrapped or saturated count. Atomic increments use the existing `fetch_add` and set a persistent flag only on wrap; byte addition records the overflow bit. Unit tests exercise the integer boundary and concurrent atomic increments. The local runner still does not produce a distributed result: actual starts and lifecycle integration remain to be implemented.

Release binaries built with Rust 1.99.1 on Linux 7.0.0-42, Intel Core Ultra 7 155U. Initial harness: 10,000 requests, warmup, five runs per workload, sequential before then after, same prebuilt Rust loopback server. JSON records are `docs/benchmark-counters-before.json` and `docs/benchmark-counters-after.json`. Binary SHA-256: before `22993c7b03f6ffa31c03657bcc3bd1818b6f21095ab4be66891108557ef6adb9`; after `107b4e2ff0be88a760249cfd1a79d0e680a7e7ffe6dad4399ff7b707e5451512`; server `a173aa83cee60de673bc287a306d391203e67efba207885be6afe6624035a925`.

The first comparison varied strongly: median changes serial -2.6%, concurrent -0.6%, high concurrency -4.6%, no reuse -13.6%, one redirect +27.8%, three redirects +9.8%; even bounded scenario throughput changed +36.1% although this change did not add counter work to that scenario path. This is evidence of substantial environmental variation, not evidence that all changes are causal.

A follow-up used one shared server, 30,000 requests per sample and five pairs per workload, alternating order AB BA AB BA AB after warmup; raw data and script are `docs/benchmark-counters-paired.json` and `docs/benchmark-counters-paired.py`. Median throughput in requests/s and full ranges:

| Workload | Before median (range) | After median (range) | Change |
| --- | ---: | ---: | ---: |
| serial | 33,721 (27,100–37,307) | 33,607 (27,918–37,558) | -0.3% |
| concurrent | 81,856 (68,862–111,517) | 74,675 (61,431–87,919) | -8.8% |
| high concurrency | 100,700 (90,273–113,463) | 99,773 (92,025–107,234) | -0.9% |
| no reuse | 30,506 (25,124–35,102) | 30,359 (23,577–34,663) | -0.5% |
| one redirect | 40,312 (38,285–47,274) | 37,345 (30,002–46,659) | -7.4% |
| three redirects | 18,805 (16,417–22,372) | 16,938 (14,905–22,949) | -9.9% |

All ranges overlap, pairwise changes vary widely, and median absolute rates also shifted markedly between experiments. The second experiment is a stronger control on run order, but does not establish a statistically reliable overhead or rule out a real regression, especially for redirects. No CPU profiling or allocation profiling was performed. A quieter machine, more repetitions or a targeted counter microbenchmark/profile would be needed before quantifying or optimizing the overhead. JSON and script are intentionally untracked; `/tmp` binaries are ephemeral.

## Counter benchmark after power-setting change — 2026-11-03

Repeated both earlier experiments after the user changed the machine's power setting. Binary and server hashes were verified unchanged. `cpu0` reported `scaling_governor=powersave` and `intel_pstate=active` before and after; this observation alone does not identify the user's complete power profile or prove fixed frequency. The full original harness again used 10,000 requests per sample, five repetitions per binary, identical server binary, and sequential before/after runs. Results are in the untracked `docs/benchmark-counters-power-before.json` and `docs/benchmark-counters-power-after.json`.

| Workload | Before median | After median | Change |
| --- | ---: | ---: | ---: |
| serial | 37,874 | 35,944 | -5.1% |
| concurrent | 194,003 | 192,939 | -0.5% |
| high concurrency | 213,230 | 195,516 | -8.3% |
| no reuse | 81,896 | 80,469 | -1.7% |
| rate limited | 1,000 | 1,000 | +0.0% |
| one redirect | 93,526 | 87,381 | -6.6% |
| three redirects | 47,399 | 45,758 | -3.5% |
| single-step scenario | 231,174 | 225,485 | -2.5% |
| journey | 74,731 | 75,176 | +0.6% |
| bounded journey | 70,531 | 68,683 | -2.6% |

Rates are requests/s for single-URL workloads and iterations/s for scenarios. The limited case is constrained by configuration and cannot show small speed differences. For one redirect the observed ranges were 92,004–97,578 before and 85,372–90,224 after; sequential order can confound the comparison.

The existing alternate-order script was rerun with the same server, 30,000 requests and five pairs (AB BA AB BA AB), writing `docs/benchmark-counters-power-paired.json`; the previous paired results were not overwritten. Medians and observed ranges in requests/s:

| Workload | Before median (range) | After median (range) | Change |
| --- | ---: | ---: | ---: |
| serial | 38,389 (32,997–42,706) | 39,354 (38,527–39,868) | +2.5% |
| concurrent | 196,707 (183,532–198,438) | 194,256 (188,019–197,821) | -1.2% |
| high concurrency | 217,584 (201,955–233,722) | 228,716 (221,136–235,582) | +5.1% |
| no reuse | 76,943 (76,487–78,922) | 76,301 (75,756–78,741) | -0.8% |
| one redirect | 89,179 (88,615–90,639) | 87,686 (86,658–88,759) | -1.7% |
| three redirects | 41,614 (35,083–42,572) | 40,825 (33,644–41,638) | -1.9% |

One redirect was slower after the change in every paired comparison (-3.7%, -0.3%, -3.3%, -1.1%, -1.1%); three redirects were slower in four of five (-4.1%, -5.9%, -3.9%, -1.9%, +0.7%). These results are consistent with a small redirect-path cost and justify a targeted investigation; five pairs on one machine are not a rigorous significance test or proof of causation. High-concurrency results differ in direction between the sequential and paired experiments, so no conclusion about its overhead is supported. The unchanged binary/server hashes enable comparison with the previous results, but `/tmp` binaries remain ephemeral. No profiling or power/frequency tracing was performed.

## Redirect counter investigation — 2026-11-03

Goal: investigate the small redirect slowdown seen above without changing tracked product code. `perf` 7.0.14 worked with `perf_event_paranoid=2`. The production checked binary is the previous `/tmp/horde-counter-after` (SHA-256 `107b4e2ff0be88a760249cfd1a79d0e680a7e7ffe6dad4399ff7b707e5451512`). A temporary source copy in `/tmp/horde-redirect-isolate` was built in release mode with only the redirect increment changed to a plain `fetch_add`; connection and byte checks were retained. Its binary hash was `aec95f5a8cac06ffc86d2a8738e769906f3e5308c538c73a83eb0699828ff1fd`. **This diagnostic binary cannot detect redirect counter overflow and must not be used as Horde.** It was built separately, so compiler layout and build-path effects are not excluded. The original binary was also retained (hash in the earlier section).

For controlled tests, `taskset` pinned Horde to CPUs 0–3 and the same Rust server (hash `a173aa83cee60de673bc287a306d391203e67efba207885be6afe6624035a925`) to CPUs 4–13. This changes the experimental topology and absolute rates compared with the earlier unpinned measurements; it does not guarantee fixed frequency or eliminate all contention. Local HTTP, concurrency 16, 100,000 logical requests per sample, five checked/unchecked pairs in alternating AB BA AB BA AB order, warmed per variant. `docs/benchmark-redirect-isolation.py` and `.json` contain the harness and raw values (both untracked):

| Redirects | Checked median (range), req/s | Unchecked median (range), req/s | Checked vs unchecked |
| ---: | ---: | ---: | ---: |
| 1 | 61,359 (59,907–61,738) | 61,625 (61,462–62,015) | -0.43% |
| 3 | 28,855 (27,585–29,242) | 28,991 (28,000–29,120) | -0.47% |

This isolation suggests the redirect overflow check alone is small in this setup; it does not prove a universal upper bound. The server and scheduling can mask client overhead, and the diagnostic build may change code layout.

`perf record` sampled `cpu_core/cycles/u` at 199 Hz with DWARF call graphs, client CPUs 0–3, server CPUs 4–13, three redirects and 200,000 logical requests per binary. There were 5,112 checked and 5,422 unchecked samples, with zero lost samples reported. The raw perf data remain under `/tmp/horde-redirect-{checked,unchecked}.perf.data`; untracked scripts/reports are `docs/profile-redirects.py` and `docs/profile-redirect-{checked,unchecked}.txt`. Report percentages are self sample shares, not request latency, allocation counts or total system CPU: in the checked profile `memmove` was 5.75%, `_int_malloc` 4.41%, `malloc` 3.19%, and URL/IDNA parsing and HTTP client clone/drop functions were also visible. The unchecked profile has similar dominant categories. Redirect count code is inlined; a 0.3% reporting threshold and sample noise prevent attributing a specific percentage to it. Kernel time and server CPU are excluded; profiling also changes run cost.

Five alternating `perf stat` pairs on the same affinity and three-redirect workload, 100,000 requests per run (`docs/stat-redirects.py` and `.json`), counted user-mode core events for the client only:

| Per logical request | Checked median (range) | Unchecked median (range) |
| --- | ---: | ---: |
| cycles | 292,082 (289,084–299,600) | 290,900 (290,742–294,375) |
| instructions | 346,561 (346,282–346,814) | 346,576 (346,333–346,844) |

Checked cycles were about 0.4% higher by median; instructions were effectively equal. These are whole-process user-mode counts normalized by logical requests, not cost measurements of the increment itself. Ranges overlap, only five pairs were measured, and no kernel or server counters were included.

A final three-way experiment used the original pre-counter binary, checked binary and diagnostic redirect-only-unchecked binary, five rotated orders, same pinning and 100,000 requests on three redirects (`docs/benchmark-redirect-three-way.py` and `.json`). Median/range req/s: original 29,599 (28,945–34,025); checked 30,291 (28,535–33,044); unchecked 29,385 (28,355–31,911). Strong overlap and changed relative ordering mean the experiment cannot attribute the earlier 1–2% slowdown to a particular check or prove that it has disappeared. The earlier unpinned comparison and this topology are different workloads from the system's point of view.

Conclusion: keep overflow detection; do not optimize or remove it on this evidence. Potential separate profiling targets include URL/IDNA parsing, request/connector cloning and allocation/copy work during redirection, but the sampled functions alone do not prove which change would improve end-to-end throughput. Test any proposal against redirect semantics and remeasure under representative and quieter conditions. No coverage of TLS, external network, kernel cycles or remote distributed execution is implied.

## Redirect URL parsing and request cloning investigation — 2026-11-03

No tracked product code was changed. The reqwest 0.12.28 source shows `Request::try_clone` clones the URL, headers and extensions and clones a reusable body; `Client::execute` consumes the request, so simply delaying that clone until the response arrives is not possible with this API. Even a final 200 response currently entails the clone made before sending it. Avoiding that clone safely would need a different request ownership/design and separate evaluation; do not assume that cloning a reusable body necessarily copies all its bytes.

A minimal, untracked `docs/redirect-micro` Rust harness measures isolated operations in release mode. Five separate processes, pinned to CPU 0, 300,000 iterations per operation per process, local loopback URL and two headers on the header-clone request; the body-clone request is a separate POST with 1 KiB reusable body and no added headers. Data in `docs/redirect-micro-results.csv` (nanoseconds per operation, warmups included before timing, medians and ranges):

| Operation | Median ns | Range ns |
| --- | ---: | ---: |
| relative `Url::join` | 112.6 | 101.4–115.7 |
| request clone with two headers | 108.2 | 103.4–112.9 |
| request clone with reusable body | 95.7 | 89.7–101.7 |
| current same-origin Referer construction | 74.2 | 73.6–75.6 |
| experimental same-origin Referer construction | 60.7 | 57.6–63.4 |

These are not allocation measurements or end-to-end latency shares; workloads differ, including the headers on the body example. The separate microbenchmark crate resolves its own dependencies under its untracked Cargo.lock; do not assume feature unification is identical to Horde. `Url::join` handles URL resolution and should not be replaced by ad hoc concatenation without a complete compatibility/security argument.

Experimental candidate (patch `docs/referer-fast-prototype.diff`, based on the current source at the time of measurement): calculate same-origin once for both header policy and Referer, and for the common same-origin case with no credentials and no fragment build the header directly from the existing URL string. All other cases use the original sanitization path, including HTTPS-to-HTTP downgrade suppression and cross-origin path/query removal. A standalone differential test compared the old and proposed Referer policy over 144 URL pairs, including credentials, fragments, ports, IPv6, percent encoding and downgrade. The existing release-mode integration tests `scenario_redirects` (7) and `standalone` (14) also passed on the prototype. These fixtures are not an exhaustive proof of URL-policy equivalence. The temporary build does not disable counter overflow detection. Baseline SHA-256 `107b4e2ff0be88a760249cfd1a79d0e680a7e7ffe6dad4399ff7b707e5451512`, prototype `f7d7b82ee748becf9c627fd3e7ef7221bbe63053bd0bfc5ee88d5b44c2e442f0`, server `a173aa83cee60de673bc287a306d391203e67efba207885be6afe6624035a925`. Different source/build paths can affect code layout; the binaries in `/tmp` are ephemeral.

End-to-end loopback measurement: `docs/benchmark-referer-fast.py` / `.json`, five alternating AB BA AB BA AB pairs, 100,000 logical requests per sample, concurrency 16, `-L` even in the zero-redirect control, warmup, client CPUs 0–3 and server CPUs 4–13. Rates in req/s:

| Redirects | Baseline median (range) | Prototype median (range) | Change |
| ---: | ---: | ---: | ---: |
| 0 | 158,129 (147,676–166,391) | 156,524 (131,293–161,892) | -1.01% |
| 1 | 66,698 (63,574–67,269) | 66,602 (65,148–67,795) | -0.14% |
| 3 | 29,372 (29,322–29,929) | 29,949 (28,776–30,203) | +1.97% |

Three-redirect pairwise changes were +2.83%, +0.66%, +2.11%, +1.16%, -1.86%; ranges overlap. The control varies too. These results suggest a small gain with repeated same-origin redirects, not a proven general speedup for single redirects or real sites.

A separate `perf stat` experiment (`docs/stat-referer-fast.py` and `.json`) used five alternating pairs, 100,000 requests, concurrency 16, three redirects and the same CPU pinning. In this file `checked` means the prototype and `unchecked` the baseline; both preserve overflow checking. For prototype vs baseline, median user-mode core instructions per logical request were 344,590 (344,185–344,784) vs 346,488 (346,294–346,732), approximately -0.55%; cycles were 287,842 (287,678–292,115) vs 289,273 (288,517–290,546), approximately -0.49%. These are whole-client user-mode counters, not per-function measurements; kernel and server work are excluded. CPU/sample variation, separate build/code layout, loopback topology and this specific URL distribution limit the inference.

Recommendation for review: the fast Referer path is a plausible, narrow optimization, but its gain is small and it introduces a second path in sensitive sanitization logic. If adopted, retain the fallback, add the differential and explicit security tests to the repository, benchmark cross-origin and credential-bearing cases, and rerun the existing suite before merging. A clone-avoidance redesign is not justified solely by this microbenchmark. Keep the current checked implementation until the prototype is reviewed.

## Referer fast-path confirmation attempts — 2026-11-03

This was a review of the existing prototype, not an integration into product code. Baseline and prototype hashes were rechecked unchanged from the preceding section. Added a synthetic HTTP server (`docs/referer-benchmark-server.rs`, compiled SHA-256 `9cb2f5159fb77aff0c36cd207d7fadaa30cc5aa80c7356d8ae8e6190b73a9d2c`) listening only on `127.0.0.1` and `127.0.0.2` at the same port. Relative paths exercise same-origin redirects; absolute locations alternate loopback IPs for cross-origin redirects. Server and client CPU affinities remain 4–13 and 0–3 respectively; proxy environment variables are removed and both addresses are placed in NO_PROXY. Every run validates completion, 200 statuses, bytes and redirect counts. Server implementation and workload differ from the previous experiment, so absolute throughput is not directly comparable.

Five alternating pairs per case, 50,000 requests each, 16 concurrent workers, query string on the initial URL; `docs/benchmark-referer-mixed.py` / `.json` (untracked). Median throughput req/s, with complete ranges:

| Case | Baseline median (range) | Prototype median (range) | Change |
| --- | ---: | ---: | ---: |
| no redirects | 169,613 (168,426–180,139) | 169,919 (167,407–175,315) | +0.18% |
| same origin, one | 64,639 (63,804–68,071) | 65,465 (54,473–67,274) | +1.28% |
| cross origin, one | 60,966 (59,852–61,841) | 60,702 (47,296–62,185) | -0.43% |
| same origin, three | 26,908 (25,282–27,280) | 27,033 (26,143–27,525) | +0.47% |
| cross origin, three | 25,136 (24,818–25,850) | 25,278 (24,901–25,746) | +0.57% |

For same-origin three redirects, a separate run used 15 alternating pairs of 50,000 requests (`docs/benchmark-referer-same-15.py` / `.json`). Medians were 26,936 vs 27,191 req/s (+0.94%); individual paired deltas had a median of +0.81%, ranging from -5.05% to +2.22%, with 8 positive and 7 negative. Baseline rates fell from 29,196 in the first pair to 25,216 in the last, so considerable temporal drift remains even with alternating order. These are not independent samples and no formal confidence claim is justified.

The actual HTTP client was also exercised with synthetic `user:password@` URL credentials, a query and fragment (`docs/benchmark-referer-auth.py` / `.json`), five alternating pairs each. Median baseline → prototype: same-origin one redirect 57,813 → 57,026 (-1.36%); cross-origin one 53,499 → 52,636 (-1.61%); same-origin three 25,375 → 25,526 (+0.59%); cross-origin three 24,314 → 24,194 (-0.49%). Full ranges overlap and one-redirect samples contain outliers. These figures do not establish a regression or a gain on their own. Reqwest extracts initial URL credentials into Basic authorization; this HTTP benchmark exercises that request behavior but does not prove that a `Url` passed to Referer still contains userinfo.

Expanded the isolated `docs/redirect-micro` harness with four separate cases and alternating old/fast order across five process launches. Median ns per Referer construction, baseline → prototype: same origin 86.53 → 62.07; cross origin 120.07 → 123.04; same origin with URL credentials 103.74 → 107.00; same origin with fragment 83.13 → 87.70. Samples and ranges are in `docs/redirect-micro-mixed-results.csv`. The fallback differences are only a few nanoseconds, with overlapping ranges and one process per sample; they do not directly translate to an HTTP-level regression. URL cases use synthetic data only. The harness isolates operations, not actual allocation counts.

Correctness checks: the temporary prototype's old-versus-new Referer comparison now covers the original 144 fixed pairs plus 2,592 generated combinations of scheme, host, credentials, ports, encoded path, query and fragment. Explicit tests verify same-origin credential/fragment removal, cross-origin path/query removal and HTTPS-to-HTTP suppression. `docs/test-referer-auth.py` additionally verifies both actual binaries over loopback: Basic auth remains on a same-origin redirect, is removed across origins, and the Referer has the expected sanitized form. The test and benchmark use only fictitious credentials. The original URL-policy and scenario/standalone integration suites had passed in the earlier investigation. The complete prototype diff, including tests, is stored as `docs/referer-fast-prototype.diff`.

Decision recommendation: do not integrate this fast path yet. It reduces isolated same-origin Referer construction time, but end-to-end improvements are typically around 0–1% in the added workloads, noisy and inconsistent across pairs, while adding a second code path to credential-sensitive handling. The earlier five-pair +1.97% three-redirect result was not consistently reproduced; the 15-pair run showed substantial drift. Keep the exactness/overflow checks and current implementation. A quieter controlled machine, longer independent randomized runs and broader realistic HTTPS/cross-origin workloads would be needed to establish a material end-to-end benefit before accepting the maintenance/security tradeoff. No real credentials or remote services were used.

## In-flight request stopping policy — proposal for review

This is a proposal for offline execution, not implemented behavior. It applies to distributed single-URL runs and preserves the existing meaning of standalone `--duration`; it does not enable a listener. All time decisions use the agent's monotonic clock. Administrative limits come from trusted local policy, and a controller cannot extend them.

### Relevant existing behavior

The local runner treats duration as a window for starting requests. After that window it waits for already-started logical requests. Its request timeout applies to the whole logical request, including the redirect chain and final response body; redirects do not reset the timeout. The current scheduler can reserve a request ID before waiting for rate scheduling, so reserved IDs are not proof of an actual HTTP start. The offline `StartGate` models limits but is not wired into the runner.

### Options for requests in flight when a run stops

| Option | Behavior | Advantages | Drawbacks |
| --- | --- | --- | --- |
| Immediate cancellation | Cancel all in-flight requests as soon as any duration or stop event occurs. | Shorter shutdown and a simple local resource boundary. | Changes the meaning of duration, biases metrics against slow responses and increases incomplete tests. Remote servers can still complete side effects. |
| Always drain under each request's timeout | Stop starting work, but allow all existing requests to finish or reach their individual timeout. | Preserves current duration semantics and provides complete request results. | A local run deadline could be exceeded by up to the request timeout, plus scheduling/finalization overhead; cancellation would not be prompt. |
| Drain for normal completion; enforce an absolute agent deadline and explicit cancellation | Drain after a requested duration or request-count stop, while the hard local deadline and request timeout continue to apply. Cancel in-flight work if the hard deadline or explicit cancellation occurs first. | Preserves ordinary duration semantics and bounds active work using local policy. | A run may become partial while draining; implementation must handle races and cleanup carefully. |

Recommend the third option. Request timeouts are measured from each actual start and are not restarted for redirects, body transfer or draining. A request that completes or times out on its own before the run deadline is a completed logical request; its normal success/failure classification and latency contribute to an otherwise complete result. Neither a request timeout nor an HTTP error alone makes an agent result partial. A forced run-level cancellation, including the administrative deadline while work is still running, does make it partial and is not silently rewritten as an ordinary request timeout.

### Clocks, admission and exact boundaries

Let `W` be the optional requested start-window duration, `T` the positive per-request timeout, and `D` the positive agent operational deadline relative to the locally defined execution start. `W` controls starts, not the completion time of already-started requests. `D` applies to active execution and draining; reaching it cancels remaining work. Define execution start precisely at the transition to active load generation after admission, plan validation and client setup. Admission, handshake, preparation and result transmission require their own finite lifecycle bounds before transport; do not claim `D` bounds them. Use checked duration/instant arithmetic and reject unrepresentable settings.

Before a start becomes visible or the HTTP operation is launched, check cancellation, `D`, `W`, assigned request count, local sample ceiling, rate scheduling and concurrency. A slot reserved before sleeping for rate scheduling is not a started request or a latency sample. Recheck time and cancellation after a wait and immediately before launching; coordinate the reservation/start boundary across workers so no new starts are admitted once a stop is committed. A start recorded strictly before `W` or `D` may still reach the operating system just after the boundary because of scheduling: the guarantee concerns Horde's admission decision, not the time at which the target sees bytes.

Recommend rejecting a requested `W >= D` at validation: under the hard-deadline precedence below, equality would otherwise make a duration-only run partial as soon as its requested window ended. This is stricter than the current offline `AgentLimits` validation and needs an explicit compatibility decision. If `W < D` but `W + T > D`, accept the plan only with the local deadline disclosed in the plan/preflight output: normal draining is possible, but a slow request can hit `D` and make the agent result partial. Do not silently shorten `W`, `T` or present the run as complete. An alternative is to reject every plan with `W + T > D`; it offers a conservative drain allowance but can reject useful plans whose responses normally finish much faster than their timeout. Preserve the current plan meaning and choose the disclosed hard cap first; a stricter admission policy can be added if needed. `W + T <= D` is not a wall-clock guarantee under runtime scheduling, synchronous blocking work or finalization delays.

For request-count-only runs, `D` is still required; a slow target cannot keep the agent occupied indefinitely. For duration-only runs, the finite sample ceiling limits actually started requests. If that ceiling would prevent a further start while `W` is still open and a concurrency slot is available, stop with `sample_limit` and mark partial; do not call a cap reached merely because all slots are busy and the requested duration can expire normally before another start is possible.

### State and precedence

A run has a single committed terminal reason. Before a terminal reason, normal request-count or duration completion stops new starts and enters a draining state. The normal reason is provisional until draining finishes; a deadline or cancellation can replace it before finalization. The existing offline `StartGate` fixes its stop reason and does not perform that replacement, so it must be extended rather than treated as an implementation of this policy. On a sample ceiling, explicit cancellation or agent deadline, stop admitting starts and produce a non-complete outcome. For version 1, non-complete outcomes do not contribute metrics or snapshots to the aggregate; configured CI thresholds are non-evaluable. Optional diagnostic data must not be confused with a validated partial snapshot.

Recommend these rules for simultaneous events:

1. A hard deadline or explicit cancellation committed before normal completion takes precedence over an in-progress drain. A finish exactly at or after `D` does not convert that run to complete. Treat explicit cancellation of an already terminal, fully finalized run as a no-op, not a replacement of the result.
2. If both requested count and `W` are satisfied before a run-level stop, prefer `request_budget` when the assigned number has actually started; otherwise use `duration`. No starts are permitted at or after `W`. The reason is not based on pre-reserved IDs.
3. If an individual timeout fires strictly before run-level cancellation, finish and classify that request as a timeout. At the same boundary, prefer the run-level partial outcome; do not assert a particular request category for discarded results.
4. A sample limit is partial only when it actually prevents an otherwise eligible next start before `W`; if the requested stopping condition has already been met, retain its normal reason. Define exact behavior at equality consistently using monotonic timestamps.

These precedence rules need a single serialized state transition or an equivalent race-safe mechanism, not independent worker decisions. They describe observable events; they do not guarantee real-time scheduling. Hard deadline enforcement must use a run-level mechanism independent of workers that might be waiting on I/O or a rate timer. Verify that cancellation and teardown are cancellation-safe for the actual Tokio/HTTP operations and do not leave detached tasks, unintended open connections, or unbounded waits. Dropping a local request future cannot undo a request or side effect already received by the target.

### Cancellation and disconnection

An authorized explicit cancellation is a run-level stop: prevent new starts, cancel in-flight operations, release resources, and report `cancelled`/partial if delivery remains possible. Do not fabricate normal completed-request samples for abandoned operations, and do not merge the partial result. Ensure repeated cancellation is safe and terminal reporting is deterministic. The eventual authentication, permission and idempotency rules for the cancel operation remain transport work.

Do not infer that a dropped controller connection is necessarily a cancellation: a transient disconnect and a deliberate stop have different meanings. The remote lifecycle still needs an explicit decision on disconnect, recovery, idempotency and agent restarts before transport. Regardless of that decision, the local deadline must bound active work; a lost controller must not create an unbounded run.

### Acceptance tests and measurements before integration

- No starts at or after `W` or `D`; duration finishing while work is active drains it if it finishes before `D`. Verify started versus reserved slots with a rate wait that crosses a boundary.
- Request timeout before `D` records one completed failed request, including time spent in redirects and final body. A normal drain that encounters such a timeout can still yield a complete agent result.
- A slow header, body or redirect chain still in flight at `D` is cancelled and the agent is partial; no partial histogram or counters are included in the aggregate. Test explicit cancellation before starting, during rate wait, during connect/headers, mid-body and during drain; no starts after cancellation.
- Request count reached before a long in-flight request: normal completion if it drains before `D`, partial if `D` interrupts it. Duration-only sample ceiling, duration expiry while all slots are busy, zero-start duration, simultaneous count/duration/deadline, repeated cancellation and controller loss as a separate undecided lifecycle case.
- Exercise race boundaries using a controllable monotonic clock and synchronization barriers rather than exact real-time sleeps; include multi-worker stress and verify that all worker tasks, timers, request futures and connection resources are cleaned up. Local loopback integration tests should complement deterministic unit tests.
- Measure standalone and experimental distributed paths with the existing benchmark, five repetitions, before and after. Protect the standalone hot path from unnecessary per-request synchronization; measure cancellation/deadline overhead under high concurrency, rate scheduling and redirects. No performance target is implied by this proposal.

### Approval and offline state-model progress

The stopping policy above was approved. The `distributed-offline` state model now rejects a requested duration greater than or equal to the local deadline, checks representability of the deadline, distinguishes a provisional drain reason from a finalized outcome, lets an observed deadline or cancellation override a normal drain, and leaves a finalized result unchanged. `observe`, `cancel` and `complete_at` take explicit monotonic instants; the first committed forced cancellation is retained while operations are being released. The gate exposes started and in-flight counts, and a test serializes simultaneous admissions with a mutex to verify the assigned request cap. Deterministic tests cover cancellation before work, during drain, at deadline and after finalization, plus the duration/deadline boundary and sample ceiling behavior.

At that stage this was still an isolated state model: the real HTTP runner did not yet invoke it, enforce a run-level deadline, abort or join request futures on cancellation, capture actual starts into distributed results, or produce distributed results. See the later integration section for subsequent work. A caller must drive `observe` independently, serialize transitions, stop new work and release each terminated operation; `complete_at` alone does not cancel network work. In particular, state-model tests do not prove cancellation safety or resource cleanup. The standard standalone release binary is unchanged (identical SHA-256 before and after this state-model change, `107b4e2ff0be88a760249cfd1a79d0e680a7e7ffe6dad4399ff7b707e5451512`); no claim is made about the performance of a future integrated distributed path. Implementation of the actual driver, loopback tests and benchmarks is still required before transport.

## Local HTTP agent integration and performance — 2026-11-03
The approved stop policy is now connected to the existing HTTP runner behind `distributed-offline` through `run_local`. A one-way `Cancellation` signal is idempotent; a preflight exposes the local deadline and whether the requested duration plus timeout can reach it. Normal stops drain, while cancellation or the agent deadline aborts and joins worker tasks and returns a partial outcome without metrics. A sample-cap stop wakes workers waiting for rate slots. The API requires callers to signal cancellation and await the run; dropping the caller future is not a substitute for verified graceful cleanup. No listener, authenticated remote transport, destination policy or conversion into a wire result is provided.
Loopback integration tests cover cancellation before start, before headers, while reading bodies, while waiting for a rate slot, with multiple concurrent requests, and during redirects; deadline, normal drain, individual request timeout, sample caps and actual-start counting are also covered. Peer-observed EOF/reset confirms closure for the tested HTTP/1.1 connections; it does not prove cancellation of remote side effects or every possible transport implementation. Rate/deadline scheduling is subject to cooperative runtime and OS scheduling; the deadline is not a hard real-time guarantee.
Performance: five alternating/rotated process launches per binary/case, 10,000 logical requests per launch, warmup, Rust loopback server pinned to CPUs 4–13 and Horde to CPUs 0–3, Linux 7.0.0-42 and Rust 1.99.1. Standalone before hash `107b4e2ff0be88a760249cfd1a79d0e680a7e7ffe6dad4399ff7b707e5451512`; standalone after `2c18b162afad3d3fb50d047e4b543279a67be5d40e44f650541c9ec0ee748dd3`; local agent harness `574443bf64e8314bc9e7d588edd25bb1d990def3687debb1cd0146e879cd9081`; server `a173aa83cee60de673bc287a306d391203e67efba207885be6afe6624035a925`. Client/server share a machine. Reported throughput uses the existing runner measurement window.
| Case | Before standalone | After standalone | Experimental local agent |
| --- | ---: | ---: | ---: |
| serial_standalone | 56367 | 56365 | — |
| concurrent_standalone | 159563 | 159579 | — |
| concurrent_bounded_rate | 13346 | 13373 | 13323 |
| redirect_three_bounded_rate | 10631 | 10645 | 10659 |

Units are requests/s. `serial_standalone` and `concurrent_standalone` use default exact latency and no rate. The other two cases use bounded latency and target rate 1,000,000,000 starts/s for all binaries; three-redirect case follows three redirects. The offline helper invokes `run_local`, with deadline 60 s and request timeout 10 s; the standalone CLI uses its default 30 s request timeout. No request timed out in these runs, but the timeout configurations were not identical. This is an experimental local API, not a remote benchmark. Comparing it with standalone includes its state checks, watch-based rate-wait wakeup and result path, not transport. Different benchmark executable linkage and code layout can affect results.

All individual rates (req/s, in run order):
```json
{
  "serial_standalone": {
    "before": [
      55427,
      55765,
      57614,
      57186,
      56367
    ],
    "standalone": [
      56365,
      55187,
      57439,
      57399,
      55771
    ]
  },
  "concurrent_standalone": {
    "before": [
      154582,
      160784,
      160817,
      159563,
      156591
    ],
    "standalone": [
      159822,
      158628,
      160304,
      159579,
      158487
    ]
  },
  "concurrent_bounded_rate": {
    "before": [
      13301,
      13466,
      13428,
      13346,
      13289
    ],
    "standalone": [
      13584,
      13240,
      13535,
      13373,
      13297
    ],
    "offline": [
      13323,
      13227,
      13384,
      13227,
      13410
    ]
  },
  "redirect_three_bounded_rate": {
    "before": [
      10664,
      10631,
      10634,
      10536,
      10480
    ],
    "standalone": [
      10649,
      10829,
      9467,
      10590,
      10645
    ],
    "offline": [
      10659,
      10609,
      10713,
      10414,
      10670
    ]
  }
}
```

A separate five-launch standalone experiment on concurrency 16 and zero redirects used the same rate and bounded-latency options individually and together. Median rates: plain 175,122 req/s; bounded only 160,324; rate only 13,438; both 13,392. These are this loopback setup and rate setting only; they indicate that activating the current rate scheduler can dominate the comparison, and do not by themselves identify the cause or generalize to all rates. They mask the incremental overhead of the experimental state machine. Investigate with profiling and representative rates before optimizing. Individual component rates:
```json
{
  "plain": [
    176447,
    175122,
    176132,
    174958,
    173997
  ],
  "bounded": [
    160324,
    163133,
    160104,
    159860,
    160739
  ],
  "rate": [
    13236,
    13478,
    13424,
    13542,
    13438
  ],
  "both": [
    13298,
    13392,
    13277,
    13520,
    13412
  ]
}
```

The temporary harness, binaries and JSON files were removed after transcribing the raw values here. No transport, internet benchmark or profiling was performed.

After the benchmark, a validation guard was added for duration-only runs: the local sample ceiling must be strictly below `usize::MAX`, which the current scheduler uses as its unbounded request-count sentinel. This prevents the scheduler from silently exhausting its representable IDs before the sample cap can be detected. The guard runs during preflight, outside the timed request path; the benchmark's assigned-count workload did not exercise this duration-only branch. The complete suite and targeted checks were rerun after this addition, but the performance binaries above were not rebuilt for it.

## Process interruptions and agent terminology — proposal for review

Current state: there is no distributed transport, controller or remote process. The offline local agent API has an idempotent `Cancellation` signal and returns a partial outcome after aborting and joining its request workers. Neither the CLI nor that API currently maps operating-system signals to this cancellation, and no remote cancellation, shutdown acknowledgement, recovery or identity mechanism exists. Standalone signal handling and its report/exit behavior are not yet specified by this proposal.

### Proposed controller interruption

For the eventual controller, a first interrupt requests cancellation of the active run: stop issuing work, send an authenticated, run-scoped and idempotent cancellation to all assigned agents concurrently, and wait only for a locally configured bounded grace period for terminal replies. An agent without a valid terminal response remains missing/unconfirmed; do not claim it stopped merely because a cancellation was sent. Show a cancelled/partial result with per-agent status; retain metrics only from agents whose fully completed results were already validated, in accordance with the existing contract. CI thresholds are non-evaluable and the controller exits nonzero. A second interrupt skips the grace period and terminates promptly, with no guarantee that the report can be emitted. On Unix, consider 130 for SIGINT and 143 for SIGTERM, with a separately documented portable behavior; decide exact CLI and JSON behavior during implementation.

If the controller crashes, loses power or receives SIGKILL it cannot send cancellation. Agents must have independent local deadlines and resource controls. A TCP disconnect is not itself proof of a deliberate cancellation. Options are to continue within the local deadline, cancel immediately when a loss is detected, or cancel after a bounded reconnection grace. Recommend cancelling after a short locally configured, finite grace period, so an unattended load stops while brief connection disruption need not necessarily abort it. This is a new lifecycle choice to review, not a claim of instantaneous detection: network failure detection and heartbeats have their own bounds, and the agent deadline applies even if loss is never detected. Reconnection during the grace must authenticate the same controller identity and run and must not start duplicate work; reconnect after cancellation may only inspect an existing terminal record, not silently restart. Scope, durability and authorization of those records remain to be designed before implementation.

### Proposed agent interruption

On SIGINT or SIGTERM, an agent should stop accepting new runs, signal cancellation to active runs, abort and join their request workers and release resources; wait for shutdown only up to a configured local grace. If its connection to the controller still works, send a sanitized terminal interruption status, but do not merge incomplete snapshots. If the deadline/grace expires or the process is killed, the controller sees an unavailable, timed-out or missing agent and marks the run partial. Killing a process is not evidence that already received target requests were rolled back. SIGKILL, kernel failure and power loss cannot run cleanup handlers or guarantee a final message.

Never automatically resubmit a lost or interrupted agent's work: a controller cannot know whether requests reached the target. A restarted agent must not resume an old run by default. Durable run-id deduplication and recovery would need a separate design; without durable state, reject ambiguous retries instead of promising at-most-once execution across restarts. Treat reported identifiers as labels only: authorize control and reconnection using the future authenticated transport.

### Terminology for review

Recommend **Grunt** for the remote load-generating process, retaining **controller** as the functional name for the coordinator. Grunts are Warcraft III Horde troops and the word is short enough for CLI examples. Explain the mapping once in English documentation (`Grunt: a remote load generator`), and keep security/error messages precise. Do not rename existing APIs, introduce a role command or freeze wire names until the user chooses the terminology. Alternatives: **Peon** (recognizable Warcraft worker but may imply building/harvesting rather than generating load), **Raider** (evocative but could be confused with the HTTP requests themselves), or plain **agent** for maximal operational clarity.

### Tests before deployment

Exercise first/second controller interrupt, bounded concurrent cancellation, unreachable agents, duplicate/reordered acknowledgements and non-evaluable thresholds. For agents, test shutdown while idle, in rate wait, in connection setup, mid-body and during drain; graceful shutdown of multiple active runs; forced termination without final messages; restart and ambiguous retries. Verify no new starts after committed cancellation and join/cleanup locally, then test real authenticated transport and process signals separately. Test connection loss and reconnect around the grace boundary with a controlled clock and real sockets. A mock or offline cancellation test alone cannot prove signal handling or remote lifecycle behavior.

## Local process signals and Grunt terminology — 2026-11-03
The name **Grunt** and a configurable reconnection grace were approved. No remote agent process, transport, reconnect protocol or grace configuration exists yet. The local CLI now handles SIGINT and, on Unix, SIGTERM for both single-URL and scenario runs. A first signal requests one-way cancellation; workers are aborted and joined before the interrupted report is printed. Text and JSON reports explicitly identify a partial interruption, omit metrics and mark CI thresholds not evaluable. Unix exits 130 for SIGINT and 143 for SIGTERM. A second signal or a fixed five-second local shutdown timeout forces exit; no final report or complete cleanup is guaranteed in that case. On non-Unix platforms, the CLI supports Ctrl+C. These process signal conventions do not imply recovery from SIGKILL, power loss, or remote side-effect rollback.
The same `Cancellation` primitive is used by the offline local Grunt execution API. No production Grunt executable exists. Unix process tests exercise SIGINT, SIGTERM, text and JSON, a scenario, simultaneous connections, second-signal forced exit, and a local Grunt fixture. The tested peers observed their HTTP/1.1 connections close. Tests do not establish behavior for an authenticated remote protocol, TLS teardown, uncooperative blocking tasks, or target-side cancellation.
Performance check: five alternating AB BA AB BA AB pairs, 20,000 logical requests/iterations per run with warmups, client pinned to CPUs 0–3 and Rust loopback server to CPUs 4–13. Linux 7.0.0-42, Rust 1.99.1, Intel Core Ultra 7 155U. Before binary SHA-256 `2c18b162afad3d3fb50d047e4b543279a67be5d40e44f650541c9ec0ee748dd3`, after `cabb82a57d5bd1d62644c393662aba72110a4df125ca3fa00ddd84d162c1ea5c`, server `a173aa83cee60de673bc287a306d391203e67efba207885be6afe6624035a925`. Medians and observed ranges:

| Case | Before | After | Change |
| --- | ---: | ---: | ---: |
| serial | 57,427 (54,670–57,856) | 56,784 (55,024–57,722) | -1.12% |
| concurrent | 160,523 (154,747–168,334) | 159,902 (159,621–163,551) | -0.39% |
| redirect_three | 31,210 (21,875–32,953) | 31,002 (29,757–32,352) | -0.67% |
| scenario_single | 154,431 (143,709–154,821) | 151,181 (146,009–153,447) | -2.10% |

Units are requests/s except scenario iterations/s. One-step scenario only; redirects case uses three. All ranges overlap, so five pairs on this single machine do not establish a significant regression or prove its absence. Scenario throughput is calculated from its reported elapsed time; other throughput comes from each JSON report. Raw samples, in run order:
```json
{
  "serial": {
    "before": [
      57856,
      57427,
      57521,
      54670,
      57267
    ],
    "after": [
      55652,
      55024,
      57722,
      56908,
      56784
    ]
  },
  "concurrent": {
    "before": [
      168334,
      154747,
      160842,
      160516,
      160523
    ],
    "after": [
      163551,
      161929,
      159621,
      159902,
      159763
    ]
  },
  "redirect_three": {
    "before": [
      32953,
      31210,
      31311,
      27054,
      21875
    ],
    "after": [
      32352,
      31002,
      31060,
      29757,
      29895
    ]
  },
  "scenario_single": {
    "before": [
      154431,
      154735,
      154821,
      143709,
      149712
    ],
    "after": [
      153447,
      152912,
      151181,
      146009,
      147348
    ]
  }
}
```

Temporary binaries, local server and benchmark script were removed after recording the results here. No performance claims are made for distributed transport.
