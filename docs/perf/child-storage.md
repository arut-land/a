# Child storage: phase 1 research and design

2026-10-31. Branch `child-storage`; researched main
`5610d9e021bbf83044a80974c7f6eac147fa2564`. No product code changed.

## Finding

Children already own ordinary session logs in the same JSONL format as roots.
They are not stored as whole-transcript blobs. Three different things are easy
to confuse:

| Stored object | Contents and purpose |
| --- | --- |
| Child `log.jsonl` | Authoritative ordered facts, including the child's own input, tools and replies; ordinary session storage |
| Context blob | Immutable projection inputs from an inherited cut, including message text and provider payloads; shares earlier nodes and inherited cuts, independent of ancestor log files |
| Result/attachment blob | Large tool result text, shell output, images, etc.; content addressing is useful independently of child storage |

A first context node can serialize essentially the parent's entire local
projection prefix. That **is a second, transcript-like representation of message
content in a blob**, but it is neither the child's authoritative log nor an
encoded whole child transcript. The distinction matters: deleting result blobs
will not fix duplicate child rows, and deleting context blobs without replacing
their lifetime guarantees will break inherited conversations.

Recommend keeping ordinary child logs and fixing the public relationship and
presentation first. A later simplification can replace persistent projection
nodes with references to retained ordinary log cuts. That requires an explicit
decision about ancestor retention; it is not a representation-only refactor.

## Source evidence

Paths and line numbers below refer to the researched commit.

| Boundary | Evidence |
| --- | --- |
| Same log format | `crates/engine/src/store.rs:41` defines `Header { id, workspace, parent }`; `crates/engine/src/store/jsonl.rs:119` chooses the nested directory, and `:155` writes the same sealed header. `:164` recursively discovers children. |
| Child creation | `crates/engine/src/lib.rs:827` starts initial input once; `:861` creates/loads an ordinary owned conversation; `:895` imports `Spawned` at seq 1. `crates/engine/src/driver.rs:2904` builds its ownership header. |
| Parent facts | `crates/kernel/src/session.rs:1371` commits `Link`, optional `Wait`, and `Watch` before starting the child. `:1407` handles settlement; `crates/protocol/src/lib.rs:477` defines link, spawn and watch wire facts. |
| Frozen authority | `crates/engine/src/driver/spawn.rs:16` records awaitedness, admission time, owning call, request, resolved model/routing, installed generations, context image, selected extensions/settings and any refusal. `:283` freezes them before admission. |
| Graph production | `crates/engine/src/projection.rs:518` freezes through the owning call; `crates/engine/src/context.rs:30` writes only the uncached local suffix, with model-result segment boundaries. `:186` writes `application/vnd.a.context+json`, origin `Context`. |
| Graph data | `crates/protocol/src/publication.rs:32` defines the root image; `:40` defines nodes; `:80` lists projection values; `:144` enumerates transitive retention edges. `crates/engine/src/transcript.rs:147` converts log facts to projection inputs. |
| Graph consumption | `crates/engine/src/context.rs:226` expands nodes and applies cut-local edits/dependency rules; `crates/engine/src/transcript.rs:340` projects a cut through the transcript reducer. `crates/engine/src/driver/side.rs:139` also uses these cuts for model-only conversations. |
| Payload spill | `crates/ext-spill/src/lib.rs:53` defaults to a 4,096-byte limit; `:89` spills model-visible result bytes, only for an inline result without held content. It is not a session/transcript encoder. |
| Child result conversion | `crates/ext-subagent/src/lib.rs:100` reads a blob-backed child outcome and returns result text; `crates/ext/src/lib.rs:117` formats that outcome. Watch results may themselves reference a child's last tool-result blob. |
| Access boundary | `crates/engine/src/outbox.rs:126` excludes context roots from ordinary attachment references; `crates/engine/src/fetch.rs:38` checks session access before serving bytes. Graph hashes are not public attachments. |

