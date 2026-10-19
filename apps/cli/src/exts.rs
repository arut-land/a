//! The extensions `a` links. Each is a crate, and a cargo feature of this
//! one, so leaving one out of a build leaves it out of the registry; each
//! registers through the same verbs as any other, and can be replaced by
//! one of the same name.

// Only the daemon, which runs on Unix alone, assembles them.
#![cfg_attr(not(unix), allow(dead_code))]

use engine::Config;
use ext::host::Registry;
use ext::{DynExtension, Refused, Tier};

/// The extensions this build links, in their install order, each as its
/// `[ext.<name>]` table sets it, with the policy guarded. The fixtures
/// come only with the `echo` agent they serve, in a build that has them.
pub fn extensions(config: &Config) -> Result<Vec<Installed>, Refused> {
    let step_limit = ext::StepLimit {
        max: whole(config, ext::LOOP, "max_steps")?.unwrap_or(ext::StepLimit::default().max),
    };
    #[cfg(feature = "retry")]
    let retry = {
        let default = ext_retry::Retry::default();
        ext_retry::Retry {
            attempts: whole(config, "retry", "attempts")?.unwrap_or(default.attempts),
            transport: whole(config, "retry", "transport")?.unwrap_or(default.transport),
            ..default
        }
    };
    let mut all: Vec<Installed> = vec![
        (Tier::Pre, Box::new(step_limit)),
        #[cfg(feature = "policy")]
        (
            Tier::Guarded,
            Box::new(ext_policy::Policy {
                ask: config.ask.clone(),
            }),
        ),
        #[cfg(feature = "prompt")]
        (Tier::Pre, Box::new(ext_prompt::Prompt)),
        #[cfg(feature = "shell")]
        (Tier::Pre, Box::new(ext_shell::Shell)),
        #[cfg(feature = "edit")]
        (Tier::Pre, Box::new(ext_edit::Edit)),
        #[cfg(feature = "chat")]
        (Tier::Pre, Box::new(ext_chat::Chat)),
        #[cfg(feature = "settings")]
        (Tier::Pre, Box::new(ext_settings::Settings)),
        #[cfg(feature = "subagent")]
        (Tier::Pre, Box::new(ext_subagent::Subagent)),
        // After every other: what another extension owns it is not asked.
        #[cfg(feature = "retry")]
        (Tier::Post, Box::new(retry)),
    ];
    if config.agent.as_deref() == Some(ext_fixtures::ECHO)
        && let Some(fixtures) = ext_fixtures::extension()
    {
        all.push((Tier::User, fixtures));
    }
    Ok(all)
}

/// An extension to install, and where.
pub type Installed = (Tier, Box<dyn DynExtension>);

/// Whole number `key` of extension `ext`'s table, if it sets one.
fn whole<T: TryFrom<u64>>(config: &Config, ext: &str, key: &str) -> Result<Option<T>, Refused> {
    let n = (config.ext.table(ext).whole(key)).map_err(|e| Refused(e.to_string()))?;
    n.map(|n| T::try_from(n).map_err(|_| Refused(format!("ext.{ext}.{key} is too large"))))
        .transpose()
}

