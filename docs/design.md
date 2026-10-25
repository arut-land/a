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

Commands that start or attach resolve an immutable installed image by content
identity in the executable data directory. Daemon status, log and stop operate
on the local home without touching executable installation. An unchanged source or installed image needs no
executable read, copy, flush or reference publication. First installation
verifies a private snapshot: reflink on Linux where supported, clonefile on
macOS with a metadata-preserving copy fallback, CopyFile on Windows preserving
alternate streams. Source files never share writable inodes with installed
images. Installation is crash consistent rather than durable: each image and receipt
is ordered before its name (a write barrier on Apple platforms, not a
drive-cache flush), and a lost image or reference is recovered on the next
start. Small installation receipts and source fingerprints survive image
reclamation, preventing old or relocated sources from being installed again.
macOS quarantine and code signatures are retained. Windows retires a mapped
old image under a private name; a later installation reclaims it after process
exit, without blocking replacement or adding cleanup work to ordinary starts.
A local
client first connects to the daemon's endpoint. On a cold start, clients
serialize through `start.lock`, recheck the connection, take the daemon
lock, then start a detached `a __daemon`. Unix passes the bound listener
and lock; the client connects into its backlog. Windows starts the child
with an explicit stdio handle allowlist and waits for discovery publication
under the start lock. Its locked daemon record selects a process-generation
pipe, so retained clients cannot reserve a successor's endpoint. Neither path polls for readiness. The daemon
holds its pid-file lock for its lifetime, excluding other writers to that
home. `sys::Paths` resolves native configuration, state, data, cache, logs
and runtime locations; `A_HOME` selects an isolated root. See [paths and
install layout](paths.md). Optional [autostart](autostart.md) registers
the daemon with the OS to start at login and stay up; clients still self-spawn
it when none is running.

The daemon serves connections and session actors on `rt`. Idle sessions
hibernate; the daemon exits when no clients or work pin it. Upgrades drain
work and restart the staged build; clients reconnect from their last seq.
Work left running only in the background (detached calls, the process groups
sessions own) is handed to the staged build live rather than waited out,
when every piece can go on there: on Unix the daemon execs that build in its
own process, children and listener kept; on Windows it starts it inheriting
the groups' jobs and pipes ([agents](agents.md), `sys::handover`). Until it
can, the upgrade is pending: the older build serves on, telling the newer
build's clients what holds it.
A typed crash breaker distinguishes repeated young deaths from a reboot.
Diagnosis and restart verify the recorded process identity before signaling.

An attach supplies the client's environment and working directory. Tools
use the session's environment rather than the shell that started the daemon.
The log records its redacted form; live secret values stay out of that log.
Environment refresh is an explicit client command. A client reaches a
daemon through a `client::link::Link`, over which one hello runs: this
machine's over its socket (`LocalLink`), another's through the system
`ssh` to `a bridge` there (`SshLink`), whose stdio carries the same frames
into a local connection, so the daemon side is unchanged. The remote login
shell is given only plain words: `sh -s` with a discovery script on stdin
on a POSIX host (any login shell), `a bridge <token>` where there is no
`sh`, honouring the host's `A_HOME` or native layout. A POSIX host that
lacks `a` receives this build only when `sys::Platform` recognizes it as
matching. A client with no one at its terminal (`a web`) runs ssh in batch
mode, and a host that would ask for a password is a typed error. Later
transports are further `Link` types.

Every home has a machine identity: `protocol::MachineId`, the first 128
bits of the SHA-256 of an X25519 public key whose secret `sys::machine_id`
makes the first time the home needs one and keeps in the state directory
(mode 0600; sealed with DPAPI on Windows; see
[the measurements](perf/machine-key.md)). The daemon reads it on the
blocking pool while it loads, and its welcome names it. A client of this
machine's daemon names no machine in its hello; one over another link names
its own home's, so the daemon knows whether a client shares its files. That
claim is unauthenticated (any process of this user may claim any machine),
which the trust rule allows: it attributes, it never decides. The client's
`Connection::machine` is the daemon's. What a connection may do
(`protocol::Role`) the daemon decides in one place, `server::role_of`, from
how the connection came, never from the client: trust is this user on a way
in that authenticated this user. The daemon's one way in is its endpoint,
which the OS opens to this user alone, and a client bridged over ssh comes
through it too, so every connection owns the daemon today; a way that does
not prove that (another person's pinned key, later) is a new `server::Way`
decided there. Every request is admitted once, by `Conn::admit`, before
anything acts on it: a viewer's that would write (a mutation, an
environment, a new or forked session, the settings, a shutdown) is refused,
and its attach keeps the session's environment. No key is exchanged yet.
The client's machine is not logged: it decides nothing a replay must
reproduce, and the log names a mutation's client already. `--host` is a
`client::Host`, which is never something ssh would read as an option. A
new key is flushed to the drive (`F_FULLFSYNC` on Apple) before any welcome
names its id, since a client may pin the id from then on, and on macOS it
carries the attribute `tmutil addexclusion` sets, so a home restored from a
backup onto another Mac is not a second machine of the same id.

