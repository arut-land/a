# Active-work sleep holds

`sys::Awake` owns an optional idle-system-sleep assertion. The daemon
supervisor acquires it on the transition into work and drops it on the
transition out. An unavailable assertion is remembered until the next
transition, without retries or polling. Approval waits are not work; a
child still working keeps the daemon busy. Draining retains the assertion
until work settles or the daemon exits. The display may sleep and explicit
user sleep remains under OS policy.

macOS uses `IOPMAssertionCreateWithName` with
`PreventUserIdleSystemSleep`. `Drop` releases the assertion; the power
manager also removes assertions when their owning process dies.

Windows uses `PowerCreateRequest` with a UTF-16 reason and one
`PowerSetRequest(PowerRequestSystemRequired)`. The non-inherited
`OwnedHandle` is closed on acquisition failure and after `Drop` clears the
request. Process termination closes the handle even without unwinding.
The request belongs to the handle rather than a worker thread, so async
work may move between threads. Windows power policy can still limit the
hold, including on battery-powered Modern Standby systems.

Linux omits the optional hold. There is no existing D-Bus integration to
share; adding a bus connection and its dependency/runtime machinery only
for sleep inhibition is disproportionate. No new dependency is taken.
`systemd-inhibit` plus `sleep infinity` would add helper processes per busy
interval, fail to acknowledge acquisition, and risk retaining the sleep
hold after a daemon crash. A future shared native bus integration can
supply a logind inhibitor FD, owned by the daemon, if needed.

All backends have zero idle activity: no timer, thread, child process or
bus connection. Active native holds need no heartbeat either.

Native tests cover macOS assertion visibility and release, Windows
independent non-inherited handles and Unicode reasons, invalid reasons,
and the absence of a hold on unsupported platforms. CI supplies native
OS checks and paired hosted performance measurements.
