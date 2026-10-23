# Native release tuning

Release uses main's level-z profile, fat LTO, one codegen unit, unwinding,
packed debuginfo and native allocators. CPU baselines remain conservative.
Size is report-only, but growth must be proportionate to the benefit.

A package override is retained only when CPU improves beyond observed spread
on most native targets and idle and peak RSS do not rise beyond noise.
Measurements use the [hosted workflow](hosted.md), five alternating reference
and candidate rounds, native counters and raw p10–p90 spreads. All other
packages retain main's profile.

## Open issues

- notify 8.2 `windows.rs` uses a 100 ms timed wait; Windows idle daemon CPU can increment by 15.625 ms. The OS watcher work will address this upstream or behind the rt/sys seam.
