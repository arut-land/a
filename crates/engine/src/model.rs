//! The model a session calls: a provider and model id from its settings
//! (`host:model`, or a bare model on `OpenAI`), sent over ureq.

use std::path::Path;
use std::sync::Arc;

use llm::{Block, Event, Events, Http, Messages, Provider, Request};
use protocol::{Environment, Overrides};

use crate::config::Settings;

/// Where every session's model comes from: built per call, so a client's
/// fresh environment or an edited config file applies to the next one.
pub struct Models {
    pub settings: Settings,
    pub http: Arc<dyn Http>,
}

impl Models {
    /// The model a session in `workspace` with its own settings `session`
    /// calls, with provider variables looked up from its client environment
    /// `env` first; or why there is none, said so the user can fix it.
    pub fn model(
        &self,
        workspace: &Path,
        env: Option<&Environment>,
        session: (Overrides, Option<u64>),
    ) -> Result<Model, String> {
        let (spec, _) = self.settings.model_in(workspace, session)?;
        let lookup = |name: &str| self.settings.var(env, name);
        match llm::hosts::provider(&spec, &lookup) {
            Ok((provider, id)) => Ok(Model {
                provider,
                id,
                http: Arc::clone(&self.http),
            }),
            Err(e) if e.kind == llm::Kind::Auth => Err(format!(
                "{e}, in the shell a client attaches from or under [env] in {}",
                self.settings.home.path().display()
            )),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub struct Model {
    provider: Provider,
    id: llm::SmolStr,
    http: Arc<dyn Http>,
}

impl Model {
    /// How a call to this model is recorded: `host:model`.
    pub fn name(&self) -> String {
        format!("{}:{}", self.provider.id, self.id)
    }

    /// A request to this model for a reply to `messages`, offering `tools`.
    pub fn request(&self, messages: Messages, tools: Vec<llm::Tool>) -> Request {
        Request {
            model: self.id.clone(),
            messages,
            tools,
            ..Request::default()
        }
    }

    /// The reply to `request`, as it streams.
    pub fn send(&self, request: &Request) -> Events {
        llm::stream(&*self.http, &self.provider, request)
    }

    /// What a reply to `request` names as its producer.
    pub fn origin(&self, request: &Request) -> llm::Origin {
        llm::origin(&self.provider, &request.model)
    }
}

/// Reads `events` to their end, the answer text streaming to `on_text` as
/// it arrives.
pub async fn read(
    events: &mut Events,
    on_text: &mut (dyn FnMut(&str) + Send),
) -> Result<(), llm::Error> {
    // The text parts' indices: a reply has few.
    let mut texts = Vec::new();
    while let Some(event) = events.next().await {
        match event? {
            Event::Start {
                index,
                block: Block::Text,
                ..
            } => texts.push(*index),
            Event::Delta { index, text } if texts.contains(index) => on_text(text),
            _ => {}
        }
    }
    Ok(())
}
