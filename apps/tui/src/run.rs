//! The event loop: the session client's driver and the terminal feed this
//! thread, which folds what they send into the app and draws.

use std::io::{self, Write};

use client::{Cmd, Driver, End, Endpoint, Inbox, Link};
use protocol::Target;
use sys::Signals;

use super::Event;
use super::app::{App, Effect};
use super::render::Viewport;
use super::screen::{Screen, Stdout};
use super::term::{self, Terminal};
use crate::Daemon;

pub fn run(daemon: &Daemon, target: Target, viewport: Viewport) -> io::Result<()> {
    let workspace = sys::workspace_of(&std::env::current_dir()?);
    let (tx, rx) = client::inbox();
    let events = tx.clone();
    let driver = Driver::new(
        daemon.endpoint.clone(),
        daemon.staged.clone(),
        move |_, event| {
            let _ = events.send(Event::Client(event));
        },
    );
    let mut app = App::new(viewport, workspace, &target);
    let first = app.client.open(target);
    // ssh may ask for a password on the terminal: let it, before the
    // terminal is ours.
    let first = match daemon.endpoint {
        Endpoint::Ssh(_) => first
            .into_iter()
            .filter_map(|effect| match effect {
                client::Effect::Connect {
                    id,
                    attach,
                    restart,
                } => {
                    driver.connect(id, &attach, restart);
                    None
                }
                effect => Some(effect),
            })
            .collect(),
        Endpoint::Local(_) => first,
    };
    // Caught before the terminal is ours, so none ends the client with it
    // raw; the input thread reads them.
    let signals = Signals::catch(&term::SIGNALS)?;
    let terminal = Terminal::enter(viewport)?;
    let mut client = Client {
        driver,
        app,
        screen: Screen::new(viewport, Stdout::default()),
    };
    // The first frame goes out before the daemon is even reached.
    client.draw()?;
    term::spawn_input(tx, signals);
    client.apply(first);
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
    Lost(String),
    Quit,
}

struct Client {
    driver: Driver,
    app: App,
    screen: Screen,
}

impl Client {
    /// Blocks for the next event, folds in everything else already queued,
    /// applies what it asks for, then draws one frame.
    fn run(&mut self, rx: &Inbox<Event>) -> io::Result<Exit> {
        while let Some(first) = rx.recv() {
            let mut effects = Vec::new();
            for event in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv())) {
                match event {
                    Event::Input(input) => effects.extend(self.app.input(input)),
                    Event::Resized => self.screen.invalidate(),
                    Event::InputClosed => effects.push(Effect::Detach),
                    Event::Client(event) => {
                        let io = self.app.event(event);
                        self.apply(io);
                    }
                }
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

    fn effect(&mut self, effect: Effect) -> Option<Exit> {
        let io = match effect {
            Effect::Cmd(cmd) => self.app.command(cmd),
            Effect::Restart => self.app.client.restart(),
            Effect::Redraw => {
                self.screen.invalidate();
                Vec::new()
            }
            Effect::Detach => {
                let exit = match self.app.client.link() {
                    Link::Ended(End::Lost(why)) => return Some(Exit::Lost(why.clone())),
                    Link::Live { .. } => Exit::Detached,
                    _ => Exit::Quit,
                };
                let io = self.app.command(Cmd::Detach);
                self.apply(io);
                return Some(exit);
            }
            Effect::Quit => return Some(Exit::Quit),
        };
        self.apply(io);
        None
    }

    /// Has the driver carry out the session client's effects.
    fn apply(&mut self, mut effects: Vec<client::Effect>) {
        while !effects.is_empty() {
            let shown = effects
                .into_iter()
                .filter_map(|effect| self.driver.apply(effect))
                .collect();
            effects = self.app.absorb(shown);
        }
    }

    fn draw(&mut self) -> io::Result<()> {
        self.screen.draw(&mut self.app, term::size()?)
    }
}
