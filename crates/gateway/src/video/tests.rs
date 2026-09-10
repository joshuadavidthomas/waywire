use super::*;

fn packet(sequence: u16, timestamp: u32, generation: u32, marker: bool, payload: &[u8]) -> Packet {
    Packet {
        marker,
        sequence,
        timestamp,
        generation: sprite_desktop_protocol::pipe::Generation::new(generation)
            .expect("test RTP generation should be valid"),
        payload: payload.to_vec(),
    }
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
        fps: sprite_desktop_protocol::pipe::Fps::new(30).expect("test frame rate should be valid"),
    }
}

fn unit(timestamp: u32, generation: u32) -> Unit {
    Unit {
        data: vec![1].into(),
        kind: FrameKind::Key,
        continuity: Continuity::Continuous,
        timestamp,
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
    correlator
        .push_metadata(value)
        .expect("valid test metadata should enter the correlator");
}

fn push_unit(correlator: &mut Correlator, value: PendingUnit) -> Vec<VideoSample> {
    correlator
        .push_unit(value)
        .expect("valid test access unit should enter the correlator")
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
    assert!(consume(&mut assembler, packet(1, 90, 7, false, &[0x7c, 0x85, 1])).is_none());
    let value = consume(&mut assembler, packet(2, 90, 7, true, &[0x7c, 0x45, 2]))
        .expect("marker packet should complete the fragmented test access unit");
    assert_eq!(value.kind, FrameKind::Key);
    assert_eq!(&*value.data, [0, 0, 0, 1, 0x65, 1, 2]);
}

#[test]
fn generation_restart_stays_sticky_across_fragments() {
    let mut assembler = Assembler::default();
    let _ = consume(&mut assembler, packet(1, 1, 1, true, &[5, 1]));
    assert!(consume(&mut assembler, packet(2, 2, 2, false, &[0x7c, 0x85, 2])).is_none());
    let value = consume(&mut assembler, packet(3, 2, 2, true, &[0x7c, 0x45, 3]))
        .expect("final fragment should complete the replacement generation");
    assert_eq!(value.continuity, Continuity::AfterGap);
}

#[test]
fn rejects_invalid_fu_boundaries() {
    let mut assembler = Assembler::default();
    assert!(
        assembler
            .consume(packet(1, 1, 1, true, &[0x7c, 0xc5, 1]))
            .outcome
            .is_err()
    );
    assert!(consume(&mut assembler, packet(2, 2, 1, false, &[0x7c, 0x85, 1])).is_none());
    assert!(
        assembler
            .consume(packet(3, 2, 1, true, &[0x7c, 0x41, 2]))
            .outcome
            .is_err()
    );
}

#[test]
fn assembler_distinguishes_incomplete_recovery_and_stale_packets() {
    let mut assembler = Assembler::default();
    let incomplete = assembler.consume(packet(1, 1, 2, false, &[1, 1]));
    assert!(matches!(
        incomplete.outcome,
        Ok(AssemblyOutcome::Incomplete)
    ));

    let complete = assembler.consume(packet(2, 1, 2, true, &[5, 2]));
    assert!(matches!(complete.outcome, Ok(AssemblyOutcome::Complete(_))));
    let stale = assembler.consume(packet(3, 2, 1, true, &[5, 3]));
    assert!(matches!(stale.outcome, Ok(AssemblyOutcome::Dropped)));
    assert_eq!(stale.drops.packets, 1);

    let recovering = assembler.consume(packet(4, 2, 2, true, &[1, 4]));
    assert!(matches!(recovering.outcome, Ok(AssemblyOutcome::Dropped)));
    assert_eq!(recovering.drops.packets, 1);
}

#[test]
fn packet_drop_summary_keeps_packet_counts_and_the_last_reason() {
    let mut summary = PacketDropSummary::default();
    summary.record_drops(3, AssemblyDrop::StaleGeneration);
    summary.record_rejection("invalid test packet");

    assert_eq!(summary.dropped_packets, 3);
    assert_eq!(summary.rejected_packets, 1);
    assert_eq!(summary.last_reason.as_deref(), Some("invalid test packet"));
    assert!(summary.deadline().is_some());
}

#[test]
fn fragmented_recovery_drop_counts_every_packet_in_the_access_unit() {
    let mut assembler = Assembler::default();
    assert!(consume(&mut assembler, packet(1, 1, 1, true, &[5, 1])).is_some());

    let first_fragment = assembler.consume(packet(3, 2, 1, false, &[0x7c, 0x81, 2]));
    assert!(matches!(
        first_fragment.outcome,
        Ok(AssemblyOutcome::Incomplete)
    ));
    assert_eq!(first_fragment.drops.packets, 0);

    let final_fragment = assembler.consume(packet(4, 2, 1, true, &[0x7c, 0x41, 3]));
    assert!(matches!(
        final_fragment.outcome,
        Ok(AssemblyOutcome::Dropped)
    ));
    assert_eq!(final_fragment.drops.packets, 2);
    assert_eq!(
        final_fragment.drops.last_reason,
        Some(AssemblyDrop::RecoveringWithoutKeyframe)
    );
}

#[test]
fn discontinuities_count_packets_cleared_from_the_access_unit() {
    let mut assembler = Assembler::default();
    let buffered = assembler.consume(packet(1, 1, 1, false, &[1, 1]));
    assert!(matches!(buffered.outcome, Ok(AssemblyOutcome::Incomplete)));

    let sequence_change = assembler.consume(packet(3, 1, 1, true, &[5, 2]));
    assert!(matches!(
        sequence_change.outcome,
        Ok(AssemblyOutcome::Complete(_))
    ));
    assert_eq!(sequence_change.drops.packets, 1);
    assert_eq!(
        sequence_change.drops.last_reason,
        Some(AssemblyDrop::SequenceDiscontinuity)
    );

    let buffered = assembler.consume(packet(4, 2, 1, false, &[1, 3]));
    assert!(matches!(buffered.outcome, Ok(AssemblyOutcome::Incomplete)));

    let timestamp_change = assembler.consume(packet(5, 3, 1, true, &[5, 4]));
    assert!(matches!(
        timestamp_change.outcome,
        Ok(AssemblyOutcome::Complete(_))
    ));
    assert_eq!(timestamp_change.drops.packets, 1);
    assert_eq!(
        timestamp_change.drops.last_reason,
        Some(AssemblyDrop::TimestampDiscontinuity)
    );
}

#[test]
fn loss_and_sequence_wrap_recover_at_keyframe() {
    let mut assembler = Assembler::default();
    assert!(consume(&mut assembler, packet(u16::MAX, 1, 1, true, &[1, 1]),).is_some());
    assert!(consume(&mut assembler, packet(0, 2, 1, true, &[1, 2])).is_some());
    assert!(consume(&mut assembler, packet(2, 3, 1, true, &[1, 3])).is_none());
    assert_eq!(
        consume(&mut assembler, packet(3, 4, 1, true, &[5, 4]))
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

    assert!(push_unit(&mut correlator, pending(unit(2, 1), &budget)).is_empty());
    assert!(correlator.units.is_empty());
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

    assert!(push_unit(&mut correlator, pending(unit(1, 2), &budget)).is_empty());
    assert!(correlator.units.is_empty());
}

#[test]
fn stale_generation_packet_cannot_poison_current_assembler_state() {
    let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
    let mut assembler = Assembler::default();
    let mut correlator = Correlator::new();

    push_metadata(&mut correlator, metadata(1, 1));
    let first = consume(&mut assembler, packet(1, 3_000, 1, true, &[5, 1]))
        .expect("first generation packet should complete an access unit");
    let _ = push_unit(&mut correlator, pending(first, &budget));

    push_metadata(&mut correlator, metadata(2, 2));
    let replacement = consume(&mut assembler, packet(1, 3_000, 2, true, &[5, 2]))
        .expect("replacement generation packet should complete an access unit");
    let _ = push_unit(&mut correlator, pending(replacement, &budget));
    push_metadata(&mut correlator, metadata(3, 2));

    assert!(consume(&mut assembler, packet(2, 6_000, 1, true, &[5, 3])).is_none());
    let current = consume(&mut assembler, packet(2, 6_000, 2, true, &[5, 4]))
        .expect("current generation packet should complete an access unit");
    assert_eq!(current.continuity, Continuity::Continuous);
    let samples = push_unit(&mut correlator, pending(current, &budget));
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].metadata.sequence, 3);
    assert!(correlator.units.is_empty());
}

