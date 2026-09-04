//! The single joined session actor owns negotiation, media, and cleanup.

use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_VP9, MediaEngine};
use rtc::peer_connection::configuration::{RTCConfigurationBuilder, RTCIceServer};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::shared::marshal::Unmarshal;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};

use crate::encoder::{Encoder, EncoderConfig};
use crate::http::STUN_URL;
use crate::source::Source;

const VP9_FMTP: &str = "profile-id=1";
const PAYLOAD_TYPE: u8 = 96;
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const LOCAL_MEDIA_TTL: Duration = Duration::from_secs(90);
const DESKTOP_MEDIA_TTL: Duration = Duration::from_secs(20 * 60);
const LOCAL_OFFER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DESKTOP_OFFER_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub(crate) struct OfferRequest {
    pub offer: RTCSessionDescription,
    pub reply: oneshot::Sender<Result<RTCSessionDescription, OfferError>>,
}

pub(crate) struct ActorConfig {
    pub ffmpeg_bin: String,
    pub source: Source,
}

#[derive(Debug)]
pub(crate) enum OfferError {
    Negotiation(webrtc::error::Error),
    GatheringTimedOut,
    NoLocalDescription,
    NoNegotiatedCodec,
    Cancelled,
}

impl std::fmt::Display for OfferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Negotiation(error) => write!(f, "negotiation failed: {error}"),
            Self::GatheringTimedOut => write!(f, "ICE gathering timed out"),
            Self::NoLocalDescription => write!(f, "no local description after gathering"),
            Self::NoNegotiatedCodec => write!(f, "VP9 profile 1 was not negotiated"),
            Self::Cancelled => write!(f, "session cancelled"),
        }
    }
}

impl std::error::Error for OfferError {}

struct Handler {
    gathering_complete: CancellationToken,
    state_tx: watch::Sender<RTCPeerConnectionState>,
    terminal: CancellationToken,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: webrtc::peer_connection::RTCPeerConnectionIceEvent) {
        println!(
            "webrtc-exploration: ICE candidate type={:?} protocol={:?}",
            event.candidate.typ, event.candidate.protocol
        );
    }

    async fn on_ice_connection_state_change(
        &self,
        state: webrtc::peer_connection::RTCIceConnectionState,
    ) {
        println!("webrtc-exploration: ICE state={state:?}");
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        println!("webrtc-exploration: ICE gathering={state:?}");
        if state == RTCIceGatheringState::Complete {
            self.gathering_complete.cancel();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        println!("webrtc-exploration: peer state={state:?}");
        if self.terminal.is_cancelled() {
            return;
        }
        match state {
            RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Closed => self.terminal.cancel(),
            RTCPeerConnectionState::Unspecified
            | RTCPeerConnectionState::New
            | RTCPeerConnectionState::Connecting
            | RTCPeerConnectionState::Connected => {
                self.state_tx.send_replace(state);
            }
        };
    }
}

struct OwnedSession {
    peer_connection: Arc<dyn PeerConnection>,
    video_track: Arc<TrackLocalStaticRTP>,
    state_rx: watch::Receiver<RTCPeerConnectionState>,
    terminal: CancellationToken,
    ssrc: u32,
    payload_type: u8,
}

pub(crate) async fn run_actor(
    config: ActorConfig,
    mut requests: mpsc::Receiver<OfferRequest>,
    shutdown: CancellationToken,
) {
    let offer_idle_timeout = match config.source {
        Source::Desktop => DESKTOP_OFFER_IDLE_TIMEOUT,
        Source::Motion | Source::Chart => LOCAL_OFFER_IDLE_TIMEOUT,
    };
    let request = tokio::select! {
        request = requests.recv() => request,
        _ = tokio::time::sleep(offer_idle_timeout) => None,
        _ = shutdown.cancelled() => None,
    };
    let Some(request) = request else {
        return;
    };

    let negotiation = negotiate(request.offer, config.source, &shutdown).await;

    let (session, answer) = match negotiation {
        Ok(value) => value,
        Err(error) => {
            let _ = request.reply.send(Err(error));
            return;
        }
    };

    if request.reply.send(Ok(answer)).is_ok() {
        run_connected_session(&config, &session, &shutdown).await;
    }
    close_peer(&session.peer_connection).await;
}

