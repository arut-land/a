# Design

`a` is an agent engine: one per-user daemon owns durable sessions; clients
attach, detach and replay them. This is the canonical as-built design.
The crate module docs describe the contracts; the area docs below describe
their implementation. Planned work is listed separately under milestones.

## Goals and boundaries

- Sessions outlive clients. Several clients can follow the same session.
- A small core stores facts once, derives views, and commits intent before
  external work. Recovery follows the same fold as a live session.
- Features compose through the extension SDK. The kernel contains no
  tool, prompt, permission policy, model choice or retry policy.
- First paint, daemon startup, attach, memory and binary size have measured
  budgets. An idle process waits for events rather than polling.
- Platform details belong in `sys` and `rt`; library work is async over
  `rt::Runtime`. Nothing `a` builds depends on the crate named `tokio`.

There is always a daemon; there is no in-process fallback. Distributed
writers, migration and additional transports remain future work. This is
pre-release: formats change outright, without compatibility shims.

## Process model

The CLI stages its executable into the home runtime directory. A local
client first connects to the daemon's endpoint. On a cold start, clients
serialize through `start.lock`, recheck the connection, take the daemon
lock, then start a detached `a __daemon`. Unix passes the bound listener
and lock; the client connects into its backlog. Windows starts the child
without inherited transport handles and waits for its binding acknowledgement
under the start lock. Neither path polls for readiness. The daemon
holds its pid-file lock for its lifetime, excluding other writers to that
home. `A_HOME` overrides the default `~/.a`.

The daemon serves connections and session actors on `rt`. Idle sessions
hibernate; the daemon exits when no clients or work pin it. Upgrades drain
work and restart the staged build; clients reconnect from their last seq.
A typed crash breaker distinguishes repeated young deaths from a reboot.
Diagnosis and restart verify the recorded process identity before signaling.

An attach supplies the client's environment and working directory. Tools
use the session's environment rather than the shell that started the daemon.
The log records its redacted form; live secret values stay out of that log.
Environment refresh is an explicit client command. Remote clients use
system SSH and `a bridge`; missing installations may receive this build
only when `sys::Platform` recognizes the host as matching it.

See [the client SDK](client.md) and [CLI](cli.md), and the staging and
endpoint types in `crates/sys`. The daemon targets macOS, Linux and Windows.
Windows uses SID-restricted named pipes, overlapped completion events over
`rt`, atomic staging references and kill-on-close shell jobs. Its native CI
lane must establish runtime behavior; cross-lint alone is insufficient.
See [Windows platform details and validation limits](perf/windows.md).

## Session spine

### Log and store

The source of truth is `sessions/<id>.jsonl`: a header followed by numbered,
epoch-stamped entries. Every line is sealed with CRC32C. On a full scan, an
unsealed last line is a torn tail and is trimmed. Damage before the tail,
or a different log format, preserves the file and opens it read-only.
Sealed unknown entry kinds survive as `Body::Unknown`.

The engine's `LogStore` contract has a conformance suite for JSONL and the
test memory backend. A writer is moved into one session actor, never shared.
Local leases issue epochs above the loaded log's epoch; the daemon lock and
writer ownership enforce exclusion. This is not a cross-host fencing service.
The derived `sessions.idx` catalog lists sessions without loading their logs.

Calls have durable intents and outcomes. Tool output is structured data;
model transcripts and client presentation are projections. Approval waits,
queued input, decisions, child links, process ownership and extension state
are facts in the log. Streaming deltas are ephemeral until an outcome or
progress entry records them. Essential facts that projections cannot rebuild
still need an explicit logging audit (see milestones).

### Kernel and driver

`kernel::Core::step(Event) -> Effects` is synchronous, does no I/O and reads
no clock. The driver supplies outcomes and time as events. The kernel
numbers and folds appended entries into the ledger using the same fold as
load. A model request is prepared through extension points, the decision
is committed, and only then is the request sent.

One async engine actor per loaded session carries out those effects and
feeds results back. Each running call has its own in-flight state. Tools
declare shared or exclusive concurrency; `bash` remains exclusive. Blocking
store and provider operations run through `rt`'s blocking pool. Followers
share encoded entries and a bounded tail ring rather than separately
encoding every entry for every client.

### Durability

The kernel emits `Commit(Barrier | Lazy)` after a step's appends and typed
`Admission` requirements before work. `[daemon] durability` selects `step`
(default: barriers for decisions and outcomes) or `effects` (barriers before
external effects and at turn end). Notes alone may be lazy. The driver
batches mailbox bursts into one commit.

On Apple platforms each commit uses a write barrier, then a daemon-wide
flush epoch performs `F_FULLFSYNC` at promise points. Replay-safe work may
use the ordered prefix while publication, acknowledgements and unsafe work
wait for the durable prefix. Other platforms retain their existing sync
path. Fault-injecting store tests cover power cuts between ordering and
flushing, failed epochs, recovery and replay-safe admission. The design and
measurements are recorded in [session measurements](perf/sessions.md).

