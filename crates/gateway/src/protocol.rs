use std::io;
use std::time::Duration;

use sprite_desktop_protocol::pipe::EVENT_HEADER_BYTES;
use sprite_desktop_protocol::pipe::Event;
use sprite_desktop_protocol::pipe::EventHeader;
use sprite_desktop_protocol::pipe::ProtocolError as DecodeError;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

const PARTIAL_EVENT_DEADLINE: Duration = Duration::from_secs(2);

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
}

impl<R: AsyncRead + Unpin> EventReader<R> {
    pub(crate) fn new(input: R) -> Self {
        Self { input }
    }

    pub(crate) async fn next(&mut self) -> Result<Option<Event>, EventReadError> {
        let mut bytes = [0; EVENT_HEADER_BYTES];
        let first = self
            .input
            .read(&mut bytes[..1])
            .await
            .map_err(EventReadError::Io)?;
        if first == 0 {
            return Ok(None);
        }
        timeout(
            PARTIAL_EVENT_DEADLINE,
            self.input.read_exact(&mut bytes[1..]),
        )
        .await
        .map_err(|_elapsed| EventReadError::Truncated)?
        .map_err(map_eof)?;
        let header = EventHeader::parse(&bytes)?;
        let mut payload = vec![0; header.payload_len()];
        timeout(PARTIAL_EVENT_DEADLINE, self.input.read_exact(&mut payload))
            .await
            .map_err(|_elapsed| EventReadError::Truncated)?
            .map_err(map_eof)?;
        Event::decode(header, &payload)
            .map(Some)
            .map_err(Into::into)
    }
}

fn map_eof(error: io::Error) -> EventReadError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        EventReadError::Truncated
    } else {
        EventReadError::Io(error)
    }
}
