//! A tool releases execution while a durable question owns its continuation.

use std::marker::PhantomData;

use protocol::{Audience, Card, Json, Outcome, Question, QuestionAnswer};
use serde::{Serialize, de::DeserializeOwned};

use crate::{BoxFuture, Called, Concurrency, Cx, DynTool, Parsed, Replay, schema::Schema};

/// Returning Ask transfers the checkpoint to the session actor, without retaining a future.
#[derive(Debug)]
pub enum QuestionStep<O, S> {
    Done(O),
    Ask { question: Question, state: S },
}

/// A tool whose continuation is distinct from starting its external operation again.
pub trait QuestionTool: Send + Sync + 'static {
    type In: Schema + DeserializeOwned + Send + Sync + 'static;
    type Out: Serialize + Send;
    type State: Serialize + DeserializeOwned + Send + 'static;
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const CONCURRENCY: Concurrency = Concurrency::Exclusive;
    fn start(
        &self,
        input: &Self::In,
        cx: &Cx,
    ) -> impl Future<Output = Result<QuestionStep<Self::Out, Self::State>, String>> + Send;
    fn answer(
        &self,
        input: &Self::In,
        state: Self::State,
        answer: QuestionAnswer,
        cx: &Cx,
    ) -> impl Future<Output = Result<QuestionStep<Self::Out, Self::State>, String>> + Send;
    fn present(_input: &Self::In) -> Option<Card> {
        None
    }
    fn present_result(_input: &Self::In, _output: &Self::Out) -> Option<Card> {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parked {
    pub question: Question,
    pub checkpoint: Json,
}

pub(crate) struct Questioning<T>(pub T, pub PhantomData<T>);

impl<T: QuestionTool> Questioning<T> {
    fn called(input: &T::In, output: Result<QuestionStep<T::Out, T::State>, String>) -> Called {
        let output = match output {
            Ok(output) => output,
            Err(e) => return Called::failed(e),
        };
        let (outcome, card, parked) = match output {
            QuestionStep::Done(output) => {
                let json = match Json::encode(&output) {
                    Ok(json) => json,
                    Err(e) => return Called::failed(e.to_string()),
                };
                (
                    Outcome::Done {
                        output: json.into(),
                    },
                    T::present_result(input, &output),
                    None,
                )
            }
            QuestionStep::Ask { question, state } => {
                let checkpoint = match Json::encode(&state) {
                    Ok(json) => json,
                    Err(e) => return Called::failed(e.to_string()),
                };
                (
                    Outcome::Done {
                        output: Json::null().into(),
                    },
                    None,
                    Some(Box::new(Parked {
                        question,
                        checkpoint,
                    })),
                )
            }
        };
        Called {
            outcome,
            card,
            held: Vec::new(),
            parked,
        }
    }
}

impl<T: QuestionTool> DynTool for Questioning<T> {
    fn name(&self) -> &str {
        T::NAME
    }
    fn description(&self) -> &str {
        T::DESCRIPTION
    }
    fn audience(&self) -> Audience {
        Audience::Model
    }
    fn replay(&self) -> Replay {
        Replay::Unsafe
    }
    fn concurrency(&self) -> Concurrency {
        T::CONCURRENCY
    }
    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: T::NAME.into(),
            description: T::DESCRIPTION.into(),
            schema: serde_json::from_str(&crate::schema::of::<T::In>()).expect("derived schema"),
            strict: None,
        }
    }
    fn parse(&self, input: &Json) -> Result<Parsed, String> {
        input
            .parse::<T::In>()
            .map(Parsed::new)
            .map_err(|e| e.to_string())
    }
    fn present(&self, input: &Parsed) -> Option<Card> {
        T::present(input.get()?)
    }
    fn to_model(&self, output: &Json) -> llm::Output {
        llm::Output::Json(output.clone())
    }
    fn call<'a>(&'a self, input: Parsed, cx: &'a Cx) -> BoxFuture<'a, Called> {
        Box::pin(async move {
            let Some(input) = input.take::<T::In>() else {
                return Called::failed("invalid question tool input".into());
            };
            Self::called(&input, self.0.start(&input, cx).await)
        })
    }
    fn answer<'a>(
        &'a self,
        input: Parsed,
        state: Json,
        answer: QuestionAnswer,
        cx: &'a Cx,
    ) -> BoxFuture<'a, Called> {
        Box::pin(async move {
            let Some(input) = input.take::<T::In>() else {
                return Called::failed("invalid question tool input".into());
            };
            let state = match state.parse::<T::State>() {
                Ok(state) => state,
                Err(e) => return Called::failed(e.to_string()),
            };
            Self::called(&input, self.0.answer(&input, state, answer, cx).await)
        })
    }
}
