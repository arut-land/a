# Build tuning: backlog and evidence

What we tried, what we deferred, and what we rejected for compile time, binary
size, startup, CPU and RSS. Rejected items are kept on purpose: most were
rejected because the binary was tiny (485 KB) when we measured, and several will
pay off once it grows. Re-measure before adopting anything here.

Researched 2026-10-18 on macOS arm64, `nightly-2026-10-17`, ld-prime.

## Budgets and how to measure

| Budget | Target | Command | Baseline (2026-10-18) |
| --- | --- | --- | --- |
| Release binary | < 4 MB | `mise run size` | 473 KiB; 3018 KiB with the provider (see below) |
| Cold start (`a --version`) | — | `mise run startup` | 2.5–2.8 ms |
| CLI peak RSS | — | `/usr/bin/time -l target/release/a --version` | 1.79 MB |
| Idle daemon RSS | < 10 MB | `ps -o rss= -p <pid>` after 5 s idle (`VmRSS` on Linux) | 6.4 MB RSS, 1.8 MB footprint (2026-10-19) |
| Cold first daemon reply | < 50 ms | `mise run budgets` | 20–33 ms (2026-10-19, see engine hot paths) |
| Where size goes | — | `mise run bloat`, `mise run bloat:lines` | `.text` 343.9 KiB |

The 2026-10-21 idle RSS regression, merge-by-merge measurements and lazy
subscription fix are recorded in [idle-rss.md](idle-rss.md).

## In place

