//! The client's state: the shared [`SessionClient`] plus what only this
//! screen has (scroll, folds, the input, overlays). It turns input into
//! [`Effect`]s and draws frames; it does no I/O, so tests drive it
//! directly.

mod draw;
mod scroll;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};

use super::editor::{Edit, Editor};
use super::keys::{Input, Key, Mouse};
use super::render::{Canvas, Viewport};
use super::text::{Action, Color, Line, Style};
use super::view::{Live, MUTED};
use client::{Cmd, Env, Link, Renderer, SessionClient, Show};
use protocol::view::{Change, Item, SessionView, Update, Usage, mismatch_line};
use protocol::{HostPath, SessionSummary, Target};

/// What the event loop does for the app.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    /// The session client's to carry out.
    Cmd(Cmd),
    /// Replace an unresponsive daemon.
    Restart,
    Redraw,
    Detach,
    Quit,
}

enum Overlay {
    Picker {
        selected: usize,
    },
    Help,
    /// The daemon did not answer the hello.
    Unresponsive {
        who: String,
        log: Vec<String>,
    },
}

/// What frames show of the whole transcript: found by one scan once it
/// changed, not per frame or per streamed token.
#[derive(Clone, Copy, Default)]
struct Summary {
    /// The oldest call asked of this client still waiting, by index.
    asked: Option<usize>,
    /// The latest reply, by index (the status line shows its model).
    reply: Option<usize>,
}

/// One item's rows at a width.
struct Rows {
    width: usize,
    lines: Vec<Line>,
    /// A reply streaming in: how far its text was wrapped.
    live: Option<Live>,
}

// Screen toggles and the frame owed, each read where it is set.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub client: SessionClient,
    viewport: Viewport,
    /// This shell's workspace: the picker starts on its latest session.
    workspace: HostPath,
    /// Rows owned by the viewport and the current scroll traversal.
    cache: BTreeMap<usize, Rows>,
    collapsed: HashSet<protocol::Seq>,
    verbose: bool,
    /// Scrolled back: the top row shown, as (item, row within it), which
    /// stays put as output arrives below. `None` follows the latest.
    anchor: Option<(usize, usize)>,
    /// The transcript area's width and height when last drawn.
    width: usize,
    body: u16,
    editor: Editor,
    /// The slash menu's selected command.
    menu_at: Option<usize>,
    /// The approval list's highlight: Yes until moved to No.
    deny: bool,
    overlay: Option<Overlay>,
    /// A one-line notice (an error, a refused command) until the next key.
    flash: Option<(String, Style)>,
    recap_hidden: bool,
    /// The last frame, and where its buttons are.
    canvas: Canvas,
    /// The rows under and over the transcript, composed once per frame.
    composed: Option<draw::Composed>,
    /// The rule over the input, at the last width.
    rule: Line,
    /// `None` once the transcript changed.
    summary: Option<Summary>,
    /// Tokens the session's model calls used, summed as replies arrive.
    tokens: Usage,
    clock: WorkingClock,
    /// Inline: the items already printed into the scrollback.
    committed: usize,
    /// The attach asked for the latest session: if there is none, the
    /// picker opens instead.
    resuming: bool,
    /// A frame is owed: something a frame shows changed since the last.
    redraw: bool,
}

// Calls in one turn can settle between frames or wait for approval.
// Preserve their active time and usage without keeping a timer alive.
#[derive(Default)]
struct WorkingClock {
    turn: u64,
    since: Option<std::time::Instant>,
    elapsed: std::time::Duration,
    before: u64,
}

impl WorkingClock {
    fn sync(&mut self, turn: u64, active: bool, spent: u64) {
        if self.turn != turn {
            *self = Self {
                turn,
                before: spent,
                ..Self::default()
            };
        }
        if active {
            self.since.get_or_insert_with(std::time::Instant::now);
        } else if let Some(began) = self.since.take() {
            self.elapsed += began.elapsed();
        }
    }

    fn elapsed(&self) -> std::time::Duration {
        self.elapsed
            + self
                .since
                .map_or(std::time::Duration::ZERO, |t| t.elapsed())
    }
}

/// The keys; help lists the slash commands after them.
const HELP: &[(&str, &str)] = &[
    ("enter", "send"),
    ("ctrl-j / alt-enter", "new line"),
    ("↑ ↓", "move in the input, then through history"),
    (
        "↑/↓ enter / 1/y / 2/n",
        "choose or answer the oldest approval (input empty)",
    ),
    ("pgup / pgdn / wheel", "scroll the transcript"),
    (
        "esc",
        "deny approval, interrupt the turn, or return to latest output",
    ),
    ("ctrl-o", "show every card's whole output"),
    ("click a card", "fold or unfold it"),
    (
        "ctrl-c",
        "interrupt the running turn; else clear the input, or detach",
    ),
    ("ctrl-d", "detach with an empty input"),
    ("? / F1", "keys and commands"),
    ("ctrl-l", "redraw"),
];