Kernel activity is certified after a complete burst. Activity at N requires
both the ledger and visible frames to end at N, with `durable_seq >= N`;
`send_after(N, ...)` orders it after its entries. Live publication, snapshot
and suffix attach, recovery and the final drain share that condition.
Clients end turns on this certification rather than an entry prefix.

### Snapshots and recovery

At rest, fully synced, after enough additional log data, a root session writes
the ledger and extension fold images to `sessions/derived/<id>.snap`.
A neighboring `.idx` records line offsets. A snapshot must match the build
and extension set, and its covered entry must agree with the log. Otherwise
load folds the whole log. The indexed prefix avoids a full checksum scan;
index-assisted reads are not an integrity audit of that prefix. `derived/`
is disposable. See [session measurements](perf/sessions.md).

`Core::resume` handles recovery after load, failed commit, registry reload
or contained panic. A fresh agent per turn makes its state reconstructible
from the log. Safe tools can rerun; unsafe tools settle as interrupted rather
than repeating an uncertain effect. Logged process groups are interpreted
against the boot identity to avoid killing reused process ids. Durable waits
and child ownership let interaction continue independently of a connection.

## Extensions and SDK

`ext::Registrar` registers tools, decision-point handlers, services and
their needs, durable folds, model text, and agents. Registration tiers and
replacement by name allow overrides; the operator's policy floor is guarded.
The engine itself offers the model no tools. Extensions communicate through
the log (`observe`), typed state, services and points.

`Built<L>` checks typed Rust needs and provides at compile time;
`Configured` lets each extension read its own settings table. Dynamic
contributions are validated by the host. A tool's Rust return type determines
whether its call is immediate, async or streaming; schemas follow its input
and output types. Replay and concurrency are explicit tool contracts.

The sealed point declarations generate their hook slots together. Merge
rules and fail-open/fail-closed behavior are centralized. Points cover input,
model preparation and recovery, tool gating and results, process spawning,
observation and lifecycle. Pure loop points also shape agent actions during
replay; they receive no live session context. Agents return actions and
perform no I/O. See `crates/ext/src/{point,shape,assemble,host}.rs`.

The linked extensions own:

| Crate | Responsibility |
| --- | --- |
| `ext-shell`, `ext-edit` | Streaming `bash`, spill files and `edit` |
| `ext-mcp` | Lazy external tools over stdio and Streamable HTTP |
| `ext-prompt` | System prompt |
| `ext-policy` | Permission modes, judge and guarded operator floor |
| `ext-settings` | Model choice fold and model-spec service |
| `ext-skills` | Skill index and instructions loaded on demand from workspace generations |
| `ext-retry`, `ext-compact` | Retryable failures; overflow recovery and transcript edits |
| `ext-subagent` | `task` over owned child sessions |
| `ext-title` | Titles from a side model call |
| `ext-lua` | Optional runtime mods through the erased SDK |
| `ext-fixtures` | Test-only echo agent and sleep tool |

The default loop lives in the SDK and registers through the same machinery.
`apps/cli/src/exts.rs` assembles the linked extensions. `Models` supports
[durable side requests](side-calls.md) with role-based routing, explicit
awaited or background execution, committed results, and bounded retries;
title, compact and the policy judge use it. Compaction edits the model's
transcript without erasing history. Optional [Lua mods](ext-lua.md) already support tools,
wire points, folds, text and agents in a bounded sandbox. They do not expose
every Rust service or decision point. [Skills](ext-skills.md) publish a small
index per adopted generation and load instructions through a typed tool or
user command. Both index and instructions are ordinary logged context; reload
adoption waits for rest.

[MCP tools](ext-mcp.md) register through the same SDK and policy path.
The model initially sees a compact server index via `Text::generation`;
listing tools and loading a selected schema are explicit calls. Schema
loading uses the same typed `Cx::publish` context primitive as skills,
including edit-aware deduplication. Stdio servers start on first use,
one per server per session, with their process groups owned by the daemon.
All MCP operations are external, unsafe to replay, and admitted only after
their intent is durable; progress and typed results become logged facts.
Streamable HTTP reuses the existing model HTTP transport and runtime with
an explicit refusal of redirects. Per-request notification and publication
budgets coalesce progress into a bounded latest value. Discovery has an
aggregate byte budget; compact summaries and typed names keep the result
and its card safely below the protocol frame limit.
The client targets MCP 2026-07-28 without a legacy handshake fallback;
HTTP authentication, sampling and elicitation remain unsupported.


## Runtime, wire and clients

`rt::Runtime` supplies spawning, blocking work, time and a reactor. The
default `Threads` implementation uses async-executor and async-io plus a
blocking pool. Alternative adapters are feature-gated; the optional Tokio
adapter is not linked by `a`. Platform calls and production unsafe code
belong in `sys`; `rt` owns readiness integration.

