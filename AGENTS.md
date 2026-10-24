# Working in this repository

`a` is an agent engine: a per-user daemon that owns durable sessions, and
clients that attach to them. Pre-release; one binary, `a`. This file is
what a contributor or an agent needs before changing anything; the design
is in [docs/design.md](docs/design.md), with per-area docs (`docs/*.md`)
and each crate's `//!` comment supplying the details.

## Layout

| Path | What |
| --- | --- |
| `docs/design.md` | Canonical as-built design, milestones and open questions |
| `apps/cli` | The binary `a`: argv (`args.rs`, usage-rs derive is the spec), the daemon's extension list (`exts.rs`), the line REPL, one turn in a pipeline (`once.rs`, docs/cli.md), `a daemon …`, `a bridge`; the black-box tests in `tests/` |
| `apps/tui` | The terminal client (docs/tui.md) |
| `apps/web` | `a web`, the browser UI (docs/web.md) |
| `apps/apple` | The macOS app (SwiftUI over `crates/ffi`; not a cargo crate) |
| `crates/sys` | Platform specifics; the only crate allowed `unsafe` |
| `crates/rt` | The async runtime seam: library code is plain async over `Runtime`, never names tokio |
| `crates/json`, `crates/transport` | Payloads carried without being parsed (tool arguments and output, replies, extension state); HTTP requests over reusable pools |
| `crates/protocol` | Wire types shared by daemon and clients, framing, the shared view reducer (`protocol::view`) |
| `crates/llm` | Language models over their wire protocols (docs/llm.md) |
| `crates/kernel` | What a session does next, as a pure function of its log and what just happened |
| `crates/engine` | The daemon: sessions, clients, agents, the log and store |
| `crates/ext`, `ext-derive` | The extension SDK every feature is built on |
| `crates/ext-*` | One feature each (shell, edit, mcp, lua, skills, policy, compact, subagent, …); `ext-fixtures` is test-only (`echo` agent, `sleep` tool) |
| `crates/client` | The client SDK every app renders over (docs/client.md) |
| `crates/ffi` | The facade the apps link through BoltFFI |
| `fixtures/` | Recorded wire corpus (`wire/v<PROTO>`) and rig's cassettes (`rig/`); the view vectors live in `crates/protocol/tests/vectors` |
| `docs/perf` | Build tuning, budgets and the measurements behind them |

Dependencies point down the layers in docs/design.md's crate map, from
the bottom: `sys`, `rt`, `json`/`transport`, `protocol`/`llm`, `ext`,
then `ext-*` and `kernel`, then `engine`. Clients (`client`, `ffi`,
`tui`, `web`) never depend on `ext`, `kernel` or `engine`; `apps/cli`
composes the daemon from the extensions.

## Rules

- **No backward compatibility.** Pre-release: change wire, log and config
  formats outright. No shims, legacy kinds, capability-gated fallbacks or
  migrations. Regenerate our own fixtures (wire corpus, view vectors) by
  script, never by hand.
- **Recorded fixtures are the truth.** `fixtures/rig` (provider cassettes
  and what rig decodes from them) and `crates/llm`'s fixtures are never
  patched: fix the codec. Never hand-write wire data. Generated outputs
  (`crates/llm/tests/encode.golden`) change only by their script, and only
  when the code's output really changed.
- **Minimal kernel, everything an extension.** A feature is an `ext-*`
  crate on the SDK's primitives, no per-feature knobs in the core. Prefer
  behaviour implied by types (a tool's call shape from its return type,
  typestate, sealed traits, marker types) so wrong compositions fail to
  compile; move work to compile time (const tables and fns, derives) where
  it pays. No presets: an extension states only what makes it different,
  and every option has a default.
- **Durable facts are minimal.** The log records irreducible new facts,
  never state derivable by folding it; projections and views derive the
  rest. Every live decision that replay must reproduce is a logged fact.
- **Validate once, at the boundary that owns the value.** Decode and check
  untrusted input where it enters (wire, config, provider responses) and
  pass typed values inward; do not revalidate what a type or constructor
  already guarantees.
- **Async to the core, over `rt`.** No crate named exactly `tokio` in
  anything `a` builds (`mise run check:no-tokio`, `deny.toml`). Never
  propose sync-first.
- **Native on every OS.** Behave as each platform's own team would build
  `a`: follow Linux, macOS and Windows conventions and recommendations and
  use their native facilities, never forcing one platform's patterns on
  another. Logic, layout and storage formats may differ per OS or
  architecture behind typed, compile-time seams (`sys`, `rt`, or a per-OS
  backend behind a trait). Nothing above those seams names an OS;
  `mise run check:windows` lints the whole workspace for Windows.
