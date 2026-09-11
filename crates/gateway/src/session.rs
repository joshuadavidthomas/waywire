mod quality;

use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::Mutex as AsyncMutex;
use tracing::info;
use waywire_protocol::browser::Feedback;
use waywire_protocol::browser::QualityLevels;
use waywire_protocol::pipe::Command;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::Kbps;
use waywire_protocol::pipe::Quality as QualityCommand;
use waywire_protocol::pipe::ReleaseAll;

use self::quality::Quality;
use crate::daemon::CommandSink;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LeaseEpoch(NonZeroU64);

impl LeaseEpoch {
    pub(crate) const FIRST: Self = Self(NonZeroU64::MIN);

    #[must_use]
    pub(crate) const fn get(self) -> u64 {
        self.0.get()
    }

    pub(crate) const fn from_nonzero(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub(crate) fn next(self) -> Self {
        NonZeroU64::new(self.get().wrapping_add(1)).map_or(Self::FIRST, Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SocketId(u64);

impl SocketId {
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
pub(crate) struct Sessions {
    commands: CommandSink,
    lease: Arc<AsyncMutex<LeaseBook>>,
    quality: Arc<Mutex<Quality>>,
}

struct LeaseBook {
    owner: Option<Lease>,
    next_epoch: LeaseEpoch,
}

#[derive(Clone, Copy)]
struct Lease {
    socket: SocketId,
    epoch: LeaseEpoch,
}

impl Sessions {
    pub(crate) fn new(commands: CommandSink, bitrate: Kbps, fps: Fps) -> Self {
        Self {
            commands,
            lease: Arc::new(AsyncMutex::new(LeaseBook {
                owner: None,
                next_epoch: LeaseEpoch::FIRST,
            })),
            quality: Arc::new(Mutex::new(Quality::new(bitrate, fps))),
        }
    }

    pub(crate) async fn acquire(&self, socket: SocketId) -> Result<LeaseState> {
        let mut book = self.lease.lock().await;
        if let Some(owner) = book.owner {
            return Ok(if owner.socket == socket {
                LeaseState::Active
            } else {
                LeaseState::Busy
            });
        }

        // ReleaseAll enters the FIFO before the new epoch becomes valid, so
        // no command from the new owner can overtake the reset. The lease can
        // be held for at worst two PIPE_DEADLINEs while that write enters the FIFO.
        self.commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await?;
        let epoch = book.next_epoch;
        book.next_epoch = epoch.next();
        book.owner = Some(Lease { socket, epoch });
        self.commands.set_active_lease(epoch);
        info!(?socket, ?epoch, "input lease acquired");
        Ok(LeaseState::Active)
    }

    pub(crate) async fn release(&self, socket: SocketId) -> Result<()> {
        let mut book = self.lease.lock().await;
        let Some(owner) = book.owner else {
            return Ok(());
        };
        if owner.socket != socket {
            return Ok(());
        }

        // The reset follows every command already queued by this epoch. Once
        // the authority is cleared, the writer drops any late stale command.
        // The lease can be held for at worst two PIPE_DEADLINEs during the send.
        self.commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await?;
        self.commands.clear_active_lease();
        book.owner = None;
        info!(socket = ?owner.socket, epoch = ?owner.epoch, "input lease released");
        Ok(())
    }

    async fn epoch(&self, socket: SocketId) -> Option<LeaseEpoch> {
        self.lease
            .lock()
            .await
            .owner
            .filter(|owner| owner.socket == socket)
            .map(|owner| owner.epoch)
    }

    pub(crate) async fn input(&self, socket: SocketId, command: Command) -> Result<InputOutcome> {
        let Some(epoch) = self.epoch(socket).await else {
            return Ok(InputOutcome::NotLeaseOwner);
        };
        self.commands.input(epoch, command).await?;
        Ok(InputOutcome::Sent)
    }

    pub(crate) fn quality(&self) -> QualityLevels {
        self.quality
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .levels()
    }

    pub(crate) async fn feedback(
        &self,
        socket: SocketId,
        feedback: Feedback,
    ) -> Result<FeedbackOutcome> {
        let book = self.lease.lock().await;
        let Some(owner) = book.owner else {
            return Ok(FeedbackOutcome::NotLeaseOwner);
        };
        if owner.socket != socket {
            return Ok(FeedbackOutcome::NotLeaseOwner);
        }

        let (change, levels) = {
            let mut quality = self.quality.lock().unwrap_or_else(PoisonError::into_inner);
            let change = quality.update(&feedback, Instant::now());
            (change, quality.levels())
        };
        if let Some(change) = change {
            info!(
                ?socket,
                old = ?change.old,
                new = ?change.new,
                "quality levels changed"
            );
            // Quality is daemon state, not user input. Holding the lease lock
            // puts this command before a release and any later acquisition.
            // The lease can be held for at worst two PIPE_DEADLINEs during the send.
            self.commands
                .system(Command::Quality(QualityCommand {
                    bitrate_kbps: change.new.bitrate,
                    fps: change.new.fps,
                    scale_percent: change.new.scale,
                }))
                .await?;
        }

        Ok(FeedbackOutcome::Applied(levels))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaseState {
    Active,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputOutcome {
    Sent,
    NotLeaseOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeedbackOutcome {
    Applied(QualityLevels),
    NotLeaseOwner,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;
    use waywire_protocol::InvalidValue;
    use waywire_protocol::browser::FeedbackValues;

    use super::*;
    use crate::daemon::TestCommandReceiver;
    use crate::daemon::test_command_sink;

    fn test_sessions() -> (Sessions, TestCommandReceiver) {
        let (commands, receiver) = test_command_sink();
        (
            Sessions::new(commands, value(Kbps::new(8_000)), value(Fps::new(60))),
            receiver,
        )
    }

    async fn acquire_and_drain(
        sessions: &Sessions,
        commands: &mut TestCommandReceiver,
        socket: SocketId,
    ) {
        assert_eq!(
            sessions
                .acquire(socket)
                .await
                .expect("lease should acquire"),
            LeaseState::Active
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
    }

    pub(super) fn value<T>(result: Result<T, InvalidValue>) -> T {
        result.expect("test quality value should be valid")
    }

    pub(super) fn feedback(
        received: u32,
        presented: u32,
        queue_peak: u32,
        queue_busy_ms: f64,
        dropped: u32,
        rtt: f64,
    ) -> Feedback {
        Feedback::new(FeedbackValues {
            received,
            presented,
            queue_peak,
            queue_busy_ms,
            sample_ms: 1000.0,
            dropped,
            rtt,
        })
        .expect("test feedback should be valid")
    }

    pub(super) fn bad_feedback() -> Feedback {
        feedback(50, 50, 5, 200.0, 0, 20.0)
    }

    pub(super) fn good_feedback() -> Feedback {
        feedback(50, 45, 0, 0.0, 0, 20.0)
    }

    #[tokio::test]
    async fn second_acquire_is_busy_and_keeps_the_owner() {
        let (sessions, mut commands) = test_sessions();
        let owner = SocketId::new(1);
        let contender = SocketId::new(2);
        acquire_and_drain(&sessions, &mut commands, owner).await;

        assert_eq!(
            sessions
                .acquire(contender)
                .await
                .expect("busy check should succeed"),
            LeaseState::Busy
        );
        assert_eq!(
            sessions
                .input(owner, Command::ReleaseAll(ReleaseAll))
                .await
                .expect("owner input should succeed"),
            InputOutcome::Sent
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
    }

    #[tokio::test]
    async fn nonowner_release_does_not_change_the_owner() {
        let (sessions, mut commands) = test_sessions();
        let owner = SocketId::new(1);
        acquire_and_drain(&sessions, &mut commands, owner).await;

        sessions
            .release(SocketId::new(2))
            .await
            .expect("nonowner release should be a no-op");
        assert_eq!(
            sessions
                .input(owner, Command::ReleaseAll(ReleaseAll))
                .await
                .expect("owner input should succeed"),
            InputOutcome::Sent
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
    }

    #[tokio::test]
    async fn nonowner_input_returns_not_owner_without_a_command() {
        let (sessions, mut commands) = test_sessions();
        acquire_and_drain(&sessions, &mut commands, SocketId::new(1)).await;

        assert_eq!(
            sessions
                .input(SocketId::new(2), Command::ReleaseAll(ReleaseAll))
                .await
                .expect("nonowner input check should succeed"),
            InputOutcome::NotLeaseOwner
        );
        assert!(
            timeout(Duration::from_millis(20), commands.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn nonowner_feedback_keeps_quality_and_sends_no_command() {
        let (sessions, mut commands) = test_sessions();
        acquire_and_drain(&sessions, &mut commands, SocketId::new(1)).await;
        let before = sessions.quality();

        assert_eq!(
            sessions
                .feedback(SocketId::new(2), bad_feedback())
                .await
                .expect("nonowner feedback check should succeed"),
            FeedbackOutcome::NotLeaseOwner
        );
        assert_eq!(sessions.quality(), before);
        assert!(
            timeout(Duration::from_millis(20), commands.recv())
                .await
                .is_err()
        );
    }
}
