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

The [native activation probe](https://github.com/arut-land/a/actions/runs/40076822281)
at `610c8722` produced the following alternating-round observations on the
Ubuntu 24.04, macOS 15 ARM64 and macOS 26 Intel images. Times are
milliseconds; registration is excluded. Raw samples are retained alongside this
document.

| Hosted target | Self-spawn median | Activation median | Activation minus self-spawn |
| --- | ---: | ---: | ---: |
| [Linux ARM64](activation/linux-arm64.json) | 5.380 | 14.561 | +9.180 |
| [Linux x64](activation/linux-x64.json) | 6.093 | 11.445 | +5.352 |
| [macOS ARM64](activation/macos-arm64.json) | 35.423 | 50.828 | +15.406 |
| [macOS x64](activation/macos-x64.json) | 47.133 | 58.769 | +11.636 |

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

The [paired perf run](https://github.com/arut-land/a/actions/runs/40065757703)
compares `171fedd7` with main at `8a0388a7` in five alternating rounds per role.
The macOS x64 rows come from a [separate run](https://github.com/arut-land/a/actions/runs/40076860169)
of the same revisions on the macOS 26 Intel image.
That main revision predates the paths work inherited by this branch, so these
deltas include both paths and activation. [The generated comparison](activation/paired.csv)
retains every paired metric's medians, p10/p90, sample counts and source revisions.
Shipped sizes below exclude fixtures. Cold reply measures unmanaged startup;
it is separate from the native activation experiment above.

| Hosted target | Main bytes | Candidate bytes | Cold reply delta (ms) | Attach p99 delta (ms) | Idle RSS delta (KiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux ARM64 | 3,694,760 | 3,728,424 | +0.917 | +0.0001 | −28 |
| Linux x64 | 4,296,008 | 4,334,664 | +0.015 | +0.018 | +52 |
| macOS ARM64 | 3,330,768 | 3,364,144 | +0.677 | +0.035 | −48 |
| macOS x64 | 3,999,064 | 4,028,032 | +6.012 | −0.100 | +44 |
| Windows ARM64 | 4,876,288 | 4,893,696 | −1.993 | −0.026 | −120 |
| Windows x64 | 5,545,472 | 5,559,296 | −0.168 | −0.015 | −96 |

Daemon and line-client CPU medians remained zero in the ten-second idle windows
on these targets. Small positive or negative deltas can reflect runner noise;
the macOS x64 cold-reply samples spread over 40–68 ms in both roles, so its
median delta of 6 ms is inside that spread.
