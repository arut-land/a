//! The event loop: the session client's driver and the terminal feed this
//! thread, which folds what they send into the app and draws.

use std::io::{self, Write};
use std::time::Duration;

use client::{Cmd, Daemon, Driver, End, Endpoint, Inbox, Io, Link};
use protocol::Target;
use sys::Signals;

use super::Event;
use super::app::{App, Effect};
use super::render::Viewport;
use super::screen::{Screen, Stdout};
use super::term::{self, Terminal};

pub fn run(daemon: &Daemon, target: Target, viewport: Viewport) -> io::Result<()> {
    let (tx, rx) = client::inbox(
        client::QueueBudget {
            bytes: 64 << 20,
            requests: 1 << 18,
            control_reserve: 16,
        },
        |event| {
            size_of::<Event>()
                + match event {
                    Event::Client(event) => event.retained_bytes(),
                    Event::Input(super::keys::Input::Paste(text)) => text.len(),
                    _ => 0,
                }
        },
    );
    let events = tx.clone();
    let runtime: rt::Rt = std::sync::Arc::new(rt::Threads::default());
    let driver = Driver::new(move |driver, event| events.event(driver, event, Event::Client))
        .start(runtime.clone(), daemon.clone());
    let mut app = App::new(viewport, &target);
    let mut first = app.client.open(target);
    // SSH may need the terminal for authentication before raw mode starts.
    if matches!(daemon.endpoint, Endpoint::Ssh(_)) {
        let connecting = first
            .iter()
            .position(|effect| matches!(effect, client::Effect::Io(Io::Connect { .. })))
            .expect("initial connect");
        if let client::Effect::Io(io) = first.remove(connecting) {
            driver.io(io).ok();
        }
        loop {
            let Some(Event::Client(event)) = rx.recv() else {
                return Err(io::Error::other("connection ended"));
            };
            let ready = matches!(
                event.kind,
                client::EventKind::Connected { .. }
                    | client::EventKind::Failed(_)
                    | client::EventKind::Wedged(_)
            );
            let effects = app.client.step(event);
            driver.pump(&mut app, effects);
            if ready {
                break;
            }
        }
    }
    // Caught before the terminal is ours, so none ends the client with it
    // raw; the input thread reads them.
    let signals = Signals::catch(&term::SIGNALS)?;
    let terminal = Terminal::enter(viewport)?;
    term::spawn_input(tx, signals);
    terminal.show()?;
    let mut screen = Screen::new(viewport, Stdout::default());
    // no-color.org: set and not empty.
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        screen = screen.without_colour();
    }
    let mut client = Client {
        driver,
        app,
        screen,
    };
    // The first frame goes out before the daemon is even reached, or the
    // git root walked up to.
    client.draw()?;
    client
        .app
        .in_workspace(sys::workspace_of(&std::env::current_dir()?));
    client.driver.pump(&mut client.app, first);
    let exit = client.run(&rx);
    let _ = client.screen.close();
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
    Lost(client::Error),
    Quit,
}

/// How often the working indicator turns.
const TICK: Duration = Duration::from_millis(125);

/// Ctrl-L: the next frame is drawn whole.
struct Redraw;

struct Client {
    driver: client::ClientHandle,
    app: App,
    screen: Screen,
}

impl Client {
    /// Blocks for the next event, folds in everything else already queued,
    /// applies what it asks for, then draws one frame if any of it shows:
    /// an ack, or a list no picker is open for, draws nothing.
    fn run(&mut self, rx: &Inbox<Event>) -> io::Result<Exit> {
        loop {
            // The one timer: the working indicator's, armed only while a
            // turn runs, so an idle client sleeps until something happens.
            let first = if self.app.working() {
                match rx.recv_timeout(TICK) {
                    Some(event) => event,
                    None if rx.closed() => break,
                    None => Event::Tick,
                }
            } else if let Some(event) = rx.recv() {
                event
            } else {
                break;
            };
            let mut effects = Vec::new();
            let mut whole = false;
            for event in std::iter::once(first)
                .chain(std::iter::from_fn(|| rx.try_recv()))
                .take(client::PUMP_BATCH)
            {
                match event {
                    Event::Suspend(done) => {
                        term::suspend()?;
                        let _ = done.send(());
                        whole = true;
                    }
                    Event::Input(input) => effects.extend(self.app.input(input)),
                    Event::Resized => whole = true,
                    Event::Tick => self.app.tick(),
                    Event::InputClosed => effects.push(Effect::Detach),
                    Event::Client(event) => {
                        let stepped = self.app.client.step(event);
                        self.driver.pump(&mut self.app, stepped);
                    }
                }
            }
            for effect in effects {
                match self.effect(effect) {
                    Ok(Some(exit)) => return Ok(exit),
                    Ok(None) => {}
                    Err(Redraw) => whole = true,
                }
            }
            if whole {
                self.screen.invalidate();
            }
            if whole || self.app.needs_frame() {
                self.draw()?;
            }
        }
        Ok(Exit::Quit)
    }

    /// `Err(Redraw)` asks for the next frame whole.
    fn effect(&mut self, effect: Effect) -> Result<Option<Exit>, Redraw> {
        let asked = match effect {
            Effect::Cmd(cmd) => self.app.command(cmd),
            Effect::Restart => self.app.client.restart(),
            Effect::Redraw => return Err(Redraw),
            Effect::Detach => {
                let exit = match self.app.client.link() {
                    Link::Ended(End::Lost(why)) => return Ok(Some(Exit::Lost(why.clone()))),
                    link if link.up() => Exit::Detached,
                    _ => Exit::Quit,
                };
                let asked = self.app.command(Cmd::Detach);
                self.driver.pump(&mut self.app, asked);
                return Ok(Some(exit));
            }
            Effect::Quit => return Ok(Some(Exit::Quit)),
        };
        self.driver.pump(&mut self.app, asked);
        Ok(None)
    }

    fn draw(&mut self) -> io::Result<()> {
        loop {
            if !sys::terminal::is_foreground()? {
                term::suspend()?;
                self.screen.invalidate();
            }
            match self.screen.draw(&mut self.app, term::size()?) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    term::suspend()?;
                    self.screen.invalidate();
                }
                result => return result,
            }
        }
    }
}
