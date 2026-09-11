use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

/// Nothing is published behind this flag: the frames that justify `Ready` reach viewers through
/// `VideoHub`'s mutex and `Notify`, and both readers look only at the tag. `Relaxed` would therefore
/// be sound; Acquire/Release costs nothing at one update per frame and keeps the edge in place for
/// a future reader that does hang data off it.
#[derive(Clone)]
pub(crate) struct Readiness {
    state: Arc<AtomicU8>,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadinessState {
    WaitingForFirstFrame = 0,
    Ready = 1,
    WaitingForKeyframe = 2,
    Stopped = 3,
}

impl Readiness {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(ReadinessState::WaitingForFirstFrame as u8)),
        }
    }

    fn decode(tag: u8) -> ReadinessState {
        match tag {
            0 => ReadinessState::WaitingForFirstFrame,
            1 => ReadinessState::Ready,
            2 => ReadinessState::WaitingForKeyframe,
            3 => ReadinessState::Stopped,
            _ => panic!("defect: invalid ReadinessState tag {tag}"),
        }
    }

    pub(crate) fn mark_video_ready(&self) {
        let _ =
            self.state.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |tag| match Self::decode(tag) {
                    ReadinessState::WaitingForFirstFrame
                    | ReadinessState::Ready
                    | ReadinessState::WaitingForKeyframe => Some(ReadinessState::Ready as u8),
                    ReadinessState::Stopped => None,
                },
            );
    }

    pub(crate) fn await_keyframe(&self) {
        let _ =
            self.state.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |tag| match Self::decode(tag) {
                    ReadinessState::Ready => Some(ReadinessState::WaitingForKeyframe as u8),
                    ReadinessState::WaitingForFirstFrame
                    | ReadinessState::WaitingForKeyframe
                    | ReadinessState::Stopped => None,
                },
            );
    }

    pub(crate) fn stop(&self) {
        self.state
            .store(ReadinessState::Stopped as u8, Ordering::Release);
    }

    pub(crate) fn is_ready(&self) -> bool {
        Self::decode(self.state.load(Ordering::Acquire)) == ReadinessState::Ready
    }

    pub(crate) fn needs_startup_frame(&self) -> bool {
        Self::decode(self.state.load(Ordering::Acquire)) == ReadinessState::WaitingForFirstFrame
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> ReadinessState {
        Self::decode(self.state.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::Readiness;
    use super::ReadinessState;

    #[test]
    fn video_ready_cannot_revive_a_stopped_runtime() {
        let readiness = Readiness::new();
        readiness.stop();
        readiness.mark_video_ready();
        assert_eq!(readiness.state(), ReadinessState::Stopped);
    }

    #[test]
    fn recovery_wait_does_not_restore_the_startup_deadline() {
        let readiness = Readiness::new();
        readiness.mark_video_ready();
        readiness.await_keyframe();
        assert_eq!(readiness.state(), ReadinessState::WaitingForKeyframe);
    }

    #[test]
    fn ready_runtime_stays_healthy_while_video_is_idle() {
        let readiness = Readiness::new();
        readiness.mark_video_ready();
        assert_eq!(readiness.state(), ReadinessState::Ready);
    }
}