async fn close_peer(peer: &Arc<dyn PeerConnection>) {
    match tokio::time::timeout(Duration::from_secs(3), peer.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("webrtc-exploration: peer close failed: {error}"),
        Err(_) => eprintln!("webrtc-exploration: peer close timed out"),
    }
}

async fn negotiate(
    offer: RTCSessionDescription,
    source: Source,
    shutdown: &CancellationToken,
) -> Result<(OwnedSession, RTCSessionDescription), OfferError> {
    let deadline = tokio::time::Instant::now() + NEGOTIATION_TIMEOUT;
    let (mut media_engine, video_codec) = build_media_engine().map_err(OfferError::Negotiation)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .map_err(OfferError::Negotiation)?;
    let ssrc = random_u32();
    let video_track = make_track(ssrc, video_codec);
    let gathering_complete = CancellationToken::new();
    let terminal = CancellationToken::new();
    let (state_tx, state_rx) = watch::channel(RTCPeerConnectionState::New);
    let handler = Arc::new(Handler {
        gathering_complete: gathering_complete.clone(),
        state_tx,
        terminal: terminal.clone(),
    });
    let (configuration, udp_addrs) = match source {
        Source::Desktop => (
            RTCConfigurationBuilder::new()
                .with_ice_servers(vec![RTCIceServer {
                    urls: vec![STUN_URL.to_owned()],
                    ..Default::default()
                }])
                .build(),
            vec!["0.0.0.0:0".to_string()],
        ),
        Source::Motion | Source::Chart => (
            RTCConfigurationBuilder::new().build(),
            vec!["127.0.0.1:0".to_string()],
        ),
    };
    let peer_connection: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(configuration)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_udp_addrs(udp_addrs)
            .build()
            .await
            .map_err(OfferError::Negotiation)?,
    );

    let setup = tokio::select! {
        result = tokio::time::timeout_at(
            deadline,
            setup_description(&peer_connection, &video_track, offer, &gathering_complete),
        ) => match result {
            Ok(result) => result,
            Err(_) => Err(OfferError::GatheringTimedOut),
        },
        _ = shutdown.cancelled() => Err(OfferError::Cancelled),
    };
    let (answer, payload_type) = match setup {
        Ok(answer) => answer,
        Err(error) => {
            close_peer(&peer_connection).await;
            return Err(error);
        }
    };
    Ok((
        OwnedSession {
            peer_connection,
            video_track,
            state_rx,
            terminal,
            ssrc,
            payload_type,
        },
        answer,
    ))
}

async fn setup_description(
    peer_connection: &Arc<dyn PeerConnection>,
    video_track: &Arc<TrackLocalStaticRTP>,
    offer: RTCSessionDescription,
    gathering_complete: &CancellationToken,
) -> Result<(RTCSessionDescription, u8), OfferError> {
    let sender = peer_connection
        .add_track(Arc::clone(video_track) as Arc<dyn TrackLocal>)
        .await
        .map_err(OfferError::Negotiation)?;
    peer_connection
        .set_remote_description(offer)
        .await
        .map_err(OfferError::Negotiation)?;
    let answer = peer_connection
        .create_answer(None)
        .await
        .map_err(OfferError::Negotiation)?;
    peer_connection
        .set_local_description(answer)
        .await
        .map_err(OfferError::Negotiation)?;
    gathering_complete.cancelled().await;
    let answer = peer_connection
        .local_description()
        .await
        .ok_or(OfferError::NoLocalDescription)?;
    let parameters = sender
        .get_parameters()
        .await
        .map_err(OfferError::Negotiation)?;
    let payload_type = parameters
        .rtp_parameters
        .codecs
        .iter()
        .find(|codec| {
            codec
                .rtp_codec
                .mime_type
                .eq_ignore_ascii_case(MIME_TYPE_VP9)
                && codec.rtp_codec.sdp_fmtp_line == VP9_FMTP
        })
        .ok_or(OfferError::NoNegotiatedCodec)?
        .payload_type;
    println!("webrtc-exploration: negotiated VP9 profile 1 payload type {payload_type}");
    Ok((answer, payload_type))
}

