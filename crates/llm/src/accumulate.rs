//! Builds the finished reply from a reply's events, and checks they arrive
//! in order: a part opens before it grows, and nothing follows `Finish`.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use serde_json::Value;

use crate::error::{Error, Kind};
use crate::spec::{
    Assistant, AssistantPart, Block, Event, Finish, Meta, Origin, Reasoning, Response, SmolStr,
    Text, ToolCall, Usage,
};

#[derive(Debug, Default)]
pub struct Accumulator {
    parts: BTreeMap<u32, Building>,
    finish: Option<Finished>,
    id: Option<SmolStr>,
    model: Option<SmolStr>,
}

#[derive(Debug)]
struct Finished {
    finish: Finish,
    usage: Usage,
    meta: Meta,
    order: Option<Vec<u32>>,
}

#[derive(Debug)]
enum Building {
    Open {
        block: Block,
        text: String,
        name: Option<SmolStr>,
        meta: Meta,
    },
    Done(AssistantPart),
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// A part that opens at `index`, which must be new.
    fn opens(&mut self, index: u32, part: Building) -> Result<(), Error> {
        match self.parts.entry(index) {
            Entry::Occupied(_) => Err(order(format!("part {index} opened twice"))),
            Entry::Vacant(slot) => {
                slot.insert(part);
                Ok(())
            }
        }
    }

    pub fn push(&mut self, event: Event) -> Result<(), Error> {
        if self.finish.is_some() {
            return Err(order("an event followed the reply's finish"));
        }
        match event {
            Event::Metadata { id, model } => {
                if id.is_some() {
                    self.id = id;
                }
                if model.is_some() {
                    self.model = model;
                }
            }
            Event::Start { index, block, name } => self.opens(
                index,
                Building::Open {
                    block,
                    text: String::new(),
                    name,
                    meta: Meta::new(),
                },
            )?,
            Event::Delta { index, text: delta } => match self.parts.get_mut(&index) {
                Some(Building::Open { text, .. }) => text.push_str(&delta),
                _ => return Err(order(format!("part {index} grew while not open"))),
            },
            Event::Meta { index, meta: more } => match self.parts.get_mut(&index) {
                Some(Building::Open { meta, .. }) => merge_meta(meta, more),
                Some(Building::Done(part)) => merge_meta(part.meta_mut(), more),
                None => return Err(order(format!("state for part {index}, which never opened"))),
            },
            Event::End {
                index,
                redacted,
                text: restated,
                call,
            } => {
                let Some(Building::Open {
                    block,
                    text,
                    name,
                    meta,
                }) = self.parts.remove(&index)
                else {
                    return Err(order(format!("part {index} ended while not open")));
                };
                let text = restated.unwrap_or(text);
                let part = match block {
                    Block::Text => AssistantPart::Text(Text { text, meta }),
                    Block::Reasoning => AssistantPart::Reasoning(Reasoning {
                        text,
                        redacted,
                        meta,
                    }),
                    Block::ToolCall => {
                        let Some(call) = call else {
                            return Err(order(format!("call {index} ended without its id")));
                        };
                        AssistantPart::ToolCall(ToolCall {
                            id: call.id,
                            name: name.unwrap_or_default(),
                            input: call.input,
                            invalid: call.invalid,
                            provider_executed: call.provider_executed,
                            meta,
                        })
                    }
                };
                self.parts.insert(index, Building::Done(part));
            }
            Event::Part { index, part } => self.opens(index, Building::Done(part))?,
            Event::Finish {
                finish,
                usage,
                meta,
                order: sequence,
            } => {
                if let Some(index) = self.parts.iter().find_map(|(index, part)| {
                    matches!(part, Building::Open { .. }).then_some(*index)
                }) {
                    return Err(order(format!("the reply finished with part {index} open")));
                }
                self.finish = Some(Finished {
                    finish,
                    usage,
                    meta,
                    order: sequence,
                });
            }
        }
        Ok(())
    }

    /// The reply, once finished. `origin` is stamped on its message.
    pub fn finish(mut self, origin: Option<Origin>) -> Result<Response, Error> {
        let Finished {
            finish,
            usage,
            meta,
            order: sequence,
        } = self.finish.take().ok_or_else(Error::incomplete)?;
        let mut content: Vec<AssistantPart> = sequence
            .unwrap_or_default()
            .into_iter()
            .filter_map(|index| match self.parts.remove(&index) {
                Some(Building::Done(part)) => Some(part),
                _ => None,
            })
            .collect();
        content.extend(self.parts.into_values().filter_map(|part| match part {
            Building::Done(part) => Some(part),
            Building::Open { .. } => None,
        }));
        Ok(Response {
            message: Assistant { content, origin },
            finish,
            usage,
            id: self.id,
            model: self.model,
            meta,
        })
    }
}

fn order(message: impl Into<String>) -> Error {
    Error::new(Kind::Decode, message)
}

/// `more` over `meta`, key by key; object values merge one level down so
/// state that arrives in pieces (a signature after the text) accumulates.
pub(crate) fn merge_meta(meta: &mut Meta, more: Meta) {
    for (key, value) in more {
        match (meta.get_mut(&key), value) {
            (Some(Value::Object(have)), Value::Object(add)) => have.extend(add),
            (_, value) => {
                meta.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests;
