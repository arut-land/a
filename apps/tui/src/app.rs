//! The client's state: the shared reducer's [`SessionView`] plus what only
//! this screen has (scroll, folds, the input, overlays). It turns input
//! into [`Effect`]s and draws frames; it does no I/O, so tests drive it
//! directly.

mod draw;
mod scroll;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::PathBuf;

use protocol::view::{Change, Item, SessionList, SessionView, Usage};
use protocol::{ClientMsg, DaemonMsg, SessionSummary, Target};
use ratatui_core::style::{Color, Style};

use super::editor::{Edit, Editor};
use super::keys::{Input, Key, Mouse};
use super::render::Viewport;
use super::text::{Action, Line};
use super::view::MUTED;

/// What the event loop does for the app.
#[derive(Debug)]
pub enum Effect {
    Send(ClientMsg),
    /// `/steer text`: input for the running turn, sent as a steer where the
    /// daemon takes them.
    Steer(String),
    /// `/env refresh`: capture this shell's environment and send it.
    RefreshEnv,
    /// Asks the daemon for its sessions (the picker).
    List,
    /// Attach to another session on a new connection.
    Open(Target),
    /// Replace an unresponsive daemon.
    Restart,
    Redraw,
    Detach,
    Quit,
}

/// The connection, as the event loop reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    /// Connected; `remote` when the daemon is another host's.
    Open {
        remote: bool,
    },
    Reconnecting,
    Lost(String),
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

/// A clickable region of the last frame.
struct Hit {
    y: u16,
    x: (u16, u16),
    action: Action,
}

/// One item's rows at a width.
struct Rows {
    width: usize,
    lines: Vec<Line>,
}

pub struct App {
    pub view: SessionView,
    pub conn: Conn,
    viewport: Viewport,
    /// This shell's workspace: the picker starts on its latest session.
    workspace: PathBuf,
    sessions: SessionList,
    /// Per item, its rows when last drawn; `None` is out of date.
    cache: Vec<Option<Rows>>,
    collapsed: HashSet<u64>,
    verbose: bool,
    /// Scrolled back: the top row shown, as (item, row within it), which
    /// stays put as output arrives below. `None` follows the latest.
    anchor: Option<(usize, usize)>,
    /// The transcript area's width and height when last drawn.
    width: usize,
    body: u16,
    editor: Editor,
    overlay: Option<Overlay>,
    /// A one-line notice (an error, a refused command) until the next key.
    flash: Option<(String, Style)>,
    recap_hidden: bool,
    hits: Vec<Hit>,
    /// Tokens the session's model calls used, summed as replies arrive.
    tokens: Usage,
    /// Inline: the items already printed into the scrollback.
    committed: usize,
    /// The attach asked for the latest session: if there is none, the
    /// picker opens instead.
    resuming: bool,
}

const HELP: &[(&str, &str)] = &[
    ("enter", "send"),
    ("ctrl-j / alt-enter", "new line"),
    ("↑ ↓", "move in the input, then through history"),
    ("y / n", "answer the oldest approval (input empty)"),
    ("pgup / pgdn / wheel", "scroll the transcript"),
    ("esc", "back to the latest output"),
    ("ctrl-o", "show every card's whole output"),
    ("click a card", "fold or unfold it"),
    (
        "ctrl-c",
        "interrupt the running turn; else clear the input, or detach",
    ),
    ("ctrl-l", "redraw"),
    ("/sessions", "switch session"),
    ("/new", "start a new session"),
    ("/interrupt", "stop the running turn"),
    ("/steer text", "tell the running turn, before its next step"),
    ("/env refresh", "send this shell's environment again"),
    ("/detach", "leave; the session keeps running"),
];

impl App {
    pub fn new(viewport: Viewport, workspace: PathBuf, target: &Target) -> Self {
        Self {
            view: SessionView::default(),
            conn: Conn::Connecting,
            viewport,
            workspace,
            sessions: SessionList::default(),
            cache: Vec::new(),
            collapsed: HashSet::new(),
            verbose: false,
            anchor: None,
            width: 0,
            body: 0,
            editor: Editor::default(),
            overlay: None,
            flash: None,
            recap_hidden: false,
            hits: Vec::new(),
            tokens: Usage::default(),
            committed: 0,
            resuming: matches!(target, Target::Latest { .. }),
        }
    }

