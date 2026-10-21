# Conversations and agents

This document specifies the conversation primitives for composable agents.
Side threads and the metrics projection are follow-up work, not part of the
initial implementation.

Implementation status: typed awaited and detached spawning, immutable prefix
images, inherited registrations, nested storage, durable inbox transport and
depth enforcement are implemented. `ext-subagent` supplies `task`, `fork_task`
and `background_task`. Side threads, named channels, questions and metrics
remain follow-up compositions. The current inbox access policy permits parent
and child edges; `InboxAccess` is the extension seam for other policies.

A conversation owns an ordered log, an inbox and a
runner. Main sessions, child agents and side threads differ in their runner
and ownership, not in their durability rules.

## Identity, ownership and storage

A session id identifies a conversation. A root has no owner. A child has one
immutable parent and one spawn identity `(parent, owning call, purpose)`.
Ancestry is a tree; messaging is a graph. Fork ancestry is provenance, never
ownership. A child may choose the same agent as its parent and spawn again.

Store roots at `sessions/<root>/log.jsonl`, children at
`sessions/<root>/children/<child>/log.jsonl`, recursively. Each directory
owns its derived indexes and snapshots. The catalog maps ids to relative
paths and parent ids; rebuilding it traverses conversation directories.
Loading by id, resuming, listing and ordinary rest-point forks work equally
for roots and descendants. No transcript depends on an ancestor file remaining
present. Import publication syncs the containing directory and each newly
created ancestor before acknowledging creation.

## Spawn data and SDK types

`Spawn` contains a prompt, context source, runner selection and typed budget.
Context is `Fresh` or `Prefix { session, seq }`, with an inclusive committed
sequence. Runner selection consists of an agent name and a model role (session,
fast, or an extension-defined role resolved by the model service). Resolve and
freeze the concrete routing and extension generation in the spawn intent.
Future configuration changes must not change admitted work. Persist admitted
authority separately from the public request, using a compact tuple codec. A
shared protocol image envelope exposes context to transcript and publication
reducers without requiring them to decode engine authority.

`Awaited<Spawn>` and `Detached<Spawn>` are distinct request types. `Sessions`
accepts the awaited type to return an `Outcome`; its detached method returns
only a durable conversation handle after admission. A detached handle is not
a future. Modes compose these constructors in tool implementations; no config
flag selects execution semantics. An extension namespace and purpose name
allow several children per call without positional identities. Calls can await
several purposes in sequence or admit several detached children. The current
kernel has one active child wait per tool call; overlapping awaited requests
receive a typed refusal instead of replacing a live waiter. This initial limit
keeps the existing wait lifecycle singular; a multi-watch join is follow-up.

`DepthBudget` is a private-field value constructed with a maximum remaining
edge count. Descending consumes one level and returns a smaller budget or
exhaustion. The engine intersects a requested budget with the parent's
remaining allowance, so a recursive tool cannot reset its depth. Persist the
remaining budget in the spawn intent. Static constructors may use const depth
parameters, but a runtime check is necessary at erased extension boundaries
and when restoring serialized data. The token allowance bounds each model
response. The duration bounds the owned child's work; admission time is persisted so recovery subtracts elapsed time
rather than replenishing it. Token and elapsed-time allowances use the
existing positive token limit and duration types; they are not currency caps.

A prefix child inherits the effective conversation through the chosen seq,
including context edits and skill publications, but does not acquire the
parent's executable ledger. Copy the prefix into an immutable context image
owned by the child and record its source identity. Project it through the same
transcript reducer used for the parent. Never load inherited tool intents,
process groups, watches or pending actions into the child's kernel. This lets
a tool fork its current context while its owning turn is unfinished. It is
separate from the ordinary fork command's rest-point policy. Pending tool
calls do not become executable work or unmatched provider tool exchanges in
the child's context. The image stores transcript projection inputs inside the
spawn authority,
rather than installing those entries in the child's executable log. Unfinished
tool intents and queued inputs are excluded, and unmatched provider exchanges
are removed after projection. Edits beginning at an excluded ledger entry keep
an inert position for their replacement, preserving summaries without ownership.
Keeping projection inputs preserves provider state and uses the same reducer for live and recovered context. Effective skill
publication identities are restored with the image, so an inherited skill is
not published again merely because its source has a different sequence space.
Extension generation images accompany the prefix; only
explicitly inheritable state is restored into the child.