Fresh `task` uses no inherited image. `fork_task` calls `.owner_context()`.
`background_task` uses fresh context and `.detach(cx)`, returning a durable
handle and optionally sending inbox messages (`crates/ext-subagent/src/lib.rs:61`,
`:85`, `:141`). An ordinary `a fork` is different: it copies a validated
rest-point prefix into an unowned root (`crates/engine/src/lib.rs:747`,
`crates/engine/src/driver.rs:359`, `crates/ext-fork/src/lib.rs:37`).

The parent `Link` contains the child id, prompt and the frozen intent. The child
starts with the same intent in `Spawned`. The intent's image contains a graph
root and publication identities, not another session log. No executable parent
ledger is imported. Watch delivery, cancellation and inbox facts remain local
execution facts.

One naming trap: `Header.parent.call` is the **link sequence**, not the tool-call
sequence. `driver.rs:2923` assigns `link.get()`. The actual owning tool call is
inside the private intent. In the probe they are 17 and 16 respectively.

## Actual disk and public-stream evidence

Raw evidence is outside the worktree at:

```text
/private/tmp/claude-502/-Users-rajaniraiyn-work-a/57b9c5f0-cf3a-47ba-ba38-f417726d005b/scratchpad/r/child-storage
```

Call this directory `E`. `probe.py`, `live.py`, `measure.py`, `analyze.py` and
their command logs preserve reproduction steps and observations; `evidence.json`
records every initial log's path, byte size and line/sequence map.

The release binary was built with `fixtures`; every session used agent `echo`.
Probe processes received an allowlisted environment, isolated `HOME` and
`A_HOME`, dummy OpenAI credentials and `http://127.0.0.1:1/v1`, with
`NO_PROXY=127.0.0.1,localhost`. No provider/proxy environment was inherited by
probes. No model request was made. Each probe stopped its own daemon with its
own `A_HOME` and `a daemon stop`; none used a broad process kill.

Initial root directory:
`E/home/sessions/01a19166d53c741298aa158bbd23c294` (call it `R`).

| Real file below `R` | Bytes | Observed contents |
| --- | ---: | --- |
| `log.jsonl` | 50,192 | Root facts; task, owner-context task, background task, nested task, large task result, another owner-context task |
| `children/01a19166d5ae761e851297cc2141cc5a/log.jsonl` | 2,600 | Fresh task: header, `spawned`, environment, input, assistant |
| `children/01a19166d5ed71f789c8792007ca386f/log.jsonl` | 2,750 | Owner-context task: same local log shape; non-null inherited context root |
| `children/01a19166d61a7444b9b8c4a45265eb55/log.jsonl` | 2,630 | Detached background task: same local log shape |
| `children/01a19166d64f70149bdcb19035b6f453/log.jsonl` | 4,608 | A task that calls another task, with its own link/watch/result |
| `children/01a19166d64f70149bdcb19035b6f453/children/01a19166d65e755a93786e0c204fb2e7/log.jsonl` | 2,604 | Grandchild, ordinary log |
| `children/01a19166d6cd7425a8e1c21c7f2ef840/log.jsonl` | 20,567 | Large input echoed as assistant text; this child transcript remains JSONL |
| `children/01a19166d7087229b8eb33e778438b63/log.jsonl` | 2,787 | Second owner-context task, after the large task |

Concrete lines in the fresh child's log:

- Line 1: format 1 header, parent session
  `01a19166-d53c-7412-98aa-158bbd23c294`, parent `call: 17`.
- Line 2: seq 1, `spawned`; frozen request is `subagent/task`, prompt
  `task marker`, `Fresh`, runner `echo`, remaining depth 7; image root is null.
- Line 4: seq 3, input segments containing `task marker`.
- Line 5: seq 4, assistant text `task marker`.

In `R/log.jsonl`, line 17 is tool call seq 16; line 18 is link seq 17 to that
child; lines 19–20 are wait and watch; line 21 is the fired outcome
`task marker`; line 22 is the tool result
`The child session finished:\ntask marker`.
The fork task's link is seq 29, through owning call 28; the background link is
seq 41, with watch 42 and no awaited-child wait. Its result is delivered through
`fired`, `enqueue`, `take`, then an echo assistant response.

Only three sealed blobs exist in the initial home's blob directory:

| Hash | Bytes | Actual contents |
| --- | ---: | --- |
| `daceda0008171068b8ff279bdcd7abad35adacab5bb66e88c214aba47d13405b` | 979 | First context node: 28 positioned projection inputs, no preceding node |
| `516d7ef133975b4a55de6b4d03f1fd7e84e8d4818dffefd76dd0f8051b1da51a` | 8,127 | Second context node: 50 local suffix inputs; `previous` references the first node |
| `37f85188f14a81399c6592fd199b8c3937f123d288742b053926f2dbf0b6c835` | 6,028 | Plain text beginning `The child session finished:` followed by the large echoed result; origin `tool_output: 66` |

Files are `E/home/blobs/<first-two-hash-characters>/<hash>`. The context nodes
contain `Position`, queued/taken input, user/assistant text, tool results and,
in the later node, child-result input. They do not contain JSONL `Entry` arrays,
ownership ledgers or a rendered complete child history. The later node retains
the spilled result by reference. Large message content can also remain inline
in logs: the 4 KiB spill rule applies to tool results, not every string.

`E/<session-uuid>-frames.json` and `-page.json` capture the actual protocol.
The initial root page has 29 items. A small task appears twice:

- Index 6: `type: tool`, call 16, tool `task`, done, formatted result text.
- Index 7: `type: child`, call 17, child id, prompt, watch 19, done outcome.

The large result is a blob in the **tool** item at index 23; index 24 is the
separate child link with its ordinary inline outcome. Fork-child pages show
their own environment, input and reply, not an expanded ancestor transcript.

`E/live-frames.json` captures an actual running background child, not just
replay: public frames are `welcome`, `attached`, `head`, `synced`, `activity`.
Committed payloads arrive on requested history pages. `E/live-home` also records
two inbox messages while the child runs `sleep 1`, and a nested task whose
result is spilled in the intermediate child. `E/promotion-commands.json`
records a successful rest-point fork of that child into an ordinary unowned
root and a subsequent echo turn.

Probe caveat: first trying `a fork` with the hyphenated UUID serialized in the
JSON header failed with `no matching source session`; retrying its 32-hex display
form succeeded. `crates/engine/src/lib.rs:705` matches `SessionId::to_string()` or its
short form, whereas JSON serializes UUIDs with hyphens. This is an observed id
input inconsistency, not evidence that child logs are missing. The failed
command and the successful retry are both retained; no product fix was made.

## Why the app shows blob text and duplicate child UI

`crates/protocol/src/view.rs:1017` creates a child item for every link, alongside the
tool item created at `:986`. Watches/fired facts update that child item
(`:1125`). The child item's `call` is its link sequence; its public fields do
not identify the owning tool call. A shared renderer cannot reliably group
these from adjacency: calls may run concurrently, and one call can create
multiple named children.

`crates/protocol/src/view/present.rs:305` falls back to `Shown::Held` for a successful
blob outcome without a presentation card. `:94` spells that as a byte count,
media type and “held as blob …”. `task` supplies no result card. This is the
large-result fallback leaking into product presentation, not the app displaying
a context-node attachment. Separately, an unreadable child-result fallback is
worded in `crates/ext/src/lib.rs:129`; do not conflate these paths.

FFI already sets `DisplayItem.navigation` to the child id
(`crates/ffi/src/lib.rs:1113`). `apps/apple/App/ChatView.swift:24` renders every
item; `:215` renders the tool and `:225` renders the child as another row. The
Swift file does not consume `navigation`. This explains duplication and the
missing drill-down on the inspected base. These are source-traced UI findings,
not a claim that this lane launched and visually verified the app.

Coordination: the local `app-ux` branch was inspected and remained at the same
base without a diff; two remote branch checks, more than three minutes apart,
found no pushed `app-ux` branch. No other lane is registered in this thread's
agent messaging tree. This report is the storage handoff for that lane and the
coordinator: group by an explicit owning-call relation, use existing child-id
navigation, and replace the attachment fallback without removing payload blobs.
Reconcile against the app lane's eventual diff before implementation.

## Agreement with the recursive conversation design