impl App {
    pub fn new(viewport: Viewport, target: &Target) -> Self {
        Self {
            client: SessionClient::new(Env::Origin),
            viewport,
            workspace: HostPath::default(),
            cache: BTreeMap::new(),
            collapsed: HashSet::new(),
            verbose: false,
            anchor: None,
            width: 0,
            body: 0,
            editor: Editor::default(),
            menu_at: Some(0),
            deny: false,
            overlay: None,
            flash: None,
            recap_hidden: false,
            canvas: Canvas::default(),
            composed: None,
            rule: Line::default(),
            summary: None,
            tokens: Usage::default(),
            clock: WorkingClock::default(),
            committed: 0,
            resuming: matches!(target, Target::Latest { .. }),
            redraw: true,
        }
    }

    /// The shell's workspace, which the picker starts on: found after the
    /// first frame, as it walks up to a git root.
    pub fn in_workspace(&mut self, workspace: HostPath) {
        self.workspace = workspace;
    }

    /// Whether the next frame would differ from the last: nothing is drawn
    /// for an ack, or a list no picker shows.
    pub const fn needs_frame(&self) -> bool {
        self.redraw
    }

    /// A turn is working and nothing waits for the person: the indicator
    /// turns, and the event loop wakes to turn it. At every other time the
    /// client has no timer.
    pub fn working(&self) -> bool {
        let turn = self.client.turn();
        // Only the recorded turn supplies the clock's start time.
        turn.running && turn.can_interrupt && turn.asking.is_none()
    }

    /// The indicator's clock ticked.
    pub fn tick(&mut self) {
        self.redraw |= self.working();
    }

    pub fn view(&self) -> &SessionView {
        self.client.view()
    }

    /// Has the session client carry out `cmd`; returns what the driver is
    /// to do.
    pub fn command(&mut self, cmd: Cmd) -> Vec<client::Effect> {
        if let Cmd::Approve { .. } = cmd {
            self.deny = false;
        }
        if let Cmd::Sessions = cmd {
            self.overlay = Some(Overlay::Picker { selected: 0 });
            self.redraw = true;
        }
        self.client.run(cmd)
    }

    /// Folds a daemon message in, as if from the open connection, showing
    /// what it changed; returns what the driver is to do.
    #[cfg(test)]
    pub fn fold(&mut self, msg: &protocol::DaemonMsg) -> Vec<client::Io> {
        let conn = self.client.connection();
        let effects = self.client.step(client::Event {
            conn,
            kind: client::EventKind::Msg(msg.clone()),
        });
        let mut io = Vec::new();
        client::pump(
            |what| {
                io.push(what);
                Ok(())
            },
            self,
            effects,
        );
        io
    }

    /// Keeps the screen's state in step with what changed in the view.
    fn changed(&mut self, update: Update) -> Vec<client::Effect> {
        if let Change::Batch { changes } = update.change {
            return changes
                .into_iter()
                .flat_map(|change| {
                    self.changed(Update {
                        change,
                        rev: update.rev,
                        seq: update.seq,
                    })
                })
                .collect();
        }
        let len = self.view().items.len();
        if !matches!(update.change, Change::Delta { .. }) {
            self.summary = None;
        }
        match update.change {
            Change::Attached { reset, .. } => {
                self.resuming = false;
                if matches!(self.overlay, Some(Overlay::Picker { .. })) {
                    self.overlay = None;
                }
                if reset {
                    self.cache.clear();
                    self.collapsed.clear();
                    self.anchor = None;
                    self.committed = 0;
                    self.tokens = Usage::default();
                    self.clock = WorkingClock::default();
                    self.recap_hidden = false;
                    self.flash = None;
                }
            }
            Change::Added { index, item } => {
                if let Item::User { text, .. } = &item {
                    self.editor.remember(text);
                }
                self.cache.remove(&index);
            }
            Change::Updated { index, .. } => {
                spend(&mut self.tokens, &self.client.view().items[index]);
                self.cache.remove(&index);
            }
            Change::Synced => self.recap_hidden = false,
            Change::Error { message } => {
                // `a resume` where this workspace has no session: pick one.
                if self.resuming && self.view().session.is_none() {
                    self.resuming = false;
                    self.notice(
                        "no session in this workspace yet: pick one, or n for a new one",
                        false,
                    );
                    return self.command(Cmd::Sessions);
                }
                self.flash = Some((message, Style::new().fg(Color::Red)));
            }
            Change::Mismatch { supported } => {
                self.flash = Some((mismatch_line(supported), Style::new().fg(Color::Red)));
            }
            Change::Snapshot => {
                self.cache.clear();
                self.anchor = None;
                self.committed = self.committed.min(len);
                self.tokens = Usage::default();
                for item in &self.client.view().items {
                    spend(&mut self.tokens, item);
                }
            }
            // A streaming reply's rows catch up with its text when drawn.
            Change::Delta { .. } | Change::Activity | Change::Draining => {}
            Change::Batch { .. } => unreachable!(),
        }
        Vec::new()
    }