Each daemon stores and names only its own sessions and paths, and a client
may hold several. A path the wire carries (`Environment::cwd`, an attach's
and a summary's `workspace`, a query's `Scope::Workspace`, an approval's
`OutsideWorkspace`) is a `protocol::HostPath`: the daemon's machine's own
spelling, which a client shows and hands back but never joins, compares as
a path or canonicalizes, since that machine may be another OS; only the
daemon reads it as a path (`HostPath::local`). A session elsewhere is a
`protocol::Address { machine, session }`, which routes and is never logged:
a daemon's entries need no machine, and a client learns each daemon's from
its welcome. The client SDK's `Daemons` is the set, one `Member` per
machine, with the names `machines.toml` and the ssh configuration's `Host`
lines give (each resolved by `ssh -G`, never parsed for meaning, and
watched with `rt::Watcher`); `mac2:3f9a` resolves on the client to a
member and the prefix it asks there, and `SessionWatch` merges the
members' lists. Every app holds a set of one today. See
[the client SDK](client.md).

See [the client SDK](client.md) and [CLI](cli.md), and the staging and
endpoint types in `crates/sys`. The daemon targets macOS, Linux and Windows.
Windows uses SID-restricted named pipes, overlapped completion events over
`rt`, atomic staging references and kill-on-close shell jobs. Its native CI
lane must establish runtime behavior; cross-lint alone is insufficient.
See [Windows platform details and validation limits](perf/windows.md).

## Session spine

### Log and store

The source of truth is `sessions/<id>/log.jsonl`: a header followed by numbered,
epoch-stamped entries. Every line is sealed with CRC32C. On a full scan, an
unsealed last line is a torn tail and is trimmed. Damage before the tail,
or a different log format, preserves the file and opens it read-only.
Sealed unknown entry kinds survive as `Body::Unknown`.

The engine's `LogStore` contract has a conformance suite for JSONL and the
test memory backend. A writer is moved into one session actor, never shared.
Local leases issue epochs above the loaded log's epoch; the daemon lock and
writer ownership enforce exclusion. This is not a cross-host fencing service.
The derived `sessions.idx` catalog lists sessions without loading their logs.
Child conversations live recursively below their parent in
`children/<id>/log.jsonl`; the catalog's ownership headers reconstruct id lookup
without scanning the directory tree on a clean start.

### Blobs

