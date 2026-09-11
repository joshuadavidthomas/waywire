use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

use sprite_desktop_protocol::browser::Continuity;
use sprite_desktop_protocol::browser::FrameKind;
use sprite_desktop_protocol::pipe::Generation;

use super::GopState;
use super::MAX_ACCESS_UNIT;
use super::rtp::AccessUnitEnd;
use super::rtp::NalType;
use super::rtp::Packet;
use super::rtp::PacketReject;
use super::rtp::RtpSequence;
use super::rtp::RtpTimestamp;

pub(super) struct Assembler {
    cursor: StreamCursor,
    keyframe: GopState,
    unit: AccessUnit,
}

enum StreamCursor {
    Fresh,
    Streaming {
        generation: Generation,
        next_sequence: RtpSequence,
    },
}

enum AccessUnit {
    Idle,
    Collecting(PartialUnit),
}

struct PartialUnit {
    timestamp: RtpTimestamp,
    kind: FrameKind,
    data: Vec<u8>,
    packets: NonZeroU64,
    fragment: Option<NalType>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self {
            cursor: StreamCursor::Fresh,
            keyframe: GopState::KeyframeCached,
            unit: AccessUnit::Idle,
        }
    }
}

#[derive(Debug)]
pub(super) struct Unit {
    pub(super) data: Arc<[u8]>,
    pub(super) kind: FrameKind,
    pub(super) continuity: Continuity,
    pub(super) timestamp: RtpTimestamp,
    pub(super) generation: Generation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AssemblyDrop {
    EmptyAccessUnit,
    GenerationDiscontinuity,
    PacketRejection,
    RecoveringWithoutKeyframe,
    SequenceDiscontinuity,
    StaleGeneration,
    TimestampDiscontinuity,
}

impl fmt::Display for AssemblyDrop {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyAccessUnit => formatter.write_str("empty RTP access unit"),
            Self::GenerationDiscontinuity => {
                formatter.write_str("RTP generation changed during a buffered access unit")
            }
            Self::PacketRejection => {
                formatter.write_str("buffered RTP access unit discarded after packet rejection")
            }
            Self::RecoveringWithoutKeyframe => {
                formatter.write_str("RTP access unit dropped while awaiting a keyframe")
            }
            Self::SequenceDiscontinuity => {
                formatter.write_str("RTP sequence changed during a buffered access unit")
            }
            Self::StaleGeneration => formatter.write_str("stale RTP generation"),
            Self::TimestampDiscontinuity => {
                formatter.write_str("RTP timestamp changed during a buffered access unit")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PacketFailure {
    Drop(AssemblyDrop),
    Reject(PacketReject),
}

impl fmt::Display for PacketFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Drop(reason) => reason.fmt(formatter),
            Self::Reject(reason) => reason.fmt(formatter),
        }
    }
}

#[derive(Debug)]
pub(super) enum AssemblyOutcome {
    Incomplete,
    Complete(Unit),
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DroppedPackets {
    pub(super) packets: NonZeroU64,
    pub(super) reason: AssemblyDrop,
}

impl DroppedPackets {
    fn record(slot: &mut Option<Self>, packets: NonZeroU64, reason: AssemblyDrop) {
        let packets = slot.map_or(packets, |dropped| {
            dropped.packets.saturating_add(packets.get())
        });
        *slot = Some(Self { packets, reason });
    }
}

#[derive(Debug)]
pub(super) struct AssemblyResult {
    pub(super) outcome: Result<AssemblyOutcome, PacketReject>,
    pub(super) drops: Option<DroppedPackets>,
}

impl AssemblyResult {
    fn accepted(outcome: AssemblyOutcome, drops: Option<DroppedPackets>) -> Self {
        Self {
            outcome: Ok(outcome),
            drops,
        }
    }

    fn rejected(error: PacketReject, drops: Option<DroppedPackets>) -> Self {
        Self {
            outcome: Err(error),
            drops,
        }
    }
}

impl Assembler {
    fn drop_unit(&mut self, drops: &mut Option<DroppedPackets>, reason: AssemblyDrop) {
        if let Some(packets) = self.unit.discard() {
            DroppedPackets::record(drops, packets, reason);
        }
        self.keyframe = GopState::Recovering;
    }

