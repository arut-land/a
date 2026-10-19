//! What a tab's chat page shows, folded from the daemon's messages, and the
//! patches that bring the page up to date: the transcript whole once an
//! attach is synced, then each change alone.

use protocol::DaemonMsg;
use protocol::view::{Change, Link, SessionView};

use crate::patch::Patch;
use crate::view::{self, End, EndRow, ItemRow, Transcript, html};

#[derive(Default)]
pub struct Page {
    pub view: SessionView,
    /// The daemon's latest complaint, until the transcript moves on.
    error: Option<String>,
    /// The daemon is gone for good.
    ended: bool,
    /// The end the page shows, once it shows the transcript.
    shown: Option<End>,
    /// Events not yet sent.
    out: Vec<u8>,
}

impl Page {
    /// The events not yet sent.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// The daemon went away, as `error` says if it says why.
    pub fn end(&mut self, error: Option<String>) {
        if error.is_some() {
            self.error = error;
        }
        self.ended = true;
        self.show_end();
    }

    fn end_row(&self) -> End {
        End {
            running: !self.ended && self.view.running(),
            error: self.error.clone(),
            ended: self.ended,
        }
    }

    /// Folds a batch in and adds what it changed to the events to send.
    /// Until the attach is synced (again, after an upgrade) the page is
    /// left as it is; then it is shown whole.
    pub fn fold(&mut self, batch: &[DaemonMsg]) {
        // Consecutive deltas of one reply go out as one.
        let mut streamed: Option<(u64, String)> = None;
        for msg in batch {
            let before = self.view.items.len();
            let Some(update) = self.view.fold(msg) else {
                continue;
            };
            match &update.change {
                Change::Error { message } => self.error = Some(message.clone()),
                Change::Added { .. } => self.error = None,
                _ => {}
            }
            let change = match update.change {
                Change::Synced => {
                    self.flush(&mut streamed);
                    self.shown = Some(self.end_row());
                    self.show_all();
                    continue;
                }
                change if self.view.link == Link::Live && self.shown.is_some() => change,
                _ => continue,
            };
            if let Change::Delta { call, text } = change {
                match &mut streamed {
                    Some((c, t)) if *c == call => t.push_str(&text),
                    _ => {
                        self.flush(&mut streamed);
                        streamed = Some((call, text));
                    }
                }
                continue;
            }
            self.flush(&mut streamed);
            match change {
                // The first item takes the place of the note that there is
                // none; a call asked about may not have been shown yet.
                Change::Added { index: 0, .. }
                | Change::Snapshot { .. }
                | Change::Recap(_)
                | Change::Attached { reset: true, .. } => self.show_all(),
                Change::Asked { .. } if self.view.items.len() > before => self.show_all(),
                Change::Added { item, .. } => {
                    let item = html(ItemRow {
                        item: &item,
                        view: &self.view,
                    });
                    self.patch(&Patch::Before {
                        selector: "#end",
                        html: &item,
                    });
                }
                Change::Updated { item, .. } | Change::Asked { item, .. } => {
                    let item = html(ItemRow {
                        item: &item,
                        view: &self.view,
                    });
                    self.patch(&Patch::Replace(&item));
                }
                _ => {}
            }
        }
        self.flush(&mut streamed);
        if self.view.link == Link::Live {
            self.show_end();
        }
    }

    /// Adds the text streamed into reply `call` to its element; the first
    /// text replaces the reply's "thinking…".
    fn flush(&mut self, streamed: &mut Option<(u64, String)>) {
        let Some((call, text)) = streamed.take() else {
            return;
        };
        let started = self
            .view
            .streaming
            .as_ref()
            .is_some_and(|s| s.call == call && s.text.len() == text.len());
        if started {
            let Some(item) = self.view.items.iter().find(|i| i.key() == call) else {
                return;
            };
            let item = html(ItemRow {
                item,
                view: &self.view,
            });
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
            view: &self.view,
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use protocol::{Body, DaemonMsg, Entry, Json, Outcome, SessionId};

    use super::Page;

    fn entry(seq: u64, body: Body) -> DaemonMsg {
        DaemonMsg::Entry(Entry::new(seq, 1, body))
    }

    fn user(seq: u64, text: &str) -> DaemonMsg {
        entry(seq, Body::User { text: text.into() })
    }

    fn delta(call: u64, text: &str) -> DaemonMsg {
        DaemonMsg::Delta {
            call,
            text: text.into(),
        }
    }

    const NONE: [String; 0] = [];

    /// A page synced to a session holding one message, and shown.
    fn synced() -> Page {
        let mut page = Page::default();
        page.fold(&[
            DaemonMsg::Attached {
                session: SessionId::new(),
                workspace: PathBuf::from("/w"),
            },
            user(1, "hi"),
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
    fn the_transcript_shows_whole_once_synced() {
        let mut page = Page::default();
        page.fold(&[
            DaemonMsg::Attached {
                session: SessionId::new(),
                workspace: PathBuf::from("/w"),
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
        page.fold(&[entry(2, Body::ModelCall { model: "m".into() })]);
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

        // The committed reply replaces the streamed one, and Stop goes.
        page.fold(&[entry(
            3,
            Body::ModelResult {
                call: 2,
                outcome: Outcome::Done {
                    output: Json::from(serde_json::json!({ "text": "a <b> c d" })),
                },
            },
        )]);
        let settled = events(&mut page);
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
    fn after_an_upgrade_the_transcript_shows_whole_again() {
        let mut page = synced();
        page.fold(&[DaemonMsg::Draining]);
        assert_eq!(events(&mut page), NONE);
        // The new daemon replays what was missed, then syncs.
        page.fold(&[
            DaemonMsg::Attached {
                session: page.view.session.unwrap(),
                workspace: PathBuf::from("/w"),
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
        page.end(None);
        let ended = events(&mut page);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert!(ended[0].contains("reload to reconnect"), "{ended:?}");

        // Before the transcript showed, the end comes with it.
        let mut page = Page::default();
        page.end(Some("Could not attach: refused".into()));
        let failed = events(&mut page);
        assert!(
            failed[0].contains("<div id=\"transcript\"") && failed[0].contains("refused"),
            "{failed:?}"
        );
    }
}
