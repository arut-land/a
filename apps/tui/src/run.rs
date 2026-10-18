//! The event loop: connections to the daemon on their own threads, the
//! terminal on another, and this one folding what they send into the app
//! and drawing.

use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use client::{Connection, Endpoint, Writer};
use protocol::view::Link;
use protocol::{ClientMsg, Target};

use super::Event;
use super::app::{App, Conn, Effect};
use super::render::{Screen, Viewport};
use super::term::{self, Terminal};
use crate::Daemon;

pub fn run(daemon: &Daemon, target: Target, viewport: Viewport) -> io::Result<()> {
    let workspace = sys::workspace_of(&std::env::current_dir()?);
    let (tx, rx) = mpsc::channel();
    // ssh may ask for a password on the terminal: let it, before the
    // terminal is ours.
    let early = match &daemon.endpoint {
        Endpoint::Ssh(_) => {
            let on_drain = || eprintln!("[waiting for the previous daemon to finish its work]");
            Some(client::connect_with(
                &daemon.endpoint,
                &daemon.staged,
                &on_drain,
                &attach(&target, 0),
            )?)
        }
        Endpoint::Local(_) => None,
    };
    let terminal = Terminal::enter(viewport)?;
    let mut client = Client {
        daemon,
        tx,
        id: 0,
        writer: None,
        app: App::new(viewport, workspace, &target),
        screen: Screen::new(viewport),
        out: io::stdout().lock(),
        viewport,
    };
    // The first frame goes out before the daemon is even reached.
    client.draw()?;
    term::spawn_input(client.tx.clone())?;
    match early {
        Some(conn) => {
            let tx = client.tx.clone();
            thread::spawn(move || read(conn, 0, &tx));
        }
        None => client.open(target, 0, false),
    }
    let exit = client.run(&rx);
    let _ = client.out.write_all(client.screen.close());
    let _ = client.out.flush();
    drop(client);
    drop(terminal);
    match exit? {
        Exit::Detached => {
            // The terminal may be gone (that is one way to detach).
            let note = "[detached; the session keeps running. `a resume` to reattach]";
            let _ = writeln!(io::stdout(), "{note}");
            Ok(())
        }
        Exit::Lost(why) => Err(io::Error::other(format!("daemon disconnected: {why}"))),
        Exit::Quit => Ok(()),
    }
}

enum Exit {
    Detached,
    Lost(String),
    Quit,
}

struct Client<'a> {
    daemon: &'a Daemon,
    tx: Sender<Event>,
    /// The current connection's number: events from replaced ones are
    /// dropped.
    id: u64,
    writer: Option<Writer>,
    app: App,
    screen: Screen,
    out: io::StdoutLock<'static>,
    viewport: Viewport,
}

