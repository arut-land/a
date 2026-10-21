# Side model calls

Extensions ask models through `ext::Models`. A side call belongs to the
session but is not an agent turn: its prompt and response never enter the
main transcript. Title, compact, and policy's judge use this service.

## SDK contract

`Side::new::<Extension>(purpose, role, context, messages)` constructs an
awaited request. `models.call(&cx, side).await` returns a committed response
or a typed `llm::Error`. A gate, recovery handler, or tool explicitly waits
by awaiting that future. `side.background().start(cx)` instead produces a
`Reaction` for an observer. `Background` cannot be passed to the awaited
API. Its result is available in the log and client view, not as a future
that could accidentally hold up the main turn.

The request offers no provider model id, tools, or mutable transcript:

- `ModelRole::Session` resolves the session's settings, including its
  `/model` override. `Fast` resolves `fast_model` in the workspace config,
  then the home config, then the session model. Extensions choose intent;
  configuration chooses a concrete provider and model.
- `SideContext::None` exposes only the extension's messages.
  `BeforeOwner` exposes the effective transcript strictly before the
  owning call, including its system prompt and prior context edits.
  The driver freezes this prefix when admitting the request. Later input
  and edits cannot change an in-flight request.
- The prompt and selected transcript are recorded separately in the
  intent. Execution appends the prompt to that prefix, retaining the
  provider encoding cache. Only normal extension outputs such as
  `cx.edit` can change the main context.

A durable identity consists of the registering extension's name, purpose,
and owning call id. `once_per_session()` removes the owner from that key;
use it for session metadata. Reusing a key means asking for the same fact,
even if the prompt or routing configuration has since changed. Distinct
operations need distinct purposes. The namespace comes from the extension
type, and registry installation already requires unique extension names.
Concurrent consumers of one key share its intent and committed result.

## Durability and recovery

The driver admits a request through the existing side-call kernel event.
`ModelCall` records the owner, purpose, resolved model, and an opaque
`request` payload encoded by the SDK: extension identity, role, context
selection, prompt, frozen transcript, lifecycle, key scope, and budget.
The intent crosses a barrier before the provider is polled.

`ModelResult` records the complete response and usage, or an outcome plus
an optional structured `llm::Error` in `failure`. The LLM crate owns error
decoding. The result crosses a barrier before the awaited consumer is
released. Duplicate consumers are released by durable publication. A
failed commit cannot acknowledge a result.

Loading a completed request returns that fact without calling a provider.
Loading an open request records `Outcome::Interrupted`: a process crash
cannot tell whether the provider charged for or completed a request.
Side calls therefore use the conservative, non-repeatable recovery class.
The owning safe tool or gate can run again, but its side request receives
the recorded interruption. No automatic recovery re-spends on an ambiguous
attempt. An extension can explicitly request new work under a new key.

Known failures are different from an ambiguous crash. `ext-retry` provides
`SideRetry` using the same retry classification, limits, provider delay,
and capped jittered backoff as its main-request handler. Before another
attempt, the driver commits a `SideRetry` decision containing the typed
failure, next attempt number, and delay. All attempts use the same frozen
request. Side retries do not enter the main `RequestError` handlers: an
overflow while summarising cannot recursively start another compaction.
A crash during backoff also records interruption instead of repeating.

No new kernel effect is needed. Side events carry lifecycle facts; existing
`Append`, `Commit`, `Reply`, `Side`, and `Cancel` effects provide durability
and ownership. Routing, request construction, provider execution, limits,
and retry policy remain outside the kernel. The additional retry event
only commits a decision and acknowledges its barrier.

## Limits, cancellation, and cost

Each session executes at most four side calls concurrently, counting
backoffs against that limit. Excess requests get an intent and a logged
`InvalidRequest` failure rather than an unbounded queue. A request has a
positive output-token limit (`NonZeroU32`) and a
wall-clock timeout covering provider execution, retries, and backoffs.
The token cap applies per attempt; retries can spend more than one cap.
This is not a currency or aggregate-token quota.

A side call has its own cancellation registration before execution starts.
Its owning call's cancellation stops awaited and background work; an
explicit interrupt also cancels open side calls. A successful owner may
finish before background work. That work remains registered with the
session, so shutdown cancels it even after its owner is forgotten. Dropping
a consumer future does not abandon an admitted request: the engine task
still records its terminal result. Provider futures run through the same
panic containment as main requests.

Clients already reduce purpose-labelled model calls to separate reply
items, each with its own usage and status. The main transcript excludes
these calls. The final provider response supplies usage; failed attempts
without a usage response cannot be assigned a measured token cost.
The log retains their typed failure and retry facts instead.

## Extensions and fixtures

Title requests `Fast` in the background with a session-wide key. There is
no separate `titled` mark that can commit before its request. Re-observation
or a later model call reuses the existing fact. It may complete after the
main response without keeping that turn waiting. Open side calls alone
do not make the session activity `Running`; the daemon still retains them
as work to cancel and drain on shutdown.

Compaction awaits `Session` with `BeforeOwner`, then writes the summary
through its normal context edit. It records its completion mark after the
edit, so a crash cannot leave a mark that suppresses an uncommitted edit.
A repeated edit is harmless, and the summary's side-call key prevents a
second model charge. Policy's judge awaits `Fast` with no transcript.

`SideProvider` is the SDK's optional alternate transport. The fixtures
extension supplies deterministic responses (`fixture <purpose>`), still
behind the real intent/result barriers and cancellation. Release builds
exclude this provider. Driver tests also replay the existing recorded
provider cassettes through `Flaky`; no provider wire fixtures are invented
or modified.

The wire format adds request and typed-failure payloads and the side-retry
verdict; its version lives in `protocol::PROTO`. Its wire corpus and shared
view vectors are generated by their existing tests. There is no
compatibility adapter for older logs.
