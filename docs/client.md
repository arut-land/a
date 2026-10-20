# The client SDK (`crates/client`)

Every app (`a`'s line REPL, the terminal client, `a web`, the macOS app
through `crates/ffi`) is a renderer over this crate. It holds everything a
client of the daemon decides, once, so that no app derives it again; an
app holds screen state and draws.

## Layers

```
app (tui, web, REPL, ffi)          renders Effects; turns keys, clicks, lines into Cmds
  │ SessionClient::{attach, run, step, …} -> Vec<Effect>
  ▼
SessionClient (session.rs)         no I/O: Link FSM, SessionView (protocol::view), TurnView,
  │                                numbered mutations; COMMANDS/parse (command.rs)
  │ Effect::{Connect, Send, SendEnvironment, Hangup}      Event::{Connected, Msg, Closed, …}
  ▼
Driver (drive.rs)                  threads: connect + read per connection, one writer
  │
  ▼
transport (lib.rs, remote.rs)      Endpoint::{Local(Home), Ssh(host)}, connect_with, hello,
                                   Connection{reader, writer}, stop/status/restart/diagnose, bridge
```

- **Transport.** `connect(endpoint, staged, on_drain)` reaches this
  machine's daemon over its Unix socket (spawning it with zero lag: the
  client takes the daemon lock, binds the socket, hands both to a detached
  `a __daemon` and connects into its backlog) or another machine's through
  `ssh <host> a bridge`, whose stdio carries the same frames. It completes
  the hello: a `Welcome` (protocol and capabilities taken up, the slash
  commands the daemon advertises), a `Draining` daemon waited out and
  tried again, a `Mismatch` turned into the one sentence that says which
  side to update. `connect_with` sends the attach right behind the hello,
  saving a round trip. `stop`, `status`, `restart` and `diagnose` are what
  `a daemon …` runs; none starts a daemon but `restart`.
- **`SessionClient`.** One session as a client follows it, without I/O.
  `Link` is the connection's state machine: `Connecting` → `Live {synced}`
  → `Draining` → `Reconnecting {since}` → `Live`, or `Wedged(Diagnosis)`
  (the daemon took the connection and does not answer) or `Ended(Detached
  | Lost(why))`. The view is `protocol::view::SessionView`, folded here
  from every `DaemonMsg`; `TurnView` (running, asking, can interrupt, can
  steer, can send) is computed from it for the app's controls. Intents
  (`Cmd`) become numbered `ClientMsg`s, kept until the daemon's ack; a
  message sent while reconnecting goes out once the link is live again.
  What used to differ between clients is decided here: reconnecting after
  a drain by id from the last seq without sending the environment again, a
  detach never reading as a lost daemon, input given while reconnecting.
- **`Cmd` and `COMMANDS`.** `parse(line, asking)` turns a typed line into
  a `Cmd`: the client's own slash commands (`/sessions`, `/new`,
  `/interrupt`, `/steer`, `/env refresh`, `/detach` and its aliases,
  `/help`), `y`/`n` while a call waits on this client, else `Cmd::Input`,
  which the daemon reads (its own slash commands included). `help(daemon)`
  lists both tables for the app's help screen.
- **`Driver`.** Carries out the I/O effects: `Connect` opens connection
  `id` on a new thread (replacing the daemon first when `restart`), reads
  it there and posts each message to the sink as an `Event`; `Send` writes
  on the newest connection; `SendEnvironment` captures this process's
  environment again (or asks the `origin` given) and sends it; `Hangup`
  drops the writer. `apply` hands back the effects that are the app's
  (`View`, `Link`, `Sessions`, `Notice`), and a notice when a send failed.
  Cheap to clone; one per session client.
- **`Inbox`.** The one queue the app's thread sleeps on, posted to by the
  driver's and the input threads: a lock around a `VecDeque` rather than
  `mpsc`, which instantiates all three channel flavours per type (7–15 KiB
  each).
- **`SessionWatch`.** The daemon's session list as it changes, over a
  connection of its own that is made again when the daemon goes.

## How an app drives it

```rust
let (post, inbox) = client::inbox();
let driver = Driver::new(endpoint, staged, move |_, event| { let _ = post.send(Event::Daemon(event)); });
let mut session = SessionClient::new(Env::Origin);
pump(&driver, &mut app, session.attach(Attach::new(target, Env::Origin)));
while let Some(event) = inbox.recv() {
    let effects = match event {
        Event::Daemon(event) => session.step(event),
        Event::Line(line) => parse(&line, session.turn().asking).map_or(vec![], |cmd| session.run(cmd)),
    };
    pump(&driver, &mut app, effects);
    app.draw();
}

/// The fixpoint: the driver's effects are carried out, the app's are
/// shown, and whatever showing them asks for goes round again.
fn pump(driver: &Driver, app: &mut App, mut effects: Vec<Effect>) {
    while !effects.is_empty() {
        let shown = effects.into_iter().filter_map(|e| driver.apply(e)).collect();
        effects = app.absorb(shown);
    }
}
```

The terminal client (`apps/tui/src/run.rs`) and the web page
(`apps/web/src/app.rs`) write this loop; the line REPL
(`apps/cli/src/main.rs`) takes one pass, which is right only because its
`show` never yields an effect.

## Renderer rules

`crates/client/tests/one_rule.rs` searches every app's sources (Swift
included) for the signs of a rule copied out of the SDK and fails on any:

- No app decides what waits on whom from the approvals (`Approval::
  Pending`, a `pendingAsks` list of its own): `TurnView::asking` and
  `asks` say so.
- No app decides whether a turn runs (`var running`, `fn running`):
  `TurnView::running`.
- No app parses slash commands (`"/detach"`, `"/steer"`, …): `parse`.

A Rust file's own `#[cfg(test)]` module may build any state it likes.

Other rules the apps keep, by convention:

- The view is `SessionView`, folded by the SDK; an app re-derives nothing
  from the transcript. What it keeps is screen state: scroll anchor, folded
  cards, the input, overlays, the tab's page.
- Threads are detached. The driver's reader threads, the input thread and
  the reaper of a spawned daemon are never joined: process exit ends them,
  and the terminal is restored by `Terminal`'s drop before that. Nothing
  slow runs in a destructor; do not add a join.
- A client never sleeps on a timer (`crates/client/tests/no_polling.rs`,
  and the strace acceptance test on Linux): the inbox, the socket and the
  daemon lock are waited on.
- The first frame goes out before the daemon is reached; connecting (and
  spawning the daemon) happens on the driver's thread.

## Tests

- `crates/client/src/session.rs`: the link's transitions and what each
  one sends, by name: a reattach after a drain never resends the
  environment, a detach is never reported as a lost daemon, events from a
  replaced connection are dropped, input while reconnecting is sent once
  live and numbered across connections, a resend after a reconnect runs
  once, the turn says what runs and what waits on this client, a wedged
  daemon ends nothing until restarted.
- `crates/client/src/lib.rs`: the mismatch sentence, `stop` signalling
  only the process that recorded itself.
- `crates/client/tests/one_rule.rs`, `no_polling.rs`: the rules above.
- Black-box: `apps/cli/tests/{acceptance,crash,home,remote}.rs` drive `a`
  against a daemon in an isolated `A_HOME` (`mise run acceptance`).
