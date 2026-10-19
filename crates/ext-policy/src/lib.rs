//! `policy`: which calls wait for the user's approval.

use ext::{Extension, Registrar};
use protocol::Permission;

/// The `policy` extension: calls of the tools in `ask` wait for the user's
/// approval; every other call is allowed.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub ask: Vec<String>,
}

impl Policy {
    pub fn ask(tools: &[&str]) -> Self {
        Self {
            ask: tools.iter().map(|&t| t.to_owned()).collect(),
        }
    }
}

impl Extension for Policy {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let ask = self.ask.clone();
        r.on::<ext::point::Permission>(move |_, call| {
            if ask.iter().any(|t| t == call.tool) {
                Permission::Ask
            } else {
                Permission::Allow
            }
        });
    }
}
