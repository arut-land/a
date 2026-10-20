//! `policy`: which calls wait for an approval, by the session's mode, and
//! who gives it: the user, or in `judge` mode a model asked aside. And
//! `policy-floor`: the operator's rules, which may only tighten.
//!
//! A mode is the session's durable state (the `mode` fold), changed with
//! `/mode` as any extension or client could change it; the default is the
//! configured one. The floor is guarded, and `permission` takes the most
//! restrictive answer, so no mode, and no replacement of `policy`, makes
//! a call the floor asks about or denies run unasked.

use ext::point::{Approve, Permission as PermissionPoint, ToolUse};
use ext::{Answer, Args, Cx, Extension, Fold, Later, PointCx, Registrar, Side, Tool};
use protocol::{Audience, Permission};
use serde::{Deserialize, Serialize};

/// How the session's calls are let through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Calls of the tools asked about wait for the user.
    #[default]
    Ask,
    /// As `ask`, but edits run unasked.
    AcceptEdits,
    /// Everything runs unasked (but what the floor holds back).
    Auto,
    /// A model asked aside approves or refuses what `ask` would ask the
    /// user about; when it cannot say, the user is asked.
    Judge,
}

impl Mode {
    pub const ALL: [Self; 4] = [Self::Ask, Self::AcceptEdits, Self::Auto, Self::Judge];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "auto",
            Self::Judge => "judge",
        }
    }

    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }
}

/// The session's mode, once one is set: before, the configured default.
#[derive(Debug, Default)]
pub struct ModeState(pub Option<Mode>);

impl Fold for ModeState {
    const NAME: &'static str = "mode";
    type Change = Mode;

    fn apply(&mut self, mode: &Mode) {
        self.0 = Some(*mode);
    }
}

/// The `policy` extension.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// The tools whose calls are asked about.
    pub ask: Vec<String>,
    /// The tools `accept-edits` lets through.
    pub edits: Vec<String>,
    /// A session's mode until it sets one.
    pub mode: Mode,
}

impl Policy {
    pub fn ask(tools: &[&str]) -> Self {
        Self {
            ask: tools.iter().map(|&t| t.to_owned()).collect(),
            edits: vec!["edit".to_owned()],
            mode: Mode::default(),
        }
    }

    /// The mode the session `cx` decides for is in.
    fn mode(&self, cx: &PointCx<'_>) -> Mode {
        cx.state(|m: &ModeState| m.0).unwrap_or(self.mode)
    }

    /// Whether `call` may run in `mode`.
    fn permission(&self, mode: Mode, call: ToolUse<'_>) -> Permission {
        let named = |tools: &[String]| tools.iter().any(|t| t == call.tool);
        match mode {
            Mode::Auto => Permission::Allow,
            Mode::AcceptEdits if named(&self.edits) => Permission::Allow,
            _ if named(&self.ask) => Permission::Ask,
            _ => Permission::Allow,
        }
    }
}

impl Extension for Policy {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 2;

    fn contribute(&self, r: &mut Registrar) {
        let (this, that) = (self.clone(), self.clone());
        r.fold::<ModeState>()
            .tool(SetMode(self.mode))
            .on::<PermissionPoint>(move |cx, call| this.permission(this.mode(cx), call))
            .on::<Approve>(move |cx, call| match that.mode(cx) {
                Mode::Judge => judge(cx, call),
                _ => Answer::Now(None),
            });
    }
}

/// `/mode`: the session's mode; `/mode <ask|accept-edits|auto|judge>` sets
/// it, logged as the `mode` fold's change.
pub struct SetMode(pub Mode);

#[derive(Serialize, Deserialize)]
pub struct Moded {
    pub mode: Mode,
}

impl Tool for SetMode {
    type In = Args;
    type Out = Moded;

    const NAME: &'static str = "mode";
    const DESCRIPTION: &'static str = "show or set how calls are approved";
    const AUDIENCE: Audience = Audience::User;

    fn call(&self, Args { args }: &Args, cx: &Cx) -> Result<Moded, String> {
        let args = args.trim();
        if args.is_empty() {
            let mode = cx.state(|m: &ModeState| m.0).unwrap_or(self.0);
            return Ok(Moded { mode });
        }
        let mode = Mode::named(args).ok_or_else(|| {
            let names: Vec<_> = Mode::ALL.iter().map(|m| m.name()).collect();
            format!("no mode {args}: one of {}", names.join(", "))
        })?;
        cx.update::<ModeState>(mode);
        Ok(Moded { mode })
    }
}

/// What the judge is told.
const JUDGE: &str = "You review the tool calls a coding agent makes in the user's workspace. Approve a call unless it could destroy work, leak secrets, or reach beyond the workspace without the user asking for it. Answer with one word: yes to approve, no to refuse.";

/// Asks a model aside whether `call` may run: `yes` approves, `no`
/// refuses, and anything else, or no answer at all, leaves it to the
/// user.
fn judge(cx: &PointCx<'_>, call: ToolUse<'_>) -> Answer<Option<bool>> {
    let Some(cx) = cx.cx() else {
        return Answer::Now(None);
    };
    let question = format!("May the agent run {} with {}?", call.tool, call.input);
    Answer::Later(Later::new(async move {
        let models = cx.models()?;
        let messages = [llm::Message::system(JUDGE), llm::Message::user(question)];
        let mut side = Side::new("judge", messages.into_iter().collect());
        side.budget.max_tokens = 16;
        let reply = models.call(&cx, side).await.ok()?;
        match reply
            .message
            .text()
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    }))
}

/// The `policy-floor` extension: the operator's rules (`$A_HOME`'s
/// `policy.toml`), each asking about or denying calls of a tool, or of a
/// tool on an input whose text holds some text. Guarded, and a rule may
/// only tighten: one that allows is refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Floor {
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The tool, by name; `*` is every tool.
    pub tool: String,
    /// Text the call's input must hold, as its JSON text writes it.
    pub input: Option<String>,
    pub effect: Permission,
}

impl Floor {
    /// The floor of the operator's rules; one that allows is refused.
    pub fn new(rules: impl IntoIterator<Item = sys::config::Rule>) -> Result<Self, String> {
        use sys::config::Effect;
        let rules = rules
            .into_iter()
            .map(|rule| {
                let effect = match rule.effect {
                    Effect::Ask => Permission::Ask,
                    Effect::Deny => Permission::Deny,
                    Effect::Allow => {
                        return Err(format!(
                            "a floor rule for {} allows: the floor may only ask or deny",
                            rule.tool
                        ));
                    }
                };
                Ok(Rule {
                    tool: rule.tool,
                    input: rule.input,
                    effect,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { rules })
    }

    /// The strictest rule `call` meets, if any.
    fn permission(&self, call: ToolUse<'_>) -> Permission {
        self.rules
            .iter()
            .filter(|r| r.tool == "*" || r.tool == call.tool)
            .filter(|r| {
                r.input
                    .as_ref()
                    .is_none_or(|text| call.input.raw().contains(text.as_str()))
            })
            .map(|r| r.effect)
            .max()
            .unwrap_or(Permission::Allow)
    }
}

impl Extension for Floor {
    const NAME: &'static str = "policy-floor";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let this = self.clone();
        r.on::<PermissionPoint>(move |_, call| this.permission(call));
    }
}

#[cfg(test)]
mod tests;
