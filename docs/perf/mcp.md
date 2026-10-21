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

## Final validation

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
