//! Hooks: code at the engine's decision points. Each point has one merge
//! rule, so any set of hooks gives one answer:
//!
//! | Point | Merge |
//! | --- | --- |
//! | `input` | replacement chain over the user's text; the first rejection wins |
//! | `prepare_step` | replacement chain: each hook sees the request the last left |
//! | `request_error` | the first retry wins |
//! | `pre_tool` | replacement chain over the input; the first block wins |
//! | `permission` | the most restrictive wins: deny > defer > ask > allow |
//! | `spawn` | replacement chain over every process a tool starts |
//! | `post_tool` | replacement chain over the outcome |
//! | `on_stop` | the first continuation wins |
//! | `observe`, `session_start`, `session_end` | every hook is told, and changes nothing |
//!
//! Merges report what changed as [`Decision`]s, which the engine logs;
//! `spawn` rewrites are not logged, as they follow from configuration
//! rather than from what happened.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use protocol::{Decision, Entry, Outcome, Permission, SessionId};
use serde_json::Value;

/// One hook. Every method defaults to changing nothing. Its decisions are
/// logged under the name of the [`Extension`](crate::Extension) that
/// registered it.
pub trait Hook: Send + Sync {
    /// What becomes of the user's input before it is logged.
    fn input(&self, _text: &str) -> Said {
        Said::Pass
    }

    /// A replacement for the next model request.
    fn prepare_step(&self, _request: &llm::Request) -> Option<llm::Request> {
        None
    }

    /// A request to send instead of one that failed with `error`, such as
    /// a compacted one after a context overflow.
    fn request_error(&self, _request: &llm::Request, _error: &str) -> Option<llm::Request> {
        None
    }

    /// A replacement for a process a tool is about to start.
    fn spawn(&self, _spawn: &Spawn) -> Option<Spawn> {
        None
    }

    fn pre_tool(&self, _tool: &str, _input: &Value) -> Pre {
        Pre::Pass
    }

    fn permission(&self, _tool: &str, _input: &Value) -> Permission {
        Permission::Allow
    }

    /// Answers a permission `ask`; the first answer wins. An ask no hook
    /// answers goes to the user, durably: see [`Gate::Ask`].
    fn approve(&self, _tool: &str, _input: &Value) -> Option<bool> {
        None
    }

    /// A replacement for a call's outcome, before it is committed.
    fn post_tool(&self, _tool: &str, _input: &Value, _outcome: &Outcome) -> Option<Outcome> {
        None
    }

    /// Text to continue with once the agent stopped.
    fn on_stop(&self, _entries: &[Entry]) -> Option<String> {
        None
    }

    fn observe(&self, _entry: &Entry) {}

    /// The session's actor started: it was created, or loaded again.
    fn session_start(&self, _session: SessionId) {}

    /// The session's actor stopped: it hibernated, or the daemon drained.
    fn session_end(&self, _session: SessionId) {}
}

/// An `input` answer.
pub enum Said {
    Pass,
    Replace(String),
    /// Refuses the input with this reason, which its client is told.
    Reject(String),
}

/// A process a tool starts: `argv[0]` is the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub argv: Vec<String>,
    /// Exactly the variables it runs with.
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

/// A `pre_tool` answer.
pub enum Pre {
    Pass,
    Replace(Value),
    /// Answers the call with this reason instead of running it.
    Block(String),
}

/// What the hooks make of a tool call they do not refuse.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    Run,
    /// No hook answered a permission `ask`: the call runs once the user
    /// approves it.
    Ask,
}

/// Asks the user before running any of these tools.
pub struct AskFor(pub Vec<String>);

impl Hook for AskFor {
    fn permission(&self, tool: &str, _: &Value) -> Permission {
        if self.0.iter().any(|t| t == tool) {
            Permission::Ask
        } else {
            Permission::Allow
        }
    }
}

