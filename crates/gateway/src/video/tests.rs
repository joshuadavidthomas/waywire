use super::*;
use crate::daemon::ReadinessState;
use crate::daemon::test_command_sink;

fn packet(
    sequence: u16,
    timestamp: u32,
    generation: u32,
    end: AccessUnitEnd,
    payload: &[u8],
) -> Packet {
    Packet {
        end,
        sequence: RtpSequence(sequence),
        timestamp: RtpTimestamp(timestamp),
        generation: sprite_desktop_protocol::pipe::Generation::new(generation)
            .expect("test RTP generation should be valid"),
        payload: payload.to_vec(),
    }
}

fn fps(value: u32) -> Fps {
    Fps::new(value).expect("test frame rate should be valid")
}

fn video_hub() -> VideoHub {
    VideoHub::new(fps(60))
}

fn metadata(sequence: u64, generation: u32) -> FrameMetadata {
    FrameMetadata {
        generation: sprite_desktop_protocol::pipe::Generation::new(generation)
            .expect("test metadata generation should be valid"),
        width: sprite_desktop_protocol::pipe::FrameDimension::new(1280)
            .expect("test frame width should be valid"),
        height: sprite_desktop_protocol::pipe::FrameDimension::new(720)
            .expect("test frame height should be valid"),
        capture_nanos: sequence,
        sequence,
        input_sequence: None,
        fps: fps(30),
    }
}

fn unit(timestamp: u32, generation: u32) -> Unit {
    Unit {
        data: vec![1].into(),
        kind: FrameKind::Key,
        continuity: Continuity::Continuous,
        timestamp: RtpTimestamp(timestamp),
        generation: sprite_desktop_protocol::pipe::Generation::new(generation)
            .expect("test unit generation should be valid"),
    }
}

fn consume(assembler: &mut Assembler, value: Packet) -> Option<Unit> {
    match assembler
        .consume(value)
        .outcome
        .expect("test RTP packet should assemble without a protocol error")
    {
        AssemblyOutcome::Complete(unit) => Some(unit),
        AssemblyOutcome::Incomplete | AssemblyOutcome::Dropped => None,
    }
}

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

#[test]
fn rtp_decoder_parses_ssrc_as_a_generation() {
    let mut bytes = vec![0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1];
    let decoded = decode_packet(&bytes).expect("positive RTP SSRC should be a valid generation");
    assert_eq!(decoded.generation.get(), 7);

    bytes[11] = 0;
    assert!(decode_packet(&bytes).is_err());
}

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
fn packet_drop_summary_keeps_packet_counts_and_the_last_reason() {
    let mut summary = PacketDropSummary::new();
    summary.record(
        NonZeroU64::new(3).expect("test count should be nonzero"),
        PacketFailure::Drop(AssemblyDrop::StaleGeneration),
    );
    summary.record_rejection(PacketFailure::Reject(PacketReject::InvalidHeader));

    let report = summary
        .report()
        .expect("recorded packet failures should report");
    assert_eq!(report.dropped, 3);
    assert_eq!(report.rejected, 1);
    assert_eq!(
        report.last_failure,
        PacketFailure::Reject(PacketReject::InvalidHeader)
    );
    assert!(summary.deadline().is_none());
}

#[tokio::test(start_paused = true)]
async fn metadata_drop_summary_reports_a_later_drop_while_idle() {
    let mut summary = MetadataDropSummary::new();
    summary.record_unmatched(1);
    let deadline = summary
        .deadline()
        .expect("pending metadata summary should have a deadline");
    sleep_until(deadline).await;
    let first = summary
        .report_if_due()
        .expect("initial unmatched metadata should become due");
    assert_eq!(first.unmatched, 1);

    summary.record_unmatched(2);
    let deadline = summary
        .deadline()
        .expect("pending metadata summary should have a deadline");
    sleep_until(deadline).await;
    let second = summary
        .report_if_due()
        .expect("metadata summary should become due without another record");
    assert_eq!(second.unmatched, 2);
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

    let first_fragment = assembler.consume(packet(3, 2, 1, AccessUnitEnd::More, &[0x7c, 0x81, 2]));
    assert!(matches!(
        first_fragment.outcome,
        Ok(AssemblyOutcome::Incomplete)
    ));
    assert_eq!(first_fragment.drops, None);

    let final_fragment = assembler.consume(packet(4, 2, 1, AccessUnitEnd::Final, &[0x7c, 0x41, 3]));
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
            sprite_desktop_protocol::pipe::Fps::new(60).expect("test frame rate should be valid"),
        )
        .is_none()
    );
}

