# Memory ownership and reclamation

Work in progress on `mem-release`, based on `8d2b9e8d` (paged protocol).
Reclamation is experimental: no keep decision or performance improvement is
claimed until the native paired cohorts finish. The protocol-37 census report
establishes a hypothesis, not a current-protocol result.

## Ownership changes

The actor and mailbox leave a lexical scope before its completion signal.
At the hibernation transition, an `AllocationScope<T>` takes the slot's retired
live state. Its fields drop that state before the allocator-release guard,
outside the residency mutex and before catalog publication. There is no timer, polling loop,
allocator-tunable change, or additional thread. The native operation remains
`malloc_trim(0)` on glibc, `malloc_zone_pressure_relief` on macOS, and
`HeapSetInformation(HeapOptimizeResources)` on Windows.

Model-only projection fields have one owner, replaced with its default value.
Durable invocation/execution state remains outside that owner. Immutable wire
frames convert their completed Vec into a boxed slice instead of calling
`shrink_to_fit`. Prefix conversion uses `Arc::unwrap_or_clone` to consume a
uniquely owned encoding; shared prefixes retain copy-on-write behavior. Blob
and history authorization/loading leases end at lexical scope boundaries.
Native ACL, COM folder, credential and LocalAlloc resources have private Drop
owners in sys.

## Audit

| Site | Previous/current pattern | Shape and disposition |
| --- | --- | --- |
| sys/memory; engine actor completion | Explicit actor/mailbox drop; allocator release existed only as a diagnostic operation | Experimental owning `AllocationScope<T>` at the slot transition; actor exits first, then retired live state drops before allocator guard and catalog publication. Keep decision pending. |
| engine/projection, driver and prefix/spawn paths | `release_model` reset five cache fields individually | Done: one defaultable `Model` owner, replaced through assignment. |
| protocol/Frame | Completed Vec followed by `shrink_to_fit` | Done: `Vec::into_boxed_slice`, shared through Arc; encoded bytes unchanged. |
| llm/conversation, Prefix → Resident | Always cloned the encoding behind an Arc | Done: standard conversion takes unique ownership with `Arc::unwrap_or_clone`; shared ownership still clones. Existing prefix invariant runs both ownership cases. |
| engine/fetch and history/answer | Explicitly dropped live/slot leases before paced delivery | Done: lexical scopes end leases before delivery. |
| sys/fs/edit Darwin ACLs | Three explicit `acl_free` sites and a cleanup-only closure | Done: private ACL owner; native error captured before destruction where errno matters. |
| sys/paths/windows known folder | Manual COM free on success/error | Done: private owner borrows the loaded deallocator; allocation drops before the library references. |
| sys/windows SID/security allocations | Manual SID-string LocalFree; separate descriptor Drop | Done: one private LocalAlloc owner for both resource kinds. |
| sys/credentials Windows reads | Copy native credential blob, then CredFree | Done: private credential owner, released by Drop. |
| sys/TransientBuffer and PageBuffer | Fallible clear/consume; retained address reservation; initialized heap scratch | Left: clear/consume are logical stream operations, not teardown. Unread/cancelled input must survive a read future. Drop already releases the reservation. Moving fallible discard into Drop would lose its error or discard unread tails. |
| protocol FrameReader/frame_heap, sys ReadBuffer | Borrowed read scratch commits through a guard; cancellation restores the initialized prefix | Left: already borrow-scoped RAII. Completion/consumption returns backing pages through the existing native seam. |
| llm SSE/native and heap buffers | Clear event/line/data at parser boundaries; preserve small scratch | Left: parser transitions are fallible and can retain a partial next event. Replacing buffers would lose the measured reservation/scratch reuse. |
| client/inbox segments | Full segments leave with work; one small head is reused | Left: ownership already releases drained burst storage. Default/take of the entire queue would discard useful small-traffic capacity. |
| client/inbox explicit guard/waker drops | Unlock before waking or dropping arbitrary callbacks | Left: deliberate early Drop is idiomatic synchronization; callback ordering is covered by the existing invariant tests. |
| engine/outbox registrations and queue state | Ownership transfer through take; explicit early guard drops; close/abandon are observable channel transitions | Left: these methods deliver cancellation/disconnection semantics. RAII alone cannot replace a transition while the shared outbox remains alive. |
| engine/history reset, llm writer/reset/close, decoder release/clear, metadata clear | Invalidate a generation, complete a protocol item or normalize a request | Left: semantic state transitions; not native-resource teardown. Drop already owns the underlying data. |
| llm Resident::retain and canonical reconstruction | Drop encoded canonical prefixes; reload canonical data only when requested | Left: the resident-history design intentionally separates encoded and canonical ownership. Retaining both would regress turn-residency. |
| sys ZeroedWords, native handles, CF objects, pressure sources, staging files | Existing Drop/OwnedFd/OwnedHandle owners or native callback destructors | Left: already RAII; OS finalizer callbacks are the foreign ownership boundary. |
| sys page-buffer reservation rollback | Explicit unmap while splitting an OS reservation | Left: native construction rollback before a PageBuffer exists; not an exposed begin/end API. No speculative mapping-error hardening added. |
| sys Linux post-fork descriptor closing | Raw close/exec/_exit path | Left: the child cannot run Rust destructors or allocate after fork. Parent resources already use OwnedFd. |
| sys child handover release | Give a shared child identity to the next executable image | Left: an exec ownership-transfer protocol, not freeing memory; changing it requires a separate handover design. |
| sys/runtime/image copyfile state | Copy result and fallible native free result are both preserved | Left: an infallible Drop alone would suppress the existing cleanup error. No new close/guard abstraction for this short linear fallible boundary. |
| engine JSONL file close before publication | Explicit drop of a standard File before rename | Left: required native publication ordering, already executed through Rust Drop; no manual native free. |
| client drive/remote and other app scopes | Early lock or stdin Drop before callbacks/EOF-sensitive waits | Left: standard ownership sequencing and observable I/O semantics; outside the inbox implementation ownership for this branch. |
| test-only early drops and native fixture cleanup | Deliberately trigger shutdown, EOF, cancellation, mapping release or foreign cleanup | Left: lifetime transitions are the behavior under test. No speculative panic-cleanup changes. |
| engine runtime-demand/context-image measurements | Explicit `sys::release` before/after snapshots | Left: diagnostic interventions distinguish retained from allocated memory; not production lifetime management. |
| CLI startup/sys::small_footprint | Existing glibc arena configuration | Left unchanged: startup policy is not reclamation; this experiment must not change allocator tunables. CLI startup is outside this branch's file ownership. |
| ext-index bigram filter | Builder slab Drop and sparse Vec shrink | Left: outside ownership; compaction also extends the sparse Vec. Box conversion is not a behavior-preserving local substitution. |

## Verification and measurements

Raw evidence, commands and logs are under the assigned
`scratchpad/r/mem-release` directory, outside the worktree. All local commands
use an allowlisted environment with dummy loopback provider settings,
`MISE_NO_ENV=1`, two Cargo jobs, and the single assigned target directory.
Fixture daemons use temporary isolated homes and their normal stop command.

The first native run passed 854 engine/protocol/llm tests. The expanded run
passed 1,051 sys/engine/protocol/llm tests (30 skipped measurement/live tests).
Native clippy and the workspace Windows lint passed. Final source checks and
release comparisons are still being completed.

Allocator census has not yet merged into origin/main. Instrumented native
cohorts and the final keep/drop recommendation depend on that merge. No
protocol-37 numbers are substituted for the requested current-protocol data.
