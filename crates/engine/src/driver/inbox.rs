//! Durable transport: both ends acknowledge only behind their log barriers.
use super::{Actor, Arc, BoxFuture, CallPorts, ContextRequest, Live, Msg, Ticket, oneshot};
use protocol::{Envelope, InboxRefusal};

impl Actor {
    pub(super) async fn envelope_reply(
        &mut self,
        id: protocol::EnvelopeId,
        reply: oneshot::Sender<super::Opened>,
    ) {
        if self.indexed().await.is_err() {
            return;
        }
        let Some(input) = self.journal.index.enqueued(&id) else {
            return;
        };
        self.await_reply(input, super::Waiter::Model(reply)).await;
    }

    pub(super) fn delivery_ticket(
        &mut self,
        reply: oneshot::Sender<Result<(), InboxRefusal>>,
    ) -> Ticket {
        let ticket = self.ticket();
        self.replies.deliveries.insert(ticket, reply);
        ticket
    }
    pub(super) fn deliver(&mut self, envelope: Envelope, ticket: Option<Ticket>) {
        self.delivery_queue.push_back((envelope, ticket));
        self.pump_delivery();
    }
    pub(super) fn pump_delivery(&mut self) {
        if self.transport_active {
            return;
        }
        let Some((envelope, ticket)) = self.delivery_queue.pop_front() else {
            return;
        };
        self.transport_active = true;
        let session = Arc::clone(&self.live.session);
        let daemon = session.shared.daemon.upgrade();
        let fence = session.fence.load(super::Ordering::SeqCst);
        self.live.session.spawn_work(
            super::WorkId::Delivery,
            super::Admission {
                through: self.journal.durable.seq(),
                requirement: super::Requirement::Durable,
            },
            |cancel| {
                Box::pin(async move {
                    let (id, recipient) = (envelope.id().clone(), envelope.destination);
                    let result = match daemon {
                        Some(daemon) => super::unless(&cancel, daemon.deliver(envelope))
                            .await
                            .unwrap_or(Err(InboxRefusal::Interrupted)),
                        None => Err(InboxRefusal::Unavailable),
                    };
                    let message = if result == Err(InboxRefusal::Interrupted) {
                        super::Completion::DeliveryLost { ticket }
                    } else {
                        super::Completion::Delivered {
                            id,
                            recipient,
                            result,
                            ticket,
                        }
                    };
                    session.emit(fence, message);
                })
            },
        );
    }
}

impl Live {
    pub(crate) async fn exchange(
        &self,
        envelope: Envelope,
        cancel: &ext::Cancel,
    ) -> std::io::Result<(protocol::Outcome, Option<protocol::Json>)> {
        let id = envelope.id().clone();
        self.enqueue(envelope)
            .await
            .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        let (reply, result) = oneshot::channel();
        self.send(Msg::AwaitEnvelope { id, reply });
        if let Some(Some(super::Opened::Cached { outcome, failure })) =
            super::unless(cancel, result).await
        {
            Ok((outcome, failure))
        } else {
            self.cancel_owned();
            Ok((protocol::Outcome::Interrupted { output: None }, None))
        }
    }

    pub(crate) async fn enqueue(&self, envelope: Envelope) -> Result<(), InboxRefusal> {
        let (reply, result) = oneshot::channel();
        self.send(Msg::Enqueue { envelope, reply });
        result.await.unwrap_or(Err(InboxRefusal::Interrupted))
    }
}

impl CallPorts {
    pub(super) fn send_message(
        &self,
        message: ext::SendMessage,
    ) -> BoxFuture<'_, Result<(), InboxRefusal>> {
        Box::pin(async move {
            let session = &self.session;
            if !self.origin.current(session) {
                return Err(InboxRefusal::Interrupted);
            }
            let (reply, result) = oneshot::channel();
            let (id, to, payload) = message.into_parts(session.id, self.origin.id);
            let request = ContextRequest::Send {
                id,
                to,
                payload,
                reply,
            };
            self.origin.send(session, request);
            result.await.unwrap_or(Err(InboxRefusal::Interrupted))
        })
    }
}