The implementation matches `docs/agents.md:19`, `:76`, `:128` and
`docs/design.md:160`: every persisted conversation is a session, ownership is
a tree, messaging is a graph, and prefix context is separate from executable
ownership. Persisted model-only side threads also use ordinary owned logs;
ephemeral turns intentionally do not (`docs/agents.md:460`).

The UI/public topology is less composable than the storage model. Session
summaries lack a parent relation (`crates/protocol/src/lib.rs:1300`), and the child
view item lacks the owning tool call. Clients can page by child id already
(`crates/engine/src/history.rs:116`), but cannot build the whole tree from session
summaries alone. Model context inheritance and human navigation should remain
distinct: expanding a child must not silently duplicate its ancestors in the
visible transcript.

The graph's independence guarantee is explicit in `docs/agents.md:89` and
`:136`, and tested by
`context::tests::frozen_context_and_its_payloads_survive_restart_and_source_log_removal`
and `driver::spawn::context_tests::context_survives_removal_of_ancestor_files`.
A plain parent-cut pointer cannot satisfy this if the referenced log is deleted.

## What we over-engineered

1. **Presentation exposes execution machinery.** A tool row, link row and
   sometimes assistant notification represent one child interaction. The model
   needs tool results; the human needs one navigable conversation relationship.
   Rendering every fact class as a peer row makes the implementation visible.
2. **The relationship is less typed than its authority.** Owner identity is
   buried inside an opaque tuple while several fields called `call` actually
   mean link sequence. Making clients decode engine authority would deepen this
   mistake. A small explicit public relation removes that whole coupling.
3. **There are two persistent representations of conversation content.** Raw
   logs and `ContextValue` nodes need codecs, edit/dependency semantics,
   publication scoping and retention traversal. This complexity buys independent
   immutable inherited context and shared suffixes; it is not gratuitous, but
   retained log cuts could buy the same semantics with fewer durable formats.
4. **Admission data is repeated.** The same frozen intent appears in parent
   `Link` and child `Spawned`; prompts also occur in tool arguments and child
   input. Some repetition supports crash-before-creation recovery and independent
   child loading. Removing it safely requires an admission reference and retention
   policy, not simply deleting a field.

There is no evidence here of a corrupt graph or of whole child transcripts being
replaced by blobs. The source and real probes support the UI diagnosis; the
existing graph invariants passed. Do not sell a storage rewrite as a proven
fix for unobserved corruption.

## Simplest correct structure and deletion boundary

Keep one append-only log format for every persisted conversation and the
existing id-addressed store. Public topology should be a typed relation:

```text
child id -> parent id, link position, owning tool call, purpose/role
context  -> fresh | immutable reference to (source session, source cut)
```

The context cut ends at the admitted position, not the parent's current head.
Keep provenance separate from ownership for promoted forks. A detached handle
is still only a session id; messaging and lifecycle do not require a second kind
of transcript.

The parent records a minimal topology link **plus the irreducible execution
facts**: frozen admission authority before child creation, watch delivery,
tool outcomes and inbox decisions. “Only a link” cannot mean dropping these:
configuration can change, and a crash may occur after admission but before a
child exists. At first, leave the existing authority copy intact. Later, one
canonical admission record can be referenced by the child once its retention
rules are explicit.

For a graph-free design, retain every ordinary source log cut while any live
reference needs it, recursively. Deleting a visible parent may remove its catalog
presence, but cannot physically discard referenced bytes. A shared log reader
projects inherited cuts through the existing reducer without importing calls,
approvals or watches. It applies only edits visible at each frozen cut and
preserves source sequence scopes, runner interpretation, publication identities,
provider state and admitted presentation generations. Clients request the
child's local paged history through the existing History API; expansion uses the
typed link, not graph blobs or a serialized transcript field.

| Can disappear | Condition |
| --- | --- |
| Independent child row plus duplicated tool presentation | Replace with one grouped, navigable relation; keep underlying tool and delivery facts |
| Persistent `ContextNode`/`ContextImage` graph codecs and blob origin | All task and side-turn inheritance reads retained raw log cuts instead |
| Graph sealing, traversal, graph-specific access/retention branches | Same replacement, with source-log retention implemented at the store boundary |
| Durable `ContextValue` encoding and graph edit/position records | Projection inputs may remain transient; one log remains the durable source |
| Copied spawn authority in the child | Canonical admission references remain resolvable after restart and parent deletion |