Bytes too large for a log line (cut tool output now; pastes and attachments
later) are blobs: content-addressed by SHA-256 (`protocol::BlobId`), owned by
the store beside the logs that reference them, one namespace per daemon. An
entry holds a `BlobRef { id, size, media_type, origin }`, never a path, so a
fork or child references its parent's blobs rather than its files. What an
entry references outside the log lives in one typed field and nowhere else:
`ToolResult.held: Vec<Held>` (`Held::Blob` or `Held::Stream`), filled from the
tool's `Tool::held(&Out)` (the one hook a tool adds; the output keeps at most a
marker, such as `truncated` or `{"detached": true}`), and `Envelope.held` on a
detached call's outcome or a job's end; and a tool's outcome itself
(`ToolOutcome`, `Outcome<Payload>`), whose value is either in its line
(`"output":…`) or, too large for it, the blob that holds it (`"blob":{…}`),
never both. Only a tool's outcome can be a blob, and so a session's result
(its last settled call's, which a child reports): a model's reply or a
query's is `Outcome<Json>`, so a blob there does not compile, and a line
that holds one does not decode. What an envelope carries is typed
(`Carried::{Message, Job, Child, Detached}`), never text to parse, and
fixes the purpose it is sent under and where it comes from: one under
another payload's purpose does not decode. A child's result held as a blob
is read back for its parent as a tool's is. `Body::blobs` lists every
reference an entry holds, wherever in it, a spawn's context image included:
what a projection must not name on the session's actor, and what a sweep
keeps.

The seam is the `blob` crate (re-exported as `ext::blob`), which does no I/O:
`BlobStore` (`writer`, `stream`, `open`, `materialize`, `sweep`),
`Materialize` (how blobs appear as files) and the shape of `Namespace` (what
that tree holds, derived from the logs). Calls reach them through `Cx.blobs`.
The file-system backends are `blob-files`, which the engine and the
extension test rig both run on.

A call's output stream (`protocol::OutputStream { session, call }`, what a
detached call's placeholder names, a background shell job's log, and the
output of a command whose leftover processes still write it) grows at
`blobs/open/<session>/<call>.log`, named by the session whose log holds the
call. It is sealed into a blob only once nothing writes it: a command that
leaves processes behind keeps its whole output as a growing stream, and a
process group the session owns (a background job, or what a command left
running) has its stream sealed as the group ends, into the blob its `job`
envelope holds (`ProcessLease::holding`). A group the session stops leaves
its stream discarded. A restart that cuts streams off ends them: a detached
call's that is not run again or handed over, and, on the first load after
the daemon starts, each group's left to the session whose end is not queued,
whose call is not open, and that was not handed over. The kernel asks
(`Work::EndStream`); the engine copies the bytes so far, cut at
`blob::STREAM_CAP`, into a blob on a storage worker, leaving the stream; the
kernel queues the call's interrupted outcome or news of the group's end
holding that blob (`Event::StreamEnded`; a group's news is logged even with
no stream, so it is ended once), and only once that is durable removes the
stream (`Work::DiscardStream`). A restart in between copies it again into
the same blob. The session counts a copy in flight as background work, so
it is not unloaded, and the daemon neither idle-exits nor drains, before it
is back. Copied, never sealed in place: a group a crash left running may
write the stream still. Calls an orphan drops have their streams removed
without a copy. Of what a recovered session's restart cut off, only a
stream that could not be read stays under `open/`.

References are named for the model by the engine when it reads them, not by
the tool and not when they are logged: the transcript adds a line for each
item a result or detached outcome holds, naming it through `blob::Places`
from what the log records, whoever's session projects it. A blob is named
`<cache>/tree/by-id/<id>` only once `Materialize::ensure` (idempotent, a stat
once made) has made it readable, so a path survives a restart or a deleted
cache; a stream is named by its path in the store if it is there. What cannot
be read is left unnamed. Pastes and attachments are named the same way. An
entry that holds something is projected on a blocking worker, never folded
into the live transcript on the session's actor. A projection without a store
names a blob by its tree place and a stream `stream:<call>`.

The JSONL backend (`blob_files::Files`) keeps `blobs/<aa>/<id>`, 0400 in a
0700 directory. A writer hashes into `staging/` as it writes and refuses bytes
past its cap; a stream, readable while a process writes it, seals into a blob
(cut at its cap) with no copy, and a call run again starts its stream over.
Sealing orders the bytes and the new name, then returns the ref: an entry
referencing it is appended after, so the shared flush epoch that makes the
entry durable covers the blob (a blob on another device is flushed at once).
Staging and unreferenced blobs are never trusted after a crash; `sweep` removes
them at rest given liveness derived from the logs, and keeps every stream.
Nothing runs it yet: there is no session deletion. Blob bytes are as private as
their log, and environment redaction does not apply to them.

`Materialize`'s only backend is `blob_files::CacheDir`:
`cache/tree/by-id/<id>`, cloned (APFS `clonefile`, Linux `FICLONE`), else
copied (Windows `CopyFile2`, which block-clones on ReFS and Dev Drive) into
`cache/staging` and renamed into the tree without replacing, read-only,
evicted least recently used first and rebuilt on demand. The tree holds only
whole files: eviction walks it alone, and what a crash leaves in staging is
removed before the next owner's first copy. Never a hard link: a write
through the cache never reaches the blob. Virtual backends (Windows Cloud
Files, macOS File Provider, Linux FUSE) would plug in behind the same trait,
answering from `Namespace` and hydrating via `BlobStore::open`.

Calls have durable intents and outcomes. Tool output is structured data;
model transcripts and client presentation are projections. Approval waits,
queued input, decisions, child links, process ownership and extension state
are facts in the log. Streaming deltas are ephemeral until an outcome or
progress entry records them. Essential facts that projections cannot rebuild
still need an explicit logging audit (see milestones).

