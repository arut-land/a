//! Skills: immutable workspace generations, with only an index offered
//! until a typed tool or user command publishes instructions to the log.

mod loader;
mod parse;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use ext::{Configured, Cx, Extension, Publication, Refused, Registrar, Schema, Table, Tool};
use serde::{Deserialize, Serialize};

pub use loader::Loader;

/// Own roots always win over these explicit additional roots. Relative
/// paths are workspace-relative; absolute paths are used as written.
#[derive(Clone, Debug, Default)]
pub struct Skills {
    pub dirs: Vec<PathBuf>,
    reserved: Vec<String>,
    catalog: Arc<BTreeMap<String, Skill>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Skill {
    description: String,
    directory: String,
    body: String,
}

impl Configured for Skills {
    fn configure(table: &impl Table) -> Result<Self, Refused> {
        Ok(Self {
            dirs: table
                .list("dirs")?
                .unwrap_or_default()
                .iter()
                .map(PathBuf::from)
                .collect(),
            ..Self::default()
        })
    }
}

impl Extension for Skills {
    const NAME: &'static str = "skills";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Load(self.catalog.clone()));
        for (name, skill) in &*self.catalog {
            r.dyn_tool(Arc::new(Command {
                name: name.clone(),
                description: skill.description.clone(),
                load: Load(self.catalog.clone()),
            }));
        }
        r.text(ext::Text::generation("skills-index", 50, self.index()));
    }
}

/// The installed overlay carries exactly the immutable catalog it contributes.
struct Catalog(Skills);
impl ext::DynExtension for Catalog {
    fn name(&self) -> ext::Name {
        ext::Name::from(<Skills as Extension>::NAME)
    }
    fn version(&self) -> u32 {
        <Skills as Extension>::VERSION
    }
    fn image(&self) -> Option<protocol::Json> {
        Some(protocol::Json::encode(&*self.0.catalog).expect("a catalog contains only strings"))
    }
    fn contribute(&self, r: &mut Registrar) {
        Extension::contribute(&self.0, r);
    }
}

impl Skills {
    /// Client-owned commands are intercepted before input reaches the daemon.
    #[must_use]
    pub fn reserving(mut self, names: impl IntoIterator<Item = &'static str>) -> Self {
        self.reserved.extend(names.into_iter().map(str::to_owned));
        self
    }

    fn index(&self) -> String {
        let mut index = String::from(
            "Current available skills (supersedes earlier indices; instructions load only on request). Call skill with {name}, or use /name:\n",
        );
        for (name, skill) in &*self.catalog {
            use std::fmt::Write as _;
            let _ = writeln!(index, "{name}: {}", skill.description);
        }
        index
    }
}

struct Instructions;
impl Publication for Instructions {
    const NAME: &'static str = "skills";
    type Key = (String, String);
}

/// The skill to load, exactly as named by the index.
#[derive(Deserialize, Schema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: String,
}

/// A failed lookup is data the model can distinguish from a failed tool.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadError {
    UnknownSkill { name: String },
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadedSkill {
    Loaded,
    AlreadyLoaded,
}

#[derive(Clone)]
struct Load(Arc<BTreeMap<String, Skill>>);

impl Load {
    async fn load(&self, name: &str, args: &str, cx: &Cx) -> Result<LoadedSkill, LoadError> {
        let skill = self
            .0
            .get(name)
            .ok_or_else(|| LoadError::UnknownSkill { name: name.into() })?;
        let text = format!(
            "Skill {name}\nBase directory: {}\n\n{}",
            skill.directory, skill.body
        );
        let key = (name.to_owned(), text.clone());
        let loaded = cx.published::<Instructions>(&key);
        if !loaded || !args.is_empty() {
            let mut context = if loaded { String::new() } else { text.clone() };
            if !args.is_empty() {
                use std::fmt::Write as _;
                let _ = write!(context, "\nUse skill {name} with these arguments:\n{args}");
            }
            cx.publish::<Instructions>(&key, context).await;
        }
        Ok(if loaded {
            LoadedSkill::AlreadyLoaded
        } else {
            LoadedSkill::Loaded
        })
    }
}

impl Tool for Load {
    type In = Input;
    type Out = Result<LoadedSkill, LoadError>;
    const NAME: &'static str = "skill";
    const DESCRIPTION: &'static str =
        "Load a named skill's instructions into the conversation. Consult the skill index first.";
    // A crash must never turn a recorded request into a read of newer files.
    const REPLAY: ext::Replay = ext::Replay::Unsafe;

    async fn call(&self, input: &Input, cx: &Cx) -> Result<Self::Out, String> {
        Ok(self.load(&input.name, "", cx).await)
    }
}

struct Command {
    name: String,
    description: String,
    load: Load,
}

impl ext::DynTool for Command {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn audience(&self) -> protocol::Audience {
        protocol::Audience::User
    }
    fn replay(&self) -> ext::Replay {
        ext::Replay::Unsafe
    }
    fn concurrency(&self) -> ext::Concurrency {
        ext::Concurrency::Exclusive
    }
    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: self.name.clone().into(),
            description: self.description.clone(),
            schema: serde_schema(),
            strict: Some(true),
        }
    }
    fn parse(&self, input: &ext::Json) -> Result<ext::Parsed, String> {
        input
            .parse::<ext::Args>()
            .map(ext::Parsed::new)
            .map_err(|e| e.to_string())
    }
    fn call<'a>(&'a self, input: ext::Parsed, cx: &'a Cx) -> ext::BoxFuture<'a, ext::Called> {
        Box::pin(async move {
            let Some(args) = input.get::<ext::Args>() else {
                return ext::Called::failed("invalid command arguments".into());
            };
            match ext::Json::encode(&self.load.load(&self.name, &args.args, cx).await) {
                Ok(output) => ext::Called {
                    outcome: protocol::Outcome::Done { output },
                    card: None,
                },
                Err(e) => ext::Called::failed(e.to_string()),
            }
        })
    }
    fn to_model(&self, output: &ext::Json) -> llm::Output {
        llm::Output::Json(output.clone())
    }
    fn present(&self, _: &ext::Parsed) -> Option<protocol::Card> {
        None
    }
}

fn serde_schema() -> serde_json::Value {
    // User commands are never offered as model tools, but share their ABI.
    serde_json::from_str(&ext::schema::of::<ext::Args>()).unwrap()
}

#[cfg(test)]
mod tests;