fn build_media_engine() -> Result<(MediaEngine, RTCRtpCodec), webrtc::error::Error> {
    let mut media_engine = MediaEngine::default();
    let codec = RTCRtpCodec {
        mime_type: MIME_TYPE_VP9.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: VP9_FMTP.to_owned(),
        rtcp_feedback: vec![],
    };
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: codec.clone(),
            payload_type: PAYLOAD_TYPE,
        },
        RtpCodecKind::Video,
    )?;
    Ok((media_engine, codec))
}

fn make_track(ssrc: u32, codec: RTCRtpCodec) -> Arc<TrackLocalStaticRTP> {
    Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
        "webrtc-exploration-stream".to_string(),
        "webrtc-exploration-video".to_string(),
        "webrtc-exploration".to_string(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec,
            ..Default::default()
        }],
    )))
}

async fn run_connected_session(
    config: &ActorConfig,
    session: &OwnedSession,
    shutdown: &CancellationToken,
) {
    let mut state_rx = session.state_rx.clone();
    let connected = tokio::select! {
        connected = wait_for_connected(&mut state_rx) => connected,
        _ = tokio::time::sleep(CONNECT_TIMEOUT) => false,
        _ = session.terminal.cancelled() => false,
        _ = shutdown.cancelled() => false,
    };
    if connected {
        run_media(config, session, shutdown).await;
    }
}

async fn wait_for_connected(state_rx: &mut watch::Receiver<RTCPeerConnectionState>) -> bool {
    loop {
        if *state_rx.borrow_and_update() == RTCPeerConnectionState::Connected {
            return true;
        }
        if state_rx.changed().await.is_err() {
            return false;
        }
    }
}

