//! The one pass every request goes through before a protocol encodes it:
//! what another origin produced is cut down to what this target can take,
//! and the transcript is repaired into a shape every provider accepts.
//!
//! - An assistant message from another protocol, provider or model loses
//!   its reasoning, its opaque items, its hosted-tool steps and all part
//!   state: those mean something only to their producer. Readable thinking
//!   is dropped rather than turned into text, which a model would take for
//!   its own answer.
//! - Tool-call ids the target cannot take are rewritten, with their results.
//! - A call nothing answered gets an error result before the conversation
//!   moves on; a result nothing asked for is dropped.
//! - A system message between a call and its results waits until after them.
//!
//! The pass is a fold, a message at a time, as the encoding it feeds is
//! ([`Fold`](crate::conversation::Fold)). It holds back only what a later message may still change: a
//! tool message whose calls are not all answered, and the system messages
//! waiting behind it. A message it does not change goes on as it is.

use std::borrow::Cow;

use crate::conversation::{Settled, Step};
use crate::error::Error;
use crate::protocol::Target;
use crate::spec::{
    Assistant, AssistantPart, Message, Meta, Output, SmolStr, ToolMessage, ToolResult,
};

/// The text an unanswered call is answered with.
pub const NO_RESULT: &str = "No result provided";

/// Whether `assistant` goes back to the protocol, provider and model that
/// produced it.
fn native(assistant: &Assistant, target: &Target<'_>) -> bool {
    assistant.origin.as_ref().is_some_and(|origin| {
        origin.protocol == target.provider.protocol.key()
            && origin.provider == *target.provider.id
            && origin.model == target.model
    })
}

/// The pass over a conversation, as far as it got.
#[derive(Clone, Default)]
pub(crate) struct Prepare {
    ids: Ids,
    /// Calls awaiting results, in call order: (id, name).
    pending: Vec<(SmolStr, SmolStr)>,
    /// The last tool message, held while calls wait whose error results
    /// would join it.
    answers: Option<ToolMessage>,
    /// System messages between a call and its results.
    held: Vec<Message>,
    /// How many messages went on.
    sent: usize,
}

impl Prepare {
    /// Passes `message` on to `fold` as `target` takes it, with what it
    /// releases; what the fold settles goes to `out`.
    pub(crate) fn push(
        &mut self,
        target: &Target<'_>,
        message: &Message,
        fold: &mut dyn Step,
        out: &mut Settled,
    ) -> Result<(), Error> {
        let Some(message) = self.fit(message, target) else {
            return Ok(());
        };
        let mut fold = Into { fold, out };
        match &*message {
            Message::Tool(_) => self.results(message, &mut fold),
            Message::System(_) if !self.pending.is_empty() => {
                self.held.push(message.into_owned());
                Ok(())
            }
            Message::System(_) | Message::User(_) | Message::Assistant(_) => {
                self.answer(&mut fold)?;
                self.release(&mut fold)?;
                if let Message::Assistant(assistant) = &*message {
                    let calls = assistant.tool_calls();
                    let calls = calls.map(|call| (call.id.clone(), call.name.clone()));
                    self.pending.extend(calls);
                }
                self.send(&message, &mut fold)
            }
        }
    }

    /// The conversation ends here: the calls still waiting are answered,
    /// and what waited goes on.
    pub(crate) fn finish(&mut self, fold: &mut dyn Step, out: &mut Settled) -> Result<(), Error> {
        let mut fold = Into { fold, out };
        self.answer(&mut fold)?;
        self.release(&mut fold)
    }

    fn send(&mut self, message: &Message, fold: &mut Into<'_>) -> Result<(), Error> {
        self.sent += 1;
        fold.fold.push(self.sent - 1, message, fold.out)
    }

    /// The system messages held, once no call waits.
    fn release(&mut self, fold: &mut Into<'_>) -> Result<(), Error> {
        for message in std::mem::take(&mut self.held) {
            self.send(&message, fold)?;
        }
        Ok(())
    }

    /// A tool message: the results that waiting calls asked for. It waits
    /// itself while calls still do.
    fn results(&mut self, mut message: Cow<'_, Message>, fold: &mut Into<'_>) -> Result<(), Error> {
        let Message::Tool(tool) = &*message else {
            return Ok(());
        };
        let pending = &mut self.pending;
        let kept: Vec<bool> = (tool.results.iter())
            .map(|result| {
                let waiting = pending.iter().position(|(id, _)| *id == result.id);
                waiting.map(|at| pending.remove(at)).is_some()
            })
            .collect();
        if kept.contains(&false)
            && let Message::Tool(tool) = message.to_mut()
        {
            let mut kept = kept.iter();
            tool.results.retain(|_| kept.next() == Some(&true));
        }
        if kept.contains(&true) {
            if let Some(answers) = self.answers.take() {
                self.send(&Message::Tool(answers), fold)?;
            }
            if self.pending.is_empty() {
                self.send(&message, fold)?;
            } else if let Message::Tool(tool) = message.into_owned() {
                self.answers = Some(tool);
            }
        }
        if self.pending.is_empty() {
            if let Some(answers) = self.answers.take() {
                self.send(&Message::Tool(answers), fold)?;
            }
            self.release(fold)?;
        }
        Ok(())
    }

