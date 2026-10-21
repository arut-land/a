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

Streamable HTTP reuses `llm::Http` / `llm::Ureq`, including backpressure
and socket shutdown when dropped. There is no second reactor, HTTP
implementation or TLS configuration. Base64 uses maintained `base64`
0.23, already present through ureq, rather than a custom encoder.

## Release size

`CARGO_BUILD_JOBS=3 mise run size` before changes: **2975 KiB**
(3,046,432 bytes). After implementation and merging main: **3072 KiB**
(3,145,856 bytes), an increase of 99,424 bytes (97.09 KiB), below the
4096 KiB cap. The difference includes main's intervening durability and
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
`llm::Ureq` construction. MCP chooses `Refuse`: even same-origin redirects
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
bytes), below 4096 KiB. The previous validated release was 3072 KiB
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

The final release is **3300 KiB** (3,380,192 bytes), below 4096 KiB. The
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
