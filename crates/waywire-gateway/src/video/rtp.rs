use std::net::SocketAddr;

use thiserror::Error;
use waywire_protocol::pipe::Generation;

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
    use proptest::prelude::*;
    use proptest::sample::Index;

    use super::*;

    /// An RTP packet laid out by RFC 3550 section 5.1, written here rather
    /// than through the decoder under test.
    #[derive(Clone, Debug)]
    struct Layout {
        marker: bool,
        sequence: u16,
        timestamp: u32,
        ssrc: u32,
        csrcs: Vec<u32>,
        extension: Option<(u16, Vec<u32>)>,
        /// Padding bytes after the payload, the last holding their count.
        padding: u8,
        payload: Vec<u8>,
    }

    impl Layout {
        fn bytes(&self) -> Vec<u8> {
            let count = u8::try_from(self.csrcs.len()).expect("at most 15 CSRCs");
            let mut bytes = vec![
                0x80 | (u8::from(self.padding > 0) << 5)
                    | (u8::from(self.extension.is_some()) << 4)
                    | count,
                (u8::from(self.marker) << 7) | 0x60, // payload type 96
            ];
            bytes.extend(self.sequence.to_be_bytes());
            bytes.extend(self.timestamp.to_be_bytes());
            bytes.extend(self.ssrc.to_be_bytes());
            for csrc in &self.csrcs {
                bytes.extend(csrc.to_be_bytes());
            }
            if let Some((profile, words)) = &self.extension {
                bytes.extend(profile.to_be_bytes());
                bytes.extend(
                    u16::try_from(words.len())
                        .expect("short extension")
                        .to_be_bytes(),
                );
                for word in words {
                    bytes.extend(word.to_be_bytes());
                }
            }
            bytes.extend(&self.payload);
            if self.padding > 0 {
                bytes.extend(std::iter::repeat_n(0, usize::from(self.padding) - 1));
                bytes.push(self.padding);
            }
            bytes
        }

        /// Where the CSRC list ends and where the payload starts.
        fn offsets(&self) -> (usize, usize) {
            let csrc_end = 12 + 4 * self.csrcs.len();
            let payload = csrc_end
                + self
                    .extension
                    .as_ref()
                    .map_or(0, |(_, words)| 4 + 4 * words.len());
            (csrc_end, payload)
        }
    }

    fn layouts() -> impl Strategy<Value = Layout> {
        (
            any::<bool>(),
            any::<u16>(),
            any::<u32>(),
            1..=u32::MAX,
            proptest::collection::vec(any::<u32>(), 0..=15),
            proptest::option::of((any::<u16>(), proptest::collection::vec(any::<u32>(), 0..6))),
            prop_oneof![Just(0_u8), any::<u8>()],
            proptest::collection::vec(any::<u8>(), 1..48),
        )
            .prop_map(
                |(marker, sequence, timestamp, ssrc, csrcs, extension, padding, payload)| Layout {
                    marker,
                    sequence,
                    timestamp,
                    ssrc,
                    csrcs,
                    extension,
                    padding,
                    payload,
                },
            )
    }

    proptest! {
        #[test]
        fn rtp_packets_decode_to_the_fields_they_were_built_from(layout in layouts()) {
            let packet = decode_packet(&layout.bytes()).expect("well-formed RTP packet");
            prop_assert_eq!(&packet.payload, &layout.payload);
            prop_assert_eq!(packet.sequence, RtpSequence(layout.sequence));
            prop_assert_eq!(packet.timestamp, RtpTimestamp(layout.timestamp));
            prop_assert_eq!(packet.generation.get(), layout.ssrc);
            prop_assert_eq!(
                packet.end,
                if layout.marker { AccessUnitEnd::Final } else { AccessUnitEnd::More }
            );
        }

        #[test]
        fn rtp_packets_cut_before_their_payload_name_the_part_cut(
            layout in layouts(),
            cut in any::<Index>(),
        ) {
            let bytes = layout.bytes();
            let (csrc_end, payload) = layout.offsets();
            let length = cut.index(payload);
            let expected = if length < 12 {
                PacketReject::InvalidHeader
            } else if length < csrc_end {
                PacketReject::TruncatedCsrc
            } else if length < csrc_end + 4 {
                PacketReject::TruncatedExtension
            } else {
                PacketReject::TruncatedExtensionPayload
            };
            prop_assert_eq!(decode_packet(&bytes[..length]).err(), Some(expected));
        }

        #[test]
        fn rtp_padding_must_leave_a_payload(layout in layouts(), excess in 1..=255_usize) {
            let mut bytes = layout.bytes();
            let (_, payload) = layout.offsets();
            // Mark the whole packet body as padding, or more than all of it.
            bytes[0] |= 0x20;
            let available = bytes.len() - payload;
            if let Ok(count) = u8::try_from(available) {
                *bytes.last_mut().expect("payload") = count;
                prop_assert_eq!(
                    decode_packet(&bytes).err(),
                    Some(PacketReject::WrongPayloadTypeOrEmptyPayload)
                );
            }
            if let Ok(count) = u8::try_from(available + excess) {
                *bytes.last_mut().expect("payload") = count;
                prop_assert_eq!(decode_packet(&bytes).err(), Some(PacketReject::InvalidPadding));
            }
            *bytes.last_mut().expect("payload") = 0;
            prop_assert_eq!(decode_packet(&bytes).err(), Some(PacketReject::InvalidPadding));
        }

        #[test]
        fn rtp_packets_with_another_version_payload_type_or_no_ssrc_are_rejected(
            layout in layouts(),
            version in 0..=3_u8,
            payload_type in 0..=127_u8,
        ) {
            let bytes = layout.bytes();
            let mut changed = bytes.clone();
            changed[0] = (changed[0] & 0x3f) | (version << 6);
            prop_assert_eq!(
                decode_packet(&changed).is_err_and(|error| error == PacketReject::InvalidHeader),
                version != 2
            );
            let mut changed = bytes.clone();
            changed[1] = (changed[1] & 0x80) | payload_type;
            prop_assert_eq!(
                decode_packet(&changed).err(),
                (payload_type != 96).then_some(PacketReject::WrongPayloadTypeOrEmptyPayload)
            );
            let mut changed = bytes;
            changed[8..12].fill(0);
            prop_assert_eq!(decode_packet(&changed).err(), Some(PacketReject::ZeroGeneration));
        }
    }

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
