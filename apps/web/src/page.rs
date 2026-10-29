//! What a tab's chat page shows of its session client, and the patches
//! that stream requested history into the native DOM, then each change alone.

use client::{Attach, Cmd, Effect, Env, Link, Renderer, SessionClient, Show};
use protocol::view::{Change, Item, Part, Status, Update};
use protocol::{SessionId, Target};

use crate::patch::Patch;
use crate::view::{self, End, EndRow, ItemRow, TitleSpan, Transcript, html};

// Amortize range round trips while the browser consumes the full transcript.
const HISTORY_BYTES: usize = 1 << 20;

pub struct Page {
    pub client: SessionClient,
    session: SessionId,
    dom_end: usize,
    /// Canonical Markdown and native selection require one complete item.
    oversized: Option<(usize, usize)>,
    refresh: std::collections::BTreeSet<usize>,
    effects: Vec<Effect>,
    forking: bool,
    /// The daemon's latest complaint, until the transcript moves on.
    error: Option<String>,
    /// The end the page shows, once it shows the transcript.
    shown: Option<End>,
    /// The title the page shows.
    titled: Option<String>,
    /// Whether a view change could have changed the cached title.
    title_dirty: bool,
    /// Text streamed since the last patch, for one reply attempt.
    streamed: Option<(protocol::CallId, u16, String)>,
    live: Option<(protocol::CallId, u16)>,
    /// Events not yet sent.
    out: Vec<u8>,
}

impl Page {
    /// A page for `session`, which keeps the environment it has; and the
    /// attach it asks the driver for.
    pub fn new(session: SessionId) -> (Self, Vec<Effect>) {
        let mut client = SessionClient::new(Env::Keep);
        client.demand(client::HistoryDemand {
            anchor: client::HistoryAnchor::After(0),
            bytes: HISTORY_BYTES,
            ..Default::default()
        });
        let open = client.attach(Attach::new(Target::Id(session), Env::Keep));
        let page = Self {
            client,
            session,
            dom_end: 0,
            oversized: None,
            refresh: std::collections::BTreeSet::default(),
            effects: Vec::new(),
            forking: false,
            error: None,
            shown: None,
            titled: None,
            title_dirty: true,
            streamed: None,
            live: None,
            out: Vec::new(),
        };
        (page, open)
    }

    /// The events not yet sent.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Whether the transcript will not change again.
    pub const fn ended(&self) -> bool {
        matches!(self.client.link(), Link::Ended(_) | Link::Wedged(_))
    }

    /// A line typed in the composer, parsed as every client parses it.
    pub fn line(&mut self, text: &str) -> Vec<Effect> {
        match client::parse(text, self.client.turn().asking) {
            None => Vec::new(),
            Some(cmd) => {
                self.error = None;
                self.command(cmd)
            }
        }
    }

    /// Whether the last line was admitted by the SDK and driver pump.
    pub fn admitted(&self) -> bool {
        self.error.is_none()
    }

    pub fn question(
        &mut self,
        call: protocol::CallId,
        id: protocol::Seq,
        action: &str,
        values: std::collections::BTreeMap<String, String>,
    ) -> Vec<Effect> {
        let answer = self
            .client
            .view()
            .state
            .question(call)
            .ok_or_else(|| "Question is no longer pending".to_string())
            .and_then(|(current, question)| {
                if current != id {
                    return Err("Question changed; review it again".into());
                }
                client::QuestionDraft { values }.answer(id, &question, action)
            });
        match answer {
            Ok(answer) => {
                self.error = None;
                self.client.run(Cmd::Answer { call, answer })
            }
            Err(error) => {
                self.error = Some(error);
                Vec::new()
            }
        }
    }

    pub fn command(&mut self, cmd: Cmd) -> Vec<Effect> {
        match cmd {
            // This page's own controls do these, or nothing does.
            Cmd::Help | Cmd::Sessions | Cmd::Open(_) | Cmd::Detach | Cmd::RefreshEnv => {
                self.error = Some("not available on the web page".to_owned());
                Vec::new()
            }
            Cmd::Fork(seq) => {
                self.forking = true;
                self.client.run(Cmd::Fork(seq))
            }
            cmd => self.client.run(cmd),
        }
    }

    fn synced(&self) -> bool {
        matches!(self.client.link(), Link::Live { .. })
    }

    fn end_row(&self) -> End {
        let ended = self.ended();
        End {
            running: !ended && self.client.turn().running,
            error: self.error.clone(),
            ended,
        }
    }

