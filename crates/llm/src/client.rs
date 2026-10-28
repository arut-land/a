//! Sends one request and reads its reply as a stream of events. The stream
//! ends with `Finish` or with one error, never both and never neither: a
//! body that stops before the provider finished is [`Kind::Incomplete`].
//! Nothing here retries; [`Error::is_retryable`] tells the caller whether
//! to.
//!
//! The events build the reply as they are read: each is lent to the
//! reader, then folded into the reply, so its text is never copied out.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Instant;

use crate::accumulate::Accumulator;
use crate::error::{Error, Kind};
use crate::http::{Body, BoxFuture, Http};
use crate::protocol::{Decoder, Flow, Provider, Target};
use crate::spec::{Event, Origin, Request, Response};
use crate::sse::{DecodeLimits, SseFramer};

/// Wire bytes and decoder state, without ownership of canonical history.
/// Preparation does no network I/O; dispatch consumes the prepared request.
pub struct Prepared {
    protocol: Arc<dyn crate::protocol::Protocol>,
    origin: Origin,
    outgoing: Result<(http::Request<crate::http::Payload>, Box<dyn Decoder>), Error>,
}

/// Encodes and fits a request before its caller authorizes network dispatch.
pub fn prepare_request(provider: &Provider, request: &Request) -> Prepared {
    let target = Target {
        provider,
        model: &request.model,
    };
    Prepared {
        protocol: Arc::clone(&provider.protocol),
        origin: origin(provider, &request.model),
        outgoing: provider
            .request(&request.model, request)
            .map(|http_request| (http_request, provider.protocol.decoder(&target, request))),
    }
}

impl Prepared {
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    pub fn start(self, http: &dyn Http) -> Events {
        let mut started = None;
        let state = match self.outgoing {
            Ok((request, decoder)) => {
                started = Some(Instant::now());
                State::Sending {
                    reply: http.send(request),
                    decoder,
                }
            }
            Err(error) => State::Failed(Some(error)),
        };
        Events {
            state,
            started,
            protocol: self.protocol,
            reply: Accumulator::new(),
            read: None,
        }
    }
}

/// Streams the reply to `request` from `provider`.
pub fn stream(http: &dyn Http, provider: &Provider, request: &Request) -> Events {
    prepare_request(provider, request).start(http)
}

/// The finished reply to `request`.
pub async fn generate(
    http: &dyn Http,
    provider: &Provider,
    request: &Request,
) -> Result<Response, Error> {
    let mut events = stream(http, provider, request);
    while let Some(event) = events.next().await {
        event?;
    }
    events.finish(Some(origin(provider, &request.model)))
}

/// What the reply's message names as its producer.
pub fn origin(provider: &Provider, model: &str) -> Origin {
    Origin {
        protocol: provider.protocol.key(),
        provider: provider.id.clone(),
        model: model.into(),
    }
}

pub struct Events {
    state: State,
    started: Option<Instant>,
    protocol: Arc<dyn crate::protocol::Protocol>,
    /// The reply the events read so far build.
    reply: Accumulator,
    /// The event lent to the reader, folded into `reply` at the next read.
    read: Option<Event>,
}

impl Events {
    /// A locally rejected request never crosses the HTTP send boundary.
    pub fn started(&self) -> Option<Instant> {
        self.started
    }

    #[inline(never)]
    pub fn usage_json(&self) -> Option<crate::Json> {
        let usage = match self.read.as_ref() {
            Some(Event::Finish { usage, .. }) => usage,
            _ => &self.reply.finish.as_ref()?.usage,
        };
        crate::Usage::encode_counts(&usage.counts()).ok()
    }

    /// The reply's next event, lent until the next read; `None` once the
    /// reply ended or failed.
    pub async fn next(&mut self) -> Option<Result<&Event, Error>> {
        match std::future::poll_fn(|cx| self.advance(cx)).await? {
            Ok(()) => self.read.as_ref().map(Ok),
            Err(error) => Some(Err(error)),
        }
    }

