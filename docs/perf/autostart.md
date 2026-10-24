# Autostart measurements

Autostart adds no dependency. `scripts/autostart/hosted.py` runs on a
disposable hosted runner against its native installation; the perf
workflow's `autostart` job runs it on all six hosted targets weekly and on
demand (`gh workflow run perf -f autostart=true`). It checks the
behaviour in [autostart](../autostart.md) and alternates twelve rounds per
role, timing a complete `a ls` process through its first daemon reply:

- **self-spawn**: autostart off, no daemon running, so the client starts one;
- **first after login**: autostart on, the registered entry started the way
  the OS starts it at login (`systemctl --user start`, `launchctl bootstrap`,
  the `Run` value's command line), then the first `a ls` three seconds later;
- **warm**: the next `a ls` to that same daemon.

Times are medians in milliseconds from the
[probe run](https://github.com/arut-land/a/actions/runs/40088540155) at
`237ea748`; raw samples are retained alongside this document.

| Hosted target | Self-spawn | First after login | Warm | First after login minus warm |
| --- | ---: | ---: | ---: | ---: |
| [Linux x64](autostart/linux-x64.json) | 5.919 | 3.097 | 2.185 | +0.912 |
| [Linux ARM64](autostart/linux-arm64.json) | 5.723 | 2.769 | 2.149 | +0.621 |
| [macOS ARM64](autostart/macos-arm64.json) | 22.762 | 16.262 | 11.610 | +4.652 |
| [macOS x64](autostart/macos-x64.json) | 44.279 | 15.025 | 14.819 | +0.207 |
| [Windows x64](autostart/windows-x64.json) | 39.689 | 14.830 | 13.352 | +1.478 |
| [Windows ARM64](autostart/windows-arm64.json) | 56.639 | 19.213 | 17.447 | +1.766 |

The first connect after login costs what a connect to a daemon that has
served nobody yet costs, a little over a warm one, and avoids the
self-spawn on every target. The same rounds passed: isolated homes refused,
idle exit without autostart, residency with it (self-spawned and
login-started alike), the login entry deferring to a running daemon, a
daemon rewriting an outdated registration (Linux, macOS), `off` removing
the registration and its systemd enablement, the Windows entry leaving a
detached daemon, and `a.exe` embedding the `detached` console policy.

The [paired perf runs](https://github.com/arut-land/a/actions/runs/40086837175)
compare the branch with autostart off (`20905f90`; Windows at `e3f8d1cc` in
[x64](https://github.com/arut-land/a/actions/runs/40089709195) and
[ARM64](https://github.com/arut-land/a/actions/runs/40089711126) runs, which
add the console manifest) against `cf79f0dd`, main with the paths work and
no autostart, in five alternating rounds per role on the same runner.
Shipped sizes exclude fixtures.

| Hosted target | Reference bytes | Candidate bytes | Cold reply delta (ms) | Startup delta (ms) | Attach p99 delta (ms) | Idle RSS delta (KiB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Linux x64 | 4,349,896 | 4,364,744 | +0.051 | −0.014 | +0.008 | +48 |
| Linux ARM64 | 3,741,736 | 3,755,304 | −0.165 | 0.000 | +0.006 | +12 |
| macOS ARM64 | 3,364,192 | 3,397,344 | +2.992 | −1.806 | +0.073 | −80 |
| macOS x64 | 4,044,504 | 4,060,984 | −2.228 | −0.030 | +0.396 | −8 |
| Windows x64 | 5,553,664 | 5,569,024 | −0.940 | −0.043 | +0.072 | +8 |
| Windows ARM64 | 4,890,112 | 4,903,936 | +5.046 | +0.245 | +0.014 | −36 |

Most deltas lie inside a role's p10–p90 spread on the same runner (the
macOS ARM64 reference cold replies spread over 18–52 ms, Windows ARM64's
over 66–95 ms). Two fall outside both roles' spreads, on Windows x64: cold
reply −0.94 ms and attach p99 +0.07 ms; the run of `20905f90` there,
without the manifest, measured −0.11 ms and +0.008 ms. Daemon and
line-client idle CPU stayed at zero everywhere.

The cold path has nothing new to do: a client never reads the registration,
and an isolated or unregistered daemon asks once, on a blocking worker two
seconds after it starts serving.
