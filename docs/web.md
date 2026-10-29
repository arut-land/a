# The web UI (`a web`, `apps/web`)

`a web` is a client process like the terminal client: it serves a browser
UI on a random port of 127.0.0.1, prints a link that works once, opens the
browser (`--no-open` only prints), and runs until stopped. Closing it stops
only the UI; sessions keep running in the daemon. Every page and patch is
rendered in Rust (hypertext's typed markup); in the browser, Datastar
applies the patches each tab's event stream sends. There is no JavaScript
of ours.

## Design

```
browser tab ──GET /s/<id>/events?tab=…──> Live: SessionClient + Page + Driver   (one per tab)
            <──text/event-stream: datastar-patch-elements──┘
            ──POST /api/{send,approve,interrupt} (signals: tab, draft)──> App::tell(tab, TabIn)
```

- **Server** (`http.rs`): a small HTTP/1.1 server for one local user, one
  request per connection, answered whole or as an event stream that lasts
  until the tab goes away. It runs on the `rt` runtime (`rt::Threads`):
  the listener and each connection wait on their sockets through its
  reactor (`rt::Async`), so neither a request nor an open tab holds a
  thread, and the app runs on its async workers. `httparse` parses; the
  `http` types are the ones `transport`
  already links. No tokio, hyper or axum (`deny.toml`).
- **Gate** (`http.rs`, `Gate`): who may use the UI. The printed link
  carries a one-time token; redeeming it sets a cookie for that host.
  Browsers open at most six HTTP/1.1 connections to a host and each chat
  tab holds one open for its stream, so a chat page moves to a host of its
  own, `t<label>.localhost`, by a redirect that carries a token good once
  for sixty seconds (`handoff`); a cookie is per host, so each tab host
  signs in that way. Requests to another port, another host or a foreign
  origin are refused.
- **Routes** (`app.rs`, `App::handle`): `GET /` the session list (or why
  there is none); `POST /new` starts a session in the directory `a web`
  runs in, as `a new` does, and redirects to it; `GET /s/<id>` the chat
  page (through the handoff when it is asked for on the main host);
  `GET /s/<id>/events?tab=<32 hex>` the tab's stream; `POST /api/send`,
  `/api/approve?call=&ok=`, `/api/interrupt` with the page's signals
  (`tab`, `draft`) as the body. A call for a tab that is not live answers
  404 so the page reloads.
- **A live tab** (`app.rs`, `Live`): a `SessionClient` of its own (the
  SDK's, docs/client.md), a `Page` rendered from it, and the `Driver`
  under it, so each tab is its own daemon client and detaches when its
  stream is dropped (the tab closed or reloaded). The driver's events and
  the tab's calls (`TabIn::Line`, `TabIn::Cmd`) arrive on one channel; the
  page's patches go out after each batch (`Page::settle`).
- **The page** (`page.rs`, `view.rs`, `patch.rs`): the chat page renders
  at once without its transcript; the transcript arrives over the stream,
  in requested ranges after controls are synced. Each delivered range is
  appended to the native DOM, and its SDK payloads are released. All delivered
  history remains available to native find, selection, copy and accessibility;
  offscreen content is not hidden. Changed ordinals replace their stable DOM
  wrappers. Delivery batches admit one oversized committed item when needed
  to render its canonical Markdown once; partial discovery ranges stay out
  of the DOM. Active replies stream independently while history transfers. Every item renders inside an element of its own (`#i{key}`)
  so a change replaces just that item; new items go in before `#end`,
  which holds the Stop button while a turn runs, the daemon's latest
  complaint, and whether it went away. Streamed text is appended to the
  reply's element as it comes. Patches are Datastar's
  `datastar-patch-elements` events, framed here (the `datastar` crate's
  builder buffers lines and misses a lone `\r`, which also ends an
  event-stream line).
- **Styles** (`style.rs`): inline `style=` strings, no stylesheet; colors
  are custom properties under `light-dark()`, layouts fluid.
- **The script**: Datastar 1.0.4 as published, committed deflated beside
  the source (`assets/datastar-1.0.4.js.zz`, 13 KB of 34), embedded and
  served compressed under a versioned path so browsers keep it. A unit test
  inflates it and holds it to the `.js` byte for byte.
- **Composer admission**: `admitted` is `false` until the server accepts a
  draft, then holds that draft until the effect consumes it. The effect
  clears the composer only if it still contains the accepted text, and
  resets `admitted` to `false`. Datastar [removes signals assigned
  `null`](https://data-star.dev/reference/attributes#data-signals); reading
  a missing signal creates an empty string. Using `null` here caused the
  effect to repeatedly read and remove its own dependency, overflowing the
  browser stack even in an empty chat. Keeping the signal present removes
  that loop without changing Datastar or transcript patches.

## Running and testing

- `mise run web`: the dev build against an isolated home
  (`target/web-home`) and the model-backed agent with `.env.local`'s keys
  (`agent = "echo"` in its `config.toml` for none); its daemon stops with
  it.
- `mise run test:web`: `apps/cli/tests/blackbox/web.rs`, black-box against `a web`
  in an isolated `A_HOME` with the echo agent: the link signs in once and
  foreign requests are refused, a chat moves to a host of its own, a tab
  streams replies and answers asks until it closes, a long transcript sends
  each item once, a tab rides out a daemon upgrade.
- Unit tests: the gate (`a_host_is_ours_on_our_port_only`, the cookie and
  handoff rules), the page's patches for an attach, a delta and an update,
  the event framing, the embedded script.
- Browser regression: `a_live_composer_never_throws_or_discards_a_newer_draft`
  runs the served page and committed Datastar in real headless Chromium,
  catches uncaught exceptions from initial attach through live echo turns,
  and preserves edits made while an admission response is pending. It is
  part of `mise run test:web` and the full suite, without an ignore marker.
  `test:web` requires Node and npm and prepares `playwright-core@1.64.1`
  before running the tests. It uses installed Google Chrome on Linux and
  macOS; without Chrome it downloads the Chromium revision pinned by that
  driver. CI runs `test:web` before the full suite, using the runner image's
  Chrome and Node. Direct Cargo invocations need this preparation once:
  `node apps/web/test-browser.mjs`.
  The driver and downloaded browser live under `CARGO_TARGET_DIR` when set,
  otherwise the OS temporary directory, never in the source tree.
  The driver has no npm dependencies and adds no Rust or shipped dependency;
  it exercises the actual Datastar signal engine that expression-only tests
  cannot cover. On the M3, the driver-only install added one package
  (13.3 MiB; npm reported 658 ms), with no browser download when using
  installed Chrome.

## Known costs

Each live tab's daemon client is a `client::Driver`, with the threads its
connection takes. Styles repeat per
item (~200 bytes); a `<style>` block with class names would shrink every
patch. Neither changes what the page shows.

## Fork a conversation

`/fork [seq]` copies the current conversation at a rest point and switches to it.
Files stay as they are; this does not rewind or copy the workspace. The fork
keeps its recorded cwd and environment. See [fork semantics](fork.md).

## User questions

Typed server forms render string/number inputs, boolean and choice selectors,
multiple selection, defaults and optional-field inclusion. Submit, Decline and
Cancel send JSON in a POST body; the daemon validates the exact question
sequence. Invalid fields remain editable. URL questions show the complete
address and an explicit browser link with no opener or referrer. Opening a
link does not submit consent or completion.