fn sample(sequence: u64, bytes: usize, kind: FrameKind) -> VideoSample {
    VideoSample {
        data: vec![1; bytes].into(),
        kind,
        continuity: Continuity::Continuous,
        metadata: metadata(sequence, 1),
    }
}

#[tokio::test]
async fn viewer_queue_delivers_every_frame_up_to_its_bound() {
    for (max_fps, frame_bound) in [(10, 6_u64), (30, 16), (60, 30), (120, 60)] {
        let hub = VideoHub::new(fps(max_fps));
        let _ = hub.broadcast(&sample(0, 1, FrameKind::Key));
        let mut frames = hub.subscribe();
        assert_eq!(frames.next().await.metadata.sequence, 0);

        for sequence in 1..=frame_bound {
            let _ = hub.broadcast(&sample(sequence, 1, FrameKind::Delta));
        }
        for expected in 1..=frame_bound {
            let frame = tokio::time::timeout(Duration::from_millis(100), frames.next())
                .await
                .expect("the literal frame bound should fit without overflow");
            assert_eq!(frame.metadata.sequence, expected);
        }
    }
}

#[tokio::test]
async fn viewer_queue_clears_on_the_first_frame_past_its_bound() {
    for (max_fps, frame_bound) in [(10, 6_u64), (30, 16), (60, 30), (120, 60)] {
        let hub = VideoHub::new(fps(max_fps));
        let _ = hub.broadcast(&sample(0, 1, FrameKind::Key));
        let mut frames = hub.subscribe();
        assert_eq!(frames.next().await.metadata.sequence, 0);

        for sequence in 1..=frame_bound {
            let _ = hub.broadcast(&sample(sequence, 1, FrameKind::Delta));
        }
        let _ = hub.broadcast(&sample(frame_bound + 1, 1, FrameKind::Delta));
        let recovery_sequence = frame_bound + 2;
        let _ = hub.broadcast(&sample(recovery_sequence, 1, FrameKind::Key));

        let recovered = frames.next().await;
        assert_eq!(recovered.metadata.sequence, recovery_sequence);
        assert_eq!(recovered.kind, FrameKind::Key);
        assert_eq!(recovered.continuity, Continuity::AfterGap);
    }
}

#[test]
fn viewer_queue_recovers_after_byte_overflow_without_large_allocations() {
    let mut queue = ViewerQueue::new(GopState::Recovering, None, ViewerBounds::new(8, 4));
    assert_eq!(queue.push(sample(0, 1, FrameKind::Key)), Pushed::Queued);
    assert_eq!(
        queue
            .pop()
            .expect("queued keyframe should be present")
            .metadata
            .sequence,
        0
    );

    assert_eq!(queue.push(sample(1, 4, FrameKind::Delta)), Pushed::Queued);
    assert_eq!(queue.push(sample(2, 1, FrameKind::Delta)), Pushed::Dropped);
    assert_eq!(queue.push(sample(3, 1, FrameKind::Delta)), Pushed::Dropped);
    assert_eq!(queue.push(sample(4, 1, FrameKind::Key)), Pushed::Queued);

    let recovered = queue.pop().expect("recovery keyframe should be queued");
    assert_eq!(recovered.metadata.sequence, 4);
    assert_eq!(recovered.kind, FrameKind::Key);
    assert_eq!(recovered.continuity, Continuity::AfterGap);
}

