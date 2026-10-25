# Skills

`skills` is a default CLI feature and an extension, disabled with
`[ext] disable = ["skills"]` or omitted with `--no-default-features`.
Put each skill in `$A_HOME/skills/<directory>/SKILL.md` or the workspace's
`.a/skills/<directory>/SKILL.md`. The frontmatter name is its identity and
command name. Workspace definitions override home definitions. Discovery is
one directory deep, sorted by path; duplicate names within one root are errors.
Discovery reads bounded frontmatter to resolve identities and precedence before
capturing winning sources. Changes to a shadowed body cannot delay a settled
workspace definition.
A skill cannot replace an installed callable, including `skill` itself, or
a client-owned command such as `/help` or `/new`. The CLI supplies that reserved
namespace from the shared client command table.

```markdown
---
name: review
description: Review a change for correctness and missing tests.
---
Read the diff, trace each changed path, and report concrete defects.
Consult references/checklist.md beside this file if needed.
```

The frontmatter uses YAML with required text fields `name` and `description`.
Names contain 1–64 lower-case ASCII letters, digits or internal hyphens.
Descriptions are nonempty, at most 1024 bytes after whitespace is folded.
`yaml-rust2` parses syntax as events; headers are limited to 8192 bytes and
16 levels of nesting. No YAML tree or alias expansion is constructed.
Unknown metadata is validated and ignored, never interpreted as permissions.
Duplicate keys in any mapping, non-scalar keys, missing required fields,
collections or null for required fields, tags, anchors and aliases reject the
candidate with a typed error and file line number. Quoted keys, multiline
scalars, YAML escapes, flow collections and folded/literal blocks are supported.
All description whitespace is folded before indexing. Delimiters are lines
containing `---`; LF and CRLF work.
The parser choice and measurements are in [perf/skills.md](perf/skills.md).

Only names and descriptions enter the model's index. `skill({"name":"review"})`
loads the instructions; `/review` uses the same loading path through a user
callable. `/review arguments` also records those arguments as context. Commands
acknowledge loading; like the existing user commands, they do not start a model
turn. The next conversational input uses the loaded instructions. The containing
directory accompanies loaded instructions, so relative references can be read
with the normal filesystem tools. Supporting files are neither read nor run by
this extension. Reading them later follows those tools' ordinary logging and
policy rules.

A successful load seals `Body::Context` text and its typed publication key
in one log entry. A durable prefix therefore contains both or neither.
Subsequent loads return `already_loaded` while that publication remains in
the effective transcript. `Edit` ranges, including compaction, invalidate the
covered publications. A subsequent load restores omitted instructions, and
the adopted generation's index is restored before the next model request or
retry. Snapshots retain this same edit-aware projection. An edited body or relocated directory
can be loaded again after adoption. Unknown names return the typed
`LoadError::UnknownSkill { name }` inside the tool's result and publish nothing.
Loads are exclusive, so concurrent model requests cannot publish the same
instructions twice. An interrupted load is unsafe to replay: recovery does not
rerun it against current files. The transcript reads the recorded context; the
publication projection reads only log entries. Neither reopens a skill during replay.
Opening a workspace subscription does prepare current files, independently of
replaying the recorded conversation.

## Generations

Preparation captures raw skill bytes off the async worker. Discovery reads
only tentative names in priority order; invalid descriptions, other metadata
and bodies in losing files cannot reject or delay a winner. Winning files
and unidentifiable prefixes undergo native stability checks. Malformed
observations remain snapshot inputs until the shared debounce and
re-observation establish a stable rejection. Changed observations retry
without requiring another filesystem callback. Bodies are held
in the immutable generation to prevent a tool midway through a turn from
reading a newer edit; they enter model context only on demand. Filesystem events
prepare new candidates. There is no scan on a session burst or generation read.
A valid candidate is published together; an invalid edit retains the previous
registry and reports a warning. An invalid initial catalog refuses the workspace
subscription. Limits are 4096 entries per root, 128 effective skills, 64 KiB per
file, 8 KiB frontmatter, 16 KiB of index metadata and 1 MiB combined bodies and descriptions.

