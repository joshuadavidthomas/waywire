use std::collections::VecDeque;
use std::num::NonZeroUsize;

use sprite_desktop_protocol::browser::Continuity;
use sprite_desktop_protocol::browser::FrameKind;
use sprite_desktop_protocol::browser::VideoSample;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::FrameMetadata;
use sprite_desktop_protocol::pipe::Generation;
use thiserror::Error;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;

use super::GopState;
use super::UNMATCHED_DEADLINE;
use super::assembler::Unit;
use super::rtp::RTP_CLOCK_HZ;
use super::rtp::RtpTicks;
use super::rtp::RtpTimestamp;

pub(super) const PENDING_RECORD_CAPACITY: usize = 120;
pub(super) const MAX_PENDING_UNIT_BYTES: usize = 32 << 20;

struct Pending<T> {
    value: T,
    queued_at: Instant,
}

pub(super) struct PendingUnit {
    pub(super) unit: Unit,
    pub(super) bytes: OwnedSemaphorePermit,
}

#[derive(Debug, PartialEq, Eq)]
enum MetadataMatch {
    Matched {
        metadata: FrameMetadata,
        unmatched: usize,
    },
    NotYetArrived,
    GenerationPassed,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub(super) enum ResyncReason {
    #[error("metadata count limit exceeded")]
    MetadataBacklog,
    #[error("RTP unit count limit exceeded")]
    UnitBacklog,
    #[error("video metadata/RTP correlation stalled")]
    UnmatchedDeadline,
    #[error("RTP timestamp gap {ticks:?} exceeds correlation window at {fps:?}")]
    TimestampGap { ticks: RtpTicks, fps: Fps },
    #[error("metadata generation passed the pending RTP unit generation")]
    MetadataGenerationPassed,
}

#[must_use]
#[derive(Debug)]
pub(super) struct Correlation {
    pub(super) outcome: CorrelationOutcome,
    pub(super) unmatched_metadata: usize,
}

#[derive(Debug)]
pub(super) enum CorrelationOutcome {
    Frames(Vec<VideoSample>),
    Resync(ResyncReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimestampAnchor {
    Needed,
    Present { timestamp: RtpTimestamp, fps: Fps },
}

pub(super) struct Correlator {
    metadata: VecDeque<Pending<FrameMetadata>>,
    units: VecDeque<Pending<PendingUnit>>,
    last_generation: Option<Generation>,
    timestamp_anchor: TimestampAnchor,
    keyframe: GopState,
}

impl Correlator {
    pub(super) fn new() -> Self {
        Self {
            metadata: VecDeque::new(),
            units: VecDeque::new(),
            last_generation: None,
            timestamp_anchor: TimestampAnchor::Needed,
            keyframe: GopState::KeyframeCached,
        }
    }

    pub(super) fn push_metadata(&mut self, value: FrameMetadata) -> Correlation {
        if self.metadata.len() == PENDING_RECORD_CAPACITY {
            return self.resync(ResyncReason::MetadataBacklog, 1);
        }
        self.metadata.push_back(Pending {
            value,
            queued_at: Instant::now(),
        });
        self.drain()
    }

    pub(super) fn push_unit(&mut self, value: PendingUnit) -> Correlation {
        if self.units.len() == PENDING_RECORD_CAPACITY {
            return self.resync(ResyncReason::UnitBacklog, 0);
        }
        self.units.push_back(Pending {
            value,
            queued_at: Instant::now(),
        });
        self.drain()
    }

    pub(super) fn deadline_expired(&mut self) -> Correlation {
        self.resync(ResyncReason::UnmatchedDeadline, 0)
    }

    fn drain(&mut self) -> Correlation {
        let mut samples = Vec::new();
        let mut unmatched_metadata = 0_usize;
        loop {
            let Some(pending) = self.units.pop_front() else {
                break;
            };
            let unit = &pending.value.unit;
            let metadata_count = if self.last_generation == Some(unit.generation) {
                match self.timestamp_anchor {
                    TimestampAnchor::Needed => NonZeroUsize::MIN,
                    TimestampAnchor::Present { timestamp, fps } => {
                        let ticks = unit.timestamp.elapsed_since(timestamp);
                        let Some(gap) = metadata_gap(ticks, fps) else {
                            return self.resync(
                                ResyncReason::TimestampGap { ticks, fps },
                                unmatched_metadata,
                            );
                        };
                        gap
                    }
                }
            } else {
                NonZeroUsize::MIN
            };
            let matched = self.take_nth_generation_metadata(unit.generation, metadata_count);
            let (metadata, unmatched) = match matched {
                MetadataMatch::Matched {
                    metadata,
                    unmatched,
                } => (metadata, unmatched),
                MetadataMatch::NotYetArrived => {
                    self.units.push_front(pending);
                    break;
                }
                MetadataMatch::GenerationPassed => {
                    return self.resync(ResyncReason::MetadataGenerationPassed, unmatched_metadata);
                }
            };

            unmatched_metadata = unmatched_metadata.saturating_add(unmatched);
            let PendingUnit {
                unit,
                bytes: byte_permit,
            } = pending.value;
            drop(byte_permit);
            let generation_changed = self.last_generation != Some(metadata.generation);
            let recovering = self.keyframe == GopState::Recovering;
            let continuity = if recovering
                || unit.continuity == Continuity::AfterGap
                || unmatched > 0
                || generation_changed
            {
                Continuity::AfterGap
            } else {
                Continuity::Continuous
            };
            self.last_generation = Some(metadata.generation);
            self.timestamp_anchor = TimestampAnchor::Present {
                timestamp: unit.timestamp,
                fps: metadata.fps,
            };
            if recovering && unit.kind != FrameKind::Key {
                continue;
            }
            if recovering {
                self.keyframe = GopState::KeyframeCached;
            }
            samples.push(VideoSample {
                data: unit.data,
                kind: unit.kind,
                continuity,
                metadata,
            });
        }
        Correlation {
            outcome: CorrelationOutcome::Frames(samples),
            unmatched_metadata,
        }
    }

    fn resync(&mut self, reason: ResyncReason, unmatched_metadata: usize) -> Correlation {
        let unmatched_metadata = unmatched_metadata.saturating_add(self.metadata.len());
        self.metadata.clear();
        self.units.clear();
        self.timestamp_anchor = TimestampAnchor::Needed;
        self.keyframe = GopState::Recovering;
        Correlation {
            outcome: CorrelationOutcome::Resync(reason),
            unmatched_metadata,
        }
    }

    fn take_nth_generation_metadata(
        &mut self,
        generation: Generation,
        count: NonZeroUsize,
    ) -> MetadataMatch {
        let mut matches = 0;
        for (index, item) in self.metadata.iter().enumerate() {
            match item.value.generation.get().cmp(&generation.get()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    matches += 1;
                    if matches == count.get() {
                        let metadata = item.value.clone();
                        drop(self.metadata.drain(..=index));
                        return MetadataMatch::Matched {
                            metadata,
                            unmatched: index,
                        };
                    }
                }
                std::cmp::Ordering::Greater => return MetadataMatch::GenerationPassed,
            }
        }
        MetadataMatch::NotYetArrived
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.metadata
            .front()
            .map(|pending| pending.queued_at)
            .into_iter()
            .chain(self.units.front().map(|pending| pending.queued_at))
            .min()
            .map(|at| at + UNMATCHED_DEADLINE)
    }
}

fn metadata_gap(ticks: RtpTicks, fps: Fps) -> Option<NonZeroUsize> {
    if ticks.0 == 0 {
        return Some(NonZeroUsize::MIN);
    }
    let count = (u64::from(ticks.0) * u64::from(fps.get()) + u64::from(RTP_CLOCK_HZ / 2))
        / u64::from(RTP_CLOCK_HZ);
    if count == 0 {
        return Some(NonZeroUsize::MIN);
    }
    let count = usize::try_from(count).ok()?;
    if count > PENDING_RECORD_CAPACITY {
        return None;
    }
    NonZeroUsize::new(count)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sprite_desktop_protocol::browser::Continuity;
    use sprite_desktop_protocol::browser::FrameKind;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::video::assembler::Assembler;
    use crate::video::rtp::AccessUnitEnd;
    use crate::video::tests::consume;
    use crate::video::tests::metadata;
    use crate::video::tests::packet;
    use crate::video::tests::unit;

    fn push_metadata(correlator: &mut Correlator, value: FrameMetadata) {
        let correlation = correlator.push_metadata(value);
        let frames = match correlation.outcome {
            CorrelationOutcome::Frames(frames) => frames,
            CorrelationOutcome::Resync(reason) => {
                panic!("test metadata should not resynchronise the correlator: {reason}")
            }
        };
        assert!(frames.is_empty());
    }

    fn push_unit(correlator: &mut Correlator, value: PendingUnit) -> Vec<VideoSample> {
        let correlation = correlator.push_unit(value);
        match correlation.outcome {
            CorrelationOutcome::Frames(frames) => frames,
            CorrelationOutcome::Resync(reason) => {
                panic!("test unit should not resynchronise the correlator: {reason}")
            }
        }
    }

    fn pending(unit: Unit, budget: &Arc<Semaphore>) -> PendingUnit {
        PendingUnit {
            unit,
            bytes: Arc::clone(budget)
                .try_acquire_many_owned(1)
                .expect("test byte budget should have one available permit"),
        }
    }

    #[test]
    fn first_unit_uses_only_metadata_with_its_generation() {
        let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        let mut correlator = Correlator::new();
        push_metadata(&mut correlator, metadata(1, 1));
        push_metadata(&mut correlator, metadata(2, 2));

        let samples = push_unit(&mut correlator, pending(unit(1, 2), &budget));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].metadata.sequence, 2);
        assert_eq!(samples[0].metadata.generation.get(), 2);

