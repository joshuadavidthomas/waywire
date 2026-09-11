use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::anyhow;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::CloseFrame;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use futures_util::SinkExt;
use futures_util::StreamExt;
use futures_util::stream::SplitSink;
use nix::time::ClockId;
use nix::time::clock_gettime;
use rust_embed::Embed;
use sprite_desktop_protocol::Record;
use sprite_desktop_protocol::browser::ClientEvent;
use sprite_desktop_protocol::browser::ClientMessage;
use sprite_desktop_protocol::browser::ControlState;
use sprite_desktop_protocol::browser::QualityLevels;
use sprite_desktop_protocol::browser::VideoSample;
use sprite_desktop_protocol::browser::parse_browser_record;
use sprite_desktop_protocol::pipe::Command;
use sprite_desktop_protocol::pipe::H264_PROFILE;
use sprite_desktop_protocol::pipe::MAX_CLIPBOARD_BYTES;
use sprite_desktop_protocol::pipe::Text as TextCommand;
use thiserror::Error;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::TryAcquireError;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::Origin;
use crate::daemon::AppEvents;
use crate::daemon::Readiness;
use crate::session::FeedbackOutcome;
use crate::session::InputOutcome;
use crate::session::LeaseState;
use crate::session::Sessions;
use crate::session::SocketId;
use crate::video::VideoHub;

const WRITE_LIMIT: Duration = Duration::from_secs(5);
// A byte can expand to a six-byte `\\uXXXX` JSON escape.
const JSON_ESCAPE_WORST_CASE: usize = 6;
const MAX_CONTROL_MESSAGE_BYTES: usize = JSON_ESCAPE_WORST_CASE * MAX_CLIPBOARD_BYTES + 4096;
const OUTBOUND_MESSAGE_LIMIT: usize = 32;
const OUTBOUND_BYTE_LIMIT: usize = 2 * MAX_CONTROL_MESSAGE_BYTES;
// 32 control sockets permit about 384 MiB of outbound buffers per process.
const MAX_UPGRADED_CONNECTIONS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Audience {
    Everyone,
    LeaseOwner,
}

trait ClientEventAudience {
    fn audience(&self) -> Audience;
}

impl ClientEventAudience for ClientEvent {
    fn audience(&self) -> Audience {
        match self {
            ClientEvent::Clipboard { .. } => Audience::LeaseOwner,
            ClientEvent::Cursor(_)
            | ClientEvent::ResizeApplied(_)
            | ClientEvent::VideoConfig { .. }
            | ClientEvent::ControlState { .. }
            | ClientEvent::Pong { .. }
            | ClientEvent::Quality(_) => Audience::Everyone,
        }
    }
}

