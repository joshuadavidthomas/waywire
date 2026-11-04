use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use tokio::sync::broadcast::error::RecvError;
use tracing::info;

use super::AppState;
use super::admission::SocketAdmission;
use super::socket::SocketEnd;
use super::socket::log_socket_close;
use super::socket::send;
use crate::session::SocketId;

pub(super) async fn audio_socket(
    mut socket: WebSocket,
    state: AppState,
    socket_id: SocketId,
    _admission: SocketAdmission,
) {
    info!(socket_id = socket_id.get(), "audio socket opened");
    let reason = run_audio_socket(&mut socket, &state).await;
    log_socket_close("audio", socket_id, &reason);
}

async fn run_audio_socket(socket: &mut WebSocket, state: &AppState) -> SocketEnd {
    let mut packets = state.audio.subscribe();
    loop {
        tokio::select! {
            () = state.connections.cancellation().cancelled() => {
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
            packet = packets.recv() => {
                match packet {
                    Ok(packet) => {
                        if let Err(reason) = send(
                            socket,
                            Message::Binary(packet),
                            state.connections.cancellation(),
                        ).await {
                            return reason;
                        }
                    }
                    // The viewer resynchronises on the packets that follow.
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return SocketEnd::Shutdown,
                }
            }
        }
    }
}