    /// Patches in what `update` changed, after a batch of them is shown by
    /// [`Page::settle`]. Until the attach is synced (again, after an
    /// upgrade) the page is left as it is; then it is shown whole.
    fn update(&mut self, update: Update) {
        if let Change::Batch { changes } = update.change {
            for change in changes {
                self.update(Update {
                    change,
                    rev: update.rev,
                    seq: update.seq,
                });
            }
            return;
        }
        if self.history_update(&update.change) {
            return;
        }
        self.title_dirty |= self.title_changed(&update.change);
        if let Change::Attached { .. } = &update.change
            && self.forking
            && let Some(session) = self.client.view().session
            && session != self.session
        {
            self.session = session;
            self.forking = false;
            self.out.extend_from_slice(format!("event: datastar-patch-elements\ndata: selector body\ndata: mode append\ndata: elements <script>window.location.replace('/s/{session}')</script>\n\n").as_bytes());
        }
        match &update.change {
            Change::Error { message } => {
                self.forking = false;
                self.error = Some(message.clone());
            }
            Change::Added { .. } => self.error = None,
            _ => {}
        }
        let change = match update.change {
            Change::Synced => {
                self.flush();
                self.shown = Some(self.end_row());
                if self.client.view().history.is_some() {
                    if self.dom_end == 0 {
                        self.show_shell();
                    }
                    self.request_range();
                } else {
                    self.show_all();
                }
                return;
            }
            change if self.synced() && self.shown.is_some() => change,
            _ => return,
        };
        if let Change::Delta { call, text } = change {
            let attempt = self
                .client
                .view()
                .streaming
                .as_ref()
                .expect("delta streams a reply")
                .attempt;
            match &mut self.streamed {
                Some((c, a, t)) if (*c, *a) == (call, attempt) => t.push_str(&text),
                Some((c, ..)) if *c == call => self.streamed = Some((call, attempt, text)),
                _ => {
                    self.flush();
                    self.streamed = Some((call, attempt, text));
                }
            }
            return;
        }
        self.flush();
        let view = self.client.view();
        match change {
            Change::Added { index: 0, .. }
            | Change::Snapshot
            | Change::Synced
            | Change::Attached { reset: true, .. } => self.show_all(),
            Change::Added { item, .. } => {
                let item = html(ItemRow { item: &item, view });
                self.patch(&Patch::Before {
                    selector: "#end",
                    html: &item,
                });
            }
            Change::Updated { index, .. } => {
                let item = html(ItemRow {
                    item: &view.items[index],
                    view,
                });
                self.patch(&Patch::Replace(&item));
            }
            _ => {}
        }
    }

    fn history_update(&mut self, change: &Change) -> bool {
        if let Change::Activity = change
            && let Some(history) = &self.client.view().history
        {
            self.title_dirty = true;
            let restart = self
                .oversized
                .map(|(index, _)| index)
                .filter(|index| history.changed.contains(index));
            self.refresh.extend(
                history
                    .changed
                    .iter()
                    .copied()
                    .filter(|index| *index < self.dom_end),
            );
            if let Some(index) = restart {
                self.oversized = None;
                self.effects
                    .extend(self.client.demand(client::HistoryDemand {
                        anchor: client::HistoryAnchor::After(index),
                        bytes: 1,
                        ..Default::default()
                    }));
            } else {
                self.request_range();
            }
        }
        if matches!(change, Change::Snapshot) && self.client.view().history.is_some() {
            self.show_range();
            return true;
        }
        false
    }

    fn title_changed(&self, change: &Change) -> bool {
        match change {
            Change::Snapshot | Change::Attached { reset: true, .. } => true,
            Change::Added { item, .. } => matches!(item,
                Item::Reply { purpose: Some(purpose), status: Status::Done, .. }
                    if purpose == "title"
            ),
            Change::Updated {
                index,
                part: Part::Outcome,
            } => matches!(
                &self.client.view().items[*index],
                Item::Reply { purpose: Some(purpose), .. } if purpose == "title"
            ),
            _ => false,
        }
    }

    /// Ends a batch of updates: what streamed goes out, and the end shows
    /// again if it changed.
    pub fn settle(&mut self) {
        self.flush();
        if self.synced() {
            self.show_end();
            self.show_title();
        }
    }

    /// Shows the session's title again if it changed.
    fn show_title(&mut self) {
        if !self.title_dirty {
            return;
        }
        self.title_dirty = false;
        let title = self.client.view().side("title");
        if title != self.titled.as_deref() {
            let span = html(TitleSpan { title });
            self.titled = title.map(str::to_owned);
            self.patch(&Patch::Replace(&span));
        }
    }

