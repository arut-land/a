# Autostart

A client starts the daemon when none is running ([client](client.md)). The
native launch adapter keeps registered Linux installations under systemd;
isolated `A_HOME` instances and unmanaged installations start directly.
`a daemon autostart on` registers future login launches and installs the
running build first. `a daemon autostart off` disables future launches
without stopping the running daemon. Use `a daemon stop` to stop it.
The installed macOS app exposes these controls as **Start at login** in its
Command Line settings. Neither control needs root. Explicit `A_HOME`
instances cannot register.

The registration and its enabled state belong to the OS. A daemon of the
native installation that has served for two seconds
asks for it on a blocking worker, off every reply's path; that is its one
wake not caused by a client or by work, and an isolated daemon has none. Idleness does not end it before the answer,
and the answer is not activity: a transient daemon past its idle limit exits
as soon as it learns so. If autostart is on, it rewrites an outdated
registration (a new executable path or native layout) and from then on stays
up instead of exiting when idle, waking for nothing while idle. A daemon
started without autostart, or before it was turned on, keeps the idle exit;
turning it off leaves a running daemon until it stops. `a daemon stop` and
an upgrade drain still end a resident daemon; the next client starts a
successor through the native launch adapter, which becomes resident the same
way. The OS does not restart it automatically. A drain that hands its work
over instead (see
[agents](agents.md)) execs the newer build in the same process on Linux and
macOS, so the service manager tracks the same daemon, still resident.

The login entry is `a __daemon --login`, and the daemon log records its
start with the `login` origin. It takes `start.lock` like a self-spawning
client. A managed Linux start asks systemd without holding either startup
lock; systemd waits for the daemon to bind and send `READY=1` before the
client connects. Concurrent starts reuse the same unit job. Under a service
manager (Linux, macOS)
it then waits for the daemon record as a client does, so a predecessor still
draining or stopping lets go of its endpoint first, binds, and serves in the
foreground the manager tracks, with the same exclusive home claim and
peer-verified endpoint as any daemon. On Unix, an entry point older than the
build the install selects (the app's bundled helper) re-execs that build in
the same PID before serving, so the first client never has to drain it. It
does so at most once: it never execs the image it is already running, even
when that image reports another id than the marker naming it.

## Linux

`autostart on` probes the running `systemd --user` manager, writes
`a-daemon-HASH.service` under the XDG configuration base's `systemd/user`,
and enables it by path (`WantedBy=default.target`), so the user manager
starts it with the user's first session. Names use the home's endpoint
identity, so layouts with different bases never touch each other's unit.
First attach after stop uses `systemctl --user start` for an existing unit;
explicit replacement uses `restart`, so systemd keeps tracking the successor
even while the previous process finishes exiting. A reset or replacement
also clears the unit's failed state. The unit uses `Type=notify` and
`NotifyAccess=main`; readiness means the home's endpoint is bound. An
outdated unit is rewritten and reloaded before it is started.
The unit runs the executable selected by `Paths::runtime().current_exe()`
with `/` as working directory, umask 0077, `KillMode=mixed`, a 30-second
stop timeout and the resolved XDG base directories in its environment, so
the manager's login environment cannot select another home.
`XDG_RUNTIME_DIR` is left to the manager, which sets the session's own. It has no
`Restart=`. Disabling it with `systemctl --user disable` is respected: the
daemon is resident only while the unit is enabled. The installer does not
enable lingering.

OpenRC, containers, minimal systems and WSL without a user systemd manager
have nothing to register with: `autostart on` says so, and a start there is
always a self-spawn. WSL with a running user manager can opt in normally.

## macOS

The CLI registers `~/Library/LaunchAgents/dev.a.daemon.HASH.plist` when no
registration exists. launchd loads it at the next login, where `RunAtLoad`
starts the daemon. The app initially registers its bundled agent
(`dev.a.daemon`, which names its helper inside the bundle) through
`SMAppService` and links to Login Items settings when approval is required.
Once registered, CLI and app controls use the existing registration's owner:
the app queries status and changes settings through its bundled CLI helper.
Initial SMAppService registration explicitly hands the bundled owner to the
helper, so an agent awaiting loading or approval cannot create a second plist.
Both registrations have umask 0077, a 30-second exit timeout and no
`KeepAlive`.

Off uses `launchctl disable`, leaving a loaded agent and its live daemon
alone. On uses `launchctl enable` to clear a disabled override, including
one set in Login Items. Neither control uses `bootout` or unregisters a live
SMAppService agent. Status and daemon residency consult the same disabled
list. The app refreshes status when it becomes active. macOS shows its
"Background Items Added" notification when it first loads an agent.

## Windows

`autostart on` writes a value under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, the per-user startup
list Settings > Apps > Startup and Task Manager show and let the user switch
off; the daemon is resident only while Explorer's `StartupApproved` record
allows it. A logon-triggered scheduled task was the alternative: it needs
COM registration, does not appear in that list, and runs without a window
only under a stored-credential or batch logon that standard users lack.

The full quoted command must fit the Run key's native limit of 260 UTF-16
code units (excluding the terminating NUL). Registration rejects longer
commands before writing the registry and asks for a shorter installation
path.

The value runs `a.exe __daemon --login`, which starts a detached daemon
exactly as a client does and exits. A console program started at logon
would otherwise get a console window (a Terminal tab on Windows 11), so
`a.exe` embeds an application manifest (MSVC builds, the shipped Windows
targets; a GNU-toolchain build has none) whose `consoleAllocationPolicy` is
`detached`: started by Explorer or the `Run` key it gets no console, while
started from a console it shares that console as before. When something
without a console starts `a` with no standard handles (Explorer, the Run
dialog), `a` itself asks for the console the OS would have made
(`AllocConsoleWithOptions`); the daemon never does. Windows releases before
the policy ignore it and allocate a console as they always have, so there
the window shows only for the moment the entry takes to start the daemon. A
second, windows-subsystem launcher would avoid that moment on every release,
at the cost of a second executable beside `a` in the install layout; the
manifest keeps one binary and is how the Windows console team expects a
console program to opt out of a console.
