use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

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
use sprite_desktop_protocol::browser::BrowserError;
use sprite_desktop_protocol::browser::ClientEvent;
use sprite_desktop_protocol::browser::ClientMessage;
use sprite_desktop_protocol::browser::ControlState;
use sprite_desktop_protocol::browser::QualityLevels;
use sprite_desktop_protocol::browser::VideoSample;
use sprite_desktop_protocol::browser::encode_video_frame;
use sprite_desktop_protocol::browser::parse_browser_record;
use sprite_desktop_protocol::pipe::Command;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::MAX_CLIPBOARD_BYTES;
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
use tracing::info;
use tracing::warn;

use crate::daemon::AppEvents;
use crate::daemon::Readiness;
use crate::session::LeaseState;
use crate::session::Sessions;
use crate::session::SocketId;
use crate::video::VideoHub;

const WRITE_LIMIT: Duration = Duration::from_secs(5);
const MAX_CONTROL_MESSAGE_BYTES: usize = 6 * MAX_CLIPBOARD_BYTES + 4096;
const OUTBOUND_MESSAGE_LIMIT: usize = 32;
const OUTBOUND_BYTE_LIMIT: usize = 2 * MAX_CONTROL_MESSAGE_BYTES;
const MAX_UPGRADED_CONNECTIONS: usize = 32;

