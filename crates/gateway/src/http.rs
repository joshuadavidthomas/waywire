use crate::{
    daemon::RuntimeState,
    gateway_timings::{GatewayTimings, TimingRecord},
    protocol::{self, JsonInput, TextAction, VideoSample},
    session::{LeaseState, Sessions},
    video::VideoHub,
};
use axum::{
    Router,
    body::Body,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use nix::time::{ClockId, clock_gettime};
use rust_embed::Embed;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::{
    sync::CancellationToken,
    task::{TaskTracker, task_tracker::TaskTrackerToken},
};

const WRITE_LIMIT: Duration = Duration::from_secs(5);
const MAX_CONTROL_MESSAGE_BYTES: usize = 6 * protocol::MAX_CLIPBOARD_BYTES + 4096;
const OUTBOUND_MESSAGE_LIMIT: usize = 32;
const OUTBOUND_BYTE_LIMIT: usize = 2 * MAX_CONTROL_MESSAGE_BYTES;
const MAX_UPGRADED_CONNECTIONS: usize = 32;
#[derive(Embed)]
#[folder = "../../apps/web/dist/"]
struct Assets;
#[derive(Clone)]
pub struct SocketConnections {
    permits: Arc<Semaphore>,
    tasks: TaskTracker,
    cancellation: CancellationToken,
}
struct SocketAdmission {
    _task: TaskTrackerToken,
    _permit: OwnedSemaphorePermit,
}
impl SocketConnections {
    pub fn new() -> Self {
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
        let permit = self.permits.clone().try_acquire_owned().ok()?;
        Some(SocketAdmission {
            _task: task,
            _permit: permit,
        })
    }
    pub fn begin_shutdown(&self) {
        self.permits.close();
        self.tasks.close();
        self.cancellation.cancel();
    }
    pub async fn wait(&self) {
        self.tasks.wait().await;
    }
}

#[derive(Clone)]
pub struct AppState {
    pub runtime: RuntimeState,
    pub sessions: Sessions,
    pub hub: VideoHub,
    pub origin: Arc<str>,
    pub frame_rate: u32,
    pub timings: Option<GatewayTimings>,
    connections: SocketConnections,
    next_id: Arc<AtomicU64>,
}
impl AppState {
    pub fn new(
        runtime: RuntimeState,
        sessions: Sessions,
        hub: VideoHub,
        origin: String,
        frame_rate: u32,
        timings: Option<GatewayTimings>,
        connections: SocketConnections,
    ) -> Self {
        Self {
            runtime,
            sessions,
            hub,
            origin: origin.into(),
            frame_rate,
            timings,
            connections,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }
}
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/stream", get(stream))
        .route("/control", get(control))
        .fallback(get(asset))
        .with_state(state)
}
async fn health(State(s): State<AppState>) -> Response {
    if s.runtime.is_ready() {
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
    State(s): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !valid_origin(&headers, &s.origin) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(admission) = s.connections.admit() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade
        .max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| stream_socket(socket, s, admission))
        .into_response()
}
async fn stream_socket(mut socket: WebSocket, s: AppState, _admission: SocketAdmission) {
    if send(
        &mut socket,
        Message::Text(
            json!({"type":"video-config","version":2,"codec":"avc1.F40034","frameRate":s.frame_rate})
                .to_string()
                .into(),
        ),
        &s.connections.cancellation,
    )
    .await
    .is_err()
    {
        return;
    }
    let (id, bootstrap, subscription) = s.hub.subscribe();
    for frame in bootstrap {
        if send_video(
            &mut socket,
            &frame,
            id,
            s.timings.as_ref(),
            &s.connections.cancellation,
        )
        .await
        .is_err()
        {
            s.hub.unsubscribe(id);
            return;
        }
    }
    loop {
        tokio::select! {
            _ = s.connections.cancellation.cancelled() => break,
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            frame = subscription.next() => {
                if send_video(
                    &mut socket,
                    &frame,
                    id,
                    s.timings.as_ref(),
                    &s.connections.cancellation,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        }
    }
    s.hub.unsubscribe(id)
}
async fn control(
    State(s): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !valid_origin(&headers, &s.origin) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(admission) = s.connections.admit() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let id = s.next_id.fetch_add(1, Ordering::Relaxed);
    upgrade
        .max_message_size(MAX_CONTROL_MESSAGE_BYTES)
        .max_frame_size(MAX_CONTROL_MESSAGE_BYTES)
        .on_upgrade(move |socket| control_socket(socket, s, id, admission))
        .into_response()
}
struct OutboundItem {
    message: Message,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct Outbound {
    tx: mpsc::Sender<OutboundItem>,
    bytes: Arc<Semaphore>,
}
impl Outbound {
    fn enqueue(&self, message: Message) -> Result<(), ()> {
        let size = message_size(&message);
        let permits = u32::try_from(size).map_err(|_| ())?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(permits)
            .map_err(|_| ())?;
        self.tx
            .try_send(OutboundItem {
                message,
                _bytes: bytes,
            })
            .map_err(|_| ())
    }
    fn enqueue_json(&self, value: serde_json::Value) -> Result<(), ()> {
        self.enqueue(Message::Text(value.to_string().into()))
    }
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
    mut rx: mpsc::Receiver<OutboundItem>,
    cancellation: CancellationToken,
) {
    let _cancel_on_drop = CancelOnDrop(cancellation.clone());
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            item = rx.recv() => {
                let Some(item) = item else { break };
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    result = timeout(WRITE_LIMIT, sink.send(item.message)) => {
                        if result.map_or(true, |result| result.is_err()) {
                            break;
                        }
                    }
                }
            }
        }
    }
}
async fn control_socket(socket: WebSocket, s: AppState, id: u64, _admission: SocketAdmission) {
    let cancellation = s.connections.cancellation.child_token();
    let (sink, mut incoming) = socket.split();
    let (tx, rx) = mpsc::channel(OUTBOUND_MESSAGE_LIMIT);
    let outbound = Outbound {
        tx,
        bytes: Arc::new(Semaphore::new(OUTBOUND_BYTE_LIMIT)),
    };
    let mut writer: JoinHandle<()> = tokio::spawn(control_writer(sink, rx, cancellation.clone()));
    let mut events = s.runtime.events.subscribe();
    let (bitrate, fps, scale) = s.sessions.quality();
    let initial = s.runtime.events.initial().into_iter().chain([json!({
        "type": "quality",
        "bitrate": bitrate,
        "fps": fps,
        "scale": scale,
    })
    .to_string()]);
    for value in initial {
        if outbound.enqueue(Message::Text(value.into())).is_err() {
            cancellation.cancel();
            writer.abort();
            let _ = writer.await;
            return;
        }
    }
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            message = incoming.next() => {
                let Some(Ok(message)) = message else {
                    break;
                };
                let result = match message {
                    Message::Text(text) => handle_text(&outbound, &s, id, text.as_str()).await,
                    Message::Binary(bytes) => match protocol::parse_browser_record(&bytes) {
                        Ok(command) => s.sessions.input(id, command).await.map_err(|_| ()),
                        Err(_) => Err(()),
                    },
                    Message::Close(_) => break,
                    _ => Err(()),
                };
                if result.is_err() {
                    break;
                }
            }
            event = events.recv() => {
                match event {
                    Ok(value) => {
                        if s.sessions.owns(id).await
                            && outbound.enqueue(Message::Text(value.into())).is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    cancellation.cancel();
    let _ = s.sessions.release(id).await;
    if timeout(WRITE_LIMIT, &mut writer).await.is_err() {
        writer.abort();
        let _ = writer.await;
    }
}
async fn handle_text(outbound: &Outbound, s: &AppState, id: u64, text: &str) -> Result<(), ()> {
    match text {
        "acquire" => {
            let state = match s.sessions.acquire(id).await.map_err(|_| ())? {
                LeaseState::Active => "active",
                LeaseState::Busy => "busy",
            };
            outbound.enqueue_json(json!({"type":"control-state","state":state}))
        }
        "release" => {
            s.sessions.release(id).await.map_err(|_| ())?;
            outbound.enqueue_json(json!({"type":"control-state","state":"ready"}))
        }
        _ => match protocol::parse_json(text.as_bytes()).map_err(|_| ())? {
            JsonInput::Ping { id } => {
                let time = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(|_| ())?;
                let nanos = i128::from(time.tv_sec()) * 1_000_000_000 + i128::from(time.tv_nsec());
                outbound
                    .enqueue_json(json!({"type":"pong","id":id,"serverNanos":nanos.to_string()}))
            }
            JsonInput::Feedback(feedback) => {
                let Some((bitrate, fps, scale)) =
                    s.sessions.feedback(id, feedback).await.map_err(|_| ())?
                else {
                    return Ok(());
                };
                outbound.enqueue_json(
                    json!({"type":"quality","bitrate":bitrate,"fps":fps,"scale":scale}),
                )
            }
            JsonInput::Text {
                action,
                text,
                sequence,
            } => s
                .sessions
                .text(id, matches!(action, TextAction::Preedit), text, sequence)
                .await
                .map_err(|_| ()),
            JsonInput::ClipboardWrite { text } => {
                s.sessions.clipboard(id, text).await.map_err(|_| ())
            }
        },
    }
}
async fn send_video(
    socket: &mut WebSocket,
    frame: &VideoSample,
    connection_id: u64,
    timings: Option<&GatewayTimings>,
    cancellation: &CancellationToken,
) -> Result<(), ()> {
    let bytes = protocol::encode_video(frame);
    let byte_length = bytes.len();
    let timing_span = timings.and_then(|timings| {
        let sample = timings.sample()?;
        timings
            .publish(sample, |timestamp_nanos| TimingRecord::SocketWriteStart {
                timestamp_nanos,
                sequence: frame.metadata.sequence,
                generation: frame.metadata.generation,
                connection_id,
                byte_length,
            })
            .then_some(sample)
    });
    send(socket, Message::Binary(bytes.into()), cancellation).await?;
    if let (Some(timings), Some(span)) = (timings, timing_span) {
        timings.record_in_epoch(span, |timestamp_nanos| TimingRecord::SocketWriteEnd {
            timestamp_nanos,
            sequence: frame.metadata.sequence,
            generation: frame.metadata.generation,
            connection_id,
            byte_length,
        });
    }
    Ok(())
}

async fn send(
    socket: &mut WebSocket,
    message: Message,
    cancellation: &CancellationToken,
) -> Result<(), ()> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(()),
        result = timeout(WRITE_LIMIT, socket.send(message)) => {
            result.map_err(|_| ())?.map_err(|_| ())
        }
    }
}
fn valid_origin(headers: &HeaderMap, expected: &str) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return false;
    };
    origins.next().is_none() && origin.to_str().is_ok_and(|v| v == expected && v != "null")
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
    let mime = if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    response(StatusCode::OK, mime, file.data.into_owned())
}
fn response(status: StatusCode, mime: &'static str, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'self'; connect-src 'self' ws: wss:; script-src 'self'; style-src 'self'; img-src 'self' data:"));
    response
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outbound_queue_enforces_count_and_byte_bounds_independently() {
        let (tx, _rx) = mpsc::channel(1);
        let outbound = Outbound {
            tx,
            bytes: Arc::new(Semaphore::new(10)),
        };
        assert!(outbound.enqueue(Message::Text("1".into())).is_ok());
        assert!(outbound.enqueue(Message::Text("2".into())).is_err());

        let (tx, _rx) = mpsc::channel(2);
        let outbound = Outbound {
            tx,
            bytes: Arc::new(Semaphore::new(4)),
        };
        assert!(outbound.enqueue(Message::Text("123".into())).is_ok());
        assert!(outbound.enqueue(Message::Text("12".into())).is_err());
    }

    #[tokio::test]
    async fn outbound_queue_returns_byte_permits_on_rejection_and_release() {
        let (tx, rx) = mpsc::channel(1);
        let bytes = Arc::new(Semaphore::new(4));
        let outbound = Outbound {
            tx,
            bytes: bytes.clone(),
        };
        drop(rx);
        assert!(outbound.enqueue(Message::Text("1234".into())).is_err());
        assert_eq!(bytes.available_permits(), 4);

        let (tx, mut rx) = mpsc::channel(1);
        let outbound = Outbound {
            tx,
            bytes: bytes.clone(),
        };
        assert!(outbound.enqueue(Message::Text("123".into())).is_ok());
        assert_eq!(bytes.available_permits(), 1);
        drop(rx.recv().await.unwrap());
        assert_eq!(bytes.available_permits(), 4);
    }

    #[test]
    fn origin_requires_one_exact_value() {
        let mut h = HeaderMap::new();
        assert!(!valid_origin(&h, "https://x.test"));
        h.append(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!valid_origin(&h, "https://x.test"));
        h.clear();
        h.append(header::ORIGIN, HeaderValue::from_static("https://x.test"));
        assert!(valid_origin(&h, "https://x.test"));
        h.append(header::ORIGIN, HeaderValue::from_static("https://x.test"));
        assert!(!valid_origin(&h, "https://x.test"));
    }
}
