//! Install order satisfies extension dependencies at compile time through
//! [`Built`]. Runtime configuration may replace registrations, so the same
//! dependency checks also apply to disabled and dynamically loaded extensions.

use std::sync::Arc;

use engine::Config;
use ext::host::{Built, Coordinator, GenerationLayer, Registry};
use ext::{Configured, DynExtension, Name, Refused, Registrar, Table, Tier};

/// Tiering and dependency validation apply equally to linked and source registrations.
pub fn registry(config: &Config) -> Result<Registry, Refused> {
    let home = sys::Home::from_env().map_err(|e| Refused(e.to_string()))?;
    registry_at(config, &home)
}

fn registry_at(
    config: &Config,
    #[allow(unused_variables)] home: &sys::Home,
) -> Result<Registry, Refused> {
    let registry = linked(config)?;
    let mut layers: Vec<Arc<dyn GenerationLayer>> = Vec::new();
    #[cfg(feature = "lua")]
    if !config.ext.disable.iter().any(|name| name == "lua") {
        layers.push(Arc::new(ext_lua::Loader::new(
            home.root(),
            config.ext.clone(),
        )?));
    }
    #[cfg(feature = "skills")]
    if !config.ext.disable.iter().any(|name| name == "skills") {
        let skills =
            configured::<ext_skills::Skills>(config)?.reserving(client::COMMANDS.iter().flat_map(
                |command| std::iter::once(command.name).chain(command.aliases.iter().copied()),
            ));
        layers.push(Arc::new(ext_skills::Loader::new(home.root(), skills)));
    }
    let coordinator = Arc::new(Coordinator::new(registry.clone(), layers));
    registry.with_in(Tier::Pre, &coordinator)
}

fn linked(config: &Config) -> Result<Registry, Refused> {
    let built = Built::new().with(Tier::Pre, &configured::<ext::StepLimit>(config)?)?;
    #[cfg(feature = "policy")]
    let built = {
        let floor = config.floor.clone().map_err(Refused)?;
        let floor = ext_policy::Floor::new(floor).map_err(Refused)?;
        built
            .with(Tier::Guarded, &floor)?
            .with(Tier::Pre, &configured::<ext_policy::Policy>(config)?)?
    };
    #[cfg(feature = "fork")]
    let built = built.with(Tier::Pre, &configured::<ext_fork::Fork>(config)?)?;
    #[cfg(feature = "prompt")]
    let built = built.with(Tier::Pre, &configured::<ext_prompt::Prompt>(config)?)?;
    #[cfg(feature = "shell")]
    let built = built.with(Tier::Pre, &configured::<ext_shell::Shell>(config)?)?;
    // Needs the shell: without it, this does not compile.
    #[cfg(feature = "edit")]
    let built = built.with(Tier::Pre, &configured::<ext_edit::Edit>(config)?)?;
    #[cfg(feature = "mcp")]
    let built = built.with(Tier::Pre, &configured::<ext_mcp::Mcp>(config)?)?;
    #[cfg(feature = "compact")]
    let built = built.with(Tier::Pre, &configured::<ext_compact::Compact>(config)?)?;
    #[cfg(feature = "skills")]
    let built = built.with(Tier::Pre, &configured::<ext_skills::Skills>(config)?)?;
    #[cfg(feature = "settings")]
    let built = built.with(Tier::Pre, &configured::<ext_settings::Settings>(config)?)?;
    #[cfg(feature = "subagent")]
    let built = built.with(Tier::Pre, &configured::<ext_subagent::Subagent>(config)?)?;
    // After every other: what another extension owns it is not asked.
    #[cfg(feature = "retry")]
    let built = built.with(Tier::Post, &configured::<ext_retry::Retry>(config)?)?;
    #[cfg(feature = "title")]
    let built = built.with(Tier::Post, &configured::<ext_title::Title>(config)?)?;
    let mut built = built.with(Tier::Post, &ext_metrics::Metrics)?;
    if config.agent.as_deref() == Some(ext_fixtures::ECHO)
        && let Some(fixtures) = ext_fixtures::extension()
    {
        built = built.with_dyn(Tier::User, &*fixtures)?;
    }
    // Disabled: replaced by nothing of the same name.
    for name in &config.ext.disable {
        built = built.with_dyn(Tier::User, &Disabled(Name::from(name.as_str())))?;
    }
    Ok(built.registry())
}

/// Extension `E`, as its `[ext.<name>]` table in `config` sets it.
fn configured<E: Configured>(config: &Config) -> Result<E, Refused> {
    E::configure(&Settings(config.ext.table(E::NAME)))
}

/// An extension's table of the config file, as the extension reads it.
struct Settings(sys::config::Table);

impl Settings {
    fn read<T>(read: Result<T, sys::config::ParseError>) -> Result<T, Refused> {
        read.map_err(|e| Refused(e.to_string()))
    }
}

impl Table for Settings {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn text(&self, key: &str) -> Result<Option<&str>, Refused> {
        Self::read(self.0.text(key))
    }

    fn whole(&self, key: &str) -> Result<Option<u64>, Refused> {
        Self::read(self.0.whole(key))
    }

