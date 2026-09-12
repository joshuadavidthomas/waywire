use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use tokio_util::sync::CancellationToken;
use tracing::info;
use waywire_protocol::Record;
use waywire_protocol::browser::ClientEvent;
use waywire_protocol::browser::VideoSample;
use waywire_protocol::pipe::Generation;

use super::AppState;
use super::admission::SocketAdmission;
use super::outbound::client_event_message;
use super::socket::SocketEnd;
use super::socket::log_socket_close;
use super::socket::send;
use crate::session::SocketId;

pub(super) async fn stream_socket(
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
    let mut subscription = state.hub.subscribe();
    let mut announced_generation = None;
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
                    &mut announced_generation,
                    &state.connections.cancellation,
                ).await {
                    return reason;
                }
            }
        }
    }
}

async fn send_video(
    socket: &mut WebSocket,
    frame: &VideoSample,
    announced_generation: &mut Option<Generation>,
    cancellation: &CancellationToken,
) -> Result<(), SocketEnd> {
    let generation = frame.metadata.generation;
    if *announced_generation != Some(generation) {
        let configuration = ClientEvent::video_config(frame.metadata.chroma.h264_profile().codec());
        let configuration = client_event_message(&configuration)?;
        // The JSON configuration and its generation's first keyframe share this
        // socket, so WebSocket message order is the decoder reconfiguration contract.
        send(socket, configuration, cancellation).await?;
        *announced_generation = Some(generation);
    }
    let bytes = frame.encode();
    send(socket, Message::Binary(bytes.into()), cancellation).await
}