/// The registry of [`extensions`].
pub fn registry(config: &Config) -> Result<Registry, Refused> {
    extensions(config)?
        .iter()
        .try_fold(Registry::default(), |r, (tier, ext)| {
            r.with_dyn(*tier, &**ext)
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ext::Gate;
    use ext::host::Decided;
    use ext::point::View;
    use ext::{Facts, PointCx, Replay, States};
    use protocol::{Json, SessionId, Verdict};
    use serde_json::json;

    use super::*;

    /// A session no handler reads.
    struct Bare;

    impl View for Bare {
        fn model(&self) -> String {
            String::new()
        }

        fn states(&self, read: &mut dyn FnMut(&States)) {
            read(&States::default());
        }
    }

    fn config(ask: &[&str], agent: Option<&str>) -> Config {
        Config {
            ask: ask.iter().map(|&t| t.to_owned()).collect(),
            agent: agent.map(str::to_owned),
            ..Config::default()
        }
    }

    fn gate(registry: &Registry, tool: &str, decided: &mut Decided) -> Gate {
        let view = Bare;
        let cx = PointCx::new(SessionId::new(), Path::new("/w"), &view);
        let gated = (registry
            .hooks()
            .gate(&cx, tool, &Json::new(&json!({})))
            .now())
        .unwrap();
        *decided = gated.decisions;
        gated.gate
    }

    fn tools(registry: &Registry) -> Vec<String> {
        let specs = registry.tools().specs();
        specs.into_iter().map(|t| t.name.to_string()).collect()
    }

    #[test]
    fn every_extension_registers_as_any_other() {
        let registry = registry(&config(&["bash"], None)).unwrap();
        assert_eq!(tools(&registry), ["bash", "edit", "task"]);
        let extensions: Vec<_> = registry.extensions().collect();
        assert_eq!(
            extensions,
            [
                ("policy", 1),
                ("loop", 1),
                ("prompt", 1),
                ("shell", 1),
                ("edit", 1),
                ("chat", 1),
                ("settings", 1),
                ("subagent", 1),
                ("retry", 1)
            ]
        );
        assert_eq!(
            registry.agents().collect::<Vec<_>>(),
            [ext::LOOP, ext_chat::CHAT]
        );

        let mut decided = Decided::new();
        assert_eq!(gate(&registry, "bash", &mut decided), Gate::Ask);
        assert!(
            matches!(&decided[..], [(name, Verdict::Permission { .. })] if &**name == "policy")
        );

        let prompt = registry.system_prompt();
        assert!(prompt.starts_with("You are `a`"), "{prompt}");
        assert!(
            prompt.contains("`bash`") && prompt.contains("`edit`"),
            "{prompt}"
        );
        assert!(prompt.len() < 2000, "{} bytes", prompt.len());
    }

    #[test]
    fn removing_an_extension_crate_removes_its_tool_and_nothing_else() {
        let config = config(&["bash"], None);
        let all = registry(&config).unwrap();
        let paragraphs = |r: &Registry| -> Vec<String> {
            r.system_prompt().split("\n\n").map(str::to_owned).collect()
        };
        // What each contributes that the others do not.
        let own: [(&str, &[&str], usize, bool); 9] = [
            ("policy", &[], 0, false),
            ("loop", &[], 0, false),
            ("prompt", &[], 1, false),
            ("shell", &["bash"], 1, false),
            ("edit", &["edit"], 1, false),
            ("chat", &[], 0, true),
            ("retry", &[], 0, false),
            ("settings", &[], 0, false),
            ("subagent", &["task"], 1, false),
        ];
        for (name, tools_of, texts_of, agent_of) in own {
            let rest = extensions(&config)
                .unwrap()
                .into_iter()
                .filter(|(_, ext)| *ext.name() != *name)
                .try_fold(Registry::default(), |r, (tier, ext)| {
                    r.with_dyn(tier, &*ext)
                });
            if name == "shell" {
                // `edit` works where commands run: without the shell it is
                // refused, not left to guess.
                let refused = rest.err().unwrap();
                assert!(
                    refused
                        .0
                        .starts_with("extension edit: it needs dyn ext::service::CwdOf"),
                    "{refused}"
                );
                continue;
            }
            let rest = rest.unwrap();
            let mut left: Vec<_> = all.extensions().map(|(n, _)| n).collect();
            left.retain(|n| *n != name);
            assert_eq!(rest.extensions().map(|(n, _)| n).collect::<Vec<_>>(), left);
            let mut kept = tools(&all);
            kept.retain(|t| !tools_of.contains(&t.as_str()));
            assert_eq!(tools(&rest), kept, "{name}");
            let (before, after) = (paragraphs(&all), paragraphs(&rest));
            assert!(after.iter().all(|p| before.contains(p)), "{name}");
            assert_eq!(before.len() - after.len(), texts_of, "{name}");
            // The default loop is always there.
            assert_eq!(rest.agents().count(), 1 + usize::from(!agent_of), "{name}");
            let facts = Facts {
                workspace: Path::new("/w"),
                dir: Path::new("/w"),
            };
            assert_eq!(rest.context(&facts).is_some(), name != "prompt", "{name}");
            // Only the policy asks.
            let asks = gate(&rest, "bash", &mut Decided::new()) == Gate::Ask;
            assert_eq!(asks, name != "policy", "{name}");
        }
    }

    #[test]
    fn replay_policy_is_per_tool() {
        let registry = registry(&config(&[], Some(ext_fixtures::ECHO))).unwrap();
        let tools = registry.tools();
        assert_eq!(tools.get("bash").unwrap().replay(), Replay::Unsafe);
        assert_eq!(tools.get("edit").unwrap().replay(), Replay::Unsafe);
        assert_eq!(tools.get("sleep").unwrap().replay(), Replay::Safe);
        // Only the echo agent brings the fixtures.
        let chat = super::registry(&config(&[], None)).unwrap();
        assert!(chat.tools().get("sleep").is_none());
        assert_eq!(
            chat.agents().collect::<Vec<_>>(),
            [ext::LOOP, ext_chat::CHAT]
        );
    }

    /// Every kind of model failure there is.
    const KINDS: [llm::Kind; 12] = {
        use llm::Kind::*;
        [
            Transport,
            Auth,
            RateLimit,
            Quota,
            ContextOverflow,
            InvalidRequest,
            ContentFilter,
            Server,
            Decode,
            Incomplete,
            Unsupported,
            Other,
        ]
    };

    /// A kind added to `llm` is a kind this table must place: the match
    /// is exhaustive.
    const fn listed(kind: llm::Kind) -> bool {
        use llm::Kind::*;
        match kind {
            Transport | Auth | RateLimit | Quota | ContextOverflow | InvalidRequest
            | ContentFilter | Server | Decode | Incomplete | Unsupported | Other => true,
        }
    }

    /// Who answers a failure of `kind`, in `registry`.
    fn answers(registry: &Registry, kind: llm::Kind) -> Option<String> {
        let view = Bare;
        let cx = PointCx::new(SessionId::new(), Path::new("/w"), &view).about(2);
        let error = llm::Error::new(kind, "x");
        let failed = ext::point::Failed {
            error: &error,
            attempt: 0,
        };
        match registry.hooks().request_error(&cx, failed) {
            ext::Asked::None => None,
            ext::Asked::Now(by, _) | ext::Asked::Later { by, .. } => Some(by.to_string()),
        }
    }

    /// Each kind of model failure is owned by at most one of the default
    /// extensions: none asks about another's. So which answers does not
    /// depend on their order.
    #[test]
    fn request_error_kinds_partition() {
        let config = config(&[], None);
        let defaults = extensions(&config).unwrap();
        for kind in KINDS {
            assert!(listed(kind));
            let owners: Vec<String> = defaults
                .iter()
                .filter_map(|(tier, ext)| {
                    let alone = Registry::default().with_dyn(*tier, &**ext).ok()?;
                    answers(&alone, kind)
                })
                .collect();
            assert!(owners.len() <= 1, "{kind:?} is owned by {owners:?}");
            let owner = owners.first().map(String::as_str);
            let want = kind.is_retryable().then_some("retry");
            assert_eq!(owner, want, "{kind:?}");
        }
    }

    /// Falls back to another model on a rate limit: a user's handler for a
    /// kind retry owns too.
    struct Fallback;

    impl ext::Extension for Fallback {
        const NAME: &'static str = "fallback";
        const VERSION: u32 = 1;

        fn contribute(&self, r: &mut ext::Registrar) {
            r.on::<ext::point::RequestError>(|_, failed| {
                let mine = failed.error.kind == llm::Kind::RateLimit;
                ext::Answer::Now(mine.then_some(ext::Recover::Resend))
            });
        }
    }

    /// Retry installs after every other extension: a handler of the user's
    /// for a kind it would retry is asked first, whatever the install order.
    #[test]
    fn retry_answers_last_however_it_is_installed() {
        let retry_first = Registry::default()
            .with_in(Tier::Post, &ext_retry::Retry::default())
            .and_then(|r| r.with(&Fallback))
            .unwrap();
        let retry_last = Registry::default()
            .with(&Fallback)
            .and_then(|r| r.with_in(Tier::Post, &ext_retry::Retry::default()))
            .unwrap();
        for registry in [retry_first, retry_last] {
            let rate = answers(&registry, llm::Kind::RateLimit);
            assert_eq!(rate.as_deref(), Some("fallback"));
            let server = answers(&registry, llm::Kind::Server);
            assert_eq!(server.as_deref(), Some("retry"));
        }
    }

    #[test]
    fn schemas_describe_objects_that_name_their_required_inputs() {
        let registry = registry(&config(&[], Some(ext_fixtures::ECHO))).unwrap();
        for spec in registry.tools().specs() {
            let schema = &spec.schema;
            assert_eq!(schema["type"], "object", "{}", spec.name);
            assert_ne!(spec.description, "");
            for required in schema["required"].as_array().unwrap() {
                let name = required.as_str().unwrap();
                assert!(
                    schema["properties"][name].is_object(),
                    "{}: {name}",
                    spec.name
                );
            }
        }
    }
}
