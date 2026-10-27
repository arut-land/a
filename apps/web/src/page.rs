//! What a tab's chat page shows of its session client, and the patches
//! that bring the page up to date: the transcript whole once an attach is
//! synced, then each change alone.

use client::{Attach, Cmd, Effect, Env, Link, Renderer, SessionClient, Show};
use protocol::view::{Change, Item, Part, Status, Update};
use protocol::{SessionId, Target};

use crate::patch::Patch;
use crate::view::{self, End, EndRow, ItemRow, TitleSpan, Transcript, html};

pub struct Page {
    pub client: SessionClient,
    session: SessionId,
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
    /// Events not yet sent.
    out: Vec<u8>,
}

impl Page {
    /// A page for `session`, which keeps the environment it has; and the
    /// attach it asks the driver for.
    pub fn new(session: SessionId) -> (Self, Vec<Effect>) {
        let mut client = SessionClient::new(Env::Keep);
        let open = client.attach(Attach::new(Target::Id(session), Env::Keep));
        let page = Self {
            client,
            session,
            forking: false,
            error: None,
            shown: None,
            titled: None,
            title_dirty: true,
            streamed: None,
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
        self.title_dirty |= match &update.change {
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
        };
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
                self.show_all();
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
            // The first item takes the place of the note that there is
            // none; a sync brings the recap.
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
        let view = self.client.view();
        // Shown whole when it started streaming, or started again (a
        // retry) and so has none of this text.
        let started = view
            .streaming
            .as_ref()
            .is_none_or(|s| s.call != call || s.attempt != attempt || s.text.len() == text.len());
        if started {
            let Some(item) = view.items.iter().find(|i| i.key() == call.0) else {
                return;
            };
            let item = html(ItemRow { item, view });
            self.patch(&Patch::Replace(&item));
        } else {
            let selector = view::reply_id(call);
            self.patch(&Patch::Append {
                selector: &selector,
                html: &view::delta(&text),
            });
        }
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
                Show::Link | Show::Sessions => {}
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {

    use client::{ConnId, Error, Event, EventKind, Welcome};
    use protocol::{Body, CallId, Caps, DaemonMsg, Entry, Epoch, Json, Outcome, Seq, SessionId};

    use super::Page;

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
            first[0].contains("<div id=\"i2\"")
                && first[0].contains("<span id=\"reply-2\">a &lt;b&gt;</span>"),
            "{first:?}"
        );
        page.fold(&[delta(2, " c"), delta(2, " d")]);
        assert_eq!(
            events(&mut page),
            ["event: datastar-patch-elements\n\
              data: selector #reply-2\n\
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
            .split_once("<span id=\"reply-2\">")
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
                "event: datastar-patch-elements\ndata: selector #reply-2\ndata: mode append\ndata: elements <span>?</span>"
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
