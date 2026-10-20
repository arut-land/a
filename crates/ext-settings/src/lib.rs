//! `settings`: a session's own choice of model over the config layers the
//! engine reads, as the [`ModelSpec`] the engine asks, and the command
//! that shows and changes it.

use std::path::Path;
use std::sync::Arc;

use ext::{Args, Cx, Extension, Fold, Layers, ModelSpec, Registrar, States, Tool};
use protocol::Audience;
use serde::Serialize;

/// The `settings` extension.
pub struct Settings;

/// The model the session chose for itself, if it did: `/model <spec>`
/// sets it, `/model reset` clears it.
#[derive(Debug, Default)]
pub struct ModelChoice(pub Option<String>);

impl Fold for ModelChoice {
    const NAME: &'static str = "model";
    type Change = Option<String>;

    fn apply(&mut self, choice: &Option<String>) {
        self.0.clone_from(choice);
    }
}

impl Extension for Settings {
    const NAME: &'static str = "settings";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.fold::<ModelChoice>()
            .provide::<dyn ModelSpec>(Arc::new(Self))
            .tool(Model);
    }
}

/// Where a session's own choice comes from.
const OWN: &str = "this session";

impl ModelSpec for Settings {
    fn model(
        &self,
        states: &States,
        layers: &dyn Layers,
        workspace: &Path,
    ) -> Result<(String, String), String> {
        match states.get::<ModelChoice>().and_then(|m| m.0.clone()) {
            Some(model) => Ok((model, OWN.to_owned())),
            None => layers.model(workspace),
        }
    }
}

/// `/model`: the model the session calls and the layer that names it;
/// `/model <host:model>` sets it for this session, over every config
/// layer, and `/model reset` leaves it to them again.
struct Model;

/// The session's model, and the layer that names it.
#[derive(Serialize)]
struct Chosen {
    model: String,
    from: String,
}

impl Tool for Model {
    type In = Args;
    type Out = Chosen;

    const NAME: &'static str = "model";
    const DESCRIPTION: &'static str = "set the session's model";
    const AUDIENCE: Audience = Audience::User;

    fn call(&self, Args { args }: &Args, cx: &Cx) -> Result<Chosen, String> {
        let args = args.as_str();
        let layers = cx
            .service::<dyn Layers>()
            .ok_or("this engine reads no settings")?;
        let chosen = if args.is_empty() {
            cx.state(|m: &ModelChoice| m.0.clone())
        } else {
            let choice = (args != "reset").then(|| args.to_owned());
            cx.update::<ModelChoice>(choice.clone());
            choice
        };
        let (model, from) = match chosen {
            Some(model) => (model, OWN.to_owned()),
            None => layers.model(&cx.workspace)?,
        };
        Ok(Chosen { model, from })
    }
}