    fn advance(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<(), Error>>> {
        if let Some(event) = self.read.take()
            && let Err(error) = self.reply.push(event)
        {
            self.state = State::Over;
            return Poll::Ready(Some(Err(error)));
        }
        Poll::Ready(match ready!(self.poll_event(cx)) {
            Some(Ok(event)) => {
                self.read = Some(event);
                Some(Ok(()))
            }
            Some(Err(error)) => Some(Err(error)),
            None => None,
        })
    }

    /// The finished reply, `origin` stamped on its message.
    pub fn finish(mut self, origin: Option<Origin>) -> Result<Response, Error> {
        if let Some(event) = self.read.take() {
            self.reply.push(event)?;
        }
        self.reply.finish(origin)
    }

    /// The answer text so far of a reply its reader stopped reading:
    /// everything decoded, and what is still open as far as it got.
    pub fn interrupted(mut self) -> Option<String> {
        let mut cut = Vec::from_iter(self.read.take());
        if let State::Reading(reader) = &mut self.state {
            cut.extend(reader.queue.drain(..));
            reader.decoder.cut(&mut cut);
        }
        for event in cut {
            // An event out of order ends what can be read.
            if self.reply.push(event).is_err() {
                break;
            }
        }
        self.reply.into_text()
    }
}

enum State {
    Sending {
        reply: BoxFuture<'static, io::Result<crate::http::Response>>,
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

/// A successful reply's body, being decoded.
struct Reader {
    body: Body,
    sse: Box<SseFramer>,
    decoder: Box<dyn Decoder>,
    queue: VecDeque<Event>,
    /// What the last chunk decoded to, on its way to `queue`.
    decoded: Vec<Event>,
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

impl Events {
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Event, Error>>> {
        let this = self;
        loop {
            match &mut this.state {
                State::Sending { reply, .. } => {
                    let reply = ready!(reply.as_mut().poll(cx));
                    let State::Sending { decoder, .. } =
                        std::mem::replace(&mut this.state, State::Over)
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
                        State::Reading(Reader::new(body, decoder))
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
                            read.extend_from_slice(
                                &chunk[..chunk.len().min(64 * 1024 - read.len())],
                            );
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
    fn new(body: Body, decoder: Box<dyn Decoder>) -> Self {
        Self {
            body,
            sse: Box::new(SseFramer::with_limits(DecodeLimits::default())),
            decoder,
            queue: VecDeque::new(),
            decoded: Vec::new(),
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
            let polled = ready!(self.body.as_mut().poll_next(cx));
            let mut out = std::mem::take(&mut self.decoded);
            let step = match polled {
                Some(Ok(chunk)) => {
                    let decoder = &mut self.decoder;
                    self.sse
                        .push(&chunk, |frame| decoder.frame(frame, &mut out))
                }
                Some(Err(error)) => Err(Error::new(
                    Kind::Transport,
                    format!("the reply broke off: {error}"),
                )),
                None => {
                    self.done = true;
                    self.decoder.end(&mut out).map(|()| Flow::Done)
                }
            };
            self.queue.extend(out.drain(..));
            self.decoded = out;
            match step {
                Ok(Flow::More) => {}
                Ok(Flow::Done) => self.done = true,
                // Events decoded before the failure still reach the caller;
                // then the error ends the stream.
                Err(error) => self.failure = Some(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::responses;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted(AtomicUsize);
    impl Http for Counted {
        fn send(
            &self,
            _: http::Request<crate::http::Payload>,
        ) -> BoxFuture<'static, io::Result<crate::http::Response>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err(io::Error::other("test transport")) })
        }
    }

    #[test]
    fn locally_rejected_requests_have_no_execution_start() {
        let http = Counted(AtomicUsize::new(0));
        let provider = Provider::new(
            "openai",
            "http://recorded",
            crate::Auth::None,
            responses::Responses::new(responses::HOSTS[0].1),
        );
        let mut request = Request {
            model: "test".into(),
            messages: [crate::Message::user("hello")].into_iter().collect(),
            ..Request::default()
        };
        assert!(stream(&http, &provider, &request).started().is_some());
        assert_eq!(http.0.load(Ordering::Relaxed), 1);
        request
            .headers
            .push(("invalid\nheader".into(), "value".into()));
        assert!(stream(&http, &provider, &request).started().is_none());
        assert_eq!(http.0.load(Ordering::Relaxed), 1);
    }
}
