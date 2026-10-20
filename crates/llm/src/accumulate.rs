//! Builds the finished reply from a reply's events, and checks they arrive
//! in order: a part opens before it grows, and nothing follows `Finish`.
//! It keeps no copy of the text that streams: a part's text arrives whole,
//! moved, with its end.

use crate::error::{Error, Kind};
use crate::spec::{
    Assistant, AssistantPart, Ended, Event, Finish, Meta, Opaque, Origin, Reasoning, Response,
    SmolStr, Text, ToolCall, Usage,
};

#[derive(Debug, Default)]
pub struct Accumulator {
    /// The parts by index, in index order. A reply holds few, and a list
    /// costs less code than a map.
    parts: Vec<(u32, Building)>,
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

/// A part while it streams: a call's name, and the state it gathers. Its
/// end says what it was.
#[derive(Debug)]
enum Building {
    Open { name: Option<SmolStr>, meta: Meta },
    Done(AssistantPart),
}

impl Building {
    /// The part an open part ends as.
    fn end(self, index: u32, ended: Ended) -> Result<AssistantPart, Error> {
        let Self::Open { name, meta } = self else {
            return Err(order(format!("part {index} ended while not open")));
        };
        Ok(match ended {
            Ended::Text { text } => AssistantPart::Text(Text { text, meta }),
            Ended::Reasoning { text, redacted } => AssistantPart::Reasoning(Reasoning {
                text,
                redacted,
                meta,
            }),
            Ended::Call(call) => AssistantPart::ToolCall(ToolCall {
                id: call.id,
                name: name.unwrap_or_default(),
                input: call.input,
                invalid: call.invalid,
                provider_executed: call.provider_executed,
                meta,
            }),
        })
    }
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// A part that opens at `index`, which must be new.
    fn opens(&mut self, index: u32, part: Building) -> Result<(), Error> {
        match self.parts.binary_search_by_key(&index, |(at, _)| *at) {
            Ok(_) => Err(order(format!("part {index} opened twice"))),
            Err(at) => {
                self.parts.insert(at, (index, part));
                Ok(())
            }
        }
    }

    fn part(&mut self, index: u32) -> Option<&mut Building> {
        let at = self
            .parts
            .binary_search_by_key(&index, |(at, _)| *at)
            .ok()?;
        Some(&mut self.parts[at].1)
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
            Event::Start { index, name, .. } => self.opens(
                index,
                Building::Open {
                    name,
                    meta: Meta::new(),
                },
            )?,
            Event::Delta { index, .. } => {
                if !matches!(self.part(index), Some(Building::Open { .. })) {
                    return Err(order(format!("part {index} grew while not open")));
                }
            }
            Event::Meta { index, meta: more } => match self.part(index) {
                Some(Building::Open { meta, .. }) => meta.merge(more),
                Some(Building::Done(part)) => part.meta_mut().merge(more),
                None => return Err(order(format!("state for part {index}, which never opened"))),
            },
            Event::End { index, ended } => {
                let Some(slot) = self
                    .part(index)
                    .filter(|part| matches!(part, Building::Open { .. }))
                else {
                    return Err(order(format!("part {index} ended while not open")));
                };
                // An empty opaque part holds the slot meanwhile, at no
                // allocation.
                let open = std::mem::replace(
                    slot,
                    Building::Done(AssistantPart::Opaque(Opaque::default())),
                );
                *slot = Building::Done(open.end(index, ended)?);
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

    /// The answer text of the parts that ended, in index order: a reply
    /// cut short. One part's text is moved, not copied.
    pub fn into_text(self) -> Option<String> {
        let mut texts = self.parts.into_iter().filter_map(|(_, part)| match part {
            Building::Done(AssistantPart::Text(text)) => Some(text.text),
            _ => None,
        });
        let mut text = texts.next()?;
        texts.for_each(|more| text.push_str(&more));
        Some(text).filter(|text| !text.is_empty())
    }

    /// The reply, once finished. `origin` is stamped on its message.
    pub fn finish(mut self, origin: Option<Origin>) -> Result<Response, Error> {
        let Finished {
            finish,
            usage,
            meta,
            order: sequence,
        } = self.finish.take().ok_or_else(Error::incomplete)?;
        let mut parts: Vec<(u32, Option<AssistantPart>)> = (self.parts.into_iter())
            .map(|(index, part)| match part {
                Building::Done(part) => (index, Some(part)),
                Building::Open { .. } => (index, None),
            })
            .collect();
        let mut content = Vec::with_capacity(parts.len());
        for index in sequence.unwrap_or_default() {
            if let Ok(at) = parts.binary_search_by_key(&index, |(at, _)| *at) {
                content.extend(parts[at].1.take());
            }
        }
        content.extend(parts.into_iter().filter_map(|(_, part)| part));
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

#[cfg(test)]
mod tests;
