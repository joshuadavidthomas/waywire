use std::net::SocketAddr;

use sprite_desktop_protocol::pipe::Generation;
use thiserror::Error;

pub(super) const RTP_CLOCK_HZ: u32 = 90_000;
const DYNAMIC_PAYLOAD_TYPE: u8 = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RtpTimestamp(pub(super) u32);

impl RtpTimestamp {
    pub(super) fn elapsed_since(self, earlier: Self) -> RtpTicks {
        RtpTicks(self.0.wrapping_sub(earlier.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RtpTicks(pub(super) u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RtpSequence(pub(super) u16);

impl RtpSequence {
    pub(super) fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NalType(u8);

impl NalType {
    pub(super) fn new(value: u8) -> Option<Self> {
        match value {
            1..=23 => Some(Self(value)),
            0 | 24..=u8::MAX => None,
        }
    }

    pub(super) fn value(self) -> u8 {
        self.0
    }

    pub(super) fn is_keyframe(self) -> bool {
        self.0 == 5
    }
}

#[derive(Debug)]
pub(super) struct Packet {
    pub(super) end: AccessUnitEnd,
    pub(super) sequence: RtpSequence,
    pub(super) timestamp: RtpTimestamp,
    pub(super) generation: Generation,
    pub(super) payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccessUnitEnd {
    Final,
    More,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum PacketReject {
    #[error("invalid RTP header")]
    InvalidHeader,
    #[error("truncated RTP CSRC list")]
    TruncatedCsrc,
    #[error("truncated RTP extension")]
    TruncatedExtension,
    #[error("RTP extension overflow")]
    ExtensionOverflow,
    #[error("truncated RTP extension payload")]
    TruncatedExtensionPayload,
    #[error("invalid RTP padding")]
    InvalidPadding,
    #[error("wrong RTP payload type or empty payload")]
    WrongPayloadTypeOrEmptyPayload,
    #[error("RTP SSRC generation must be positive")]
    ZeroGeneration,
    #[error("non-loopback RTP source {address}")]
    NonLoopbackSource { address: SocketAddr },
    #[error("RTP timestamp changed during FU-A")]
    TimestampChangedDuringFu,
    #[error("empty H.264 RTP payload")]
    EmptyPayload,
    #[error("NAL packet interleaved with FU-A")]
    NalInterleavedWithFu,
    #[error("truncated STAP-A length")]
    TruncatedStapLength,
    #[error("invalid STAP-A NAL length")]
    InvalidStapNalLength,
    #[error("unsupported H.264 packetization type {kind}")]
    UnsupportedPacketization { kind: u8 },
    #[error("truncated FU-A payload")]
    TruncatedFuPayload,
    #[error("invalid FU-A header")]
    InvalidFuHeader,
    #[error("invalid FU-A sequence")]
    InvalidFuSequence,
    #[error("RTP marker ended an incomplete FU-A")]
    MarkerEndedIncompleteFu,
    #[error("H.264 access unit exceeds byte limit")]
    AccessUnitTooLarge,
}

pub(super) fn decode_packet(data: &[u8]) -> Result<Packet, PacketReject> {
    if data.len() < 12 || data[0] >> 6 != 2 {
        return Err(PacketReject::InvalidHeader);
    }
    let mut start = 12 + usize::from(data[0] & 15) * 4;
    if start > data.len() {
        return Err(PacketReject::TruncatedCsrc);
    }
    if data[0] & 0x10 != 0 {
        if start + 4 > data.len() {
            return Err(PacketReject::TruncatedExtension);
        }
        let words = usize::from(u16::from_be_bytes([data[start + 2], data[start + 3]]));
        start = start
            .checked_add(4 + words * 4)
            .ok_or(PacketReject::ExtensionOverflow)?;
        if start > data.len() {
            return Err(PacketReject::TruncatedExtensionPayload);
        }
    }
    let mut end = data.len();
    if data[0] & 0x20 != 0 {
        let padding = usize::from(data[data.len() - 1]);
        if padding == 0 || padding > end - start {
            return Err(PacketReject::InvalidPadding);
        }
        end -= padding;
    }
    if start == end || data[1] & 0x7f != DYNAMIC_PAYLOAD_TYPE {
        return Err(PacketReject::WrongPayloadTypeOrEmptyPayload);
    }
    let generation = Generation::new(u32::from_be_bytes([data[8], data[9], data[10], data[11]]))
        .map_err(|_invalid_generation| PacketReject::ZeroGeneration)?;
    Ok(Packet {
        end: if data[1] & 0x80 == 0 {
            AccessUnitEnd::More
        } else {
            AccessUnitEnd::Final
        },
        sequence: RtpSequence(u16::from_be_bytes([data[2], data[3]])),
        timestamp: RtpTimestamp(u32::from_be_bytes([data[4], data[5], data[6], data[7]])),
        generation,
        payload: data[start..end].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_decoder_parses_ssrc_as_a_generation() {
        let mut bytes = vec![0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1];
        let decoded =
            decode_packet(&bytes).expect("positive RTP SSRC should be a valid generation");
        assert_eq!(decoded.generation.get(), 7);

        bytes[11] = 0;
        assert!(decode_packet(&bytes).is_err());
    }
}
