//! Sends one request and reads its reply as a stream of events. The stream
//! ends with `Finish` or with one error, never both and never neither: a
//! body that stops before the provider finished is [`Kind::Incomplete`].
//! Nothing here retries; [`Error::is_retryable`] tells the caller whether
//! to.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use futures_core::Stream;

use crate::accumulate::Accumulator;
use crate::error::{Error, Kind};
use crate::http::{Body, BoxFuture, Http};
use crate::prepare::prepare;
use crate::protocol::{Decoder, Flow, Frame, Framing, Provider, Target};
use crate::spec::{Event, Origin, Request, Response, SmolStr};
use crate::sse::{NdjsonFramer, SseFramer};

/// Streams the reply to `request` from `provider`.
pub fn stream(http: &dyn Http, provider: &Provider, request: &Request) -> Events {
    let target = Target {
        provider,
        model: &request.model,
    };
    let request = prepare(request.clone(), &target);
    let state = match provider.request(&request.model, &request) {
        Ok((http_request, framing)) => State::Sending {
            reply: http.send(http_request),
            framing,
            decoder: provider.protocol.decoder(&target, &request),
        },
        Err(error) => State::Failed(Some(error)),
    };
    Events {
        state,
        protocol: Arc::clone(&provider.protocol),
    }
}

/// The finished reply to `request`.
pub async fn generate(
    http: &dyn Http,
    provider: &Provider,
    request: &Request,
) -> Result<Response, Error> {
    let mut events = stream(http, provider, request);
    let mut acc = Accumulator::new();
    while let Some(event) = std::future::poll_fn(|cx| Pin::new(&mut events).poll_next(cx)).await {
        acc.push(event?)?;
    }
    acc.finish(Some(origin(provider, &request.model)))
}

/// What the reply's message names as its producer.
pub fn origin(provider: &Provider, model: &str) -> Origin {
    Origin {
        protocol: SmolStr::new_static(provider.protocol.key()),
        provider: provider.id.clone(),
        model: model.into(),
    }
}

pub struct Events {
    state: State,
    protocol: Arc<dyn crate::protocol::Protocol>,
}

enum State {
    Sending {
        reply: BoxFuture<'static, io::Result<crate::http::Response>>,
        framing: Framing,
        decoder: Box<dyn Decoder>,
    },
    Failing {
        status: u16,
        headers: http::HeaderMap,
        body: Body,
        read: Vec<u8>,
    },
    Reading(Reader),
    Failed(Option<Error>),
    Over,
}

enum Framer {
    Sse(SseFramer),
    Ndjson(NdjsonFramer),
}

impl From<Framing> for Framer {
    fn from(framing: Framing) -> Self {
        match framing {
            Framing::Sse => Self::Sse(SseFramer::new()),
            Framing::Ndjson => Self::Ndjson(NdjsonFramer::new()),
        }
    }
}

impl Framer {
    /// Decode the frames `chunk` completes, until the decoder is done.
    fn feed(
        &mut self,
        decoder: &mut dyn Decoder,
        chunk: &[u8],
        out: &mut Vec<Event>,
    ) -> Result<Flow, Error> {
        match self {
            Self::Sse(sse) => {
                for event in sse.push(chunk) {
                    let frame = Frame {
                        event: &event.event,
                        data: &event.data,
                    };
                    if decoder.frame(frame, out)? == Flow::Done {
                        return Ok(Flow::Done);
                    }
                }
            }
            Self::Ndjson(lines) => {
                for line in lines.push(chunk) {
                    if line_frame(decoder, &line, out)? == Flow::Done {
                        return Ok(Flow::Done);
                    }
                }
            }
        }
        Ok(Flow::More)
    }
}

/// A successful reply's body, being decoded.
struct Reader {
    body: Body,
    framer: Framer,
    decoder: Box<dyn Decoder>,
    queue: VecDeque<Event>,
    done: bool,
    finished: bool,
    /// A failure to report once `queue` drains.
    failure: Option<Error>,
}

enum Next {
    Event(Event),
    Fail(Error),
    Over,
}

