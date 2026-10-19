//! The extensions `a` links. Each is a crate, and a cargo feature of this
//! one, so leaving one out of a build leaves it out of the registry; each
//! registers through the same verbs as any other, and can be replaced by
//! one of the same name.

// Only the daemon, which runs on Unix alone, assembles them.
#![cfg_attr(not(unix), allow(dead_code))]

use engine::Config;
use ext::host::Registry;
use ext::{DynExtension, Refused, Tier};

/// The extensions this build links, in their install order, with the
/// policy as `config` sets it and guarded. The fixtures come only with the
/// `echo` agent they serve, in a build that has them.
pub fn extensions(config: &Config) -> Vec<(Tier, Box<dyn DynExtension>)> {
    let mut all: Vec<(Tier, Box<dyn DynExtension>)> = vec![
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
        #[cfg(feature = "retry")]
        (Tier::Pre, Box::new(ext_retry::Retry::default())),
    ];
    if config.agent.as_deref() == Some(ext_fixtures::ECHO)
        && let Some(fixtures) = ext_fixtures::extension()
    {
        all.push((Tier::User, fixtures));
    }
    all
}

/// The registry of [`extensions`].
pub fn registry(config: &Config) -> Result<Registry, Refused> {
    extensions(config)
        .iter()
        .try_fold(Registry::default(), |r, (tier, ext)| {
            r.with_dyn(*tier, &**ext)
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ext::host::{Decided, Gate};
    use ext::point::View;
    use ext::{Facts, PointCx, Replay, States};
    use protocol::{Decision, SessionId};
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

    fn gate(registry: &Registry, tool: &str, decided: &mut Decided) -> Result<Gate, String> {
        let view = Bare;
        let cx = PointCx::new(SessionId::new(), Path::new("/w"), &view);
        registry.hooks().gate(&cx, tool, &mut json!({}), decided)
    }

    fn tools(registry: &Registry) -> Vec<String> {
        let specs = registry.tools().specs();
        specs.into_iter().map(|t| t.name.to_string()).collect()
    }

    #[test]
    fn every_extension_registers_as_any_other() {
        let registry = registry(&config(&["bash"], None)).unwrap();
        assert_eq!(tools(&registry), ["bash", "edit"]);
        let extensions: Vec<_> = registry.extensions().collect();
        assert_eq!(
            extensions,
            [
                ("policy", 1),
                ("prompt", 1),
                ("shell", 1),
                ("edit", 1),
                ("chat", 1),
                ("retry", 1)
            ]
        );
        assert_eq!(registry.agents().collect::<Vec<_>>(), [ext_chat::CHAT]);

        let mut decided = Decided::new();
        assert_eq!(gate(&registry, "bash", &mut decided), Ok(Gate::Ask));
        assert!(
            matches!(&decided[..], [(name, Decision::Permission { .. })] if &**name == "policy")
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
        let own: [(&str, &[&str], usize, bool); 6] = [
            ("policy", &[], 0, false),
            ("prompt", &[], 1, false),
            ("shell", &["bash"], 1, false),
            ("edit", &["edit"], 1, false),
            ("chat", &[], 0, true),
            ("retry", &[], 0, false),
        ];
        for (name, tools_of, texts_of, agent_of) in own {
            let rest = extensions(&config)
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
            assert_eq!(rest.agents().count(), usize::from(!agent_of), "{name}");
            let facts = Facts {
                workspace: Path::new("/w"),
                dir: Path::new("/w"),
            };
            assert_eq!(rest.context(&facts).is_some(), name != "prompt", "{name}");
            // Only the policy asks.
            let asks = gate(&rest, "bash", &mut Decided::new()) == Ok(Gate::Ask);
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
        assert_eq!(chat.agents().collect::<Vec<_>>(), [ext_chat::CHAT]);
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