    pub fn notice(&mut self, text: impl Into<String>, error: bool) {
        let style = if error {
            Style::new().fg(Color::Red)
        } else {
            MUTED
        };
        self.flash = Some((text.into(), style));
        self.redraw = true;
    }

    pub fn input(&mut self, input: Input) -> Option<Effect> {
        self.redraw = true;
        match input {
            Input::Key(key) => self.key(key),
            Input::Mouse(mouse) => self.mouse(mouse),
            Input::Paste(text) => {
                if self.overlay.is_none() {
                    self.editor.paste(&text.replace('\t', "    "));
                    self.menu_at = Some(0);
                }
                None
            }
        }
    }

    fn key(&mut self, key: Key) -> Option<Effect> {
        self.flash = None;
        if self.overlay.is_some() {
            return self.overlay_key(key);
        }
        let asked = self.client.turn().asking;
        let page = self.page();
        match key {
            Key::Ctrl('c') if self.client.turn().can_interrupt => return Some(self.interrupt()),
            Key::Ctrl('c') if !self.editor.is_empty() => self.editor.clear(),
            Key::Ctrl('c') => return Some(self.leave()),
            Key::Ctrl('d') if self.editor.is_empty() => return Some(self.leave()),
            Key::Ctrl('o') => {
                self.verbose = !self.verbose;
                if self.viewport == Viewport::Inline {
                    self.anchor = None;
                }
                self.cache.clear();
            }
            Key::Ctrl('l') => return Some(Effect::Redraw),
            Key::PageUp => self.scroll_by(page),
            Key::PageDown => self.scroll_by(-page),
            Key::F(1) => self.overlay = Some(Overlay::Help),
            Key::Char('?') if self.editor.is_empty() && asked.is_none() => {
                self.overlay = Some(Overlay::Help);
            }
            Key::Esc if !self.menu().is_empty() => self.menu_at = None,
            Key::Char(c @ ('y' | 'n' | 'Y' | 'N' | '1' | '2'))
                if self.editor.is_empty() && asked.is_some() =>
            {
                let call = asked?;
                return Some(Effect::Cmd(Cmd::Approve {
                    call,
                    approved: matches!(c, 'y' | 'Y' | '1'),
                }));
            }
            Key::Up | Key::Down | Key::Tab | Key::Ctrl('p' | 'n')
                if self.editor.is_empty() && asked.is_some() =>
            {
                self.deny = !self.deny;
                self.cache.clear();
            }
            Key::Enter if self.editor.is_empty() && asked.is_some() => {
                let deny = std::mem::take(&mut self.deny);
                return Some(Effect::Cmd(Cmd::Approve {
                    call: asked?,
                    approved: !deny,
                }));
            }
            Key::Esc if self.editor.is_empty() && asked.is_some() => {
                return Some(Effect::Cmd(Cmd::Approve {
                    call: asked?,
                    approved: false,
                }));
            }
            Key::Esc if self.client.turn().can_interrupt => {
                return Some(self.interrupt());
            }
            Key::Esc if self.anchor.is_some() => self.anchor = None,
            Key::Esc => self.recap_hidden = true,
            key => {
                let menu = self.menu();
                let last = menu.len().saturating_sub(1);
                let at = self.menu_at.unwrap_or(0).min(last);
                match key {
                    Key::Up | Key::Ctrl('p') if !menu.is_empty() => {
                        self.menu_at = Some(at.saturating_sub(1));
                    }
                    Key::Down | Key::Ctrl('n') if !menu.is_empty() => {
                        self.menu_at = Some((at + 1).min(last));
                    }
                    // Tab takes the selected name; a command with
                    // arguments waits for them, one without runs on Enter.
                    Key::Tab | Key::Enter
                        if !menu.is_empty() && self.editor.text() != menu[at].0 =>
                    {
                        let (name, usage, _) = &menu[at];
                        let args = usage.len() > name.len();
                        self.editor.clear();
                        self.editor.insert(name);
                        if args {
                            self.editor.insert(" ");
                        } else if key == Key::Enter
                            && let Edit::Submit(input) = self.editor.key(Key::Enter)
                        {
                            return self.submit(input);
                        }
                    }
                    key => {
                        self.menu_at = Some(0);
                        if let Edit::Submit(input) = self.editor.key(key) {
                            return self.submit(input);
                        }
                    }
                }
            }
        }
        None
    }

