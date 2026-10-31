# The client SDK (`crates/client`)

Every app (`a`'s line REPL, the terminal client, `a web`, the macOS app
through `crates/ffi`) is a renderer over this crate. It holds everything a
client of the daemon decides, once, so that no app derives it again; an
app holds screen state and draws.

## Demanded history

`SessionClient` keeps control state, authored pending inputs, live reply tails and
its current requested transcript range. Attach receives a public `Head`: a
session/generation/log cut, item count, changed ordinals and controls. It does
not receive settled history or internal spawn/context authority images.

`demand(HistoryDemand)` replaces the presentation window. `HistoryAnchor`
selects the tail, an ordinal before/after, or a byte continuation within a large
text item. `HistoryDemand::older/newer` advances from the current position.
`SessionView::history` identifies the installed range and source byte offsets;
`items` contains that range alone. The default delivery allowance is 64 KiB,
independent of session length. Presentation and search text continues losslessly across requests;
an indivisible nontext item may exceed the allowance. Search starts a text
range at its match. Recall returns a complete editable input, independently
of the presentation allowance.

`read_history(reader, demand)` delivers `Show::History` independently of the
presentation window, for recall, search or export. Its `HistoryPage` carries
the fixed cut, exact item indices, total count and text continuation offsets.
`older()` and `newer()` preserve the scan filter and delivery allowance; pass
them to `read_history_at` at the page’s original cut. Normal new turns leave
that cut stable.
A new generation cancels old requests and emits `Notice::HistoryChanged`.
Closing a reader cancels outstanding delivery. There is no history-length cap.
Immutable item storage is shared by matching requested ranges within a process;
the cache holds weak references, so the last consumer releases the payloads.
`release_history()` also withdraws the presentation demand: subsequent heads do
not read a range already transferred to a renderer such as a browser DOM.

The daemon derives ordinal/fact positions from the log and reconstructs only
requested items at the requested cut. Public heads replace the former full-view
far-attach cache. `Synced` certifies controls; a renderer can wait separately for
`history_pending()` when it needs the requested range before acting.

`demand_sides(purposes)` declares completed side values the renderer needs as
metadata, independently of its transcript window. The thin daemon index keeps
only each purpose's latest completed ordinal and revision. A public head includes
coordinates only for declared purposes; the SDK resolves them through the same
range lane and weak page cache. The previous value stays visible until its
replacement page arrives. A new generation cancels outstanding pages and clears
old metadata; withdrawing a purpose releases its page. `SessionView::side(purpose)`
reads requested metadata, and `Change::Metadata` updates headers without replacing
presentation rows. The UI selects purposes; core layers do not name plugins.

## Child operations and stored output

A committed `Link.call` names the call that created the child. The shared
reducer replaces that call's tool row with a typed child item, preserving the
row key. Further children of the same call have their own link keys. Watches
update each child's status; completion never adds a second tool-result row.
The history index retains the contributing facts so a requested old cut can
still show the tool before admission and the child after admission. Input recall
retains the original editable command independently of its child presentation.

Child presentation includes a short work title and result summary.
`DisplayItem.navigation` names the child's session for native navigation; web
links and terminal row actions open that same session. It is not a context or
execution-authority image.

`Shown::Held` carries a retrievable output reference. Its text is a size label;
renderers show the shared `OPEN_OUTPUT` action and fetch the bytes on demand.
The FFI exposes this as `Presentation.content`, and `Session.output` starts a
cancellable `BlobFetch` on the attached session's authority. Storage identities
are retrieval parameters, never visible labels. A child's stored tool-return
summary can supply that output reference without restoring the tool card.