#[test]
fn generation_change_does_not_apply_timestamp_gap_across_ssrcs() {
    let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
    let mut correlator = Correlator::new();
    push_metadata(&mut correlator, metadata(1, 1));
    let _ = push_unit(&mut correlator, pending(unit(3_000, 1), &budget));
    push_metadata(&mut correlator, metadata(2, 1));
    push_metadata(&mut correlator, metadata(3, 2));

    let samples = push_unit(&mut correlator, pending(unit(u32::MAX, 2), &budget));
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].metadata.sequence, 3);
    assert_eq!(samples[0].continuity, Continuity::AfterGap);
    assert!(correlator.metadata.is_empty());
}

#[test]
fn huge_timestamp_gap_fails_instead_of_guessing() {
    assert!(
        metadata_gap(
            u32::MAX,
            sprite_desktop_protocol::pipe::Fps::new(60).expect("test frame rate should be valid"),
        )
        .is_err()
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

#[test]
fn viewer_queue_keeps_exact_frame_and_byte_bounds() {
    let hub = VideoHub::new();
    assert_eq!(
        hub.broadcast(sample(0, 1, FrameKind::Key)),
        GopState::KeyframeCached
    );
    let (_, _, frames) = hub.subscribe();
    for sequence in 1..=8 {
        let kind = if sequence == 8 {
            FrameKind::Key
        } else {
            FrameKind::Delta
        };
        let _ = hub.broadcast(sample(sequence, 1, kind));
    }
    assert_eq!(
        lock(&frames.queue, "test video viewer queue").frames.len(),
        MAX_VIEWER_FRAMES
    );

    let hub = VideoHub::new();
    let _ = hub.broadcast(sample(0, 1, FrameKind::Key));
    let (_, _, bytes) = hub.subscribe();
    let _ = hub.broadcast(sample(1, MAX_VIEWER_BYTES, FrameKind::Key));
    assert_eq!(
        lock(&bytes.queue, "test video viewer queue").bytes,
        MAX_VIEWER_BYTES
    );
}

#[test]
fn overflow_keeps_arriving_keyframe_and_discards_delta() {
    let hub = VideoHub::new();
    let _ = hub.broadcast(sample(0, 1, FrameKind::Key));
    let (_, _, slow) = hub.subscribe();
    let (_, _, independent) = hub.subscribe();
    for sequence in 1..=8 {
        let _ = hub.broadcast(sample(sequence, 1, FrameKind::Delta));
    }
    {
        let mut independent_queue = lock(&independent.queue, "test video viewer queue");
        independent_queue.frames.clear();
        independent_queue.bytes = 0;
    }

    let _ = hub.broadcast(sample(20, 1, FrameKind::Delta));
    assert!(
        lock(&slow.queue, "test video viewer queue")
            .frames
            .is_empty()
    );
    assert_eq!(
        lock(&independent.queue, "test video viewer queue")
            .frames
            .len(),
        1
    );
    let _ = hub.broadcast(sample(21, 1, FrameKind::Key));
    let slow_queue = lock(&slow.queue, "test video viewer queue");
    assert_eq!(slow_queue.frames.len(), 1);
    assert_eq!(slow_queue.frames[0].kind, FrameKind::Key);
    assert_eq!(slow_queue.frames[0].continuity, Continuity::AfterGap);
}

#[tokio::test]
async fn viewer_starts_and_recovers_only_at_keyframe() {
    let hub = VideoHub::new();
    let (_, bootstrap, subscription) = hub.subscribe();
    assert!(bootstrap.is_empty());
    for sequence in 0..10 {
        let _ = hub.broadcast(VideoSample {
            data: vec![1].into(),
            kind: FrameKind::Delta,
            continuity: Continuity::Continuous,
            metadata: metadata(sequence, 1),
        });
    }
    let _ = hub.broadcast(VideoSample {
        data: vec![1].into(),
        kind: FrameKind::Key,
        continuity: Continuity::Continuous,
        metadata: metadata(11, 1),
    });
    let value = subscription.next().await;
    assert_eq!(value.kind, FrameKind::Key);
    assert_eq!(value.continuity, Continuity::AfterGap);
}