#[derive(Debug, Error)]
#[error("{primary:#}")]
struct SocketFailure {
    #[source]
    primary: anyhow::Error,
    cleanup: Option<anyhow::Error>,
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
enum SocketEnd {
    Shutdown,
    ClientClosed(Option<CloseFrame>),
    Disconnected,
    Failed(SocketFailure),
}

impl SocketEnd {
    fn failed(error: impl Into<anyhow::Error>) -> Self {
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

#[derive(Embed)]
#[folder = "../../apps/web/dist/"]
struct Assets;

#[derive(Clone)]
pub(crate) struct SocketConnections {
    permits: Arc<Semaphore>,
    tasks: TaskTracker,
    cancellation: CancellationToken,
}

struct SocketAdmission {
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

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) readiness: Readiness,
    pub(crate) events: AppEvents,
    pub(crate) sessions: Sessions,
    pub(crate) hub: VideoHub,
    pub(crate) origin: Origin,
    pub(crate) connections: SocketConnections,
    pub(crate) next_socket_id: Arc<AtomicU64>,
}

impl AppState {
    fn allocate_socket_id(&self) -> SocketId {
        SocketId::new(self.next_socket_id.fetch_add(1, Ordering::Relaxed))
    }
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route(
            "/stream",
            get(
                |State(state): State<AppState>,
                 headers: HeaderMap,
                 upgrade: WebSocketUpgrade| async move {
                    upgrade_socket(state, &headers, upgrade, SocketKind::Stream)
                },
            ),
        )
        .route(
            "/control",
            get(
                |State(state): State<AppState>,
                 headers: HeaderMap,
                 upgrade: WebSocketUpgrade| async move {
                    upgrade_socket(state, &headers, upgrade, SocketKind::Control)
                },
            ),
        )
        .fallback(get(asset))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Response {
    if state.readiness.is_ready() {
        response(
            StatusCode::OK,
            "text/plain; charset=utf-8",
            b"ok\n".to_vec(),
        )
    } else {
        response(
            StatusCode::SERVICE_UNAVAILABLE,
            "text/plain; charset=utf-8",
            b"unavailable\n".to_vec(),
        )
    }
}

#[derive(Clone, Copy)]
enum SocketKind {
    Stream,
    Control,
}

fn upgrade_socket(
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

async fn stream_socket(
    mut socket: WebSocket,
    state: AppState,
    socket_id: SocketId,
    _admission: SocketAdmission,
) {
    info!(socket_id = socket_id.get(), "stream socket opened");
    let reason = run_stream_socket(&mut socket, &state).await;
    log_socket_close("stream", socket_id, &reason);
}

async fn run_stream_socket(socket: &mut WebSocket, state: &AppState) -> SocketEnd {
    let configuration = ClientEvent::video_config(H264_PROFILE.codec());
    let configuration = match client_event_message(&configuration) {
        Ok(message) => message,
        Err(error) => return error.into(),
    };
    if let Err(error) = send(socket, configuration, &state.connections.cancellation).await {
        return error;
    }

    let mut subscription = state.hub.subscribe();
    loop {
        tokio::select! {
            () = state.connections.cancellation.cancelled() => {
                return SocketEnd::Shutdown;
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(frame))) => {
                        return SocketEnd::ClientClosed(frame);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return SocketEnd::failed(
                            anyhow::Error::new(error).context("WebSocket read failed"),
                        );
                    }
                    None => return SocketEnd::Disconnected,
                }
            }
            frame = subscription.next() => {
                if let Err(reason) = send_video(
                    socket,
                    &frame,
                    &state.connections.cancellation,
                ).await {
                    return reason;
                }
            }
        }
    }
}

struct OutboundItem {
    message: Message,
    bytes: OwnedSemaphorePermit,
}

#[derive(Debug, Error)]
enum OutboundError {
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
struct Outbound {
    tx: mpsc::Sender<OutboundItem>,
    bytes: Arc<Semaphore>,
}

impl Outbound {
    fn enqueue(&self, message: Message) -> Result<(), OutboundError> {
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

    fn enqueue_event(&self, event: &ClientEvent) -> Result<(), OutboundError> {
        self.enqueue(client_event_message(event)?)
    }
}

fn client_event_message(event: &ClientEvent) -> Result<Message, OutboundError> {
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
enum WriterFailure {
    #[error("WebSocket write failed")]
    Write(#[source] axum::Error),
    #[error("WebSocket write timed out after {limit:?}", limit = WRITE_LIMIT)]
    Timeout,
}

async fn control_writer(
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

async fn control_socket(
    socket: WebSocket,
    state: AppState,
    socket_id: SocketId,
    _admission: SocketAdmission,
) {
    info!(socket_id = socket_id.get(), "control socket opened");
    let reason = run_control_socket(socket, &state, socket_id).await;
    log_socket_close("control", socket_id, &reason);
}

fn log_socket_close(kind: &'static str, socket_id: SocketId, reason: &SocketEnd) {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lease {
    Held,
    NotHeld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriterStatus {
    Running,
    Finished,
}

#[expect(
    clippy::too_many_lines,
    reason = "the exhaustive socket select keeps event ordering and cancellation in one loop"
)]
async fn run_control_socket(socket: WebSocket, state: &AppState, socket_id: SocketId) -> SocketEnd {
    let (snapshot, mut events) = state.events.subscribe();
    let cancellation = state.connections.cancellation.child_token();
    let (sink, mut incoming) = socket.split();
    let (sender, receiver) = mpsc::channel(OUTBOUND_MESSAGE_LIMIT);
    let outbound = Outbound {
        tx: sender,
        bytes: Arc::new(Semaphore::new(OUTBOUND_BYTE_LIMIT)),
    };
    let mut writer = tokio::spawn(control_writer(sink, receiver, cancellation.clone()));
    let mut writer_status = WriterStatus::Running;
    let mut lease = Lease::NotHeld;

    let reason = 'connected: {
        if let Err(error) = enqueue_snapshot(
            &outbound,
            snapshot,
            state.sessions.quality(),
            Lease::NotHeld,
        ) {
            break 'connected error.into();
        }

        loop {
            tokio::select! {
                biased;
                () = state.connections.cancellation.cancelled() => break 'connected SocketEnd::Shutdown,
                writer_result = &mut writer => {
                    writer_status = WriterStatus::Finished;
                    break 'connected match writer_result {
                        Ok(Ok(())) => SocketEnd::Shutdown,
                        Ok(Err(error)) => SocketEnd::failed(error),
                        Err(error) => SocketEnd::failed(
                            anyhow::Error::new(error).context("control writer task failed"),
                        ),
                    };
                }
                incoming_result = incoming.next() => {
                    let message = match incoming_result {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => break 'connected SocketEnd::failed(
                            anyhow::Error::new(error).context("WebSocket read failed"),
                        ),
                        None => break 'connected SocketEnd::Disconnected,
                    };
                    let result = match message {
                        Message::Text(text) => {
                            handle_text(&outbound, state, socket_id, &mut lease, text.as_str()).await
                        }
                        Message::Binary(bytes) => {
                            let command = parse_browser_record(&bytes).map_err(|error| {
                                SocketEnd::failed(
                                    anyhow::Error::new(error)
                                        .context("browser control parse failed"),
                                )
                            });
                            match command {
                                Ok(command) => match lease {
                                    Lease::NotHeld => Ok(()),
                                    Lease::Held => match state.sessions.input(socket_id, command).await {
                                        Ok(InputOutcome::Sent) => Ok(()),
                                        Ok(InputOutcome::NotLeaseOwner) => {
                                            lease = Lease::NotHeld;
                                            warn!(
                                                socket_id = socket_id.get(),
                                                "control socket input lease disagreed with session owner"
                                            );
                                            Ok(())
                                        }
                                        Err(error) => Err(SocketEnd::failed(
                                            error.context("send binary input"),
                                        )),
                                    },
                                },
                                Err(error) => Err(error),
                            }
                        }
                        Message::Close(frame) => break 'connected SocketEnd::ClientClosed(frame),
                        Message::Ping(_) | Message::Pong(_) => Ok(()),
                    };
                    if let Err(error) = result {
                        break 'connected error;
                    }
                }
                event_result = events.recv() => {
                    match event_result {
                        Ok(event) => match (event.audience(), lease) {
                            (Audience::Everyone, Lease::Held | Lease::NotHeld)
                            | (Audience::LeaseOwner, Lease::Held) => {
                                if let Err(error) = outbound.enqueue_event(&event) {
                                    break 'connected error.into();
                                }
                            }
                            (Audience::LeaseOwner, Lease::NotHeld) => {}
                        },
                        Err(broadcast::error::RecvError::Lagged(count)) => {
                            debug!(lagged_events = count, "control event receiver lagged");
                            if let Err(error) = enqueue_snapshot(
                                &outbound,
                                state.events.snapshot(),
                                state.sessions.quality(),
                                lease,
                            ) {
                                break 'connected error.into();
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break 'connected
                            SocketEnd::failed(anyhow!("control event broadcaster closed")),
                    }
                }
            }
        }
    };

    finish_control_socket(
        reason,
        state,
        socket_id,
        cancellation,
        writer_status,
        &mut writer,
    )
    .await
}

fn enqueue_snapshot(
    outbound: &Outbound,
    snapshot: Vec<ClientEvent>,
    quality: QualityLevels,
    lease: Lease,
) -> Result<(), OutboundError> {
    for event in snapshot {
        match (event.audience(), lease) {
            (Audience::Everyone, Lease::Held | Lease::NotHeld)
            | (Audience::LeaseOwner, Lease::Held) => outbound.enqueue_event(&event)?,
            (Audience::LeaseOwner, Lease::NotHeld) => {}
        }
    }
    outbound.enqueue_event(&ClientEvent::Quality(quality))
}

#[derive(Debug, Error)]
enum CleanupFailure {
    #[error("release input lease")]
    Release(#[source] anyhow::Error),
    #[error("control writer cleanup")]
    Writer(#[source] anyhow::Error),
    #[error("release input lease; control writer cleanup also failed: {writer:#}")]
    Both {
        #[source]
        release: anyhow::Error,
        writer: anyhow::Error,
    },
}

async fn finish_control_socket(
    mut reason: SocketEnd,
    state: &AppState,
    socket_id: SocketId,
    cancellation: CancellationToken,
    writer_status: WriterStatus,
    writer: &mut JoinHandle<Result<(), WriterFailure>>,
) -> SocketEnd {
    cancellation.cancel();
    let release_error = state.sessions.release(socket_id).await.err();

    let writer_error = match writer_status {
        WriterStatus::Finished => None,
        WriterStatus::Running => match timeout(WRITE_LIMIT, &mut *writer).await {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(error))) => Some(anyhow::Error::new(error)),
            Ok(Err(error)) => Some(anyhow::Error::new(error).context("control writer task failed")),
            Err(_elapsed) => {
                writer.abort();
                let _ = writer.await;
                Some(anyhow!(
                    "control writer did not stop within {WRITE_LIMIT:?}"
                ))
            }
        },
    };
    let cleanup_error = match (release_error, writer_error) {
        (Some(release), Some(writer)) => {
            Some(anyhow::Error::new(CleanupFailure::Both { release, writer }))
        }
        (Some(error), None) => Some(anyhow::Error::new(CleanupFailure::Release(error))),
        (None, Some(error)) => Some(anyhow::Error::new(CleanupFailure::Writer(error))),
        (None, None) => None,
    };

    let Some(cleanup_error) = cleanup_error else {
        return reason;
    };
    match &mut reason {
        SocketEnd::Failed(failure) => failure.cleanup = Some(cleanup_error),
        SocketEnd::Shutdown | SocketEnd::ClientClosed(_) | SocketEnd::Disconnected => warn!(
            socket_id = socket_id.get(),
            reason = %format_args!("{cleanup_error:#}"),
            "socket cleanup failed"
        ),
    }
    reason
}

#[expect(
    clippy::too_many_lines,
    reason = "one exhaustive match keeps the complete browser message protocol visible"
)]
async fn handle_text(
    outbound: &Outbound,
    state: &AppState,
    socket_id: SocketId,
    lease: &mut Lease,
    text: &str,
) -> Result<(), SocketEnd> {
    let message = ClientMessage::parse_json(text.as_bytes()).map_err(|error| {
        SocketEnd::failed(anyhow::Error::new(error).context("browser control parse failed"))
    })?;
    match message {
        ClientMessage::AcquireControl => {
            let control_state = match state
                .sessions
                .acquire(socket_id)
                .await
                .map_err(|error| SocketEnd::failed(error.context("acquire input lease")))?
            {
                LeaseState::Active => {
                    *lease = Lease::Held;
                    ControlState::Active
                }
                LeaseState::Busy => {
                    *lease = Lease::NotHeld;
                    ControlState::Busy
                }
            };
            outbound
                .enqueue_event(&ClientEvent::ControlState {
                    state: control_state,
                })
                .map_err(SocketEnd::from)?;
            if control_state == ControlState::Active
                && let Some(text) = state.events.latest_clipboard()
            {
                outbound
                    .enqueue_event(&ClientEvent::Clipboard { text })
                    .map_err(SocketEnd::from)?;
            }
            Ok(())
        }
        ClientMessage::ReleaseControl => {
            state
                .sessions
                .release(socket_id)
                .await
                .map_err(|error| SocketEnd::failed(error.context("release input lease")))?;
            *lease = Lease::NotHeld;
            outbound
                .enqueue_event(&ClientEvent::ControlState {
                    state: ControlState::Ready,
                })
                .map_err(SocketEnd::from)
        }
        ClientMessage::Ping { id: request_id } => {
            let time = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(|error| {
                SocketEnd::failed(anyhow::Error::new(error).context("read monotonic clock"))
            })?;
            let nanos = i128::from(time.tv_sec()) * 1_000_000_000 + i128::from(time.tv_nsec());
            outbound
                .enqueue_event(&ClientEvent::Pong {
                    id: request_id,
                    server_nanos: nanos.to_string(),
                })
                .map_err(SocketEnd::from)
        }
        ClientMessage::Feedback(feedback) => {
            let outcome = state
                .sessions
                .feedback(socket_id, feedback)
                .await
                .map_err(|error| SocketEnd::failed(error.context("apply quality feedback")))?;
            match outcome {
                FeedbackOutcome::Applied(levels) => outbound
                    .enqueue_event(&ClientEvent::Quality(levels))
                    .map_err(SocketEnd::from),
                FeedbackOutcome::NotLeaseOwner => Ok(()),
            }
        }
        ClientMessage::Text {
            action,
            text,
            sequence,
        } => match *lease {
            Lease::NotHeld => Ok(()),
            Lease::Held => {
                let outcome = state
                    .sessions
                    .input(
                        socket_id,
                        Command::Text(TextCommand {
                            action,
                            sequence,
                            text,
                        }),
                    )
                    .await
                    .map_err(|error| SocketEnd::failed(error.context("send text input")))?;
                match outcome {
                    InputOutcome::Sent => Ok(()),
                    InputOutcome::NotLeaseOwner => {
                        *lease = Lease::NotHeld;
                        warn!(
                            socket_id = socket_id.get(),
                            "control socket input lease disagreed with session owner"
                        );
                        Ok(())
                    }
                }
            }
        },
        ClientMessage::ClipboardWrite { text } => match *lease {
            Lease::NotHeld => Ok(()),
            Lease::Held => {
                let outcome = state
                    .sessions
                    .input(socket_id, Command::Clipboard(text))
                    .await
                    .map_err(|error| SocketEnd::failed(error.context("send clipboard input")))?;
                match outcome {
                    InputOutcome::Sent => Ok(()),
                    InputOutcome::NotLeaseOwner => {
                        *lease = Lease::NotHeld;
                        warn!(
                            socket_id = socket_id.get(),
                            "control socket input lease disagreed with session owner"
                        );
                        Ok(())
                    }
                }
            }
        },
    }
}

async fn send_video(
    socket: &mut WebSocket,
    frame: &VideoSample,
    cancellation: &CancellationToken,
) -> Result<(), SocketEnd> {
    let bytes = frame.encode();
    send(socket, Message::Binary(bytes.into()), cancellation).await
}

async fn send(
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

async fn asset(axum::extract::OriginalUri(uri): axum::extract::OriginalUri) -> Response {
    let path = if uri.path() == "/" {
        "index.html"
    } else {
        uri.path().trim_start_matches('/')
    };
    let Some(file) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let extension = Path::new(path).extension().and_then(|value| value.to_str());
    response(
        StatusCode::OK,
        mime_for_extension(extension),
        file.data.into_owned(),
    )
}

fn mime_for_extension(extension: Option<&str>) -> &'static str {
    match extension {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("json" | "map") => "application/json; charset=utf-8",
        Some(_) | None => "application/octet-stream",
    }
}

fn response(status: StatusCode, mime: &'static str, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self' ws: wss:; script-src 'self'; \
             style-src 'self'; img-src 'self' data:",
        ),
    );
    response
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

    #[test]
    fn combined_cleanup_error_renders_release_and_writer_failures() {
        let error = anyhow::Error::new(CleanupFailure::Both {
            release: anyhow!("release failed"),
            writer: anyhow!("writer failed"),
        });
        let rendered = format!("{error:#}");

        assert!(rendered.contains("release failed"));
        assert!(rendered.contains("writer failed"));
    }

    #[test]
    fn vite_asset_extensions_have_nosniff_compatible_mime_types() {
        assert_eq!(mime_for_extension(Some("svg")), "image/svg+xml");
        assert_eq!(mime_for_extension(Some("png")), "image/png");
        assert_eq!(mime_for_extension(Some("ico")), "image/x-icon");
        assert_eq!(mime_for_extension(Some("woff2")), "font/woff2");
        assert_eq!(
            mime_for_extension(Some("json")),
            "application/json; charset=utf-8"
        );
        assert_eq!(
            mime_for_extension(Some("map")),
            "application/json; charset=utf-8"
        );
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
