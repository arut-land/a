# The client SDK (`crates/client`)

Every app (`a`'s line REPL, the terminal client, `a web`, the macOS app
through `crates/ffi`) is a renderer over this crate. It holds everything a
client of the daemon decides, once, so that no app derives it again; an
app holds screen state and draws.

## Layers

```
app (tui, web, REPL, ffi)          a Renderer: shows Show effects; turns keys, clicks, lines into Cmds
  │ SessionClient::{attach, offer, run, step, …} -> Vec<Effect>
  ▼
SessionClient (session.rs)         no I/O: Link FSM, SessionView (protocol::view), TurnView, Offer,
  │                                numbered mutations; COMMANDS/parse (command.rs)
  │ Effect::Io(Connect | Send | SendEnvironment | Hangup)   Event { conn, kind: Connected | Msg | … }
  │ Effect::Show(View | Link | Sessions | Notice)
  ▼
Driver (drive.rs)                  pump: the one fixpoint loop; threads: connect + read per
  │                                connection, one writer
  ▼
transport (lib.rs, remote.rs)      Daemon { endpoint: Local(Home) | Ssh(host), staged }: connect,
                                   connect_as, sessions, new_session; hello, Error, Connection,
                                   stop/status/restart/diagnose, bridge
```

- **Transport.** `Daemon { endpoint, staged }` is the daemon as a client
  reaches it: this machine's over its Unix socket (spawned with zero lag:
  the client takes the daemon lock, binds the socket, hands both to a
  detached `a __daemon` and connects into its backlog; one client starts
  it at a time, under `start.lock`, and the others wait on that lock, not
  on a timer) or another machine's through `ssh <host> a bridge`, whose
  stdio carries the same frames. [Autostart](autostart.md) only starts the
  daemon earlier, at login; it never changes this path. `connect` completes the hello: a
  `Welcome` (protocol and capabilities taken up, the slash commands the
  daemon advertises), a `Draining` daemon waited out and tried again, a
  mismatch as `Error::Mismatch { theirs, newer, host }`, whose `Display`
  is the one sentence that says which side to update. `connect_as` sends
  the attach right behind the hello, saving a round trip. `Error` is the
  typed reason a daemon could not be reached or stopped serving
  (`Mismatch`, `NotResponding`, `Refused`, `Closed`, `Io`); it converts
  to `io::Error` for the commands that report that way. `stop`, `status`,
  `restart` and `diagnose` are what `a daemon …` runs; none starts a
  daemon but `restart`. The daemon lists sessions newest first; no client
  sorts, and `SessionSummary` prints the `a ls` line.
- **`SessionClient`.** One session as a client follows it, without I/O.
  `Link` is the connection's state machine: `Connecting` → `Replaying` →
  `Live` → `Draining` → `Reconnecting {since}` → `Replaying`, or
  `Wedged(Diagnosis)` (the daemon took the connection and does not answer)
  or `Ended(Detached | Lost(Error))`. The view is
  `protocol::view::SessionView`, folded here from every `DaemonMsg`;
  `TurnView` (running, asking, can interrupt, can send) combines that view
  with pending submitted mutations: input awaiting publication or an ack
  can be interrupted even while the local view is idle. Intents become
  numbered `ClientMsg`s, kept until the daemon's ack; a message sent while
  reconnecting goes out once the connection is up again. `offer()` refuses
  with a typed `Notice` unless attached. Each intent (`input`, `steer`,
  `force_steer`, `withdraw(seq)` of the user's queued input,
  `interrupt`, `approve(call)`, `refresh_env`) is accepted only when the
  turn takes it, otherwise returning a `Notice` saying why. `run(Cmd)`
  goes through the same offers and shows the notice. Cancellation has room beyond the normal mutation
  window; repeated interrupts coalesce until another mutation is submitted.
  What used to differ between clients is decided here:
  reconnecting after a drain by id from the last seq without sending the
  environment again, a detach never reading as a lost daemon, input given
  while reconnecting.
