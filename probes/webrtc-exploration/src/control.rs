//! Temporary, fixed control WebSocket bridge to the existing loopback gateway.

use std::time::Duration;

use axum::extract::ws::{Message as ClientMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use http::{Request, header};
use tokio_tungstenite::tungstenite::{Message as UpstreamMessage, protocol::WebSocketConfig};
use tokio_util::sync::CancellationToken;

const UPSTREAM_URL: &str = "ws://127.0.0.1:8080/control";
const UPSTREAM_ORIGIN: &str = "https://sprite-desktop-rust-6ra.sprites.app";
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) async fn bridge(mut client: WebSocket, shutdown: CancellationToken) {
    let request = Request::builder()
        .uri(UPSTREAM_URL)
        .header(header::ORIGIN, UPSTREAM_ORIGIN)
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-version", "13")
        .header(
            "sec-websocket-key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .expect("fixed upstream request is valid");
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let connection = tokio::select! {
        _ = shutdown.cancelled() => return,
        result = tokio::time::timeout(SEND_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(request, Some(websocket_config), false)
        ) => result,
    };
    let (mut upstream, _) = match connection {
        Ok(Ok(connection)) => connection,
        _ => {
            eprintln!("webrtc-exploration: control upstream connection failed or timed out");
            let _ = tokio::time::timeout(SEND_TIMEOUT, client.close()).await;
            return;
        }
    };

    loop {
        tokio::select! {
            message = client.recv() => {
                let Some(Ok(message)) = message else { break };
                if message_size(&message) > MAX_MESSAGE_BYTES {
                    break;
                }
                let closes = matches!(message, ClientMessage::Close(_));
                let Some(message) = to_upstream(message) else { continue };
                if !matches!(
                    tokio::time::timeout(SEND_TIMEOUT, upstream.send(message)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
                if closes { break; }
            }
            message = upstream.next() => {
                let Some(Ok(message)) = message else { break };
                if upstream_message_size(&message) > MAX_MESSAGE_BYTES {
                    break;
                }
                let closes = matches!(message, UpstreamMessage::Close(_));
                let Some(message) = to_client(message) else { continue };
                if !matches!(
                    tokio::time::timeout(SEND_TIMEOUT, client.send(message)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
                if closes { break; }
            }
            _ = shutdown.cancelled() => break,
        }
    }

    let _ = tokio::time::timeout(SEND_TIMEOUT, upstream.close(None)).await;
    let _ = tokio::time::timeout(SEND_TIMEOUT, client.close()).await;
}

fn message_size(message: &ClientMessage) -> usize {
    match message {
        ClientMessage::Text(text) => text.len(),
        ClientMessage::Binary(bytes) | ClientMessage::Ping(bytes) | ClientMessage::Pong(bytes) => {
            bytes.len()
        }
        ClientMessage::Close(_) => 0,
    }
}

fn upstream_message_size(message: &UpstreamMessage) -> usize {
    match message {
        UpstreamMessage::Text(text) => text.len(),
        UpstreamMessage::Binary(bytes)
        | UpstreamMessage::Ping(bytes)
        | UpstreamMessage::Pong(bytes) => bytes.len(),
        UpstreamMessage::Close(_) | UpstreamMessage::Frame(_) => 0,
    }
}

fn to_upstream(message: ClientMessage) -> Option<UpstreamMessage> {
    Some(match message {
        ClientMessage::Text(text) => UpstreamMessage::Text(text.as_str().into()),
        ClientMessage::Binary(bytes) => UpstreamMessage::Binary(bytes),
        ClientMessage::Ping(bytes) => UpstreamMessage::Ping(bytes),
        ClientMessage::Pong(bytes) => UpstreamMessage::Pong(bytes),
        ClientMessage::Close(frame) => UpstreamMessage::Close(frame.map(|frame| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: frame.code.into(),
                reason: frame.reason.as_str().into(),
            }
        })),
    })
}

fn to_client(message: UpstreamMessage) -> Option<ClientMessage> {
    match message {
        UpstreamMessage::Text(text) => Some(ClientMessage::Text(text.as_str().into())),
        UpstreamMessage::Binary(bytes) => Some(ClientMessage::Binary(bytes)),
        UpstreamMessage::Ping(bytes) => Some(ClientMessage::Ping(bytes)),
        UpstreamMessage::Pong(bytes) => Some(ClientMessage::Pong(bytes)),
        UpstreamMessage::Close(frame) => Some(ClientMessage::Close(frame.map(|frame| {
            axum::extract::ws::CloseFrame {
                code: frame.code.into(),
                reason: frame.reason.as_str().into(),
            }
        }))),
        UpstreamMessage::Frame(_) => None,
    }
}
