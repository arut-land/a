# MCP client choice and cost

Measured on the shared macOS development machine, 2026-10-21. Build jobs
are limited to three. Latency benchmarks are deliberately deferred to a
quiet machine: first-turn timings under simultaneous sibling builds
would not establish a regression or its absence.

## Library survey

A throwaway manifest outside the repository resolved three clients. For
each, `cargo tree -e normal -p <name> --prefix none --format '{p}'`
counted distinct package/version lines, including the root, removing
repeated `(*)` markers. No surveyed client was compiled or added here.

| Client | Features selected | Distinct packages | Runtime violation |
| --- | --- | ---: | --- |
| rmcp 3.6.0 | no defaults; client, transport-child-process | 54 | tokio 1.54.0 |
| rust-mcp-sdk 2.0.0 | no defaults; client, stdio | 67 | tokio 1.54.0 |
| mcp-sdk 0.0.3 | defaults | 61 | tokio 1.54.0 |

Manifests: [rmcp](https://docs.rs/crate/rmcp/3.6.0/features),
[rust-mcp-sdk](https://docs.rs/crate/rust-mcp-sdk/2.0.0/source/Cargo.toml),
[mcp-sdk](https://docs.rs/crate/mcp-sdk/0.0.3). Each fails the runtime
budget before code-size or CPU comparisons are meaningful. Adapting them
would require replacing their runtime-dependent client implementation.
We therefore implement only the tool client subset over `json` (through
`protocol::Json`), serde_json, `rt` and the existing process spawner.

Streamable HTTP reuses `llm::Http` / `llm::Client`, including backpressure
and closing the connection when dropped. There is no second reactor, HTTP
implementation or TLS configuration. Base64 uses maintained `base64`
0.23, already present through ureq-proto, rather than a custom encoder.

## Release size

`CARGO_BUILD_JOBS=3 mise run size` before changes: **2975 KiB**
(3,046,432 bytes). After implementation and merging main: **3072 KiB**
(3,145,856 bytes), an increase of 99,424 bytes (97.09 KiB). The difference includes main's intervening durability and
protocol work, so it is not an isolated measurement of MCP's contribution.
The fixture is behind a test feature and is not linked into the shipped
`a`; the size task's fixture scan passed. MCP changed no wire encoding.

## First-turn work

The daemon test verifies a configured server has not started after a
session's ordinary first turn. With no servers, the extension contributes
nothing. With configured servers, registration constructs one compact
server index and three static SDK tool specifications; it performs no
server I/O and starts no background work. Schema discovery happens only
inside a logged tool call. Three-server first-turn work is exercised by
`an_ordinary_first_turn_starts_zero_of_zero_or_three_configured_servers`:
both configurations complete their first turn with **zero server starts**.
Latency was not timed on this busy host.

Owner follow-up: on a quiet host measure identical first turns with zero
and three configured servers, with a warm daemon and isolated homes,
and run budgets/startup. Prompt bytes grow by the compact index, so model
provider latency cannot be asserted to be identical without measurement.

## Initial integration validation

After merging main, `CARGO_BUILD_JOBS=3 mise run check` passed formatting,
clippy and **932 tests**, with 28 intentionally skipped. `acceptance`
passed all 12 tests. `check:windows`, `machete`, `check:no-tokio`, `deny`
and `size` all passed. The workspace run includes web tests; no Swift
code was changed. Budgets, startup and provider latency measurements
were not run on this shared machine.

MCP invariants include
`discovery_is_lazy_and_only_selected_schemas_reach_the_model`,
`crashes_and_protocol_failures_are_logged_as_distinct_facts`,
`timeout_kills_the_request_and_a_later_call_gets_a_new_process`,
`cancellation_kills_the_process_even_when_the_server_ignores_it`,
`replay_never_repeats_a_server_effect_and_stop_owns_its_tree`,
`an_unfinished_external_call_is_never_reissued_after_daemon_crash`,
`approval_and_cancellation_precede_any_remote_effect`, and
`interrupting_an_active_call_kills_its_server`. Codec tests cover fragmented
JSON/SSE, response identity and safe mirrored HTTP headers.

## Review hardening

The HTTP transport now requires an explicit `Redirects` policy at every
`llm::Client` construction. MCP chooses `Refuse`: even same-origin redirects
return a typed outcome before a second request. The cross-host leak proof
and a same-origin 307 test exercise the real HTTP transport.

One state object per RPC request charges all notification methods against
256 messages / 64 KiB and retains one latest progress value, at most 1 KiB.
Publication has eight slots, with a reserved final update and 100 ms
spacing; pending progress is flushed even when the server goes quiet.
There is no background producer or unbounded intermediate queue. The
connection reuses an 8 KiB heap read buffer so the timer race does not
double an inline buffer in nested async futures.

Discovery counts serialized bytes before retaining each page, stopping at
1 MiB across the entire list. Summaries shorten descriptions to 512 UTF-8
bytes. The oversized-page reproduction returns a typed limit; another
test frames an accepted schema and its escaped card with at least 12 MiB
of frame headroom. Server and tool identities are validated newtypes.
A fixture that spawns a descendant verifies whole-group cleanup on both
daemon stop and cancellation.

The three independent review regressions remain in the suite:
`cross_host_redirects_never_receive_mirrored_arguments`,
`notification_flood_is_bounded_per_request`, and
`paginated_discovery_cannot_exceed_the_client_frame_budget`.

After merging main (`c7d9de5`), formatting, clippy and all **965 tests**
passed (28 intentionally skipped). Acceptance passed 13 tests, web passed
6 tests, and Windows lint, machete, no-tokio and deny all passed.
`CARGO_BUILD_JOBS=3 mise run size` passed at **3120 KiB** (3,195,584
bytes). The previous validated release was 3072 KiB
(3,145,856 bytes); the increase of 49,728 bytes (48.56 KiB) includes
main's fork work as well as review hardening. This is not an isolated
measurement of either contribution. Release artifacts and retained test
directories were removed, with no remaining test servers or daemons.
Quiet-machine latency/budgets remain deferred as requested.

Main subsequently added `ext-skills` and the general publication seam.
MCP now shares `Text::generation` for its compact index and `Cx::publish`
for explicit schema loading. Keys include the exact schema, deduplicate
only while its context is effective, and are restored from the log after
restart. The frame-budget test includes publication keys and text as well
as the result card. Transport state remains private to `ext-mcp`.

## Shared publication integration validation

After merging main's skills/publication work (`5918a4e`) and adopting it
in MCP (`f4efc19`), the final check passed formatting, clippy and **983
tests**, with 28 intentionally skipped. Acceptance passed all 13 tests;
web passed all 6. Windows lint, machete, no-tokio, deny and size passed.
An earlier full run lost a CLI `--version` warmup to SIGKILL; the unchanged
test passed on the complete rerun.

The final release is **3300 KiB** (3,380,192 bytes). The
previous validated pre-review release was 3072 KiB (3,145,856 bytes), a
difference of 234,336 bytes (228.84 KiB). This includes main's skills and
fork integration as well as MCP changes. Release artifacts and the one
retained startup-test directory were removed. No task-owned servers or
daemons remained; quiet-machine performance measurements stay deferred.

Implementation and integration commits, newest first:

```text
f4efc19 mcp: share generation context and typed schema publications
57a1dc7 docs: resolve skills and mcp design together
5918a4e merge: integrate shared context publication for mcp
6c30867 docs: record mcp review limits and validation
8e517c2 test: split mcp fixture dispatch and retain review cases
0a3b235 mcp: keep receive storage out of timer futures
c7d9de5 merge: integrate main with mcp review fixes
9193107 mcp: prove descendant cleanup on stop and cancellation
7ed1cd2 mcp: publish pending progress while the peer is quiet
e8588e9 mcp: validate identities at config and discovery boundaries
27aca66 mcp: bound aggregate discovery and compact summaries
37992f4 mcp: bound request notifications and coalesce progress
0beba5f mcp: refuse redirects before resending requests
```

## Schema content hash without a C toolchain

Measured on 2026-10-21, aarch64 macOS, Rust nightly-2026-10-17
(`rustc 1.101.0-nightly 5d8fec066`), with `CARGO_BUILD_JOBS=2` throughout.
`main` was already merged at `cf292dd`. Both measurements run
`mise run size`, hence the shipped `cargo build-release --workspace`
profile: opt-level `s`, fat LTO, one codegen unit, stripped symbols,
size-optimized rebuilt std, and serde_json at opt-level 3. Only the MCP
hash dependency and its call site differ in shipped code; test assertions
are not linked. Exact lengths come from `wc -c target/release/a`.

| SHA-256 implementation | Features | Shipped bytes | KiB (size task) |
| --- | --- | ---: | ---: |
| ring 0.17.14 | no defaults; std | 3,877,312 | 3786 |
| sha2 0.11.1 (RustCrypto) | no defaults | 3,860,784 | 3770 |

Choose **sha2**, saving **16,528 bytes (16.14 KiB)** while retaining full
SHA-256 collision resistance. Both builds pass the fixture scan. Sha2 uses Rust implementations and CPU intrinsics, without
a C build script or Windows SDK headers. The ring baseline reproduces
`assert.h` missing in `check:windows`; the replacement passes the same
plain cross-target clippy task.

Already-present alternatives are not suitable substitutes: `crc32c`,
`rustc-hash` (FxHash), and the small FNV-1a implementations are
noncryptographic. The lockfile also contains OpenSSL through native TLS
on other platforms; that is neither a portable pure-Rust dependency nor
linked by macOS TLS. Ring remains in the lockfile for Linux's existing
rustls transport, but `cargo tree -i ring --target` shows no users for
either `aarch64-apple-darwin` or `x86_64-pc-windows-msvc` after this change.
There was no other portable SHA-256 implementation already linked here.

The preimage remains the exact `protocol::Json` bytes of the remote tool
definition. Keys remain `(server, tool, "sha256-v1", [u8; 32])`, serialized
exactly as before. The existing publication test now pins both complete
keys for its original and changed schemas; it passed with ring before the
swap. Expected digest bytes were also independently computed with Python
`hashlib.sha256` over the same compact, sorted-key JSON. No key bytes or
version change, and no fixture regeneration is needed.

Validation: `mise run check` passed formatting, clippy and all **1168
tests** (30 skipped); `acceptance` passed all **23 tests**. `check:windows`,
`machete`, `deny`, `check:no-tokio`, and both `size` runs passed. Windows
lint also passed with target-specific `CC` and `AR` pointing at nonexistent
executables. All four black-box budgets passed: cold-start median
**28.73 ms** against a **2.05 ms** process floor; idle daemon RSS
**9792 KiB**, client RSS **8624 KiB**, and client CPU **0 ns** over three
seconds. These are local observations on the shared host, not a controlled
hash-performance comparison. Attach p99 was **193.54 µs**; the separate
release quiet-shell budget also passed with **zero runtime wakes** in one
second.

## Incremental stdio framing

Measured 2026-10-27 on the shared M3 (macOS 27.0.1 arm64,
Rust 1.101.0-nightly `5d8fec066`). Baseline is `5b6d12c1`; the candidate
changes only stdio framing in production. Both use the survey's release
probe profile: opt-level `z`, fat LTO, one codegen unit, identical resolved
dependencies, and `CARGO_BUILD_JOBS=2`. These are shared-host observations,
not quiet-machine or cross-platform latency qualifications.

The finding was confirmed before editing production code. Every pipe read
searched the entire accumulated message, making fragmented frames take
quadratic search work. Removing consumed bytes with `Vec::drain` kept the
largest frame's allocation on each cached connection. Initial baseline
reproduction measured 20.24 ms CPU/call [19.30–23.33] for 900,000-byte
responses; two footprint samples ranged 38.09–39.22 MiB at 32 cached
connections and 130.03–132.88 MiB at 128.

The connection now remembers how many pending bytes were searched. It
searches only newly read bytes and, after decoding a complete frame,
replaces its allocation with only the unread suffix. That suffix comes
from the final 8 KiB read; an empty suffix allocates nothing. Incomplete
messages keep their bytes and cursor across a dropped read future. This
removes repeated prefix scans and completed-frame retention without a new
dependency, message limit, wire change, or background task.

The workload is the survey's real stdio Python MCP peer and public MCP
extension probe (`api-probe/src/main.rs` and `mcp-peer.py` from the
`r3-tools-load` evidence archive). Local crate paths were retargeted to this
worktree, with the same manifest and lockfile for both binaries. One warmup
call precedes timing. Five rounds alternate baseline/candidate order,
reversing it each round: 30 calls per round at 900,000 bytes and 1,000 at
32 bytes. CPU is the client process's user plus system CPU divided by the
call count; the peer's CPU is excluded. Every result's exact text length is
checked. Values below are medians of round statistics [minimum–maximum].
P99 uses nearest rank; a 30-call round's p99 is its maximum.

| Metric | Before | After |
| --- | ---: | ---: |
| 900,000-byte CPU/call, ms | 26.16 [23.68–27.39] | 5.88 [5.74–6.27] |
| 900,000-byte round p50, ms | 28.81 [24.73–31.33] | 8.11 [7.99–9.91] |
| 900,000-byte round p99, ms | 34.62 [31.24–49.03] | 9.42 [8.62–18.65] |
| 32-byte round p50, µs | 52.29 [51.13–53.38] | 54.04 [50.75–55.25] |
| 32-byte round p99, µs | 120.29 [73.83–307.12] | 290.21 [75.79–781.21] |

Large-response client CPU falls 77.5%. Pooling the 150 large calls per
build gives p50/p99 of 28.38/41.42 ms before and 8.21/13.82 ms after.
The initial 32-byte pooled p50/p99 is 51.88/145.33 µs before and
52.38/341.12 µs after. The initial timing batch overlapped the local
Windows lint build. A second five-round alternating 32-byte batch after
that build completed gives the following results; other users of this
shared machine were not controlled.

| Follow-up 32-byte metric | Before | After |
| --- | ---: | ---: |
| Round p50, µs | 55.77 [33.65–61.85] | 57.29 [51.83–64.50] |
| Round p99, µs | 416.92 [317.58–485.92] | 286.58 [109.21–523.42] |
| Pooled p50 / p99, µs | 55.29 / 416.92 | 55.67 / 371.00 |

Small-call medians remain close; tails change direction between batches.
These samples do not establish a small-call tail regression or a quiet-host
nonregression guarantee.

For footprint, three independent rounds alternate order at each connection
count. Each connection makes a 32-byte warmup, a 900,000-byte call, then a
32-byte call, and remains cached. The probe waits 12 seconds, records native
macOS physical footprint, then observes another five seconds before
teardown, matching the survey's settling window. Values are MiB,
median [minimum–maximum], for the client probe including its in-process rig.

| Cached connections | Before | After |
| --- | ---: | ---: |
| 32 | 37.70 [37.08–39.64] | 12.59 [12.48–12.63] |
| 128 | 129.66 [129.64–131.22] | 14.91 [14.86–14.94] |

The median reduction is 25.11 MiB at 32 connections and 114.75 MiB at 128.
Absolute figures differ from the earlier survey on `d6bb6cc`; both sides
here run the same newer base and dependencies.

Regression coverage uses real child pipes. The completed-frame storage
test failed on the original implementation's retained capacity, then
passed with the fix. Tests also cover resuming a cancelled partial read
with its cursor intact, fragmented UTF-8 replies, adjacent notifications
and replies, and cancellation while a peer has emitted an unfinished
reply. Process cleanup remains exercised through the real extension rig.

Validation: all 32 MCP tests passed with the `fixture` feature; crate
check, formatting, clippy on all targets/features with `-D warnings`,
documentation generation, and workspace `check:windows` passed. No
speculative hardening or out-of-scope source edits were needed.
Raw scripts, release probes and JSONL observations for this round are
retained outside the worktree at `/private/tmp/mcp-frame-buffer-r3` for
independent review. Branch CI is deferred until that review.

## HTTP OAuth (round 3)

The client uses the existing pooled HTTP transport with redirects refused.
A 401 discovers protected-resource and authorization-server metadata, then uses
an authorization code with S256 PKCE and resource indicators. Stored credentials
bind the resource, issuer and client identity. A rejected access token can refresh;
`invalid_grant` starts a new authorization. A 403 `insufficient_scope` requests the
union of previous and challenged scopes. Only explicit authorization challenges
permit retrying the original operation; other transport failures remain terminal.

The default client ID is the single `OAUTH_CLIENT_ID` constant:
`https://arut.land/.well-known/oauth-client/a.json`. The owner will host this
metadata document; this change adds no hosted copy. The local fixture resolves
that URL to its own document without contacting the public host. A server needing
a pre-registered public client can set `<server>_client_id` and `<server>_issuer`
together. The issuer must appear in that resource's metadata and match the
validated authorization-server metadata. No need for dynamic registration was
demonstrated, so it is not implemented.

Authorization uses an ephemeral `127.0.0.1` listener through `rt`, with callback
path `/oauth/callback`. An internal tool presents the authorization URL using the
existing approval card. The headless daemon never opens a browser. The verifier,
callback code and tokens stay outside tool inputs, results and session logs.
Authorization state belongs to the credential store, independent of session replay.
For a daemon on another host, the browser must be able to reach that host's
loopback callback, for example through an SSH tunnel; automatic callback forwarding
would need client/transport work outside the owned paths.

`sys::credentials` uses Keychain on macOS and Credential Manager on Windows.
Native calls run on the existing storage workers. Windows stores multiple native
credential parts so its 2560-byte blob limit does not limit OAuth token length.
The native round-trip test covers 10,000-byte values, replacement and deletion.
It also reproduced macOS treating an empty data update as unchanged; a byte of
native framing permits an empty value to replace an existing value.
Integration tests use an isolated private-file store, never existing credentials.
Linux uses the explicitly permitted 0600 file under the instance's state directory,
inside a 0700 directory, through the existing private-file implementation.

### Dependency comparison

On the M3, Rust nightly-2026-10-17, `opt-level=z`, fat LTO, one codegen unit,
stripped, a standalone Secret Service reader using `secret-service` 5.2.0 with
`default-features=false, features=["rt-async-io-crypto-rust"]` linked **1,222,320
bytes**. Its normal dependency graph contained **96 distinct packages including
the probe** on macOS and **97** for Linux, with no crate named `tokio` in either. The same probe using `std::fs::read`
linked **286,368 bytes**. The **935,952-byte** difference is an isolated reader
comparison, not a predicted change to `a`; it does not include shared-dependency
elimination in `a`. It establishes the substantial D-Bus/crypto surface needed
for this otherwise small storage operation. This implementation chooses the
permitted private-file backend instead of another D-Bus client and executor.
Neither probe contacted a Secret Service daemon. These figures are native macOS
code-size observations, not Linux runtime performance measurements.

Header challenges use `http-auth` 0.1.10 with default features disabled; form
encoding uses `form_urlencoded` 1.2.2. They handle quoted challenge parameters,
multiple schemes and percent encoding rather than introducing custom parsers.
Only `http-auth` is a new package in the workspace lock resolution; its `memchr`
dependency, `form_urlencoded` and `percent-encoding` were already present.

There is no OAuth background task, connection, poller or refresh timer. Credentials
are loaded only after an authorization challenge, then shared in memory for that
registry. Public servers and successful authenticated calls perform no credential
I/O. Discovery, refresh and browser interaction happen only after a challenge. Payload clones share their existing
`Arc` pieces while authentication retries, rather than copying the request body.
The protected-server invariant failed on the original release client with
`expected a JSON-RPC 2.0 object`, before OAuth was implemented. Tests also caught
and removed an eager credential-read dependency: a public server now succeeds even
when the credential directory is unavailable. Warm protected calls reuse validated
credentials without storage I/O, and a new MCP instance restores them from storage.

### Release and idle observations

Both shipped builds passed `mise run size` and its fixture scan. Baseline product
source is `7318ef3f`, candidate product source is `8c4942e3`; the later main merge
through `9370a19c` changes tooling/scripts/docs only, with no product Rust,
manifest, lockfile, release-profile or Rust-toolchain changes. The same native
M3, nightly-2026-10-17, `CARGO_BUILD_JOBS=2`, and shipped release profile were used.

| Metric | Before | After |
| --- | ---: | ---: |
| Shipped bytes (exact) | 6,789,424 | 6,873,712 |
| Idle daemon RSS, KiB | 10,632 [10,576–10,704] | 10,560 [10,480–10,624] |
| Idle daemon CPU over 3 seconds, ms | 0 [0–0] | 0 [0–0] |
| Daemon threads | 7 [7–7] | 7 [7–7] |

Size increases **84,288 bytes (82.31 KiB, 1.24%)**. Size is deterministic, with
zero observed spread. RSS and CPU are medians [minimum–maximum] from six samples
per build. Order alternates AB/BA across six rounds. Every sample starts a fresh
isolated home and plain client with one configured loopback HTTP MCP server,
waits 12 seconds for blocking workers to retire, then observes CPU for three
seconds. Provider variables are removed and replaced with a loopback URL and
dummy credentials. The loopback server received zero requests. Each daemon was
stopped with its isolated `A_HOME` before deleting the home.

CPU accounting had only floating-point residuals below 10^-12 seconds; values
above are rounded to milliseconds. The overlapping RSS ranges do not establish a
memory improvement or an exact zero-byte cost. The observation establishes no
added idle CPU or threads in this workload, alongside the absence of OAuth-owned
background tasks. These shared-host macOS observations are not cross-platform
performance guarantees.

After main integration, touched-crate checks and all **305 tests** passed (two
existing tests skipped), as did formatting, all-target clippy with `-D warnings`,
documentation generation and workspace Windows lint. The normal all-target
dependency graph contains no crate named `tokio`. No wire or recorded-provider
fixture changed. Phase-1 verification used an external source mirror for the generated lockfile
resolution. The owner committed that resolution in `e814148d` before phase 2.


### Independent review fixes

Authorization question names now come from a typed counter in the SDK's parent
invocation context. Pagination and other HTTP requests in the same operation
share that counter; per-request authorization retries remain independent. The
counter is allocated only when authorization is presented, lives for the parent
operation, and adds no background work or idle activity.

The local HTTP fixture publishes its readiness URL with `sys::fs::write_private`,
whose atomic replacement exposes a complete file. Readers can no longer mistake
an empty file between creation and writing for readiness.

`paginated_authorization_names_are_unique_in_the_engine` starts the real engine
in an isolated process and home, sends typed client messages over its native IPC,
and runs one `mcp_list` against the real local HTTP fixture. Page one requires
initial authorization; page two requires a scope upgrade. The original code
reproduced the failure: only one authorization invocation was accepted and the
parent returned `authorization declined`. With the fix, two invocations complete,
both pages finish, and the granted scope is `read write`. Only the browser and
remote authorization server are fixtures. Existing workspace crates `engine` and
`ext-fixtures` are test-only dependencies; no production dependency was added.

On the M3, all 45 MCP tests passed. The engine pagination regression and the
previously flaky no-scope authorization test each passed 20 consecutive stress
iterations. Touched-crate check, all-target clippy with `-D warnings`, formatting,
and the workspace Windows lint passed. The owner-approved metadata URL remains
unchanged and still requires hosting by the owner. The owner subsequently authorized generated lockfile ownership; commit
`07db53a1` integrates the two existing-workspace test-dependency edges. No registry
package was added by that patch.

The post-review shipped build passed `mise run size` and its fixture scan at
**6,873,760 bytes**, 48 bytes above the phase-1 candidate and 84,336 bytes above
the original pre-OAuth baseline. These are exact size observations, not new idle
measurements; the interleaved phase-1 idle observations above remain the measured
RSS/CPU evidence.

The single phase-2 CI dispatch is run `40668274523` at `e04de3af`. Its Miri job
stopped on the credential file invariant because Miri cannot interpret native
`open` flags (`0x800`). The test is now explicitly ignored under Miri, like the
native credential-store invariant; ordinary native credential tests still pass.
This is a deterministic interpreter limitation, so no failed-job retry was made.
The follow-up is locally verified and is not covered by the already dispatched
SHA. Local runtime Miri invariants passed; the full local sys Miri suite stops
earlier at the existing, out-of-scope config test's unsupported macOS `acl_init`.


### Credential lifecycle coordination (owner-approved follow-up)

`Authorizations` owns one lifecycle for each exact configured server name,
resource URI, issuer and client ID. The daemon's existing MCP instance shares
that value across its sessions and connections. `Mcp::with_authorizations` lets
separately composed instances share the same value, providing the small typed
seam for the forthcoming daemon-wide scoping work. No process-global cache or
cross-home singleton was added.

Requests receive a numbered access-token generation; refresh credentials remain
inside the owner. Immutable access-token bytes are shared through `Arc<str>`.
This uses Serde's existing `rc` feature, with no new package. Credential load,
refresh, reauthorization, native persistence and publication form one owned
transaction. Concurrent callers join its result. A late 401 from an older
generation reuses the newer publication, including its shared failure, instead
of redeeming the old refresh token again. A transient refresh error retains the
last persisted refresh credential for a later operation to retry.

The token endpoint's optional `expires_in` is recorded as an absolute expiration
in the private credential record (see [RFC 6749, section 5.1](https://www.rfc-editor.org/rfc/rfc6749#section-5.1)).
An incoming request with an expired generation joins renewal before sending the
expired token. Missing expiry leaves rejection-driven renewal in place. There
is no expiry timer, idle connection or maintenance task. Idle RSS and CPU were
not remeasured for this follow-up; the earlier idle observations remain
historical evidence, not an exact zero-byte cost claim for the new owner.

The runtime owns the renewal future. Cancelling a caller drops that caller's
wait and presentation candidacy while other callers continue awaiting the
transaction. A reauthorization selects a live waiting caller's existing approval
presentation. Native storage completion precedes publishing a replacement:
storage writes are not abandoned at the network deadline, since accepted native
jobs can outlive a dropped future. The existing configured MCP timeout bounds
network work. No new pagination, scope, token-length or caller-count cap was added.

The real-engine regressions use sixteen sessions and connections, a local HTTP
fixture with rotating refresh tokens, and a delayed old-generation 401 after the
replacement can already have been published. The fixture checks that both access
and refresh replacements are on disk before accepting their first use. Separate
cases cover one shared authorization after revocation, a shared declined result,
expiry before HTTP, starter cancellation during refresh, transient failure and
retry, concurrent scope step-up, and sharing between separately composed MCP
instances. All 55 MCP tests passed in dev and release builds. All eight lifecycle
integration cases passed 20 stress iterations each. Clippy with `-D warnings`,
formatting, workspace Windows lint, documentation and the normal all-target tokio
ban check passed. Both private lifecycle invariants passed Miri.

The original release client reproduced sixteen refresh redemptions for one
rotating token. An initial measurement also exposed a losing request deleting a
replacement saved by another request. The fixture now returns a failed response
for that persistence violation rather than panicking, allowing complete samples.
Both compared binaries use the same final fixture. Six samples per build run in
alternating AB/BA rounds on the shared M3, with fresh isolated homes, the shipped
release profile, nightly-2026-10-17 and two Cargo build jobs. Before product code
is `07db53a1` with the regression harness added; the after build includes this
coordinator. Values below are medians [minimum–maximum].

| Whole regression command | Before | After |
| --- | ---: | ---: |
| Refresh redemptions | 16 [16–16] | 1 [1–1] |
| Wall time, ms | 1,038.05 [965.76–1,150.21] | 816.68 [767.67–1,150.28] |
| User + system CPU, ms | 100 [100–110] | 90 [80–100] |
| Native time maximum RSS, KiB | 11,664 [11,648–11,680] | 11,184 [11,120–11,296] |

The before regression fails; all sixteen after requests complete. These timings
include daemon startup, fixture delays and shutdown, and compare failed versus
successful execution. They do not establish an equivalent-success latency or
CPU/RSS improvement. Native `time -lp` reports command resource usage, not idle
daemon RSS or the sum of concurrent processes' RSS. CPU values have 10 ms
resolution. No samples were discarded from the final complete six-round replay.
Raw samples, initial/provisional runs, source hashes and the replay script are
retained outside the worktree beside `mcp-oauth-refresh.report.md`.

Pending approval cards still belong to an engine parent call. Interrupting the
chosen parent while its approval is pending can end that question and give every
waiter the shared authorization-declined result. Preserving that pending card
across its parent's interruption needs an SDK/engine authority lifetime seam
outside these owned paths (`Cx::invoke`, `ContextOrigin`, and invocation/cancellation
ownership). The coordinator can select another live caller before presenting,
and refresh work survives starter cancellation; the pending-card lifetime remains
an integration requirement for the owner/scoping lane. No SDK, engine, protocol,
client or UI changes were made here. The owner explicitly deferred interrupted
multipart Windows credential writes; that case remains unchanged. The approved
client metadata URL remains the owner's unhosted placeholder.

CI is deferred until independent review, as requested.

The shipped coordinator build passed `mise run size` and its fixture scan at
**6,907,008 bytes**, versus the previously measured pre-coordination OAuth build's
**6,873,760 bytes**: **+33,248 bytes (32.47 KiB, 0.484%)**. That baseline's product
code is unchanged through `07db53a1`; the intervening lockfile and Miri-only test
changes do not ship. This is the complete feature delta, including expiry,
coordination and shared access-token serialization, rather than an isolated cost
attribution to Serde's `rc` feature. These are exact native M3 size observations;
Linux and Windows execution of this follow-up awaits the post-review CI run.


### Scope step-up review fix

A `403 insufficient_scope` leaves the previous access and refresh credential
intact while an upgrade is presented. Successful authorization is persisted
before replacing that grant. Declining or failing an upgrade keeps the stored
bytes unchanged. New operations can still obtain the cached prior access token,
while callers awaiting the failed upgrade continue to share its failure. A
rejected `401` credential still follows the existing revocation path.

The real-engine invariants
`declining_scope_step_up_preserves_previously_granted_access` and
`failing_scope_step_up_preserves_previously_granted_access` establish a read
grant, reject the write upgrade, compare the stored credential bytes, perform a
read without another card, and then refresh the original grant without another
card. They failed on the original release client: the grant was removed and the
read needed one extra authorization. Both pass after the fix. The private
lifecycle invariant also checks that cached access remains available to new
operations without hiding the failed outcome from older waiters.

Six release samples per build, alternating AB/BA on the shared M3 against the same
fixture, observed unchanged stored grants in **0/6 before versus 6/6 after**.
The subsequent original-scope operation required **1 [1–1]** extra approval card
before versus **0 [0–0]** after (median [minimum–maximum]). Whole regression-command
wall times were **708.71 [703.90–1,048.30] ms** before and
**659.12 [653.54–730.52] ms** after. These include fixture delays, startup and
shutdown and compare failed with successful execution; they are diagnostic
observations, not an equivalent-success latency claim. The after build also
integrates main through `1af5daf3`; no CPU/RSS effect is attributed to this fix.
Raw samples and source/binary hashes are retained with `mcp-oauth-stepup.report.md`.

After main integration, all 58 MCP tests passed in dev and release profiles.
Both review regressions passed 20 stress iterations each. All three lifecycle invariants passed Miri; formatting, all-target clippy with
`-D warnings`, documentation, workspace Windows lint and the normal all-target
tokio ban check passed. The earlier native credential-file Miri annotation is
included in this branch and will be covered by the phase-2 CI dispatch.

The pending-card parent-lifetime seam remains tracked for `mcp-elicit` /
`mcp-instances`. The owner-approved metadata URL and interrupted multipart Windows
credential-write decisions remain unchanged. This review fix introduces no new
dependency, wire change or additional hardening.


## Scoped MCP instances and discovery reuse (round 3)

The daemon now uses one SDK `Scoped` registry across its participating sessions,
with exact private declaration/argv/cwd/environment identities. Session is the
default, including when experimental sharing is enabled. Workspace/user stdio
scopes require explicit `experimental_stdio_sharing = true`; HTTP rejects scope
settings and retains the existing pool. Shared launch inputs belong to the trusted
declaration, not the first caller. Unused declarations start no actor or process.

A process owns one frame writer/reader and a request map with globally unique IDs.
Admission waits for retained-data credit instead of rejecting demand. Cancellation
releases caller authority immediately. Shared exchanges send MCP cancellation and
retain only charged ingress/drain state to their original deadline; an undrained
exchange retires the instance. Session cancellation kills the tree. The SDK owns
single-flight startup, session demand, retirement and native shutdown cleanup.
No unsafe call is replayed after a crash or transport retirement.

Discovery retains one immutable validated full list while its server TTL remains
fresh. A cold eligible fetch is single-flight; known immediately stale lists stay
independent. Every page contributes its earliest expiry, visibility and actual
authorizing generation. The OAuth owner exposes an opaque owner/recipient/generation
identity without exposing token bytes, checks expiry before cache lookup, and tags
the response after challenge/retry. Changing authorization generation between pages
prevents aggregate caching. Notification invalidation and publication share one
lock. No tool/MRTR result is cached, and no refresh polling or subscription stream
was introduced.

### Native release comparison

Apple M3, 16 GiB, macOS 27.0.1 arm64, nightly-2026-10-17, ordinary installed std.
Baseline: `63ac410ac780fa13cb627ee4a902a788952253b4`. Candidate runtime source:
`985c83b06e090c0abf13e87b933e2f2abfdccf73`. Both are repository release-profile fixture builds, built with
`CARGO_BUILD_JOBS=2` through `mise exec --` and an external target directory.
The same preserved baseline MCP child binary (489,424 bytes, SHA-256
`79e50e2f37bd989e34af7f2778d0e53b210fc7116726326365b2c8dd5d74aced`) is
used on both sides, so new test-fixture behavior cannot change child memory.

The original finding was reproduced before implementation. This comparison uses
five alternating AB/BA rounds per flag state on the shared host. Each observation
starts a fresh isolated A_HOME, attaches 1, then 32, then 128 retained sessions in
one workspace, and runs first list, warm list, schema and call for each newly
added session. It preserves the survey's 12-second settling and 3-second idle
windows. No providers are configured or contacted. Each daemon stops through its
own A_HOME, and every cleanup record confirms no remaining fixture PID.

Values are median [min–max] across five observations. First-list latency is the
per-observation median for the newly added sessions (1/31/96), including durable
tool dispatch, spawn/probe where necessary, list and client rendering. It does
not isolate pipe setup or handshake latency. The 358.989 ms cold observation is
retained. Native footprint is the OS per-process charge; summed child footprint
and RSS do not measure unique physical machine RAM. Parent values also include
retained sessions and tool results.

| Stdio scope / flag | Sessions | Children before → after | Child footprint MiB, before → after | Parent footprint MiB, before → after |
| --- | ---: | ---: | --- | --- |
| session / off | 1 | 1 → 1 | 1.750 [1.750–1.766] → 1.766 [1.735–1.766] | 3.657 [3.563–3.672] → 3.563 [3.532–3.641] |
| session / off | 32 | 32 → 32 | 56.248 [56.139–56.467] → 56.170 [56.045–56.561] | 6.141 [6.079–6.219] → 6.579 [6.469–6.625] |
| session / off | 128 | 128 → 128 | 225.743 [224.149–226.134] → 225.274 [224.181–226.196] | 13.625 [13.547–13.672] → 14.985 [14.922–15.063] |
| workspace / on | 1 | 1 → 1 | 1.766 [1.750–1.782] → 1.750 [1.750–1.750] | 3.641 [3.547–3.688] → 3.657 [3.547–3.766] |
| workspace / on | 32 | 32 → 1 | 56.264 [56.123–56.389] → 1.813 [1.797–1.844] | 6.141 [6.079–6.235] → 5.704 [5.625–5.844] |
| workspace / on | 128 | 128 → 1 | 224.915 [224.337–226.102] → 1.813 [1.797–1.860] | 13.594 [13.422–13.672] → 11.563 [11.469–11.641] |

| Stdio scope / flag | Sessions | First list ms, before → after | Warm list ms, before → after |
| --- | ---: | --- | --- |
| session / off | 1 | 13.037 [12.589–358.989] → 11.630 [11.547–21.197] | 7.596 [5.730–9.134] → 6.728 [6.662–14.232] |
| session / off | 32 | 11.989 [11.905–17.930] → 11.028 [10.033–20.746] | 6.048 [5.996–8.771] → 6.943 [5.934–7.908] |
| session / off | 128 | 12.993 [9.994–26.469] → 12.965 [10.032–16.786] | 6.992 [5.966–10.550] → 6.019 [5.103–7.950] |
| workspace / on | 1 | 12.502 [11.343–27.320] → 11.638 [11.496–38.652] | 6.781 [5.429–7.624] → 6.776 [6.702–11.082] |
| workspace / on | 32 | 12.144 [10.057–12.923] → 6.067 [5.135–7.997] | 6.948 [6.018–7.869] → 5.988 [5.930–7.930] |
| workspace / on | 128 | 13.280 [10.968–17.022] → 6.114 [6.002–7.655] | 6.031 [5.958–8.205] → 6.018 [5.904–6.992] |

At 128 sessions, default-scope parent footprint rises by about 1.36 MiB; parent
RSS rises from 21.359 [20.188–21.391] to 22.812 [22.766–22.891] MiB. This is an
observed cost of the new ownership/routing state, not a default-scope memory win.
The opt-in workload removes 127 fixture processes. Its summed child footprint
falls from 224.915 [224.337–226.102] to 1.813 [1.797–1.860] MiB. The first-ever
shared cold list is still 11.638 [11.497–38.652] ms: sharing removes repeated
startup, not the cost of the first process. User scope across workspaces is
validated functionally; no separate user-scope performance distribution is claimed.

For the 96 newly added sessions at 128, native daemon CPU per first-list operation
is 2.997 [2.453–4.435] ms before and 1.415 [0.735–1.718] ms after with sharing;
default scope is 3.078 [2.126–5.960] and 2.945 [2.214–4.711] ms. These are batch
CPU deltas divided by operation count, including the surrounding daemon work.
All sixty three-second settled daemon CPU windows show zero counter increments.
This is no observed idle CPU at OS resolution, not a proof of zero wakeups or a
promise about arbitrary servers. Latency remains observational on this shared M3.

### HTTP request counts

Five further alternating AB/BA rounds use a recording loopback peer, fresh homes,
and sixteen cold calls followed by sixteen warm calls. Positive TTL is
`ttlMs=300000, cacheScope=private`, with anonymous authorization and dummy fixture
arguments; no authenticated performance gain is claimed. Counts are identical
in all five samples (therefore their min/max equal the listed value).

| TTL | Version | POSTs / 16 cold calls | POSTs / 16 warm calls | First call ms | Warm calls p50 ms |
| --- | --- | ---: | ---: | --- | --- |
| missing | before | 32 | 32 | 6.948 [4.543–7.904] | 6.974 [5.996–7.008] |
| missing | after | 32 | 32 | 6.915 [6.809–7.932] | 5.998 [5.578–6.982] |
| positive private | before | 32 | 32 | 6.931 [6.157–7.903] | 6.957 [6.011–6.993] |
| positive private | after | 17 | 16 | 6.879 [6.681–7.896] | 6.004 [5.961–6.038] |

Positive-TTL cold calls drop from 32 to 17 POSTs (one list plus sixteen calls);
warm calls drop from 32 to 16. No-TTL calls remain at two POSTs per call. Request
count establishes the removed list fetch/decode; these small loopback wall times
do not establish a universal latency improvement.

Fixture-enabled a grows from 7,387,728 to 7,454,176 bytes (+64.891 KiB). This is
probe size, not shipped fixture-free size. Linux/Windows native runtime/performance
and macOS Intel measurements are unmeasured in phase 1; the full Windows cross-target
workspace clippy check passed. No dependency, daemon wire, log format or corpus changed.

### Validation and retained evidence

The selected native suite passes 101 tests, including all MCP/OAuth tests and CLI
unit/MCP daemon tests; the twelve daemon tests also pass against the release
candidate. Invariants cover 128 concurrent sessions sharing one process/probe,
reordered responses and exact progress ownership, exact environment identity,
session isolation of stateful server globals, workspace/user boundaries, stable
shared launch inputs, cancellation/drain deadlines, one session hibernating while
a peer remains attached, last-release/shutdown descendant cleanup and no effect
replay. Positive/zero/missing/expired TTL, mixed page deadlines, list invalidation,
noncacheable input-required results and private-generation changes after refresh,
expiry and paginated step-up use real children and loopback peers. The existing
rotating-token lifecycle tests remain intact. The new scope invariant fails
against the preserved baseline executable and passes on the candidate.

Touched-crate checks, fmt, all-target/all-feature clippy with `-D warnings`, the
Windows workspace lint, rustdoc and the normal-dependency no-Tokio scan passed.
The scan wrote its output outside the worktree. Hosted CI awaits independent
review; phase 1 does not dispatch it.

Raw data and binaries' hashes are retained outside the repository under the task
scratchpad's `r3/mcp-instances/`: `release-samples.jsonl`, `release-summary.json`,
`http-release.jsonl`, `http-release-summary.json`, `candidate-source.json`, host
metadata, cleanup records and the `release-measure.py`, `http-release.py` and
`release-summary.py` runners. Builds use `perf2/tgt-mcp-instances`.


## Independent review fixes (round 3)

The cold-discovery finding is reproduced: eight overlapping consumers of a
changing no-TTL peer received one snapshot. Followers now inspect the completed
result's positive TTL, earliest page deadline, authorization generation and
invalidation revision before reuse. An ineligible result makes each follower
fetch independently. Known uncached lists fetch directly without another worker
or retained flight. Missing/zero TTL and an expired first page all have native
loopback invariants; each consumer receives its own changing result.

An invalid pagination cursor discards the partial list, duplicate-name set, byte
budget and freshness state, then restarts at page one. The existing 64-request
operation budget includes restarts. HTTP and stdio invariants invalidate one
cursor and complete a fresh traversal; perpetual invalidation reaches the
budget. Completed fresh traversals cache normally, without discarded tools.
The new cold-list and cursor invariants failed against the previous code.

### Memory attribution

A temporary test-only probe at actual key construction and actor startup, using
a real fixture child, recorded the following ARM64 inventory. These are inline
Rust sizes and owned string capacities, not a byte-exact heap census: allocator
nodes, task allocations and fragmentation cannot be summed from these values.

| Component in the reviewed implementation | Observed size / ownership | Change |
| --- | --- | --- |
| Launch identity | 104 inline bytes; seven environment entries own 3,800 payload bytes plus 336 bytes of string headers and B-tree storage; argv owns another 228 payload bytes | Retain the immutable client environment through its existing `Arc`; declaration settings imply argv and overrides. Build a launch spec only on startup. |
| Discovery state | 464 inline bytes, plus weak flight/result allocations retained after completion | Keep a small state and allocate entries on demand; completed uncached discovery retains no entry or flight. |
| Authorization slot | Even `None` occupies 128 bytes | An opaque shared generation identity makes the slot pointer-sized (8 bytes here), preserving recipient, credential-owner and generation equality. |
| Scoped registry / leases | One key and SDK demand entry per activated instance; a request lease wrapper is 16 bytes | Keep the required ownership and last-release semantics; no session-count cap. |
| Routing | State is 136 bytes; a pending record is 312 bytes, with zero pending records after requests settle; actor future is 1,432 bytes | Retain per-request ownership and globally unique IDs; no idle request table entries. |
| Reader scratch | 8,192 bytes per helper, also present in main | Start at 512 bytes, grow to the same 8,192-byte read chunk for active frames and release the larger chunk when the frame completes. Frame limits and large-frame support are unchanged. |

Startup context and declaration clones now happen inside the reserved startup
closure. Ready-instance lookups carry references instead of unused startup
contexts. The opaque-slot invariant fails on the previous implementation
(`128 != 8`) and passes on the real current type. Existing large/adjacent frame
and cancelled-fragment tests cover adaptive reader storage.

### Default-scope release comparison

Five alternating AB/BA pairs repeat the original default-scope workload, with
sharing disabled, at 1, 32 and 128 retained sessions. The baseline remains
`63ac410a`; both versions use the identical preserved baseline fixture binary.
Each newly added session lists, lists again, loads a schema and calls echo, then
settles for twelve seconds followed by a three-second idle window. Homes and
workspaces are isolated; provider environment is absent. All values below are
medians [minimum–maximum] over five runs on the shared M3. No samples are removed.

| Sessions | Version | Daemon footprint MiB | Daemon RSS MiB | First-list p50 ms |
| ---: | --- | --- | --- | --- |
| 1 | main | 3.594 [3.532–3.719] | 11.328 [11.266–11.438] | 11.178 [10.070–14.696] |
| 1 | review fixes | 3.657 [3.594–3.719] | 11.375 [11.312–11.438] | 10.257 [8.122–14.269] |
| 32 | main | 6.125 [6.110–6.172] | 13.859 [13.828–13.906] | 10.029 [9.979–14.029] |
| 32 | review fixes | 6.094 [6.047–6.157] | 13.812 [12.844–13.875] | 10.958 [9.921–13.870] |
| 128 | main | 13.610 [13.438–13.672] | 21.312 [18.688–21.406] | 13.000 [9.083–14.137] |
| 128 | review fixes | 13.266 [13.266–13.329] | 20.984 [20.078–21.047] | 11.065 [9.558–20.039] |

Every run retains exactly 1 / 32 / 128 helper processes, respectively. At 128,
daemon footprint is 0.344 MiB below the interleaved main median, instead of the
reviewed 1.360 MiB increase. At 32 it is 0.031 MiB below main. The one-session
full-sweep median is still 0.063 MiB above main, with overlapping ranges; this
sweep alone does not establish the requested no-increase condition there.

Five additional alternating AB/BA pairs repeat the same one-session stage and
collect `vmmap -summary` and `heap -s --showSizes --noContent` only after the
settled/idle counters. Footprint is 3.704 [3.641–3.766] MiB before and 3.672
[3.563–3.719] after; live malloc bytes are 408,208 [406,928–409,712] before and
407,184 [405,904–407,408] after. The diagnostic repeat reverses the footprint
median difference and has slightly lower live heap. Native maps show variation
in dirty small-allocation regions, allocator metadata and cached stacks, while
settled live thread counts remain seven in both versions. These observations
qualify the small-run result; they do not prove a deterministic per-run footprint
bound. No allocator-specific mitigation is added.

At 128, daemon CPU per first-list operation is 3.363 [2.047–3.802] ms before and
2.575 [1.953–3.732] after, using batch deltas over the 96 new sessions. All thirty
three-second idle CPU windows have zero increments at OS resolution. Latency
ranges overlap and the M3 is shared; no universal latency or CPU gain is claimed.

The comparison binary corresponds to `bdbf0c52` after merging main `e0202822`;
the merge contributes unrelated engine shutdown/test fixes. The original main
baseline and fixture hashes remain pinned. Earlier intermediate comparisons are
retained rather than discarded: `phase2/` precedes the compact authorization
identity and `phase2-final/` precedes deferring startup-context clones. The final
full-sweep samples/summary are `phase2-lazy/release-{samples.jsonl,summary.json}`;
the diagnostic repeat and native maps are in `phase2-profile/`. Binary/source
hashes, validation logs and the reproducing scripts remain beside them in the
external task scratchpad. All forty new benchmark-home cleanup records report
successful daemon stop and no remaining recorded helper PIDs.

The selected suite passes 105 native tests, including twelve MCP daemon tests
against the release comparison binary and every OAuth lifecycle test. Touched
crate checks, fmt, all-target/all-feature clippy with `-D warnings`, the full
Windows cross-target lint, rustdoc and the normal-dependency no-Tokio scan pass.
No dependency, wire, corpus, credential lifecycle or feature limit changes.

### Launch policy admission and OAuth fixture ownership

After the owner extended SDK ownership to `crates/ext/src/tool.rs`,
`Cx::has_handlers<P>()` exposes presence in the active registry without executing
handlers or exposing the registry. MCP admits launch policy before constructing
an identity or looking up a ready instance. Broader scope admits only the empty
spawn chain: the current Spawn handlers require session authority. A private
policy value binds the declaration and session/declaration policy class into the
identity. Session launches continue through `Processes::spawn`, including refusal
and command rewriting. HTTP transport ownership is unchanged.

Real-child tests cover refusing and rewriting hooks at session/workspace/user
scope, actual rewritten argv/environment, reaping the wrapped child, and a hooked
session trying to join an already running unhooked shared instance. Both policy
invariants fail when the admission guard is disabled and pass with it enabled.

The hosted Windows OAuth failure is reproduced in
[40709033654](https://github.com/arut-land/a/actions/runs/40709033654): the child
reports `AddrInUse` / another daemon running, followed by the parent's bind
timeout. The Windows discovery implementation briefly tries the daemon-record
lock; probing before the fixture's child claims that record can win the lock and
make startup fail. Each fixture already has a unique isolated home. The child now
owns its listener and acknowledges readiness from extension installation, after
the engine's exclusive record claim and publication. The parent blocks on that
acknowledgement and drains child output, then performs one endpoint connection.
There is no connection retry or sleep. The child also receives explicit isolated
`A_HOME`. A native invariant checks that acknowledgement follows exclusive record
ownership; it fails against the previous fixture with a missing record and passes
with the handshake. OAuth single-flight/rotation lifecycle tests remain intact.

`hibernate_secs=0` remains on the coordinator's follow-up list, unchanged here.
No speculative hardening is added.

### Final matched-main comparison after policy admission

Main advanced to `f3a51b4e` with native frame buffers and the charged-memory
budget work. It is merged in `39013482`. The final comparison rebuilds that exact
main and the candidate, with `fixtures`, jobs=2 and the same preserved fixture
binary. Both programs therefore include the same engine/protocol/frame changes;
these absolute values must not be compared to the earlier `63ac410a` table to
attribute MCP savings. Five alternating AB/BA pairs repeat the identical default
scope workload, isolated homes, sanitized provider environment and 12+3-second
settle/idle windows. Values are median [minimum–maximum].

| Sessions | Version | Daemon footprint MiB | Daemon RSS MiB | First-list p50 ms |
| ---: | --- | --- | --- | --- |
| 1 | matched main | 3.657 [3.610–3.688] | 11.359 [11.312–11.391] | 12.613 [10.273–29.183] |
| 1 | final candidate | 3.688 [3.594–3.782] | 11.484 [11.391–11.562] | 10.312 [10.275–27.744] |
| 32 | matched main | 7.938 [7.891–8.000] | 15.641 [15.594–15.703] | 10.961 [9.996–14.747] |
| 32 | final candidate | 7.969 [7.907–8.047] | 15.766 [15.703–15.828] | 10.704 [10.096–12.988] |
| 128 | matched main | 20.891 [20.860–20.907] | 28.547 [19.453–28.594] | 12.101 [10.028–18.862] |
| 128 | final candidate | 20.750 [20.610–20.829] | 28.531 [28.391–28.594] | 9.994 [9.959–15.952] |

At 128 sessions, footprint is 0.141 MiB below matched main, with non-overlapping
sample ranges. The 1- and 32-session medians remain 0.031 MiB higher, with
overlapping ranges; these rows do not establish the literal no-increase goal.
No samples are dropped or allocator-specific mitigation added. Every run retains
exactly 1/32/128 helpers. All thirty idle CPU windows have zero increments at OS
resolution. First-list CPU at 128 is 3.114 [2.027–4.884] ms per operation before
and 2.128 [1.899–3.605] after, over the batch of 96 new sessions. Latency and CPU
ranges overlap on this shared M3; no universal gain is claimed.

Release acceptance checks caught Cargo's shared primary-output alias when the
external main build left its binary at `release/a` and the candidate build was
considered fresh. No measurements used that output. Candidate SDK/MCP/CLI release
artifacts were rebuilt, distinct executable hashes checked, and the release CLI
suite passed before sampling. `approved-source.json` pins both executable hashes,
main/candidate revisions and fixture hash; `approved/release-samples.jsonl` and
`approved/release-summary.json` retain every sample. The native validation log is
`approved-final-validation.log`: 194 selected tests pass, including SDK invariants,
all MCP/OAuth tests and twelve release CLI tests. Check, fmt, all-target/all-feature
clippy with `-D warnings`, Windows workspace lint, rustdoc and the normal no-Tokio
scan pass. The new identity invariant also fails if both admission classes are
made identical. All ten final benchmark daemon stops succeeded. Candidate cleanup
snapshots contain no remaining helper PIDs. One baseline cleanup snapshot listed
a PID that was absent at the final audit; the original record and follow-up audit
are retained. No MCP dependency, wire or corpus changes are introduced.

### Hosted validation and process-identity test correction

[CI 40732366946](https://github.com/arut-land/a/actions/runs/40732366946) passed
Linux check, budgets, Miri, fuzz, thread and dependency jobs. Windows x64 ran
1,991 tests: 1,990 passed, including the OAuth engine-readiness, pagination,
rotation and other lifecycle invariants. The sole failure was a new policy test's
use of `RecordedIdentity::alive()` on a PID: that API diagnoses process groups
and deliberately returns false on Windows. This was a deterministic test error,
not a flake. The corrected invariant reads the helper's native process-start
identity before teardown and verifies that same identity is gone afterwards,
without confusing a PID with a group or a subsequently reused PID. Production
code and the measured release executables are unchanged. No failed-job rerun is
used for this error; a replacement CI dispatch requires an owner exception to
the one-dispatch instruction.


### Caller-owned authorization waits and typed startup fan-out

The focused review reproduced two failures on `410239d5`: generation lookup
waited outside the caller's timeout/cancellation guard, and `Scoped` reconstructed
startup I/O errors from `(ErrorKind, String)`, making the MCP downcast ineffective.
The release regressions fail before the fix: a discovery crash is reported as
`Spawn("Crashed")`, and all six short/cancelled generation waiters remain blocked
until the fixture releases another caller's renewal.

Generation lookup now uses the same timeout/cancellation guard as HTTP exchange
and instance acquisition. Dropping this wait releases its OAuth participation;
the existing credential owner continues renewal for remaining callers. No OAuth
credential-owner implementation changes are needed. MCP carries startup's typed
`Result<Instance, Failure>` as the scoped resource value, retaining failed outcomes
only through joined demand. Failed discovery cleans up its child before publishing
the outcome. A pipe actor has its own cancellation signal so handshake cleanup
cannot cancel the scope before fan-out. Only successful outcomes can create a
session-retained MCP lease; a subsequent explicit request can retry after cleanup.
The SDK registry and its lifetime implementation are unchanged.

The real-child invariant polls eight list callers into one gated discovery, then
checks one launch/probe, typed crash/unsupported-version outcomes for every caller,
and an explicit later retry. The OAuth invariant uses one shared credential owner,
a 10-second renewal caller, and list/schema/call waiters with 100 ms timeouts or
cancellation after their first poll. Its loopback fixture holds renewal until all
short waiters finish or the one-second regression observation expires, always
releases it and joins the owned callers, and checks exactly one refresh and a
successful long caller. A baseline debug negative run left one fixture process;
its exact command and PID were verified and terminated, recorded in
`rereview-negative-cleanup.json`. Subsequent negative/positive samples explicitly
close the fixture before the failing assertions.

Five alternating AB/BA pairs run preserved release test executables on the shared
M3, with provider variables absent and isolated `A_HOME`/credential stores. Both
versions use the same real fixture and workload. These measure waiter release in
a deliberately delayed renewal, not remote-server throughput. Some local checks
ran concurrently; no quiet-host qualification is claimed. Milliseconds are median
[minimum–maximum] across the five samples:

| Waiter | Before | After |
| --- | ---: | ---: |
| timed list | 1019.466 [1016.608–1049.861] | 102.131 [100.196–103.770] |
| timed schema | 1019.597 [1016.435–1049.505] | 101.138 [100.748–103.252] |
| timed call | 1019.466 [1017.132–1050.996] | 101.770 [100.384–103.586] |
| cancelled list | 1017.404 [1006.772–1036.057] | 0.023 [0.021–0.084] |
| cancelled schema | 1017.132 [1006.832–1035.701] | 0.025 [0.021–0.112] |
| cancelled call | 1017.059 [1006.979–1035.631] | 0.020 [0.013–0.237] |

Every baseline sample fails the prompt-return invariant; every candidate sample
passes it and preserves the shared renewal. `rereview-samples.jsonl`,
`rereview-summary.json`, per-sample logs and executable hash manifests retain the
complete evidence under the external MCP scratch directory. The fix is
`444d7316`; `6546616f` merges main `1e60834a` before final validation. Debug and
release suites each pass all 73 MCP tests, including every OAuth lifecycle test.
After the fixture dispatcher and cancellation helper refactor, all 73 debug tests
and both focused release regressions pass again. Check, fmt, all-target/all-feature
clippy with `-D warnings`, workspace Windows lint and rustdoc pass.
No dependencies, wire formats, corpus, feature limits or speculative hardening
change. `hibernate_secs=0` remains the coordinator's follow-up.

## Durable elicitation and question publication (2026-10-29)

MCP form and URL elicitation use one typed question/answer path shared with
policy gates and service-owned OAuth cards. A parked tool keeps its schema
and continuation checkpoint in the log, without retaining its request future
or an exchange timer. No dependency was added. `PROTO` is 38; the wire corpus
and reducer vectors were generated by their tests.

The original finding was verified at `f343ff37`: `input_required` became
`Failure::InputRequired`. It cannot complete an elicitation workload, so its
failure latency is not a successful-operation baseline. The initial typed
implementation is `bd101801`.

Release measurements on the shared Apple M3, macOS 27.0.1 arm64, used Rust
1.101.0-nightly (2026-10-16), the unchanged `build-release` profile (`z`, fat
LTO, one codegen unit, size-optimized rebuilt std), one external target and
`CARGO_BUILD_JOBS=2` under `mise exec --`. Provider variables were removed.
All binaries were built before sampling; no own build ran during a sample.

Idle compares shipped, fixture-free binaries at `f343ff37` and the final
implementation. Ten pairs alternate AB/BA. Each sample starts a daemon in a
fresh isolated home, settles for two seconds, then reads RSS and cumulative
`ps` CPU across three seconds, followed by that home's `a daemon stop`.

| Idle metric | Before: median [min–max], IQR | After: median [min–max], IQR |
| --- | --- | --- |
| RSS, KiB | 9,800 [9,728–9,872], 32 | 9,864 [9,824–9,984], 56 |
| CPU seconds per window | 0.00 [0.00–0.00], 0 | 0.00 [0.00–0.00], 0 |

The RSS median rises by 64 KiB (0.65%), within overlapping ranges. These
short CPU windows have `ps`'s centisecond resolution; they show no detected
idle CPU increase, not a universal zero-CPU guarantee. Shipped binary bytes
are **7,023,504 → 7,206,176** (+182,672 bytes / 178.39 KiB / 2.60%).

Latency uses real daemons, files and IPC, the echo agent, and a deterministic
stdio MCP peer. Ten batch pairs alternate AB/BA, with 32 cycles per batch and
the first two discarded: 300 retained samples per variant. Confirmation is
answer send to receiving its durable Answer entry; the real `bash true`
completes outside that timer. The elicitation flow starts with tool input,
answers a five-field form (string, integer, boolean, single and multiple
choice), answers a URL question, and ends at the final result. Automatic
test answers exclude human time and browser navigation.

| Workload, milliseconds | Before: median [min–max], IQR | After: median [min–max], IQR |
| --- | --- | --- |
| Confirmation acknowledgement, `f343ff37` → final | 3.081 [1.472–24.686], 1.970 | 3.940 [1.432–17.667], 4.992 |
| Form + URL, `bd101801` → final | 109.994 [86.975–683.272], 89.260 | 22.044 [13.175–310.126], 53.466 |

Confirmation acknowledgement rises 0.859 ms (27.88%) in this run, with wide
overlapping ranges on the shared host; this does not establish unchanged
confirmation latency. The form/URL median falls **79.96%**. The measured delay
came from publication: parked questions had no admitted external work and did not request immediate
publication while the turn was active, so each waited for the existing 50 ms
sync timer. The kernel now requests publication while its user-question inbox
is nonempty. The engine performs the existing durable flush before exposing
those facts. This also serves policy questions, without another timer or
configuration option. Merely changing the commit's durability classification
did not improve the measured workload and was discarded.

`user_questions_request_durable_publication_under_either_durability_setting`
fails on the initial implementation and passes with the publication change.
The ignored black-box benchmarks are `mcp::confirmation_round_trip_latency`
and `mcp::question_round_trip_latency`; build the CLI and MCP fixture with
`build-release`, then run the matching release black-box executable alone.
The archived baseline uses the identical confirmation harness with its old
answer constructor. The pre-publication comparison uses the same form/URL
harness on both implementations. No outliers were removed. IQR uses inclusive
quartiles. These results cover the recorded small form, not large-schema or
many-concurrent-question costs.

[Machine, profile, binary hashes and statistics](mcp-elicitation.json) and
[all raw samples in order](mcp-elicitation.samples.jsonl) identify the exact
binaries and retain the spread needed to assess shared-host noise.