- **`Cmd` and `COMMANDS`.** `parse(line, asking)` turns a typed line into
  a `Cmd`: the client's own slash commands (`/sessions`, `/new`,
  `/interrupt`, `/steer`, `/env refresh`, `/detach` and its aliases,
  `/help`), `y`/`n` while a call waits on this client, else `Cmd::Input`,
  which the daemon reads (its own slash commands included). `help(daemon)`
  lists both tables for the app's help screen.
- **`Driver` and `pump`.** Carries out the `Io` effects: `Connect` opens
  connection `conn` on a new thread (replacing the daemon first when
  `restart`), reads it there and posts each message to the sink as an
  `Event`; `Send` writes on the newest connection (the session client
  numbered it); `SendEnvironment` captures this process's environment
  again (or asks the `origin` given) and sends it; `Hangup` drops the
  writer. `pump(renderer, effects)` is the fixpoint every app runs: the
  `Io` is done, the `Show`s go to the `Renderer`, and what showing asks
  for goes round again; a send that failed is shown as a notice. Cheap to
  clone; one per session client.
- **`Inbox`.** The one queue the app's thread sleeps on, posted to by the
  driver's and the input threads: a lock around a `VecDeque` rather than
  `mpsc`, which instantiates all three channel flavours per type (7–15 KiB
  each).
- **`SessionWatch`.** The daemon's session list as it changes, over a
  connection of its own that is made again when the daemon goes.

## How an app drives it

```rust
impl Renderer for App {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect> { /* View, Link, Sessions, Notice */ }
}

let (post, inbox) = client::inbox();
let driver = Driver::new(daemon, move |_, event| { let _ = post.send(Input::Daemon(event)); });
let mut app = App { session: SessionClient::new(Env::Origin), .. };
driver.pump(&mut app, app.session.attach(Attach::new(target, Env::Origin)));
while let Some(input) = inbox.recv() {
    let effects = match input {
        Input::Daemon(event) => app.session.step(event),
        Input::Line(line) => parse(&line, app.session.turn().asking).map_or(vec![], |cmd| app.session.run(cmd)),
    };
    driver.pump(&mut app, effects);
    app.draw();
}
```

The terminal client (`apps/tui/src/run.rs`), the web page
(`apps/web/src/app.rs`), the line REPL (`apps/cli/src/main.rs`) and the
FFI feed (`crates/ffi/src/live.rs`) are `Renderer`s over this one loop;
`client::pump` is the same loop with any I/O, which tests use with none.

## Renderer rules

`crates/client/src/one_rule.rs` searches every app's sources (Swift
included) for the signs of a rule copied out of the SDK and fails on any:

- No app decides what waits on whom from the approvals (`Approval::
  Pending`, a `pendingAsks` list of its own): `TurnView::asking` and
  `asks` say so.
- No app decides whether a turn runs (`var running`, `fn running`):
  `TurnView::running`.
- No app parses slash commands (`"/detach"`, `"/steer"`, …): `parse`.
- No app decides when a reply stops streaming (`part == .outcome`): the
  FFI's every update names the call whose reply is streaming.

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
  one, the turn says what runs and what waits on this client, an offer
  is made only while attached and each intent only when the turn takes
  it, a wedged daemon ends nothing until restarted.
- `crates/client/src/drive.rs`: the pump runs until the renderer asks for
  nothing more.
- `crates/client/src/lib.rs`: the mismatch sentence, `stop` signalling
  only the process that recorded itself, a lock let go after the wait is
  not kept.
- `crates/client/src/one_rule.rs` (a test module), `tests/no_polling.rs`:
  the rules above.
- Black-box: `apps/cli/tests/blackbox/{acceptance,crash,home,remote}.rs`
  drive `a` against a daemon in an isolated `A_HOME` (`mise run
  acceptance` runs the first).