#[tokio::test]
async fn overflow_keeps_arriving_keyframe_and_discards_delta() {
    let hub = video_hub();
    let _ = hub.broadcast(&sample(0, 1, FrameKind::Key));
    let mut slow = hub.subscribe();
    let mut independent = hub.subscribe();
    let _ = slow.next().await;
    let _ = independent.next().await;
    for sequence in 1..=ViewerBounds::for_max_fps(fps(60)).frames {
        let _ = hub.broadcast(&sample(
            u64::try_from(sequence).expect("test sequence should fit"),
            1,
            FrameKind::Delta,
        ));
        let _ = independent.next().await;
    }

    let _ = hub.broadcast(&sample(20, 1, FrameKind::Delta));
    assert_eq!(independent.next().await.metadata.sequence, 20);
    let _ = hub.broadcast(&sample(21, 1, FrameKind::Key));
    let recovered = slow.next().await;
    assert_eq!(recovered.kind, FrameKind::Key);
    assert_eq!(recovered.continuity, Continuity::AfterGap);
}

#[tokio::test]
async fn cancelled_waiter_rechecks_durable_queue_state() {
    let hub = video_hub();
    let mut subscription = hub.subscribe();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), subscription.next())
            .await
            .is_err()
    );

    let _ = hub.broadcast(&sample(1, 1, FrameKind::Key));
    let frame = tokio::time::timeout(Duration::from_secs(1), subscription.next())
        .await
        .expect("replacement wait should be notified");
    assert_eq!(frame.metadata.sequence, 1);
}

