/// [`out`]s a formatted line.
macro_rules! sayln {
    () => {
        $crate::out(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::out(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// [`out`]s formatted text.
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::out(format_args!($($arg)*))
    };
}

macro_rules! errln {
    ($($arg:tt)*) => {
        $crate::write(std::io::stderr().lock(), 1, format_args!("{}\n", format_args!($($arg)*)))
    };
}

mod args;
mod exts;
mod once;

use std::fmt;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::thread;

use client::{Daemons, Host, Member};
use client::{Driver, End, Env, Event, Link, Renderer, SessionClient, Show};
use protocol::Target;
use protocol::view::{
    Change, Item, Kind, Part, Presented, SessionView, Shown, answer_line, mismatch_line, recap_line,
};
use sys::{Home, Staged};

use crate::args::{Args, Autostart, Cmd, Daemon};

fn main() -> ExitCode {
    sys::small_footprint();
    // The daemon never wants one: whoever starts it gives it no terminal.
    if std::env::args_os()
        .nth(1)
        .is_none_or(|first| first != "__daemon")
    {
        sys::console::attach_when_launched_without_one();
    }
    // Help, the version, completion requests and usage errors (status 2)
    // end the process here.
    let args = Args::parse();
    let (prompt, mode) = (args.prompt, args.mode);
    let result = match args.cmd {
        Some(Cmd::Daemon { action }) if prompt.is_none() => {
            daemon_cmd(args.host.as_ref(), action).map(|()| ExitCode::SUCCESS)
        }
        cmd => daemon_of(args.host.as_ref(), cmd.as_ref()).and_then(|(home, daemons)| {
            let daemon = &daemons.first().daemon;
            let done = match cmd {
                None => return session(&home, &daemons, Target::New, prompt, mode),
                Some(Cmd::Resume { id: None, any }) => {
                    let latest = Target::Latest { any_workspace: any };
                    return session(&home, &daemons, latest, prompt, mode);
                }
                Some(Cmd::Resume { id: Some(id), .. } | Cmd::Attach { id }) => {
                    return session(&home, &daemons, Target::Prefix(id), prompt, mode);
                }
                Some(_) if prompt.is_some() => {
                    eprintln!("a: -p runs a turn: it takes no command but `resume` or `attach`");
                    return Ok(ExitCode::from(2));
                }
                Some(Cmd::Ls) => ls(daemon),
                Some(Cmd::New) => new(daemon),
                Some(Cmd::Fork { session }) => fork(&home, &daemons, &session),
                Some(Cmd::Bridge { token }) => client::bridge(
                    here(&home, args.host.as_ref())?,
                    token.as_deref().unwrap_or(&daemon.staged.id),
                ),
                Some(Cmd::Web { no_open }) => web(daemons.clone(), !no_open),
                Some(Cmd::Daemon { .. }) => {
                    unreachable!("daemon commands are handled before installation")
                }
                Some(Cmd::Serve {
                    login,
                    listen_fd,
                    detached,
                    lock_fd,
                    reset_breaker,
                    handover_fd,
                }) => serve(
                    here(&home, args.host.as_ref())?,
                    &daemon.staged,
                    Handoff {
                        listen: listen_fd,
                        lock: lock_fd,
                        handover: handover_fd,
                    },
                    reset_breaker,
                    detached,
                    login,
                ),
            };
            done.map(|()| ExitCode::SUCCESS)
        }),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("a: {}", client::sanitize(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

/// The status `a` ends with when stdout's reader has gone: 128 + SIGPIPE,
/// as a program SIGPIPE ended would.
const STDOUT_GONE: i32 = 141;

/// Writes `args` to stdout, at once. A reader that has gone (`a ls | head
/// -1`) ends `a` there, quietly, as SIGPIPE ends a C program; a session it
/// was attached to goes on in the daemon.
fn out(args: fmt::Arguments<'_>) {
    write(io::stdout().lock(), 0, args);
}

fn write(mut output: impl Write + IsTerminal, stream: usize, args: fmt::Arguments<'_>) {
    static TERMINALS: std::sync::LazyLock<std::sync::Mutex<[client::Sanitizer; 2]>> =
        std::sync::LazyLock::new(Default::default);
    let written = if output.is_terminal() {
        let text = TERMINALS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[stream]
            .push(args.to_string().as_bytes());
        output.write_all(text.as_bytes())
    } else {
        output.write_fmt(args)
    };
    if let Err(e) = written.and_then(|()| output.flush())
        && e.kind() == io::ErrorKind::BrokenPipe
    {
        sys::terminal::restore();
        std::process::exit(STDOUT_GONE);
    }
}

/// Entry points that attach resolve their installed image first, so a daemon
/// they may spawn runs the newest installed build. `a web` serves pages, not
/// a person at its terminal, so ssh may not ask there. The set holds the
/// one daemon `--host` names, else this machine's.
fn daemon_of(host: Option<&Host>, cmd: Option<&Cmd>) -> io::Result<(Home, Daemons)> {
    let home = Home::from_env()?;
    let staged = home.runtime().stage(&std::env::current_exe()?)?;
    // What a daemon of this build can be handed, said where the daemon it
    // replaces looks: a failure only costs that daemon's waiting it out.
    let _ = home.runtime().declare_handover(&staged.id);
    let prompt = match cmd {
        Some(Cmd::Web { .. }) => sys::Prompt::Never,
        _ => sys::Prompt::Terminal,
    };
    let daemons = match host {
        Some(host) => Daemons::one(
            host.as_str(),
            client::Daemon::new(
                client::SshLink::new(host.clone(), prompt, home.clone()),
                staged,
            ),
        ),
        None => Daemons::one(Member::LOCAL, client::Daemon::local(home.clone(), staged)),
    };
    Ok((home, daemons))
}

/// The set with the machine `target` names (`a attach mac2:3f9a`), from
/// `machines.toml` or the ssh configuration, where the set lacks it.
fn reaching(home: &Home, daemons: &Daemons, target: &Target) -> io::Result<Daemons> {
    daemons.clone().naming(
        target,
        &client::Sources::of(home),
        sys::Prompt::Terminal,
        home,
    )
}

/// This machine's home, for a command that acts on this machine only.
fn here<'a>(home: &'a Home, host: Option<&Host>) -> io::Result<&'a Home> {
    match host {
        None => Ok(home),
        Some(host) => Err(io::Error::other(format!(
            "this command acts on this machine; run it on {host} instead"
        ))),
    }
}

/// Connects to the daemon. A wedged local daemon is diagnosed, and
/// `a daemon restart` suggested; a session at a terminal offers the
/// restart itself.
fn connect(daemon: &client::Daemon) -> io::Result<client::Connection> {
    let on_drain = || eprintln!("[waiting for the previous daemon to finish its work]");
    daemon.connect(&on_drain).map_err(|e| wedged(daemon, e))
}

/// `e` as reported: a daemon that does not answer is diagnosed where its
/// link can.
fn wedged(daemon: &client::Daemon, e: client::Error) -> io::Error {
    if e == client::Error::NotResponding
        && let Some(diagnosis) = daemon.link.diagnose()
    {
        report(&diagnosis);
        return io::Error::new(io::ErrorKind::TimedOut, RESTART);
    }
    e.into()
}

const RESTART: &str = "run `a daemon restart` to replace it";

fn daemon_cmd(host: Option<&Host>, action: Daemon) -> io::Result<()> {
    let home = Home::from_env()?;
    let home = here(&home, host)?.clone();
    match action {
        Daemon::Autostart {
            state: Autostart::On,
        } => {
            sys::autostart::enable(&home)?;
            sayln!("autostart on: the daemon starts at login and stays up");
        }
        Daemon::Autostart {
            state: Autostart::Off,
        } => {
            sys::autostart::disable(&home)?;
            sayln!("autostart off: the daemon starts on first use and exits when idle");
        }
        Daemon::Log => match std::fs::read_to_string(home.daemon_log()) {
            Ok(text) => out(format_args!("{text}")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        },
        Daemon::Status => {
            let d = client::diagnose(&home);
            match (d.pid, client::status(&home)) {
                (None, _) | (_, Ok(None)) => sayln!("not running"),
                (_, Ok(Some(s))) => sayln!(
                    "running (pid {}, {}), {} sessions, {} other clients",
                    s.pid,
                    s.build,
                    s.sessions,
                    s.clients.saturating_sub(1)
                ),
                (Some(pid), Err(e)) => sayln!("running (pid {pid}) but not responding: {e}"),
            }
            sayln!("log: {}", d.log.display());
            if let Some(note) = d.crash_note() {
                sayln!("{note}");
            }
        }
        Daemon::Stop => {
            let stopped = client::stop(&home)?;
            sayln!("{}", if stopped { "stopped" } else { "not running" });
        }
        Daemon::Restart => {
            let installed = home.runtime().stage(&std::env::current_exe()?)?;
            client::Daemon::local(home, installed).restart()?;
            sayln!("restarted");
        }
    }
    Ok(())
}

/// One turn and out ([`once`]) on a prompt, or on what is piped to stdin
/// (unless `A_TUI=plain` asks for the line REPL); otherwise the full-screen
/// client at a terminal, the line REPL anywhere else, which leave `mode` to
/// `/mode`.
fn session(
    home: &Home,
    daemons: &Daemons,
    target: Target,
    prompt: Option<String>,
    mode: Option<String>,
) -> io::Result<ExitCode> {
    let daemons = reaching(home, daemons, &target)?;
    let (member, target) = daemons.resolve(target)?;
    let daemon = &member.daemon;
    if prompt.is_some() || (!io::stdin().is_terminal() && !tui::plain()) {
        return once::run(daemon, target, prompt, mode);
    }
    match tui::viewport() {
        Some(viewport) => tui::run(daemon, target, viewport),
        None => repl(daemon, target),
    }
    .map(|()| ExitCode::SUCCESS)
}

/// What wakes the line client.
enum Input {
    Busy,
    /// A line typed; `None` once stdin closed.
    Line(Option<String>),
    Client(Event),
}

/// The line-mode client: the shared session client, its updates printed
/// as lines and its intents typed. When the daemon drains for an upgrade,
/// it reconnects (starting the new build) and replays what it missed.
fn repl(daemon: &client::Daemon, target: Target) -> io::Result<()> {
    let _terminal = sys::terminal::LineMode::enter()?;
    let (tx, rx) = client::inbox(client::QueueBudget::default(), |input| {
        size_of::<Input>()
            + match input {
                Input::Client(event) => event.retained_bytes(),
                Input::Line(Some(text)) => text.len(),
                Input::Line(None) | Input::Busy => 0,
            }
    });
    let events = tx.clone();
    let runtime: rt::Rt = std::sync::Arc::new(rt::Threads::default());
    let driver = Driver::new(move |driver, event| events.event(driver, event, Input::Client))
        .start(runtime.clone(), daemon.clone());
    let mut repl = Repl {
        client: SessionClient::new(Env::Origin),
        screen: Screen::default(),
        input: Some(tx),
        live: false,
        exit: None,
    };
    let mut effects = repl.client.open(target);
    loop {
        driver.pump(&mut repl, effects);
        if let Some(exit) = repl.exit.take() {
            return exit;
        }
        let Some(input) = rx.recv() else {
            return Ok(());
        };
        effects = match input {
            Input::Busy => return Err(client::Error::Busy.into()),
            Input::Client(event) => repl.client.step(event),
            Input::Line(None) => repl.client.detach(),
            Input::Line(Some(line)) => repl.line(&line)?,
        };
    }
}

const NOT_RESPONDING: &str = "daemon not responding";

struct Repl {
    client: SessionClient,
    screen: Screen,
    /// Taken by the thread reading stdin, once attached.
    input: Option<client::Post<Input>>,
    /// Attached once at least.
    live: bool,
    /// Set once something shown ends the client.
    exit: Option<io::Result<()>>,
}

impl Repl {
    /// What a typed line asks for. While the daemon does not answer, the
    /// line answers the prompt instead.
    fn line(&mut self, line: &str) -> io::Result<Vec<client::Effect>> {
        if let Link::Wedged(d) = self.client.link() {
            return match line.trim() {
                "r" => Ok(self.client.restart()),
                "l" => {
                    for line in d.log_tail() {
                        eprintln!("  | {}", client::sanitize(&line));
                    }
                    prompt();
                    Ok(Vec::new())
                }
                _ => Err(io::Error::new(io::ErrorKind::TimedOut, NOT_RESPONDING)),
            };
        }
        Ok(match client::parse(line, self.client.turn().asking) {
            None => Vec::new(),
            Some(client::Cmd::Help) => {
                for (usage, help) in client::help(self.client.commands()) {
                    sayln!("  {usage:<16} {help}");
                }
                Vec::new()
            }
            Some(cmd) => self.client.run(cmd),
        })
    }

    fn link(&mut self) -> Option<io::Result<()>> {
        match self.client.link() {
            Link::Replaying { .. } | Link::Live { .. } => {
                self.live = true;
                self.read_stdin();
            }
            Link::Draining if !self.live => {
                eprintln!("[waiting for the previous daemon to finish its work]");
            }
            Link::Reconnecting { .. } => eprintln!("[daemon upgraded; reconnecting]"),
            Link::Wedged(d) => {
                report(d);
                if !io::stdin().is_terminal() {
                    return Some(Err(io::Error::new(io::ErrorKind::TimedOut, RESTART)));
                }
                prompt();
                self.read_stdin();
            }
            Link::Ended(End::Detached) => {
                sayln!("[detached; the session keeps running. `a resume` to reattach]");
                return Some(Ok(()));
            }
            Link::Ended(End::Lost(why)) if self.live => {
                return Some(Err(io::Error::other(format!("daemon disconnected: {why}"))));
            }
            Link::Ended(End::Lost(why)) => return Some(Err(why.clone().into())),
            Link::Connecting | Link::Draining => {}
        }
        None
    }

    /// Reads typed lines from here on, which a first attach is waited for:
    /// piped input then goes to the session it attached.
    fn read_stdin(&mut self) {
        let Some(tx) = self.input.take() else {
            return;
        };
        thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(Input::Line(Some(line))).is_err() {
                    tx.close_with(Input::Busy);
                    return;
                }
            }
            let _ = tx.send(Input::Line(None));
        });
    }
}

/// Shows what the client tells; what ends the client is kept for the loop.
impl Renderer for Repl {
    fn show(&mut self, shown: Vec<Show>) -> Vec<client::Effect> {
        for show in shown {
            if self.exit.is_some() {
                break;
            }
            match show {
                Show::View(update) => {
                    // Nothing to attach to: no such session.
                    if let (None, Change::Error { message }) =
                        (self.client.view().session, &update.change)
                    {
                        self.exit = Some(Err(io::Error::other(message.clone())));
                        continue;
                    }
                    self.screen.show(&update.change, self.client.view());
                }
                Show::Notice(notice) => eprintln!("[{}]", client::sanitize(&notice.to_string())),
                Show::Sessions => {
                    for s in self.client.sessions() {
                        sayln!("  {s}");
                    }
                }
                Show::Link => self.exit = self.link(),
            }
        }
        Vec::new()
    }
}

fn prompt() {
    eprint!("  [r] restart daemon  [l] show log  [q] quit: ");
    let _ = io::stderr().flush();
}

/// What a client can tell of a daemon that does not answer.
fn report(d: &client::Diagnosis) {
    errln!(
        "! {} is not responding after {}s",
        d.who(),
        client::HELLO_TIMEOUT.as_secs()
    );
    errln!("  log: {}", d.log.display());
    if let Some(note) = d.crash_note() {
        errln!("  {}", note);
    }
}

/// Prints the shared view's updates as transcript lines, each item as
/// [`Item::present`] has it. A model reply streams in as deltas; on replay
/// there are none, so the committed text is printed instead. What waits
/// for an approval is asked once the replay is in, after its recap, and as
/// it begins to wait from then on.
#[derive(Default)]
struct Screen {
    stream: client::ReplyStream,
    /// The replay of the latest attach is in.
    synced: bool,
}

impl Screen {
    fn show(&mut self, change: &Change, view: &SessionView) {
        if !matches!(change, Change::Batch { .. }) {
            for event in self.stream.project(view) {
                match event {
                    client::ReplyEvent::Start => say!("a    "),
                    client::ReplyEvent::Text(text) => say!("{text}"),
                    client::ReplyEvent::End => sayln!(),
                }
            }
        }
        match change {
            Change::Batch { changes } => {
                for change in changes {
                    self.show(change, view);
                }
            }
            Change::Attached {
                session, workspace, ..
            } => {
                self.synced = false;
                sayln!("[session {} · {}]", session.short(), workspace);
            }
            Change::Added { item, .. } => {
                if !matches!(item, Item::Assistant { .. }) {
                    print_all(&added(item));
                }
            }
            // Sent in place of a long replay: shown as the replay would be.
            Change::Snapshot => {
                for item in &view.items {
                    if matches!(item.present().kind, Kind::Reply | Kind::Assistant) {
                        continue;
                    }
                    print_all(&added(item));
                    if item.settled() {
                        print_all(&outcome(&item.present()));
                    }
                }
            }
            Change::Updated { index, part } => self.updated(&view.items[*index], *part, view),
            Change::Synced => {
                self.synced = true;
                if let Some(recap) = &view.recap {
                    sayln!("[{}]", recap_line(recap));
                }
                for call in view.state.inbox() {
                    if let Some(item) = view.items.iter().find(|i| i.key() == call.0) {
                        sayln!("{}", ask(&view.present(item)));
                    }
                }
            }
            Change::Mismatch { supported } => eprintln!("[{}]", mismatch_line(*supported)),
            Change::Error { message } => eprintln!("[error: {}]", client::sanitize(message)),
            Change::Delta { .. } | Change::Activity | Change::Draining => {}
        }
    }

    fn updated(&mut self, item: &Item, part: Part, view: &SessionView) {
        let p = view.present(item);
        match (part, p.kind) {
            (Part::Outcome, Kind::Reply) => {
                if let Some(usage) = p.usage {
                    sayln!("     · {usage}");
                }
                if !p.status.is_empty() {
                    sayln!("  └ {}", p.status);
                }
            }
            (Part::Outcome, Kind::Tool | Kind::Command | Kind::Child) => print_all(&outcome(&p)),
            (Part::Phase, _) if self.synced && p.buttons != protocol::view::Buttons::None => {
                sayln!("{}", ask(&p));
            }
            (Part::Presentation, _) if self.synced => {
                if let Some(text) = description_update(&p) {
                    sayln!("{text}");
                }
            }
            (Part::Answer { approved }, _) => {
                sayln!("  · {}", answer_line(approved));
            }
            _ => {}
        }
    }
}

fn print_all(lines: &[String]) {
    for line in lines {
        sayln!("{line}");
    }
}

fn description_update(p: &Presented<'_>) -> Option<String> {
    p.description.map(|_| ask(p))
}

/// The question a call waiting for approval puts.
fn ask(p: &Presented<'_>) -> String {
    client::approval_parts(p)
        .into_iter()
        .map(|part| match part {
            client::ApprovalPart::Controls { title, .. } => {
                format!("? {title}  {}  [y/n]", p.status)
            }
            part => part.text().unwrap_or_default().to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The lines an item starts with, as it is added.
fn added(item: &Item) -> Vec<String> {
    let p = item.present();
    match p.kind {
        Kind::User => vec![format!("you  {}", p.body.text())],
        Kind::Assistant => vec![format!("a    {}", p.body.text())],
        Kind::Command => vec![format!("cmd  {}", p.title)],
        Kind::Tool => vec![format!("tool {}", p.title)],
        Kind::Hook | Kind::Environment => vec![format!("  · {}", p.title)],
        Kind::Child => vec![format!("{} {}", p.title, p.body.text())],
        // A reply is printed when done: as its text streams, or whole. A
        // side request's reply is its extension's, not the conversation's.
        Kind::Reply | Kind::Side => Vec::new(),
    }
}

/// The lines a settled call or command ends with: its body behind a
/// gutter, then its status.
fn outcome(p: &Presented<'_>) -> Vec<String> {
    let body = p.body.text();
    let mut lines: Vec<String> = body
        .lines()
        .filter(|_| !matches!(p.body, Shown::None))
        .map(|line| format!("  │ {line}"))
        .collect();
    lines.push(format!("  └ {}", p.status));
    lines
}

fn ls(daemon: &client::Daemon) -> io::Result<()> {
    for s in connect(daemon)?.sessions()? {
        sayln!("{s}");
    }
    Ok(())
}

fn new(daemon: &client::Daemon) -> io::Result<()> {
    let session = daemon.new_session().map_err(|e| wedged(daemon, e))?;
    sayln!("{}", session.short());
    Ok(())
}

fn fork(home: &Home, daemons: &Daemons, source: &str) -> io::Result<()> {
    let (source, seq) = match source.rsplit_once('@') {
        Some((source, seq)) => (
            source,
            Some(
                seq.parse::<u64>()
                    .ok()
                    .and_then(protocol::Seq::new)
                    .ok_or_else(|| io::Error::other("fork sequence must be a positive integer"))?,
            ),
        ),
        None => (source, None),
    };
    let target = Target::Fork {
        session: source.to_owned(),
        seq,
    };
    let daemons = reaching(home, daemons, &target)?;
    let (member, Target::Fork { session, seq }) = daemons.resolve(target)? else {
        unreachable!("a fork resolves to a fork")
    };
    let daemon = &member.daemon;
    let session = daemon
        .fork_session(session, seq)
        .map_err(|e| wedged(daemon, e))?;
    sayln!("{}", session.short());
    Ok(())
}

#[cfg(feature = "web")]
fn web(daemons: Daemons, open: bool) -> io::Result<()> {
    web::run(daemons, open)
}

#[cfg(not(feature = "web"))]
fn web(_daemons: Daemons, _open: bool) -> io::Result<()> {
    Err(io::Error::other("this build of a has no web UI"))
}

/// What whoever started the daemon handed it, by descriptor or handle
/// number: a client its listener and lock, a daemon of the build before
/// those and the record of its work ([`sys::handover`]).
struct Handoff {
    listen: Option<i32>,
    lock: Option<i32>,
    handover: Option<i32>,
}

/// The daemon. Started by a client (`listen_fd`), it leaves the client's
/// session and discards untyped stderr; by hand, it stays at the terminal;
/// by the OS at login (`login`), it stays in the foreground the service
/// manager tracks. The engine owns the typed diagnostic log in every case.
fn serve(
    home: &Home,
    staged: &Staged,
    Handoff {
        listen: listen_fd,
        lock: lock_fd,
        handover: handover_fd,
    }: Handoff,
    reset_breaker: bool,
    detached: bool,
    login: bool,
) -> io::Result<()> {
    if login && (listen_fd.is_some() || lock_fd.is_some() || detached) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a login start has no client handoff",
        ));
    }
    if login {
        sys::autostart::enter(home, &staged.id)?;
        if sys::autostart::LOGIN == sys::autostart::Login::Spawn {
            return client::spawn_at_login(home);
        }
    }
    sys::crash::install(home.crash_dir(), sys::crash::Build::of(&staged.id));
    if detached || listen_fd.is_some() {
        sys::detach_session()?;
        sys::log::discard_stderr()?;
    }
    let (lock, listener) = if login {
        match client::bind_at_login(home)? {
            Some((lock, listener)) => (Some(lock), listener),
            // A client started one first; it learns of autostart itself.
            None => return Ok(()),
        }
    } else {
        let lock = lock_fd
            .map(|fd| sys::adopt_lock(sys::InheritedNumber::from(fd).duplicate()?, home))
            .transpose()?;
        let listener = if let Some(fd) = listen_fd {
            sys::Listener::adopt(
                sys::InheritedNumber::from(fd).duplicate()?,
                &home.endpoint(),
            )?
        } else {
            home.bind()?
        };
        (lock, listener)
    };
    let handed = handover_fd
        .map(|fd| sys::handover::read_record(sys::InheritedNumber::from(fd)))
        .transpose()?;
    let start = engine::Start {
        lock,
        reset_breaker,
        logs: login || detached || listen_fd.is_some(),
        login,
        // Only a home that can register asks, so an isolated daemon has no
        // question pending and nothing to wake for.
        resident: sys::autostart::applies(home).then_some(sys::autostart::refresh),
        // The next build runs with this daemon's environment, which is
        // what its first client gave it.
        successor: Some(|exe| {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("__daemon").envs(std::env::vars_os());
            cmd
        }),
        handed,
    };
    engine::serve(
        &listener,
        home,
        &engine::Config {
            extensions: exts::registry,
            ..engine::Config::load(home)
        },
        &staged.id,
        start,
    )
}

/// Every renderer shows an item the same way: whatever one says of an
/// item (its title, its status, its body), the others say too, over the
/// shared view vectors. The layout is each renderer's; the words are not.
#[cfg(all(test, unix, feature = "web"))]
mod parity {
    use protocol::view::{Kind, SessionView, Shown};
    use protocol::{DaemonMsg, Decode};

    /// Text as a renderer that cannot wrap, colour or indent would have it.
    fn bare(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The text of markup: tags dropped, entities read back.
    fn text_of_html(html: &str) -> String {
        let mut text = String::new();
        let mut tag = false;
        for c in html.chars() {
            match c {
                '<' => tag = true,
                '>' => tag = false,
                c if !tag => text.push(c),
                _ => {}
            }
        }
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&")
    }

    #[test]
    fn every_renderer_shows_an_item_the_same_way() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/protocol/tests/vectors/view.json"
        );
        let vectors: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut items = 0;
        for vector in vectors {
            let mut view = SessionView::default();
            for m in vector["messages"].as_array().unwrap() {
                view.fold(&DaemonMsg::from_json(m.to_string().as_bytes()).unwrap());
            }
            for item in &view.items {
                let p = item.present();
                if p.kind == Kind::Side {
                    continue;
                }
                items += 1;
                let look = tui::view::Look {
                    verbose: true,
                    ..tui::view::Look::default()
                };
                let tui = tui::view::texts(&tui::view::rows(item, 200, look)).join("\n");
                let web = text_of_html(&web::view::html(web::view::ItemRow { item, view: &view }));
                // The line printer prints an item as it is added, asks when it
                // asks, and ends it once settled: in between it says nothing.
                let mut repl = super::added(item);
                let asking = p.buttons != protocol::view::Buttons::None;
                if asking {
                    repl.push(super::ask(&p));
                }
                if item.settled() {
                    repl.extend(super::outcome(&p));
                }
                let repl = repl.join("\n");
                let repl_status = item.settled() || asking;
                // A reply's title is its model, shown with its usage, which the
                // terminal keeps in its status line.
                let title = if p.kind == Kind::Reply { "" } else { &p.title };
                let said = [
                    bare(title),
                    bare(&p.status),
                    match &p.body {
                        Shown::None => String::new(),
                        body => bare(body.text().lines().next().unwrap_or_default()),
                    },
                ];
                for (name, shown) in [("tui", &tui), ("web", &web), ("repl", &repl)] {
                    let shown = bare(shown);
                    for (n, word) in said.iter().enumerate() {
                        if n == 1 && name == "repl" && !repl_status {
                            continue;
                        }
                        assert!(
                            shown.contains(word.as_str()),
                            "{name} leaves {word:?} out of {item:?}:\n{shown}"
                        );
                    }
                }
            }
        }
        assert!(items > 20, "{items} items checked");
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    use protocol::{Body, CallId, DaemonMsg, Decode, Entry, Epoch, Json, Seq};

    #[test]
    fn the_repl_never_prints_a_late_description() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../crates/protocol/tests/vectors/view.json"
        ))
        .unwrap();
        let vector = vectors
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "approval reason precedes its optional description")
            .unwrap();
        let mut view = SessionView::default();
        for value in vector["messages"].as_array().unwrap() {
            let mut msg = DaemonMsg::from_json(value.to_string().as_bytes()).unwrap();
            if let DaemonMsg::Entry(entry) = &mut msg
                && matches!(entry.body, Body::ModelResult { .. })
            {
                view.fold(&DaemonMsg::Entry(Entry::new(
                    entry.seq,
                    Epoch(0),
                    Body::Answer {
                        call: CallId::new(1).unwrap(),
                        value: Json::encode(&true).unwrap(),
                        origin: None,
                    },
                )));
                entry.seq = Seq::new(entry.seq.get() + 1).unwrap();
                view.fold(&msg);
                assert_eq!(description_update(&view.present(&view.items[0])), None);
                return;
            }
            view.fold(&msg);
        }
        panic!("scenario has no description");
    }
    #[test]
    fn every_summary_follows_complete_effective_input_before_controls() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../crates/protocol/tests/vectors/view.json"
        ))
        .unwrap();
        for vector in vectors
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["name"].as_str().unwrap().starts_with("approval reviews "))
        {
            let mut view = SessionView::default();
            for message in vector["messages"].as_array().unwrap() {
                view.fold(&DaemonMsg::from_json(message.to_string().as_bytes()).unwrap());
            }
            let p = view.present(&view.items[0]);
            let text = ask(&p);
            let reason = text.find("Why:").unwrap();
            let summary = text.find("Model summary (unverified):").unwrap();
            assert!(reason < summary && summary < text.find("[y/n]").unwrap());
            let markers: &[&str] = if vector["name"].as_str().unwrap().contains("multiline") {
                &[
                    "printf harmless\nrm -rf /tmp/approval-test-sentinel",
                    "60000",
                    "every argument",
                ]
            } else {
                &[
                    "/work/deep/config.ini",
                    "-allow = false",
                    "+allow = true",
                    "-delete = no",
                    "+delete = yes",
                    "flag",
                ]
            };
            for marker in markers {
                assert!(text.find(marker).unwrap() < reason, "{marker}: {text}");
            }
        }
    }
}
