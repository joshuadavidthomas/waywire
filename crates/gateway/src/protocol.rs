use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use sprite_desktop_protocol::pipe::Decoder;
use sprite_desktop_protocol::pipe::Event;
use sprite_desktop_protocol::pipe::ProtocolError as DecodeError;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

const PARTIAL_EVENT_DEADLINE: Duration = Duration::from_secs(2);
const READ_BUFFER_BYTES: usize = 8 * 1024;

#[derive(Debug, Error)]
pub(crate) enum EventReadError {
    #[error("peer output ended halfway through an event")]
    Truncated,
    #[error("event pipe failed")]
    Io(#[source] io::Error),
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

pub(crate) struct EventReader<R> {
    input: R,
    decoder: Decoder<Event>,
    ready: VecDeque<Event>,
}

impl<R: AsyncRead + Unpin> EventReader<R> {
    pub(crate) fn new(input: R) -> Self {
        Self {
            input,
            decoder: Decoder::new(),
            ready: VecDeque::new(),
        }
    }

    pub(crate) async fn next(&mut self) -> Result<Option<Event>, EventReadError> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            let mut bytes = [0; READ_BUFFER_BYTES];
            let count = if self.decoder.pending() > 0 {
                timeout(PARTIAL_EVENT_DEADLINE, self.input.read(&mut bytes))
                    .await
                    .map_err(|_elapsed| EventReadError::Truncated)?
                    .map_err(EventReadError::Io)?
            } else {
                self.input
                    .read(&mut bytes)
                    .await
                    .map_err(EventReadError::Io)?
            };
            if count == 0 {
                return if self.decoder.pending() == 0 {
                    Ok(None)
                } else {
                    Err(EventReadError::Truncated)
                };
            }
            self.ready.extend(self.decoder.push(&bytes[..count])?);
        }
    }
}
