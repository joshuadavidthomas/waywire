use super::*;
fn packet(sequence: u16, timestamp: u32, ssrc: u32, marker: bool, payload: &[u8]) -> Packet {
    Packet {
        marker,
        sequence,
        timestamp,
        ssrc,
        payload: payload.to_vec(),
    }
}
fn metadata(sequence: u64, generation: u32) -> FrameMetadata {
    FrameMetadata {
        generation,
        width: 1280,
        height: 720,
        capture_nanos: sequence,
        sequence,
        input_sequence: 0,
        fps: 30,
    }
}
fn unit(timestamp: u32, generation: u32) -> Unit {
    Unit {
        data: vec![1].into(),
        key: true,
        discontinuity: false,
        timestamp,
        ssrc: generation,
    }
}
#[test]
fn emits_final_access_unit_at_marker() {
    let mut a = Assembler::default();
    assert!(
        a.consume(packet(1, 90, 7, false, &[0x7c, 0x85, 1]))
            .unwrap()
            .is_none()
    );
    let value = a
        .consume(packet(2, 90, 7, true, &[0x7c, 0x45, 2]))
        .unwrap()
        .unwrap();
    assert!(value.key);
    assert_eq!(&*value.data, [0, 0, 0, 1, 0x65, 1, 2]);
}
#[test]
fn generation_restart_stays_sticky_across_fragments() {
    let mut a = Assembler::default();
    a.consume(packet(1, 1, 1, true, &[5, 1])).unwrap();
    assert!(
        a.consume(packet(2, 2, 2, false, &[0x7c, 0x85, 2]))
            .unwrap()
            .is_none()
    );
    let value = a
        .consume(packet(3, 2, 2, true, &[0x7c, 0x45, 3]))
        .unwrap()
        .unwrap();
    assert!(value.discontinuity);
}
#[test]
fn rejects_invalid_fu_boundaries() {
    let mut a = Assembler::default();
    assert!(a.consume(packet(1, 1, 1, true, &[0x7c, 0xc5, 1])).is_err());
    assert!(
        a.consume(packet(2, 2, 1, false, &[0x7c, 0x85, 1]))
            .unwrap()
            .is_none()
    );
    assert!(a.consume(packet(3, 2, 1, true, &[0x7c, 0x41, 2])).is_err());
}
#[test]
fn loss_and_sequence_wrap_recover_at_keyframe() {
    let mut a = Assembler::default();
    assert!(
        a.consume(packet(u16::MAX, 1, 1, true, &[1, 1]))
            .unwrap()
            .is_some()
    );
    assert!(a.consume(packet(0, 2, 1, true, &[1, 2])).unwrap().is_some());
    assert!(a.consume(packet(2, 3, 1, true, &[1, 3])).unwrap().is_none());
    assert!(
        a.consume(packet(3, 4, 1, true, &[5, 4]))
            .unwrap()
            .unwrap()
            .discontinuity
    );
}
fn pending(unit: Unit, budget: &Arc<Semaphore>) -> PendingUnit {
    PendingUnit {
        unit,
        _bytes: budget.clone().try_acquire_many_owned(1).unwrap(),
    }
}

#[test]
fn first_unit_uses_only_metadata_with_its_generation() {
    let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
    let mut correlator = Correlator::new();
    correlator.push_metadata(metadata(1, 1)).unwrap();
    correlator.push_metadata(metadata(2, 2)).unwrap();

    let samples = correlator.push_unit(pending(unit(1, 2), &budget)).unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].metadata.sequence, 2);
    assert_eq!(samples[0].metadata.generation, 2);

    assert!(
        correlator
            .push_unit(pending(unit(2, 1), &budget))
            .unwrap()
            .is_empty()
    );
    assert!(correlator.units.is_empty());
}

#[test]
fn reordered_replacements_never_exchange_metadata() {
    let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
    let mut correlator = Correlator::new();
    correlator.push_metadata(metadata(1, 1)).unwrap();
    correlator
        .push_unit(pending(unit(3_000, 1), &budget))
        .unwrap();
    correlator.push_metadata(metadata(2, 2)).unwrap();
    correlator.push_metadata(metadata(3, 3)).unwrap();

    let samples = correlator.push_unit(pending(unit(1, 3), &budget)).unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].metadata.sequence, 3);
    assert_eq!(samples[0].metadata.generation, 3);

    assert!(
        correlator
            .push_unit(pending(unit(1, 2), &budget))
            .unwrap()
            .is_empty()
    );
    assert!(correlator.units.is_empty());
}

