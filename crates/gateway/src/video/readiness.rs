use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

/// Shared video readiness state.
///
/// Each critical section only reads the state or replaces the `Copy` value in one assignment, so
/// recovering a poisoned mutex cannot expose a partly-applied transition.
#[derive(Clone)]
pub(crate) struct Readiness {
    state: Arc<Mutex<ReadinessState>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadinessState {
    WaitingForFirstFrame,
    Ready,
    WaitingForKeyframe,
    Stopped,
}

impl Readiness {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ReadinessState::WaitingForFirstFrame)),
        }
    }

    pub(crate) fn mark_video_ready(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = match *state {
            ReadinessState::WaitingForFirstFrame
            | ReadinessState::Ready
            | ReadinessState::WaitingForKeyframe => ReadinessState::Ready,
            ReadinessState::Stopped => ReadinessState::Stopped,
        };
    }

    pub(crate) fn await_keyframe(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = match *state {
            ReadinessState::WaitingForFirstFrame => ReadinessState::WaitingForFirstFrame,
            ReadinessState::Ready | ReadinessState::WaitingForKeyframe => {
                ReadinessState::WaitingForKeyframe
            }
            ReadinessState::Stopped => ReadinessState::Stopped,
        };
    }

    pub(crate) fn stop(&self) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = ReadinessState::Stopped;
    }

    pub(crate) fn is_ready(&self) -> bool {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) == ReadinessState::Ready
    }

    pub(crate) fn needs_startup_frame(&self) -> bool {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
            == ReadinessState::WaitingForFirstFrame
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> ReadinessState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