Input is logged as segments: typed text, pastes, and pastes and attachments
kept by reference, so a client shows a paste as a placeholder and the model
reads it tagged. Whatever a model reads that no person typed is an
`llm::Tag`, made by one renderer: `<pasted-content>`, `<steer from=…>`,
`<message from=… channel=…>`, `<background-result call=… command=…>`,
`<truncated bytes=… path=…/>`, and
another model's `<reasoning-summary source=…>` or `<reasoning source=…>`
([llm.md](llm.md)). What the model is shown of a call's outcome is a pure
point, `present_result`, which defaults to the tool's own `to_model`; the log
keeps the outcome, its payload in its line or in a blob. Its handlers are given what the result holds,
already named (`ext::Placed`), so they do no I/O.

A result too long to show whole is spilled, for any tool, by `ext-spill` in
two steps. The `ext::Record` service is asked once as a call ends, with its
`Cx`, before the result is logged: the one place a result may be changed with
I/O. `spill` moves a result whose model text is over `limit` (4 KiB) into a
blob, and the outcome is logged as that blob (`Payload::Blob`, origin
`ToolOutput`): the log line holds the reference alone, and the blob is
durable before the entry. The blob is the model text itself (`text/plain`,
or the JSON the model reads as `application/json`), so the bytes the ends are
cut from are the file the model greps. A result that holds something already
(the shell's own cut output) is left as its tool made it, and a call another
invoked is never recorded: its invoker reads the output as the tool gave it
(`Toolset::call`, against `Toolset::run`).

Recording comes before the kernel's `post_tool` chain, which keeps the kernel
pure (no I/O between the chain and the append) and keeps one value: the
chain sees the payload as it is logged, and a rewrite replaces it, the
original (by reference) logged with the decision. Of a spilled result,
`post_tool` sees only the reference, never the bytes. The live transcript
stops short of an entry that references a blob and goes on from there on a
blocking worker as the next request is made, so each blob is read once and
never on the actor. The transcript reads a
payload's blob back through `Places::read` and names it through
`Places::blob` before the presentation is asked (`ext::Stored`), so the
default `present_result` shows the whole text and spill's handler shows its
first `head` and last `tail` bytes (2 KiB each, cut at a near line end,
never inside a character) around `<truncated bytes=… path=…/>`; the
transcript names every reference in `held` itself. The cut is a function of
the blob's text and settings alone, so every projection shows the same bytes
and a provider's prompt cache holds; where the blob cannot be named, the
tag names no path. A result held as a blob with no card is shown as what
it is and its reference (`Shown::Held`); a client that wants the bytes
fetches them by that reference (below, the wire). The apps do not fetch
yet. Its settings are in [agents.md](agents.md).

### Kernel and driver

`kernel::Core::step(Event) -> Effects` is synchronous, does no I/O and reads
no clock. The driver supplies outcomes and time as events. The kernel
numbers and folds appended entries into the ledger using the same fold as
load. A model request is prepared through extension points, the decision
is committed, and only then is the request sent.

One async engine actor per loaded session carries out those effects and
feeds results back. Each running call has its own in-flight state. Tools
declare shared or exclusive concurrency; shell commands remain exclusive. Blocking
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
the ledger and extension fold images to `sessions/<id>/derived/log.snap`.
A neighboring `.idx` records line offsets. A snapshot must match the build
and extension set, and its covered entry must agree with the log. Otherwise
load folds the whole log. Loading validates the covered prefix's seals and
ordering before restoring the image: a snapshot saves folding that prefix,
rather than reading and decoding it. `derived/` is disposable. See
[session measurements](perf/sessions.md).

`Core::resume` handles recovery after load, failed commit, registry reload
or contained panic. A fresh agent per turn makes its state reconstructible
from the log. Safe tools can rerun; unsafe tools settle as interrupted rather
than repeating an uncertain effect. Logged process numbers are diagnostic data;
recovery cannot turn them into signaling authority. Unix live leases keep their
leader unreaped through group cleanup; Windows leases own kill-on-close jobs
assigned and verified before the primary thread runs. Unix group control covers
cooperative descendants; descendants can leave the group. Durable waits
and child ownership let interaction continue independently of a connection.

Process creation uses a declarative plan: native spawn file actions and default
close-on-exec on macOS, a dense descriptor allowlist closed with glibc's
closefrom/close_range actions on Linux, and Unicode environment plus explicit
handle and job lists on Windows. The musl backend applies the same fixed plan
through async-signal-safe child syscalls, using close_range or an old-kernel
bounded close loop when a descriptor inventory is available. The close_range
path does not require procfs; without either facility, musl launch fails closed.
Executable and working-directory paths are made absolute before spawning.
There are no user callbacks between fork and exec. SSH
transports preserve terminal access and stderr; shell commands have no terminal
and null stdin, while protocol subprocesses use explicit pipes. Old Linux
kernels observe owned children with a blocking, event-driven wait rather than
polling; if descendant watches are unavailable, group cleanup
ends the remaining cooperative descendants when the leader exits. Forced daemon
stop requires a retained verified native handle; macOS and kernels without
pidfds refuse numeric-PID force-stop and retain RPC shutdown.

The shell process service admits at most 32 outstanding launches and running
processes. Captured output crosses one 64 KiB buffer per process into an open
blob stream, with storage back-pressure and a hard 64 MiB retention limit for
foreground and background jobs. A foreground command whose output the model saw
cut is sealed into a blob its result holds. One that left processes running
has its result hold the stream they still write, cut or not. Otherwise the
stream is discarded. A stream is named from its reference alone
(`blob::Places::stream`), whether or not it is still there, so a log projects
the same before and after a job ends. A background job's result holds its stream, open while it runs;
a group the session owns has its stream sealed as it ends, held by its end's
queued notice, which waits for the next model call rather than starting a
turn. A command's `timeout` (`[ext.shell] timeout`, 120 s by default, at
most 600 per call) is how long the session waits on it: past it the command
goes on in the background, as a forced steer sends it (`Cx::detach`), its
result coming back on the queue; only a call another call invoked, which
must be waited on, is stopped at it. Group observation is capped at 4096
members; exceeding it revokes the live lease. Following streams and enumerating groups use admitted
storage workers, leaving reactor workers available for other sessions. Once
owned-scope cleanup ends, capture finishes accepted writes and drains at most
1 MiB for up to 100 ms; escaped pipe holders cannot retain admission or delay
session shutdown indefinitely.
See [process measurements](perf/processes.md) for native validation and RSS.