Keep result/image/paste blobs, hashing/materialization, ordinary JSONL checksums
and indexes, id catalog, durable admission barriers, watches/inboxes, budgets,
extension-generation restoration and typed runner policies. Keep provider-prefix
compilation/caches unless measurements justify removing them: they avoid replaying
settled model ancestry (`crates/engine/src/driver/prefix.rs:43`). Those caches are not
evidence that transcripts need a second durable format.

No migration or compatibility layer: change the protocol and log format outright
when their schemas change (currently PROTO 43 and JSONL format 1). Generate a new
versioned wire corpus and view vectors with their existing update scripts/tests.
Never edit recordings or old versioned wire files by hand. Old isolated research
homes can be discarded; do not access or migrate a user's home.

## Measurements and projections

Host: macOS arm64. Release build, fixtures enabled; current implementation only.
No proposed implementation exists, so there is **no measured before/after
performance claim** and no latency, RSS or shipped-binary-size saving claimed.

Five paired rounds alternated `task` then `fork_task` and the reverse, each in
a newly created root with equivalent 8 KiB parent text and a small child result.
Each round has a distinct parent marker to avoid cross-round content deduplication.
The same isolated daemon served the pairs. `E/storage-rounds.json` records raw
per-admission sealed-blob growth; `E/measure-commands.json` records the commands.

| Existing behavior | Added sealed blob bytes, median [min–max] | Added blobs |
| --- | ---: | ---: |
| Fresh task | 0 [0–0] | 0 in every round |
| Owner-context task | 16,969 [16,969–16,969] | 1 in every round |

This compares two current context semantics, not an A/B of the proposed design.
It measures logical file bytes, excluding indexes, directory allocation and
materialization caches. No timing comparison was made. Initial mixed-workload
inventory: 88,738 log bytes across eight logs; 9,106 context-node bytes plus
6,028 result-blob bytes. The second cut shares the first node rather than
copying it into each child intent.

Projected effects of retained raw cuts:

- Disk: removes the extra context serialization for referenced content; may
  retain larger raw logs containing nonprojecting facts and edited-out payloads.
  Net storage improvement is workload-dependent.
- CPU: avoids node encoding/hashing/writes at admission; raw-cut replay can cost
  more than the current cached suffix and bounded graph traversal. No speedup
  is established.
- Memory: fewer graph expansion intermediates are possible, but provider context
  still needs projection, and naive ancestor flattening could increase memory.
  Preserve paging, bounded traversal and compiler prefix reuse.
- Binary/code size: removing a second durable projection codec should reduce
  code, but byte savings need release A/B builds with identical features and
  debug/strip settings. No numerical projection is justified yet.

Future performance acceptance should interleave at least five release A/B
rounds on repeated forks, deep trees, restart, bounded model-only turns and large
results. Report median [min–max] for admission latency, first child page and
restart, plus peak/idle memory and total retained bytes. Use echo or recorded
fixtures, never a real provider request.

## Staged writer plan and proposed file ownership

No ownership outside this report is claimed in phase 1. Assign these lanes
explicitly before product work:

