# Paths and install layout

`sys::Paths` (also named `Home` at daemon call sites) resolves directory
purposes once. `InstallLayout` distinguishes native per-user locations from
an explicit isolated root. Callers use these paths, without OS checks or
constructing platform directory suffixes. Resolution creates no directories;
reading configuration does not require a writable home.

| Purpose | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Configuration, policy, extensions, skills | `$XDG_CONFIG_HOME/a` | `~/Library/Application Support/dev.a/config` | LocalAppData `a/config` |
| Sessions, catalog, retained output, blobs, crash breaker | `$XDG_STATE_HOME/a` | `~/Library/Application Support/dev.a` | LocalAppData `a/state` |
| Installed assets and current executable staging | `$XDG_DATA_HOME/a` | `~/Library/Application Support/dev.a` | LocalAppData `a/data` |
| Disposable cache | `$XDG_CACHE_HOME/a` | `~/Library/Caches/dev.a` | LocalAppData `a/cache` |
| Diagnostic logs and crash reports | `$XDG_STATE_HOME/a/logs` | `~/Library/Logs/dev.a` | LocalAppData `a/logs` |
| Temporary files | Runtime directory `a-tmp` | Darwin user temporary directory `dev.a` | LocalAppData `a/temp` |

Windows resolves LocalAppData with `SHGetKnownFolderPath`, including redirected
folders. Resolution opens the shell-folders and COM Windows API contracts through
`libloading`, using a System32-only search. It copies the returned path, frees the
COM allocation, then releases both library references. There are no static
Shell32 or COM imports for path resolution; isolated `A_HOME` resolution does not
open either contract. A fresh-process invariant checks that native resolution
retains neither Shell32, Ole32 nor Combase. Configuration contains provider keys
and machine-specific tool paths,
so it stays local alongside state, builds, caches and diagnostics. RoamingAppData
is reserved for explicitly portable, secret-free preferences; there are no such
preferences in this layout. macOS obtains its
per-user temporary directory with `confstr(_CS_DARWIN_USER_TEMP_DIR)`, independently
of an inherited `TMPDIR`.

Linux ignores relative or empty XDG values. Defaults are `~/.config`,
`~/.local/state`, `~/.local/share` and `~/.cache`. These conventions need no
systemd, desktop session, D-Bus or glibc facility. Complete absolute XDG bases
work without `HOME` or a passwd entry; home lookup occurs only when a base
needs its default. A read-only configuration
or network home can coexist with writable local state/cache using absolute
`XDG_STATE_HOME` and `XDG_CACHE_HOME`. Retained output is durable state, never
purgeable cache. The layout does not silently move sessions or claim remote
filesystem durability; use a local state root in containers and WSL where
local locking and durability are needed.

The endpoint backend selects a validated private `XDG_RUNTIME_DIR`, an existing
private `/run/user/UID`, or `/tmp/a-UID`. It verifies ownership, directory type,
permissions and socket pathname length. A missing runtime directory works
without logind, including Alpine/musl and headless containers. A `noexec` mount
can hold sockets; executable staging uses the data directory instead. macOS
uses the endpoint backend's private `/tmp/a-UID` directory for IPC; its Darwin
user temporary directory serves temporary files.
Endpoint claims, peer checks and published address discovery remain owned by
the endpoint backend. Named pipes on Windows have no filesystem socket path.

`A_HOME` explicitly selects an isolated layout: configuration, durable state,
data, cache, logs, temporary files, daemon records and executable staging all
stay under that root. Empty values fail; relative values become absolute before
spawning the daemon. The Unix transport address still uses the endpoint
backend's private runtime directory, so arbitrarily long isolated roots work;
its discovery record stays under the isolated root. No native directory is
created for isolated storage. Tests stop only their own isolated daemon.

Daemon startup creates the resolved temporary directory privately before
registering extensions. Launch passes
it as `TMPDIR`, and as `TMP` and `TEMP` on Windows. Cached shell facts use that
same directory, including foreground starts; tool environments override attaching
clients' temporary-directory values with it.

Daemon launch preserves the selected layout through its cleared environment:
isolated instances pass `A_HOME`; native instances pass the resolved XDG bases
on Linux, preserve the resolved home on macOS, and use Known Folder lookup on
Windows. Native launches do
not turn their state directory into an isolated `A_HOME`. Extension and skill
loaders use the configuration root, while endpoint ownership uses the durable
state root. SSH discovery selects `sys::RemoteLayout` from the remote host's
platform inside the SSH call, including that host's `A_HOME` or XDG overrides.
It can find an installed remote build across client platforms; copying this
build still requires a matching remote platform. Sync and async launch use
the same layout scripts. Uploads stream binary bytes through SSH stdin to a
private temporary file beside the destination, then publish it with a rename.
The destination is a quoted positional argument, preserving spaces, apostrophes
and shell metacharacters in native or overridden roots.
There is no migration from older layouts.

Executable staging remains `data/runtime` and retains its existing publication
behavior. Installer-owned executable identity and avoiding routine staging are
a separate change; the data-directory seam supplies its location. A read-only
or `noexec` data directory can still prevent staging until that change lands.

[Directory resolution measurements](perf/paths.md) include native module lists,
first-lookup profiles, paired performance measurements and dependency cost.
