//! `settings`: a session's own choice of model, kept as its state over the
//! config layers and offered to the engine as the [`ModelSpec`] service,
//! and `/model`, which shows and changes it. Nothing below this crate
//! knows the choice by name: the engine asks the service.

use std::path::Path;
use std::sync::Arc;

use ext::{
    Args, Configured, Cx, Extension, Fold, Layers, ModelSpec, Provides, Refused, Registrar, States,
    Table, Tool,
};
use protocol::Audience;
use serde::{Deserialize, Serialize};

/// The `settings` extension.
pub struct Settings;

/// The model the session chose for itself, over every config layer, if it
/// did.
#[derive(Debug, Default, Serialize, Deserialize)]
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
            .provide::<dyn ModelSpec>(Arc::new(Settings))
            .tool(Model);
    }
}

impl Provides<dyn ModelSpec> for Settings {}

impl Configured for Settings {
    fn configure(_: &impl Table) -> Result<Self, Refused> {
        Ok(Self)
    }
}

impl ModelSpec for Settings {
    fn model(
        &self,
        states: &States,
        layers: &dyn Layers,
        workspace: &Path,
    ) -> Result<(String, String), String> {
        let chosen = states.get::<ModelChoice>().and_then(|m| m.0.clone());
        named(chosen, layers, workspace)
    }
}

/// The model `chosen` for the session, or the layers' for `workspace`.
fn named(
    chosen: Option<String>,
    layers: &dyn Layers,
    workspace: &Path,
) -> Result<(String, String), String> {
    match chosen {
        Some(model) => Ok((model, "this session".to_owned())),
        None => layers.model(workspace),
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

    async fn call(&self, Args { args }: &Args, cx: &Cx) -> Result<Chosen, String> {
        let args = args.as_str();
        let layers = cx
            .service::<dyn Layers>()
            .ok_or("this engine reads no config layers")?;
        if !args.is_empty() {
            let choice = (args != "reset").then(|| args.to_owned());
            cx.update::<ModelChoice>(&choice).await;
        }
        let chosen = cx.state(|m: &ModelChoice| m.0.clone());
        let (model, from) = named(chosen, layers, &cx.workspace)?;
        Ok(Chosen { model, from })
    }
}
