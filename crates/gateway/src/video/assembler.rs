use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

use sprite_desktop_protocol::browser::Continuity;
use sprite_desktop_protocol::browser::FrameKind;
use sprite_desktop_protocol::pipe::Generation;

use super::GopState;
use super::MAX_ACCESS_UNIT;
use super::rtp::AccessUnitEnd;
use super::rtp::Packet;
use super::rtp::PacketReject;
use super::rtp::RtpSequence;
use super::rtp::RtpTimestamp;

pub(super) struct Assembler {
    data: Vec<u8>,
    kind: FrameKind,
    fu_kind: Option<u8>,
    timestamp: Option<RtpTimestamp>,
    next_sequence: Option<RtpSequence>,
    generation: Option<Generation>,
    buffered_packets: u64,
    keyframe: GopState,
}

impl Default for Assembler {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            kind: FrameKind::Delta,
            fu_kind: None,
            timestamp: None,
            next_sequence: None,
            generation: None,
            buffered_packets: 0,
            keyframe: GopState::KeyframeCached,
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
    pub(super) fn consume(&mut self, packet: Packet) -> AssemblyResult {
        let mut drops = None;
        if self
            .generation
            .is_some_and(|generation| packet.generation.get() < generation.get())
        {
            // SSRC is the monotonic frame generation. A packet from an older
            // encoder must not alter access-unit or restart state.
            DroppedPackets::record(&mut drops, NonZeroU64::MIN, AssemblyDrop::StaleGeneration);
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        if self
            .generation
            .is_some_and(|generation| packet.generation.get() > generation.get())
        {
            if let Some(packets) = self.clear_access_unit() {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::GenerationDiscontinuity);
            }
            self.next_sequence = None;
            self.keyframe = GopState::Recovering;
        }
        self.generation = Some(packet.generation);
        if self
            .next_sequence
            .is_some_and(|next| next != packet.sequence)
        {
            if let Some(packets) = self.clear_access_unit() {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::SequenceDiscontinuity);
            }
            self.keyframe = GopState::Recovering;
        }
        self.next_sequence = Some(packet.sequence.next());
        if self
            .timestamp
            .is_some_and(|timestamp| timestamp != packet.timestamp)
        {
            let incomplete = self.fu_kind.is_some();
            if let Some(packets) = self.clear_access_unit() {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::TimestampDiscontinuity);
            }
            self.keyframe = GopState::Recovering;
            if incomplete {
                return AssemblyResult::rejected(PacketReject::TimestampChangedDuringFu, drops);
            }
        }
        self.timestamp = Some(packet.timestamp);
        let payload = packet.payload;
        if let Err(error) = self.append_payload(&payload) {
            if let Some(packets) = self.clear_access_unit() {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::PacketRejection);
            }
            self.keyframe = GopState::Recovering;
            return AssemblyResult::rejected(error, drops);
        }
        self.buffered_packets = self.buffered_packets.saturating_add(1);
        if packet.end == AccessUnitEnd::More {
            return AssemblyResult::accepted(AssemblyOutcome::Incomplete, drops);
        }
        if self.fu_kind.is_some() {
            if let Some(buffered) = self.clear_access_unit()
                && let Some(packets) = NonZeroU64::new(buffered.get().saturating_sub(1))
            {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::PacketRejection);
            }
            self.keyframe = GopState::Recovering;
            return AssemblyResult::rejected(PacketReject::MarkerEndedIncompleteFu, drops);
        }
        let data = std::mem::take(&mut self.data);
        let kind = std::mem::replace(&mut self.kind, FrameKind::Delta);
        let access_unit_packets = std::mem::take(&mut self.buffered_packets);
        self.timestamp = None;
        if data.is_empty() {
            if let Some(packets) = NonZeroU64::new(access_unit_packets) {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::EmptyAccessUnit);
            }
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        if self.keyframe == GopState::Recovering && kind != FrameKind::Key {
            if let Some(packets) = NonZeroU64::new(access_unit_packets) {
                DroppedPackets::record(
                    &mut drops,
                    packets,
                    AssemblyDrop::RecoveringWithoutKeyframe,
                );
            }
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        let continuity = if self.keyframe == GopState::Recovering {
            self.keyframe = GopState::KeyframeCached;
            Continuity::AfterGap
        } else {
            Continuity::Continuous
        };
        AssemblyResult::accepted(
            AssemblyOutcome::Complete(Unit {
                data: data.into(),
                kind,
                continuity,
                timestamp: packet.timestamp,
                generation: packet.generation,
            }),
            drops,
        )
    }

    fn append_payload(&mut self, payload: &[u8]) -> Result<(), PacketReject> {
        if payload.is_empty() {
            return Err(PacketReject::EmptyPayload);
        }
        let packet_type = payload[0] & 31;
        if self.fu_kind.is_some() && packet_type != 28 {
            return Err(PacketReject::NalInterleavedWithFu);
        }
        match packet_type {
            1..=23 => self.append_nal(payload),
            24 => {
                let mut rest = &payload[1..];
                while !rest.is_empty() {
                    if rest.len() < 2 {
                        return Err(PacketReject::TruncatedStapLength);
                    }
                    let len = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
                    rest = &rest[2..];
                    if len == 0 || len > rest.len() {
                        return Err(PacketReject::InvalidStapNalLength);
                    }
                    self.append_nal(&rest[..len])?;
                    rest = &rest[len..];
                }
                Ok(())
            }
            28 => self.append_fu(payload),
            kind => Err(PacketReject::UnsupportedPacketization { kind }),
        }
    }

    fn append_fu(&mut self, payload: &[u8]) -> Result<(), PacketReject> {
        if payload.len() < 3 {
            return Err(PacketReject::TruncatedFuPayload);
        }
        let start = payload[1] & 0x80 != 0;
        let end = payload[1] & 0x40 != 0;
        let reserved = payload[1] & 0x20 != 0;
        let kind = payload[1] & 31;
        if reserved || kind == 0 || kind > 23 || start && end {
            return Err(PacketReject::InvalidFuHeader);
        }
        match (start, self.fu_kind) {
            (true, None) => {
                self.push(&[0, 0, 0, 1, payload[0] & 0xe0 | kind])?;
                self.fu_kind = Some(kind);
            }
            (false, Some(active)) if active == kind => {}
            _ => return Err(PacketReject::InvalidFuSequence),
        }
        self.push(&payload[2..])?;
        if kind == 5 {
            self.kind = FrameKind::Key;
        }
        if end {
            self.fu_kind = None;
        }
        Ok(())
    }

    fn append_nal(&mut self, nal: &[u8]) -> Result<(), PacketReject> {
        if nal[0] & 31 == 5 {
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

    fn clear_access_unit(&mut self) -> Option<NonZeroU64> {
        self.data.clear();
        self.kind = FrameKind::Delta;
        self.fu_kind = None;
        self.timestamp = None;
        NonZeroU64::new(std::mem::take(&mut self.buffered_packets))
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
