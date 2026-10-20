//! The model a session calls: a provider and model id from its spec
//! (`host:model`, or a bare model on `OpenAI`), sent over ureq.

use std::sync::Arc;

use llm::{Block, Event, Events, Http, Messages, Provider, Request};
use protocol::Environment;

use crate::config::Settings;

/// Where every session's model comes from: built per call, so a client's
/// fresh environment or an edited config file applies to the next one.
pub struct Models {
    pub settings: Settings,
    pub http: Arc<dyn Http>,
}

impl Models {
    /// The model `spec` names, with provider variables looked up from the
    /// session's client environment `env` first; or why there is none,
    /// said so the user can fix it.
    pub fn model(&self, env: Option<&Environment>, spec: &str) -> Result<Model, String> {
        let lookup = |name: &str| self.settings.var(env, name);
        match llm::hosts::provider(spec, &lookup) {
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

    /// The model's id, as its provider names it.
    pub fn id(&self) -> llm::SmolStr {
        self.id.clone()
    }

    /// A request to this model for a reply to `messages`, offering `tools`.
    pub fn request(&self, messages: Messages, tools: llm::Tools) -> Request {
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
