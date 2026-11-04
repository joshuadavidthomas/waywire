//! Desktop audio from the compositor to viewers.
//!
//! The compositor sends each 20 ms of desktop audio as one Opus packet in RTP
//! payload type 111 to the loopback port it shares with video, and the RTP
//! receiver passes those packets here. Every `/audio` socket gets each packet
//! as one binary message holding the bare Opus packet. A viewer that falls
//! behind loses packets instead of delaying the ones after them, and a viewer
//! that joins starts with the next packet.

use axum::body::Bytes;
use tokio::sync::broadcast;

/// RTP payload type of the compositor's Opus packets.
const PAYLOAD_TYPE: u8 = 111;
const RTP_HEADER_BYTES: usize = 12;
/// Packets a viewer may fall behind before it loses some: 320 ms.
const VIEWER_PACKETS: usize = 16;

#[derive(Clone)]
pub(crate) struct AudioHub(broadcast::Sender<Bytes>);

impl Default for AudioHub {
    fn default() -> Self {
        Self(broadcast::channel(VIEWER_PACKETS).0)
    }
}

impl AudioHub {
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.0.subscribe()
    }

    /// Pass the Opus packet in an audio RTP packet to every viewer.
    pub(crate) fn offer_rtp(&self, packet: &[u8]) -> RtpClaim {
        if packet.len() < RTP_HEADER_BYTES || packet[1] & 0x7f != PAYLOAD_TYPE {
            return RtpClaim::NotAudio;
        }
        // The compositor writes a bare version 2 header: no padding,
        // extension or CSRCs.
        if packet[0] == 0x80 && packet.len() > RTP_HEADER_BYTES {
            // No viewers is not an error.
            drop(
                self.0
                    .send(Bytes::copy_from_slice(&packet[RTP_HEADER_BYTES..])),
            );
        }
        RtpClaim::Audio
    }
}

/// Whose an RTP packet is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RtpClaim {
    /// The audio payload type: forwarded to viewers, or discarded when its header
    /// is not the compositor's bare one.
    Audio,
    /// Video's to decode.
    NotAudio,
}