impl Stream for Events {
    type Item = Result<Event, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            match &mut this.state {
                State::Sending { reply, .. } => {
                    let reply = ready!(reply.as_mut().poll(cx));
                    let State::Sending {
                        framing, decoder, ..
                    } = std::mem::replace(&mut this.state, State::Over)
                    else {
                        continue;
                    };
                    let (parts, body) = match reply {
                        Ok(reply) => reply.into_parts(),
                        Err(error) => {
                            this.state = State::Failed(Some(error.into()));
                            continue;
                        }
                    };
                    this.state = if parts.status.is_success() {
                        State::Reading(Reader::new(body, framing, decoder))
                    } else {
                        State::Failing {
                            status: parts.status.as_u16(),
                            headers: parts.headers,
                            body,
                            read: Vec::new(),
                        }
                    };
                }
                State::Failing {
                    status,
                    headers,
                    body,
                    read,
                } => {
                    match ready!(body.as_mut().poll_next(cx)) {
                        Some(Ok(chunk)) if read.len() < 64 * 1024 => {
                            read.extend_from_slice(&chunk);
                            continue;
                        }
                        Some(Ok(_) | Err(_)) | None => {}
                    }
                    let error = this.protocol.error(*status, headers, read);
                    this.state = State::Failed(Some(error));
                }
                State::Reading(reader) => match ready!(reader.poll(cx)) {
                    Next::Event(event) => return Poll::Ready(Some(Ok(event))),
                    Next::Fail(error) => this.state = State::Failed(Some(error)),
                    Next::Over => this.state = State::Over,
                },
                State::Failed(error) => {
                    let error = error.take();
                    this.state = State::Over;
                    return Poll::Ready(error.map(Err));
                }
                State::Over => return Poll::Ready(None),
            }
        }
    }
}

impl Reader {
    fn new(body: Body, framing: Framing, decoder: Box<dyn Decoder>) -> Self {
        Self {
            body,
            framer: framing.into(),
            decoder,
            queue: VecDeque::new(),
            done: false,
            finished: false,
            failure: None,
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Next> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                if self.finished {
                    return Poll::Ready(Next::Fail(Error::new(
                        Kind::Decode,
                        "the provider sent more after finishing the reply",
                    )));
                }
                self.finished = matches!(event, Event::Finish { .. });
                return Poll::Ready(Next::Event(event));
            }
            if let Some(error) = self.failure.take() {
                return Poll::Ready(Next::Fail(error));
            }
            if self.finished {
                return Poll::Ready(Next::Over);
            }
            if self.done {
                return Poll::Ready(Next::Fail(Error::incomplete()));
            }
            let mut out = Vec::new();
            let step = match ready!(self.body.as_mut().poll_next(cx)) {
                Some(Ok(chunk)) => self.framer.feed(self.decoder.as_mut(), &chunk, &mut out),
                Some(Err(error)) => Err(Error::new(
                    Kind::Transport,
                    format!("the reply broke off: {error}"),
                )),
                None => self.eof(&mut out),
            };
            self.queue.extend(out);
            match step {
                Ok(Flow::More) => {}
                Ok(Flow::Done) => self.done = true,
                // Events decoded before the failure still reach the caller;
                // then the error ends the stream.
                Err(error) => self.failure = Some(error),
            }
        }
    }

    fn eof(&mut self, out: &mut Vec<Event>) -> Result<Flow, Error> {
        self.done = true;
        let flushed = match &mut self.framer {
            Framer::Ndjson(lines) => lines.finish(),
            Framer::Sse(_) => None,
        };
        let last = match flushed {
            Some(line) => line_frame(self.decoder.as_mut(), &line, out)?,
            None => Flow::More,
        };
        if last == Flow::More {
            self.decoder.end(out)?;
        }
        Ok(Flow::Done)
    }
}

fn line_frame(decoder: &mut dyn Decoder, line: &[u8], out: &mut Vec<Event>) -> Result<Flow, Error> {
    let line = String::from_utf8_lossy(line);
    decoder.frame(
        Frame {
            event: "",
            data: &line,
        },
        out,
    )
}