    /// Every call still waiting gets an error result before the
    /// conversation moves on, joining the results it began.
    fn answer(&mut self, fold: &mut Into<'_>) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let results = self.pending.drain(..).map(|(id, name)| ToolResult {
            id,
            name,
            output: Output::Text(NO_RESULT.to_owned()),
            error: true,
            meta: Meta::new(),
        });
        let mut answers = self.answers.take().unwrap_or_default();
        answers.results.extend(results);
        self.send(&Message::Tool(answers), fold)
    }

    /// `message` cut down to what `target` takes; `None` for a turn left
    /// with nothing.
    fn fit<'m>(&mut self, message: &'m Message, target: &Target<'_>) -> Option<Cow<'m, Message>> {
        let protocol = &target.provider.protocol;
        let fit = |id: &str| protocol.tool_id(id);
        match message {
            Message::Assistant(assistant) => {
                let native = native(assistant, target);
                let ids: Vec<_> = assistant
                    .tool_calls()
                    .filter(|call| native || !call.provider_executed)
                    .map(|call| {
                        self.ids
                            .rename(&call.id, |id| if native { None } else { fit(id) })
                    })
                    .collect();
                let unchanged = assistant
                    .tool_calls()
                    .filter(|call| native || !call.provider_executed)
                    .zip(&ids)
                    .all(|(call, id)| call.id == *id);
                let fits = |part: &AssistantPart| match part {
                    AssistantPart::Text(_) | AssistantPart::Media(_) => part.meta().is_empty(),
                    AssistantPart::ToolCall(call) => {
                        !call.provider_executed && call.meta.is_empty()
                    }
                    _ => false,
                };
                if unchanged && (native || assistant.content.iter().all(fits)) {
                    return (!assistant.content.is_empty()).then_some(Cow::Borrowed(message));
                }
                let mut assistant = assistant.clone();
                assistant.content.retain(|part| {
                    native
                        || match part {
                            AssistantPart::Text(_) | AssistantPart::Media(_) => true,
                            AssistantPart::ToolCall(call) => !call.provider_executed,
                            AssistantPart::Reasoning(_)
                            | AssistantPart::ToolResult(_)
                            | AssistantPart::Opaque(_) => false,
                        }
                });
                let mut ids = ids.into_iter();
                for part in &mut assistant.content {
                    if !native {
                        part.meta_mut().clear();
                    }
                    if let AssistantPart::ToolCall(call) = part {
                        call.id = ids.next().expect("one allocated id per call");
                    }
                }
                (!assistant.content.is_empty()).then_some(Cow::Owned(Message::Assistant(assistant)))
            }
            Message::Tool(tool)
                if tool
                    .results
                    .iter()
                    .any(|r| self.ids.renamed(&r.id).is_some()) =>
            {
                let mut tool = tool.clone();
                for result in &mut tool.results {
                    if let Some(id) = self.ids.renamed(&result.id) {
                        result.id = id;
                    }
                }
                Some(Cow::Owned(Message::Tool(tool)))
            }
            message => Some(Cow::Borrowed(message)),
        }
    }
}

/// The fold a pass sends into, and where that fold settles.
struct Into<'a> {
    fold: &'a mut dyn Step,
    out: &'a mut Settled,
}

#[derive(Clone, Default)]
struct Ids {
    /// Sorted by the id renamed, as `taken` is sorted: sets this small
    /// cost less code sorted than hashed.
    renamed: Vec<(SmolStr, SmolStr)>,
    taken: Vec<SmolStr>,
}

impl Ids {
    fn rename(&mut self, id: &SmolStr, fit: impl Fn(&str) -> Option<String>) -> SmolStr {
        if let Some(renamed) = self.renamed(id) {
            return renamed;
        }
        let mut fitted = fit(id).unwrap_or_else(|| id.to_string());
        // Prefix the counter when a target truncates away the suffix.
        let base = fitted.clone();
        let mut n = 1;
        while self.taken(&fitted) {
            n += 1;
            let suffixed = format!("{base}_{n}");
            fitted = fit(&suffixed).unwrap_or(suffixed);
            if self.taken(&fitted) {
                let prefixed = format!("{n}_{base}");
                fitted = fit(&prefixed).unwrap_or(prefixed);
            }
        }
        let fitted = SmolStr::from(fitted);
        self.take(fitted.clone());
        let at = self.renamed.partition_point(|(held, _)| held < id);
        self.renamed.insert(at, (id.clone(), fitted.clone()));
        fitted
    }

    fn renamed(&self, id: &str) -> Option<SmolStr> {
        let at = self
            .renamed
            .binary_search_by(|(held, _)| held.as_str().cmp(id));
        at.ok().map(|at| self.renamed[at].1.clone())
    }

    fn taken(&self, id: &str) -> bool {
        self.taken
            .binary_search_by(|held| held.as_str().cmp(id))
            .is_ok()
    }

    fn take(&mut self, id: SmolStr) {
        if let Err(at) = self.taken.binary_search(&id) {
            self.taken.insert(at, id);
        }
    }
}

/// `request` as `target` takes it, whole: the messages the pass sends.
#[cfg(test)]
pub(crate) fn prepare(request: &crate::spec::Request, target: &Target<'_>) -> crate::spec::Request {
    /// The messages a pass sends, as they are.
    struct Sent(Vec<Message>);

    impl Step for Sent {
        fn push(&mut self, _: usize, message: &Message, _: &mut Settled) -> Result<(), Error> {
            self.0.push(message.clone());
            Ok(())
        }
    }

    let (mut pass, mut sent) = (Prepare::default(), Sent(Vec::new()));
    let mut out = Settled::default();
    for message in request.messages.iter() {
        let _ = pass.push(target, message, &mut sent, &mut out);
    }
    let _ = pass.finish(&mut sent, &mut out);
    crate::spec::Request {
        messages: sent.0.into(),
        ..request.clone()
    }
}

#[cfg(test)]
mod tests;