The native `SessionStore` still owns only the SDK's demanded range. Its
`TranscriptView` retains the preceding display page model while installing an
adjacent page, including separate identities for fragments of a large text
item (`HistoryWindow.text_key` and byte offset). Native `NSScrollView` geometry
tracks whether the viewport is at the bottom. Layout preserves a visible row
and its pixel offset when reading earlier output, and follows the bottom only
when pinned there. Revisions do not issue scroll commands. These display pages
are released with the transcript view; they do not extend SDK history demand.
An NSTableView realizes visible row views, caching measured heights instead of
keeping full-page layout trees. Off-screen reusable hosts clear their SwiftUI
content when removed, releasing text layout and backing layers.

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
Driver (drive.rs)                  pump: the one fixpoint loop; async transport tasks
  │                                on the supplied runtime
  ▼
transport (lib.rs, link.rs,        Daemon { link: Arc<dyn Link>, staged }: connect, connect_as,
           remote.rs, query.rs,    sessions, new_session, query, fetch; the one hello over any
           fetch.rs)               Link; Error, Connection, stop/status/restart/diagnose, bridge
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
  are written once, in `link::connect`, over any link. A daemon whose
  upgrade to a newer build must wait welcomes that build's client anyway,
  saying what holds it (`Connection::upgrade`); one that cannot serve it
  meanwhile says so in its `Draining` (`held`), which is an error, never a
  wait; the blocking API
  (`Daemon::connect`, `status`, `stop`) runs the same code on its caller's
  thread. A new transport is a new type, never an arm in an enum:
  - `LocalLink(Home)`: this machine's daemon over its socket, spawned
    with zero lag (the client takes the daemon lock, binds the socket,
    hands both to a detached `a __daemon` and connects into its backlog;
    one client starts it at a time, under `start.lock`, and the others
    retry a contended lock with cancellable runtime timers). Registered Linux
    installations instead ask their systemd user unit to start or replace the
    daemon and connect after its readiness notification. Isolated and unmanaged
    installations start directly. See [autostart](autostart.md).
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
  numbered `ClientMsg`s, kept until the daemon's ack or until the public head
  acknowledges this client's authored number or a later one
  (the ack is queued at commit, behind what the session already
  published, so it can trail the turn it started); a message sent while
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
  connection `conn` on the supplied runtime (replacing the daemon first when
  `restart`), reads it asynchronously and posts each message to the sink as an
  `Event`; `Send` writes on the newest connection (the session client
  numbered it); `SendEnvironment` captures this process's environment
  again (or asks the `origin` given) and sends it. Initial `Env::Origin`
  attaches ask the same callback locally and keep the bridge environment
  remotely; `Hangup` drops the
  writer. `pump(renderer, effects)` is the fixpoint every app runs: the
  `Io` is done, the `Show`s go to the `Renderer`, and what showing asks
  for goes round again; a send that failed is shown as a notice. Cheap to
  clone; one per session client.
