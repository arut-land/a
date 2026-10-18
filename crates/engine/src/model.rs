//! The model a session calls: a provider and model id from `A_MODEL`
//! (`host:model`, or a bare model on `OpenAI`), sent over ureq.

use std::collections::HashMap;
use std::sync::Arc;

use llm::{Accumulator, Block, Event, Message, Provider, Request, Response};

pub struct Model {
    provider: Provider,
    id: llm::SmolStr,
    http: llm::Ureq,
}

impl Model {
    pub fn from_spec(spec: &str, rt: Arc<dyn rt::Runtime>) -> Result<Self, String> {
        let (provider, id) = llm::hosts::provider(spec).map_err(|e| e.to_string())?;
        Ok(Self {
            provider,
            id,
            http: llm::Ureq::new(rt),
        })
    }

    /// How a call to this model is recorded: `host:model`.
    pub fn name(&self) -> String {
        format!("{}:{}", self.provider.id, self.id)
    }

    /// The reply to `messages`. Answer text streams to `on_text` as it
    /// arrives.
    pub fn reply(
        &self,
        messages: Vec<Message>,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Response, llm::Error> {
        let request = Request {
            model: self.id.clone(),
            messages,
            ..Request::default()
        };
        rt::block_on(async {
            let mut events = llm::stream(&self.http, &self.provider, &request);
            let mut acc = Accumulator::new();
            let mut blocks = HashMap::new();
            while let Some(event) = std::future::poll_fn(|cx| {
                futures_core::Stream::poll_next(std::pin::Pin::new(&mut events), cx)
            })
            .await
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
            acc.finish(Some(llm::origin(&self.provider, &self.id)))
        })
    }
}