    fn flag(&self, key: &str) -> Result<Option<bool>, Refused> {
        Self::read(self.0.flag(key))
    }

    fn list(&self, key: &str) -> Result<Option<&[String]>, Refused> {
        Self::read(self.0.list(key))
    }
}

/// What a disabled extension is replaced by: one of its name that
/// contributes nothing.
struct Disabled(Name);

impl DynExtension for Disabled {
    fn name(&self) -> Name {
        Name::clone(&self.0)
    }

    fn version(&self) -> u32 {
        0
    }

    fn contribute(&self, _: &mut Registrar) {}
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

    fn isolated_registry(config: &Config) -> Result<Registry, Refused> {
        let rig = ext::testing::Rig::new(&SessionId::new().to_string(), Registry::default());
        registry_at(config, &sys::Home::from(rig.dir.join("home")))
    }

    /// A session no handler reads.
    struct Bare;

    impl View for Bare {
        fn session(&self) -> SessionId {
            SessionId::new()
        }

        fn workspace(&self) -> &Path {
            Path::new("/w")
        }

        fn model(&self) -> String {
            String::new()
        }

        fn states(&self, read: &mut dyn FnMut(&States)) {
            read(&States::default());
        }
    }

    /// A daemon's settings that ask about `ask`, with `extra` in its file.
    fn configured(ask: &[&str], agent: Option<&str>, extra: &str) -> Config {
        let ask: Vec<_> = ask.iter().map(|t| format!("{t:?}")).collect();
        let text = format!("[ext.policy]\nask = [{}]\n{extra}", ask.join(", "));
        Config {
            agent: agent.map(str::to_owned),
            ext: text.parse::<sys::Config>().unwrap().ext,
            ..Config::default()
        }
    }

    fn config(ask: &[&str], agent: Option<&str>) -> Config {
        configured(ask, agent, "")
    }