    fn overlay_key(&mut self, key: Key) -> Option<Effect> {
        let last = self.client.sessions().len().saturating_sub(1);
        match (&mut self.overlay, key) {
            (Some(Overlay::Picker { selected }), Key::Up | Key::Ctrl('p')) => {
                *selected = selected.saturating_sub(1);
            }
            (Some(Overlay::Picker { selected }), Key::Down | Key::Ctrl('n')) => {
                *selected = (*selected + 1).min(last);
            }
            (Some(Overlay::Picker { selected }), Key::Enter) => {
                let n = *selected;
                return self.act(Action::Pick(n));
            }
            (Some(Overlay::Picker { .. }), Key::Char('n')) => return self.act(Action::NewSession),
            (Some(Overlay::Picker { .. }), Key::Ctrl('c')) => return Some(self.leave()),
            (Some(Overlay::Picker { .. }), Key::Esc | Key::Char('q')) => self.overlay = None,
            (Some(Overlay::Unresponsive { .. }), Key::Char('r')) => {
                return self.act(Action::Restart);
            }
            (Some(Overlay::Unresponsive { .. }), Key::Char('q') | Key::Esc | Key::Ctrl('c')) => {
                return Some(Effect::Quit);
            }
            (Some(Overlay::Unresponsive { .. } | Overlay::Picker { .. }), _) => {}
            // Help: any key closes it.
            (_, _) => self.overlay = None,
        }
        None
    }

    fn submit(&mut self, input: protocol::Segments) -> Option<Effect> {
        let cmd = client::parse_input(input, self.client.turn().asking)?;
        match &cmd {
            Cmd::Detach => return Some(Effect::Detach),
            Cmd::Interrupt => return Some(self.interrupt()),
            Cmd::Help => {
                self.overlay = Some(Overlay::Help);
                return None;
            }
            Cmd::Input(_) => {
                self.anchor = None;
                self.recap_hidden = true;
            }
            Cmd::Steer(_) => self.anchor = None,
            Cmd::Sessions => self.overlay = Some(Overlay::Picker { selected: 0 }),
            _ => {}
        }
        Some(Effect::Cmd(cmd))
    }

    fn summary(&mut self) -> Summary {
        let view = self.client.view();
        let asking = self.client.turn().asking;
        *self.summary.get_or_insert_with(|| Summary {
            asked: asking.and_then(|call| view.items.binary_search_by_key(&call.0, Item::key).ok()),
            reply: view
                .items
                .iter()
                .rposition(|item| matches!(item, Item::Reply { purpose: None, .. })),
        })
    }

    fn interrupt(&mut self) -> Effect {
        self.notice("interrupt requested · what should it do next?", false);
        Effect::Cmd(Cmd::Interrupt)
    }

    /// Ctrl-C when nothing runs: detaches; the session (and the daemon)
    /// keep running.
    fn leave(&self) -> Effect {
        if self.view().session.is_some() {
            Effect::Detach
        } else {
            Effect::Quit
        }
    }

    fn mouse(&mut self, mouse: Mouse) -> Option<Effect> {
        match mouse {
            Mouse::WheelUp => self.wheel(-1),
            Mouse::WheelDown => self.wheel(1),
            Mouse::Press { x, y } => {
                return self.canvas.hit(x, y).and_then(|a| self.act(a));
            }
        }
        None
    }

    fn wheel(&mut self, dir: i32) {
        match &mut self.overlay {
            Some(Overlay::Picker { selected }) => {
                let last = self.client.sessions().len().saturating_sub(1);
                *selected = selected.saturating_add_signed(dir as isize).min(last);
            }
            Some(_) => {}
            None => self.scroll_by(-dir * 3),
        }
    }

