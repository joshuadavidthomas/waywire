//! Loopback-only HTTP routes with validated offer and control admissions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Bytes;
use axum::extract::ws::{WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rtc::peer_connection::sdp::{RTCSdpType, RTCSessionDescription};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::session::{OfferError, OfferRequest};
use crate::source::Source;

pub const BIND_ADDR: &str = "127.0.0.1:3220";
const EXPECTED_ORIGIN: &str = "http://127.0.0.1:3220";
const EXPECTED_HOST: &str = "127.0.0.1:3220";
const MAX_OFFER_BYTES: usize = 64 * 1024;
pub(crate) const STUN_URL: &str = "stun:stun.l.google.com:19302";

#[derive(Clone)]
pub(crate) struct AppState {
    claimed: Arc<AtomicBool>,
    control_claimed: Arc<AtomicBool>,
    requests: mpsc::Sender<OfferRequest>,
    source: Source,
    shutdown: CancellationToken,
    pub(crate) control_tasks: TaskTracker,
}

impl AppState {
    pub fn new(
        requests: mpsc::Sender<OfferRequest>,
        source: Source,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            claimed: Arc::new(AtomicBool::new(false)),
            control_claimed: Arc::new(AtomicBool::new(false)),
            requests,
            source,
            shutdown,
            control_tasks: TaskTracker::new(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicConfig {
    source: Source,
    ice_servers: Vec<PublicIceServer>,
    control: bool,
}

#[derive(Serialize)]
struct PublicIceServer {
    urls: Vec<&'static str>,
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/client.js", get(client_js))
        .route("/config", get(config))
        .route("/control", get(control))
        .route(
            "/offer",
            post(offer).layer(DefaultBodyLimit::max(MAX_OFFER_BYTES)),
        )
        .with_state(state)
}

async fn index() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../client.html"),
    )
        .into_response()
}

async fn config(State(state): State<AppState>) -> Json<PublicConfig> {
    let desktop = state.source == Source::Desktop;
    Json(PublicConfig {
        source: state.source,
        ice_servers: if desktop {
            vec![PublicIceServer {
                urls: vec![STUN_URL],
            }]
        } else {
            vec![]
        },
        control: desktop,
    })
}

async fn client_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../dist/client.js"),
    )
        .into_response()
}

fn header_matches(headers: &HeaderMap, name: header::HeaderName, expected: &str) -> bool {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return false;
    };
    values.next().is_none() && value.to_str().is_ok_and(|value| value == expected)
}

async fn control(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if state.source != Source::Desktop {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !header_matches(&headers, header::ORIGIN, EXPECTED_ORIGIN) {
        return (StatusCode::FORBIDDEN, "origin denied").into_response();
    }
    if !header_matches(&headers, header::HOST, EXPECTED_HOST) {
        return (StatusCode::FORBIDDEN, "host denied").into_response();
    }
    let Ok(upgrade) = upgrade else {
        return (StatusCode::UPGRADE_REQUIRED, "websocket upgrade required").into_response();
    };
    if state.control_claimed.swap(true, Ordering::SeqCst) {
        return (StatusCode::CONFLICT, "control already used").into_response();
    }
    if state.shutdown.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let shutdown = state.shutdown.clone();
    // Count pending upgrades too: an upgraded socket outlives HTTP draining.
    let admission = state.control_tasks.token();
    upgrade
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            let _admission = admission;
            crate::control::bridge(socket, shutdown).await;
        })
}

async fn offer(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !header_matches(&headers, header::ORIGIN, EXPECTED_ORIGIN) {
        return (StatusCode::FORBIDDEN, "origin denied").into_response();
    }
    if !header_matches(&headers, header::HOST, EXPECTED_HOST) {
        return (StatusCode::FORBIDDEN, "host denied").into_response();
    }
    let offer: RTCSessionDescription = match serde_json::from_slice(&body) {
        Ok(offer) => offer,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("invalid offer: {error}")).into_response();
        }
    };
    if offer.sdp_type != RTCSdpType::Offer {
        return (StatusCode::BAD_REQUEST, "description type must be offer").into_response();
    }
    if state.claimed.swap(true, Ordering::SeqCst) {
        return (StatusCode::CONFLICT, "session already used").into_response();
    }

    let (reply, answer) = oneshot::channel();
    if state
        .requests
        .send(OfferRequest { offer, reply })
        .await
        .is_err()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "session actor stopped").into_response();
    }
    match answer.await {
        Ok(Ok(answer)) => Json(answer).into_response(),
        Ok(Err(OfferError::GatheringTimedOut)) => {
            (StatusCode::GATEWAY_TIMEOUT, "ICE gathering timed out").into_response()
        }
        Ok(Err(OfferError::Cancelled)) | Err(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, "session stopped").into_response()
        }
        Ok(Err(error)) => (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    fn request(body: &'static str) -> Request<Body> {
        Request::post("/offer")
            .header(header::ORIGIN, EXPECTED_ORIGIN)
            .header(header::HOST, EXPECTED_HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    fn state(source: Source) -> AppState {
        let (tx, _rx) = mpsc::channel(1);
        AppState::new(tx, source, CancellationToken::new())
    }

    #[tokio::test]
    async fn config_exposes_desktop_stun_and_control_only_for_desktop() {
        let response = router(state(Source::Desktop))
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "source": "desktop",
                "iceServers": [{"urls": [STUN_URL]}],
                "control": true
            })
        );

        let response = router(state(Source::Chart))
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"source": "chart", "iceServers": [], "control": false})
        );
    }

    #[tokio::test]
    async fn control_is_source_guarded_before_admission() {
        let response = router(state(Source::Motion))
            .oneshot(Request::get("/control").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn control_rejects_wrong_origin_before_upgrade() {
        let response = router(state(Source::Desktop))
            .oneshot(
                Request::get("/control")
                    .header(header::HOST, EXPECTED_HOST)
                    .header(header::ORIGIN, "http://evil.invalid")
                    .header(header::CONNECTION, "upgrade")
                    .header(header::UPGRADE, "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn invalid_description_does_not_claim_session() {
        let (tx, _rx) = mpsc::channel(1);
        let app = router(AppState::new(tx, Source::Motion, CancellationToken::new()));
        let bad = app
            .clone()
            .oneshot(request(r#"{"type":"answer","sdp":"x"}"#))
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        let malformed = app.oneshot(request("not json")).await.unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_by_extractor_limit() {
        let (tx, _rx) = mpsc::channel(1);
        let app = router(AppState::new(tx, Source::Motion, CancellationToken::new()));
        let body = format!(
            r#"{{"type":"offer","sdp":"{}"}}"#,
            "x".repeat(MAX_OFFER_BYTES)
        );
        let response = app
            .oneshot(
                Request::post("/offer")
                    .header(header::ORIGIN, EXPECTED_ORIGIN)
                    .header(header::HOST, EXPECTED_HOST)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