#[test]
fn stale_generation_packet_cannot_poison_current_assembler_state() {
    let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
    let mut assembler = Assembler::default();
    let mut correlator = Correlator::new();

    correlator.push_metadata(metadata(1, 1)).unwrap();
    let first = assembler
        .consume(packet(1, 3_000, 1, true, &[5, 1]))
        .unwrap()
        .unwrap();
    correlator.push_unit(pending(first, &budget)).unwrap();

    correlator.push_metadata(metadata(2, 2)).unwrap();
    let replacement = assembler
        .consume(packet(1, 3_000, 2, true, &[5, 2]))
        .unwrap()
        .unwrap();
    correlator.push_unit(pending(replacement, &budget)).unwrap();
    correlator.push_metadata(metadata(3, 2)).unwrap();

    assert!(
        assembler
            .consume(packet(2, 6_000, 1, true, &[5, 3]))
            .unwrap()
            .is_none()
    );
    let current = assembler
        .consume(packet(2, 6_000, 2, true, &[5, 4]))
        .unwrap()
        .unwrap();
    assert!(!current.discontinuity);
    let samples = correlator.push_unit(pending(current, &budget)).unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].metadata.sequence, 3);
    assert!(correlator.units.is_empty());
}

#[test]
fn generation_change_does_not_apply_timestamp_gap_across_ssrcs() {
    let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
    let mut correlator = Correlator::new();
    correlator.push_metadata(metadata(1, 1)).unwrap();
    correlator
        .push_unit(pending(unit(3_000, 1), &budget))
        .unwrap();
    correlator.push_metadata(metadata(2, 1)).unwrap();
    correlator.push_metadata(metadata(3, 2)).unwrap();

    let samples = correlator
        .push_unit(pending(unit(u32::MAX, 2), &budget))
        .unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].metadata.sequence, 3);
    assert!(samples[0].discontinuity);
    assert!(correlator.metadata.is_empty());
}

#[test]
fn huge_timestamp_gap_fails_instead_of_guessing() {
    assert!(metadata_gap(u32::MAX, 60).is_err());
}
fn sample(sequence: u64, bytes: usize, key: bool) -> VideoSample {
    VideoSample {
        data: vec![1; bytes].into(),
        key,
        discontinuity: false,
        metadata: metadata(sequence, 1),
    }
}

#[test]
fn viewer_queue_keeps_exact_frame_and_byte_bounds() {
    let hub = VideoHub::new();
    hub.broadcast(sample(0, 1, true));
    let (_, _, frames) = hub.subscribe();
    for sequence in 1..=MAX_VIEWER_FRAMES as u64 {
        hub.broadcast(sample(sequence, 1, sequence == MAX_VIEWER_FRAMES as u64));
    }
    assert_eq!(frames.queue.lock().unwrap().frames.len(), MAX_VIEWER_FRAMES);

    let hub = VideoHub::new();
    hub.broadcast(sample(0, 1, true));
    let (_, _, bytes) = hub.subscribe();
    hub.broadcast(sample(1, MAX_VIEWER_BYTES, true));
    assert_eq!(bytes.queue.lock().unwrap().bytes, MAX_VIEWER_BYTES);
}

#[test]
fn overflow_keeps_arriving_keyframe_and_discards_delta() {
    let hub = VideoHub::new();
    hub.broadcast(sample(0, 1, true));
    let (_, _, slow) = hub.subscribe();
    let (_, _, independent) = hub.subscribe();
    for sequence in 1..=MAX_VIEWER_FRAMES as u64 {
        hub.broadcast(sample(sequence, 1, false));
    }
    independent.queue.lock().unwrap().frames.clear();
    independent.queue.lock().unwrap().bytes = 0;

    hub.broadcast(sample(20, 1, false));
    assert!(slow.queue.lock().unwrap().frames.is_empty());
    assert_eq!(independent.queue.lock().unwrap().frames.len(), 1);
    hub.broadcast(sample(21, 1, true));
    let slow_queue = slow.queue.lock().unwrap();
    assert_eq!(slow_queue.frames.len(), 1);
    assert!(slow_queue.frames[0].key && slow_queue.frames[0].discontinuity);
}

#[tokio::test]
async fn viewer_starts_and_recovers_only_at_keyframe() {
    let hub = VideoHub::new();
    let (_, bootstrap, subscription) = hub.subscribe();
    assert!(bootstrap.is_empty());
    for n in 0..10 {
        hub.broadcast(VideoSample {
            data: vec![1].into(),
            key: false,
            discontinuity: false,
            metadata: metadata(n, 1),
        });
    }
    hub.broadcast(VideoSample {
        data: vec![1].into(),
        key: true,
        discontinuity: false,
        metadata: metadata(11, 1),
    });
    let value = subscription.next().await;
    assert!(value.key && value.discontinuity);
}