    pub(super) fn consume(&mut self, packet: Packet) -> AssemblyResult {
        let mut drops = None;
        match self.cursor.advance(packet.generation, packet.sequence) {
            CursorAdvance::Stale => {
                // SSRC is the monotonic frame generation. A packet from an older
                // encoder must not alter access-unit or restart state.
                DroppedPackets::record(&mut drops, NonZeroU64::MIN, AssemblyDrop::StaleGeneration);
                return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
            }
            CursorAdvance::GenerationChanged => {
                self.drop_unit(&mut drops, AssemblyDrop::GenerationDiscontinuity);
            }
            CursorAdvance::SequenceChanged => {
                self.drop_unit(&mut drops, AssemblyDrop::SequenceDiscontinuity);
            }
            CursorAdvance::Started | CursorAdvance::Continuous => {}
        }
        if let AccessUnit::Collecting(unit) = &self.unit
            && unit.timestamp != packet.timestamp
        {
            let fragment = unit.fragment;
            self.drop_unit(&mut drops, AssemblyDrop::TimestampDiscontinuity);
            if fragment.is_some() {
                return AssemblyResult::rejected(PacketReject::TimestampChangedDuringFu, drops);
            }
        }
        let payload = packet.payload;
        let unit = match self.unit.append(packet.timestamp, &payload, packet.end) {
            Ok(UnitAppend::Incomplete) => {
                return AssemblyResult::accepted(AssemblyOutcome::Incomplete, drops);
            }
            Ok(UnitAppend::Complete(unit)) => unit,
            Err(rejection) => {
                if let Some(packets) = rejection.buffered {
                    DroppedPackets::record(&mut drops, packets, AssemblyDrop::PacketRejection);
                }
                self.keyframe = GopState::Recovering;
                return AssemblyResult::rejected(rejection.reason, drops);
            }
        };
        if unit.data.is_empty() {
            DroppedPackets::record(&mut drops, unit.packets, AssemblyDrop::EmptyAccessUnit);
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        let continuity = match (self.keyframe, unit.kind) {
            (GopState::Recovering, FrameKind::Delta) => {
                DroppedPackets::record(
                    &mut drops,
                    unit.packets,
                    AssemblyDrop::RecoveringWithoutKeyframe,
                );
                return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
            }
            (GopState::Recovering, FrameKind::Key) => {
                self.keyframe = GopState::KeyframeCached;
                Continuity::AfterGap
            }
            (GopState::KeyframeCached, FrameKind::Delta | FrameKind::Key) => Continuity::Continuous,
        };
        AssemblyResult::accepted(
            AssemblyOutcome::Complete(Unit {
                data: unit.data.into(),
                kind: unit.kind,
                continuity,
                timestamp: unit.timestamp,
                generation: packet.generation,
            }),
            drops,
        )
    }
}

enum CursorAdvance {
    Started,
    Continuous,
    GenerationChanged,
    SequenceChanged,
    Stale,
}

impl StreamCursor {
    fn advance(&mut self, generation: Generation, sequence: RtpSequence) -> CursorAdvance {
        match *self {
            Self::Fresh => {
                *self = Self::Streaming {
                    generation,
                    next_sequence: sequence.next(),
                };
                CursorAdvance::Started
            }
            Self::Streaming {
                generation: current,
                ..
            } if generation.get() < current.get() => CursorAdvance::Stale,
            Self::Streaming {
                generation: current,
                ..
            } if generation.get() > current.get() => {
                *self = Self::Streaming {
                    generation,
                    next_sequence: sequence.next(),
                };
                CursorAdvance::GenerationChanged
            }
            Self::Streaming { next_sequence, .. } => {
                *self = Self::Streaming {
                    generation,
                    next_sequence: sequence.next(),
                };
                if sequence == next_sequence {
                    CursorAdvance::Continuous
                } else {
                    CursorAdvance::SequenceChanged
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum FuPosition {
    Start,
    Middle,
    End,
}

struct RejectedPacket {
    reason: PacketReject,
    buffered: Option<NonZeroU64>,
}

enum UnitAppend {
    Incomplete,
    Complete(PartialUnit),
}

impl AccessUnit {
    fn discard(&mut self) -> Option<NonZeroU64> {
        let packets = match self {
            Self::Idle => None,
            Self::Collecting(unit) => Some(unit.packets),
        };
        *self = Self::Idle;
        packets
    }

    fn append(
        &mut self,
        timestamp: RtpTimestamp,
        payload: &[u8],
        end: AccessUnitEnd,
    ) -> Result<UnitAppend, RejectedPacket> {
        let current = std::mem::replace(self, Self::Idle);
        let unit = match current {
            Self::Idle => {
                PartialUnit::start(timestamp, payload).map_err(|reason| RejectedPacket {
                    reason,
                    buffered: None,
                })?
            }
            Self::Collecting(mut unit) => {
                let buffered = Some(unit.packets);
                unit.append_payload(payload)
                    .map_err(|reason| RejectedPacket { reason, buffered })?;
                unit.packets = unit.packets.saturating_add(1);
                unit
            }
        };
        match (end, unit.fragment) {
            (AccessUnitEnd::More, None | Some(_)) => {
                *self = Self::Collecting(unit);
                Ok(UnitAppend::Incomplete)
            }
            (AccessUnitEnd::Final, None) => Ok(UnitAppend::Complete(unit)),
            (AccessUnitEnd::Final, Some(_)) => Err(RejectedPacket {
                reason: PacketReject::MarkerEndedIncompleteFu,
                buffered: NonZeroU64::new(unit.packets.get().saturating_sub(1)),
            }),
        }
    }
}

impl PartialUnit {
    fn start(timestamp: RtpTimestamp, payload: &[u8]) -> Result<Self, PacketReject> {
        let mut unit = Self {
            timestamp,
            kind: FrameKind::Delta,
            data: Vec::new(),
            packets: NonZeroU64::MIN,
            fragment: None,
        };
        unit.append_payload(payload)?;
        Ok(unit)
    }

    fn append_payload(&mut self, payload: &[u8]) -> Result<(), PacketReject> {
        let Some(header) = payload.first() else {
            return Err(PacketReject::EmptyPayload);
        };
        let packet_type = header & 31;
        if self.fragment.is_some() && packet_type != 28 {
            return Err(PacketReject::NalInterleavedWithFu);
        }
        match packet_type {
            1..=23 => self.append_nal(payload),
            24 => self.append_stap(&payload[1..]),
            28 => self.append_fu(payload),
            kind => Err(PacketReject::UnsupportedPacketization { kind }),
        }
    }

    fn append_stap(&mut self, mut payload: &[u8]) -> Result<(), PacketReject> {
        while !payload.is_empty() {
            if payload.len() < 2 {
                return Err(PacketReject::TruncatedStapLength);
            }
            let length = usize::from(u16::from_be_bytes([payload[0], payload[1]]));
            payload = &payload[2..];
            if length == 0 || length > payload.len() {
                return Err(PacketReject::InvalidStapNalLength);
            }
            self.append_nal(&payload[..length])?;
            payload = &payload[length..];
        }
        Ok(())
    }

    fn append_fu(&mut self, payload: &[u8]) -> Result<(), PacketReject> {
        if payload.len() < 3 {
            return Err(PacketReject::TruncatedFuPayload);
        }
        if payload[1] & 0x20 != 0 {
            return Err(PacketReject::InvalidFuHeader);
        }
        let Some(kind) = NalType::new(payload[1] & 31) else {
            return Err(PacketReject::InvalidFuHeader);
        };
        let position = match (payload[1] & 0x80 != 0, payload[1] & 0x40 != 0) {
            (true, true) => return Err(PacketReject::InvalidFuHeader),
            (true, false) => FuPosition::Start,
            (false, true) => FuPosition::End,
            (false, false) => FuPosition::Middle,
        };
        match (position, self.fragment) {
            (FuPosition::Start, None) => {
                self.push(&[0, 0, 0, 1, payload[0] & 0xe0 | kind.value()])?;
                self.fragment = Some(kind);
            }
            (FuPosition::Start, Some(_)) | (FuPosition::Middle | FuPosition::End, None) => {
                return Err(PacketReject::InvalidFuSequence);
            }
            (FuPosition::Middle | FuPosition::End, Some(active)) => {
                if active != kind {
                    return Err(PacketReject::InvalidFuSequence);
                }
            }
        }
        self.push(&payload[2..])?;
        if kind.is_keyframe() {
            self.kind = FrameKind::Key;
        }
        match position {
            FuPosition::Start | FuPosition::Middle => {}
            FuPosition::End => self.fragment = None,
        }
        Ok(())
    }

    fn append_nal(&mut self, nal: &[u8]) -> Result<(), PacketReject> {
        // STAP-A lengths delimit opaque member bytes, so a member type outside
        // `NalType` is appended, not rejected; only known types are classified.
        if NalType::new(nal[0] & 31).is_some_and(NalType::is_keyframe) {
            self.kind = FrameKind::Key;
        }
        self.push(&[0, 0, 0, 1])?;
        self.push(nal)
    }

    fn push(&mut self, value: &[u8]) -> Result<(), PacketReject> {
        if self.data.len().saturating_add(value.len()) > MAX_ACCESS_UNIT {
            return Err(PacketReject::AccessUnitTooLarge);
        }
        self.data.extend_from_slice(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::rtp::AccessUnitEnd;
    use crate::video::tests::consume;
    use crate::video::tests::packet;

    #[test]
    fn emits_final_access_unit_at_marker() {
        let mut assembler = Assembler::default();
        assert!(
            consume(
                &mut assembler,
                packet(1, 90, 7, AccessUnitEnd::More, &[0x7c, 0x85, 1])
            )
            .is_none()
        );
        let value = consume(
            &mut assembler,
            packet(2, 90, 7, AccessUnitEnd::Final, &[0x7c, 0x45, 2]),
        )
        .expect("marker packet should complete the fragmented test access unit");
        assert_eq!(value.kind, FrameKind::Key);
        assert_eq!(&*value.data, [0, 0, 0, 1, 0x65, 1, 2]);
    }

    #[test]
    fn generation_restart_stays_sticky_across_fragments() {
        let mut assembler = Assembler::default();
        let _ = consume(
            &mut assembler,
            packet(1, 1, 1, AccessUnitEnd::Final, &[5, 1]),
        );
        assert!(
            consume(
                &mut assembler,
                packet(2, 2, 2, AccessUnitEnd::More, &[0x7c, 0x85, 2])
            )
            .is_none()
        );
        let value = consume(
            &mut assembler,
            packet(3, 2, 2, AccessUnitEnd::Final, &[0x7c, 0x45, 3]),
        )
        .expect("final fragment should complete the replacement generation");
        assert_eq!(value.continuity, Continuity::AfterGap);
    }

    #[test]
    fn generation_change_drops_open_fragment_before_rejecting_continuation() {
        let mut assembler = Assembler::default();
        let started = assembler.consume(packet(1, 1, 1, AccessUnitEnd::More, &[0x7c, 0x81, 1]));
        assert!(matches!(started.outcome, Ok(AssemblyOutcome::Incomplete)));

        let changed = assembler.consume(packet(2, 1, 2, AccessUnitEnd::Final, &[0x7c, 0x41, 2]));
        assert!(matches!(
            changed.outcome,
            Err(PacketReject::InvalidFuSequence)
        ));
        assert_eq!(
            changed.drops,
            Some(DroppedPackets {
                packets: NonZeroU64::MIN,
                reason: AssemblyDrop::GenerationDiscontinuity,
            })
        );
    }

    #[test]
    fn sequence_gap_drops_open_fragment_before_rejecting_continuation() {
        let mut assembler = Assembler::default();
        let started = assembler.consume(packet(1, 1, 1, AccessUnitEnd::More, &[0x7c, 0x81, 1]));
        assert!(matches!(started.outcome, Ok(AssemblyOutcome::Incomplete)));

        let changed = assembler.consume(packet(3, 1, 1, AccessUnitEnd::Final, &[0x7c, 0x41, 2]));
        assert!(matches!(
            changed.outcome,
            Err(PacketReject::InvalidFuSequence)
        ));
        assert_eq!(
            changed.drops,
            Some(DroppedPackets {
                packets: NonZeroU64::MIN,
                reason: AssemblyDrop::SequenceDiscontinuity,
            })
        );
    }

    #[test]
    fn rejects_invalid_fu_boundaries() {
        let mut assembler = Assembler::default();
        assert!(
            assembler
                .consume(packet(1, 1, 1, AccessUnitEnd::Final, &[0x7c, 0xc5, 1]))
                .outcome
                .is_err()
        );
        assert!(
            consume(
                &mut assembler,
                packet(2, 2, 1, AccessUnitEnd::More, &[0x7c, 0x85, 1])
            )
            .is_none()
        );
        assert!(
            assembler
                .consume(packet(3, 2, 1, AccessUnitEnd::Final, &[0x7c, 0x41, 2]))
                .outcome
                .is_err()
        );
    }

    #[test]
    fn assembler_distinguishes_incomplete_recovery_and_stale_packets() {
        let mut assembler = Assembler::default();
        let incomplete = assembler.consume(packet(1, 1, 2, AccessUnitEnd::More, &[1, 1]));
        assert!(matches!(
            incomplete.outcome,
            Ok(AssemblyOutcome::Incomplete)
        ));

        let complete = assembler.consume(packet(2, 1, 2, AccessUnitEnd::Final, &[5, 2]));
        assert!(matches!(complete.outcome, Ok(AssemblyOutcome::Complete(_))));
        let stale = assembler.consume(packet(3, 2, 1, AccessUnitEnd::Final, &[5, 3]));
        assert!(matches!(stale.outcome, Ok(AssemblyOutcome::Dropped)));
        assert_eq!(
            stale
                .drops
                .expect("stale packet should be counted")
                .packets
                .get(),
            1
        );

        let recovering = assembler.consume(packet(4, 2, 2, AccessUnitEnd::Final, &[1, 4]));
        assert!(matches!(recovering.outcome, Ok(AssemblyOutcome::Dropped)));
        assert_eq!(
            recovering
                .drops
                .expect("recovery drop should be counted")
                .packets
                .get(),
            1
        );
    }

    #[test]
    fn fragmented_recovery_drop_counts_every_packet_in_the_access_unit() {
        let mut assembler = Assembler::default();
        assert!(
            consume(
                &mut assembler,
                packet(1, 1, 1, AccessUnitEnd::Final, &[5, 1])
            )
            .is_some()
        );

        let first_fragment =
            assembler.consume(packet(3, 2, 1, AccessUnitEnd::More, &[0x7c, 0x81, 2]));
        assert!(matches!(
            first_fragment.outcome,
            Ok(AssemblyOutcome::Incomplete)
        ));
        assert_eq!(first_fragment.drops, None);

        let final_fragment =
            assembler.consume(packet(4, 2, 1, AccessUnitEnd::Final, &[0x7c, 0x41, 3]));
        assert!(matches!(
            final_fragment.outcome,
            Ok(AssemblyOutcome::Dropped)
        ));
        assert_eq!(
            final_fragment.drops,
            Some(DroppedPackets {
                packets: NonZeroU64::new(2).expect("test count should be nonzero"),
                reason: AssemblyDrop::RecoveringWithoutKeyframe,
            })
        );
    }

    #[test]
    fn discontinuities_count_packets_cleared_from_the_access_unit() {
        let mut assembler = Assembler::default();
        let buffered = assembler.consume(packet(1, 1, 1, AccessUnitEnd::More, &[1, 1]));
        assert!(matches!(buffered.outcome, Ok(AssemblyOutcome::Incomplete)));

        let sequence_change = assembler.consume(packet(3, 1, 1, AccessUnitEnd::Final, &[5, 2]));
        assert!(matches!(
            sequence_change.outcome,
            Ok(AssemblyOutcome::Complete(_))
        ));
        assert_eq!(
            sequence_change.drops,
            Some(DroppedPackets {
                packets: NonZeroU64::MIN,
                reason: AssemblyDrop::SequenceDiscontinuity,
            })
        );

        let buffered = assembler.consume(packet(4, 2, 1, AccessUnitEnd::More, &[1, 3]));
        assert!(matches!(buffered.outcome, Ok(AssemblyOutcome::Incomplete)));

        let timestamp_change = assembler.consume(packet(5, 3, 1, AccessUnitEnd::Final, &[5, 4]));
        assert!(matches!(
            timestamp_change.outcome,
            Ok(AssemblyOutcome::Complete(_))
        ));
        assert_eq!(
            timestamp_change.drops,
            Some(DroppedPackets {
                packets: NonZeroU64::MIN,
                reason: AssemblyDrop::TimestampDiscontinuity,
            })
        );
    }

    #[test]
    fn loss_and_sequence_wrap_recover_at_keyframe() {
        let mut assembler = Assembler::default();
        assert!(
            consume(
                &mut assembler,
                packet(u16::MAX, 1, 1, AccessUnitEnd::Final, &[1, 1]),
            )
            .is_some()
        );
        assert!(
            consume(
                &mut assembler,
                packet(0, 2, 1, AccessUnitEnd::Final, &[1, 2])
            )
            .is_some()
        );
        assert!(
            consume(
                &mut assembler,
                packet(2, 3, 1, AccessUnitEnd::Final, &[1, 3])
            )
            .is_none()
        );
        assert_eq!(
            consume(
                &mut assembler,
                packet(3, 4, 1, AccessUnitEnd::Final, &[5, 4])
            )
            .expect("keyframe should complete sequence-loss recovery")
            .continuity,
            Continuity::AfterGap
        );
    }
}