    fn act(&mut self, action: Action) -> Option<Effect> {
        match action {
            Action::Toggle(call) => {
                if !self.collapsed.remove(&call) {
                    self.collapsed.insert(call);
                }
                self.stale(call);
            }
            Action::Approve(call, approved) => {
                return Some(Effect::Cmd(Cmd::Approve { call, approved }));
            }
            Action::Pick(n) => {
                let id = self.listed().nth(n)?.id;
                self.overlay = None;
                if Some(id) != self.view().session {
                    return Some(Effect::Cmd(Cmd::Open(Target::Id(id))));
                }
            }
            Action::NewSession => {
                self.overlay = None;
                return Some(Effect::Cmd(Cmd::Open(Target::New)));
            }
            Action::Dismiss if self.overlay.is_some() => self.overlay = None,
            Action::Dismiss => self.recap_hidden = true,
            Action::Bottom => self.anchor = None,
            Action::Restart => {
                self.overlay = None;
                return Some(Effect::Restart);
            }
            Action::Quit => return Some(Effect::Quit),
        }
        None
    }

    /// Item `key`'s rows are out of date.
    fn stale(&mut self, key: protocol::Seq) {
        if let Ok(at) = self
            .client
            .view()
            .items
            .binary_search_by_key(&key, Item::key)
        {
            self.cache.remove(&at);
        }
    }

    /// Sessions as the picker lists them: newest first, as the daemon
    /// lists them.
    fn listed(&self) -> impl Iterator<Item = &SessionSummary> {
        self.client.sessions().iter()
    }

    fn pick_default(&mut self) {
        let here = self.listed().position(|s| s.workspace == self.workspace);
        if let Some(Overlay::Picker { selected }) = &mut self.overlay {
            *selected = here.unwrap_or(0);
            self.redraw = true;
        }
    }

    fn page(&self) -> i32 {
        i32::from(self.body).max(3) - 2
    }
}

/// Shows what the client's effects tell this screen.
impl Renderer for App {
    fn show(&mut self, shown: Vec<Show>) -> Vec<client::Effect> {
        let mut asked = Vec::new();
        for show in shown {
            match show {
                Show::View(update) => {
                    self.redraw = true;
                    asked.extend(self.changed(update));
                }
                Show::Sessions => self.pick_default(),
                Show::Notice(notice) => self.notice(notice.to_string(), true),
                Show::Link => {
                    self.redraw = true;
                    if let Link::Wedged(d) = self.client.link() {
                        self.overlay = Some(Overlay::Unresponsive {
                            who: d.who(),
                            log: d.log_tail(),
                        });
                    }
                }
            }
        }
        self.clock.sync(
            self.client.view().state.turn_start(),
            self.working(),
            self.tokens.output,
        );
        asked
    }
}

/// Adds what a finished model call used to `tokens`.
fn spend(tokens: &mut Usage, item: &Item) {
    if let Item::Reply {
        usage: Some(usage),
        purpose: None,
        ..
    } = item
    {
        tokens.input += usage.input;
        tokens.output += usage.output;
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use client::{ConnId, Event, EventKind, Welcome};
    use protocol::{Audience, Body, CallId, Caps, DaemonMsg, Entry, Epoch, Json, Seq, Verdict};

    #[test]
    fn display_batches_invalidate_only_affected_terminal_rows() {
        let mut app = App::new(Viewport::Full, &Target::New);
        app.client.open(Target::New);
        app.client.step(Event {
            conn: ConnId(1),
            kind: EventKind::Connected {
                welcome: Welcome {
                    proto: protocol::PROTO,
                    caps: Caps::ALL,
                },
                remote: false,
                commands: Vec::new(),
                machine: protocol::MachineId([0; 16]),
                upgrade: None,
            },
        });
        app.fold(&DaemonMsg::Attached {
            session: protocol::SessionId::new(),
            workspace: "/w".into(),
        });
        for (seq, body) in [
            Body::Assistant {
                text: "unchanged".into(),
            },
            Body::ToolCall {
                invoke: None,
                tool: "custom".into(),
                input: Json::null(),
                id: None,
                card: None,
                by: Audience::Model,
            },
        ]
        .into_iter()
        .enumerate()
        {
            app.fold(&DaemonMsg::Entry(Entry::new(
                Seq::new(seq as u64 + 1).unwrap(),
                Epoch(0),
                body,
            )));
        }
        app.cache = (0..2)
            .map(|i| {
                (
                    i,
                    Rows {
                        width: 80,
                        lines: Vec::new(),
                        live: None,
                    },
                )
            })
            .collect();
        app.fold(&DaemonMsg::Entry(Entry::new(
            Seq::new(3).unwrap(),
            Epoch(0),
            Body::Decision {
                by: "rewrite".into(),
                call: CallId::new(2),
                d: Verdict::PreTool {
                    input: Json::encode(&"effective").unwrap(),
                },
            },
        )));
        assert_eq!(app.cache.len(), 1);
        assert_eq!(app.cache[&0].width, 80);
        assert!(!app.cache.contains_key(&1));
        assert!(!app.cache.contains_key(&2));
    }
}
