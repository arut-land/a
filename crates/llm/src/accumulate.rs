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

    pub fn is_finished(&self) -> bool {
        self.finish.is_some()
    }

    /// The text streamed so far, finished parts and open ones, in order.
    pub fn text(&self) -> String {
        self.parts
            .values()
            .filter_map(|part| match part {
                Building::Open {
                    block: Block::Text,
                    text,
                    ..
                } => Some(text.as_str()),
                Building::Done(AssistantPart::Text(text)) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
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
mod tests {
    use serde_json::json;

    use super::*;
    use crate::spec::{CallEnd, Reason};

    fn finish(order: Option<Vec<u32>>) -> Event {
        Event::Finish {
            finish: Finish::new(Reason::Stop, None),
            usage: Usage::default(),
            meta: Meta::new(),
            order,
        }
    }

    #[test]
    fn builds_parts_in_index_order_with_state_and_calls() {
        let mut acc = Accumulator::new();
        let events = [
            Event::Metadata {
                id: Some("r1".into()),
                model: None,
            },
            Event::Start {
                index: 0,
                block: Block::Reasoning,
                name: None,
            },
            Event::Start {
                index: 1,
                block: Block::ToolCall,
                name: Some("f".into()),
            },
            Event::Delta {
                index: 1,
                text: "{\"a\":".into(),
            },
            Event::Delta {
                index: 0,
                text: "think".into(),
            },
            Event::End {
                index: 0,
                redacted: false,
                text: None,
                call: None,
            },
            Event::Meta {
                index: 0,
                meta: Meta::from([("p".into(), json!({"sig": "s"}))]),
            },
            Event::End {
                index: 1,
                redacted: false,
                text: None,
                call: Some(CallEnd {
                    id: "c1".into(),
                    input: json!({"a": 1}).as_object().unwrap().clone(),
                    invalid: None,
                    provider_executed: false,
                }),
            },
            finish(None),
        ];
        for event in events {
            acc.push(event).unwrap();
        }
        let response = acc.finish(None).unwrap();
        assert_eq!(response.id.as_deref(), Some("r1"));
        let [
            AssistantPart::Reasoning(reasoning),
            AssistantPart::ToolCall(call),
        ] = response.message.content.as_slice()
        else {
            panic!("{:?}", response.message.content)
        };
        assert_eq!(reasoning.text, "think");
        assert_eq!(reasoning.meta["p"]["sig"], "s");
        assert_eq!((call.id.as_str(), call.name.as_str()), ("c1", "f"));
        assert_eq!(call.input["a"], 1);
    }

    #[test]
    fn a_restated_end_and_a_stated_order_win() {
        let mut acc = Accumulator::new();
        for index in [0, 1] {
            acc.push(Event::Start {
                index,
                block: Block::Text,
                name: None,
            })
            .unwrap();
            acc.push(Event::Delta {
                index,
                text: "draft".into(),
            })
            .unwrap();
            acc.push(Event::End {
                index,
                redacted: false,
                text: Some(format!("final {index}")),
                call: None,
            })
            .unwrap();
        }
        acc.push(finish(Some(vec![1, 0]))).unwrap();
        let texts: Vec<_> = acc
            .finish(None)
            .unwrap()
            .message
            .content
            .into_iter()
            .map(|part| match part {
                AssistantPart::Text(text) => text.text,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(texts, ["final 1", "final 0"]);
    }

    #[test]
    fn rejects_events_out_of_order() {
        let mut acc = Accumulator::new();
        assert!(
            acc.push(Event::Delta {
                index: 0,
                text: "x".into()
            })
            .is_err()
        );
        acc.push(finish(None)).unwrap();
        assert!(
            acc.push(Event::Metadata {
                id: None,
                model: None
            })
            .is_err()
        );
    }
}
