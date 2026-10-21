# Forking a conversation

`a fork <session>[@<seq>]` prints the new session's short id. The source can
be a full id or an unambiguous prefix. `seq` is inclusive and defaults to the
latest committed entry visible when the daemon receives the request. Sequence
numbers start at 1. Attach to the printed id to continue.

This is **conversation-only**. Files are neither copied nor rewound, and shell
commands and edits already made still affect the shared workspace. The new
session starts with the source workspace, cwd and recorded environment at the
selected entry. Secrets redacted from that environment stay absent; explicitly
refresh the fork's environment if needed. Attaching to fork it does not capture
the caller's environment, even over a bridge.

The client SDK exposes `Daemon::fork_session`, `Target::Fork`, and
`Cmd::Fork`. In the TUI, line REPL, web composer and FFI's `submit`, `/fork`
copies the current conversation; `/fork <seq>` chooses its boundary. The FFI
also exposes `Session::fork`. A successful interactive fork switches to the
new session; a refusal leaves the source attached.

## Rest points

The selected prefix must leave the kernel at rest: no unfinished model or
tool call, pending approval, queued input, child watch or agent action yet to
be recorded. The daemon refuses other boundaries before creating any log;
it never repeats a copied operation or invents a tool result. Prefixes with
recorded background process ownership are also refused, since the new session
must not acquire the source's process groups. Earlier rest points remain
available while the source runs.

## Durable history

The fork owns a copy of every entry through the boundary, with the original
sequence numbers, payloads and epochs. Its next entry is an `ext` fact owned
by `fork`, state `parent`, recording `{ session, seq }`. Fork ancestry is not
child ownership: the fork has no owning call and never reports to its source.
The source file, writer lease, catalog record and derived snapshots are not
changed or loaded for recovery, even when the source is hibernated.

Copying costs O(prefix bytes) disk space and replay per fork. This deliberately
avoids cross-file prefix readers: deletion of the parent cannot break a fork,
there is no shared lease or retention graph, and hibernation and snapshots use
the existing independent-session path. The import is written and synced under
a pending filename, then renamed and the directory synced before publication.
A crash exposes a whole imported log or none; pending files are not sessions.
Publication and cataloguing share a recovery guard. Any failed or unwound
import keeps the catalog dirty, including on orderly shutdown, so the next
startup discovers a log left behind after rename or directory-sync failure.
Derived files can be removed and rebuilt from that log.

Extension folds replay only the chosen prefix. Dynamic extension installation
facts retain portable generation images: Lua source, tool grants and execution
limits, plus immutable skill catalogs. Skill publications and transcript edits
are replayed only through the selected sequence: a fork after a load keeps it
loaded; a fork before that load can load it independently. Replay reconstructs the recorded generation, even after its files
change or disappear; it never substitutes today's Lua code. Images are part
of the durable log, not a snapshot cache. A build without the required
generation opens that history read-only with `ReadOnly::Extensions`; it never
replaces the recorded generation. Restoring a Lua generation currently scans
the prefix before using its derived state snapshot. A subsequent explicit Lua reload or
workspace generation change can adopt a new generation at rest.

The coordinator chose conversation-only semantics for this implementation.
**Owner confirmation remains open** on that product decision and on whether
future forks should accept unfinished prefixes by recording interruptions.
This implementation accepts rest points only.

Fork requests return `ForkError` variants for lookup, sequence, unfinished
work, owned resources, unavailable generations, disabled policy, viewer,
shutdown and storage failures. The wire carries `DaemonMsg::ForkRejected`;
the SDK retains the enum as `client::Error::Fork`. The shared client rule
renders each refusal once and keeps the source attached. The SDK's
`ValidatedPrefix` owns the checked entries and typed ancestry; its constructor
checks continuity and the extension's rest-point policy before import.
