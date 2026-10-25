# Conversations and agents

Every conversation a call owns is one primitive, `ext::Conversation`: a
title, a summary, the policy judge's verdict, an approval description and
a sub-agent's task differ only in how they are built. This document
specifies that primitive, its storage and lifecycle, inboxes, and the
metrics derived from it. Metrics are derived from qualified provider-attempt
facts.

Implementation status: typed turns and tasks, awaited and detached
execution, ephemeral turns, keyed reuse, immutable prefix images, inherited
registrations, nested storage, durable inbox transport and depth
enforcement are implemented. `ext-subagent` supplies `task`, `fork_task`
and `background_task`; title, compact, judge and approval descriptions are
turns. Named channels and questions remain follow-up compositions. The
`Route` service decides who a message reaches; `ext-subagent`'s route
permits parent and child edges.

A conversation owns an ordered log, an inbox and a runner. Main sessions,
child agents and side threads differ in their runner and ownership, not in
their durability rules.

## One primitive

`Conversation<R, S = Persisted>` is built by the extension that wants it.
Every dimension that differs between uses is a type or a builder method
with a default, so an extension writes only what is different about its
conversation, and an impossible combination does not compile:

| Dimension | Types and methods | Default |
| --- | --- | --- |
| Runner | `Conversation::turn::<E>(purpose, prompt)` gives `R = Turn`: one model response per input, no tools, no agent loop. `Conversation::task::<E>(purpose, prompt)` gives `R = Task`: a registered agent kind's loop with inherited tools | |
| Storage | `S = Persisted` keeps an owned child log; `.ephemeral()` (turns only) gives `S = Ephemeral` | persisted |
| Keyed reuse | `.key(k)` (turns only): a repeated key returns the committed result; a task is its call's, once per purpose | the owning call |
| Thread | `.per_call()` (turns only) scopes the thread to the owning call | the session |
| Context | `.owner_context()`: the owner's conversation before the owning call. Turns project it per input; tasks copy it as a prefix image | fresh |
| Bounds | `.bounds(exchanges, bytes)` (turns only), `.max_tokens(n)`, `.budget(b)`, `.depth(n)` (tasks only) | 8 exchanges, 64 KiB; 4096 tokens, 60 s; depth 8 |
| Routing | `.fast()`, `.role(ModelRole)`; `.kind(name)` (tasks only) selects another registered agent kind | the session's model; the current kind |
| Placement | `.placement(Placement::Here)` | here |
| Execution | `.ask(cx)` awaits the committed result; `.detach()` gives a turn's `Background` observer, `.detach(cx)` a task's `ConversationHandle` | |

There are no presets: title, compact, judge and descriptions are each a
few builder calls in their own crate. Title is
`turn::<Title>("title", prompt).fast().owner_context().bounds(2, 8192).max_tokens(32)`,
detached under the key `initial` on its first model call and asked under
the owning call's key by `/title`. A sub-agent is
`task::<Subagent>("task", prompt)`, plus `.owner_context()` for
`fork_task` and `.detach(cx)` for `background_task`.

Compile-fail tests in `ext::conversation` hold the impossible combinations:
an ephemeral task (an agent loop runs its tools from its own log), a keyed
task, an agent kind on a turn, ephemeral twice, a question scope on an
awaited turn, awaiting a `Background`, and polling a `ConversationHandle`.

The engine implements one service, `ext::Conversations`, over the admitted
forms the builder lowers to: `ModelTurn` for a turn and `Spawn` for a task.
Neither has a public constructor, so the typed builder is the only way in.
`Placement` is `Here` or `Machine(MachineId)`, and the engine admits only
`Here`: any other is refused at admission as `SpawnRefusal::Placement`,
before a child exists. Each daemon stores its own sessions, so the model for
a conversation placed on another machine is fixed now, before it runs:

- the child is a root session in that machine's daemon's store, owned by
  the parent's `protocol::Address` (machine and session), never nested
  under `children/` here, and its id is that daemon's to make;
- the parent records the spawn as a durable intent and then its outcome,
  keyed by the call's `EnvelopeId`, as for any external call that is
  unsafe to replay: recovery settles an intent without an outcome as
  interrupted rather than spawn twice, and the remote daemon admits a
  repeated envelope id once;
