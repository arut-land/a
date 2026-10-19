//! `settings`: the commands that show and change what a session runs with,
//! over the [`ext::Settings`] the engine keeps.

use ext::{Command, Cx, Extension, Registrar};
use protocol::Overrides;
use serde_json::{Value, json};

/// The `settings` extension.
pub struct Settings;

impl Extension for Settings {
    const NAME: &'static str = "settings";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.command(Model);
    }
}

/// `/model`: the model the session calls and the layer that names it;
/// `/model <host:model>` sets it for this session, over every config
/// layer, and `/model reset` leaves it to them again.
struct Model;

impl Command for Model {
    const NAME: &'static str = "model";
    const DESCRIPTION: &'static str = "set the session's model";

    async fn run(&self, args: &str, cx: &Cx) -> Result<Value, String> {
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
        Ok(json!({ "model": model, "from": from }))
    }
}