## Inheritance

Inheritance is the intersection of two source declarations:
`Extension::INHERIT` (default `true`) and the selected `AgentKind::INHERIT`
(default `Inheritance::All`). A kind declares `Except(&[Compact::NAME,
Title::NAME])` or `Only(&[Shell::NAME, ...])` using extension type constants.
`Registrar::agent_kind::<K>()` registers the kind's factory and declaration
together. Ordinary erased registrations, including Lua agents, use the default.
`Loop` is the default Rust kind. The model selects a kind by name; neither
`Spawn` nor the task tool arguments contain inheritance options.

An extension's `false` always wins, including over `Only`. Selection operates
on the parent's effective registrations, so recursion cannot regain an excluded
extension. Tools, client callables, hooks, text, folds, services and agents travel
together. Registration validates both the default child service graph and every
registered kind's graph; dropping a provider while retaining its dependent is a
startup error. A kind must retain the registration supplying its agent factory.
There is no per-feature list in the engine.

`AgentKind::INHERIT_SETTINGS` defaults to `true`. Folds opt into the session
settings layer with `Fold::SETTINGS`: model overrides and policy mode do so.
Each child starts with fresh resources and conversation folds, copies only
settings belonging to its selected registrations when enabled, then applies the
kind's source-defined `settings` layer. A kind can seed registered settings with
`States::set`; seeding a non-setting does not compile. Disabling inheritance
resolves model roles against fresh settings instead of the parent's overrides.
The daemon's configured base layers and operator policy remain in force.

Admission records the resolved extension names, settings image, model and route
in the spawn intent. The complete source generation manifest remains restoration
metadata for layered loaders, including Lua: excluded handlers never execute.
Restoration applies the frozen set, rather than rerunning a potentially changed
kind declaration, and validates its settings and service graph. Child generations
stay pinned; reload cannot widen their authority.

The `task` tool retains the current kind by default; its optional `kind` selects
a registered kind. `reviewer` is a source-declared example: it excludes
`Compact`, starts with fresh settings, and adds a policy layer denying tool calls.
It reviews the supplied prompt or `fork_task` context without tool side effects.

## Durable lifecycle

The parent commits spawn identity, frozen spec, child id and watch before
creating or loading the child. Recovery resolves that identity to the existing
child; replay itself performs no I/O. A crash between intent and creation may
complete creation, but cannot allocate another child. Admission failure is a
terminal result for that identity. The child starts its initial input once.

Awaited work ties a call's wait to its watch. Detached work leaves the parent
call free to finish, and delivers a `Fired` result at a parent step boundary.
A watch has one terminal result. Duplicate child notifications cannot append
another result, even after reload. A notification during parent execution
waits until the next safe boundary; it does not mutate an in-flight request.

Interrupt cancels outstanding owned work, including detached descendants;
successful completion of the spawning call does not. Once cancellation wins,
late success cannot replace the interrupted result. Shutdown drains or leaves
recoverable ownership facts. Compaction edits model context only: it never
removes spawn identity, delivery deduplication or watches from the ledger.
Ordinary forks reject prefixes with outstanding watches. Completed copied
watches remain completed and never reconnect to the source's descendants.
A prefix-context child imports no ownership facts at all.

## Inboxes, channels and questions

Messaging uses an envelope with a durable id `(sender, call, purpose)`, sender,
destination and payload. A destination is a conversation inbox or a typed
channel name within a root's namespace. Membership and subscription changes
are ordered channel log facts. Broadcast resolves and records its recipient
set at enqueue time; later subscribers do not receive old broadcasts unless
an explicit replay request says so. A work queue instead records one claimed
recipient. These are separate extension policies over the same envelope.

Send commits an outbox intent before delivery. The receiver commits enqueue
and deduplicates by envelope id before acknowledging. The sender then records
acknowledgement. Recovery resends unacknowledged envelopes with the same id;
this is at-least-once transport with exactly-once logged enqueue. Receiver
sequence numbers define order. Each sender serializes its durable outbox;
concurrent senders are ordered by receiver admission, not wall clocks.

