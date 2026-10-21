use kernel::{Admission, Durability, Gated, Later, Name, Recover, Ticket};
use protocol::{CallId, Entry, Json, Outcome, Seq, SessionId, ToolChoice};
use std::time::Duration;

pub type Effects = smallvec::SmallVec<[Effect; 4]>;

#[derive(Debug, PartialEq)]
pub enum Effect {
    Invocation {
        ticket: Ticket,
        result: Result<CallId, String>,
    },
    Append(Entry),
    Commit {
        through: u64,
        durability: Durability,
        publish: bool,
    },
    Require(Admission),
    Reply {
        ticket: Ticket,
        result: Result<(), String>,
    },
    Side {
        ticket: Ticket,
        side: Option<CallId>,
    },
    Prepare {
        call: CallId,
        attempt: u16,
        choice: ToolChoice,
    },
    Send {
        call: CallId,
    },
    RunTool {
        call: CallId,
        tool: String,
        input: Json,
    },
    Resume {
        call: CallId,
        tool: String,
        input: Json,
        value: Json,
    },
    Await {
        call: CallId,
        answer: Later<Gated>,
    },
    Recover {
        call: CallId,
        answer: Later<Option<(Name, Recover)>>,
    },
    Sleep {
        call: CallId,
        after: Duration,
    },
    Cancel {
        call: CallId,
    },
    StartChild {
        link: Seq,
        child: SessionId,
        input: String,
    },
    Interrupt {
        child: SessionId,
    },
    Report {
        outcome: Outcome,
    },
    Kill {
        pgid: u32,
    },
    MarkBusy(bool),
    Deliver {
        envelope: protocol::Envelope,
        ticket: Option<Ticket>,
    },
    Delivery {
        ticket: Ticket,
        result: Result<(), protocol::InboxRefusal>,
    },
}

impl Effect {
    pub const fn starts(&self) -> bool {
        matches!(
            self,
            Self::Prepare { .. }
                | Self::Send { .. }
                | Self::RunTool { .. }
                | Self::Resume { .. }
                | Self::Await { .. }
                | Self::Recover { .. }
                | Self::Sleep { .. }
                | Self::StartChild { .. }
                | Self::Deliver { .. }
        )
    }

    pub const fn starts_call(&self) -> Option<CallId> {
        match *self {
            Self::Prepare { call, .. }
            | Self::Send { call }
            | Self::RunTool { call, .. }
            | Self::Resume { call, .. }
            | Self::Await { call, .. }
            | Self::Recover { call, .. }
            | Self::Sleep { call, .. } => Some(call),
            _ => None,
        }
    }

    pub const fn external(&self) -> bool {
        self.starts()
            || matches!(
                self,
                Self::Reply { .. }
                    | Self::Side { .. }
                    | Self::Interrupt { .. }
                    | Self::Report { .. }
                    | Self::Kill { .. }
            )
    }
}

impl From<kernel::Work> for Effect {
    fn from(work: kernel::Work) -> Self {
        match work {
            kernel::Work::Invocation { ticket, result } => Self::Invocation { ticket, result },
            kernel::Work::Reply { ticket, result } => Self::Reply { ticket, result },
            kernel::Work::Side { ticket, side } => Self::Side { ticket, side },
            kernel::Work::Prepare {
                call,
                attempt,
                choice,
            } => Self::Prepare {
                call,
                attempt,
                choice,
            },
            kernel::Work::Send { call } => Self::Send { call },
            kernel::Work::RunTool { call, tool, input } => Self::RunTool { call, tool, input },
            kernel::Work::Resume {
                call,
                tool,
                input,
                value,
            } => Self::Resume {
                call,
                tool,
                input,
                value,
            },
            kernel::Work::Await { call, answer } => Self::Await { call, answer },
            kernel::Work::Recover { call, answer } => Self::Recover { call, answer },
            kernel::Work::Sleep { call, after } => Self::Sleep { call, after },
            kernel::Work::Cancel { call } => Self::Cancel { call },
            kernel::Work::StartChild { link, child, input } => {
                Self::StartChild { link, child, input }
            }
            kernel::Work::Interrupt { child } => Self::Interrupt { child },
            kernel::Work::Report { outcome } => Self::Report { outcome },
            kernel::Work::Kill { pgid } => Self::Kill { pgid },
            kernel::Work::MarkBusy(value) => Self::MarkBusy(value),
            kernel::Work::Deliver { envelope, ticket } => Self::Deliver { envelope, ticket },
            kernel::Work::Delivery { ticket, result } => Self::Delivery { ticket, result },
        }
    }
}