#[derive(Debug, Error)]
enum SocketEnd {
    #[error("gateway shutdown")]
    GatewayShutdown,
    #[error("client sent a WebSocket close frame without a status")]
    ClientCloseWithoutStatus,
    #[error("client sent WebSocket close frame {code}: {reason}")]
    ClientClose { code: u16, reason: String },
    #[error("client disconnected without a close frame")]
    ClientDisconnected,
    #[error("WebSocket read failed: {0}")]
    Read(#[source] axum::Error),
    #[error("WebSocket write timed out after five seconds")]
    WriteTimeout,
    #[error("WebSocket write failed: {0}")]
    Write(#[source] axum::Error),
    #[error(transparent)]
    Outbound(#[from] OutboundError),
    #[error("browser control parse failed: {0}")]
    Browser(#[source] BrowserError),
    #[error("{operation} failed: {source:#}")]
    Session {
        operation: &'static str,
        #[source]
        source: anyhow::Error,
    },
    #[error("read monotonic clock: {0}")]
    Clock(#[source] nix::errno::Errno),
    #[error("control event subscription lagged by {0} events")]
    EventLagged(u64),
    #[error("control event broadcaster closed")]
    EventBroadcastClosed,
    #[error("control writer stopped without a socket termination")]
    WriterStopped,
    #[error("control writer task failed: {0}")]
    WriterTask(#[source] tokio::task::JoinError),
    #[error("control writer did not stop within five seconds")]
    WriterShutdownTimeout,
    #[error("{primary}; socket cleanup also failed: {cleanup}")]
    Cleanup {
        primary: Box<Self>,
        cleanup: Box<Self>,
    },
}

impl SocketEnd {
    fn is_failure(&self) -> bool {
        match self {
            Self::GatewayShutdown
            | Self::ClientCloseWithoutStatus
            | Self::ClientClose { .. }
            | Self::ClientDisconnected => false,
            Self::Read(_)
            | Self::WriteTimeout
            | Self::Write(_)
            | Self::Outbound(_)
            | Self::Browser(_)
            | Self::Session { .. }
            | Self::Clock(_)
            | Self::EventLagged(_)
            | Self::EventBroadcastClosed
            | Self::WriterStopped
            | Self::WriterTask(_)
            | Self::WriterShutdownTimeout => true,
            Self::Cleanup { primary, cleanup } => primary.is_failure() || cleanup.is_failure(),
        }
    }

    fn client_close(frame: Option<CloseFrame>) -> Self {
        match frame {
            Some(frame) => Self::ClientClose {
                code: frame.code,
                reason: frame.reason.to_string(),
            },
            None => Self::ClientCloseWithoutStatus,
        }
    }

    fn session(operation: &'static str, source: anyhow::Error) -> Self {
        Self::Session { operation, source }
    }

    fn with_cleanup(self, cleanup: Self) -> Self {
        Self::Cleanup {
            primary: Box::new(self),
            cleanup: Box::new(cleanup),
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
    task: TaskTrackerToken,
    permit: OwnedSemaphorePermit,
}

impl SocketAdmission {
    fn release(self) {
        let Self { task, permit } = self;
        drop(task);
        drop(permit);
    }
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
        Some(SocketAdmission { task, permit })
    }

    pub(crate) fn begin_shutdown(&self) {
        self.permits.close();
        self.tasks.close();
        self.cancellation.cancel();
    }

    pub(crate) async fn wait(&self) {
        self.tasks.wait().await;
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    readiness: Readiness,
    events: AppEvents,
    sessions: Sessions,
    hub: VideoHub,
    origin: Arc<str>,
    frame_rate: Fps,
    connections: SocketConnections,
    next_socket_id: Arc<AtomicU64>,
}

impl AppState {
    pub(crate) fn new(
        readiness: Readiness,
        events: AppEvents,
        sessions: Sessions,
        hub: VideoHub,
        origin: String,
        frame_rate: Fps,
        connections: SocketConnections,
    ) -> Self {
        Self {
            readiness,
            events,
            sessions,
            hub,
            origin: origin.into(),
            frame_rate,
            connections,
            next_socket_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn allocate_socket_id(&self) -> SocketId {
        SocketId::new(self.next_socket_id.fetch_add(1, Ordering::Relaxed))
    }
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/stream", get(stream))
        .route("/control", get(control))
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

async fn stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !valid_origin(&headers, &state.origin) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(admission) = state.connections.admit() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let socket_id = state.allocate_socket_id();
    upgrade
        .max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| stream_socket(socket, state, socket_id, admission))
        .into_response()
}

async fn stream_socket(
    mut socket: WebSocket,
    state: AppState,
    socket_id: SocketId,
    admission: SocketAdmission,
) {
    info!(socket_id = socket_id.get(), "stream socket opened");
    let reason = run_stream_socket(&mut socket, &state).await;
    log_socket_close("stream", socket_id, &reason);
    admission.release();
}

async fn run_stream_socket(socket: &mut WebSocket, state: &AppState) -> SocketEnd {
    let configuration = ClientEvent::video_config("avc1.F40034".into(), state.frame_rate);
    let configuration = match client_event_message(&configuration) {
        Ok(message) => message,
        Err(error) => return error.into(),
    };
    if let Err(error) = send(socket, configuration, &state.connections.cancellation).await {
        return error;
    }

    let (subscriber_id, bootstrap, subscription) = state.hub.subscribe();
    let result = async {
        for frame in bootstrap {
            send_video(socket, &frame, &state.connections.cancellation).await?;
        }

        loop {
            tokio::select! {
                () = state.connections.cancellation.cancelled() => {
                    return Ok(SocketEnd::GatewayShutdown);
                }
                incoming = socket.recv() => {
                    match incoming {
                        Some(Ok(Message::Close(frame))) => {
                            return Ok(SocketEnd::client_close(frame));
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => return Ok(SocketEnd::Read(error)),
                        None => return Ok(SocketEnd::ClientDisconnected),
                    }
                }
                frame = subscription.next() => {
                    send_video(socket, &frame, &state.connections.cancellation).await?;
                }
            }
        }
    }
    .await;
    state.hub.unsubscribe(subscriber_id);
    match result {
        Ok(reason) | Err(reason) => reason,
    }
}

async fn control(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !valid_origin(&headers, &state.origin) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(admission) = state.connections.admit() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let socket_id = state.allocate_socket_id();
    upgrade
        .max_message_size(MAX_CONTROL_MESSAGE_BYTES)
        .max_frame_size(MAX_CONTROL_MESSAGE_BYTES)
        .on_upgrade(move |socket| control_socket(socket, state, socket_id, admission))
        .into_response()
}

struct OutboundItem {
    message: Message,
    bytes: OwnedSemaphorePermit,
}

#[derive(Debug, Error)]
enum OutboundError {
    #[error("outbound message has {size} bytes, which exceeds the queue's integer limit")]
    MessageTooLarge { size: usize },
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

#[derive(Clone)]
struct Outbound {
    tx: mpsc::Sender<OutboundItem>,
    bytes: Arc<Semaphore>,
}

impl Outbound {
    fn enqueue(&self, message: Message) -> Result<(), OutboundError> {
        let size = message_size(&message);
        let permits =
            u32::try_from(size).map_err(|_overflow| OutboundError::MessageTooLarge { size })?;
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

async fn control_writer(
    mut sink: SplitSink<WebSocket, Message>,
    mut receiver: mpsc::Receiver<OutboundItem>,
    cancellation: CancellationToken,
) -> Result<(), SocketEnd> {
    let cancel_when_writer_stops = CancelOnDrop(cancellation.clone());
    let result = 'writer: loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break 'writer Ok(()),
            item = receiver.recv() => {
                let Some(OutboundItem { message, bytes }) = item else {
                    break 'writer Err(SocketEnd::WriterStopped);
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
                            Ok(Err(error)) => break 'writer Err(SocketEnd::Write(error)),
                            Err(_elapsed) => break 'writer Err(SocketEnd::WriteTimeout),
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
    admission: SocketAdmission,
) {
    info!(socket_id = socket_id.get(), "control socket opened");
    let reason = run_control_socket(socket, &state, socket_id).await;
    log_socket_close("control", socket_id, &reason);
    admission.release();
}

fn log_socket_close(kind: &'static str, socket_id: SocketId, reason: &SocketEnd) {
    if reason.is_failure() {
        warn!(socket = kind, socket_id = socket_id.get(), reason = %reason, "socket closed");
    } else {
        info!(socket = kind, socket_id = socket_id.get(), reason = %reason, "socket closed");
    }
}

async fn run_control_socket(socket: WebSocket, state: &AppState, socket_id: SocketId) -> SocketEnd {
    // Subscribe before reading the snapshot so an update cannot fall between them.
    let mut events = state.events.subscribe();
    let initial_events = state.events.initial();
    let cancellation = state.connections.cancellation.child_token();
    let (sink, mut incoming) = socket.split();
    let (sender, receiver) = mpsc::channel(OUTBOUND_MESSAGE_LIMIT);
    let outbound = Outbound {
        tx: sender,
        bytes: Arc::new(Semaphore::new(OUTBOUND_BYTE_LIMIT)),
    };
    let mut writer: JoinHandle<Result<(), SocketEnd>> =
        tokio::spawn(control_writer(sink, receiver, cancellation.clone()));
    let mut writer_finished = false;

    let reason = 'connected: {
        if let Err(error) =
            enqueue_initial_events(&outbound, initial_events, state.sessions.quality())
        {
            break 'connected error.into();
        }

        loop {
            tokio::select! {
                biased;
                () = state.connections.cancellation.cancelled() => {
                    break 'connected SocketEnd::GatewayShutdown;
                }
                writer_result = &mut writer => {
                    writer_finished = true;
                    break 'connected match writer_result {
                        Ok(Ok(())) => SocketEnd::WriterStopped,
                        Ok(Err(error)) => error,
                        Err(error) => SocketEnd::WriterTask(error),
                    };
                }
                incoming_result = incoming.next() => {
                    let message = match incoming_result {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => break 'connected SocketEnd::Read(error),
                        None => break 'connected SocketEnd::ClientDisconnected,
                    };
                    let result = match message {
                        Message::Text(text) => {
                            handle_text(&outbound, state, socket_id, text.as_str()).await
                        }
                        Message::Binary(bytes) => {
                            let command = parse_browser_record(&bytes)
                                .map_err(SocketEnd::Browser);
                            match command {
                                Ok(command) => state
                                    .sessions
                                    .input(socket_id, command)
                                    .await
                                    .map_err(|error| SocketEnd::session("send binary input", error)),
                                Err(error) => Err(error),
                            }
                        }
                        Message::Close(frame) => {
                            break 'connected SocketEnd::client_close(frame);
                        }
                        Message::Ping(_) | Message::Pong(_) => Ok(()),
                    };
                    if let Err(error) = result {
                        break 'connected error;
                    }
                }
                event_result = events.recv() => {
                    match event_result {
                        Ok(event) => {
                            if state.sessions.owns(socket_id).await
                                && let Err(error) = outbound.enqueue_event(&event)
                            {
                                break 'connected error.into();
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(count)) => {
                            break 'connected SocketEnd::EventLagged(count);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break 'connected SocketEnd::EventBroadcastClosed;
                        }
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
        writer_finished,
        &mut writer,
    )
    .await
}

fn enqueue_initial_events(
    outbound: &Outbound,
    initial_events: Vec<ClientEvent>,
    quality: QualityLevels,
) -> Result<(), OutboundError> {
    for event in initial_events
        .into_iter()
        .chain([ClientEvent::Quality(quality)])
    {
        outbound.enqueue_event(&event)?;
    }
    Ok(())
}

async fn finish_control_socket(
    mut reason: SocketEnd,
    state: &AppState,
    socket_id: SocketId,
    cancellation: CancellationToken,
    writer_finished: bool,
    writer: &mut JoinHandle<Result<(), SocketEnd>>,
) -> SocketEnd {
    cancellation.cancel();
    if let Err(error) = state.sessions.release(socket_id).await {
        reason = reason.with_cleanup(SocketEnd::session("release input lease", error));
    }

    if !writer_finished {
        match timeout(WRITE_LIMIT, &mut *writer).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => reason = reason.with_cleanup(error),
            Ok(Err(error)) => reason = reason.with_cleanup(SocketEnd::WriterTask(error)),
            Err(_elapsed) => {
                writer.abort();
                let _ = writer.await;
                reason = reason.with_cleanup(SocketEnd::WriterShutdownTimeout);
            }
        }
    }
    reason
}

async fn handle_text(
    outbound: &Outbound,
    state: &AppState,
    socket_id: SocketId,
    text: &str,
) -> Result<(), SocketEnd> {
    match text {
        "acquire" => {
            let control_state = match state
                .sessions
                .acquire(socket_id)
                .await
                .map_err(|error| SocketEnd::session("acquire input lease", error))?
            {
                LeaseState::Active => ControlState::Active,
                LeaseState::Busy => ControlState::Busy,
            };
            outbound
                .enqueue_event(&ClientEvent::ControlState {
                    state: control_state,
                })
                .map_err(SocketEnd::from)
        }
        "release" => {
            state
                .sessions
                .release(socket_id)
                .await
                .map_err(|error| SocketEnd::session("release input lease", error))?;
            outbound
                .enqueue_event(&ClientEvent::ControlState {
                    state: ControlState::Ready,
                })
                .map_err(SocketEnd::from)
        }
        _ => match ClientMessage::parse_json(text.as_bytes()).map_err(SocketEnd::Browser)? {
            ClientMessage::Ping { id: request_id } => {
                let time = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(SocketEnd::Clock)?;
                let nanos = i128::from(time.tv_sec()) * 1_000_000_000 + i128::from(time.tv_nsec());
                outbound
                    .enqueue_event(&ClientEvent::Pong {
                        id: request_id,
                        server_nanos: nanos.to_string(),
                    })
                    .map_err(SocketEnd::from)
            }
            ClientMessage::Feedback(feedback) => {
                let levels = state
                    .sessions
                    .feedback(socket_id, feedback)
                    .await
                    .map_err(|error| SocketEnd::session("apply quality feedback", error))?;
                if let Some(levels) = levels {
                    outbound
                        .enqueue_event(&ClientEvent::Quality(levels))
                        .map_err(SocketEnd::from)?;
                }
                Ok(())
            }
            ClientMessage::Text {
                action,
                text,
                sequence,
            } => state
                .sessions
                .input(
                    socket_id,
                    Command::Text {
                        action,
                        text,
                        sequence,
                    },
                )
                .await
                .map_err(|error| SocketEnd::session("send text input", error)),
            ClientMessage::ClipboardWrite { text } => state
                .sessions
                .input(socket_id, Command::Clipboard(text))
                .await
                .map_err(|error| SocketEnd::session("send clipboard input", error)),
        },
    }
}

async fn send_video(
    socket: &mut WebSocket,
    frame: &VideoSample,
    cancellation: &CancellationToken,
) -> Result<(), SocketEnd> {
    let bytes = encode_video_frame(frame);
    send(socket, Message::Binary(bytes.into()), cancellation).await
}

async fn send(
    socket: &mut WebSocket,
    message: Message,
    cancellation: &CancellationToken,
) -> Result<(), SocketEnd> {
    tokio::select! {
        () = cancellation.cancelled() => Err(SocketEnd::GatewayShutdown),
        result = timeout(WRITE_LIMIT, socket.send(message)) => {
            match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(SocketEnd::Write(error)),
                Err(_elapsed) => Err(SocketEnd::WriteTimeout),
            }
        }
    }
}

fn valid_origin(headers: &HeaderMap, expected: &str) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return false;
    };
    origins.next().is_none()
        && origin
            .to_str()
            .is_ok_and(|value| value == expected && value != "null")
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
    fn socket_close_reasons_keep_peer_and_cleanup_details() {
        let peer_close = SocketEnd::client_close(Some(CloseFrame {
            code: 1008,
            reason: "policy".into(),
        }));
        assert_eq!(
            peer_close.to_string(),
            "client sent WebSocket close frame 1008: policy"
        );
        assert!(!peer_close.is_failure());

        let reason = peer_close.with_cleanup(SocketEnd::WriterShutdownTimeout);
        assert_eq!(
            reason.to_string(),
            "client sent WebSocket close frame 1008: policy; socket cleanup also failed: \
             control writer did not stop within five seconds"
        );
        assert!(reason.is_failure());
        assert!(SocketEnd::WriteTimeout.is_failure());
        assert!(!SocketEnd::GatewayShutdown.is_failure());
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
        let mut headers = HeaderMap::new();
        assert!(!valid_origin(&headers, "https://x.test"));
        headers.append(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!valid_origin(&headers, "https://x.test"));
        headers.clear();
        headers.append(header::ORIGIN, HeaderValue::from_static("https://x.test"));
        assert!(valid_origin(&headers, "https://x.test"));
        headers.append(header::ORIGIN, HeaderValue::from_static("https://x.test"));
        assert!(!valid_origin(&headers, "https://x.test"));
    }
}
