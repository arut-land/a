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
experimental_stdio_sharing = false

# Broader stdio scopes require the experimental flag above.
docs_scope = "session" # session (default), workspace, user

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
HTTP uses the daemon's shared transport pool and OAuth credential owner. Optional
`<name>_client_id` and `<name>_issuer` configure pre-registered clients together.
Workspace `.mcp.json` and workspace executable declarations are not read.

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
publishes a definition; no remote schemas enter the initial request. Discovery
shares an immutable validated list while every page has a positive server TTL.
The earliest page expiry limits the whole list. Missing, zero or invalid TTLs
stay uncached; known immediately stale lists fetch independently. Concurrent
eligible discovery joins one fetch. Private HTTP lists bind the credential
owner's opaque authorization generation, including anonymous authorization.
Lookup observes expiry and joins any renewal first; each response reports its
actual generation after challenge/retry. Changing generation or visibility
between pages prevents caching that aggregate. Relevant list-change notifications,
process retirement and configuration replacement invalidate reuse. Freshness is
checked only on access: no polling or subscription stream. Tool results and
`input_required` outcomes are never cached. HTTP calls use the selected definition
to construct required mirrored parameter headers.

Stdio defaults to one instance per configured target per session, regardless of
configuration provenance. The MCP specification defines no process shareability
or instance scope. Set `experimental_stdio_sharing = true` and explicitly choose
`<name>_scope = "workspace"` or `"user"` only for servers whose application state
may be shared. Broader scopes are rejected without the flag; HTTP rejects scope
settings because remote process lifetime belongs to the server. User means this
daemon/home's principal, not all OS users. Workspace uses the engine's workspace
identity; separate worktrees remain separate. A deliberately shared server can
retain global browser or other application state: request routing is no sandbox.

One daemon registry reserves each exact declaration/scope/launch identity before
startup, so concurrent demand performs one spawn and one discovery probe. Session
launches use their client's cwd, environment and spawn hooks. Shared launches use
the trusted declaration's captured non-secret daemon environment plus explicit
server overrides, and the workspace directory or user configuration directory;
the first caller never supplies the shared environment or session spawn hooks.
Different declarations, argv, cwd, environment and scopes never alias.

The SDK's scoped process owner retains only activated leases. Hibernation drops
one session's demand; the last lease, configuration retirement or daemon shutdown
awaits native tree cleanup. Idle helpers hold neither session work nor daemon
liveness. A crash retires that generation; only another explicit call may start
its successor. Globally unique request IDs and a per-request ingress/progress map
allow replies to arrive out of order. One writer serializes whole frames while
the reader continues routing replies. A 32 MiB admission budget waits before
allocating frames and retains each request's credit through cancellation drain;
it delays demand without rejecting features or limiting the total session count.

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
There is no `initialize` fallback. Session cancellation retires the process tree.
Shared cancellation removes the caller's context and queues an MCP cancellation
for that request. Only its charged ingress/drain record remains until the reply
or the original exchange deadline. An undrained exchange retires the instance at
that deadline; collateral calls fail and are never replayed automatically. HTTP refuses all redirects before any
resend, including same-origin redirects: configure the final endpoint
explicitly. Provider HTTP callers choose their redirect policy explicitly.
HTTP uses `llm::Client` over the existing runtime, supports JSON and
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
