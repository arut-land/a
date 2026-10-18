//! The model a session calls: a provider and model id from its settings
//! (`host:model`, or a bare model on `OpenAI`), sent over ureq.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;
use llm::{Accumulator, Block, Event, Http, Message, Provider, Request, Response};
use protocol::Environment;

use crate::config::Settings;

/// Where every session's model comes from: built per call, so a client's
/// fresh environment or an edited config file applies to the next one.
pub struct Models {
    pub settings: Settings,
    pub http: Arc<dyn Http>,
}

impl Models {
    /// The model a session in `workspace` calls, with provider variables
    /// looked up from its client environment `env` first; or why there is
    /// none, said so the user can fix it.
    pub fn model(&self, workspace: &Path, env: Option<&Environment>) -> Result<Model, String> {
        let spec = self.settings.model(workspace)?;
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
    pub fn request(&self, messages: Vec<Message>, tools: Vec<llm::Tool>) -> Request {
        Request {
            model: self.id.clone(),
            messages,
            tools,
            ..Request::default()
        }
    }

    /// The reply to `request`. Answer text streams to `on_text` as it
    /// arrives.
    pub async fn reply(
        &self,
        request: &Request,
        on_text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Response, llm::Error> {
        let mut events = llm::stream(&*self.http, &self.provider, request);
        let mut acc = Accumulator::new();
        let mut blocks = HashMap::new();
        while let Some(event) = std::future::poll_fn(|cx| Pin::new(&mut events).poll_next(cx)).await
        {
            let event = event?;
            match &event {
                Event::Start { index, block, .. } => {
                    blocks.insert(*index, *block);
                }
                Event::Delta { index, text } if blocks.get(index) == Some(&Block::Text) => {
                    on_text(text);
                }
                _ => {}
            }
            acc.push(event)?;
        }
        acc.finish(Some(llm::origin(&self.provider, &request.model)))
    }
}