| Stage | Writer ownership | Deliverable and invariant tests |
| --- | --- | --- |
| 1. Public relationship | `crates/protocol/src/lib.rs`, `view.rs`, `view/history.rs`, `view/present.rs`; relevant kernel link production in `crates/kernel/src/session.rs`; engine `driver/spawn.rs`, `driver.rs`; corresponding tests and generated wire/view vectors | Expose owning tool call, link position, child id and purpose without private-authority decoding. Test `concurrent_children_group_under_their_owning_calls` and `several_named_children_remain_distinct`. Keep authority/storage unchanged initially. |
| 2. App conversation UI | app-ux lane: `apps/apple/App/ChatView.swift`, `ContentView.swift`, `apps/apple/AKit/Sources/AKit/SessionStore.swift`; FFI `crates/ffi/src/lib.rs`/`live.rs` if needed | One child affordance, id navigation, useful result preview and demanded child pages. Test `child_navigation_pages_the_child_history` and `large_child_results_render_without_storage_jargon`; Swift reducer/UI coverage. Coordinate FFI ownership before parallel edits. |
| 3. Retained log cuts | Engine store/catalog/history, `context.rs`, `projection.rs`, `transcript.rs`, `driver/spawn.rs`, `driver/side.rs`, `driver/prefix.rs`; protocol `publication.rs`; ext history/conversation seams only if required | Replace graph storage across both task and model-only cuts, then delete graph codecs/writers/traversal. Test restart, source removal semantics, bounded reads, edits and pending exchanges with real files/processes; retain recorded-provider equivalence tests. Assign this as one coordinated storage lane to avoid codec/reader skew. |
| 4. Admission deduplication | Engine admission/store/catalog boundary and protocol topology facts | Only after cut retention works: canonical admission reference, preserving `crash_after_admission_creates_the_same_child_once` and selected generation/settings after reload. Evaluate whether saved bytes justify a new lookup. |
| 5. Specification and measurements | `docs/agents.md`, `docs/design.md`, `docs/perf/<writer-branch>.md`, each changed crate's manifest/test files | Update retention contract, regenerate owned fixtures, release A/B measurements, macOS checks and Windows lint; subsequent full CI under the coordinator's phase-2 process. |

Stage 3 must cover both task and model-only contexts before deleting graph types.
Tests such as `a_frozen_child_ignores_later_parent_edits`,
`pending_exchanges_never_become_child_work`,
`identical_local_sequences_and_provider_ids_remain_in_their_scopes`,
`nested_children_remain_addressable_after_the_catalog_is_lost` and
`bounded_side_context_does_not_materialize_unrequested_ancestry` describe
behavior the replacement must preserve, not expendable graph machinery.

## Validation and owner-pending decisions

- macOS release echo probes: ordinary, owner-context, detached, nested and large
  results; live inbox delivery; public head/history capture; child promotion.
  `E/invariants.txt`: eight invariant-named checks passed on actual files/pages.
- `cargo nextest run --release -p engine -p protocol`: **522 passed, 16 skipped**
  (ignored budgets/benchmarks). Full output: `E/nextest.log`.
- `mise run check:windows`: **passed**, full workspace clippy excluding ext-lua,
  all targets, `x86_64-pc-windows-msvc`, warnings denied. `E/windows.log`.
- Cargo used `mise exec --`, `CARGO_BUILD_JOBS=2` and only the assigned external
  `tgt-child-storage` directory. Subsequent tooling uses an external copied mise
  configuration with the repository's `[env]` loader removed. The initial build
  invocation used the repository mise config, which declares automatic loading
  of `.env.local`; this was noticed before any live probe. No such file contents
  were explicitly read, printed or committed. All live probes use a separate
  fully constructed environment independent of mise.
- No new product tests needed for this documentation-only diff. No full CI
  dispatched in phase 1; target/evidence retained for phase 2 as instructed.
  Apple app runtime verification belongs to app-ux; this lane verified the
  storage/protocol behavior on macOS and traced the existing Swift rendering.

Owner decisions before a storage rewrite:

1. **Retain source logs or preserve independent graph cuts?** Physically removing
   referenced logs would break child context under a raw-cut design. This is
   certain if deletion is allowed, not speculative; retaining hidden source cuts
   costs disk and a store-owned retention graph. Keeping today's graph avoids that
   change. Recommendation: land stages 1–2 first; require explicit agreement on
   retention before stage 3.
2. **Canonical admission reference?** Losing its source would prevent recovery or
   widen/change admitted authority. Likelihood is tied to the same deletion
   policy; cost is a retained admission lookup and its crash-ordering tests.
   Do not deduplicate authority before deciding this.
3. **Serialized id input consistency.** The hyphenated-id fork refusal is observed
   on this base. Users copying ids from JSON can encounter it; frequency was not
   measured. A boundary normalization/parser change and a real CLI regression
   test should be a separately owned follow-up, not a child-storage rewrite.

No speculative hardening was added. No additional non-test source files are
needed for this research deliverable. Await coordinator review and phase-2
instructions after the report commit is pushed.
