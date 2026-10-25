//! The event loop: the session client's driver and the terminal feed this
//! thread, which folds what they send into the app and draws.

use std::io::{self, Write};
use std::time::Duration;

use client::{Cmd, Daemon, Driver, End, Inbox, Io, Link};
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
                    Event::Input(super::keys::Input::Paste(text), _) => text.len(),
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
    // The link may need the terminal (an ssh password) before raw mode starts.
    if daemon.link.needs_terminal() {
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
    let input = term::spawn_input(tx.clone(), signals);
    let mut screen = Screen::new(viewport, Stdout::default());
    // no-color.org: set and not empty.
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        screen = screen.without_colour();
    }
    let mut client = Client {
        driver,
        app,
        screen,
        terminal,
        input,
        post: tx,
        shown: false,
        runtime,
        ticks: Ticks::default(),
    };
    // The first frame goes out before the daemon is even reached, or the
    // git root walked up to.
    client.draw()?;
    client
        .app
        .in_workspace(sys::workspace_of(&std::env::current_dir()?).try_into()?);
    client.driver.pump(&mut client.app, first);
    let exit = client.run(&rx);
    let _ = client.screen.close_terminal();
    drop(client);
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

/// At most one indicator wake is outstanding; idle frames arm none.
#[derive(Default)]
struct Ticks {
    armed: bool,
}

impl Ticks {
    fn arm(&mut self, working: bool, rt: &rt::Rt, post: &client::Post<Event>) {
        if !working || self.armed {
            return;
        }
        self.armed = true;
        let post = post.clone();
        let runtime = rt.clone();
        rt.spawn(Box::pin(async move {
            runtime.sleep(TICK).await;
            let _ = post.send(Event::Tick);
        }));
    }
}

/// Ctrl-L: the next frame is drawn whole.
struct Redraw;

struct Client {
    driver: client::ClientHandle,
    app: App,
    screen: Screen,
    terminal: Terminal,
    input: term::Control,
    post: client::Post<Event>,
    shown: bool,
    runtime: rt::Rt,
    ticks: Ticks,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.input.cancel();
    }
}

impl Client {
    /// Blocks for the next event, folds in everything else already queued,
    /// applies what it asks for, then draws one frame if any of it shows:
    /// an ack, or a list no picker is open for, draws nothing.
    fn run(&mut self, rx: &Inbox<Event>) -> io::Result<Exit> {
        loop {
            let (mut whole, exit) = self.control_step()?;
            if let Some(exit) = exit {
                return Ok(exit);
            }
            self.ticks
                .arm(self.app.working(), &self.runtime, &self.post);
            let first = if whole {
                Event::Control
            } else if let Some(event) = rx.recv() {
                event
            } else {
                break;
            };
            let mut effects = Vec::new();
            for event in std::iter::once(first)
                .chain(std::iter::from_fn(|| rx.try_recv()))
                .take(client::PUMP_BATCH)
            {
                match event {
                    Event::Control => {
                        let (changed, exit) = self.control_step()?;
                        whole |= changed;
                        if let Some(exit) = exit {
                            return Ok(exit);
                        }
                    }
                    Event::Input(input, epoch) => {
                        if epoch == self.input.epoch() {
                            effects.extend(self.app.input(input));
                        }
                    }
                    Event::Tick => {
                        self.ticks.armed = false;
                        self.app.tick();
                    }
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
            let (changed, exit) = self.control_step()?;
            whole |= changed;
            if let Some(exit) = exit {
                return Ok(exit);
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

    fn control_step(&mut self) -> io::Result<(bool, Option<Exit>)> {
        let pending = self.input.take();
        let changed = !pending.is_empty();
        if let Some(interrupt) = pending.interrupt {
            for _ in 0..(if interrupt.repeat { 2 } else { 1 }) {
                if let Some(effect) = self
                    .app
                    .input(super::keys::Input::Key(super::keys::Key::Ctrl('c')))
                    && let Ok(Some(exit)) = self.effect(effect)
                {
                    return Ok((changed, Some(exit)));
                }
            }
        }
        if pending.closed.is_some()
            && let Ok(Some(exit)) = self.effect(Effect::Detach)
        {
            return Ok((changed, Some(exit)));
        }
        if let Some(resume) = pending.suspend {
            term::suspend()?;
            self.shown = false;
            self.screen.resume();
            drop(resume);
        }
        Ok((changed, None))
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
                term::request_suspend(&self.post, &self.input);
                return Ok(());
            }
            let drawn = if self.shown {
                self.screen.draw(&mut self.app, term::size()?)
            } else {
                self.terminal.show().and_then(|()| {
                    self.shown = true;
                    self.screen.draw(&mut self.app, term::size()?)
                })
            };
            match drawn {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.screen.invalidate();
                    if sys::terminal::output_interrupted() {
                        return Ok(());
                    }
                }
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_arms_no_ticks_and_working_arms_only_one() {
        let rt: rt::Rt = std::sync::Arc::new(rt::Threads::new(1));
        let (post, inbox) = client::inbox(client::QueueBudget::default(), |_| size_of::<Event>());
        let mut ticks = Ticks::default();
        ticks.arm(false, &rt, &post);
        assert!(inbox.recv_timeout(TICK * 2).is_none());
        ticks.arm(true, &rt, &post);
        ticks.arm(true, &rt, &post);
        assert!(matches!(
            inbox.recv_timeout(Duration::from_secs(5)),
            Some(Event::Tick)
        ));
        assert!(inbox.recv_timeout(TICK * 2).is_none());
        ticks.armed = false;
        ticks.arm(false, &rt, &post);
        assert!(inbox.recv_timeout(TICK * 2).is_none());
    }
}
