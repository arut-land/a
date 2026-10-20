# Lua extensions

Build with `cargo build -p a --features lua`. Lua is opt-in while quiet-machine
startup measurements are outstanding; see [the runtime comparison](perf/lua.md).
The runtime is vendored Lua 5.4 through mlua. No system Lua installation is needed.

Put a UTF-8 `name.lua` in `$A_HOME/ext/` or the workspace's `.a/ext/`.
The file stem (`name`) is its extension identity. Files are installed in name
order, after the Rust extensions. A workspace file replaces a home file of the
same name. Tools and text replace earlier contributions of the same name using
the SDK's normal tier rules. Guarded extensions cannot be replaced.

File events prepare changes, additions and removals off the async worker.
The kernel adopts a ready generation when no calls are open. Running calls
retain their original functions and state view. New folds rebuild from a
consistent snapshot and suffix, including pending changes. A broken edit keeps
the last working generation and reports an error; invalid mods at first load
are warned about and omitted. `/lua-reload` prepares edits explicitly and
acknowledges completion. Workspace subscriptions end with their last session.

Use `[ext] disable = ["name"]` in the home config to omit a mod or a compiled
extension. `disable = ["lua"]` omits the loader. Config changes take effect on
daemon restart; source edits take effect without restarting.

## API

Functions receive Lua values decoded once from the SDK's JSON text ABI and
return values encoded to JSON. `nil` is a passing hook result or a null tool
result. Use `a.null` to preserve a null inside a table. JSON arrays and empty
objects retain their shape when passed through; `{}` created in Lua is an object.

```lua
a.tool{
    name = "greet",
    description = "Greet someone.",
    audience = "user", -- /greet Ada; omit for a model tool
    input = {type = "object", properties = {args = {type = "string"}}},
    run = function(input, cx)
        return {greeting = "Hello " .. input.args}
    end,
}
```

A user command receives `{args = "text after the command"}`. The input schema
is advertised to the model; a Lua function checks any additional constraints
it needs. Lua tools are exclusive and unsafe to replay after a crash.

`a.on(point, function(input, cx) ... end)` supports the SDK's wire points:
`input`, `pre_tool`, `permission`, `approve`, `on_stop`, and `observe`.
A rewrite returns `{replace = value}`, a block returns `{stop = "reason"}`,
and `nil` passes. Permission answers are `"allow"`, `"ask"`, `"defer"` or
`"deny"`; approval answers are booleans. `on_stop` returns continuation text
or nil. `observe` is read-only; its return is ignored. Handler errors use the
point's declared `ON_ERROR`: for example, an input error refuses that input,
while an `on_stop` error passes. Hooks cannot await effects.

`a.text{name = "name", order = 0, text = "system text"}` contributes system
text. `a.fold{name = "count", initial = 0, apply = function(state, change)
return state + change end}` declares durable JSON state. Reducers must be
pure: they run in a separate VM during replay and cannot perform effects.
A failed reduction leaves its previous value intact. Keep state in folds;
module globals are shared by sessions and are discarded on reload.

Tool context functions use a dot, not a colon:

| Function | Result |
| --- | --- |
| `cx.state(name)` | This mod's committed fold value, or nil if absent |
| `cx.update(name, change)` | Await a durable change; the next read sees it |
| `cx.progress(text)` | Publish call progress |
| `cx.tool(name, input)` | Await a granted tool through policy |
| `cx.session` | Session identity |

Hooks receive `cx.session` and read-only `cx.state(name)`. A saved context
cannot act after its call finishes. Named tool delegation is the service
interface: arbitrary Rust trait services and non-wire decision points remain
Rust-only.

`a.agent{name = "scripted", step = function(input) ... end}` registers a pure
agent. Set `agent = "scripted"` at the top level of the home or workspace
config. Inputs are JSON variants such as `{user = {text = "hello"}}` or
`{tool_done = {call = id, outcome = outcome}}`. Return an array of SDK actions,
for example `{{say = input.user.text}}` or
`{{call = {tool = "bash", input = {command = "pwd"}}}}`; return nil for no
actions. Tool actions run through the engine and its normal approval flow.
Agent tool proposals need the mod's corresponding `tools` capability too.
An agent may return at most 128 actions. Agent functions cannot await effects.
Errors stop their turn with a diagnostic.

[The counter example](examples/counter.lua) uses tools, a hook, text and a fold.
Copy it as `$A_HOME/ext/counter.lua`, then run `/count` or type `count please`.

## Effects and limits

No mod gets ambient filesystem, process, network, environment or native-code
access. To delegate to `bash`, grant that tool in the home config:

```toml
[ext.runner]
tools = ["bash"]
```

Then `runner.lua` can await it:

```lua
a.tool{
    name = "run", audience = "user", description = "Run a greeting.",
    input = {type = "object"},
    run = function(input, cx)
        return cx.tool("bash", {command = "printf hello"})
    end,
}
```

The capability does not authorize an effect. Delegation runs the target's
`pre_tool`, permission, approval and `post_tool` hooks; shell spawning also
runs its normal spawn hooks. A deny or defer fails the call. Synchronous approval answers are honored. A request requiring a later approval,
including the client's, is currently refused. Configure
policy deliberately for delegated tools that should run without interaction.
Delegated work shares its parent's durable call identity and progress; it does
not create an independent tool intent. Delegation depth is capped at eight.
An HTTP extension, if installed, can be delegated to in the same way; this
crate adds no independent HTTP client.

The sandbox omits `os`, `io`, `debug`, `package`, `load`, protected calls,
user coroutines, metatable mutation, bytecode dumping and native pattern matching.
The API and standard libraries are read-only views. These cannot
be enabled through tool capabilities. Async wrappers are created by the host
before the sandbox closes. Lua tools yield through coroutine futures over `rt`;
waiting on a delegated process does not occupy an async worker.

Defaults, configurable in the home file:

```toml
[ext.lua]
memory_bytes = 4194304
instructions = 100000
timeout_ms = 25
```

Memory is capped per VM; a mod with folds has a separate reducer VM.
Instructions are counted per invocation, including async resumes. The time
limit is checked by the instruction hook per active poll, excluding awaited
I/O; it is not an OS-enforced hard deadline for native functions. Native
pattern functions are omitted because they cannot be interrupted by the hook.
Each invocation may send at most 128 host operations and 1 MiB of outbound
progress, state changes and delegated input, which the Lua allocator cannot
account for. JSON is bounded to 256 KiB, 64 levels and 8192 nodes; repeated table identities
are rejected. Source is capped at 256 KiB, text at 8 KiB, and a workspace loads at most 128 mods. A mod registers at most 128
contributions.

Workspace preparation also limits combined source to 1 MiB and directory scans
to 4096 entries per root, with an aggregate compilation budget of one second.
Fold rebuilds use a snapshot plus at most 100,000 suffix entries within a
one-second budget. A rejected rebuild keeps the current generation and reports
why; no partial state is installed.
