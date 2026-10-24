# Activation measurements

The activation adapter uses `listenfd` 1.0.2 only on Linux. A target-specific
`cargo tree -p sys --target x86_64-unknown-linux-gnu --edges normal --depth 2`
shows its only Linux dependency is the existing libc. Resolving libsystemd
0.7.2 instead added twelve packages, including nix, nom, hmac and a second
RustCrypto digest/SHA-2 generation. Those facilities are unused by this
socket-only integration. `listenfd` supplies the documented descriptor
handoff without a local protocol implementation; sys additionally requires
the current PID, exactly one descriptor and the standard fd-3 origin.
Windows-only listenfd dependencies are not built by this Linux dependency.

`scripts/activation/hosted.py` measures the complete `a ls` process through
its first daemon reply, with an already installed executable and warm file
cache. It alternates twelve self-spawn and manager-activation rounds on the
same hosted runner. Registration and initial discovery bootstrap are outside
the timed interval. Managed rounds check rapid stop/reconnect separately, then
wait for a real idle exit before timing the next connect. Each round proves
idle exit without connecting to the
socket; status inspection must leave it idle. An activated socket must keep
its inode across daemon exit/reactivation. Uninstall must leave another native
home running and locate its own artifacts after a configuration-base change.
A home without a registration cannot adopt an activation descriptor. A manager
pinned to an older executable must serve a newer staged build after one drain;
a fixture-only agent verifies the actual image, not just its advertised ID.
Results contain every sample, medians, their difference and the measured
executable's bytes. The activation
probe build includes fixtures; shipped binary size comes from the normal
six-target perf workflow instead.

The ordinary perf workflow compares the unchanged unmanaged startup path
against main in alternating same-runner rounds, including startup, attach,
idle memory and CPU accounting. Native activation is optional and does not
change the idle exit deadline or add a resident helper.

The [native activation probe](https://github.com/arut-land/a/actions/runs/40065815817)
at `6fac0d70` produced the following alternating-round observations. Times are
milliseconds; registration is excluded. Raw samples are retained alongside this
document.

| Hosted target | Self-spawn median | Activation median | Activation minus self-spawn |
| --- | ---: | ---: | ---: |
| [Linux ARM64](activation/linux-arm64.json) | 5.103 | 11.994 | +6.892 |
| [Linux x64](activation/linux-x64.json) | 4.756 | 10.655 | +5.899 |
| [macOS ARM64](activation/macos-arm64.json) | 27.038 | 33.719 | +6.681 |
| [macOS x64](activation/macos-x64.json) | 30.970 | 37.547 | +6.576 |

These targets passed idle exit, socket inode preservation, cross-home
uninstall, rejection without registration
and upgrade from a manager pinned to an older executable. Activation is an
opt-in lifecycle integration; these measurements do not support a latency
advantage over self-spawn.

Linux also passed uninstall after changing the configuration base. Both macOS
targets passed rapid stop/reconnect with a one-second launchd throttle, as well
as the idle-exit timing rounds. The Swift app build and tests passed in
[CI](https://github.com/arut-land/a/actions/runs/40065827570). Interactive
SMAppService approval on a signed installation remains a manual validation.
