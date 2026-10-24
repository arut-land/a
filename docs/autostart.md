# Autostart

A client starts the daemon itself when none is running (self-spawn,
[client](client.md)); that path never consults the OS registration, so it
is equally fast with autostart on or off. Autostart is the optional OS
integration on top: `a daemon autostart on` registers the daemon to start
when the user logs in, so it is already serving when the first client
connects, and `a daemon autostart off` removes the registration. `on`
installs the running build first, since the registration names the
installed one. On macOS
the installed app offers the same as **Start at login** in its Command Line
settings. Neither needs root, and explicit `A_HOME` instances cannot
register: they always start on first use.

The registration is the only state, and it belongs to the OS: present means
on. A daemon that has served for two seconds asks for it on a blocking
worker, off every reply's path. Idleness does not end it before the answer,
and the answer is not activity: a transient daemon past its idle limit exits
as soon as it learns so. If autostart is on, it rewrites an outdated
registration (a new executable path or native layout) and from then on stays
up instead of exiting when idle, waking for nothing while idle. A daemon
started without autostart, or before it was turned on, keeps the idle exit;
turning it off leaves a running daemon until it stops. `a daemon stop` and
an upgrade drain still end a resident daemon; the next client self-spawns a
successor, which becomes resident the same way, and the OS does not
restart it.

The login entry is `a __daemon --login`, and the daemon log records its
start with the `login` origin. It takes `start.lock` like a self-spawning
client, so a client starting at the same moment either waits and connects to
it or wins and makes it exit quietly. Under a service manager (Linux, macOS)
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

The CLI writes `~/Library/LaunchAgents/dev.a.daemon.HASH.plist`, which
launchd loads at the next login, where `RunAtLoad` starts the daemon. It is
written, not bootstrapped, because loading it while a daemon serves would
only start a second one to find the first. `autostart off` boots it out and
removes it. The app registers its bundled agent (`dev.a.daemon`, which names
its helper inside the bundle) through `SMAppService`, links to Login Items
settings when approval is required, and refreshes that status when it
becomes active. Both agents have umask 0077 and a 30-second exit timeout
and no `KeepAlive`. A daemon counts the app's agent as on while launchd has
it loaded. Either is off while the user has switched it off in Login Items:
Background Task Management records that in launchd's disabled list
(`launchctl print-disabled`), which the daemon reads. When launchd first
loads either agent, macOS shows its "Background Items Added" notification;
for the CLI's agent that is at the next login.

## Windows

`autostart on` writes a value under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, the per-user startup
list Settings > Apps > Startup and Task Manager show and let the user switch
off; the daemon is resident only while Explorer's `StartupApproved` record
allows it. A logon-triggered scheduled task was the alternative: it needs
COM registration, does not appear in that list, and runs without a window
only under a stored-credential or batch logon that standard users lack.

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