/// Decisions to log, with the extension whose hook made each.
pub type Decided = Vec<(&'static str, Decision)>;

/// Hooks in the order they run, each with its extension's name, as a
/// [`Registry`](crate::Registry) resolves them. The default, none, allows
/// everything.
#[derive(Default)]
pub struct Hooks(pub(crate) Vec<(&'static str, Arc<dyn Hook>)>);

impl Hooks {
    /// The user's text as the `input` chain leaves it; `Err` is why it was
    /// refused.
    pub fn input(&self, text: &mut String, decided: &mut Decided) -> Result<(), String> {
        for &(name, ref hook) in &self.0 {
            match hook.input(text) {
                Said::Pass => {}
                Said::Replace(next) => {
                    let original = std::mem::replace(text, next);
                    decided.push((name, Decision::Input { original }));
                }
                Said::Reject(reason) => return Err(format!("{name}: {reason}")),
            }
        }
        Ok(())
    }

    /// The request to retry model call `call` with after `error`, for the
    /// `attempt`th time, if any hook offers one.
    pub fn request_error(
        &self,
        (call, attempt): (u64, u32),
        request: &llm::Request,
        error: &str,
        decided: &mut Decided,
    ) -> Option<llm::Request> {
        let (name, retry) = self
            .0
            .iter()
            .find_map(|(name, hook)| Some((*name, hook.request_error(request, error)?)))?;
        let error = error.to_owned();
        decided.push((
            name,
            Decision::Retry {
                call,
                attempt,
                error,
            },
        ));
        Some(retry)
    }

    pub fn spawn(&self, spawn: &mut Spawn) {
        for (_, hook) in &self.0 {
            if let Some(next) = hook.spawn(spawn) {
                *spawn = next;
            }
        }
    }

    pub fn session_start(&self, session: SessionId) {
        for (_, hook) in &self.0 {
            hook.session_start(session);
        }
    }

    pub fn session_end(&self, session: SessionId) {
        for (_, hook) in &self.0 {
            hook.session_end(session);
        }
    }

    pub fn prepare_step(&self, call: u64, request: &mut llm::Request, decided: &mut Decided) {
        for &(name, ref hook) in &self.0 {
            if let Some(next) = hook.prepare_step(request) {
                *request = next;
                decided.push((name, Decision::PrepareStep { call }));
            }
        }
    }

    /// Whether a call may run, with `input` as the `pre_tool` chain leaves
    /// it. `Err` is why not, which answers the call.
    pub fn gate(
        &self,
        tool: &str,
        input: &mut Value,
        decided: &mut Decided,
    ) -> Result<Gate, String> {
        for &(name, ref hook) in &self.0 {
            match hook.pre_tool(tool, input) {
                Pre::Pass => {}
                Pre::Replace(next) => {
                    *input = next;
                    let decision = Decision::PreTool {
                        tool: tool.to_owned(),
                        input: input.clone(),
                    };
                    decided.push((name, decision));
                }
                Pre::Block(reason) => {
                    let decision = Decision::Block {
                        tool: tool.to_owned(),
                        reason: reason.clone(),
                    };
                    decided.push((name, decision));
                    return Err(reason);
                }
            }
        }
        let Some((by, permission)) = self
            .0
            .iter()
            .map(|(name, hook)| (*name, hook.permission(tool, input)))
            .filter(|(_, permission)| *permission > Permission::Allow)
            .reduce(|most, next| if next.1 > most.1 { next } else { most })
        else {
            return Ok(Gate::Run);
        };
        let tool = tool.to_owned();
        decided.push((
            by,
            Decision::Permission {
                tool: tool.clone(),
                permission,
            },
        ));
        match permission {
            Permission::Allow => Ok(Gate::Run),
            Permission::Ask => {
                let answer = self
                    .0
                    .iter()
                    .find_map(|(name, hook)| Some((*name, hook.approve(&tool, input)?)));
                let Some((who, approved)) = answer else {
                    return Ok(Gate::Ask);
                };
                decided.push((
                    who,
                    Decision::Approval {
                        tool,
                        approved,
                        call: None,
                    },
                ));
                if approved {
                    Ok(Gate::Run)
                } else {
                    Err(format!("{who} did not approve this call"))
                }
            }
            Permission::Defer => Err(format!("{by} deferred this call")),
            Permission::Deny => Err(format!("{by} denied this call")),
        }
    }

    pub fn post_tool(
        &self,
        call: u64,
        (tool, input): (&str, &Value),
        outcome: &mut Outcome,
        decided: &mut Decided,
    ) {
        for &(name, ref hook) in &self.0 {
            if let Some(next) = hook.post_tool(tool, input, outcome) {
                let original = std::mem::replace(outcome, next);
                decided.push((name, Decision::PostTool { call, original }));
            }
        }
    }

    pub fn on_stop(&self, entries: &[Entry], decided: &mut Decided) -> Option<String> {
        let (hook, text) = self
            .0
            .iter()
            .find_map(|(name, hook)| Some((*name, hook.on_stop(entries)?)))?;
        decided.push((hook, Decision::OnStop { text: text.clone() }));
        Some(text)
    }

    pub fn observe(&self, entry: &Entry) {
        for (_, hook) in &self.0 {
            hook.observe(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A hook that gives fixed answers.
    struct Fixed {
        name: &'static str,
        pre: fn(&Value) -> Pre,
        permission: Permission,
        approve: Option<bool>,
        post: Option<&'static str>,
        stop: Option<&'static str>,
    }

    fn fixed(name: &'static str) -> Fixed {
        Fixed {
            name,
            pre: |_| Pre::Pass,
            permission: Permission::Allow,
            approve: None,
            post: None,
            stop: None,
        }
    }

    impl Hook for Fixed {
        fn pre_tool(&self, _: &str, input: &Value) -> Pre {
            (self.pre)(input)
        }

        fn permission(&self, _: &str, _: &Value) -> Permission {
            self.permission
        }

        fn approve(&self, _: &str, _: &Value) -> Option<bool> {
            self.approve
        }

        fn post_tool(&self, _: &str, _: &Value, _: &Outcome) -> Option<Outcome> {
            self.post.map(|error| Outcome::Failed {
                error: error.to_owned(),
            })
        }

        fn on_stop(&self, _: &[Entry]) -> Option<String> {
            self.stop.map(str::to_owned)
        }
    }

    fn hooks(list: impl IntoIterator<Item = Fixed>) -> Hooks {
        let named = |hook: Fixed| (hook.name, Arc::new(hook) as Arc<dyn Hook>);
        Hooks(list.into_iter().map(named).collect())
    }

    fn gate(hooks: &Hooks) -> (Result<Gate, String>, Value, Decided) {
        let mut input = json!({ "n": 0 });
        let mut decided = Decided::new();
        let verdict = hooks.gate("t", &mut input, &mut decided);
        (verdict, input, decided)
    }

    fn names(decided: &Decided) -> Vec<&'static str> {
        decided.iter().map(|(name, _)| *name).collect()
    }

    #[test]
    fn no_hooks_allow_everything_and_log_nothing() {
        let (verdict, input, decided) = gate(&Hooks::default());
        assert_eq!(verdict, Ok(Gate::Run));
        assert_eq!(input, json!({ "n": 0 }));
        assert_eq!(decided, Decided::new());
    }

    #[test]
    fn pre_tool_replacements_chain_and_the_first_block_wins() {
        let bump = |input: &Value| Pre::Replace(json!({ "n": input["n"].as_i64().unwrap() + 1 }));
        let chained = hooks([
            Fixed {
                pre: bump,
                ..fixed("a")
            },
            Fixed {
                pre: bump,
                ..fixed("b")
            },
        ]);
        let (verdict, input, decided) = gate(&chained);
        assert_eq!((verdict, input), (Ok(Gate::Run), json!({ "n": 2 })));
        assert_eq!(names(&decided), ["a", "b"]);

        let blocked = hooks([
            Fixed {
                pre: bump,
                ..fixed("a")
            },
            Fixed {
                pre: |_| Pre::Block("no".into()),
                ..fixed("b")
            },
            Fixed {
                pre: |_| Pre::Block("never asked".into()),
                ..fixed("c")
            },
        ]);
        let (verdict, _, decided) = gate(&blocked);
        assert_eq!(verdict, Err("no".into()));
        assert_eq!(names(&decided), ["a", "b"]);
    }

    #[test]
    fn the_most_restrictive_permission_wins() {
        use Permission::{Allow, Ask, Defer, Deny};
        let with = |permissions: &[Permission]| {
            let names = ["a", "b", "c"];
            hooks(
                names
                    .into_iter()
                    .zip(permissions)
                    .map(|(name, &permission)| Fixed {
                        permission,
                        ..fixed(name)
                    }),
            )
        };
        let cases = [
            (vec![Allow, Allow], None),
            (vec![Ask, Allow], Some(("a", Ask))),
            (vec![Ask, Defer, Ask], Some(("b", Defer))),
            (vec![Defer, Deny, Ask], Some(("b", Deny))),
            (vec![Deny, Deny], Some(("a", Deny))),
        ];
        for (permissions, want) in cases {
            let (verdict, _, decided) = gate(&with(&permissions));
            let got = decided.first().map(|(name, decision)| match decision {
                Decision::Permission { permission, .. } => (*name, *permission),
                other => panic!("{other:?}"),
            });
            assert_eq!(got, want, "{permissions:?}");
            let runs = verdict.is_ok_and(|gate| gate == Gate::Run);
            assert_eq!(runs, want.is_none(), "{permissions:?}");
        }
    }

    #[test]
    fn an_ask_runs_when_a_hook_approves_and_goes_to_the_user_when_none_answers() {
        let asks = |approve| {
            hooks([
                Fixed {
                    permission: Permission::Ask,
                    ..fixed("policy")
                },
                Fixed {
                    approve,
                    ..fixed("approver")
                },
            ])
        };
        let (approved, _, decided) = gate(&asks(Some(true)));
        assert_eq!(approved, Ok(Gate::Run));
        assert!(matches!(
            &decided[..],
            [
                ("policy", Decision::Permission { .. }),
                ("approver", Decision::Approval { approved: true, .. })
            ]
        ));
        assert!(gate(&asks(Some(false))).0.is_err());
        let (unanswered, _, decided) = gate(&asks(None));
        assert_eq!(unanswered, Ok(Gate::Ask));
        assert_eq!(names(&decided), ["policy"]);
    }

    #[test]
    fn post_tool_chains_and_on_stop_takes_the_first_continuation() {
        let set = hooks([
            Fixed {
                post: Some("first"),
                ..fixed("a")
            },
            fixed("quiet"),
            Fixed {
                post: Some("second"),
                stop: Some("go on"),
                ..fixed("b")
            },
            Fixed {
                stop: Some("never"),
                ..fixed("c")
            },
        ]);
        let mut decided = Decided::new();
        let mut outcome = Outcome::Interrupted { output: None };
        set.post_tool(7, ("t", &json!({})), &mut outcome, &mut decided);
        assert_eq!(
            outcome,
            Outcome::Failed {
                error: "second".into()
            }
        );
        assert!(matches!(
            &decided[..],
            [
                (
                    "a",
                    Decision::PostTool {
                        call: 7,
                        original: Outcome::Interrupted { output: None }
                    }
                ),
                (
                    "b",
                    Decision::PostTool {
                        original: Outcome::Failed { .. },
                        ..
                    }
                ),
            ]
        ));

        decided.clear();
        assert_eq!(set.on_stop(&[], &mut decided).as_deref(), Some("go on"));
        assert_eq!(names(&decided), ["b"]);
    }

    /// Appends its mark to inputs and argv, refuses `no`, and retries
    /// failed requests with its mark as the model.
    struct Marks(&'static str);

    impl Hook for Marks {
        fn input(&self, text: &str) -> Said {
            match text {
                "no" => Said::Reject("not that".into()),
                text => Said::Replace(format!("{text}{}", self.0)),
            }
        }

        fn request_error(&self, _: &llm::Request, _: &str) -> Option<llm::Request> {
            Some(llm::Request {
                model: self.0.into(),
                ..llm::Request::default()
            })
        }

        fn spawn(&self, spawn: &Spawn) -> Option<Spawn> {
            let mut next = spawn.clone();
            next.argv.push(self.0.to_owned());
            Some(next)
        }
    }

    fn marks() -> Hooks {
        Hooks(vec![
            ("a", Arc::new(Marks("1")) as Arc<dyn Hook>),
            ("b", Arc::new(Marks("2"))),
        ])
    }

    #[test]
    fn input_and_spawn_chain_and_the_first_retry_wins() {
        let hooks = marks();
        let mut decided = Decided::new();
        let mut text = "hi".to_owned();
        hooks.input(&mut text, &mut decided).unwrap();
        assert_eq!(text, "hi12");
        assert!(matches!(
            &decided[..],
            [("a", Decision::Input { original: a }), ("b", Decision::Input { original: b })]
                if a == "hi" && b == "hi1"
        ));
        let mut refused = "no".to_owned();
        assert_eq!(
            hooks.input(&mut refused, &mut decided),
            Err("a: not that".into())
        );

        decided.clear();
        let retry = hooks.request_error((4, 1), &llm::Request::default(), "overflow", &mut decided);
        assert_eq!(retry.unwrap().model, "1");
        assert!(matches!(
            &decided[..],
            [("a", Decision::Retry { call: 4, attempt: 1, error })] if error == "overflow"
        ));
        assert!(
            Hooks::default()
                .request_error((4, 1), &llm::Request::default(), "e", &mut decided)
                .is_none()
        );

        let mut spawn = Spawn {
            argv: vec!["bash".into()],
            env: BTreeMap::new(),
            cwd: "/".into(),
        };
        hooks.spawn(&mut spawn);
        assert_eq!(spawn.argv, ["bash", "1", "2"]);
    }
}