## Extensions and SDK

`ext::Registrar` registers tools, decision-point handlers, services and
their needs, durable folds, model text, and agents. Registration tiers and
replacement by name allow overrides; the operator's policy floor is guarded.
The engine itself offers the model no tools. Extensions communicate through
the log (`observe`), typed state, services and points.

Queries are what clients ask the extensions without touching a log: the
files of a workspace for an @-mention, say. `r.query(Q)` registers one;
like a tool, its answer's shape (at once, a future, or streamed updates
before the reply) is the type `answer` returns, and its `Scope` type
(`Daemon`, `Workspace`, `Session`) is what its `QueryCx` holds. A query
is asked about its scope or a narrower one that implies it, and its
`QueryCx` names who asks (`machine`, the client's claimed machine, so a
query can tell a client on the daemon's machine from one elsewhere) and the
`role` the daemon gave it.
`QueryCx` has no way to append, publish or change state, so a query can never
write a log. Welcome advertises the queries; each is ephemeral,
correlated by the client's id, cancelled by `Cancel` or a hangup, and
admitted against a `QueueBudget` of its own, so queries held open never
take a mutation's place. A streamed answer is paced: each update waits
for room in the connection's outbox, and the answer is not polled
meanwhile. Queries come from the daemon's linked registry, not a
workspace's generation layers.
`WorkspaceShared<T>` keeps one value per workspace, made on first use and
dropped with its last user; the generation `Coordinator` holds its
workspaces in one, and [`ext-index`](ext-index.md) its workspace indexes.

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
| `ext-shell`, `ext-edit` | One streaming native shell tool, its output as blobs, and `edit` |
| `ext-mcp` | Lazy external tools over stdio and Streamable HTTP |
| `ext-prompt` | System prompt |
| `ext-policy` | Permission modes, judge and guarded operator floor |
| `ext-settings` | Model choice fold and model-spec service |
| `ext-skills` | Skill index and instructions loaded on demand from workspace generations |
| `ext-retry`, `ext-compact` | Retryable failures; overflow recovery and transcript edits |
| `ext-subagent` | `task` over owned child sessions |
| `ext-title` | Titles from a side model call |
| `ext-spill` | Long tool results shown as their ends and where the whole is |
| `ext-index` | One file index per workspace: `files.search`, `files.grep`, `glob`, `grep` |
| `ext-lua` | Optional runtime mods through the erased SDK |
| `ext-fixtures` | Test-only echo agent and sleep tool |

The shell extension registers one concrete SDK tool type at startup: `bash`
on Unix; native Bash discovered through PATH, Git for Windows registry/install
paths or MSYS2 on Windows; otherwise `powershell` using pwsh, else Windows
PowerShell 5.1. The Windows system WSL launcher is excluded. The daemon pins
that executable until restart, with no shell setting. Tool descriptions and
prompt environment context share cached `ExecutionFacts`: dialect, path style,
utilities, temporary directory and platform cleanup limits. Commands receive
the published temporary directory even after a different client attaches.

PowerShell uses one bounded Unicode `-Command` payload without profiles,
interactive input or execution-policy changes. Explicit exit codes survive;
otherwise the final command's success becomes 0 or 1, with normal error semantics.
A best-effort UTF-8 FileSystem cwd receipt cannot replace the command's status.
The 5.1 SDK type binds its native Utility and Management modules before running
commands, retaining client module paths. The same process service supervises either dialect. See
[shell execution evidence](perf/shell.md).

The default loop lives in the SDK and registers through the same machinery.
`apps/cli/src/exts.rs` assembles the linked extensions. One typed
[conversation primitive](agents.md) covers side requests and sub-agents
alike: turns or agent tasks, persisted or ephemeral, keyed, bounded,
awaited or detached, with role-based routing, committed results and bounded
retries; title, compact, the policy judge and `ext-subagent` are each a few
builder calls on it. Compaction edits the model's
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
default `Threads` implementation uses async-executor and async-io with owned
workers and separately admitted blocking classes. Async workers never exceed
four and the CPU ceiling is the CPU count minus one (at least one). The
daemon's `Config::daemon` counts effective parallelism (Windows CPU sets and
job caps) and runs blocking jobs at the foreground class; clients keep the
standard count and their caller's class. Catalog reconstruction
shares storage admission. macOS pressure events ask actors to release disposable
projection caches while retaining session ownership. Platform calls and unsafe
code belong in `sys`; `rt` owns admission and readiness integration.
[Native resource measurements](perf/resources.md) describe the policy and channel
comparisons.