    /// Adds the text streamed into reply `call` to its element; the first
    /// text replaces the reply's "thinking…".
    fn flush(&mut self) {
        let Some((call, attempt, text)) = self.streamed.take() else {
            return;
        };
        if self.live == Some((call, attempt)) {
            let selector = format!("#live-reply-{call}");
            self.patch(&Patch::Append {
                selector: &selector,
                html: &view::delta(&text),
            });
        } else {
            let row = html(view::LiveReply { call, text: &text });
            self.patch(&Patch::Replace(&row));
            self.live = Some((call, attempt));
        }
    }

    fn show_shell(&mut self) {
        let end = html(EndRow {
            end: &self.end_row(),
        });
        self.patch(&Patch::Replace(&format!("<div id=\"transcript\" style=\"{}\"><span id=\"history-end\" data-loaded=\"0\"></span><div id=\"live\"></div>{end}</div>", crate::style::COLUMN)));
    }

    fn show_range(&mut self) {
        let view = self.client.view();
        let Some(history) = &view.history else {
            return;
        };
        let start = history.start;
        let end = history.end;
        let fragment = history.fragment.clone();
        let settled_live = self.live.is_some_and(|(call, _)| {
            view.items.iter().any(|item| {
                item.key() == call.0
                    && item.settled()
                    && fragment
                        .as_ref()
                        .is_none_or(|f| f.key != call.0 || f.end == f.total)
            })
        });
        let incomplete = fragment.as_ref().filter(|f| f.start > 0 || f.end < f.total);
        let complete_end = incomplete.map_or(end, |f| f.index);
        let rows = self.range_rows(start, complete_end);
        if self.shown.is_none() {
            self.shown = Some(self.end_row());
            self.show_shell();
        }
        let mut inserted = String::new();
        for (index, row) in rows {
            if index < self.dom_end {
                self.patch(&Patch::Replace(&row));
            } else {
                inserted.push_str(&row);
            }
            self.refresh.remove(&index);
        }
        if !inserted.is_empty() {
            self.patch(&Patch::Before {
                selector: "#history-end",
                html: &inserted,
            });
        }
        self.oversized = incomplete.map(|f| (f.index, f.total));
        if start <= self.dom_end {
            self.dom_end = self.dom_end.max(complete_end);
        }
        if settled_live {
            self.patch(&Patch::Replace("<div id=\"live\"></div>"));
            self.live = None;
        }
        self.patch(&Patch::Replace(&format!(
            "<span id=\"history-end\" data-loaded=\"{}\"></span>",
            self.dom_end
        )));
        self.effects.extend(self.client.release_history());
        self.request_range();
    }

    fn range_rows(&self, start: usize, end: usize) -> Vec<(usize, String)> {
        let view = self.client.view();
        view.items
            .iter()
            .enumerate()
            .filter(|(offset, _)| {
                let index = start + offset;
                index < end && (index >= self.dom_end || self.refresh.contains(&index))
            })
            .map(|(offset, item)| {
                let index = start + offset;
                (
                    index,
                    format!(
                        "<div id=\"h-{index}\" style=\"display:contents\">{}</div>",
                        html(ItemRow { item, view })
                    ),
                )
            })
            .collect()
    }

    fn request_range(&mut self) {
        if self.client.history_pending() {
            return;
        }
        let Some(history) = &self.client.view().history else {
            return;
        };
        let demand = if let Some((item, bytes)) = self.oversized {
            client::HistoryDemand {
                anchor: client::HistoryAnchor::TextAfter { item, offset: 0 },
                bytes,
                ..Default::default()
            }
        } else if let Some(index) = self.refresh.first().copied() {
            client::HistoryDemand {
                anchor: client::HistoryAnchor::After(index),
                bytes: 1,
                ..Default::default()
            }
        } else if self.dom_end < history.count {
            client::HistoryDemand {
                anchor: client::HistoryAnchor::After(self.dom_end),
                bytes: HISTORY_BYTES,
                ..Default::default()
            }
        } else {
            return;
        };
        self.effects.extend(self.client.demand(demand));
    }

    fn show_all(&mut self) {
        let end = self.shown.clone().unwrap_or_default();
        let transcript = html(Transcript {
            view: self.client.view(),
            end: &end,
        });
        self.patch(&Patch::Replace(&transcript));
    }

    /// Shows the end again if it changed, or the transcript whole if the
    /// page does not show it yet.
    fn show_end(&mut self) {
        let end = self.end_row();
        match &self.shown {
            Some(shown) if *shown == end => {}
            Some(_) => {
                let row = html(EndRow { end: &end });
                self.patch(&Patch::Replace(&row));
                self.shown = Some(end);
            }
            None => {
                self.shown = Some(end);
                self.show_all();
            }
        }
    }

