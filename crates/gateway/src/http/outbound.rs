use std::sync::Arc;

use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use futures_util::SinkExt;
use futures_util::stream::SplitSink;
use thiserror::Error;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::TryAcquireError;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use waywire_protocol::browser::ClientEvent;
use waywire_protocol::pipe::MAX_CLIPBOARD_BYTES;

use super::socket::SocketEnd;
use super::socket::WRITE_LIMIT;

// A byte can expand to a six-byte `\\uXXXX` JSON escape.
const JSON_ESCAPE_WORST_CASE: usize = 6;
pub(super) const MAX_CONTROL_MESSAGE_BYTES: usize =
    JSON_ESCAPE_WORST_CASE * MAX_CLIPBOARD_BYTES + 4096;
pub(super) const OUTBOUND_MESSAGE_LIMIT: usize = 32;
pub(super) const OUTBOUND_BYTE_LIMIT: usize = 2 * MAX_CONTROL_MESSAGE_BYTES;

pub(super) struct OutboundItem {
    pub(super) message: Message,
    bytes: OwnedSemaphorePermit,
}

#[derive(Debug, Error)]
pub(super) enum OutboundError {
    #[error("outbound byte budget is exhausted")]
    ByteBudgetExhausted,
    #[error("outbound byte budget is closed")]
    ByteBudgetClosed,
    #[error("outbound message queue is full")]
    MessageQueueFull,
    #[error("outbound message queue is closed")]
    MessageQueueClosed,
    #[error("client event serialization failed: {0}")]
    Serialization(#[source] serde_json::Error),
}

impl From<OutboundError> for SocketEnd {
    fn from(error: OutboundError) -> Self {
        Self::failed(error)
    }
}

#[derive(Clone)]
pub(super) struct Outbound {
    pub(super) tx: mpsc::Sender<OutboundItem>,
    pub(super) bytes: Arc<Semaphore>,
}

impl Outbound {
    pub(super) fn enqueue(&self, message: Message) -> Result<(), OutboundError> {
        let size = message_size(&message);
        let permits = u32::try_from(size).unwrap_or(u32::MAX);
        let bytes = Arc::clone(&self.bytes)
            .try_acquire_many_owned(permits)
            .map_err(|error| match error {
                TryAcquireError::NoPermits => OutboundError::ByteBudgetExhausted,
                TryAcquireError::Closed => OutboundError::ByteBudgetClosed,
            })?;
        self.tx
            .try_send(OutboundItem { message, bytes })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => OutboundError::MessageQueueFull,
                mpsc::error::TrySendError::Closed(_) => OutboundError::MessageQueueClosed,
            })
    }

    pub(super) fn enqueue_event(&self, event: &ClientEvent) -> Result<(), OutboundError> {
        self.enqueue(client_event_message(event)?)
    }
}

pub(super) fn client_event_message(event: &ClientEvent) -> Result<Message, OutboundError> {
    let text = serde_json::to_string(event).map_err(OutboundError::Serialization)?;
    Ok(Message::Text(text.into()))
}

fn message_size(message: &Message) -> usize {
    match message {
        Message::Text(value) => value.len(),
        Message::Binary(value) | Message::Ping(value) | Message::Pong(value) => value.len(),
        Message::Close(value) => value.as_ref().map_or(0, |value| value.reason.len() + 2),
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Debug, Error)]
pub(super) enum WriterFailure {
    #[error("WebSocket write failed")]
    Write(#[source] axum::Error),
    #[error("WebSocket write timed out after {limit:?}", limit = WRITE_LIMIT)]
    Timeout,
}

pub(super) async fn control_writer(
    mut sink: SplitSink<WebSocket, Message>,
    mut receiver: mpsc::Receiver<OutboundItem>,
    cancellation: CancellationToken,
) -> Result<(), WriterFailure> {
    let cancel_when_writer_stops = CancelOnDrop(cancellation.clone());
    let result = 'writer: loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break 'writer Ok(()),
            item = receiver.recv() => {
                let Some(OutboundItem { message, bytes }) = item else {
                    break 'writer Ok(());
                };
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        drop(bytes);
                        break 'writer Ok(());
                    }
                    write_result = timeout(WRITE_LIMIT, sink.send(message)) => {
                        drop(bytes);
                        match write_result {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => break 'writer Err(WriterFailure::Write(error)),
                            Err(_elapsed) => break 'writer Err(WriterFailure::Timeout),
                        }
                    }
                }
            }
        }
    };
    drop(cancel_when_writer_stops);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outbound_queue_enforces_count_and_byte_bounds_independently() {
        let (tx, receiver_guard) = mpsc::channel(1);
        let outbound = Outbound {
            tx,
            bytes: Arc::new(Semaphore::new(10)),
        };
        assert!(outbound.enqueue(Message::Text("1".into())).is_ok());
        assert!(matches!(
            outbound.enqueue(Message::Text("2".into())),
            Err(OutboundError::MessageQueueFull)
        ));
        drop(receiver_guard);

        let (tx, receiver_guard) = mpsc::channel(2);
        let outbound = Outbound {
            tx,
            bytes: Arc::new(Semaphore::new(4)),
        };
        assert!(outbound.enqueue(Message::Text("123".into())).is_ok());
        assert!(matches!(
            outbound.enqueue(Message::Text("12".into())),
            Err(OutboundError::ByteBudgetExhausted)
        ));
        drop(receiver_guard);
    }
    #[tokio::test]
    async fn outbound_queue_returns_byte_permits_on_rejection_and_release() {
        let (tx, rx) = mpsc::channel(1);
        let bytes = Arc::new(Semaphore::new(4));
        let outbound = Outbound {
            tx,
            bytes: Arc::clone(&bytes),
        };
        drop(rx);
        assert!(matches!(
            outbound.enqueue(Message::Text("1234".into())),
            Err(OutboundError::MessageQueueClosed)
        ));
        assert_eq!(bytes.available_permits(), 4);

        let (tx, mut rx) = mpsc::channel(1);
        let outbound = Outbound {
            tx,
            bytes: Arc::clone(&bytes),
        };
        assert!(outbound.enqueue(Message::Text("123".into())).is_ok());
        assert_eq!(bytes.available_permits(), 1);
        let item = rx
            .recv()
            .await
            .expect("queued test message should remain available");
        drop(item);
        assert_eq!(bytes.available_permits(), 4);
    }
    #[test]
    fn outbound_queue_reports_a_closed_byte_budget() {
        let (sender, receiver_guard) = mpsc::channel(1);
        let bytes = Arc::new(Semaphore::new(4));
        bytes.close();
        let outbound = Outbound { tx: sender, bytes };

        assert!(matches!(
            outbound.enqueue(Message::Text("1".into())),
            Err(OutboundError::ByteBudgetClosed)
        ));
        drop(receiver_guard);
    }
}