impl Client<'_> {
    /// Blocks for the next event, folds in everything else already queued,
    /// applies what it asks for, then draws one frame.
    fn run(&mut self, rx: &Receiver<Event>) -> io::Result<Exit> {
        while let Ok(first) = rx.recv() {
            let mut effects = Vec::new();
            for event in std::iter::once(first).chain(rx.try_iter()) {
                effects.extend(self.event(event));
            }
            for effect in effects {
                if let Some(exit) = self.effect(effect) {
                    return Ok(exit);
                }
            }
            self.draw()?;
        }
        Ok(Exit::Quit)
    }

    fn event(&mut self, event: Event) -> Option<Effect> {
        match event {
            Event::Input(input) => return self.app.input(input),
            Event::Resized => self.screen.invalidate(),
            Event::InputClosed => return Some(Effect::Detach),
            Event::Connected(id, writer, remote) if id == self.id => {
                self.writer = Some(writer);
                self.app.conn = Conn::Open { remote };
            }
            Event::Daemon(id, msg) if id == self.id => return self.app.fold(&msg),
            Event::Closed(id, why) if id == self.id => {
                self.writer = None;
                match self.app.view.session {
                    // A draining daemon closes every connection: reconnect
                    // (which starts the new build) and replay what was
                    // missed.
                    Some(session) if self.app.view.link == Link::Draining => {
                        self.app.conn = Conn::Reconnecting;
                        self.open(Target::Id(session), self.app.view.seq, false);
                    }
                    _ => self.app.conn = Conn::Lost(why),
                }
            }
            Event::Failed(id, e) if id == self.id => match &self.daemon.endpoint {
                Endpoint::Local(home) if e.kind() == io::ErrorKind::TimedOut => {
                    let d = client::diagnose(home);
                    let who = d.pid.map_or_else(
                        || "the daemon".to_owned(),
                        |pid| format!("the daemon (pid {pid})"),
                    );
                    self.app.unresponsive(who, d.log_tail());
                }
                _ => self.app.conn = Conn::Lost(e.to_string()),
            },
            Event::Notice(text) => self.app.notice(text, false),
            // From a connection already replaced.
            Event::Connected(..) | Event::Daemon(..) | Event::Closed(..) | Event::Failed(..) => {}
        }
        None
    }

    fn effect(&mut self, effect: Effect) -> Option<Exit> {
        let result = match effect {
            Effect::Send(msg) => self.send(&msg),
            Effect::List => self.send(&ClientMsg::List),
            Effect::RefreshEnv => match client::capture() {
                Ok(env) => self
                    .send(&ClientMsg::Environment(env))
                    .map(|()| self.app.notice("environment refreshed", false)),
                Err(e) => Err(format!("not refreshed: {e}")),
            },
            Effect::Open(target) => {
                if let Some(mut writer) = self.writer.take() {
                    let _ = writer.send(&ClientMsg::Detach);
                }
                self.open(target, 0, false);
                Ok(())
            }
            Effect::Restart => {
                let target = self.app.view.session.map_or(Target::New, Target::Id);
                self.open(target, 0, true);
                Ok(())
            }
            Effect::Redraw => {
                self.screen.invalidate();
                Ok(())
            }
            Effect::Detach => {
                return Some(match (self.writer.take(), &self.app.conn) {
                    (Some(mut writer), _) => {
                        let _ = writer.send(&ClientMsg::Detach);
                        Exit::Detached
                    }
                    (None, Conn::Lost(why)) => Exit::Lost(why.clone()),
                    (None, _) => Exit::Quit,
                });
            }
            Effect::Quit => return Some(Exit::Quit),
        };
        if let Err(e) = result {
            self.app.notice(format!("not sent: {e}"), true);
        }
        None
    }

    fn send(&mut self, msg: &ClientMsg) -> Result<(), String> {
        match &mut self.writer {
            Some(writer) => writer.send(msg).map_err(|e| e.to_string()),
            None => Err("not connected".to_owned()),
        }
    }

    /// Opens a new connection on its own thread (replacing the daemon
    /// first when `restart`), attaches, then reads it.
    fn open(&mut self, target: Target, since: u64, restart: bool) {
        self.id += 1;
        if !matches!(self.app.conn, Conn::Reconnecting) {
            self.app.conn = Conn::Connecting;
        }
        let (daemon, tx, id) = (self.daemon.clone(), self.tx.clone(), self.id);
        thread::spawn(move || {
            let notice = tx.clone();
            let on_drain = move || {
                let text = "waiting for the previous daemon to finish its work".to_owned();
                let _ = notice.send(Event::Notice(text));
            };
            let first = attach(&target, since);
            let conn = match (&daemon.endpoint, restart) {
                (Endpoint::Local(home), true) => {
                    client::restart(home, &daemon.staged).and_then(|mut conn| {
                        conn.send_all(&first(&conn)?)?;
                        Ok(conn)
                    })
                }
                (endpoint, _) => client::connect_with(endpoint, &daemon.staged, &on_drain, &first),
            };
            match conn {
                Ok(conn) => read(conn, id, &tx),
                Err(e) => {
                    let _ = tx.send(Event::Failed(id, e));
                }
            }
        });
    }

    fn draw(&mut self) -> io::Result<()> {
        let (w, h) = sys::tty::size().unwrap_or((80, 24));
        let (w, h) = (w.max(10), h.max(4));
        if self.viewport == Viewport::Inline {
            let lines = self.app.scrollback(w);
            if !lines.is_empty() {
                self.out.write_all(self.screen.scrollback(&lines))?;
            }
        }
        let height = self.app.height(w, h).max(1);
        let mut buf = self.screen.frame(w, height);
        self.app.draw(&mut buf);
        self.out.write_all(self.screen.render(buf))?;
        self.out.flush()
    }
}

/// What a connection sends first: the attach to `target` from this shell's
/// workspace and with its environment (or a remote bridge's), replaying
/// from `since`.
fn attach(target: &Target, since: u64) -> impl Fn(&Connection) -> io::Result<Vec<ClientMsg>> {
    move |conn| Ok(vec![conn.attach(target.clone(), since, true)?])
}

/// Hands over the writer, then forwards every message until the
/// connection ends.
fn read(conn: Connection, id: u64, tx: &Sender<Event>) {
    let Connection {
        mut reader,
        writer,
        remote,
        ..
    } = conn;
    if tx
        .send(Event::Connected(id, writer, remote.is_some()))
        .is_err()
    {
        return;
    }
    let why = loop {
        match reader.recv() {
            Ok(Some(msg)) => {
                if tx.send(Event::Daemon(id, msg)).is_err() {
                    return;
                }
            }
            Ok(None) => break "the daemon closed the connection".to_owned(),
            Err(e) => break e.to_string(),
        }
    };
    let _ = tx.send(Event::Closed(id, why));
}