        push_metadata(&mut correlator, metadata(3, 3));
        let correlation = correlator.push_unit(pending(unit(2, 2), &budget));
        assert!(matches!(
            correlation.outcome,
            CorrelationOutcome::Resync(ResyncReason::MetadataGenerationPassed)
        ));
        assert_eq!(correlation.unmatched_metadata, 1);
        assert!(correlator.deadline().is_none());
    }

    #[test]
    fn reordered_replacements_never_exchange_metadata() {
        let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        let mut correlator = Correlator::new();
        push_metadata(&mut correlator, metadata(1, 1));
        let _ = push_unit(&mut correlator, pending(unit(3_000, 1), &budget));
        push_metadata(&mut correlator, metadata(2, 2));
        push_metadata(&mut correlator, metadata(3, 3));

        let samples = push_unit(&mut correlator, pending(unit(1, 3), &budget));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].metadata.sequence, 3);
        assert_eq!(samples[0].metadata.generation.get(), 3);

        push_metadata(&mut correlator, metadata(4, 4));
        let correlation = correlator.push_unit(pending(unit(2, 3), &budget));
        assert!(matches!(
            correlation.outcome,
            CorrelationOutcome::Resync(ResyncReason::MetadataGenerationPassed)
        ));
        assert_eq!(correlation.unmatched_metadata, 1);
        assert!(correlator.deadline().is_none());
    }

    #[test]
    fn stale_generation_packet_cannot_poison_current_assembler_state() {
        let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        let mut assembler = Assembler::default();
        let mut correlator = Correlator::new();

        push_metadata(&mut correlator, metadata(1, 1));
        let first = consume(
            &mut assembler,
            packet(1, 3_000, 1, AccessUnitEnd::Final, &[5, 1]),
        )
        .expect("first generation packet should complete an access unit");
        let _ = push_unit(&mut correlator, pending(first, &budget));

        push_metadata(&mut correlator, metadata(2, 2));
        let replacement = consume(
            &mut assembler,
            packet(1, 3_000, 2, AccessUnitEnd::Final, &[5, 2]),
        )
        .expect("replacement generation packet should complete an access unit");
        let _ = push_unit(&mut correlator, pending(replacement, &budget));
        push_metadata(&mut correlator, metadata(3, 2));

        assert!(
            consume(
                &mut assembler,
                packet(2, 6_000, 1, AccessUnitEnd::Final, &[5, 3])
            )
            .is_none()
        );
        let current = consume(
            &mut assembler,
            packet(2, 6_000, 2, AccessUnitEnd::Final, &[5, 4]),
        )
        .expect("current generation packet should complete an access unit");
        assert_eq!(current.continuity, Continuity::Continuous);
        let samples = push_unit(&mut correlator, pending(current, &budget));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].metadata.sequence, 3);
        assert!(correlator.deadline().is_none());
    }

    #[test]
    fn generation_change_does_not_apply_timestamp_gap_across_ssrcs() {
        let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        let mut correlator = Correlator::new();
        push_metadata(&mut correlator, metadata(1, 1));
        let _ = push_unit(&mut correlator, pending(unit(3_000, 1), &budget));
        push_metadata(&mut correlator, metadata(3, 2));

        let correlation = correlator.push_unit(pending(unit(u32::MAX, 2), &budget));
        let samples = match correlation.outcome {
            CorrelationOutcome::Frames(frames) => frames,
            CorrelationOutcome::Resync(reason) => {
                panic!("generation change should correlate without resync: {reason}")
            }
        };
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].metadata.sequence, 3);
        assert_eq!(samples[0].continuity, Continuity::AfterGap);
        assert_eq!(correlation.unmatched_metadata, 0);
        assert!(correlator.deadline().is_none());
    }

    #[test]
    fn huge_timestamp_gap_fails_instead_of_guessing() {
        assert!(
            metadata_gap(
                RtpTicks(u32::MAX),
                sprite_desktop_protocol::pipe::Fps::new(60)
                    .expect("test frame rate should be valid"),
            )
            .is_none()
        );
    }

    #[test]
    fn metadata_backlog_resync_counts_every_record_and_returns_unit_permits() {
        let budget = Arc::new(Semaphore::new(1));
        let mut correlator = Correlator::new();
        assert!(push_unit(&mut correlator, pending(unit(1, 2), &budget)).is_empty());
        for sequence in 0..PENDING_RECORD_CAPACITY {
            push_metadata(
                &mut correlator,
                metadata(
                    u64::try_from(sequence).expect("test sequence should fit"),
                    1,
                ),
            );
        }

        let correlation = correlator.push_metadata(metadata(999, 1));
        assert!(matches!(
            correlation.outcome,
            CorrelationOutcome::Resync(ResyncReason::MetadataBacklog)
        ));
        assert_eq!(budget.available_permits(), 1);
        assert!(correlator.deadline().is_none());
        assert_eq!(correlation.unmatched_metadata, PENDING_RECORD_CAPACITY + 1);
    }

    #[test]
    fn unit_backlog_resync_returns_queued_and_rejected_unit_permits() {
        let budget = Arc::new(Semaphore::new(PENDING_RECORD_CAPACITY + 1));
        let mut correlator = Correlator::new();
        for timestamp in 0..PENDING_RECORD_CAPACITY {
            assert!(
                push_unit(
                    &mut correlator,
                    pending(
                        unit(
                            u32::try_from(timestamp).expect("test timestamp should fit"),
                            1
                        ),
                        &budget,
                    ),
                )
                .is_empty()
            );
        }

        let correlation = correlator.push_unit(pending(unit(999, 1), &budget));
        assert!(matches!(
            correlation.outcome,
            CorrelationOutcome::Resync(ResyncReason::UnitBacklog)
        ));
        assert_eq!(correlation.unmatched_metadata, 0);
        assert_eq!(budget.available_permits(), PENDING_RECORD_CAPACITY + 1);
        assert!(correlator.deadline().is_none());
    }

    #[test]
    fn unmatched_deadline_resync_clears_both_queues() {
        let budget = Arc::new(Semaphore::new(1));
        let mut correlator = Correlator::new();
        push_metadata(&mut correlator, metadata(1, 1));
        assert!(push_unit(&mut correlator, pending(unit(1, 2), &budget)).is_empty());

        let correlation = correlator.deadline_expired();
        assert!(matches!(
            correlation.outcome,
            CorrelationOutcome::Resync(ResyncReason::UnmatchedDeadline)
        ));
        assert_eq!(correlation.unmatched_metadata, 1);
        assert_eq!(budget.available_permits(), 1);
        assert!(correlator.deadline().is_none());
    }

    #[test]
    fn timestamp_gap_resync_returns_the_triggering_unit_permit() {
        let budget = Arc::new(Semaphore::new(1));
        let mut correlator = Correlator::new();
        push_metadata(&mut correlator, metadata(1, 1));
        assert_eq!(
            push_unit(&mut correlator, pending(unit(1, 1), &budget)).len(),
            1
        );
        push_metadata(&mut correlator, metadata(2, 1));

        let correlation = correlator.push_unit(pending(unit(u32::MAX, 1), &budget));
        assert!(matches!(
            correlation.outcome,
            CorrelationOutcome::Resync(ResyncReason::TimestampGap { .. })
        ));
        assert_eq!(correlation.unmatched_metadata, 1);
        assert_eq!(budget.available_permits(), 1);
        assert!(correlator.deadline().is_none());
    }

    #[test]
    fn delayed_metadata_correlates_pending_unit_and_releases_permit() {
        let budget = Arc::new(Semaphore::new(1));
        let mut correlator = Correlator::new();

        let waiting = correlator.push_unit(pending(unit(3_000, 1), &budget));
        let waiting_frames = match waiting.outcome {
            CorrelationOutcome::Frames(frames) => frames,
            CorrelationOutcome::Resync(reason) => {
                panic!("a unit awaiting metadata should not resynchronise: {reason}")
            }
        };
        assert!(waiting_frames.is_empty());
        assert_eq!(waiting.unmatched_metadata, 0);
        assert_eq!(budget.available_permits(), 0);

        let matched = correlator.push_metadata(metadata(42, 1));
        let matched_frames = match matched.outcome {
            CorrelationOutcome::Frames(frames) => frames,
            CorrelationOutcome::Resync(reason) => {
                panic!("matching metadata should not resynchronise: {reason}")
            }
        };
        assert_eq!(matched.unmatched_metadata, 0);
        assert_eq!(matched_frames.len(), 1);
        assert_eq!(matched_frames[0].metadata.sequence, 42);
        assert_eq!(matched_frames[0].metadata.generation.get(), 1);
        assert_eq!(budget.available_permits(), 1);
    }

    #[test]
    fn resync_reanchors_so_a_wide_timestamp_gap_does_not_resync_again() {
        let budget = Arc::new(Semaphore::new(1));
        let mut correlator = Correlator::new();
        push_metadata(&mut correlator, metadata(1, 3));
        assert_eq!(
            push_unit(&mut correlator, pending(unit(3_000, 3), &budget)).len(),
            1
        );
        push_metadata(&mut correlator, metadata(2, 3));

        let correlation = correlator.deadline_expired();
        assert!(matches!(
            correlation.outcome,
            CorrelationOutcome::Resync(ResyncReason::UnmatchedDeadline)
        ));

        push_metadata(&mut correlator, metadata(3, 3));
        let recovered = push_unit(&mut correlator, pending(unit(u32::MAX, 3), &budget));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].kind, FrameKind::Key);
        assert_eq!(recovered[0].continuity, Continuity::AfterGap);
    }

    #[test]
    fn post_resync_delta_is_dropped_until_keyframe_starts_a_gap() {
        let budget = Arc::new(Semaphore::new(1));
        let mut correlator = Correlator::new();
        push_metadata(&mut correlator, metadata(1, 1));
        assert_eq!(
            push_unit(&mut correlator, pending(unit(1, 1), &budget)).len(),
            1
        );
        push_metadata(&mut correlator, metadata(2, 1));
        let correlation = correlator.deadline_expired();
        assert!(matches!(
            correlation.outcome,
            CorrelationOutcome::Resync(ResyncReason::UnmatchedDeadline)
        ));

        push_metadata(&mut correlator, metadata(3, 1));
        let mut delta = unit(2, 1);
        delta.kind = FrameKind::Delta;
        assert!(push_unit(&mut correlator, pending(delta, &budget)).is_empty());

        push_metadata(&mut correlator, metadata(4, 1));
        let recovered = push_unit(&mut correlator, pending(unit(3, 1), &budget));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].continuity, Continuity::AfterGap);
    }
}