    fn patch(&mut self, patch: &Patch) {
        patch.write_to(&mut self.out);
    }
}

/// Patches in what the client's effects change.
impl Renderer for Page {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect> {
        for show in shown {
            match show {
                Show::View(update) => self.update(update),
                Show::Notice(notice) => {
                    self.forking = false;
                    self.error = Some(notice.to_string());
                }
                Show::Link if self.ended() => {
                    if let Link::Ended(client::End::Lost(why)) = self.client.link()
                        && self.shown.is_none()
                    {
                        self.error = Some(format!("Could not attach: {why}"));
                    }
                    self.flush();
                    self.show_end();
                }
                Show::Link | Show::Sessions | Show::History { .. } => {}
            }
        }
        std::mem::take(&mut self.effects)
    }
}

#[cfg(test)]
mod tests {

    use client::{ConnId, Error, Event, EventKind, Welcome};
    use protocol::{Body, CallId, Caps, DaemonMsg, Entry, Epoch, Json, Outcome, Seq, SessionId};

    use super::{HISTORY_BYTES, Page};

    fn connected(conn: u64) -> Event {
        Event {
            conn: ConnId(conn),
            kind: EventKind::Connected {
                welcome: Welcome {
                    proto: 1,
                    caps: Caps::ALL,
                },
                remote: false,
                commands: Vec::new(),
                machine: protocol::MachineId([0; 16]),
                upgrade: None,
            },
        }
    }

    fn closed(conn: u64) -> Event {
        Event {
            conn: ConnId(conn),
            kind: EventKind::Closed(Error::Closed),
        }
    }

    /// A page whose connection is up.
    fn page() -> Page {
        let (mut page, _) = Page::new(SessionId::new());
        page.event(connected(1));
        page
    }

    impl Page {
        /// Takes in what the driver reports, as the pump would.
        fn event(&mut self, event: Event) {
            let effects = self.client.step(event);
            client::pump(|_| Ok(()), self, effects);
        }

        /// Folds `msgs` in as one batch from the open connection.
        fn fold(&mut self, msgs: &[DaemonMsg]) {
            for msg in msgs {
                let conn = self.client.connection();
                self.event(Event {
                    conn,
                    kind: EventKind::Msg(msg.clone()),
                });
            }
            self.settle();
        }
    }

