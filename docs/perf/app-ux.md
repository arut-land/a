# macOS transcript investigation: scope stop

Phase 1 is **incomplete**. This report records baseline reproductions on
`5610d9e021bbf83044a80974c7f6eac147fa2564` (PROTO 43). No product fix, fixture
change, or performance improvement is claimed. Implementation stopped at the
owner's file-ownership boundary. No CI workflow was dispatched.

## Reproduction and attribution

The release macOS app and release fixture-enabled CLI were built on native
arm64 macOS 27.2. The real daemon used a scratch `A_HOME`, `[daemon] agent =
"echo"`, dummy credentials and a loopback provider URL. An external diagnostic
Swift test drove the production FFI and `SessionStore` through three turns:

1. `spawn child says hello`.
2. `spawn ` followed by 600 repetitions of `child output line\n`.
3. `par 2 spawn parallel child`.

The first turn produced a tool item at key 4 and a child item at key 5.
The tool's body was `The child session finished:\nchild says hello`; the
child's body was `child says hello`. The two rows have different keys:
this reproduction is duplicate presentation of one operation, rather than
duplicate Swift identifiers. The parallel turn produced two tool rows and
two child rows, with distinct child session IDs.

The large child produced a tool item at key 11 and a child at key 12. Its
tool body was `10828 bytes of text/plain, held as blob` followed by a blob
identifier. This is a real spilled tool outcome. It is not evidence that a
context-graph authority image leaked into the transcript. The generic
`Shown::Held` presentation in `crates/protocol/src/view/present.rs` turns that
reference into visible storage terminology.

The native app attached to the same session. Its simultaneous store dump
contained the same tool/child pairs. `ItemView` renders `.child` as a label
and ignores the FFI's existing `DisplayItem.navigation`, so its own transcript
is not reachable from that row. An in-process capture of the app's content
view shows the parallel tool cards followed by the two child labels.

Both other clients reproduce the shared symptom:

- The real web server's authenticated session event stream contains the task
  cards, `held as blob` text and four child links for the four actual children.
  This was inspected as emitted HTML, without a browser screenshot.
- A real `a attach` process in an isolated 180-column, 80-row PTY displays
  task content, `held as blob` and child content. Its raw terminal output is
  retained. This was not a manual interactive TUI review.

The shared reducer creates `Item::Tool` for `Body::ToolCall` and independently
creates `Item::Child` for `Body::Link`. Demand paging reconstructs both. The
evidence therefore locates this duplicate operation presentation below the
three renderers; it does not establish that history-paging introduced it.

## Scrolling

The native diagnostic moved the actual transcript `NSScrollView` upward by
250 points, then used a second FFI session attached to the same parent to
send `external update while scrolled up`. The scroll view reported:

| Observation | Clip-view bounds | `followsHistory` |
| --- | --- | --- |
| Before moving up | `(0, 6638, 883, 410)` | true |
| After moving up, before the external update | `(0, 6388, 883, 410)` | true |
| After the external update | `(0, 9405, 883, 410)` | true |

This is a programmatic native scroll reproduction, not a wheel/trackpad test.
It demonstrates loss of viewport position after an external update. The
`FollowEnd` view in `apps/apple/App/ChatView.swift` calls `scrollTo("end")`
on every revision whenever `followsHistory` is true. That flag tracks the
requested history range; it does not track whether the viewport is at the
bottom. Loading an older range is the only upward-scrolling path that turns
it off. Within a range, the viewport is therefore subject to unsolicited
scroll commands. The paging callback also chooses an end item of the new
page rather than retaining a visible anchor from the old page.

The existing
`demanded_history_replaces_the_native_window_without_losing_the_session`
Swift test passed on this baseline. It checks range replacement, byte bounds,
identity and returning to the tail, without instantiating a transcript view.
Its success does not establish reliable native scrolling or anchor stability.

## White space and performance: unresolved

The in-process PNG and native view hierarchy are retained. The PNG includes
white regions, but `NSView.cacheDisplay` does not faithfully capture every
composited/material surface. Neither the owner's white-space symptom nor
the responsible view has been conclusively identified. External
`screencapture` failed with `could not create image from display`;
`CGPreflightScreenCaptureAccess()` and `AXIsProcessTrusted()` both returned
false. No permission prompt or system permission change was attempted.

