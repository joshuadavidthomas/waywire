mod quality;

use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::watch;
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
pub(crate) struct OwnershipEpoch(NonZeroU64);

impl OwnershipEpoch {
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
    ownership: Arc<AsyncMutex<OwnershipBook>>,
    ownership_changes: watch::Sender<OwnershipAvailability>,
    quality: Arc<Mutex<Quality>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnershipAvailability {
    Available,
    OwnedBy(SocketId),
}

struct OwnershipBook {
    input_owner: Option<InputOwnership>,
    next_epoch: OwnershipEpoch,
}

#[derive(Clone, Copy)]
struct InputOwnership {
    socket: SocketId,
    epoch: OwnershipEpoch,
}

impl Sessions {
    pub(crate) fn new(commands: CommandSink, bitrate: Kbps, fps: Fps) -> Self {
        let (ownership_changes, _) = watch::channel(OwnershipAvailability::Available);
        Self {
            commands,
            ownership: Arc::new(AsyncMutex::new(OwnershipBook {
                input_owner: None,
                next_epoch: OwnershipEpoch::FIRST,
            })),
            ownership_changes,
            quality: Arc::new(Mutex::new(Quality::new(bitrate, fps))),
        }
    }

    pub(crate) fn subscribe_ownership(&self) -> watch::Receiver<OwnershipAvailability> {
        self.ownership_changes.subscribe()
    }

    pub(crate) async fn acquire(&self, socket: SocketId) -> Result<OwnershipState> {
        let mut book = self.ownership.lock().await;
        if let Some(input_owner) = book.input_owner {
            return Ok(if input_owner.socket == socket {
                OwnershipState::Active
            } else {
                OwnershipState::Busy
            });
        }

        // ReleaseAll enters the FIFO before the new epoch becomes valid, so
        // no command from the new input owner can overtake the reset. The input ownership
        // lock can be held for at worst two PIPE_DEADLINEs while that write enters the FIFO.
        self.commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await?;
        let epoch = book.next_epoch;
        book.next_epoch = epoch.next();
        book.input_owner = Some(InputOwnership { socket, epoch });
        self.commands.set_active_ownership(epoch);
        self.ownership_changes
            .send_replace(OwnershipAvailability::OwnedBy(socket));
        info!(?socket, ?epoch, "input ownership acquired");
        Ok(OwnershipState::Active)
    }

    pub(crate) async fn release(&self, socket: SocketId) -> Result<ReleaseOutcome> {
        let mut book = self.ownership.lock().await;
        let Some(input_owner) = book.input_owner else {
            return Ok(ReleaseOutcome::NotInputOwner);
        };
        if input_owner.socket != socket {
            return Ok(ReleaseOutcome::NotInputOwner);
        }

        // The reset follows every command already queued by this epoch. Once
        // the authority is cleared, the writer drops any late stale command.
        // The input ownership lock can be held for at worst two PIPE_DEADLINEs during the send.
        self.commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await?;
        self.commands.clear_active_ownership();
        book.input_owner = None;
        self.ownership_changes
            .send_replace(OwnershipAvailability::Available);
        info!(
            socket = ?input_owner.socket,
            epoch = ?input_owner.epoch,
            "input ownership released"
        );
        Ok(ReleaseOutcome::Released)
    }

    async fn epoch(&self, socket: SocketId) -> Option<OwnershipEpoch> {
        self.ownership
            .lock()
            .await
            .input_owner
            .filter(|input_owner| input_owner.socket == socket)
            .map(|input_owner| input_owner.epoch)
    }

