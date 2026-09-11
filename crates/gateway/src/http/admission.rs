use std::sync::Arc;

use axum::extract::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use thiserror::Error;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;
use url::Url;

use super::AppState;
use super::control::control_socket;
use super::outbound::MAX_CONTROL_MESSAGE_BYTES;
use super::stream::stream_socket;

// 32 control sockets permit about 384 MiB of outbound buffers per process.
const MAX_UPGRADED_CONNECTIONS: usize = 32;

#[derive(Clone, Debug)]
pub(crate) struct Origin(Box<str>);

impl Origin {
    pub(crate) fn parse(raw: &str) -> std::result::Result<Self, OriginError> {
        let url = Url::parse(raw).map_err(OriginError::Url)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(OriginError::Scheme);
        }
        if url.host_str().is_none() {
            return Err(OriginError::MissingHost);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(OriginError::Credentials);
        }
        Ok(Self(url.origin().ascii_serialization().into()))
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Error)]
pub(crate) enum OriginError {
    #[error("origin is not a URL")]
    Url(#[source] url::ParseError),
    #[error("origin scheme must be HTTP or HTTPS")]
    Scheme,
    #[error("origin must include a host")]
    MissingHost,
    #[error("origin must not include credentials")]
    Credentials,
}

#[derive(Clone)]
pub(crate) struct SocketConnections {
    permits: Arc<Semaphore>,
    tasks: TaskTracker,
    pub(super) cancellation: CancellationToken,
}

pub(super) struct SocketAdmission {
    _task: TaskTrackerToken,
    _permit: OwnedSemaphorePermit,
}

impl SocketConnections {
    pub(crate) fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(MAX_UPGRADED_CONNECTIONS)),
            tasks: TaskTracker::new(),
            cancellation: CancellationToken::new(),
        }
    }

    fn admit(&self) -> Option<SocketAdmission> {
        // Take the tracking token first. Once shutdown closes the semaphore,
        // no untracked upgrader can cross the admission cutoff.
        let task = self.tasks.token();
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok()?;
        Some(SocketAdmission {
            _task: task,
            _permit: permit,
        })
    }

    pub(crate) fn begin_shutdown(&self) {
        self.permits.close();
        self.tasks.close();
        self.cancellation.cancel();
    }

    pub(crate) async fn wait(&self) {
        self.tasks.wait().await;
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[derive(Clone, Copy)]
pub(super) enum SocketKind {
    Stream,
    Control,
}

pub(super) fn upgrade_socket(
    state: AppState,
    headers: &HeaderMap,
    upgrade: WebSocketUpgrade,
    kind: SocketKind,
) -> Response {
    if !valid_origin(headers, &state.origin) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(admission) = state.connections.admit() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let socket_id = state.allocate_socket_id();
    match kind {
        SocketKind::Stream => upgrade
            .max_message_size(4096)
            .max_frame_size(4096)
            .on_upgrade(move |socket| stream_socket(socket, state, socket_id, admission))
            .into_response(),
        SocketKind::Control => upgrade
            .max_message_size(MAX_CONTROL_MESSAGE_BYTES)
            .max_frame_size(MAX_CONTROL_MESSAGE_BYTES)
            .on_upgrade(move |socket| control_socket(socket, state, socket_id, admission))
            .into_response(),
    }
}

fn valid_origin(headers: &HeaderMap, expected: &Origin) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return false;
    };
    origins.next().is_none()
        && origin
            .to_str()
            .is_ok_and(|value| value == expected.as_str() && value != "null")
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use axum::http::HeaderValue;
    use axum::http::header;

    use super::Origin;
    use super::valid_origin;
    #[test]
    fn canonical_origin_strips_path_and_rejects_foreign_shapes() {
        assert_eq!(
            Origin::parse("https://example.test/a")
                .expect("absolute HTTPS test URL should parse")
                .as_str(),
            "https://example.test"
        );
        assert!(Origin::parse("example.test").is_err());
        assert!(Origin::parse("https://u@example.test").is_err());
    }
    #[test]
    fn origin_requires_one_exact_value() {
        let expected = Origin::parse("https://x.test").expect("test origin should parse");
        let mut headers = HeaderMap::new();
        assert!(!valid_origin(&headers, &expected));
        headers.append(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!valid_origin(&headers, &expected));
        headers.clear();
        headers.append(header::ORIGIN, HeaderValue::from_static("https://x.test"));
        assert!(valid_origin(&headers, &expected));
        headers.append(header::ORIGIN, HeaderValue::from_static("https://x.test"));
        assert!(!valid_origin(&headers, &expected));
    }
}