`json::Json` holds shared raw JSON bytes. Arguments, output, model responses
and extension state pass through layers without decoding by non-owners.
Large record decoders live in their owning crates to avoid repeated generic
instantiations. [The provider layer](llm.md) implements Responses, Chat,
Messages and Gemini with recorded-cassette parity; it runs HTTP via `rt`'s
blocking pool and leaves retry policy to extensions.

The daemon wire is framed over the local endpoint or bridge stdio: a
little-endian u32 length, a `FrameKind` byte, then the body, JSON or a
`Chunk`: a blob's id, a little-endian u64 offset and the bytes, never
base64. A reader refuses a kind it does not know, and one that wants a
message refuses a chunk. `protocol::PROTO` defines the current version. Typed ids and message classes distinguish durable entries,
ephemeral traffic and control. Hello negotiates a supported version; attach
replays from a seq or supplies a view snapshot, ends replay with `Synced`,
then follows live entries. `Query { id, service, scope, body }` is
answered by `Update`s, if it streams, then exactly one `Reply { id,
outcome }`, on any connection, attached or not; `Cancel { id }` makes the
reply interrupted. `Fetch { id, session, blob, offset, len }` shares the
query ids: the daemon sends the blob's bytes from `offset` (`len` of them,
or to its end, clamped) as chunk frames in order, each once the client has
room for it (`Outbox::send_paced`), then one `Reply` whose output is the
count sent. A blob is served only through a session whose log references
it, checked where the fetch enters (`FrameLog::references`, folded lazily
from where the last fetch left off, so a session nothing fetches from pays
nothing): a hash alone opens nothing. A chunk names its blob and offset,
not the request, so the same frame will carry an upload. A wire encoding change requires a new version
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
| Data and wire | `json`, `protocol`, `llm`, `blob` (the blob seam, no I/O) |
| Blob backends | `blob-files` (over `sys`; the engine and the extension test rig) |
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
The supervisor's no-polling invariant uses explicit future polling and a fixed
runtime clock: queued notifications are consumed before asserting that the
next wait is the idle deadline.

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
- Can snapshot prefix validation avoid fully decoding every payload without
  weakening damaged-log detection? Catalog rebuilds and failed-commit recovery
  also have whole-log paths worth measuring.
- What are daemon RSS and cancellation behavior under real provider/tool
  load, beyond idle and synthetic workloads?
- Which runtime-extension capabilities and isolation should MCP and future
  out-of-process extensions share with Lua?
