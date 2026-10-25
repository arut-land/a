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
transport (lib.rs, link.rs,        Daemon { link: Arc<dyn Link>, staged }: connect, connect_as,
           remote.rs, query.rs)    sessions, new_session, query; the one hello over any Link;
                                   Error, Connection, stop/status/restart/diagnose, bridge
```

- **Transport.** `Daemon { link, staged, pinned }` is the daemon as a
  client reaches it; `pinned` is the machine it must be, where
  `machines.toml` says, and a welcome naming another is
  `Error::OtherMachine`, before anything the connection would send behind
  the hello (an attach and its environment) goes out. The pin refuses a
  mismatch; it does not authenticate, since the far daemon's claim of its
  machine is unproven until keys are exchanged. A `Link` (`link.rs`) opens a byte stream to one daemon,
  ready for the hello, and says what only it knows: whether opening may
  need the person at the terminal (`needs_terminal`), what can be told of
  a daemon that never answered (`unanswered`), and which side to update
  on a mismatch. The hello, its timeout, the drain wait and the retries
  are written once, in `link::connect`, over any link; the blocking API
  (`Daemon::connect`, `status`, `stop`) runs the same code on its caller's
  thread. A new transport is a new type, never an arm in an enum:
  - `LocalLink(Home)`: this machine's daemon over its socket, spawned
    with zero lag (the client takes the daemon lock, binds the socket,
    hands both to a detached `a __daemon` and connects into its backlog;
    one client starts it at a time, under `start.lock`, and the others
    wait on that lock, not on a timer). [Autostart](autostart.md) only
    starts the daemon earlier, at login; it never changes this path.
  - `SshLink { host, prompt, home }` (`remote.rs`): another machine's, through
    the system `ssh` to `a bridge`, whose stdio carries the same frames
    after a sync line and the host's environment. The remote login shell
    is given only plain words, which any shell parses alike. A POSIX host
    (whatever its login shell, csh and fish included) runs `sh -s` with a
    discovery script on stdin, which says `A-SH <token>` first and then
    runs the `a` on `PATH`, else the one its runtime directory holds (as
    the host's `A_HOME` or native layout says); `uname -sm` comes before
    installing this build where neither is found. Only a host that never
    said `A-SH` (no `sh`: cmd or PowerShell on Windows) is asked for
    `a bridge <token>` directly, which serves where its `a` is on PATH; an
    `a` that sh found and that failed is reported as it failed. A script is grouped, so `sh` reads it whole before
    running any of it; the upload follows only once the script says
    `ready`, since a shell such as dash reads ahead. `Prompt::Never`
    (`a web`) runs ssh with `BatchMode=yes` and no terminal, and reads
    what it says, so a host that needs a password or a host key confirmed
    is `LinkError::NeedsInteractiveAuth`, not a prompt nobody sees.
    `host` is a `client::Host`, never an ssh option; `home` is this
    machine's, whose machine id the hello names to the far daemon.

  `connect` completes the hello: a
  `Welcome` (protocol and capabilities taken up, the slash commands the
  daemon advertises) and the `Connection`'s `machine`, the daemon's, a `Draining` daemon waited out and tried again, a
  mismatch as `Error::Mismatch { theirs, newer, host }`, whose `Display`
  is the one sentence that says which side to update. `connect_as` sends
  the attach right behind the hello, saving a round trip. `Error` is the
  typed reason a daemon could not be reached or stopped serving
  (`Mismatch`, `NotResponding`, `Refused`, `Closed`, `Link(LinkError)`,
  `OtherMachine`, `Io`); it converts to `io::Error` for the commands that report that
  way. `stop`, `status`, `restart` and `diagnose` are what `a daemon …`
  runs; none starts a daemon but `restart`. The daemon lists sessions
  newest first; no client sorts, and `SessionSummary` prints the `a ls`
  line.
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
- **`Daemons`.** The daemons a client holds, one per machine, each a
  `Member { name, daemon }`; the first is the default, where a new session
  starts. Every app holds one today (this machine's as `local`, or
  `--host`'s), so nothing changes for it; `a attach mac2:3f9a` adds the
  machine it names. `resolve(target)` says which member a target is on and
  what to ask it: `mac2:<prefix>` (a member's name, or its pinned machine
  id) goes to that member as `<prefix>`, a bare prefix to the one member,
  or with several to the one whose list holds it (`Target::Id`; a prefix
  found nowhere names the machines that did not answer, where it may be),
  and what makes a session to the first. `sessions()` asks every member at once and
  answers each machine's list, or why it could not, apart: one machine
  away never hides the others. A `Listed` is a summary with its member's
  name and machine (`address()`, a `protocol::Address`), shown
  `mac2:3f9a…`. No daemon ever stores an address for its own sessions:
  each lists only its own, and the machine comes from the connection.
- **Machines.** `Sources { machines, ssh }` is where the names come from:
  `machines.toml` (`sys::config::machines`; a machine's name, its `ssh`
  host, its pinned `id`) first, then each `Host` of the ssh configuration
  that is not a pattern and that no machine is named for, its `Include`s
  followed where they stand as ssh follows them (`~` the home, a relative
  path in `~/.ssh`, globs in order, at most 16 deep). `named(name)` reads
  the ssh configuration only when `machines.toml` names none. Only `Host`
  and `Include` lines are read; what a host is comes from the system `ssh -G` (`Route {
  user, hostname, port }`), so `Match`, `Include`, tokens and the system
  file mean what ssh says, and a second alias of one route is left out.
  `resolve()` runs one `ssh -G` per machine, all at once, and only when
  asked: no command's start reads either file unless a target names a
  machine. `watch(rt, sink)` tells the list again whenever either file
  changes (`rt::Watcher` on their directories, the latest change's answer
  only; an included file outside `~/.ssh` is read again with them, but its
  own change is not watched). DNS-SD browsing is a later source beside
  these.
- **`SessionWatch`.** Every member's session list as it changes, each over
  a connection of its own that is made again when its daemon goes, merged
  in the set's order. The sink gets the merged list and a `Problem { on,
  error }` for each member that cannot list now, which lists nothing until
  it is reached again; the others' lists stay.
- **Queries.** `Daemon::query(service, scope, body)` (or
  `Connection::query`) asks one of the queries the welcome advertised
  (`Connection::queries`) over a connection of its own, so its answers
  never wait behind a session's replay. `Answers` iterates its `Update`s,
  then its `Reply(Outcome)`; a `Canceller` stops it from another thread,
  and dropping both hangs up, which cancels it too. The query goes out
  right behind the hello. The FFI's `QueryStream` is this, its answers
  taken with an async `next()` a few ahead of the app: none is dropped,
  as the daemon streams no faster than the app takes them.

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
- `crates/client/src/remote.rs`, `tests/ssh.rs`: a shell reading ahead
  leaves the bridge nothing of the script; a link that may not prompt
  says a person must authenticate instead of asking.
- `crates/client/src/lib.rs`: the mismatch sentence, `stop` signalling
  only the process that recorded itself, a lock let go after the wait is
  not kept.
- `crates/client/src/one_rule.rs` (a test module), `tests/no_polling.rs`:
  the rules above.
- Black-box: `apps/cli/tests/blackbox/{acceptance,crash,home,remote}.rs`
  drive `a` against a daemon in an isolated `A_HOME` (`mise run
  acceptance` runs the first); `remote.rs`'s fake `ssh` refuses anything
  but plain words, as a non-POSIX login shell would.