    fn entry(seq: u64, body: Body) -> DaemonMsg {
        DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(1), body))
    }

    fn user(seq: u64, text: &str) -> DaemonMsg {
        entry(
            seq,
            Body::Input {
                segments: text.into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        )
    }

    fn delta(call: u64, text: &str) -> DaemonMsg {
        DaemonMsg::Delta {
            call: CallId::new(call).unwrap(),
            text: text.into(),
            attempt: 0,
        }
    }

    const NONE: [String; 0] = [];

    /// A page synced to a session holding one message, and shown.
    fn synced() -> Page {
        let mut page = page();
        page.fold(&[
            DaemonMsg::Attached {
                session: SessionId::new(),
                workspace: "/w".into(),
            },
            user(1, "hi"),
            DaemonMsg::Activity {
                seq: 1,
                activity: protocol::Activity::Idle,
            },
            DaemonMsg::Synced { seq: 1 },
        ]);
        page.take();
        page
    }

    /// The events of `page` not yet taken, one string each.
    fn events(page: &mut Page) -> Vec<String> {
        let out = String::from_utf8(page.take()).unwrap();
        out.split_terminator("\n\n").map(str::to_owned).collect()
    }

    fn deliver_history(page: &mut Page, message: DaemonMsg) -> Vec<client::Io> {
        let effects = page.client.step(Event {
            conn: page.client.connection(),
            kind: EventKind::Msg(message),
        });
        let mut sent = Vec::new();
        client::pump(
            |io| {
                sent.push(io);
                Ok(())
            },
            page,
            effects,
        );
        sent
    }
    fn requested_range(sent: Vec<client::Io>) -> (u64, protocol::history::Request) {
        sent.into_iter()
            .find_map(|io| match io {
                client::Io::Send(protocol::ClientMsg::History { id, range }) => Some((id, range)),
                _ => None,
            })
            .unwrap()
    }

    fn range_json(
        index: &protocol::view::Index,
        entries: &[Entry],
        range: &protocol::history::Request,
    ) -> String {
        serde_json::to_string(
            &index
                .page(range, |seq| {
                    Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
                })
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn replacing_a_pending_oversized_item_cancels_its_old_request() {
        let mut page = page();
        let session = page.session;
        let mut entries = vec![Entry::new(
            Seq::new(1).unwrap(),
            Epoch(1),
            Body::Input {
                segments: format!("/mode {}auto", " ".repeat(HISTORY_BYTES + 100)).into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        )];
        let mut index = protocol::view::Index::new(session, "/w".into());
        index.apply(&entries[0]);
        deliver_history(
            &mut page,
            DaemonMsg::Attached {
                session,
                workspace: "/w".into(),
            },
        );
        let (id, range) = requested_range(deliver_history(
            &mut page,
            DaemonMsg::Head {
                head: Box::new(index.head(Epoch(1))),
            },
        ));
        deliver_history(&mut page, DaemonMsg::Synced { seq: 1 });
        let fetched = range_json(&index, &entries, &range);
        let (old_id, old_range) = requested_range(deliver_history(
            &mut page,
            DaemonMsg::HistoryPart {
                id,
                offset: 0,
                text: fetched,
                done: true,
            },
        ));
        assert!(page.oversized.is_some());
        assert_eq!(page.dom_end, 0);
        page.take();
        entries.push(Entry::new(
            Seq::new(2).unwrap(),
            Epoch(1),
            Body::ToolCall {
                invoke: None,
                tool: "mode".into(),
                input: Json::encode(&serde_json::json!({"mode":"auto"})).unwrap(),
                id: None,
                card: None,
                by: protocol::Audience::User,
            },
        ));
        index.apply(&entries[1]);
        let sent = deliver_history(
            &mut page,
            DaemonMsg::Head {
                head: Box::new(index.head_after(Epoch(1), 1)),
            },
        );
        assert!(sent.iter().any(
            |io| matches!(io, client::Io::Send(protocol::ClientMsg::Cancel { id }) if *id == old_id)
        ));
        let (id, range) = requested_range(sent);
        assert_eq!(range.text_offset, None);
        let stale = range_json(&index, &entries, &old_range);
        deliver_history(
            &mut page,
            DaemonMsg::HistoryPart {
                id: old_id,
                offset: 0,
                text: stale,
                done: true,
            },
        );
        assert_eq!(page.take(), [] as [u8; 0]);
        let fetched = range_json(&index, &entries, &range);
        deliver_history(
            &mut page,
            DaemonMsg::HistoryPart {
                id,
                offset: 0,
                text: fetched,
                done: true,
            },
        );
        let patches = String::from_utf8(page.take()).unwrap();
        assert!(patches.contains("id=\"h-0\"") && patches.contains("id=\"i2\""));
        assert!(patches.contains("data: mode before"));
        assert_eq!(page.dom_end, 1);
        assert!(page.oversized.is_none());
    }

    #[test]
    fn oversized_items_keep_canonical_markup_and_release_server_pages() {
        let mut page = page();
        let session = page.session;
        let texts: Vec<_> = (1..=3)
            .map(|seq| {
                if seq == 1 {
                    format!("unique-{seq} {}", "x".repeat(1_100_000))
                } else {
                    format!("## unique-{seq}\n{}", "x".repeat(1_100_000))
                }
            })
            .collect();
        let entries: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(offset, text)| {
                let body = if offset == 0 {
                    Body::Input {
                        segments: text.as_str().into(),
                        mode: protocol::Mode::Queue,
                        taken: true,
                        origin: None,
                        force: false,
                    }
                } else {
                    Body::Assistant { text: text.clone() }
                };
                Entry::new(Seq::new(offset as u64 + 1).unwrap(), Epoch(1), body)
            })
            .collect();
        let mut index = protocol::view::Index::new(session, "/w".into());
        for entry in &entries {
            index.apply(entry);
        }
        let mut messages = std::collections::VecDeque::from([
            DaemonMsg::Attached {
                session,
                workspace: "/w".into(),
            },
            DaemonMsg::Head {
                head: Box::new(index.head(Epoch(1))),
            },
            DaemonMsg::Synced { seq: 3 },
        ]);
        let mut whole_items = 0;
        let mut pages = 0;
        while let Some(message) = messages.pop_front() {
            let effects = page.client.step(Event {
                conn: page.client.connection(),
                kind: EventKind::Msg(message),
            });
            client::pump(
                |io| {
                    if let client::Io::Send(protocol::ClientMsg::History { id, range }) = io {
                        let fetched = index
                            .page(&range, |seq| {
                                Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
                            })
                            .unwrap();
                        if range.bytes > HISTORY_BYTES {
                            assert_eq!(range.text_offset, Some(0));
                            let span = fetched.fragment.as_ref().unwrap();
                            assert_eq!((span.start, span.end), (0, span.total));
                            assert_eq!(fetched.items.len(), 1);
                            whole_items += 1;
                        }
                        pages += 1;
                        messages.push_back(DaemonMsg::HistoryPart {
                            id,
                            offset: 0,
                            text: serde_json::to_string(&fetched).unwrap(),
                            done: true,
                        });
                    }
                    Ok(())
                },
                &mut page,
                effects,
            );
        }
        assert!(pages > 3);
        assert_eq!(whole_items, 3);
        assert_eq!(page.dom_end, 3);
        assert!(page.client.view().items.is_empty());
        let patches = String::from_utf8(page.take()).unwrap();
        assert_eq!(patches.matches("id=\"transcript\"").count(), 1);
        for seq in 1..=3 {
            assert_eq!(patches.matches(&format!("unique-{seq}")).count(), 1);
            if seq > 1 {
                assert!(patches.contains(&format!("unique-{seq}</h2>")));
            }
        }
    }

    #[test]
    fn a_committed_range_is_inserted_with_one_dom_operation() {
        let mut page = page();
        let session = page.session;
        let entries: Vec<_> = (1..=3)
            .map(|seq| {
                Entry::new(
                    Seq::new(seq).unwrap(),
                    Epoch(1),
                    Body::Assistant {
                        text: format!("unique-{seq}"),
                    },
                )
            })
            .collect();
        let mut index = protocol::view::Index::new(page.session, "/w".into());
        for entry in &entries {
            index.apply(entry);
        }
        deliver_history(
            &mut page,
            DaemonMsg::Attached {
                session,
                workspace: "/w".into(),
            },
        );
        let (id, range) = requested_range(deliver_history(
            &mut page,
            DaemonMsg::Head {
                head: Box::new(index.head(Epoch(1))),
            },
        ));
        deliver_history(&mut page, DaemonMsg::Synced { seq: 3 });
        deliver_history(
            &mut page,
            DaemonMsg::HistoryPart {
                id,
                offset: 0,
                text: range_json(&index, &entries, &range),
                done: true,
            },
        );
        let patches = String::from_utf8(page.take()).unwrap();
        assert_eq!(patches.matches("data: mode before\n").count(), 1);
        for seq in 1..=3 {
            assert_eq!(patches.matches(&format!("unique-{seq}")).count(), 1);
        }
    }

    #[test]
    fn submission_admission_includes_sdk_and_driver_rejection() {
        let mut page = synced();
        let effects = page.line("/interrupt");
        client::pump(|_| Ok(()), &mut page, effects);
        assert!(!page.admitted());
        let effects = page.line("send this draft");
        client::pump(|_| Ok(()), &mut page, effects);
        assert!(page.admitted());
        let effects = page.line("transport rejected");
        client::pump(|_| Err(Error::Busy), &mut page, effects);
        assert!(!page.admitted());
        let effects = page.line("/help");
        client::pump(|_| Ok(()), &mut page, effects);
        assert!(!page.admitted());
    }

    #[test]
    fn the_transcript_shows_whole_once_synced() {
        let mut page = page();
        page.fold(&[
            DaemonMsg::Attached {
                session: SessionId::new(),
                workspace: "/w".into(),
            },
            user(1, "hi"),
        ]);
        assert_eq!(events(&mut page), NONE, "patched while replaying");
        page.fold(&[DaemonMsg::Synced { seq: 1 }]);
        let shown = events(&mut page);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(
            shown[0].contains("data: elements <div id=\"transcript\"")
                && shown[0].contains(">hi<")
                && shown[0].contains("<div id=\"end\""),
            "{shown:?}"
        );
    }

    #[test]
    fn a_reply_streams_in_by_appends() {
        let mut page = synced();
        page.fold(&[entry(
            2,
            Body::ModelCall {
                request: None,
                model: "m".into(),
                attempt: 0,
                choice: protocol::ToolChoice::Auto,
                r#for: None,
                purpose: None,
            },
        )]);
        let added = events(&mut page);
        assert!(
            added[0].contains("data: selector #end\ndata: mode before\n"),
            "{added:?}"
        );
        assert!(added[0].contains("thinking…"), "{added:?}");
        assert!(added[1].contains(">Stop<"), "running: {added:?}");

        // The first text replaces "thinking…"; the rest is appended, as
        // one append for the deltas of one batch.
        page.fold(&[delta(2, "a <b>")]);
        let first = events(&mut page);
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(
            first[0].contains("<div id=\"live\"")
                && first[0].contains("<span id=\"live-reply-2\">a &lt;b&gt;</span>"),
            "{first:?}"
        );
        page.fold(&[delta(2, " c"), delta(2, " d")]);
        assert_eq!(
            events(&mut page),
            ["event: datastar-patch-elements\n\
              data: selector #live-reply-2\n\
              data: mode append\n\
              data: elements <span> c d</span>"]
        );

        // The committed reply replaces the streamed one; Stop stays until idle.
        page.fold(&[entry(
            3,
            Body::ModelResult {
                r#for: None,
                failure: None,
                call: CallId::new(2).unwrap(),
                outcome: Outcome::Done {
                    output: Json::encode(&serde_json::json!({ "text": "a <b> c d" })).unwrap(),
                },
            },
        )]);
        let mut settled = events(&mut page);
        assert_eq!(settled.len(), 1, "{settled:?}");
        page.fold(&[DaemonMsg::Activity {
            seq: 3,
            activity: protocol::Activity::Idle,
        }]);
        settled.extend(events(&mut page));
        assert_eq!(settled.len(), 2, "{settled:?}");
        assert!(
            settled[0].starts_with("event: datastar-patch-elements\ndata: elements <div id=\"i2\""),
            "{settled:?}"
        );
        assert!(!settled[0].contains("reply-2"), "{settled:?}");
        assert!(
            settled[1].contains("<div id=\"end\"") && !settled[1].contains("Stop"),
            "{settled:?}"
        );
    }

    #[test]
    fn retries_in_one_batch_replace_buffered_text_before_appending() {
        let mut page = synced();
        page.fold(&[entry(
            2,
            Body::ModelCall {
                request: None,
                model: "m".into(),
                attempt: 0,
                choice: protocol::ToolChoice::Auto,
                r#for: None,
                purpose: None,
            },
        )]);
        events(&mut page);
        page.fold(&[
            delta(2, "failed attempt"),
            DaemonMsg::Delta {
                call: CallId::new(2).unwrap(),
                attempt: 1,
                text: "€new".into(),
            },
            DaemonMsg::Delta {
                call: CallId::new(2).unwrap(),
                attempt: 1,
                text: "!".into(),
            },
        ]);
        let patches = events(&mut page);
        assert_eq!(patches.len(), 1);
        let reply = patches[0]
            .split_once("<span id=\"live-reply-2\">")
            .and_then(|(_, tail)| tail.split_once("</span>"))
            .map(|(text, _)| text);
        assert_eq!(reply, Some("€new!"), "{patches:?}");
        page.fold(&[DaemonMsg::Delta {
            call: CallId::new(2).unwrap(),
            attempt: 1,
            text: "?".into(),
        }]);
        assert_eq!(
            events(&mut page),
            [
                "event: datastar-patch-elements\ndata: selector #live-reply-2\ndata: mode append\ndata: elements <span>?</span>"
            ]
        );
    }

    #[test]
    fn a_titling_reply_titles_the_bar_and_not_the_transcript() {
        let mut page = synced();
        assert!(!page.title_dirty);
        let [call, result] = title(2, "Greeting <you>");
        page.fold(&[call]);
        page.take();
        let conn = page.client.connection();
        page.event(Event {
            conn,
            kind: EventKind::Msg(result),
        });
        assert!(page.title_dirty);
        page.settle();
        assert!(!page.title_dirty);
        let titled = events(&mut page);
        let title = "data: elements <span id=\"title\"";
        let shown: Vec<_> = titled.iter().filter(|e| e.contains("Greeting")).collect();
        assert_eq!(shown.len(), 1, "{titled:?}");
        assert!(
            shown[0].contains(title) && shown[0].contains("Greeting &lt;you&gt;"),
            "{titled:?}"
        );
        page.fold(&[]);
        assert_eq!(events(&mut page), NONE, "the title shows once");
    }

    fn title(seq: u64, text: &str) -> [DaemonMsg; 2] {
        [
            entry(
                seq,
                Body::ModelCall {
                    request: None,
                    model: "m".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: Some(CallId::new(1).unwrap()),
                    purpose: Some("title".into()),
                },
            ),
            entry(
                seq + 1,
                Body::ModelResult {
                    r#for: Some(CallId::new(1).unwrap()),
                    failure: None,
                    call: CallId::new(seq).unwrap(),
                    outcome: Outcome::Done {
                        output: Json::encode(&serde_json::json!({
                            "message": { "content": [{ "type": "text", "text": text }] },
                        }))
                        .unwrap(),
                    },
                },
            ),
        ]
    }

    #[test]
    fn an_absent_title_stays_cached_during_deltas_and_unrelated_items() {
        let mut page = synced();
        assert!(!page.title_dirty);
        page.fold(&[entry(
            2,
            Body::ModelCall {
                request: None,
                model: "m".into(),
                attempt: 0,
                choice: protocol::ToolChoice::Auto,
                r#for: None,
                purpose: None,
            },
        )]);
        page.take();
        for msg in [delta(2, "a"), user(3, "next")] {
            let conn = page.client.connection();
            page.event(Event {
                conn,
                kind: EventKind::Msg(msg),
            });
            assert!(!page.title_dirty, "unrelated update invalidated the title");
            page.settle();
            assert_eq!(page.titled, None);
        }
    }

    #[test]
    fn snapshots_replace_and_remove_the_cached_title() {
        let mut page = synced();
        page.fold(&title(2, "Old"));
        page.take();
        for text in [Some("New"), None] {
            let mut view = protocol::view::SessionView::default();
            view.fold(&DaemonMsg::Attached {
                session: page.client.view().session.unwrap(),
                workspace: "/w".into(),
            });
            view.fold(&user(1, "hi"));
            if let Some(text) = text {
                for msg in title(2, text) {
                    view.fold(&msg);
                }
            }
            page.fold(&[DaemonMsg::Snapshot(Box::new(view.snapshot()))]);
            assert_eq!(page.titled.as_deref(), text);
            assert!(events(&mut page).iter().any(|e| e.contains("id=\"title\"")));
            assert!(!page.title_dirty);
        }
    }

    #[test]
    fn a_replayed_title_waits_for_reconnect_to_sync() {
        let mut page = synced();
        page.fold(&title(2, "Old"));
        page.take();
        page.fold(&[DaemonMsg::Draining { held: None }]);
        let session = page.client.view().session.unwrap();
        page.event(closed(1));
        page.event(connected(2));
        page.fold(&[DaemonMsg::Attached {
            session,
            workspace: "/w".into(),
        }]);
        page.fold(&title(4, "Replayed"));
        assert_eq!(events(&mut page), NONE);
        assert_eq!(page.titled.as_deref(), Some("Old"));
        assert!(page.title_dirty);
        page.fold(&[DaemonMsg::Synced { seq: 5 }]);
        assert_eq!(page.titled.as_deref(), Some("Replayed"));
        assert!(!page.title_dirty);
        assert!(
            events(&mut page)
                .iter()
                .any(|e| e.contains("id=\"title\"") && e.contains("Replayed"))
        );
    }

    #[test]
    fn switching_sessions_clears_the_cached_title() {
        let mut page = synced();
        page.fold(&title(2, "Old"));
        page.take();
        let session = SessionId::new();
        let effects = page.client.attach(client::Attach::new(
            protocol::Target::Id(session),
            client::Env::Keep,
        ));
        client::pump(|_| Ok(()), &mut page, effects);
        page.event(connected(2));
        page.fold(&[
            DaemonMsg::Attached {
                session,
                workspace: "/w".into(),
            },
            user(1, "new session"),
            DaemonMsg::Synced { seq: 1 },
        ]);
        assert_eq!(page.titled, None);
        assert!(!page.title_dirty);
        assert!(
            events(&mut page)
                .iter()
                .any(|e| e.contains("id=\"title\"") && !e.contains("Old"))
        );
    }

    #[test]
    fn after_an_upgrade_the_transcript_shows_whole_again() {
        let mut page = synced();
        page.fold(&[DaemonMsg::Draining { held: None }]);
        assert_eq!(events(&mut page), NONE);
        let session = page.client.view().session.unwrap();
        page.event(closed(1));
        page.event(connected(2));
        // The new daemon replays what was missed, then syncs.
        page.fold(&[
            DaemonMsg::Attached {
                session,
                workspace: "/w".into(),
            },
            user(2, "again"),
        ]);
        assert_eq!(events(&mut page), NONE, "patched while replaying");
        page.fold(&[DaemonMsg::Synced { seq: 2 }]);
        let shown = events(&mut page);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(shown[0].contains(">hi<") && shown[0].contains(">again<"));
    }

    #[test]
    fn the_end_says_the_daemon_went_away() {
        let mut page = synced();
        page.event(closed(1));
        let ended = events(&mut page);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert!(ended[0].contains("reload to reconnect"), "{ended:?}");

        // Before the transcript showed, the end comes with it.
        let (mut page, _) = Page::new(SessionId::new());
        page.event(Event {
            conn: ConnId(1),
            kind: EventKind::Failed(Error::Refused("refused".into())),
        });
        let failed = events(&mut page);
        assert!(
            failed[0].contains("<div id=\"transcript\"") && failed[0].contains("refused"),
            "{failed:?}"
        );
    }
}
