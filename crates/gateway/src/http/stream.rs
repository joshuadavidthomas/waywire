use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use tokio_util::sync::CancellationToken;
use tracing::info;
use waywire_protocol::Record;
use waywire_protocol::browser::ClientEvent;
use waywire_protocol::browser::VideoSample;
use waywire_protocol::pipe::H264_PROFILE;

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

async fn send_video(
    socket: &mut WebSocket,
    frame: &VideoSample,
    cancellation: &CancellationToken,
) -> Result<(), SocketEnd> {
    let bytes = frame.encode();
    send(socket, Message::Binary(bytes.into()), cancellation).await
}