| Setting | Where | Why |
| --- | --- | --- |
| Cranelift for workspace crates in dev | `.cargo/profiles.toml` | Fast codegen for code we edit |
| Deps at LLVM opt-level 0, no debug info, in dev; the few the tests feel at 1 (serde_json, vte, unicode-width, rustix, memchr) | `.cargo/profiles.toml` | Proc-macros (and their syn) run once per build and build ~2x faster at 0: clean dev build 10.7 s → 4.4 s when only `syn` and `usage-derive` were named. `package."*"` outranks `build-override`, so the default is 0 and the hot ones are named, not the other way round (2026-10-20) |
| `-Zthreads=8` | `.cargo/config.toml` | Parallel rustc frontend |
| Release: fat LTO, `codegen-units = 1`, `strip = "symbols"`, packed split debuginfo | `.cargo/profiles.toml` | Matches uv, ruff, mise, Deno. Thin LTO measured +14% size |
| `panic = "unwind"` in release | `.cargo/profiles.toml` | A panicking session actor must not kill the daemon |
| `cargo build-release` adds `-Zbuild-std=std,panic_unwind` | `.cargo/config.toml` | No gain today; makes size levers stronger (see experiments) |
| Linux: clang + mold | `.cargo/config.toml`, `mise.toml` | Link time. Untested until Linux CI |
| Windows MSVC: `+crt-static`, `/STACK:8388608` | `.cargo/config.toml` | No VC++ runtime dependency (uv, ruff, Deno, Zed, oxc, ripgrep). Untested until Windows CI |
| mbx (mr-boxington) cargo wrapper | `mise.toml` | Shared compile cache across checkouts and worktrees |
| Native TLS on Apple and Windows (`ureq` `native-tls`), rustls + platform verifier elsewhere | `crates/provider/Cargo.toml` | 3968 → 3179 KiB on macOS: drops ring, rustls, webpki |
| Release `opt-level = "s"` for everything, `3` for `serde_json` | `.cargo/profiles.toml` | Grown crate by crate from "cold crates at s" (3179 → 3018 KiB, startup unchanged at 3.4 vs 3.5 ± 0.2 ms) through protocol (decode and encode under 5% slower, −92 KiB), llm (decoders ~6% per frame, −160 KiB), engine/ext/client/sys/rt (−160 KiB, no measured daemon CPU change), tui (delta frame ~45 µs at s and 3 alike, docs/tui.md), std via build-std (−13 KiB), the toml crates and ureq/usage-argv at z (−32 KiB over 3). Flipped to one default on 2026-10-20: 2846 → 2717 KiB on macOS (the crates never listed, url's and native-tls's dependencies, uuid, hypertext, ratatui-core's among them, shrank more than z → s cost; 25 entries → 2, and a new crate needs none). serde_json frames every message: on a recorded 123-entry, 609 KiB chat session (`crates/protocol/tests/bench.rs`, fastest of 10, median of 5 runs) decoding the log takes 284 µs at 3 and 316 µs at s (+11%), encoding it 131 and 173 µs (+32%), for 32 KiB of binary (2846 vs 2813 KiB); the view's fold, which serde_json does not run, 69 and 71 µs (2026-10-21). `alloc` was the other exception (the terminal client's frame allocated per span) until the client's own grid and `SmolStr` rows: with those, `alloc` at s and at 3 measure the same on every `render_cost` row (docs/tui.md), and the exception went (−65 KiB, 2026-10-20) |

## Deferred: adopt when the trigger fires

| Idea | Trigger | Expected effect | Evidence |
| --- | --- | --- | --- |
| Release `opt-level = "s"` for cold code, `3` for deps and hot crates | Binary > ~2 MB | Zed: macOS −33%, Windows −28.5% (speed not measured); Deno uses `z` + ~55 hot crates at 3 | zed#65168, deno `Cargo.toml` |
| Linux `-Clink-arg=-Wl,--icf=safe -Cllvm-args=-addrsig` (release only, CI) | Linux CI exists | Zed Linux −23.5% with symbol stripping; Deno −3.77 MB | zed#65169, deno#34478 |
| Strip with `--strip-unneeded` after uploading symbols | Crash reporting exists | Part of Zed's Linux win | zed#65169 |
| macOS chained fixups (`MACOSX_DEPLOYMENT_TARGET = "13.0"`) | Minimum macOS chosen | Deno startup 9.8 → 9.0 ms. Here: fixups emitted, +16 KB, no startup change | deno#35409 |
| `-Ctarget-cpu=apple-m1` for `aarch64-apple-darwin` only | Hot CPU paths exist | Unmeasured; nushell and Bun do it (every arm64 Mac is ≥ M1) | — |
| PGO, thin-LTO instrumented build, fat-LTO final | Representative workload exists | uv −8.9% time, −13% size; ruff −8–14%; mise −6–10%; rust-analyzer ~20%; mbx itself −3.1% instructions ("marginal") | uv#21001–21004, uv#22451, ruff#27570, mise#11031, r-a#19582, jdx/mr-boxington#184 |
| Allocator: measure system vs mimalloc v2 vs jemalloc | Daemon has real idle and load workloads | Projects split three ways: jemalloc (Astral, Biome), mimalloc (Bun, oxc, turborepo), system (Deno reverted jemalloc, deno#19187). glibc arenas can inflate threaded RSS | uv allocator docs |
| musl static + mimalloc for Linux distribution | Linux packaging | Portable binary; musl malloc is slow (ripgrep uses jemalloc on musl) | ripgrep `main.rs` |
| Non-PIE Linux link (`-no-pie`) | Cold start budget bites | mise hook-env 6.6 → 3.5 ms, page faults 2201 → 334; costs ASLR | mise#13687 |
| `hint-mostly-unused` for heavy deps (`windows-sys`, Lua bindings) | Such deps arrive | Faster builds of huge, mostly unused crates; no effect on usage-rs | `[unstable] profile-hint-mostly-unused = true` |
| Split large crates | One crate dominates incremental builds | uv: incremental 30–43% faster | uv#22260 |
| `linker = "rust-lld"` for Windows MSVC dev builds | Windows dev | Faster links (rust-analyzer, turborepo) | turborepo `.cargo/config.toml` |
| `#[derive(DebugNoInline)]`-style derive on huge enums | Big error or IR enums show in `bloat` | uv −2.09% | uv#22430 |
| Avoid monomorphizing heavy generic types (dyn at boundaries) | `bloat:lines` shows generic blowup | Targeted size wins | uv size work |
| Startup symbol ordering (two-pass link with an order file) | Binary > ~10 MB and RSS matters | Deno Linux idle RSS 47 → 29 MiB, −2 ms | deno#36219 |
| zstd-compressed embedded payloads (web assets, Lua stdlib) | Embedded assets grow | Deno, Bun (ICU pruning + lazy zstd ~6.8 MB) | deno `cli/build.rs`, bun#41330 |
| Commit the attaching client's environment without waiting | Cold REPL start matters more than recording the environment before the replay | One `F_FULLFSYNC` (2.6 ms) off the cold start | — |
| `/OPT:SAFEICF` on Windows | Full ICF breaks function identity | Bun hit BigInt breakage with full ICF | bun#30219 |

## Notes from adding the LLM provider (2026-10-18)

- Size before the provider: 473 KiB. With rig-core + ureq + rustls: 3968 KiB (std 626K, rig_core 394K, rustls 316K, serde_json 177K, ring 132K).
- `ureq`'s TLS module is gated on its full `native-tls` feature; `native-tls-no-default` builds but panics at runtime ("feature is not enabled: native-tls"). The full feature also links `webpki-root-certs` (~213 KiB) although `RootCerts::PlatformVerifier` never uses it. Revisit if ureq gates the roots separately.
- Dev builds compile workspace crates with Cranelift, which cannot unwind on macOS arm64: a panic aborts the process instead of unwinding. Release (LLVM) unwinds, and the engine wraps every effect in `catch_unwind`. Revisit if Cranelift gains unwinding here, or build `engine` with LLVM in dev if dev aborts become a problem.

## Own provider layer instead of rig (2026-10-18)

`crates/llm` replaces rig-core: its wire codecs and quirk tables, ported by hand onto a small core (spec, writer, client), with no catalog, telemetry or agent framework. Recorded rig cassettes (`fixtures/rig`) hold it to rig's decoding.

| Step | Binary | Daemon footprint after a model call |
| --- | --- | --- |
| rig as published (catalog 428 KB) | 2873 KiB | 8.3 MB |
| rig vendored, catalog trimmed, pricing patched out | 2453 KiB | 4.7 MB |
| `crates/llm` (Responses only), stateless replay of provider items | 1774 KiB | 4.7 MB (two turns, encrypted reasoning replayed) |
| `crates/llm`, all four wires (Responses, Chat with 24 hosts, Messages with 5, Gemini) | 2114 KiB | 4.4 MB (two turns on Claude through Messages) |
| Simplified: shared model facts, `SmolStr` ids, typed borrowing stream parsers (decode ~2x faster, allocations per text frame 17→2 on Responses, 53→9 on Gemini) | 2291 KiB | — |
| One deserializer instantiation per typed frame (Chat's repeated-key fallback reparses text) | 2259 KiB | — |
| `llm` at `opt-level = "s"` (decoders ~6% slower per frame) | 2098 KiB | — |
| Agent loop, hooks, remote bridge and binary staging | 2422 KiB | — |
| Shared record decoders (`protocol::Decode`, `llm::Response::from_value`): release builds instantiate a generic deserializer once per calling crate, so each large one is decoded in its owning crate; hook decisions externally tagged | 2211 KiB | — |

## Engine hot paths (2026-10-19)

Measured on macOS arm64 against the release build, base `eb361d9` vs the
engine after the session-log, fan-out and cold-path changes. A Python client
speaks the wire protocol over the daemon socket with the echo agent;
time to first delta uses `A_AGENT=chat` against a local HTTP server replaying
the recorded text reply of `fixtures/rig/cassettes/openai/streaming_grammar/
tool_then_followup_text.yaml` (`OPENAI_BASE_URL`). Footprint is `footprint
<pid>` (dirty memory), RSS is `ps`. Cold runs alternate the builds so machine
load hits them alike; 40 runs each.

| Metric | Base | Now | What changed |
| --- | --- | --- | --- |
| Echo reply, input to committed answer (p50) | 5.98 ms | 2.99 ms | A step's entries commit in one batch, one fsync |
| First delta, local model (p50) | 6.26 ms | 3.48 ms | User message and model-call intent share an fsync |
| Model reply committed (p50) | 8.08 ms | 6.02 ms | Same |
| Cold `a new` (p50 / p10) | 20.6 / 18.4 ms | 13.7 / 12.0 ms | No fsync before a session's first entry, none on the pid file |
| Cold `a ls` (p50 / p10) | 15.9 / 14.7 ms | 10.5 / 9.0 ms | No fsync on the pid file |
| Idle daemon | 6384 KB RSS, 1808 KB footprint | 6384 KB, 1824 KB | — |
| 50 sessions × 20 exchanges, attached (footprint) | 5969 KB | 5633 KB | Idle actor future 5960 → 136 B (each unit of work boxed) |
| Same, detached / hibernated (footprint) | 5185 / 4304 KB | 4849 / 3936 KB | Same |
| Allocations per echo exchange, 1 / 8 clients | 59 / 184 (just before frames) | 60 / 130 | Each frame encoded once and shared (`protocol::Frame`) |
| Bytes allocated per exchange, 8 clients | 44.4 KB (same) | 28.2 KB | Same |
| Release binary | 2341 KiB (`__text` 1,684,000 B) | 2373 KiB (`__text` 1,721,760 B) | Extension records, child results, store I/O off the workers; one shared entry serializer won back 13.8 KB of `__text` |

Allocation counts come from a counting global allocator in a throwaway engine
test that drives `handle` over a socket pair (client side included, so only
the differences mean anything).

Where the time and memory still go:

- Every commit is one `F_BARRIERFSYNC` on Apple (`sys::sync_data`), 247 µs
  here against 2.6 ms for `F_FULLFSYNC` (Rust's `sync_data`). It keeps write
  order, which the torn-tail recovery relies on, but the newest commits may
  be lost on power loss; the owner chose that. Echo reply p50 3.0 → 0.77 ms,
  p90 1.7 ms (black-box harness, 200 exchanges).
- A cold REPL start still pays two flushes before its first reply: the
  attaching client's environment is committed before the replay, and a new
  file's first commit also syncs its directory.
- Idle RSS is mostly clean pages of Security and CoreFoundation (linked for
  native TLS); dirty footprint is 1.8 MB. Both TLS choices link Security on
  macOS (the platform verifier does too).
- Freed session memory stays in malloc's zones after hibernation (97%
  fragmentation, 94 KB live of 2.8 MB) until the OS reclaims it under
  pressure.

## Rejected for now, with the reason and when to revisit

| Idea | Measured here | Why not now | Revisit if |
| --- | --- | --- | --- |
| `panic = "abort"` | −14% (418 KB) | Kills the daemon on any panic. No project isolates panics under abort except with a process per session | Sessions move to separate processes |
| `panic = "immediate-abort"` | −55% (218 KB, needs build-std + `--target`) | Same as above, and no panic message | Same as above |
| Global `opt-level = "z"` | −27% (354 KB with build-std, 420 KB without) | Slows CPU-bound code; startup unchanged at our size | Size matters more than throughput for a specific artifact |
| Thin LTO in release | +14% (554 KB) | Bigger and no faster | Release build time becomes a bottleneck |
| `-Zlocation-detail=none`, `-Zfmt-debug=shallow` | −2.5 KB text, file unchanged | Guts panic locations and Debug output in logs | A size-critical artifact without logs |
| `-Zbuild-std-features=optimize_for_size` | Build fails (E0152 duplicate lang item `sized`) | Broken on this nightly | A newer nightly fixes it |
| `-Zshare-generics` in release | — | Zed: sharing made binaries bigger; removing it was −12.65% on macOS | — |
| rust-lld `--icf=safe` on macOS | text −2.7 KB, file +16 KB | ld-prime already deduplicates (`-dead_strip`, no `-no_deduplicate`) | ld-prime changes or the binary grows a lot |
| Zed's `-Wl,-unexported_symbol,__R*` | No-op | `nm -gU` shows only `__mh_execute_header` exported | We export symbols (plugins, FFI dylib) |
| `malloc_zone_pressure_relief` after hibernating sessions | Hibernated footprint unchanged (4176 → 4016 KB, noise); it returned 0 in a standalone test too | No effect on this macOS | `malloc_trim` once Linux is measured |
| One boxed `rt::unblock` for every store call | `__text` −704 B | Not worth the indirection | — |
| `target-cpu=native` in distributed builds | — | Non-portable; no project ships it (Bun's x86 baseline is nehalem) | Local-only builds |
| BOLT | — | mise's own PR calls its timing inconclusive | Large binary with a stable hot path on Linux |
| wild linker | — | Unverified claims; Zed dropped mold and wild (zed#53078) | It becomes a rustc default or proves faster here |
| `[unstable] build-std` globally | — | Also rebuilds std for dev at our dep opt-level | Cargo allows build-std per profile |

## Experiments (release with build-std, macOS arm64)

| Variant | File (B) | `__text` (B) | Note |
| --- | --- | --- | --- |
| Baseline | 485,200 | 357,580 | Startup 2.5 ms, RSS 1.79 MB |
| Without build-std | 485,216 | 352,196 | |
| `opt-level = "s"` | 435,728 | 293,412 | |
| `opt-level = "z"` | 354,432 | 246,468 | Startup within noise |
| `z` + location-detail + fmt-debug | 354,320 | 243,712 | |
| Thin LTO | 554,528 | 386,344 | |
| `panic = "abort"` | 418,240 | 342,004 | |
| `immediate-abort` (`--target`) | 218,336 | 158,276 | |
| Deployment target 13.0 | 501,152 | 357,580 | Chained fixups emitted |
| rust-lld `--icf=safe` | 501,680 | 354,884 | |
| `optimize_for_size` | build fails | | |

Startup was 2.2–2.8 ms for every variant (hyperfine, 400–1000 runs); at this
size exec and dyld dominate, so all startup differences were noise.

Dev builds: proc-macros at opt-level 0, 1, 3 gave clean builds of 4.5, 10.7 and
13.2 s, with the same 0.32 s incremental rebuild. Cranelift and LLVM rebuilds
were equal (0.31 vs 0.32 s) on a 49-line crate. mbx cached cranelift dev builds
and build-std release builds; only the final link is never cached.

## Leads that turned out wrong

- uv's `codegen-units = 1` did not make runtime faster (±3–6%, noise); it made
  PGO builds 48–60% faster and the binary 17% smaller (uv#22303).
- mise does not use `codegen-units = 1`; it went back to 16 (mise#12505).
- "aube" is a Node package manager, not mbx (jdx/mr-boxington).
- Deno's ICF saved 3.77 MB, not the 6 MB in its config comment (deno#34478).

## Persistent model-only threads (F01)

The release measurements use `CARGO_BUILD_JOBS=2 mise run size`, including
build-std and the normal shipping feature set. No budget or startup runs
were used.

| State | Release binary |
| --- | ---: |
| Main before F01 | 3915 KiB |
| F01 before lifecycle review | 3996 KiB |
| Reviewed ownership, recovery and retention changes | 3948 KiB |

The review recovers 48 KiB of the original 81 KiB growth and leaves 148 KiB
under the 4096 KiB cap. Message decoding shares one set of byte-based field
readers. SDK turn, runner and role codecs live in their owning crate. Thread
envelopes use tuples, committed replies share the actor's existing delivery
path, and derived indexes retain compact position rows. Recovery consumes
abandoned inputs through existing take and interrupt facts.

Thread ids use the existing UUID crate's namespace derivation (`v5`, with
its maintained `sha1_smol` backend), so an owner and thread key determine the
id without storing a writable foreign conversation id in copied history.
The measurements include that dependency; a second hash implementation is
unnecessary. Admission also rejects an existing id owned by another session.

The 1000-regeneration probe bounds the last request to fewer than 3100
messages and child history to less than 2 MB. The child keeps 2002 own
instruction/reply messages and no copies of the owner's original input.

Production Rust grows by 213 lines during review, or 730 over main,
excluding test files and inline test modules. This exceeds the original
500-line estimate by 230: the original implementation was 17 lines over,
and review requires 213 additional lines. Those changes comprise engine
ownership/recovery, per-turn projection and shared committed reply delivery
(+177), typed SDK codecs (+81), generic kernel recovery (+10), and protocol
ownership derivation (+5), offset by merging the message readers (-60).

Files outside the original F01 manifest:

| File | Reason |
| --- | --- |
| `Cargo.toml`, `Cargo.lock` | Enable UUID namespace derivation for session-owned thread ids. |
| `apps/cli/src/exts.rs` | Check the title extension's new source version. |
| `apps/cli/tests/blackbox/web.rs` | Count attachments for the selected session when owned threads also appear in listings. |
| `crates/engine/src/driver.rs` | Admit runners, restore model-only bindings after faults, and supply generic recovery with abandoned input sequences. |
| `crates/engine/src/driver/inbox.rs` | Await committed conversation replies through the actor. |
| `crates/engine/src/projection.rs` | Share committed reply delivery across tool, model and inbox consumers. |
| `crates/engine/src/services.rs` | Bind model-only conversations without tools or loop hooks. |
| `crates/engine/src/store/index.rs` | Index thread identities, input aliases and committed outcomes without payload retention. |
| `crates/engine/src/transcript.rs` | Retain only thread exchanges and project per-turn owner context separately. |
| `crates/ext/src/host.rs` | Expose explicit runner classification at the existing binding seam. |
| `crates/ext-subagent/src/lib.rs` | Select agent runners through the shared SDK runner type. |
| `crates/llm/src/conversation.rs` | Decode persisted rich messages through their owning codec. |
| `crates/llm/src/spec/read.rs` | Share message and response field readers. |
| `crates/llm/src/spec/read/tests.rs` | Verify every message role and opaque provider state survives decoding. |
| `crates/protocol/src/publication.rs` | Encode frozen authority and compact thread envelopes without copied child ids. |
| `docs/perf/build-tuning.md` | Record measured size, dependency choice and the production line-count exception. |
