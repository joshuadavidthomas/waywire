use std::collections::VecDeque;
use std::future::pending;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use anyhow::Result;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tokio::time::timeout_at;
use waywire_protocol::Decoder;
use waywire_protocol::ProtocolError as DecodeError;
use waywire_protocol::browser::ClientEvent;
use waywire_protocol::browser::CursorState;
use waywire_protocol::pipe::ClipboardText;
use waywire_protocol::pipe::CursorPosition;
use waywire_protocol::pipe::Event;

use crate::video::VideoPipeline;

const PARTIAL_EVENT_DEADLINE: Duration = Duration::from_secs(2);
const READ_BUFFER_BYTES: usize = 8 * 1024;
const CURSOR_POSITION_PERIOD: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorPositionGate {
    Open,
    Quiet {
        until: Instant,
        pending: Option<CursorPosition>,
    },
}

impl CursorPositionGate {
    fn push(&mut self, position: CursorPosition, now: Instant) -> Option<CursorPosition> {
        match self {
            Self::Open => {
                *self = Self::Quiet {
                    until: now + CURSOR_POSITION_PERIOD,
                    pending: None,
                };
                Some(position)
            }
            Self::Quiet { pending, .. } => {
                *pending = Some(position);
                None
            }
        }
    }

    const fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Open => None,
            Self::Quiet { until, .. } => Some(*until),
        }
    }

    fn flush(&mut self, now: Instant) -> Option<CursorPosition> {
        match std::mem::replace(self, Self::Open) {
            Self::Open | Self::Quiet { pending: None, .. } => None,
            Self::Quiet {
                pending: Some(position),
                ..
            } => {
                *self = Self::Quiet {
                    until: now + CURSOR_POSITION_PERIOD,
                    pending: None,
                };
                Some(position)
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct AppEvents {
    tx: broadcast::Sender<ClientEvent>,
    latest_clipboard: Arc<Mutex<Option<ClipboardText>>>,
    latest_cursor: Arc<Mutex<CursorState>>,
}

impl AppEvents {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            tx,
            latest_clipboard: Arc::new(Mutex::new(None)),
            latest_cursor: Arc::new(Mutex::new(CursorState::default())),
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<ClientEvent> {
        let cursor = self
            .latest_cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let clipboard = self
            .latest_clipboard
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut values = vec![ClientEvent::Cursor(cursor)];
        if let Some(text) = clipboard {
            values.push(ClientEvent::Clipboard { text });
        }
        values
    }

    pub(crate) fn subscribe(&self) -> (Vec<ClientEvent>, broadcast::Receiver<ClientEvent>) {
        let receiver = self.tx.subscribe();
        (self.snapshot(), receiver)
    }

    pub(crate) fn latest_clipboard(&self) -> Option<ClipboardText> {
        self.latest_clipboard
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn publish(&self, value: ClientEvent) {
        let _ = self.tx.send(value);
    }

    #[cfg(test)]
    pub(crate) fn publish_for_test(&self, value: ClientEvent) {
        self.publish(value);
    }
}

pub(super) async fn read_events(
    stdout: tokio::process::ChildStdout,
    events: AppEvents,
    pipeline: VideoPipeline,
) -> Result<()> {
    let mut reader = EventReader::new(stdout);
    let mut position_gate = CursorPositionGate::Open;
    loop {
        let position_deadline = position_gate.deadline();
        tokio::select! {
            event = reader.next() => {
                let event = event?;
                match event {
                    Event::Frame(metadata) => pipeline.metadata(metadata).await?,
                    Event::Clipboard(text) => {
                        *events
                            .latest_clipboard
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(text.clone());
                        events.publish(ClientEvent::Clipboard { text });
                    }
                    Event::ResizeApplied(applied) => {
                        events.publish(ClientEvent::ResizeApplied(applied));
                    }
                    Event::CursorShape(shape) => {
                        publish_cursor(&events, |cursor| cursor.shape = shape);
                    }
                    Event::CursorVisibility(visibility) => {
                        publish_cursor(&events, |cursor| cursor.visibility = visibility);
                    }
                    Event::CursorPosition(position) => {
                        if let Some(position) = position_gate.push(position, Instant::now()) {
                            publish_cursor(&events, |cursor| cursor.position = Some(position));
                        }
                    }
                }
            }
            () = async move {
                match position_deadline {
                    Some(until) => sleep_until(until).await,
                    None => pending::<()>().await,
                }
            } => {
                if let Some(position) = position_gate.flush(Instant::now()) {
                    publish_cursor(&events, |cursor| cursor.position = Some(position));
                }
            }
        }
    }
}

fn publish_cursor<F>(events: &AppEvents, update: F)
where
    F: FnOnce(&mut CursorState),
{
    let event = {
        let mut cursor = events
            .latest_cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        update(&mut cursor);
        ClientEvent::Cursor(cursor.clone())
    };
    events.publish(event);
}

#[derive(Debug, Error)]
enum EventReadError {
    #[error("event pipe closed at an event boundary")]
    Closed,
    #[error("peer output ended halfway through an event")]
    Truncated,
    #[error("peer output stalled for {after:?} halfway through an event")]
    Stalled { after: Duration },
    #[error("event pipe failed")]
    Io(#[source] io::Error),
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

struct EventReader<R> {
    input: R,
    decoder: Decoder<Event>,
    ready: VecDeque<Event>,
    partial_since: Option<Instant>,
}

impl<R: AsyncRead + Unpin> EventReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            decoder: Decoder::new(),
            ready: VecDeque::new(),
            partial_since: None,
        }
    }

    /// Cancellation-safe: partial bytes live in the decoder and their deadline lives in
    /// `partial_since`, never in the future returned by this call.
    async fn next(&mut self) -> Result<Event, EventReadError> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(event);
            }
            let mut bytes = [0; READ_BUFFER_BYTES];
            let count = match self.partial_since {
                Some(partial_since) => timeout_at(
                    partial_since + PARTIAL_EVENT_DEADLINE,
                    self.input.read(&mut bytes),
                )
                .await
                .map_err(|_elapsed| EventReadError::Stalled {
                    after: partial_since.elapsed(),
                })?
                .map_err(EventReadError::Io)?,
                None => self
                    .input
                    .read(&mut bytes)
                    .await
                    .map_err(EventReadError::Io)?,
            };
            if count == 0 {
                return if self.decoder.pending() == 0 {
                    Err(EventReadError::Closed)
                } else {
                    Err(EventReadError::Truncated)
                };
            }
            let was_pending = self.decoder.pending();
            let decoded = self.decoder.push(&bytes[..count])?;
            let completed_record = !decoded.is_empty();
            self.ready.extend(decoded);
            match self.decoder.pending() {
                0 => self.partial_since = None,
                _ if was_pending == 0 || completed_record => {
                    // A decoded record followed by trailing bytes means this read completed the
                    // old record and started a new one. Anchor the deadline to the new record.
                    self.partial_since = Some(Instant::now());
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::time::timeout;
    use waywire_protocol::Record;
    use waywire_protocol::pipe::CursorPosition;

    use super::*;

    fn event(x: i32) -> Event {
        Event::CursorPosition(CursorPosition { x, y: x + 1 })
    }

    #[tokio::test]
    async fn split_record_across_two_writes() {
        let (mut writer, input) = tokio::io::duplex(1024);
        let encoded = event(1).encode();
        let split = encoded.len() / 2;
        writer
            .write_all(&encoded[..split])
            .await
            .expect("first event part should write");
        let write_tail = tokio::spawn(async move {
            tokio::task::yield_now().await;
            writer
                .write_all(&encoded[split..])
                .await
                .expect("second event part should write");
        });
        let mut reader = EventReader::new(input);

        assert_eq!(
            reader.next().await.expect("split event should decode"),
            event(1)
        );
        write_tail.await.expect("tail writer should run");
    }

    #[tokio::test]
    async fn two_records_arrive_in_one_write() {
        let (mut writer, input) = tokio::io::duplex(1024);
        let mut encoded = event(1).encode();
        encoded.extend_from_slice(&event(2).encode());
        writer
            .write_all(&encoded)
            .await
            .expect("event pair should write");
        let mut reader = EventReader::new(input);

        assert_eq!(
            reader.next().await.expect("first event should decode"),
            event(1)
        );
        assert_eq!(
            reader.next().await.expect("second event should decode"),
            event(2)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completed_record_and_trailing_partial_get_separate_deadlines() {
        let (mut writer, input) = tokio::io::duplex(1024);
        let first = event(1).encode();
        let second = event(2).encode();
        let first_split = first.len() / 2;
        let second_split = second.len() / 2;
        writer
            .write_all(&first[..first_split])
            .await
            .expect("first partial event should write");
        let write_rest = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut batch = first[first_split..].to_vec();
            batch.extend_from_slice(&second[..second_split]);
            writer
                .write_all(&batch)
                .await
                .expect("completed first event and partial second event should write together");
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            writer
                .write_all(&second[second_split..])
                .await
                .expect("second event tail should write before its own deadline");
        });
        let mut reader = EventReader::new(input);

        assert_eq!(
            reader.next().await.expect("first event should decode"),
            event(1)
        );
        assert_eq!(
            reader
                .next()
                .await
                .expect("second event should get a fresh partial deadline"),
            event(2)
        );
        write_rest.await.expect("writer task should run");
    }

    #[tokio::test]
    async fn closed_at_record_boundary_is_typed() {
        let (writer, input) = tokio::io::duplex(1024);
        drop(writer);
        let mut reader = EventReader::new(input);

        assert!(matches!(reader.next().await, Err(EventReadError::Closed)));
    }

    #[tokio::test]
    async fn partial_header_at_eof_is_truncated() {
        let (mut writer, input) = tokio::io::duplex(1024);
        writer
            .write_all(&event(1).encode()[..1])
            .await
            .expect("partial header should write");
        drop(writer);
        let mut reader = EventReader::new(input);

        assert!(matches!(
            reader.next().await,
            Err(EventReadError::Truncated)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn partial_record_stalls_at_anchored_deadline() {
        let (mut writer, input) = tokio::io::duplex(1024);
        let start = Instant::now();
        writer
            .write_all(&event(1).encode()[..1])
            .await
            .expect("partial header should write");
        let mut reader = EventReader::new(input);

        let error = reader
            .next()
            .await
            .expect_err("partial record should reach its deadline");
        match error {
            EventReadError::Stalled { .. } => {}
            EventReadError::Closed
            | EventReadError::Truncated
            | EventReadError::Io(_)
            | EventReadError::Decode(_) => {
                panic!("partial record should return the stalled error, got {error:?}");
            }
        }
        assert_eq!(start.elapsed(), PARTIAL_EVENT_DEADLINE);
        drop(writer);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_next_does_not_reset_partial_deadline() {
        let (mut writer, input) = tokio::io::duplex(1024);
        let start = Instant::now();
        writer
            .write_all(&event(1).encode()[..1])
            .await
            .expect("partial header should write");
        let mut reader = EventReader::new(input);

        assert!(
            timeout(Duration::from_secs(1), reader.next())
                .await
                .is_err()
        );
        let error = reader
            .next()
            .await
            .expect_err("partial record should stall at its original deadline");
        match error {
            EventReadError::Stalled { .. } => {}
            EventReadError::Closed
            | EventReadError::Truncated
            | EventReadError::Io(_)
            | EventReadError::Decode(_) => {
                panic!("cancelled partial record should return stalled, got {error:?}");
            }
        }
        assert_eq!(start.elapsed(), PARTIAL_EVENT_DEADLINE);
        drop(writer);
    }

    #[test]
    fn cursor_position_gate_publishes_first_and_latest_without_idle_deadline() {
        let now = Instant::now();
        let first = CursorPosition { x: 1, y: 2 };
        let replaced = CursorPosition { x: 3, y: 4 };
        let latest = CursorPosition { x: 5, y: 6 };
        let mut gate = CursorPositionGate::Open;

        assert_eq!(gate.push(first, now), Some(first));
        assert_eq!(gate.deadline(), Some(now + CURSOR_POSITION_PERIOD));
        assert_eq!(gate.push(replaced, now), None);
        assert_eq!(gate.push(latest, now), None);
        let first_deadline = gate.deadline().expect("quiet gate should have a deadline");
        assert_eq!(gate.flush(first_deadline), Some(latest));
        assert_eq!(
            gate.deadline(),
            Some(first_deadline + CURSOR_POSITION_PERIOD)
        );
        let final_deadline = gate
            .deadline()
            .expect("rearmed gate should have a deadline");
        assert_eq!(gate.flush(final_deadline), None);
        assert_eq!(gate.deadline(), None);
    }
}
