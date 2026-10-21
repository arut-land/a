use protocol::{Activity, view::SessionView};

pub type SubmissionId = protocol::Author;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionState {
    Queued,
    Taken,
    Finished(Activity),
}

/// Ownership comes from the log; only ordered activity certifies completion.
pub struct Submission {
    pub id: SubmissionId,
    pub state: SubmissionState,
}

impl Submission {
    pub const fn new(id: SubmissionId) -> Self {
        Self {
            id,
            state: SubmissionState::Queued,
        }
    }

    pub fn fold(&mut self, view: &SessionView) {
        if self.state == SubmissionState::Queued && view.state.taken.contains(&self.id) {
            self.state = SubmissionState::Taken;
        }
        if self.state == SubmissionState::Taken
            && let Some(activity) = view.activity
            && (activity == Activity::Idle || activity.requires_action())
        {
            self.state = SubmissionState::Finished(activity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{
        Audience, Author, Body, ClientId, ClientSeq, DaemonMsg, Decode, Entry, Epoch, Json, Mode,
        Seq,
    };

    fn fold(submission: &mut Submission, view: &mut SessionView, msg: &DaemonMsg) {
        view.fold(msg);
        submission.fold(view);
    }

    fn origin() -> Author {
        Author {
            client: ClientId::new(),
            seq: ClientSeq::new(1).unwrap(),
        }
    }

    fn entry(seq: u64, body: Body) -> DaemonMsg {
        DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(0), body))
    }

    fn input(origin: Author, taken: bool) -> Body {
        Body::Input {
            text: "same".into(),
            mode: Mode::Queue,
            taken,
            origin: Some(origin),
        }
    }

    #[test]
    fn identical_text_from_another_client_cannot_complete_a_submission() {
        let mut submission = Submission::new(origin());
        let mut view = SessionView::default();
        let id = submission.id;
        fold(&mut submission, &mut view, &entry(1, input(origin(), true)));
        fold(
            &mut submission,
            &mut view,
            &DaemonMsg::Activity {
                seq: 1,
                activity: Activity::Idle,
            },
        );
        assert_eq!(submission.state, SubmissionState::Queued);
        fold(&mut submission, &mut view, &entry(2, input(id, false)));
        fold(
            &mut submission,
            &mut view,
            &DaemonMsg::Activity {
                seq: 2,
                activity: Activity::Idle,
            },
        );
        assert_eq!(submission.state, SubmissionState::Queued);
        let encoded = serde_json::to_vec(&DaemonMsg::Snapshot(Box::new(view.snapshot()))).unwrap();
        view = SessionView::default();
        fold(
            &mut submission,
            &mut view,
            &DaemonMsg::from_json(&encoded).unwrap(),
        );
        fold(
            &mut submission,
            &mut view,
            &entry(
                3,
                Body::Take {
                    input: Seq::new(2).unwrap(),
                },
            ),
        );
        assert_eq!(submission.state, SubmissionState::Taken);
        let encoded = serde_json::to_vec(&DaemonMsg::Snapshot(Box::new(view.snapshot()))).unwrap();
        view = SessionView::default();
        submission = Submission::new(id);
        fold(
            &mut submission,
            &mut view,
            &DaemonMsg::from_json(&encoded).unwrap(),
        );
        assert_eq!(submission.state, SubmissionState::Taken);
        fold(
            &mut submission,
            &mut view,
            &DaemonMsg::Activity {
                seq: 3,
                activity: Activity::Idle,
            },
        );
        assert_eq!(submission.state, SubmissionState::Finished(Activity::Idle));
    }

    #[test]
    fn completion_waits_for_certified_activity() {
        let mut submission = Submission::new(origin());
        let mut view = SessionView::default();
        let id = submission.id;
        fold(&mut submission, &mut view, &entry(1, input(id, true)));
        fold(
            &mut submission,
            &mut view,
            &entry(
                2,
                Body::Assistant {
                    text: "done".into(),
                },
            ),
        );
        for seq in [0, 1, 3] {
            fold(
                &mut submission,
                &mut view,
                &DaemonMsg::Activity {
                    seq,
                    activity: Activity::Idle,
                },
            );
            assert_eq!(submission.state, SubmissionState::Taken);
        }
        fold(
            &mut submission,
            &mut view,
            &DaemonMsg::Activity {
                seq: 2,
                activity: Activity::Running,
            },
        );
        assert_eq!(submission.state, SubmissionState::Taken);
        fold(
            &mut submission,
            &mut view,
            &DaemonMsg::Activity {
                seq: 2,
                activity: Activity::Idle,
            },
        );
        assert_eq!(submission.state, SubmissionState::Finished(Activity::Idle));
    }

    #[test]
    fn reconnect_snapshot_retains_ownership_of_a_taken_slash_command() {
        let id = Author {
            client: ClientId::new(),
            seq: ClientSeq::new(1).unwrap(),
        };
        let mut daemon_view = SessionView::default();
        for msg in [
            entry(
                1,
                Body::Input {
                    text: "/model m".into(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: Some(id),
                },
            ),
            entry(
                2,
                Body::ToolCall {
                    invoke: None,
                    tool: "model".into(),
                    input: Json::null(),
                    id: None,
                    card: None,
                    by: Audience::User,
                },
            ),
        ] {
            daemon_view.fold(&msg);
        }
        let mut view = SessionView::default();
        let encoded =
            serde_json::to_vec(&DaemonMsg::Snapshot(Box::new(daemon_view.snapshot()))).unwrap();
        view.fold(&DaemonMsg::from_json(&encoded).unwrap());
        view.fold(&DaemonMsg::Activity {
            seq: 2,
            activity: Activity::Idle,
        });
        let mut submission = Submission::new(id);
        submission.fold(&view);
        assert_eq!(submission.state, SubmissionState::Finished(Activity::Idle));
    }
}