`json::Json` holds shared raw JSON bytes. Arguments, output, model responses
and extension state pass through layers without decoding by non-owners.
Large record decoders live in their owning crates to avoid repeated generic
instantiations. [The provider layer](llm.md) implements Responses, Chat,
Messages and Gemini with recorded-cassette parity; it runs HTTP via `rt`'s
blocking pool and leaves retry policy to extensions.

The daemon wire is length-prefixed JSON over the local endpoint or bridge
stdio; `protocol::PROTO` defines the current version. Typed ids and message classes distinguish durable entries,
ephemeral traffic and control. Hello negotiates a supported version; attach
replays from a seq or supplies a view snapshot, ends replay with `Synced`,
then follows live entries. A wire encoding change requires a new version
and a generated corpus; there is no older-format fallback.

`protocol::view` is the shared reducer; `Item::present()` supplies common
presentation. Rust, FFI and Swift use shared view vectors. `client` owns the
link/reconnect state machine, mutation numbering, offers, slash commands
and renderer pump. Its transport driver still uses connection/read threads
and a shared writer. Apps retain screen state and render SDK effects:
[CLI](cli.md), [TUI](tui.md), [web](web.md), and SwiftUI through BoltFFI.
Moving the client driver onto the reactor remains deferred.

## Crate map

| Layer | Crates and apps |
| --- | --- |
| Platform and execution | `sys`, `rt` |
| Data and wire | `json`, `protocol`, `llm` |
| Extension contract and implementations | `ext`, `ext-derive`, `ext-*` |
| Session decisions and execution | `kernel`, `engine` (including log store and catalog) |
| Client contract and foreign facade | `client`, `ffi` |
| Entry points and renderers | `apps/cli`, `apps/tui`, `apps/web`, `apps/apple` (Xcode, not Cargo) |

## What is measured

Measurements carry their workload, build, host and date in `docs/perf`.
Do not treat a historical result as a current budget pass.

- [Build tuning](perf/build-tuning.md): size, startup, executor/channel
  choices, provider overhead, fan-out, idle memory and rejected alternatives.
- [Sessions](perf/sessions.md): whole-log versus snapshot load, retained
  memory and commit costs for the two durability settings.
- [Lua](perf/lua.md): runtime choice, optional-build cost and sandbox budgets.
- [Skills](perf/skills.md): frontmatter parser comparison and shipped size.

`mise run size` checks shipped bytes and fixture exclusion. `budgets` checks
cold first reply, attach latency, idle RSS and quiet shell execution on an
isolated release build. `startup` compares process launch against its floor.
Timing runs require a quiet machine. CI also runs fmt, clippy, nextest,
dependency checks, fuzzing, Miri and the Apple facade/app lane. Windows has
a native build/lint lane with required workspace and daemon acceptance
tests, plus cross-target linting. Windows runtime validation depends on that
native lane; it cannot run on the Apple development host.

## Milestones

**Done:** durable CLI sessions on macOS/Linux; connect-or-spawn, staging,
drain/reconnect, CRC logs, snapshots, recovery, typed extensions and policy;
recorded provider codecs; TUI, web and SwiftUI renderers over the client SDK;
lazy MCP tools; optional Lua mods; lazy skill instructions; child sessions; side model calls
for title, compact and judge with purpose/outcome logging; conversation-only
forks from `(session, seq)` with independent copied logs, recorded ancestry,
exact extension generations and rest-point validation ([fork semantics](fork.md)).
These supersede the original M1/M2/M3 schedule: web and remote SSH are already present.

**Next:**

- Approval explanations that retain and present why a call needs consent.
- Audit essential non-derivable facts: exact sent-request identity, runtime
  decisions and recovery inputs. Side-call logging exists; it is not proof
  that every externally relevant fact is recorded.
- Establish native Windows CI results and measure its daemon budgets on
  a quiet Windows host; validate storage behavior across power loss.

**Later:** cross-host lease authority and migration, hot handoff,
checkpoints, additional codecs/transports, and deterministic fault simulation
for the distributed design. Each needs its own contract and measurements.

## Open questions

- What exact facts must a model call retain for reproducible requests across
  configuration, extension and provider changes? A derived transcript alone
  does not establish exact sent bytes.
- The coordinator selected conversation-only forks; owner confirmation remains
  open. History is copied independently, while files stay shared. Should future
  forks accept unfinished prefixes by recording interruptions?
- How much can shared flush epochs amortize the stronger Apple power-loss
  guarantee under realistic concurrent workloads on a quiet machine?
- When should indexed replay verify all skipped lines? Catalog rebuilds and
  failed-commit recovery still have whole-log paths worth measuring.
- What are daemon RSS and cancellation behavior under real provider/tool
  load, beyond idle and synthetic workloads?
- Which runtime-extension capabilities and isolation should MCP and future
  out-of-process extensions share with Lua?
