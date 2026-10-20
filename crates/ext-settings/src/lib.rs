//! `settings`: the commands that show and change what a session runs with,
//! over the [`ext::Settings`] the engine keeps.

use ext::{Args, Cx, Extension, Registrar, Tool};
use protocol::{Audience, Overrides};
use serde::Serialize;

/// The `settings` extension.
pub struct Settings;

impl Extension for Settings {
    const NAME: &'static str = "settings";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Model);
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
        let settings = cx
            .service::<dyn ext::Settings>()
            .ok_or("this engine keeps no settings")?;
        if !args.is_empty() {
            let model = (args != "reset").then(|| args.to_owned());
            settings
                .set(cx, Overrides { model })
                .await
                .map_err(|e| e.to_string())?;
        }
        let (model, from) = settings.model(cx)?;
        Ok(Chosen { model, from })
    }
}
