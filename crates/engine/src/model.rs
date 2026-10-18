//! The model a session calls: a provider and model id from `A_MODEL`
//! (`host:model`, or a bare model on `OpenAI`), sent over ureq.

use std::collections::HashMap;
use std::pin::Pin;

use futures_core::Stream;
use llm::{Accumulator, Block, Event, Http, Message, Provider, Request, Response};

pub struct Model {
    provider: Provider,
    id: llm::SmolStr,
    http: Box<dyn Http>,
}

impl Model {
    pub fn from_spec(spec: &str, rt: rt::Rt) -> Result<Self, String> {
        let (provider, id) = llm::hosts::provider(spec).map_err(|e| e.to_string())?;
        Ok(Self::new(provider, id, llm::Ureq::new(rt)))
    }

    pub fn new(provider: Provider, id: llm::SmolStr, http: impl Http) -> Self {
        Self {
            provider,
            id,
            http: Box::new(http),
        }
    }

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