    /// Folds a daemon message into the view and keeps the screen's state in
    /// step with what changed.
    pub fn fold(&mut self, msg: &DaemonMsg) -> Option<Effect> {
        if self.sessions.fold(msg) {
            self.pick_default();
            return None;
        }
        let update = self.view.fold(msg)?;
        let len = self.view.items.len();
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
                    self.recap_hidden = false;
                    self.flash = None;
                }
            }
            Change::Added { index, item } => {
                if let Item::User { text, .. } = &item {
                    self.editor.remember(text);
                }
                if index + 1 == len && self.cache.len() == index {
                    self.cache.push(None);
                } else {
                    self.cache.clear();
                }
            }
            Change::Updated { index, item, .. } | Change::Asked { index, item } => {
                spend(&mut self.tokens, &item);
                if self.cache.len() == len {
                    self.cache[index] = None;
                } else {
                    self.cache.clear();
                }
            }
            Change::Delta { call, .. } => {
                self.stale(call);
            }
            Change::Recap(_) => self.recap_hidden = false,
            Change::Error { message } => {
                // `a resume` where this workspace has no session: pick one.
                if self.resuming && self.view.session.is_none() {
                    self.resuming = false;
                    self.notice(
                        "no session in this workspace yet: pick one, or n for a new one",
                        false,
                    );
                    self.overlay = Some(Overlay::Picker { selected: 0 });
                    return Some(Effect::List);
                }
                self.flash = Some((message, Style::new().fg(Color::Red)));
            }
            Change::Mismatch { supported } => {
                self.flash = Some((
                    format!("the daemon speaks protocol {supported}; restart it"),
                    Style::new().fg(Color::Red),
                ));
            }
            Change::Snapshot { .. } => {
                self.cache.clear();
                self.anchor = None;
                self.committed = self.committed.min(len);
                self.tokens = Usage::default();
                for item in &self.view.items {
                    spend(&mut self.tokens, item);
                }
            }
            Change::Synced | Change::Draining => {}
        }
        None
    }

    /// The daemon did not answer: offer to restart it.
    pub fn unresponsive(&mut self, who: String, log: Vec<String>) {
        self.overlay = Some(Overlay::Unresponsive { who, log });
    }

    pub fn notice(&mut self, text: impl Into<String>, error: bool) {
        let style = if error {
            Style::new().fg(Color::Red)
        } else {
            MUTED
        };
        self.flash = Some((text.into(), style));
    }

    pub fn input(&mut self, input: Input) -> Option<Effect> {
        match input {
            Input::Key(key) => self.key(key),
            Input::Mouse(mouse) => self.mouse(mouse),
            Input::Paste(text) => {
                if self.overlay.is_none() {
                    self.editor.insert(&text.replace('\t', "    "));
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
        let asked = self.view.asked().next();
        let page = self.page();
        match key {
            Key::Ctrl('c') if self.running() => return Some(self.interrupt()),
            Key::Ctrl('c') if !self.editor.is_empty() => self.editor.clear(),
            Key::Ctrl('c') => return Some(self.leave()),
            Key::Ctrl('d') if self.editor.is_empty() => return Some(self.leave()),
            Key::Ctrl('o') => {
                self.verbose = !self.verbose;
                self.cache.iter_mut().for_each(|r| *r = None);
            }
            Key::Ctrl('l') => return Some(Effect::Redraw),
            Key::PageUp => self.scroll_by(page),
            Key::PageDown => self.scroll_by(-page),
            Key::F(1) => self.overlay = Some(Overlay::Help),
            Key::Esc if self.anchor.is_some() => self.anchor = None,
            Key::Esc => self.recap_hidden = true,
            Key::Char(c @ ('y' | 'n' | 'Y' | 'N')) if self.editor.is_empty() && asked.is_some() => {
                let call = asked?;
                return Some(Effect::Send(ClientMsg::approve(
                    call,
                    c.eq_ignore_ascii_case(&'y'),
                )));
            }
            key => {
                if let Edit::Submit(text) = self.editor.key(key) {
                    return self.submit(text);
                }
            }
        }
        None
    }

    fn overlay_key(&mut self, key: Key) -> Option<Effect> {
        let last = self.sessions.sessions.len().saturating_sub(1);
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

    fn submit(&mut self, text: String) -> Option<Effect> {
        Some(match text.as_str() {
            "/detach" | "/quit" | "/exit" => Effect::Detach,
            "/env refresh" if matches!(self.conn, Conn::Open { remote: true }) => {
                self.notice(
                    "the host's environment is captured when `a` connects; reattach to refresh it",
                    false,
                );
                return None;
            }
            "/env refresh" => Effect::RefreshEnv,
            "/sessions" => {
                self.overlay = Some(Overlay::Picker { selected: 0 });
                Effect::List
            }
            "/new" => Effect::Open(Target::New),
            "/interrupt" => self.interrupt(),
            "/help" => {
                self.overlay = Some(Overlay::Help);
                return None;
            }
            // With no turn running, a steer is plain input.
            _ if let Some(steer) = text.strip_prefix("/steer ") => {
                self.anchor = None;
                Effect::Steer(steer.to_owned())
            }
            _ => {
                self.anchor = None;
                self.recap_hidden = true;
                Effect::Send(ClientMsg::input(text))
            }
        })
    }

    /// Whether a model call, tool call or slash command is running: what
    /// an interrupt stops, with the rest of its turn.
    pub fn running(&self) -> bool {
        self.view.running()
    }

    fn interrupt(&mut self) -> Effect {
        self.notice("stopping the turn (ctrl-d detaches)", false);
        Effect::Send(ClientMsg::INTERRUPT)
    }

    /// Ctrl-C when nothing runs: detaches; the session (and the daemon)
    /// keep running.
    fn leave(&self) -> Effect {
        if self.view.session.is_some() {
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
                let action = self
                    .hits
                    .iter()
                    .find(|h| h.y == y && (h.x.0..h.x.1).contains(&x))
                    .map(|h| h.action);
                return action.and_then(|a| self.act(a));
            }
        }
        None
    }

    fn wheel(&mut self, dir: i32) {
        match &mut self.overlay {
            Some(Overlay::Picker { selected }) => {
                let last = self.sessions.sessions.len().saturating_sub(1);
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
                return Some(Effect::Send(ClientMsg::approve(call, approved)));
            }
            Action::Pick(n) => {
                let id = self.listed().nth(n)?.id;
                self.overlay = None;
                if Some(id) != self.view.session {
                    return Some(Effect::Open(Target::Id(id)));
                }
            }
            Action::NewSession => {
                self.overlay = None;
                return Some(Effect::Open(Target::New));
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
    fn stale(&mut self, key: u64) {
        if let Ok(at) = self.view.items.binary_search_by_key(&key, Item::key)
            && let Some(slot) = self.cache.get_mut(at)
        {
            *slot = None;
        }
    }

    /// Sessions as the picker lists them: newest first.
    fn listed(&self) -> impl Iterator<Item = &SessionSummary> {
        self.sessions.sessions.iter().rev()
    }

    fn pick_default(&mut self) {
        let here = self.listed().position(|s| s.workspace == self.workspace);
        if let Some(Overlay::Picker { selected }) = &mut self.overlay {
            *selected = here.unwrap_or(0);
        }
    }

    fn page(&self) -> i32 {
        i32::from(self.body).max(3) - 2
    }
}

/// Adds what a finished model call used to `tokens`.
fn spend(tokens: &mut Usage, item: &Item) {
    if let Item::Reply {
        usage: Some(usage), ..
    } = item
    {
        tokens.input += usage.input;
        tokens.output += usage.output;
    }
}
