use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use sprite_desktop_protocol::browser::Continuity;
use sprite_desktop_protocol::browser::FrameKind;
use sprite_desktop_protocol::browser::VideoSample;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::Generation;
use tokio::sync::Notify;
use tracing::debug;

use super::GopState;

// At the protocol maximum this spans two seconds plus their leading keyframe. Production
// keyframes arrive four times per second, so an oversized GOP reaches the byte bound first.
const MAX_GOP_FRAMES: usize = 241;
const MAX_GOP_BYTES: usize = 32 << 20;
const VIEWER_BYTES: usize = 32 << 20;

#[derive(Clone)]
pub(crate) struct VideoHub {
    inner: Arc<Mutex<Hub>>,
}

struct Hub {
    subscribers: HashMap<SubscriberId, ViewerHandle>,
    next_subscriber: u64,
    gop: Vec<VideoSample>,
    gop_bytes: usize,
    generation: Option<Generation>,
    viewer_bounds: ViewerBounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SubscriberId(u64);

#[derive(Clone)]
struct ViewerHandle {
    queue: Arc<Mutex<ViewerQueue>>,
    notify: Arc<Notify>,
}

/// A viewer can hold two GOPs at the configured maximum frame rate. The byte bound is the
/// same as the GOP cache and exceeds the encoder's 16 MiB maximum access unit.
#[derive(Clone, Copy)]
struct ViewerBounds {
    frames: usize,
    bytes: usize,
}

impl ViewerBounds {
    fn for_max_fps(fps: Fps) -> Self {
        let keyframe_interval = fps.keyframe_interval() as usize;
        Self {
            frames: keyframe_interval.saturating_mul(2),
            bytes: VIEWER_BYTES,
        }
    }

    #[cfg(test)]
    fn new(frames: usize, bytes: usize) -> Self {
        Self { frames, bytes }
    }
}

struct ViewerQueue {
    frames: VecDeque<VideoSample>,
    bytes: usize,
    keyframe: GopState,
    generation: Option<Generation>,
    bounds: ViewerBounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pushed {
    Queued,
    Dropped,
}

impl ViewerQueue {
    fn new(keyframe: GopState, generation: Option<Generation>, bounds: ViewerBounds) -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            keyframe,
            generation,
            bounds,
        }
    }

    fn push(&mut self, mut sample: VideoSample) -> Pushed {
        let generation_reset = self
            .generation
            .is_some_and(|current| current != sample.metadata.generation);
        self.generation = Some(sample.metadata.generation);
        let stream_reset = sample.continuity == Continuity::AfterGap || generation_reset;
        if stream_reset {
            self.clear();
        }
        let recovering = self.keyframe == GopState::Recovering;
        if recovering && sample.kind != FrameKind::Key {
            return Pushed::Dropped;
        }
        if sample.kind == FrameKind::Key {
            self.keyframe = GopState::KeyframeCached;
        }
        let overflow = self.frames.len() >= self.bounds.frames
            || self.bytes.saturating_add(sample.data.len()) > self.bounds.bytes;
        if overflow {
            debug!(
                frames = self.frames.len().saturating_add(1),
                bytes = self.bytes.saturating_add(sample.data.len()),
                kind = ?sample.kind,
                "viewer queue overflow"
            );
            self.clear();
            if sample.kind != FrameKind::Key {
                return Pushed::Dropped;
            }
            self.keyframe = GopState::KeyframeCached;
        }
        if stream_reset || recovering || overflow {
            sample.continuity = Continuity::AfterGap;
        }
        self.bytes += sample.data.len();
        self.frames.push_back(sample);
        Pushed::Queued
    }

    fn pop(&mut self) -> Option<VideoSample> {
        let frame = self.frames.pop_front();
        if let Some(value) = &frame {
            self.bytes -= value.data.len();
        }
        frame
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
        self.keyframe = GopState::Recovering;
    }
}

pub(crate) struct VideoSubscription {
    hub: VideoHub,
    id: SubscriberId,
    queue: Arc<Mutex<ViewerQueue>>,
    notify: Arc<Notify>,
}