    fn gate(registry: &Registry, tool: &str, decided: &mut Decided) -> Gate {
        let view = Bare;
        let cx = PointCx::new(&view);
        let gated = (registry
            .hooks()
            .gate(&cx, tool, &Json::encode(&json!({})).unwrap())
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
        let registry = isolated_registry(&config(&["bash"], None)).unwrap();
        assert_eq!(
            tools(&registry),
            [
                "bash",
                "edit",
                "skill",
                "task",
                "fork_task",
                "background_task"
            ]
        );
        let extensions: Vec<_> = registry
            .extensions()
            .filter(|(name, _)| *name != "generation-source")
            .collect();
        assert_eq!(
            extensions,
            [
                ("policy-floor", 2),
                ("loop", 1),
                ("policy", 3),
                ("fork", 1),
                ("prompt", 1),
                ("shell", 1),
                ("edit", 3),
                #[cfg(feature = "mcp")]
                ("mcp", 1),
                ("compact", 1),
                ("skills", 1),
                ("settings", 1),
                ("subagent", 1),
                ("retry", 1),
                ("title", 3),
                ("metrics", 1)
            ]
        );
        assert_eq!(
            registry.agents().collect::<Vec<_>>(),
            [ext::LOOP, "reviewer"]
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

    /// Leaving an extension out by name (`[ext] disable`) leaves out its
    /// tool and text and nothing else; one another needs is refused at
    /// startup, naming both. (Leaving its crate out of the build is the
    /// static case: `edit` without `shell` does not compile.)
    #[test]
    fn removing_an_extension_removes_its_tool_and_nothing_else() {
        let config = config(&["bash"], None);
        let all = isolated_registry(&config).unwrap();
        let paragraphs = |r: &Registry| -> Vec<String> {
            r.system_prompt().split("\n\n").map(str::to_owned).collect()
        };
        // What each contributes that the others do not.
        let own: [(&str, &[&str], usize); 12] = [
            ("title", &[], 0),
            ("compact", &[], 0),
            ("policy-floor", &[], 0),
            ("policy", &[], 0),
            ("loop", &[], 0),
            ("prompt", &[], 1),
            ("shell", &["bash"], 1),
            ("edit", &["edit"], 1),
            ("retry", &[], 0),
            ("settings", &[], 0),
            ("skills", &["skill"], 0),
            ("subagent", &["task", "fork_task", "background_task"], 1),
        ];
        for (name, tools_of, texts_of) in own {
            let without = Config {
                ext: sys::config::Exts {
                    disable: vec![name.to_owned()],
                    ..config.ext.clone()
                },
                ..config.clone()
            };
            let rest = isolated_registry(&without);
            if name == "policy-floor" {
                // The operator's floor is guarded: nothing of the user's
                // takes it away.
                assert!(rest.err().unwrap().0.contains("guarded"));
                continue;
            }
            if name == "shell" {
                // `edit` works where commands run: without the shell it is
                // refused, not left to guess.
                let refused = rest.err().unwrap();
                assert!(
                    refused
                        .0
                        .starts_with("extension shell: edit needs dyn ext::service::CwdOf"),
                    "{refused}"
                );
                continue;
            }
            let rest = rest.unwrap();
            // Replaced by nothing of its name: listed, contributing nothing.
            let listed: Vec<_> = rest
                .extensions()
                .map(|(n, _)| n)
                .filter(|n| *n != "skills-source")
                .collect();
            assert_eq!(
                listed,
                all.extensions()
                    .map(|(n, _)| n)
                    .filter(|n| *n != "skills-source")
                    .collect::<Vec<_>>()
            );
            assert_eq!(rest.extensions().find(|(n, _)| *n == name).unwrap().1, 0);
            let mut kept = tools(&all);
            kept.retain(|t| !tools_of.contains(&t.as_str()));
            assert_eq!(tools(&rest), kept, "{name}");
            let (before, after) = (paragraphs(&all), paragraphs(&rest));
            assert!(after.iter().all(|p| before.contains(p)), "{name}");
            assert_eq!(before.len() - after.len(), texts_of, "{name}");
            // The default loop is always there.
            assert_eq!(
                rest.agents().count(),
                if name == "subagent" { 1 } else { 2 },
                "{name}"
            );
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

    fn registry_refused(config: &Config) -> String {
        match isolated_registry(config) {
            Ok(_) => panic!("installed"),
            Err(refused) => refused.0,
        }
    }

    /// Each extension's settings are its table's, read by the extension
    /// (this file names no key), and the daemon's own table holds none of
    /// them.
    #[test]
    fn extensions_are_configured_by_their_tables() {
        let config = configured(
            &["bash"],
            None,
            "mode = \"auto\"\n[ext.prompt]\nsystem = \"Be terse.\"\n[ext.loop]\nmax_steps = 3\n[ext.retry]\nattempts = 2\n",
        );
        let registry = isolated_registry(&config).unwrap();
        assert!(registry.system_prompt().starts_with("Be terse."));
        assert_eq!(gate(&registry, "bash", &mut Decided::new()), Gate::Run);
        let bad = configured(&[], None, "mode = \"sometimes\"\n");
        assert_eq!(registry_refused(&bad), "ext.policy.mode: no mode sometimes");
        let wrong = configured(&[], None, "[ext.retry]\nattempts = \"many\"\n");
        assert_eq!(
            registry_refused(&wrong),
            "ext.retry.attempts must be a whole number"
        );
        let large = configured(&[], None, "[ext.loop]\nmax_steps = 70000\n");
        assert_eq!(registry_refused(&large), "ext.loop.max_steps is too large");
        let floor = Config {
            floor: Ok(vec![sys::config::Rule {
                tool: "bash".into(),
                input: None,
                effect: sys::config::Effect::Allow,
            }]),
            ..config.clone()
        };
        assert!(registry_refused(&floor).contains("may only ask or deny"));
    }

    #[test]
    fn replay_policy_is_per_tool() {
        let registry = isolated_registry(&config(&[], Some(ext_fixtures::ECHO))).unwrap();
        let tools = registry.tools();
        assert_eq!(tools.get("bash").unwrap().replay(), Replay::Unsafe);
        assert_eq!(tools.get("edit").unwrap().replay(), Replay::Unsafe);
        assert_eq!(tools.get("sleep").unwrap().replay(), Replay::Safe);
        // Only the echo agent brings the fixtures.
        let shipped = isolated_registry(&config(&[], None)).unwrap();
        assert!(shipped.tools().get("sleep").is_none());
        assert_eq!(
            shipped.agents().collect::<Vec<_>>(),
            [ext::LOOP, "reviewer"]
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
        let cx = PointCx::new(&view).about(protocol::CallId::new(2).unwrap());
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
        let all = isolated_registry(&config).unwrap();
        let names: Vec<String> = all.extensions().map(|(n, _)| n.to_owned()).collect();
        for kind in KINDS {
            assert!(listed(kind));
            // Each extension alone: the others disabled, but the guarded
            // floor and the shell `edit` needs, which answer no failure.
            let owners: Vec<String> = names
                .iter()
                .filter_map(|name| {
                    let others = (names.iter())
                        .filter(|n| *n != name && *n != "policy-floor" && *n != "shell")
                        .cloned()
                        .collect();
                    let alone = Config {
                        ext: sys::config::Exts {
                            disable: others,
                            ..config.ext.clone()
                        },
                        ..config.clone()
                    };
                    let alone = isolated_registry(&alone).ok()?;
                    answers(&alone, kind)
                })
                .collect();
            assert!(owners.len() <= 1, "{kind:?} is owned by {owners:?}");
            let owner = owners.first().map(String::as_str);
            let want = match kind {
                llm::Kind::ContextOverflow => Some("compact"),
                kind => kind.is_retryable().then_some("retry"),
            };
            assert_eq!(owner, want, "{kind:?}");
        }
    }

    /// Falls back to another model on a rate limit: a user's handler for a
    /// kind retry owns too.
    struct Fallback;

    impl ext::Extension for Fallback {
        const NAME: &'static str = "fallback";
        const VERSION: u32 = 1;

        fn contribute(&self, r: &mut Registrar) {
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
        let registry = isolated_registry(&config(&[], Some(ext_fixtures::ECHO))).unwrap();
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