Sessions adopt a prepared registry only at rest, through the same engine path
as Lua: no calls, asks, children or recovering model call. A busy session keeps
its tool objects, index and bodies. The next model call after adoption logs the
current index as ordinary context. Reloads and resumed sessions can record a
new index; later indices supersede earlier ones. Previously loaded instructions
stay in the log; load the skill again to adopt its changed body. Compaction
can hide their publication from effective context without removing history. Deleting a skill
removes it from the next index and command set, without rewriting history.
Missing roots subscribe to their nearest existing ancestors; creating or
replacing a root installs its recursive watch before preparing the catalog.
On macOS, empty roots use native watches on the existing async reactor and
initialize FSEvents only when a root exists ([RSS measurements](perf/idle-rss.md)).
Workspace subscriptions share one catalog and watcher; weak indexing releases them with the last
session. The skills source wraps an optional Lua source; changes from either produce a complete registry,
and an unsuccessful overlay retains the last working combined generation.

The SDK additions are general: `Text::generation` supplies logged context for
an immutable registry, `Cx::publish<P>` seals a typed key with its context
before returning, and cloning a registry shares registrations while rebuilding
its resolved tables. Weak generation subscriptions let derived sources prepare
before a source reload completes, including explicit `/lua-reload`. `Host` and the kernel only route these operations to
ordinary `Context` entries. No skills setting or name enters the
kernel. Protocol 8 includes the optional publication identity to a context entry. Generation context is captured at
binding, then logged before the first new model call; folding old entries does
not regenerate it.

## Other tools' layouts

The owned layout follows the existing `$A_HOME/ext` and `.a/ext` convention:
a plural feature directory containing named packages. The tools surveyed on
2026-10-21 use the same `SKILL.md` convention with different roots:

- [Claude Code](https://code.claude.com/docs/en/skills):
  `~/.claude/skills` and `.claude/skills`.
- [Codex, official documentation](https://learn.chatgpt.com/docs/build-skills):
  `~/.agents/skills` and repository `.agents/skills` roots, including ancestors
  up to the repository root.
- [pi source documentation](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/skills.md) and
  [resource loader](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/core/resource-loader.ts), with
  [config roots](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/config.ts):
  `~/.pi/agent/skills`, `.pi/skills`, and shared `.agents/skills` roots.

Other tools' roots are opt-in. Add explicit directories to the home config:

```toml
[ext.skills]
dirs = ["/home/me/.claude/skills", ".claude/skills", ".agents/skills", ".pi/skills"]
```

Absolute paths are literal; relative paths resolve against the workspace.
There is no tilde expansion or implicit scan of other tools' directories.
Additional roots load in listed order, later overriding earlier; the owned
home and workspace roots load last. Settings change on daemon restart; file
edits change through generations.

Discovery skips nonregular `SKILL.md` candidates with the typed
`sys::fs::RejectedFile::NotRegular` reason. It checks metadata before open and
checks the resulting handle again; the Unix open is nonblocking, so a FIFO
substituted between checks cannot stall shared generation preparation.

Resolved package and `SKILL.md` paths must remain inside their configured root.
An escaping symlink is skipped with `RejectedFile::OutsideRoot`; an alias whose
resolved target is inside the root is allowed. An explicitly configured root
may itself be an alias. Unix discovery opens the resolved relative path through
pinned directory descriptors with `NOFOLLOW`, preventing a concurrent ancestor
replacement from redirecting the open. Windows checks the opened handle's final
path before reading. The scan reads no candidate body until these checks pass.


Forks replay publication identities and transcript edits only through the selected
inclusive sequence. A prefix after a load retains that publication; one before
it has none. `Extensions` installation facts also retain the immutable skill
catalog image (names, descriptions, base-directory text, and bodies), so the
fork can load its historical instructions after current files change or vanish.
Those images are restoration data, not model context: only the index enters
context until loading is requested. The skills source restores its own layer
and delegates the underlying recorded installation to Lua when present; neither
layer decodes the other's images. Restored installation identity is checked
against the recorded installation before the fork is accepted.

Workspace source notifications coalesce through the coordinator's 100 ms
settle timer. `Sources::open` receives the host's `Delay` callback, so the
wait yields an async task before `Prepare` admits blocking generation work;
the loader chooses no reactor and sleeps no worker.
