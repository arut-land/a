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
