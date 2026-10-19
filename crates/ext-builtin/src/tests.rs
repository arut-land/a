use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use ext::PointCx;
use ext::host::{Decided, Gate};
use ext::point::View;
use protocol::{Decision, SessionId};
use serde_json::json;

use super::*;
use crate::state::Cwd;
use crate::testing::Rig;

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

fn gate(registry: &Registry, tool: &str, decided: &mut Decided) -> Result<Gate, String> {
    let cx = PointCx::new(SessionId::new(), Path::new("/w"), &Bare);
    registry.hooks().gate(&cx, tool, &mut json!({}), decided)
}

fn tools(registry: &Registry) -> Vec<String> {
    let specs = registry.tools().specs();
    specs.into_iter().map(|t| t.name.to_string()).collect()
}

#[test]
fn built_ins_register_as_extensions() {
    let registry = builtins(&Policy::ask(&["bash"])).unwrap();
    assert_eq!(tools(&registry), ["bash", "edit"]);
    let extensions: Vec<_> = registry.extensions().collect();
    assert_eq!(
        extensions,
        [
            ("policy", 1),
            ("prompt", 1),
            ("shell", 1),
            ("edit", 1),
            ("chat", 1)
        ]
    );
    assert_eq!(registry.agents().collect::<Vec<_>>(), [agents::CHAT]);

    let mut decided = Decided::new();
    assert_eq!(gate(&registry, "bash", &mut decided), Ok(Gate::Ask));
    assert!(matches!(&decided[..], [(name, Decision::Permission { .. })] if &**name == "policy"));

    let prompt = registry.system_prompt();
    assert!(prompt.starts_with("You are `a`"), "{prompt}");
    assert!(
        prompt.contains("`bash`") && prompt.contains("`edit`"),
        "{prompt}"
    );
    assert!(prompt.len() < 2000, "{} bytes", prompt.len());
}

#[test]
fn removing_a_built_in_extension_removes_its_tool_and_nothing_else() {
    let policy = Policy::ask(&["bash"]);
    let all = builtins(&policy).unwrap();
    let paragraphs = |r: &Registry| -> Vec<String> {
        r.system_prompt().split("\n\n").map(str::to_owned).collect()
    };
    // What each contributes that the others do not.
    let own: [(&str, &[&str], usize, bool); 5] = [
        ("policy", &[], 0, false),
        ("prompt", &[], 1, false),
        ("shell", &["bash"], 1, false),
        ("edit", &["edit"], 1, false),
        ("chat", &[], 0, true),
    ];
    for (name, tools_of, texts_of, agent_of) in own {
        let rest = extensions(&policy)
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
    let registry = registry(&Policy::default(), Some(agents::ECHO)).unwrap();
    let tools = registry.tools();
    assert_eq!(tools.get("bash").unwrap().replay(), Replay::Unsafe);
    assert_eq!(tools.get("edit").unwrap().replay(), Replay::Unsafe);
    assert_eq!(tools.get("sleep").unwrap().replay(), Replay::Safe);
    // Only the echo agent brings the fixtures.
    let chat = super::registry(&Policy::default(), None).unwrap();
    assert!(chat.tools().get("sleep").is_none());
    assert_eq!(chat.agents().collect::<Vec<_>>(), [agents::CHAT]);
}

#[test]
fn an_extension_cannot_hold_session_state_across_an_await() {
    let rig = Rig::new("hold", builtins(&Policy::default()).unwrap());
    let (reader, writer) = (rig.cx(), rig.cx());
    let ws = rig.workspace();
    let sub = ws.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let (paused, resume) = (mpsc::channel(), mpsc::channel::<()>());
    let read = std::thread::spawn(move || {
        rt::block_on(async {
            // What a read yields is owned: no lock outlives the closure.
            let before = reader.state(|cwd: &Cwd| cwd.after(None).map(Path::to_path_buf));
            paused.0.send(()).unwrap();
            let rt = Arc::clone(&reader.rt);
            while resume.1.try_recv().is_err() {
                rt.sleep(Duration::from_millis(5)).await;
            }
            let after = reader.state(|cwd: &Cwd| cwd.after(None).map(Path::to_path_buf));
            (before, after)
        })
    });
    paused.1.recv().unwrap();
    // A change commits while the reader is suspended at an await.
    writer.update::<Cwd>(Cwd::change(&sub, None));
    resume.0.send(()).unwrap();
    let (before, after) = read.join().unwrap();
    assert_eq!((before, after), (None, Some(sub)));
}