- what crosses machines is envelopes, so `Envelope::destination` becomes an
  `Address` when the first remote placement runs; the kernel's one child
  wait per call is unchanged.

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
for roots and descendants. Forking a child at rest is how it is promoted: the fork is
a root with a copied history, owned by no call, which runs on and never
reports to its old parent. No transcript depends on an ancestor file remaining
present. Import publication syncs the containing directory and each newly
created ancestor before acknowledging creation.

## Admitted requests

A task's `Spawn` contains a prompt, context, runner selection, typed budget
and placement. Context is `Fresh` or `Owner`; an owner prefix runs through
the owning call's sequence, which must be committed. Runner selection
consists of an agent name and a model role (session, fast, or an
extension-defined role resolved by the model service). Resolve and freeze
the concrete routing and extension generation in the spawn intent. Future
configuration changes must not change admitted work. Persist admitted
authority separately from the public request, using a compact tuple codec. A
shared protocol image envelope exposes context to transcript and publication
reducers without requiring them to decode engine authority.

An awaited task returns its `Outcome`; a detached one returns only a durable
conversation handle after admission. A detached handle is not a future, and
`ConversationHandle::send::<E>` is how its owner messages it. Modes compose
these in tool implementations; no config flag selects execution semantics.
An extension namespace and purpose name allow several children per call
without positional identities. Calls can await several purposes in sequence
or admit several detached children. The current kernel has one active child
wait per tool call; overlapping awaited requests receive a typed refusal
instead of replacing a live waiter. This initial limit keeps the existing
wait lifecycle singular; a multi-watch join is follow-up.

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
the child's context. The spawn envelope stores a flat typed `ContextImage`
separately from frozen execution authority. Flat typed layers own their inputs'
sequence spaces and runner kinds; explicit anchors refer to other layers.
Only anchor indices are rebased when images are composed. Positional records
keep decoding in the protocol crate without field-map visitors. No inherited
envelope or encoded ledger entry survives composition. These inputs are never
installed in the child's executable log. Unfinished tool intents and queued
inputs are excluded, and unmatched provider exchanges
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
call free to finish: its `fired` fact closes the watch, and its result waits
on the parent's queue as the parent's own `child` envelope, taken up at a
step boundary (see "One queue").
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

### One queue

A conversation has one queue, the ledger's untaken items in arrival order,
and everything that arrives for its agent waits on it: the user's input,
other sessions' envelopes, and the session's own envelopes. Each item has a
`Source`: `User`, `Agent(session)`, `Job(call)` or `Child(watch)`. The
user's input is the queue's visible part, which every client's view folds
and shows; the rest is hidden, shown at most as a status. A hidden item
taken up adds no user item to the view.

What the session started and stopped waiting on comes back as an envelope
it sends itself, `EnvelopeId { sender: self, call, purpose }`, deduplicated
by its id as any envelope is: `child`, under the watch, for the result of a
child no call waits on (its `fired` fact closes the watch first, and a load
queues a fired result whose envelope a crash cut off); `detached`, under the
call, for a detached call's outcome; `job`, under the call, for the end of
a process group the call left to the session, holding (`Envelope.held`) the
group's output stream, sealed into a blob as the group ended. A payload of
the first two is the `Outcome`; the model reads the child's result, or the
call's output as its tool presents it.

`Withdraw { input }` takes the user's queued input back before it is taken
up; editing it is a withdrawal, then the new input. Whichever of `withdraw`
and `take` the kernel logs first wins, and the other is refused or finds
nothing, so every client folds the same queue. Only visible items can be
withdrawn by a client.

### Where the queue drains

The kernel keeps only the boundaries at which another message can join the
conversation without coming between a reply's calls and their results:
`ToolDone(call)`, after a model call's tool result, where what is taken goes
into that result (`take { into: call }`); `BeforeModel`, where what is taken
is read at the request's tail, so its prefix stays cached; and `TurnEnd`,
where what is taken starts the next turn. At each, the pure `dequeue` loop
point answers a `Take` (`None`, `One`, `All`, `These`, or `IntoResult`, the
one answer that takes anything at `ToolDone`). With no handler,
`Take::default_at` answers as the kernel always has: nothing into results;
before a model call, steers, children's results, and the hidden items not
behind the user's queued input; at a turn's end, the first item, but never
a job's end (`Queued::notice`): that is news for the next model call, which
starts no turn, keeps no session busy and leaves one at rest. A policy
(all at once, one at a time, into each result) is a handler; a model-only
runner answers one input per turn, so only the user's input may join it
before its end. Owner cancellation drains the queue; it is not a policy.

