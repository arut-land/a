//! `chat`: the default loop, [`ext::Loop`], under the name `chat`.

use ext::{Extension, Registrar};

/// The agent's name.
pub const CHAT: &str = "chat";

/// The `chat` extension, which registers [`ext::Loop`] as [`CHAT`].
pub struct Chat;

impl Extension for Chat {
    const NAME: &'static str = "chat";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.agent(CHAT, || Box::new(ext::Loop::default()));
    }
}