- **Dependencies need a reason.** Prefer a maintained crate over
  hand-rolling; hand-roll only when measurements show no crate fits the
  memory, CPU or reliability needs, and record the comparison in
  `docs/perf`. Never take a crate whose size or dependency tree is out of
  proportion to what it gives.
- **Large serde types decode in their owning crate** (`protocol::Decode`,
  `llm::Response::from_value`): release builds do not share generic
  instantiations across crates. No internally tagged enum inside another.
- **Ask before speculative hardening.** Fix the failure the task is about.
  For any other robustness or security addition (a race, a torn write, an
  edge case no test or report shows), describe the failure mode, how
  likely it is and what the fix costs, and get the owner's agreement
  before adding it.
- **Never touch a user's `~/.a`.** Tests use an isolated `A_HOME` and stop
  their daemon with `A_HOME=<home> a daemon stop`; never `pkill` broadly.
  Never read, print or commit `.env.local`.
- **Git.** Commits are `area: what`, lower case, like `git log`. Branches
  are short hyphenated names of at most three words, no slashes. Do not
  rewrite history.

## Style

Idiomatic modern Rust, small and simple. The `rust-best-practices` skill
(and its references) is the general guide; where it and this file
disagree, this file wins.

- **Comments say why.** Non-obvious constraints and surprising behaviour
  only: no narration, no changelog comments; match the density around you.
- **Std traits over custom methods** (`From`, `FromStr`, `Display`,
  `IntoIterator`, …); enums over `bool` parameters; `Option` for absence,
  never sentinels.
- **Happy path first.** The main function reads straight through; early
  returns and `let … else` instead of nested `if`/`else`; supporting
  helpers sit below it.
- **No single-use helpers or variables** unless the name explains a real
  concept or hides a genuinely complex boundary; inline them otherwise.
- **Immutable by default.** Prefer expressions (`let x = if …`, `match`)
  over a `let mut` assigned in branches.
- **Imports.** One `use` per module path, no renames except to resolve a
  real collision, no globs except `use super::*` in tests and `use Enum::*`
  inside a function that matches on it.
- **Exhaustive by default.** Workspace types are not `#[non_exhaustive]`:
  adding a variant should break every `match` that must handle it.

## Tests

- Tests are designed with the code. Private invariants are tested inline
  (`#[cfg(test)] mod tests`); behaviour reached through a crate's public
  API, or across crates or processes, goes in that crate's `tests/`.
- Name a test for the invariant it holds
  (`detach_is_never_reported_as_a_lost_daemon`), not the function it
  calls. Delete or merge tests that do not earn their keep.
- Implicit behaviour gets an invariant-named test; whole bug classes are
  removed by architecture rather than guarded case by case.
- Test the real implementation: real child processes, real files under an
  isolated home, recorded fixtures for providers. Fake only what cannot be
  real in a test (a remote host behind ssh, a model behind the `echo`
  agent, an MCP server fixture); when the balance is unclear, ask. Never
  copy the code's logic into a test.
- Black-box tests (`apps/cli/tests/blackbox`, one binary) run `a` against a daemon in an
  isolated home on the dev build with the fixtures on (`echo` agent);
  budgets (`#[ignore]`d, `mise run budgets`) run on a release build, alone.
- Shared vectors: `crates/protocol/tests/vectors/view.json` is folded by
  the Rust reducer, the FFI and the Swift tests alike
  (`UPDATE_VECTORS=1 cargo test -p protocol --test view` rewrites it); the
  wire corpus under `fixtures/wire/v<PROTO>` is written once per protocol
  version by its test (`UPDATE_WIRE=1`) and never rewritten: a message
  that encodes differently is a new version.

## Tasks (`mise run …`)

| Task | What |
| --- | --- |
| `check` | fmt, clippy `-D warnings`, every test (nextest) |
| `acceptance`, `test:web` | The black-box M1 and web UI tests on the dev build |
| `budgets` | Cold start, attach latency, idle RSS on a release build with fixtures; proves the shipped build lacks them |
| `size`, `startup`, `bloat` | Release binary size (reported, scanned for fixtures), cold start against the process floor, where size goes |
| `deny`, `machete`, `check:no-tokio`, `check:windows`, `test:miri`, `fuzz` | Licenses and bans, unused deps, no tokio, the Windows lint, miri over `sys`, the bolero targets |
| `generate [dir]` | Shell completions and the man page from the argv spec |
| `web`, `apple:test`, `apple:build`, `apple:run` | `a web` against an isolated home; the macOS app's FFI, tests, build |

`mise run check` is the full local verification; while iterating, check
and test only the crates you touch (`cargo check -p`, `cargo nextest run
-p`). CI (`.github/workflows/ci.yml`) runs every check on every OS and is
the source of truth; `.github/workflows/perf.yml` measures on native
hosted OS/architecture runners.
