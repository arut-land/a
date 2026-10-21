# MCP tools

`ext-mcp` implements tools from MCP **2026-07-28**. The CLI's `mcp`
feature links it (and the shell spawner); disabling that feature removes
it. An empty configuration contributes no tools or prompt text, opens no
files and starts no processes.

Settings live with the other extensions in `$A_HOME/config.toml`. The
extension owns every key; the daemon and kernel have no MCP settings.

```toml
[ext.mcp]
servers = ["docs", "tracker"]
timeout_secs = 120 # 1..600, per request

docs = ["/usr/local/bin/docs-mcp", "--read-only"]
docs_description = "Internal documentation search"
docs_env = ["DOCS_TOKEN=explicit-server-credential"]

tracker_url = "https://tracker.example/mcp"
tracker_description = "Public issue search"
```

`ServerName` validates ASCII letters, digits or hyphens, at most 64 bytes,
when configuration or tool inputs are decoded. `ToolName` validates remote
identities at discovery and tool-input decoding: nonempty names without
whitespace or control characters, at most 128 UTF-8 bytes. Caches and
dispatch use these types throughout. At most 64 servers can be configured.
Each name selects either its argument list or `<name>_url`, never both. Commands
are argv, not shell text. The child's directory and non-secret environment
come from the attaching client (or daemon when none attached), with
`<name>_env` overriding explicitly. Spawn hooks can wrap or refuse it.
HTTP currently supports unauthenticated endpoints; OAuth and configured
HTTP credentials are not implemented. Workspace `.mcp.json` is not read.

The shared SDK `Text::generation` path records an index containing only
server names and descriptions when a session adopts its registry generation.
`mcp_list {server}` starts just that server and returns tool names and
short descriptions. `mcp_schema {server, tool}` publishes the selected full
definition as recorded context through `Cx::publish`, then acknowledges
`loaded` / `already_loaded`. Its typed publication key includes server,
tool and exact schema content. Replay deduplicates effective publications;
edits or compaction that hide the context allow it to load again. `mcp_call {server, tool, arguments}` executes it. Schemas are
fetched with the server's paginated `tools/list`; lists are projected down
before entering the log/model context. Only an explicit schema load
publishes a definition; no remote schemas enter the initial request. Definitions are fetched afresh rather than cached across
turns, so list changes need no subscription. HTTP calls fetch their
selected definition to construct required mirrored parameter headers.

One stdio process per server per session is retained by the daemon until
its session's process ownership ends. Session scope isolates clients'
working directories, launch environments and credentials. This intentionally
chooses isolation over the spec's recommendation to share transports
across conversations. Calls are exclusive within a session. Another
session never inherits that session's connection. The engine owns the
process group and records it for recovery; an exit watcher reaps its
leader and removes the cached pipes. Registry destruction also kills
retained groups. Abnormal requests drop their connection, kill the whole
group and never reuse a channel that could contain a late response.

All three tools use SDK `Replay::Unsafe` and the ordinary external tool
path: policy and approval precede dispatch, the intent has a durable
barrier before effects, progress streams through `Cx::progress`, and the
result is a logged fact. Replay uses that fact; an unfinished call becomes
interrupted and is never reissued. No MCP annotation relaxes policy. Use
`[ext.policy] ask = ["mcp_call"]` to ask for executions, or include
`mcp_list` and `mcp_schema` to gate discovery too. Operator floor rules
can match the wrapper name and serialized server/tool arguments. Tool
cards are pure functions of the logged input and result.

`ResultFact` separates successful remote values from typed local failures:
configuration, spawn, transport, refused redirect, crash, timeout,
cancellation, protocol, remote JSON-RPC error, unsupported input request,
and size limit. Remote
`isError` results stay intact and get a bad card. No request retries
implicitly. Sampling, elicitation, resources, prompts and subscriptions
are not provided; an `input_required` response is a typed failure.

Stdio uses newline-delimited JSON-RPC with per-request version and
capabilities, and probes `server/discover` before its first operation.
There is no `initialize` fallback. Cancellation attempts a nonblocking
`notifications/cancelled` write and then kills the process group, including
servers that ignore notifications. HTTP refuses all redirects before any
resend, including same-origin redirects: configure the final endpoint
explicitly. Provider HTTP callers choose their redirect policy explicitly.
HTTP uses `llm::Ureq` over the existing runtime, supports JSON and
request-scoped SSE, and closes its response stream on cancellation. It mirrors method, tool name and valid
`x-mcp-header` parameters, using the prescribed Base64 sentinel encoding
when necessary. Invalid header annotations are excluded from HTTP tool
lists. Responses are bounded at 1 MiB. Discovery also has a 1 MiB aggregate
budget across all pages, before any schema retention or projection; lists
are further limited to 64 pages / 4096 tools. Exceeding any budget returns
`Failure::Limit`. Index descriptions use a deterministic UTF-8 prefix,
at most 512 bytes including an ellipsis. A retained result plus its escaped
card therefore leaves headroom in the 16 MiB protocol frame.

Every request owns a shared budget for all notifications, including unknown
methods and unrelated progress tokens: 256 messages / 64 KiB of JSON.
Both stdio and SSE spend it before retaining progress. One pending value
holds the latest progress, truncated to 1 KiB. At most eight publications
reach the engine per request: up to seven spaced by 100 ms, and a reserved
final update. A timer runs only while an update is pending and a slot is
available, so a quiet peer's latest update is still delivered without
periodic idle polling. Floods become typed limit outcomes, close the HTTP
response or kill the stdio group, and cannot enqueue unbounded engine work.
JSON decoding also applies serde's nesting limit. External schema
references are never fetched. Argument validation belongs to the server;
the client checks the argument object and routing field types.

## Validation and sources

`cargo nextest run --workspace -E 'package(ext-mcp) | test(/^mcp::/)'`
runs the fixture client, codec and daemon tests. The `fixture` feature
builds the tiny `mcp-fixture` binary; the CLI's dev dependency enables it
for workspace tests. It is absent from release builds. Tests record
server starts and effects in isolated temporary homes, including replay
and crash recovery. A descendant fixture independently proves that daemon
stop and cancellation kill the whole server tree.
See [the dependency and size measurements](perf/mcp.md).

The implementation follows the current MCP [stdio binding](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio),
[tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools),
[per-request protocol metadata](https://modelcontextprotocol.io/specification/2026-07-28/basic/index),
and [Streamable HTTP binding](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http).

Configuration precedents: [Claude Code](https://code.claude.com/docs/en/mcp)
uses project `.mcp.json` with `mcpServers` entries containing `command`,
`args` and `env`, and separate local/user scopes. [Codex](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)
uses `[mcp_servers.<name>]` in `config.toml`, with command/args/env or URL
and HTTP authentication settings. [OpenCode](https://opencode.ai/docs/mcp-servers/)
uses `mcp` in its JSON configuration, with `type: local` command arrays
or `type: remote` URLs. We keep our existing extension table layout and
explicit argv lists instead of introducing another automatic project
configuration source.
