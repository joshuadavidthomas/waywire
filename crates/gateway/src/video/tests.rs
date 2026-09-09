use std::sync::MutexGuard;

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

fn consume(assembler: &mut Assembler, value: Packet) -> Option<Unit> {
    assembler
        .consume(value)
        .expect("test RTP packet should assemble without a protocol error")
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

fn queue(subscription: &VideoSubscription) -> MutexGuard<'_, ViewerQueue> {
    subscription
        .queue
        .lock()
        .expect("test viewer queue mutex should remain unpoisoned")
}

#[test]
fn emits_final_access_unit_at_marker() {
    let mut assembler = Assembler::default();
    assert!(consume(&mut assembler, packet(1, 90, 7, false, &[0x7c, 0x85, 1]),).is_none());
    let value = consume(&mut assembler, packet(2, 90, 7, true, &[0x7c, 0x45, 2]))
        .expect("marker packet should complete the fragmented test access unit");
    assert!(value.key);
    assert_eq!(&*value.data, [0, 0, 0, 1, 0x65, 1, 2]);
}

#[test]
fn generation_restart_stays_sticky_across_fragments() {
    let mut assembler = Assembler::default();
    consume(&mut assembler, packet(1, 1, 1, true, &[5, 1]));
    assert!(consume(&mut assembler, packet(2, 2, 2, false, &[0x7c, 0x85, 2]),).is_none());
    let value = consume(&mut assembler, packet(3, 2, 2, true, &[0x7c, 0x45, 3]))
        .expect("final fragment should complete the replacement generation");
    assert!(value.discontinuity);
}

#[test]
fn rejects_invalid_fu_boundaries() {
    let mut assembler = Assembler::default();
    assert!(
        assembler
            .consume(packet(1, 1, 1, true, &[0x7c, 0xc5, 1]))
            .is_err()
    );
    assert!(consume(&mut assembler, packet(2, 2, 1, false, &[0x7c, 0x85, 1]),).is_none());
    assert!(
        assembler
            .consume(packet(3, 2, 1, true, &[0x7c, 0x41, 2]))
            .is_err()
    );
}

#[test]
fn loss_and_sequence_wrap_recover_at_keyframe() {
    let mut assembler = Assembler::default();
    assert!(consume(&mut assembler, packet(u16::MAX, 1, 1, true, &[1, 1]),).is_some());
    assert!(consume(&mut assembler, packet(0, 2, 1, true, &[1, 2])).is_some());
    assert!(consume(&mut assembler, packet(2, 3, 1, true, &[1, 3])).is_none());
    assert!(
        consume(&mut assembler, packet(3, 4, 1, true, &[5, 4]))
            .expect("keyframe should complete sequence-loss recovery")
            .discontinuity
    );
}

fn pending(unit: Unit, budget: &Arc<Semaphore>) -> PendingUnit {
    PendingUnit {
        unit,
        _bytes: Arc::clone(budget)
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
    assert_eq!(samples[0].metadata.generation, 2);

    assert!(push_unit(&mut correlator, pending(unit(2, 1), &budget)).is_empty());
    assert!(correlator.units.is_empty());
}

#[test]
fn reordered_replacements_never_exchange_metadata() {
    let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
    let mut correlator = Correlator::new();
    push_metadata(&mut correlator, metadata(1, 1));
    push_unit(&mut correlator, pending(unit(3_000, 1), &budget));
    push_metadata(&mut correlator, metadata(2, 2));
    push_metadata(&mut correlator, metadata(3, 3));

    let samples = push_unit(&mut correlator, pending(unit(1, 3), &budget));
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].metadata.sequence, 3);
    assert_eq!(samples[0].metadata.generation, 3);

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
    push_unit(&mut correlator, pending(first, &budget));

    push_metadata(&mut correlator, metadata(2, 2));
    let replacement = consume(&mut assembler, packet(1, 3_000, 2, true, &[5, 2]))
        .expect("replacement generation packet should complete an access unit");
    push_unit(&mut correlator, pending(replacement, &budget));
    push_metadata(&mut correlator, metadata(3, 2));

    assert!(consume(&mut assembler, packet(2, 6_000, 1, true, &[5, 3]),).is_none());
    let current = consume(&mut assembler, packet(2, 6_000, 2, true, &[5, 4]))
        .expect("current generation packet should complete an access unit");
    assert!(!current.discontinuity);
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
    push_unit(&mut correlator, pending(unit(3_000, 1), &budget));
    push_metadata(&mut correlator, metadata(2, 1));
    push_metadata(&mut correlator, metadata(3, 2));

    let samples = push_unit(&mut correlator, pending(unit(u32::MAX, 2), &budget));
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
    for sequence in
        1..=u64::try_from(MAX_VIEWER_FRAMES).expect("viewer frame limit should fit in u64")
    {
        hub.broadcast(sample(
            sequence,
            1,
            sequence
                == u64::try_from(MAX_VIEWER_FRAMES).expect("viewer frame limit should fit in u64"),
        ));
    }
    assert_eq!(queue(&frames).frames.len(), MAX_VIEWER_FRAMES);

    let hub = VideoHub::new();
    hub.broadcast(sample(0, 1, true));
    let (_, _, bytes) = hub.subscribe();
    hub.broadcast(sample(1, MAX_VIEWER_BYTES, true));
    assert_eq!(queue(&bytes).bytes, MAX_VIEWER_BYTES);
}

#[test]
fn overflow_keeps_arriving_keyframe_and_discards_delta() {
    let hub = VideoHub::new();
    hub.broadcast(sample(0, 1, true));
    let (_, _, slow) = hub.subscribe();
    let (_, _, independent) = hub.subscribe();
    for sequence in
        1..=u64::try_from(MAX_VIEWER_FRAMES).expect("viewer frame limit should fit in u64")
    {
        hub.broadcast(sample(sequence, 1, false));
    }
    {
        let mut independent_queue = queue(&independent);
        independent_queue.frames.clear();
        independent_queue.bytes = 0;
    }

    hub.broadcast(sample(20, 1, false));
    assert!(queue(&slow).frames.is_empty());
    assert_eq!(queue(&independent).frames.len(), 1);
    hub.broadcast(sample(21, 1, true));
    let slow_queue = queue(&slow);
    assert_eq!(slow_queue.frames.len(), 1);
    assert!(slow_queue.frames[0].key && slow_queue.frames[0].discontinuity);
}

#[tokio::test]
async fn viewer_starts_and_recovers_only_at_keyframe() {
    let hub = VideoHub::new();
    let (_, bootstrap, subscription) = hub.subscribe();
    assert!(bootstrap.is_empty());
    for sequence in 0..10 {
        hub.broadcast(VideoSample {
            data: vec![1].into(),
            key: false,
            discontinuity: false,
            metadata: metadata(sequence, 1),
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