#[tokio::test]
async fn generation_reset_discards_queued_deltas_until_a_new_keyframe() {
    let hub = video_hub();
    let _ = hub.broadcast(&sample(0, 1, FrameKind::Key));
    let mut subscription = hub.subscribe();
    let _ = subscription.next().await;

    let _ = hub.broadcast(&sample(1, 1, FrameKind::Delta));
    let mut replacement_delta = sample(2, 1, FrameKind::Delta);
    replacement_delta.metadata.generation =
        Generation::new(2).expect("replacement generation should be valid");
    let _ = hub.broadcast(&replacement_delta);
    let mut replacement_key = sample(3, 1, FrameKind::Key);
    replacement_key.metadata.generation =
        Generation::new(2).expect("replacement generation should be valid");
    let _ = hub.broadcast(&replacement_key);

    let frame = subscription.next().await;
    assert_eq!(frame.metadata.sequence, 3);
    assert_eq!(frame.metadata.generation.get(), 2);
    assert_eq!(frame.continuity, Continuity::AfterGap);
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

#[tokio::test]
async fn broadcast_survives_an_empty_subscriber_map() {
    let hub = video_hub();
    let subscription = hub.subscribe();
    drop(subscription);

    let _ = hub.broadcast(&sample(1, 1, FrameKind::Key));
    let mut replacement = hub.subscribe();
    assert_eq!(replacement.next().await.metadata.sequence, 1);
}

#[tokio::test]
async fn oversized_cached_gop_is_not_bootstrapped() {
    let bounds = ViewerBounds::for_max_fps(fps(60));
    let hub = video_hub();
    let _ = hub.broadcast(&sample(0, 1, FrameKind::Key));
    for sequence in 1..=bounds.frames {
        let _ = hub.broadcast(&sample(
            u64::try_from(sequence).expect("test sequence should fit"),
            1,
            FrameKind::Delta,
        ));
    }
    let mut subscription = hub.subscribe();

    assert!(
        tokio::time::timeout(Duration::from_millis(20), subscription.next())
            .await
            .is_err()
    );
    let delta_sequence = u64::try_from(bounds.frames + 1).expect("test sequence should fit");
    let key_sequence = u64::try_from(bounds.frames + 2).expect("test sequence should fit");
    let _ = hub.broadcast(&sample(delta_sequence, 1, FrameKind::Delta));
    let _ = hub.broadcast(&sample(key_sequence, 1, FrameKind::Key));
    assert_eq!(subscription.next().await.metadata.sequence, key_sequence);
}

#[tokio::test]
async fn fitting_cached_gop_is_delivered_before_live_frames() {
    let hub = video_hub();
    let _ = hub.broadcast(&sample(0, 1, FrameKind::Key));
    let _ = hub.broadcast(&sample(1, 1, FrameKind::Delta));
    let mut subscription = hub.subscribe();
    let _ = hub.broadcast(&sample(2, 1, FrameKind::Delta));

    for expected in 0..=2 {
        assert_eq!(subscription.next().await.metadata.sequence, expected);
    }
}

#[tokio::test]
async fn worker_resync_waits_for_a_fresh_keyframe_before_resuming_delivery() {
    let (commands, mut command_events) = test_command_sink();
    let readiness = Readiness::new();
    let hub = video_hub();
    let (pipeline, worker) = VideoPipeline::new(hub.clone(), commands, readiness.clone());
    let worker_task = tokio::spawn(worker.run());

    pipeline
        .metadata(metadata(1, 1))
        .await
        .expect("first metadata should queue");
    pipeline
        .unit(unit(1, 1))
        .await
        .expect("first keyframe should queue");
    assert!(matches!(
        command_events.recv().await,
        Some(Command::KeyframeReadiness(KeyframeReadiness {
            state: KeyframeState::Cached,
            ..
        }))
    ));
    assert_eq!(readiness.state(), ReadinessState::Ready);
    let mut viewer = hub.subscribe();
    assert_eq!(viewer.next().await.metadata.sequence, 1);

    for sequence in 2..=(PENDING_RECORD_CAPACITY as u64 + 2) {
        pipeline
            .metadata(metadata(sequence, 1))
            .await
            .expect("backlog metadata should queue");
    }
    assert!(matches!(
        command_events.recv().await,
        Some(Command::KeyframeReadiness(KeyframeReadiness {
            state: KeyframeState::Missing,
            ..
        }))
    ));
    assert_eq!(readiness.state(), ReadinessState::WaitingForKeyframe);

    pipeline
        .metadata(metadata(500, 1))
        .await
        .expect("recovery delta metadata should queue");
    let mut delta = unit(2, 1);
    delta.kind = FrameKind::Delta;
    pipeline
        .unit(delta)
        .await
        .expect("recovery delta should queue");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), command_events.recv())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), viewer.next())
            .await
            .is_err()
    );

    pipeline
        .metadata(metadata(501, 1))
        .await
        .expect("recovery keyframe metadata should queue");
    pipeline
        .unit(unit(3, 1))
        .await
        .expect("recovery keyframe should queue");
    assert!(matches!(
        command_events.recv().await,
        Some(Command::KeyframeReadiness(KeyframeReadiness {
            state: KeyframeState::Cached,
            ..
        }))
    ));
    assert_eq!(readiness.state(), ReadinessState::Ready);
    let frame = viewer.next().await;
    assert_eq!(frame.metadata.sequence, 501);
    assert_eq!(frame.kind, FrameKind::Key);
    assert_eq!(frame.continuity, Continuity::AfterGap);

    drop(pipeline);
    assert!(worker_task.await.expect("worker task should run").is_err());
}

#[tokio::test]
async fn viewer_starts_and_recovers_only_at_keyframe() {
    let hub = video_hub();
    let mut subscription = hub.subscribe();
    for sequence in 0..10 {
        let _ = hub.broadcast(&VideoSample {
            data: vec![1].into(),
            kind: FrameKind::Delta,
            continuity: Continuity::Continuous,
            metadata: metadata(sequence, 1),
        });
    }
    let _ = hub.broadcast(&VideoSample {
        data: vec![1].into(),
        kind: FrameKind::Key,
        continuity: Continuity::Continuous,
        metadata: metadata(11, 1),
    });
    let value = subscription.next().await;
    assert_eq!(value.kind, FrameKind::Key);
    assert_eq!(value.continuity, Continuity::AfterGap);
}