async fn run_media(config: &ActorConfig, session: &OwnedSession, shutdown: &CancellationToken) {
    let udp = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("webrtc-exploration: failed to bind RTP socket: {error}");
            return;
        }
    };
    // Request the existing gateway's bounded receive capacity for encoder bursts.
    // This is a kernel socket buffer, not an application queue of stale frames.
    let socket = socket2::SockRef::from(&udp);
    if let Err(error) = socket.set_recv_buffer_size(4 * 1024 * 1024) {
        eprintln!("webrtc-exploration: receive buffer setup failed: {error}");
        return;
    }
    println!(
        "webrtc-exploration: RTP receive buffer bytes {:?}",
        socket.recv_buffer_size()
    );
    let rtp_port = match udp.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            eprintln!("webrtc-exploration: failed to read RTP port: {error}");
            return;
        }
    };
    let mut encoder = match Encoder::spawn(
        &config.ffmpeg_bin,
        config.source,
        &EncoderConfig {
            ssrc: session.ssrc,
            rtp_port,
        },
    )
    .await
    {
        Ok(encoder) => encoder,
        Err(error) => {
            eprintln!("webrtc-exploration: failed to start ffmpeg: {error}");
            return;
        }
    };
    if let Some(pid) = encoder.pid() {
        println!(
            "webrtc-exploration: ffmpeg started (pid {pid}, source {:?})",
            config.source
        );
    }

    let media_ttl = match config.source {
        Source::Desktop => DESKTOP_MEDIA_TTL,
        Source::Motion | Source::Chart => LOCAL_MEDIA_TTL,
    };
    let ttl = tokio::time::sleep(media_ttl);
    tokio::pin!(ttl);
    let mut datagram = vec![0; 2048];
    let mut previous_sequence: Option<u16> = None;
    let mut ingress_gaps: u64 = 0;
    let mut ingress_reorders: u64 = 0;
    loop {
        let packet = tokio::select! {
            received = udp.recv_from(&mut datagram) => match received {
                Ok((length, peer)) if peer.ip().is_loopback() => parse_packet(&datagram[..length], session.ssrc),
                Ok((_, peer)) => {
                    eprintln!("webrtc-exploration: rejected RTP from non-loopback peer {peer}");
                    None
                }
                Err(error) => {
                    eprintln!("webrtc-exploration: RTP socket read failed: {error}");
                    break;
                }
            },
            status = encoder.wait() => {
                match status {
                    Ok(status) => eprintln!("webrtc-exploration: ffmpeg exited: {status}"),
                    Err(error) => eprintln!("webrtc-exploration: ffmpeg wait failed: {error}"),
                }
                break;
            }
            _ = session.terminal.cancelled() => break,
            _ = shutdown.cancelled() => break,
            _ = &mut ttl => break,
        };
        let Some(mut packet) = packet else { continue };
        if let Some(previous) = previous_sequence {
            let delta = packet.header.sequence_number.wrapping_sub(previous);
            if delta == 0 || delta > 32768 {
                ingress_reorders += 1;
            } else {
                ingress_gaps += u64::from(delta - 1);
            }
        }
        previous_sequence = Some(packet.header.sequence_number);
        // Input PT 96 was validated above. The browser chooses a different
        // dynamic payload number in SDP; map to the dependency's negotiated type.
        packet.header.payload_type = session.payload_type;
        tokio::select! {
            result = session.video_track.write_rtp(packet) => {
                if let Err(error) = result {
                    eprintln!("webrtc-exploration: write_rtp failed: {error}");
                    break;
                }
            }
            status = encoder.wait() => {
                match status {
                    Ok(status) => eprintln!("webrtc-exploration: ffmpeg exited: {status}"),
                    Err(error) => eprintln!("webrtc-exploration: ffmpeg wait failed: {error}"),
                }
                break;
            }
            _ = session.terminal.cancelled() => break,
            _ = shutdown.cancelled() => break,
            _ = &mut ttl => break,
        }
    }
    encoder.shutdown().await;
    println!(
        "webrtc-exploration: loopback RTP ingress gaps {ingress_gaps}, reorders {ingress_reorders}"
    );
}

fn parse_packet(datagram: &[u8], ssrc: u32) -> Option<rtp::packet::Packet> {
    let mut bytes = BytesMut::from(datagram);
    match rtp::packet::Packet::unmarshal(&mut bytes) {
        Ok(packet) if packet.header.ssrc != ssrc => {
            eprintln!("webrtc-exploration: rejected RTP with unexpected SSRC");
            None
        }
        Ok(packet) if packet.header.payload_type != PAYLOAD_TYPE => {
            eprintln!("webrtc-exploration: rejected RTP with unexpected payload type");
            None
        }
        Ok(packet) => Some(packet),
        Err(error) => {
            eprintln!("webrtc-exploration: RTP unmarshal failed: {error}");
            None
        }
    }
}

fn random_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    // FFmpeg's -ssrc option accepts a signed 32-bit integer. Keep this
    // single-session identifier positive and nonzero at that boundary.
    ((std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish() as u32)
        & 0x7fff_ffff)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_engine_declares_only_vp9_profile_one() {
        let (engine, codec) = build_media_engine().unwrap();
        assert_eq!(codec.mime_type, MIME_TYPE_VP9);
        assert_eq!(codec.sdp_fmtp_line, "profile-id=1");
        drop(engine);
    }

    #[tokio::test]
    async fn terminal_state_is_sticky() {
        let gathering = CancellationToken::new();
        let terminal = CancellationToken::new();
        let (tx, rx) = watch::channel(RTCPeerConnectionState::New);
        let handler = Handler {
            gathering_complete: gathering,
            state_tx: tx,
            terminal: terminal.clone(),
        };
        handler
            .on_connection_state_change(RTCPeerConnectionState::Failed)
            .await;
        handler
            .on_connection_state_change(RTCPeerConnectionState::Connected)
            .await;
        assert!(terminal.is_cancelled());
        assert_eq!(*rx.borrow(), RTCPeerConnectionState::New);
    }
}
