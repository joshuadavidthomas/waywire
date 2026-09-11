use std::fmt;
use std::time::Duration;

use anyhow::anyhow;
use axum::extract::ws::CloseFrame;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use thiserror::Error;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

use crate::session::SocketId;

pub(super) const WRITE_LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
#[error("{primary:#}")]
pub(super) struct SocketFailure {
    #[source]
    primary: anyhow::Error,
    pub(super) cleanup: Option<anyhow::Error>,
}

impl SocketFailure {
    fn new(primary: impl Into<anyhow::Error>) -> Self {
        Self {
            primary: primary.into(),
            cleanup: None,
        }
    }
}

#[derive(Debug)]
pub(super) enum SocketEnd {
    Shutdown,
    ClientClosed(Option<CloseFrame>),
    Disconnected,
    Failed(SocketFailure),
}

impl SocketEnd {
    pub(super) fn failed(error: impl Into<anyhow::Error>) -> Self {
        Self::Failed(SocketFailure::new(error))
    }

    #[cfg(test)]
    fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl fmt::Display for SocketEnd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shutdown => formatter.write_str("gateway shutdown"),
            Self::ClientClosed(Some(frame)) => write!(
                formatter,
                "client sent WebSocket close frame {}: {}",
                frame.code, frame.reason
            ),
            Self::ClientClosed(None) => {
                formatter.write_str("client sent a WebSocket close frame without a status")
            }
            Self::Disconnected => formatter.write_str("client disconnected without a close frame"),
            Self::Failed(error) => formatter.write_str(error.to_string().as_str()),
        }
    }
}

pub(super) fn log_socket_close(kind: &'static str, socket_id: SocketId, reason: &SocketEnd) {
    match reason {
        SocketEnd::Failed(SocketFailure {
            primary,
            cleanup: Some(cleanup),
        }) => warn!(
            socket = kind,
            socket_id = socket_id.get(),
            reason = %format_args!("{primary:#}"),
            cleanup = %format_args!("{cleanup:#}"),
            "socket closed"
        ),
        SocketEnd::Failed(SocketFailure {
            primary,
            cleanup: None,
        }) => warn!(
            socket = kind,
            socket_id = socket_id.get(),
            reason = %format_args!("{primary:#}"),
            "socket closed"
        ),
        SocketEnd::Shutdown | SocketEnd::ClientClosed(_) | SocketEnd::Disconnected => {
            info!(socket = kind, socket_id = socket_id.get(), reason = %reason, "socket closed");
        }
    }
}

pub(super) async fn send(
    socket: &mut WebSocket,
    message: Message,
    cancellation: &CancellationToken,
) -> Result<(), SocketEnd> {
    tokio::select! {
        () = cancellation.cancelled() => Err(SocketEnd::Shutdown),
        result = timeout(WRITE_LIMIT, socket.send(message)) => {
            match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(SocketEnd::failed(
                    anyhow::Error::new(error).context("WebSocket write failed"),
                )),
                Err(_elapsed) => Err(SocketEnd::failed(anyhow!(
                    "WebSocket write timed out after {WRITE_LIMIT:?}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn socket_close_reasons_keep_peer_details() {
        let peer_close = SocketEnd::ClientClosed(Some(CloseFrame {
            code: 1008,
            reason: "policy".into(),
        }));
        assert_eq!(
            peer_close.to_string(),
            "client sent WebSocket close frame 1008: policy"
        );
        assert!(!peer_close.is_failure());
        assert!(SocketEnd::failed(anyhow!("write timeout")).is_failure());
        assert!(!SocketEnd::Shutdown.is_failure());
    }
}