- **`Inbox`.** The one queue the app's thread sleeps on, posted to by the
  driver's tasks and the input thread: a lock around a `VecDeque` rather than
  `mpsc`, which instantiates all three channel flavours per type (7–15 KiB
  each). `Driver::with_inbox` awaits space before reading another daemon
  event; the budget bounds pending delivery, never the history that can be
  opened. One oversized event can occupy an empty data lane. Priority input
  has its own byte and request accounting, and outgoing control writes and
  cancellation continue while delivery waits. Session connections carry an
  `Attach`; list watches carry only their client identity and reconnect
  without constructing a session attach.
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
  machine. `watch(hub, sink)` tells the list again whenever either file
  changes (an `rt::watch::Hub` subscription on each directory, entered only
  at its top, which follows a directory made later; the latest change's answer
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
- **Blobs.** `Daemon::fetch(session, blob, range)` (or
  `Connection::fetch`) reads a blob a session's log references, as its
  `BlobRef` names it, over a connection of its own: `range` is any
  `RangeBounds<u64>` (`..`, `..4096` for a head), clamped to the blob's
  end. `Fetched` is an `io::Read` of the bytes in order. Each chunk is
  matched by its (blob, offset) to the next byte due, and none may reach
  past the range asked for. The read that ends them fails with an
  `Unfetched` (`NotHeld`, `Cancelled`; `Unfetched::of(&error)`) if the
  daemon refused or stopped the fetch, and with `InvalidData` if a chunk
  came out of place or the count sent differs from what arrived. Its
  `canceller()` is a query's. The FFI's `BlobFetch` hands the pieces to
  the app with an async `next()` (`Bytes`, then `Done`, `NotHeld`,
  `Cancelled` or `Failed`), paced as a query's answers are.

## How an app drives it

```rust
impl Renderer for App {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect> { /* View, Link, Sessions, Notice */ }
}

let (post, inbox) = client::inbox(client::QueueBudget::default(), |_| 4096);
let driver = Driver::with_inbox(post.clone(), Input::Daemon).start(runtime, daemon);
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

Shared reducer vectors exercise the renderers' actual presentation and state
updates. The CLI parity cases compare the TUI, web and REPL at each fold;
Swift applies the same Rust-generated frames through the FFI and compares
its store with the reducer after every update, including retry replacement.
Turn controls come from `TurnView`; slash commands come from `parse`.

Other rules the apps keep, by convention:

- The view is `SessionView`, folded by the SDK; an app re-derives nothing
  from the transcript. What it keeps is screen state: scroll anchor, folded
  cards, the input, overlays, the tab's page.
- Transport and child reaping run as runtime tasks. Blocking SDK methods
  drive async operations over a shared one-worker runtime. Contended file
  locks retry nonblocking native attempts every 25 ms; cancellation drops
  the file and timer, leaving Storage workers available for lock holders
  and shutdown. Process creation uses the Storage pool. The terminal input thread is
  detached, and `Terminal` restores the terminal before process exit.
- Inbox and socket waits never poll (`crates/client/tests/no_polling.rs`
  and the strace cold-start acceptance test on Linux). Only lock contention
  retries on a timer; an uncontended start takes no timer path.
- The first frame goes out before the daemon is reached; connecting (and
  spawning the daemon) happens on the driver's runtime.

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
- `tests/no_polling.rs`: clients wait for events without polling.
- Black-box: `apps/cli/tests/blackbox/{acceptance,crash,home,remote}.rs`
  drive `a` against a daemon in an isolated `A_HOME` (`mise run
  acceptance` runs the first); `remote.rs`'s fake `ssh` refuses anything
  but plain words, as a non-POSIX login shell would.

Async consumers can use `Daemon::connect_async`, `Daemon::query_async`,
`Daemon::fetch_async`, `Answers::next_async` and `Fetched::read_async` on
an existing runtime. The FFI uses these with bounded async channels, so a
slow renderer yields the producer without blocking an executor worker.

Machine route resolution and multi-daemon session listing also use the
shared runtime. `Sources::resolve_async` runs `ssh -G` jobs on Storage;
`Daemons::sessions_async` opens and reads each member concurrently as tasks.
Their blocking entry points drive the same futures.

## User questions

`ViewState::question(call)` supplies a typed question and its introducing
sequence. `Offer::answer` and `Cmd::Answer` send that identity with keyed
responses. The daemon rejects stale identities before logging an answer.
`QuestionDraft` retains local text/defaults and parses one complete submission;
optional absence differs from an empty string or false. Policy approval
shortcuts build the same typed Confirm response. They only apply to gates.

The line REPL prints a `/answer CALL JSON` template and a decline command.
Form responses use `{"submit":{"field":"value"}}`, content-free confirmation
and URL responses use `{"accept":{}}`, and each response can instead be
`"decline"` or `"cancel"`. The envelope is
`{"question":QUESTION_SEQ,"responses":{"SERVER_KEY":RESPONSE}}`.
A state-only Continue prompt uses `{"continue":"proceed"}` (or `"decline"` /
`"cancel"` inside that response). Batches support a separate response for each
prompt. Client disconnect leaves questions open. Explicit Cancel is an answer; Stop
is an interrupt. Opening a URL is local and never submits an answer.