The receiver records `Take` with the enqueue identity at a step boundary,
before the input influences an agent or request. Folding the log reconstructs
both queued and consumed input. Messages arriving during a provider request
wait for its next step. Context edits cannot undo delivery. Forks copy consumed
context but do not replay outbound effects; queued external messages require
an explicit fork policy and default to refusal. Inbox admission is bounded to
128 queued envelopes and
returns a typed refusal rather than silently dropping input.

A question is a message carrying a correlation id and a reply address. An
answer references that id. An awaiting question uses a durable wait; an
asynchronous question is an ordinary inbox exchange. Parent/child and sibling
messaging use identical envelopes. An ext crate owns access policy, channel
membership and question presentation; the engine only validates the issued
routing capability and performs durable transport. Cyclic waits need explicit
cancellation or deadlines, not a hidden scheduler heuristic.

## Side threads (follow-up)

A side thread uses the same conversation identity, context source, role,
ownership and inbox types, with a model-only runner instead of an agent loop.
It has no tools unless an extension explicitly supplies an agent runner.
`(extension, purpose, owner scope)` selects the thread; each request has its
own turn key. Title regeneration appends a new turn to the title thread,
including previous exchanges. Retrying the same turn key reuses its outcome;
requesting a new title creates a new turn, not a new thread. Awaited and
background types retain their existing distinction. Side results stay outside
the main transcript. Migrate `Side` and `Models` onto these primitives in a
follow-up, deleting the one-shot-only implementation then.

## Metrics (follow-up)

Fold immutable call intents, attempt decisions and results by conversation id
and call id. Record resolved model, role, runner kind and start/end times with
the call facts; use monotonic elapsed durations supplied by the driver, with
wall-clock timestamps only for correlation. Sum measured provider usage per
attempt, and mark missing usage unknown rather than zero. Cost uses an
explicit price-table version or recorded provider charge, so repricing is a
separate query. Retries count as separate attempts; cached replay counts no
new work. Main versus side is a runner classification on the intent and result,
not inferred from which entries survive context editing.

Own totals sum only calls executed in that conversation. Subtree totals walk
ownership edges and count each descendant once; fork provenance adds no
ownership edge and copied history adds no spend. Report critical-path elapsed
time separately from summed call time, since children can overlap. No mutable
usage or cost counters are stored. Snapshots may cache a fold with its covered
sequence and price-table identity.

## Layer boundaries and future modes

The kernel remains a pure lifecycle reducer: ordered inputs, waits, watches,
terminal results and commit requirements. It does not select models, discover
extensions, open mailboxes or implement a task tool. The SDK owns request,
handle, budget, context and inheritance types. The engine owns identities,
barriers, routing, storage, registry generations and cancellation. Extension
crates choose modes and policy.

| Mode | Composition or contract work |
| --- | --- |
| Fresh awaited task | Tool composition: awaited fresh spawn (`ext-subagent`) |
| Current-prefix task | Tool composition: awaited spawn with prefix context |
| Detached task | Tool composition: detached spawn returning a conversation handle |
| Parent sends while child works | Tool composition: `Sessions::send` to a conversation id |
| Child asks parent | Tool composition: question and correlated reply fold in an `ext-*` crate |
| Sibling chat | Policy and tool composition over durable inboxes addressed by conversation id |
| Named channels | Needs SDK contracts for channel identity, admission, membership and recipient snapshots; current destinations are conversation ids |
| Different agent/model | Tool composition: select a registered agent and model role; runner selection currently supports agents only |
| Recursive same agent | Tool composition: inherit task registration; depth budget descends |
| Root-only extension | Rust declaration or Lua `a.inherit(false)` excludes the whole generation |
| Resumable/forkable children | Common store/catalog paths and existing fork policy |
| Model-only side threads | Needs SDK runner and lifecycle contracts; current side model calls do not implement conversations |
| Usage/cost views | Future metrics fold over call facts and ownership edges |

Implementation must prove replay identity, terminal-result uniqueness across
cancel/reload, inbox order and consumption, inheritance exclusion, exact prefix
context and depth exhaustion with deterministic invariant-named tests.

Open product questions: whether channel names are root-local or workspace-wide;
whether detached work should ever outlive explicit parent cancellation; and
whether a future fork should accept pending inboxes by explicitly discarding
or cloning them. Initial semantics above are root-local, owned cancellation,
and refusal respectively.