impl VideoSubscription {
    pub(crate) async fn next(&mut self) -> VideoSample {
        loop {
            let notified = self.notify.notified();
            if let Some(frame) = self
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop()
            {
                return frame;
            }
            // All durable progress lives in the queue. Cancellation can only discard this
            // waiter, and the next call checks the queue before waiting again.
            notified.await;
        }
    }
}

impl Drop for VideoSubscription {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.id);
    }
}

impl VideoHub {
    pub(crate) fn new(max_fps: Fps) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Hub {
                subscribers: HashMap::new(),
                next_subscriber: 1,
                gop: Vec::new(),
                gop_bytes: 0,
                generation: None,
                viewer_bounds: ViewerBounds::for_max_fps(max_fps),
            })),
        }
    }

    pub(crate) fn subscribe(&self) -> VideoSubscription {
        let mut hub = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let id = SubscriberId(hub.next_subscriber);
        hub.next_subscriber = hub.next_subscriber.wrapping_add(1);
        let bounds = hub.viewer_bounds;
        let mut viewer_queue = ViewerQueue::new(GopState::Recovering, hub.generation, bounds);
        // This explicitly accepts a subscriber joining after resync but before the recovery
        // keyframe. It receives the pre-resync GOP, a valid picture existing viewers saw. At
        // most one cached GOP is seeded here, and the recovery keyframe clears it.
        for frame in &hub.gop {
            match viewer_queue.push(frame.clone()) {
                Pushed::Queued => {}
                Pushed::Dropped => {
                    debug!(
                        gop_frames = hub.gop.len(),
                        gop_bytes = hub.gop_bytes,
                        viewer_frame_bound = bounds.frames,
                        viewer_byte_bound = bounds.bytes,
                        "cached GOP exceeds viewer bootstrap bounds"
                    );
                    break;
                }
            }
        }
        let queue = Arc::new(Mutex::new(viewer_queue));
        let notify = Arc::new(Notify::new());
        let _ = hub.subscribers.insert(
            id,
            ViewerHandle {
                queue: Arc::clone(&queue),
                notify: Arc::clone(&notify),
            },
        );
        drop(hub);
        VideoSubscription {
            hub: self.clone(),
            id,
            queue,
            notify,
        }
    }

    fn unsubscribe(&self, id: SubscriberId) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .subscribers
            .remove(&id);
    }

    // VideoWorker::run is the sole production caller. Fan-out happens without the hub lock, so
    // per-viewer ordering depends on that caller broadcasting each frame in sequence.
    pub(super) fn broadcast(&self, sample: &VideoSample) -> GopState {
        let generation = sample.metadata.generation;
        let (handles, state, stream_reset) = {
            let mut hub = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let stream_reset = sample.continuity == Continuity::AfterGap
                || hub
                    .generation
                    .is_some_and(|current| current != sample.metadata.generation);
            hub.generation = Some(sample.metadata.generation);
            if stream_reset {
                hub.gop.clear();
                hub.gop_bytes = 0;
            }
            if sample.kind == FrameKind::Key {
                hub.gop.clear();
                hub.gop_bytes = sample.data.len();
                hub.gop.push(sample.clone());
            } else if !hub.gop.is_empty() {
                if hub.gop.len() < MAX_GOP_FRAMES
                    && hub.gop_bytes + sample.data.len() <= MAX_GOP_BYTES
                {
                    hub.gop_bytes += sample.data.len();
                    hub.gop.push(sample.clone());
                } else {
                    hub.gop.clear();
                    hub.gop_bytes = 0;
                }
            }
            let state = if hub.gop.first().is_some_and(|frame| {
                frame.kind == FrameKind::Key && frame.metadata.generation == generation
            }) {
                GopState::KeyframeCached
            } else {
                GopState::Recovering
            };
            (
                hub.subscribers.values().cloned().collect::<Vec<_>>(),
                state,
                stream_reset,
            )
        };

        for handle in handles {
            let mut outgoing = sample.clone();
            if stream_reset {
                outgoing.continuity = Continuity::AfterGap;
            }
            let pushed = handle
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(outgoing);
            match pushed {
                Pushed::Queued => handle.notify.notify_one(),
                Pushed::Dropped => {}
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sprite_desktop_protocol::pipe::Generation;

    use super::*;
    use crate::video::tests::fps;
    use crate::video::tests::metadata;
    use crate::video::tests::sample;
    use crate::video::tests::video_hub;

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
}
