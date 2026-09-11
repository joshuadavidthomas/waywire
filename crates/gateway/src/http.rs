mod admission;
mod assets;
mod control;
mod outbound;
mod socket;
mod stream;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;

pub(crate) use self::admission::Origin;
pub(crate) use self::admission::SocketConnections;
use self::admission::SocketKind;
use self::admission::upgrade_socket;
use self::assets::asset;
use self::assets::response;
use crate::daemon::AppEvents;
use crate::session::Sessions;
use crate::session::SocketId;
use crate::video::Readiness;
use crate::video::VideoHub;

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
