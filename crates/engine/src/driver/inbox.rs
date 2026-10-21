//! Durable transport: both ends acknowledge only behind their log barriers.
use super::{Actor, Arc, BoxFuture, ContextRequest, Live, Msg, Own, Ticket, oneshot};
use protocol::{Envelope, InboxRefusal};

impl Actor {
    pub(super) fn delivery_ticket(
        &mut self,
        reply: oneshot::Sender<Result<(), InboxRefusal>>,
    ) -> Ticket {
        self.tickets += 1;
        let ticket = Ticket(self.tickets);
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
                through: self.journal.durable_seq,
                requirement: super::Requirement::Durable,
            },
            |cancel| {
                Box::pin(async move {
                    let id = envelope.id.clone();
                    let result = match daemon {
                        Some(daemon) => super::unless(&cancel, daemon.deliver(envelope))
                            .await
                            .unwrap_or(Err(InboxRefusal::Interrupted)),
                        None => Err(InboxRefusal::Unavailable),
                    };
                    let message = if result == Err(InboxRefusal::Interrupted) {
                        super::Completion::DeliveryLost { ticket }
                    } else {
                        super::Completion::Delivered { id, result, ticket }
                    };
                    session.emit(fence, message);
                })
            },
        );
    }
}

impl Live {
    pub(crate) async fn enqueue(&self, envelope: Envelope) -> Result<(), InboxRefusal> {
        let (reply, result) = oneshot::channel();
        self.send(Msg::Enqueue { envelope, reply });
        result.await.unwrap_or(Err(InboxRefusal::Interrupted))
    }
}

impl Own {
    pub(super) fn send_message(
        &self,
        message: ext::SendMessage,
    ) -> BoxFuture<'_, Result<(), InboxRefusal>> {
        Box::pin(async move {
            let session = self.0.upgrade().ok_or(InboxRefusal::Interrupted)?;
            if !self.1.current(&session) {
                return Err(InboxRefusal::Interrupted);
            }
            let envelope = message.envelope(session.id, self.1.call);
            let (reply, result) = oneshot::channel();
            self.1
                .send(&session, ContextRequest::Send { envelope, reply });
            result.await.unwrap_or(Err(InboxRefusal::Interrupted))
        })
    }
}
