# Native daemon activation

Self-spawn is the default. `a daemon install-activation` opts the native
per-user installation into socket activation; `a daemon uninstall-activation`
removes it and restores self-spawn. Neither command requires root. Explicit
`A_HOME` instances cannot install activation. The commands serialize with
client startup; installation stops an unmanaged daemon before handing the
endpoint to the service manager.

The manager owns the listening socket while `a` is absent. The foreground
`a __daemon --activated` adopts that socket, claims the same exclusive home
endpoint lock and daemon record, and serves the same peer-verified
`OwnedConnection` transport. It exits under the existing idle policy when
clients, work and jobs no longer pin it. Retirement releases the home claim
without withdrawing the manager's socket. The next connection starts a new
daemon; no helper process or readiness poll stays resident.

A registration record in the private state/run directory prevents self-spawn
from bypassing disabled or failed native activation. If a registered socket
cannot be reached, the client reports an actionable error. Remove activation
explicitly to return to unmanaged startup. `a daemon stop` stops a running
daemon, leaving activation installed; a later connection can start it again.
`status` and `log` do not start an idle daemon. `restart` also resets the
application crash breaker. The manager does not unconditionally restart an
idle daemon, and its rate limiting remains in effect after failures.

## Linux

The CLI probes the running `systemd --user` manager, then writes
`a-daemon.socket` and `a-daemon.service` under the XDG configuration base's
`systemd/user` directory. It links the service and enables the socket with
`systemctl --user`. The socket uses `Accept=no`, mode 0600 and the endpoint's
private runtime directory. The service runs the executable selected by
`Paths::runtime().current_exe()`, with a stable `/` working directory, umask
0077, `KillMode=mixed` and a 30-second stop timeout. Resolved XDG directory
purposes are included in the unit environment, so the user manager's login
environment cannot select another home.

`listenfd` implements the documented `LISTEN_PID`/`LISTEN_FDS` protocol. The
backend requires exactly one Unix stream socket, validates its private
pathname and sets close-on-exec before any tools start. Accepted peers use
the existing `SO_PEERCRED` checks. The socket persists across idle exit and
build drain. Uninstall disables/stops the socket before stopping its service.
The installer does not enable lingering.

OpenRC, containers, minimal systems and WSL without a user systemd manager
continue to self-spawn. WSL with a running user manager can opt in normally.
No distro or kernel-version heuristic controls this choice.

## macOS

A CLI installation writes `~/Library/LaunchAgents/dev.a.daemon.plist` and
uses `launchctl bootstrap gui/UID`; uninstall uses `bootout`. The installed
app provides **Start on connection** in its Command Line settings and
registers its bundled LaunchAgent through `SMAppService`. Approval-required
status links to Login Items settings. App registration is removed through
the app; the CLI does not bypass background-item approval.

Both agents request a launchd `SecureSocketWithKey` Unix stream socket and
consume it through `launch_activate_socket`. The initial `RunAtLoad` launch
publishes its absolute address into the private discovery record. This
bootstrap runs at registration/login, then obeys the same idle exit policy;
`KeepAlive` is absent. Installation waits for the initial verified socket
using a directory event and a bounded deadline, including app registration
that is already enabled. Approval-required registration stays pending. The bundled plist does not interpolate `$HOME`,
`$TMPDIR` or `~`. Socket mode is 0600, the process umask is 0077 and the stop
timeout is 30 seconds. Subsequent connects use the published manager socket
and the existing `getpeereid` checks. The managed daemon stays in the
foreground and does not call `setsid` or spawn a successor.

## Windows

Windows retains per-user self-spawn. A Windows service normally crosses a
privilege/session boundary, while a scheduled task introduces separate
registration, credentials and trigger behavior without a named-pipe
socket-activation contract. Neither improves the current per-user,
peer-verified pipe startup enough to justify another lifecycle. Activation
installation reports unsupported on Windows.