### Detached calls and forced steers

`Cx::detach` is a flag beside `cancel`: the session stops waiting on the
call. A tool declares `DETACH = Detach::Background` when it can go on in
the background, and its calls get the flag (`Some`), but for one another
call invoked, which that call waits on however long it runs. The flag is
set by a forced steer, or by the call itself: the shell sets it as its
timeout passes, so a timeout and a forced steer are one path. The engine
then stops awaiting the call, and the kernel logs
`detached { call }` and answers the call at once with a placeholder result
naming its output stream (`OutputStream { call }`; the blob store names the
stream's growing blob once it exists). The reply's calls stay paired with
their results, and the turn goes on. The call's real outcome arrives later
as its `detached` envelope.

A detached call is the session's work: the session stays busy, so the
daemon neither idle-exits nor finishes an upgrade drain while one runs. An
interrupt cancels detached calls, and its `interrupt` entry ends them:
nothing of theirs is queued, so they start nothing. A restart answers a
detached call whose placeholder a crash cut off with it, then runs the call
again if its tool is `Replay::Safe`; otherwise its stream is ended
(`Work::EndStream`) and its outcome, once the kernel hears back
(`Event::StreamEnded`), is an interruption saying it may not have
completed, holding the blob its output so far was sealed into. Either way
the call is recovered once. A detached call's outcome taken up
as a turn ends reaches the agent as `Input::Detached`, as a child's does as
`Input::ChildDone`.

An upgrade makes way for the newer build as soon as it can, and until then
is pending: the older build goes on serving, every client of the newer one
is welcomed with what holds it (`Welcome.upgrade`) and says so, with `a
daemon stop` to end it, and each pulse tries again. It makes way at once
when no work runs. When what runs runs only in the background (detached
calls, and the process groups sessions own as the user's work), it is
handed to the newer build live, if every such call's tool and every group's
is `ext::Resumable` (the shell is, `r.resumable_tool(x)`) and the newer
build declares the handover format this one speaks (`runtime/<id>/handover`,
`sys::handover::VERSION`). Helpers an extension starts again when needed,
owned as `Role::Helper` (an MCP server), hold no upgrade and no session at
work: they are stopped, and started again on the next call that needs them.

A handover is one commit across every session. No session loads, wakes or
is made while it runs. Each session freezes first: its groups' supervision
stops between two chunks of output, and the session takes nothing more up,
so no call can end and no turn start meanwhile. A session that refuses, or
a group that ends meanwhile, thaws them all, and a later pulse tries again;
only once every session froze does each give its groups up for good, and
what its clients asked meanwhile is refused (`Rejection::Draining`), to be
sent again to the newer build. Every client says why an upgrade waits
(`Notice::UpgradeWaits`). On Unix the daemon then execs the newer build in its
own process (the same pid, session and stderr, its listener, lock and
children kept, the shutdown signals blocked across the exec); on Windows it
starts the newer build inheriting each group's job, leader and output pipe
through an explicit handle list, under the start lock, and exits once that
build serves. The newer build claims every group the record names as it
reads it, adopts each through the tool that started it, and kills whatever
it does not adopt: a call goes on from where its stream is, as
`Work::Adopt` (never recovered as cut off), its outcome still to come, and a
job's end is still reported, holding its output. Nothing of a handover is
logged in a session. See `crates/engine/src/handover.rs`.

`Input { force }` on a steer brings the turn's next boundary on now: the
model call streaming is cut short (its partial reply is dropped, and the
steer is taken as the turn goes on), and every running call whose tool can
go on in the background is detached. Force-steering and detaching are one
mechanism.

The shell's timeout is the same mechanism on a clock: a command still
running after it goes to the background rather than stopping, and only a
call another call invoked is stopped there. A call's `timeout` (1 to 600
seconds) overrides the default.

```toml
[ext.shell]
timeout = 120  # seconds a command is waited for before it moves to the background, 1 to 600
```

### Routing and transport

Messaging uses an envelope with a durable id `(sender, call, purpose)`, sender,
destination and payload. A sender names a `Recipient`: a conversation, its
parent, or a name its route gives meaning (a channel, a group); `Recipient::send`
sends to one, and `ConversationHandle::send` to a conversation. Exactly one
extension provides `Route`, which resolves the address against the sender's
`Topology` (its id, parent and children) to the recipients, or a typed
refusal; with no route, nothing is sent. `ext-subagent`'s route permits
parent and child edges. Broadcast, channels with membership as ordered facts,
work queues that record one claimed recipient, and routing everything
through the parent are routes of other extensions over the same envelope,
composed at build time.

Send commits `outbox { envelope_id, recipients, payload }`, the recipient
set resolved once, before any delivery: a later subscriber does not receive
an old broadcast. Each recipient is delivered an envelope under the one id.
The receiver commits enqueue and deduplicates by envelope id before
acknowledging; the sender records `ack { envelope_id, recipient }` per
recipient, and the send settles once every recipient answered, with the
first refusal if any. Recovery resends to the recipients not yet
acknowledged, with the same id; this is at-least-once transport with
exactly-once logged enqueue. Receiver sequence numbers define order. Each
sender serializes its durable outbox; concurrent senders are ordered by
receiver admission, not wall clocks.

The receiver records `Take` with the enqueue identity at a step boundary,
before the input influences an agent or request. An agent's model reads a
taken message as `<message from="<sender>" channel="<purpose>">`; a
model-only thread reads its payload as the turn it asks for. Folding the log reconstructs
both queued and consumed input. Messages arriving during a provider request
wait for its next step. Context edits cannot undo delivery. Forks copy consumed
context but do not replay outbound effects; queued external messages require
an explicit fork policy and default to refusal. Inbox admission bounds what
other sessions queue to 128 envelopes and returns a typed refusal rather
than silently dropping input; the session's own envelopes are bounded by its
calls.

A question is a message carrying a correlation id and a reply address. An
answer references that id. An awaiting question uses a durable wait; an
asynchronous question is an ordinary inbox exchange. Parent/child and sibling
messaging use identical envelopes. An ext crate owns access policy, channel
membership and question presentation; the engine only validates the issued
routing capability and performs durable transport. Cyclic waits need explicit
cancellation or deadlines, not a hidden scheduler heuristic.

## Turns

A turn runs on `Runner::ModelOnly`: one response per inbox input, with no
tools, agent-loop hooks or recursive recovery handlers. A provider tool
request cannot become executable work. Its frozen spawn authority uses the
same registration intersection, settings, source-generation restoration,
model role, routing and depth allowance as an agent child. Every turn,
persisted or ephemeral, makes its provider attempts through the same
executor (`run_side`), under the same bounds, retry decisions, durable
barriers and cancellation; only a persisted turn goes through a child's
inbox.

### Identity and keyed reuse

`ThreadKey { extension, purpose, owner_scope }` identifies the owned
conversation: the extension's name, its purpose, and the session or the
owning call. Each request carries a `TurnKey` independent of the prompt,
budget and route. Repeating that key reuses the committed result, even after
restart or derived-index eviction. Concurrent consumers share admission and
result. A different key explicitly authorizes a new response in the same
thread, which appends an inbox input and sees the previous exchanges.

A thread has a conversation id derived from its current session owner and
thread key, through the existing UUID namespace derivation. Copied audit
history carries no executable child id; a fork starts its own thread with
fresh exchanges and cannot address a source-owned log.

### Context and bounds

`Fresh` supplies only the extension's messages. `Owner` records the owner
sequence strictly before the owning call; execution projects that immutable
prefix once as a per-turn input, retaining rich messages and opaque provider
state. Only extension instructions and replies enter thread history; owner
prefixes never accumulate there. Projection accepts an owner image coupling
that prefix to its own admitted registry, including registrations excluded
from the child.

The turn's bounds, a recent-exchange window and an assembled message-byte
ceiling, are frozen with the runner at admission; configuration cannot
select them. The admitted kind's system prompt and the current input always
survive. Oversized mandatory instructions fail with `InvalidRequest` before
provider execution. Systems belonging to older exchanges expire with those
exchanges. Optional context is removed in whole exchanges. Bounds never
prune durable exchanges or keyed outcomes.

### Persisted and ephemeral

A persisted turn's inbox input is enqueued, deduplicated and taken durably
in its child before influencing a request. Concurrent new turns are
processed in order, without merging or steering the active turn. The child
stores its own instructions, attempts and replies.

An ephemeral turn writes no child log and keeps no thread history. Its
admission, the four-request limit, its bounds, timeout and retries, and its
keyed result are all in the owner's log, exactly as for a persisted turn, so
a repeated key is still answered without spending again. It runs in the
owner, from authority frozen at its own admission, and its provider attempts
are counted as the owner's side work. Its deadline is raced around the
attempt itself, so a timeout settles that attempt with unknown usage; an
interrupt settles the turn in the owner's log at once, and the attempt it
cut off is settled with that same entry. An ephemeral turn never becomes a
thread's first turn, so a persisted turn after it in the same thread freezes
its own authority.

### Durability, interruption and limits

The parent's side `ModelCall` records the thread identity, frozen authority
and turn payload. `ModelResult.for` classifies the result as side work even
if edits remove its intent. Neither parent audit facts nor replies become
main model context; normal extension edits can change that context.

Admission commits before any provider is polled. Model attempts and retry
decisions also cross durable barriers. A consumer is released only after the
parent result commits; failed commits cannot acknowledge success. Open parent
requests and ambiguous attempts recover as interrupted, including crashes
during backoff. For every owned conversation, recovery terminally resolves
inbox inputs only with positive evidence of a matching sender request's
durable interrupted result. `CancelledInput` carries the owner and fact
sequence justifying resolution. Missing owners, open requests and absent
results do not prove abandonment. Existing take and interrupt facts
terminally consume abandoned inputs without removing their dedup identities,
even before a provider attempt exists. Recovery never repeats ambiguous
spend; an extension may continue the thread under an explicit new key.

`SideRetry` owns retry classification and backoff. Known failures may retry
with the same frozen input, after a committed decision. They do not enter
main `RequestError` handlers, so an overflow while summarizing cannot
recursively compact. Typed failures retain provider classifications without
putting prompts, credentials or provider bodies in diagnostics.

Each owner admits at most four active turns. Excess work receives a logged
typed failure. Each turn has a positive output-token limit and a timeout
covering inbox wait, provider attempts and backoff. Output tokens are capped
per attempt; these allowances are not aggregate currency caps.

A successful owning call leaves a detached turn running; `while_waiting()`
scopes one to the owner's current question instead, so answering it cancels
the work. Owner failure, explicit interruption, generation retirement and
shutdown cancel outstanding work. Cancellation wins over late success.
Dropping a consumer future does not abandon admitted work.

### The turns that ship

Title's first model call detaches a turn under the session-wide `initial`
key. `/title` regenerates it, and `/title <instructions>` supplies naming
instructions; each command is a turn keyed by its owning call. Client views
show the latest successful title. Compaction uses a session-scoped thread
keyed by the owning call; the judge is an ephemeral turn keyed by its call.
Compaction asks with the
owner's context, adds the user's `/compact <instructions>` to its summary
prompt, and records its completion mark after its context edit. Its
optional `threshold` is a percentage of the request's model's context
window, from the model registry (`llm::models::context_window`); a model
the registry does not know compacts only when its provider reports an
overflow, and the daemon log records `threshold_unmeasured` once per session
when that happens:

```toml
[ext.compact]
threshold = 80  # percent of the model's context window, 1 to 100
```

Approval descriptions are detached, ephemeral per-call turns scoped to the
question.

`SideProvider` is an optional test transport supplied by `ext-fixtures`.
It runs inside the same model-only executor, barriers and cancellation as
real providers (and a persisted turn's inbox); release builds exclude it. Tests reuse recorded provider
cassettes without editing them. Protocol envelopes identify runner kind and
thread ownership; the wire corpus and view vectors are generated by their
tests under the new `protocol::PROTO`, without compatibility adapters.

## Long tool results

`ext-spill` shows a tool result whose model text is over `limit` bytes as
its first `head` and last `tail` bytes around `<truncated bytes=… path=…/>`,
the path a file holding the whole text (design.md, "Blobs"). The log holds
such a result as that blob alone. A result that holds its output already,
as the shell's does, is left as its tool shows it.

```toml
[ext.spill]
limit = 4096  # the longest text shown whole, in bytes
head = 2048   # at most this much of a longer one's start
tail = 2048   # and of its end
```

Changing `head` or `tail` changes what later requests show of results
already logged.

## Metrics

`ext-metrics` folds the engine's generic execution metadata for all daemon
conversations by `AttemptId { executing_session, call, attempt }`. The driver
takes the monotonic start in memory when it sends the request. Execution
metadata rides on the existing result, retry or interruption settlement in
the same commit; recording metrics adds no entries or flushes. Facts freeze
the resolved model, role, runner, agent, side classification, wall-clock
correlation and monotonic clock epoch.

Unsettled intents retain unknown usage and duration per attempt, even after
a later retry succeeds. Attempt counts distinguish `executed` from `ambiguous`:
a crash cannot prove whether an intent was sent. No start fact is recorded.
Cancellation after sending settles one executed attempt with measured duration
and unknown usage. Usage counts are captured in `llm` before reply
serialization, including failed finishes; missing counts remain unknown. No
elapsed duration is reconstructed by subtracting clocks across a restart.

`/metrics` queries the current ownership subtree. Its optional JSON argument
is a price table `[version, currency, models]`, where `models` is a list of
`[model, rates]` pairs sorted at query admission. Each rate array has ten signed
nanocurrency rates in `llm::Count::ALL` order. Input/output counts include their
subsets, so cache rates are adjustments to the inclusive input rate. Every
nonzero coefficient requires an observed count. No built-in price is assumed.
A recorded charge in the selected currency takes precedence. Missing rates,
counts or duration increase `UsageTotal.missing`; `known` is only the observed
portion.

Own totals include only attempts executed in that conversation. The log
service selects subtree ids by following immutable ownership edges once;
aggregation reuses those ids. Copied fork facts and cached replies add no
spend. Projection API filters select agent and side/main dimensions without
changing the facts. Critical elapsed time is the envelope of measured intervals
within each clock epoch, separately from summed call durations. Multiple epochs
and unfinished intervals report incomplete coverage; wall clocks never fill
the gaps.

Disposable in-memory folds retain conversation/sequence/epoch cursors. Reads
seek to the covered line, validate it and decode only the suffix; replaced
coverage resets the fold. Prefix and suffix reads share line validation.
Copied history is excluded at the history-policy boundary. Unrelated entries
advance the cursor without changing accounting. Queries aggregate cached facts
with the supplied versioned prices; results are not cached. Repricing changes
no execution fact. No durable accumulator is maintained; query replies are
ordinary tool outcomes and are never counted as execution.

## Layer boundaries and future modes

The kernel remains a pure lifecycle reducer: ordered inputs, waits, watches,
terminal results and commit requirements. It does not select models, discover
extensions, open mailboxes or implement a task tool. The SDK owns request,
handle, budget, context and inheritance types. The engine owns identities,
barriers, routing, storage, registry generations and cancellation. Extension
crates choose modes and policy.

| Mode | Composition or contract work |
| --- | --- |
| Fresh awaited task | `Conversation::task` asked (`ext-subagent`) |
| Current-prefix task | A task with `.owner_context()` |
| Detached task | A task's `.detach(cx)`, returning a conversation handle |
| Parent sends while child works | `ConversationHandle::send` |
| Child asks parent | Tool composition: question and correlated reply fold in an `ext-*` crate |
| Sibling chat | Policy and tool composition over durable inboxes addressed by conversation id |
| Named channels | A `Route` provider resolving `Recipient::Named`, with membership as ordered facts of its own; recipients are recorded on each `outbox` |
| Different agent/model | `.kind(name)` and `.role(role)`; a turn or a task |
| Recursive same agent | Tool composition: inherit task registration; depth budget descends |
| Root-only extension | Rust declaration or Lua `a.inherit(false)` excludes the whole generation |
| Resumable/forkable children | Common store/catalog paths and existing fork policy |
| Model-only side threads | `Conversation::turn`: keyed, bounded, persisted or ephemeral |
| Conversations on other machines | `Placement` beyond `Here`: a root in the remote daemon's store, owned by address |
| Usage/cost views | `ext-metrics` derives totals from execution settlements and ownership edges |

Implementation must prove replay identity, terminal-result uniqueness across
cancel/reload, inbox order and consumption, inheritance exclusion, exact prefix
context and depth exhaustion with deterministic invariant-named tests.

Open product questions: whether channel names are root-local or workspace-wide;
whether detached work should ever outlive explicit parent cancellation; and
whether a future fork should accept pending inboxes by explicitly discarding
or cloning them. Initial semantics above are root-local, owned cancellation,
and refusal respectively.