    pub(crate) async fn input(&self, socket: SocketId, command: Command) -> Result<InputOutcome> {
        let Some(epoch) = self.epoch(socket).await else {
            return Ok(InputOutcome::NotInputOwner);
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
        let book = self.ownership.lock().await;
        let Some(input_owner) = book.input_owner else {
            return Ok(FeedbackOutcome::NotInputOwner);
        };
        if input_owner.socket != socket {
            return Ok(FeedbackOutcome::NotInputOwner);
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
            // Quality is daemon state, not user input. Holding the input ownership
            // lock puts this command before a release and any later acquisition.
            // The lock can be held for at worst two PIPE_DEADLINEs during the send.
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
pub(crate) enum OwnershipState {
    Active,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReleaseOutcome {
    Released,
    NotInputOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputOutcome {
    Sent,
    NotInputOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeedbackOutcome {
    Applied(QualityLevels),
    NotInputOwner,
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

    async fn acquire_ownership_and_drain(
        sessions: &Sessions,
        commands: &mut TestCommandReceiver,
        socket: SocketId,
    ) {
        assert_eq!(
            sessions
                .acquire(socket)
                .await
                .expect("ownership should acquire"),
            OwnershipState::Active
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
    async fn second_acquire_is_busy_and_keeps_the_input_owner() {
        let (sessions, mut commands) = test_sessions();
        let input_owner = SocketId::new(1);
        let contender = SocketId::new(2);
        acquire_ownership_and_drain(&sessions, &mut commands, input_owner).await;

        assert_eq!(
            sessions
                .acquire(contender)
                .await
                .expect("busy check should succeed"),
            OwnershipState::Busy
        );
        assert_eq!(
            sessions
                .input(input_owner, Command::ReleaseAll(ReleaseAll))
                .await
                .expect("input owner command should succeed"),
            InputOutcome::Sent
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
    }

    #[tokio::test]
    async fn non_input_owner_release_does_not_change_or_publish_ownership() {
        let (sessions, mut commands) = test_sessions();
        let input_owner = SocketId::new(1);
        let mut ownership_changes = sessions.subscribe_ownership();
        acquire_ownership_and_drain(&sessions, &mut commands, input_owner).await;
        ownership_changes
            .changed()
            .await
            .expect("ownership acquisition should be published");
        assert_eq!(
            *ownership_changes.borrow_and_update(),
            OwnershipAvailability::OwnedBy(input_owner)
        );

        assert_eq!(
            sessions
                .release(SocketId::new(2))
                .await
                .expect("non-input-owner release should be a no-op"),
            ReleaseOutcome::NotInputOwner
        );
        assert!(
            timeout(Duration::from_millis(20), ownership_changes.changed())
                .await
                .is_err()
        );
        assert_eq!(
            sessions
                .input(input_owner, Command::ReleaseAll(ReleaseAll))
                .await
                .expect("input owner command should succeed"),
            InputOutcome::Sent
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
    }

    #[tokio::test]
    async fn input_owner_release_publishes_availability_to_all_subscribers() {
        let (sessions, mut commands) = test_sessions();
        let input_owner = SocketId::new(1);
        let mut first_waiter = sessions.subscribe_ownership();
        let mut second_waiter = sessions.subscribe_ownership();
        acquire_ownership_and_drain(&sessions, &mut commands, input_owner).await;
        first_waiter
            .changed()
            .await
            .expect("first waiter should observe acquisition");
        second_waiter
            .changed()
            .await
            .expect("second waiter should observe acquisition");

        assert_eq!(
            sessions
                .release(input_owner)
                .await
                .expect("input owner release should succeed"),
            ReleaseOutcome::Released
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
        first_waiter
            .changed()
            .await
            .expect("first waiter should observe release");
        second_waiter
            .changed()
            .await
            .expect("second waiter should observe release");
        assert_eq!(
            *first_waiter.borrow_and_update(),
            OwnershipAvailability::Available
        );
        assert_eq!(
            *second_waiter.borrow_and_update(),
            OwnershipAvailability::Available
        );
    }

    #[tokio::test]
    async fn non_input_owner_input_returns_not_owner_without_a_command() {
        let (sessions, mut commands) = test_sessions();
        acquire_ownership_and_drain(&sessions, &mut commands, SocketId::new(1)).await;

        assert_eq!(
            sessions
                .input(SocketId::new(2), Command::ReleaseAll(ReleaseAll))
                .await
                .expect("non-input-owner check should succeed"),
            InputOutcome::NotInputOwner
        );
        assert!(
            timeout(Duration::from_millis(20), commands.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn non_input_owner_feedback_keeps_quality_and_sends_no_command() {
        let (sessions, mut commands) = test_sessions();
        acquire_ownership_and_drain(&sessions, &mut commands, SocketId::new(1)).await;
        let before = sessions.quality();

        assert_eq!(
            sessions
                .feedback(SocketId::new(2), bad_feedback())
                .await
                .expect("non-input-owner feedback check should succeed"),
            FeedbackOutcome::NotInputOwner
        );
        assert_eq!(sessions.quality(), before);
        assert!(
            timeout(Duration::from_millis(20), commands.recv())
                .await
                .is_err()
        );
    }
}