A three-second native `sample` capture of the release app succeeded. It
reports a single diagnostic physical footprint of 95.6 MiB, peak 102.8 MiB,
after the fixture transcript and external update. This idle capture is not
a long-session benchmark, a memory comparison or a timing distribution.
Main-thread time per update and scroll frame time have not been measured.
There is no after build, interleaved A/B series, median, speedup claim or
projection of savings. Those required measurements remain outstanding.

## Required ownership decision

The proposed shared fix needs an explicit typed association between a child
link and the call that created it, so public presentation can expose one
child operation. Awaited children have a later `WaitOn::Child { link }` fact,
but background children have no such wait. The owning call exists in the
private spawn intent and in the kernel's `spawn` arguments. Renderers should
not infer it from tool names, adjacent log entries or opaque authority JSON.

Additional non-test files requested before implementation:

- `crates/kernel/src/session.rs`: its `spawn` method already receives the
  owning `CallId`, but its `Body::Link` construction omits it. Change this
  production boundary to retain typed ownership for awaited and background
  children, rather than fixing only the awaited case in a renderer.
- `crates/protocol/src/lib.rs`: represent that association in the link type
  and change the protocol version if the chosen representation changes wire
  encoding. This exceeds ownership of the view reducer alone; permission
  for a version bump is not treated as permission to redesign durable facts.
- `crates/protocol/src/ledger.rs`: carry the association through the ledger's
  typed link projection. Its existing exhaustive link match would need to
  change with the fact type.

The representation must preserve the repository's minimal-durable-facts rule;
do not simply duplicate private intent fields without reviewing their ownership.
An alternative that derives public ownership entirely at the engine boundary
should be evaluated before choosing a durable format change. No implementation
of either approach has been started, and no necessity claim is made for a
particular wire design. The requested kernel scope is for the proposed typed
fact approach, not for adding feature-specific behavior to the kernel.

Failure likelihood is demonstrated for ordinary task calls, large spilled
results and parallel children. Cost is not measured: the proposed change needs
shared reducer/index invariants, regenerated fixtures where applicable, real
SDK/FFI/store replay tests, native UI verification and the required five-round
release A/B matrix. The Swift scroll fix and child navigation stay within the
existing allocation. White-space attribution remains necessary before changing
layout. No speculative robustness or security work was added.

## Checks, evidence and cleanup

- Release FFI package, release fixture CLI and release native app: built.
- External real-daemon FFI/store child diagnostic: passed and saved the
  failing product presentation; this is a probe, not a regression test.
- Existing Swift demand-history test: passed.
- `mise run check:windows`: passed on the unchanged product baseline.
- No Rust package was changed; package nextest suites were not run.
- No regression test or product patch is committed in this scope-stop report.

All Cargo commands ran through `mise exec --`, with `CARGO_BUILD_JOBS=2` and
the single external target directory specified in the brief. A scratch copy
of the mise configuration omitted its secret-file loader; runtime environments
were allowlisted. `TERM` was set to prevent FFI login-shell environment
acquisition. The app sources, generated bindings, Xcode project, Swift package
builds and diagnostic instrumentation were copied/generated only in scratch
storage. No `.env.local` or user `~/.a` was read.

Raw evidence is under:
`/private/tmp/claude-502/-Users-rajaniraiyn-work-a/57b9c5f0-cf3a-47ba-ba38-f417726d005b/scratchpad/r/app-ux`.
Useful files are `provenance.json`, `ffi-short.txt`, `ffi-large.txt`,
`ffi-parallel.txt`, `app-store.txt`, `window-0.png`, `native-view-tree.txt`,
`scroll-positions.txt`, `web-events.txt`, `tui-raw.txt`, `tui-plain.txt`,
`native-idle.sample.txt`, `swift-history-baseline.log` and
`windows-baseline.log`. Scratch app instrumentation changes automatic session
selection and captures diagnostics; product source remains unchanged.

The native app and web processes started for this investigation were stopped
by their own PIDs. The PTY probe exited. The isolated daemon was stopped with
`A_HOME=<scratch>/home a daemon stop`. Evidence and the external target directory
are retained for review; phase-2 cleanup and CI have not been performed.
