# Model-only side threads

Extensions use `ext::Models` to continue owned conversations outside their
main transcript. Title, compact, policy judge and approval descriptions use
this service. It admits `ModelTurn` requests onto `Runner::ModelOnly`;
there is no separate one-shot provider path.

## Identity and types

`ModelTurn::new::<Kind>(thread_key, turn_key, role, context, messages)`
constructs an awaited request. `models.call(&cx, turn).await` returns only
after the result commits. `turn.background().start(cx)` creates an observer
reaction instead. `Background` cannot be passed to the awaited API and
exposes no model-result future. `while_waiting()` scopes optional annotation
work to an unanswered question.

`ThreadKey { extension, purpose, owner_scope }` identifies the owned
conversation. `ThreadKey::new::<Extension>(purpose, owner_scope)` supplies the namespace
and requires an explicit session or call scope.
The caller supplies a `TurnKey` independently of the prompt, budget and
route. Repeating that key reuses the committed result, even after restart
or derived-index eviction. Concurrent consumers share admission and result.
A different key explicitly authorizes a new response in the same thread.

A thread has a conversation id derived from its current session owner and
thread key. Copied audit history carries no executable child id; a fork
starts its own thread with fresh exchanges. UUID namespace derivation uses
the existing UUID dependency rather than a second identity mechanism.
Its frozen spawn authority uses the same registration intersection,
settings, source-generation restoration, model role, routing and depth
allowance as an agent child. No configuration selects runner semantics.
Model-only runners have no tools, agent-loop hooks or recursive recovery
handlers. A provider tool request cannot become executable work.

## Context and execution

`ModelRole::Session`, `Fast` and extension-defined roles resolve through the
admitted registry and settings. Thread admission freezes the concrete route;
later settings or generation changes cannot widen it.

`SideContext::None` supplies only the extension messages. `BeforeOwner`
records the owner sequence strictly before the owning call. Execution
projects that immutable owner prefix once as a per-turn input, retaining
rich messages and opaque provider state. Only extension instructions and
replies enter thread history; owner prefixes never accumulate there. Its inbox
input is enqueued, deduplicated and taken durably before influencing a
request. Concurrent new turns are processed in order, without merging or
steering the active turn. Live and recovered context use the same reducer.
Projection accepts an owner image coupling that prefix to its own admitted
registry, including registrations excluded from the child.

`ModelOnlyKind::CONTEXT` declares a recent-exchange window and an assembled
message-byte ceiling in source. Admission freezes it with the runner;
configuration cannot select it. Title keeps two recent exchanges and at most
8192 serialized message bytes, including owner context. The admitted kind's
system prompt and the current input always survive. Oversized mandatory
instructions fail with `InvalidRequest` before provider execution.
Systems belonging to older exchanges expire with those exchanges. History ends
before the admitted input; keyed and plain inputs are explicit current
instructions. Optional context is removed in whole exchanges. Request
bounds never prune durable exchanges or keyed outcomes.

The parent's side `ModelCall` records the thread identity, frozen authority
and turn payload. `ModelResult.for` explicitly classifies the result as
side work even if edits remove its intent. The child owns provider attempts
and committed responses. Neither parent audit facts nor side replies become
main model context; normal extension edits can change that context.

## Durability, interruption and limits

Admission commits before any provider is polled. Child model attempts and
retry decisions also cross durable barriers. A consumer is released only
after the parent result commits; failed commits cannot acknowledge success.
Open parent requests and ambiguous child attempts recover as interrupted,
including crashes during backoff. For every owned conversation, recovery
terminally resolves inbox inputs only with positive evidence of a matching
sender request's durable interrupted result. `CancelledInput` carries the
owner and fact sequence justifying resolution. Missing owners, open requests
and absent results do not prove abandonment.
Existing take and interrupt facts terminally consume abandoned inputs
without removing their dedup identities, even before a provider attempt
exists. Recovery never repeats ambiguous spend.
An extension may continue the thread under an explicit new key.

`SideRetry` owns retry classification and backoff. Known failures may retry
with the same frozen input, after a committed decision. They do not enter
main `RequestError` handlers, so an overflow while summarizing cannot
recursively compact. Typed failures retain provider classifications without
putting prompts, credentials or provider bodies in diagnostics.

Each owner admits at most four active side requests. Excess work receives
a logged typed failure. Each turn has a positive output-token limit and a
timeout covering inbox wait, provider attempts and backoff. Output tokens
are capped per attempt; these allowances are not aggregate currency caps.

A successful owning call leaves background work running. Owner failure,
explicit interruption, generation retirement and shutdown cancel outstanding
work. Cancellation wins over late success. Dropping a consumer future does
not abandon admitted work. A persistent thread can accept explicit new work
after cancellation has drained its old inbox.

## Extensions and protocol

Title's initial background turn uses the session-wide `initial` key.
`/title` regenerates it; `/title <instructions>` supplies naming instructions.
Each explicit command appends a turn keyed by its owning call. Client views
show the latest successful title. No duplicate title fold is maintained.

Compaction and judge use session-scoped threads and owning-call turn keys.
Compaction records its completion mark after its context edit. Approval
descriptions retain call-scoped threads and question-scoped cancellation.

`SideProvider` is an optional test transport supplied by `ext-fixtures`.
It runs inside the same model-only executor, inbox, barriers and cancellation
as real providers; release builds exclude it. Tests reuse recorded provider
cassettes without editing them. Protocol envelopes identify runner kind and
thread ownership; the wire corpus and view vectors are generated by their
tests under the new `protocol::PROTO`, without compatibility adapters.
