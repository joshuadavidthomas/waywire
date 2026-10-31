use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::TryAcquireError;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::timeout;
use tracing::debug;
use waywire_protocol::Record;
use waywire_protocol::pipe::Command;

use crate::session::OwnershipEpoch;
use crate::video::Readiness;

pub(super) const COMMAND_COUNT: usize = 128;
pub(super) const COMMAND_BYTES: usize = 2 * 1024 * 1024;
const PIPE_DEADLINE: Duration = Duration::from_secs(2);
const WRITE_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Authority {
    System,
    InputOwnership(OwnershipEpoch),
}

struct Request {
    authority: Authority,
    encoded: Vec<u8>,
    permit: OwnedSemaphorePermit,
}

struct AuthorizedCommand {
    encoded: Vec<u8>,
    permit: OwnedSemaphorePermit,
}

pub(super) struct CommandReader {
    requests: mpsc::Receiver<Request>,
    active_ownership: Arc<AtomicU64>,
}

impl CommandReader {
    async fn recv(&mut self) -> Option<AuthorizedCommand> {
        while let Some(request) = self.requests.recv().await {
            let authorized = match request.authority {
                Authority::System => true,
                Authority::InputOwnership(ownership) => {
                    NonZeroU64::new(self.active_ownership.load(Ordering::Acquire))
                        .map(OwnershipEpoch::from_nonzero)
                        == Some(ownership)
                }
            };
            if authorized {
                return Some(AuthorizedCommand {
                    encoded: request.encoded,
                    permit: request.permit,
                });
            }
        }
        None
    }
}

#[cfg(test)]
pub(crate) struct TestCommandReceiver(CommandReader);

#[cfg(test)]
impl TestCommandReceiver {
    pub(crate) async fn recv(&mut self) -> Option<Command> {
        let command = self.0.recv().await?;
        Some(Command::decode(&command.encoded).expect("test command should decode"))
    }

    pub(crate) fn start_writer(
        self,
        output: tokio::io::DuplexStream,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let _result = write_commands(output, self.0).await;
        })
    }
}

#[cfg(test)]
pub(crate) fn test_command_sink() -> (CommandSink, TestCommandReceiver) {
    let (fatal, _) = mpsc::channel(1);
    let (sink, receiver) = CommandSink::new(COMMAND_COUNT, COMMAND_BYTES, fatal, Readiness::new());
    (sink, TestCommandReceiver(receiver))
}

#[cfg(test)]
pub(crate) fn test_stalled_command_sink() -> (
    CommandSink,
    TestCommandReceiver,
    mpsc::Receiver<CommandSinkError>,
    Readiness,
) {
    let (fatal, errors) = mpsc::channel(1);
    let readiness = Readiness::new();
    let (sink, receiver) = CommandSink::new(1, COMMAND_BYTES, fatal, readiness.clone());
    (sink, TestCommandReceiver(receiver), errors, readiness)
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(crate) enum CommandSinkError {
    #[error("daemon command length {length} exceeds the pipe budget's integer limit")]
    LengthOverflow { length: usize },
    #[error("daemon command byte budget closed")]
    ByteBudgetClosed,
    #[error("daemon command byte budget stalled after {deadline:?}", deadline = PIPE_DEADLINE)]
    ByteBudgetStalled,
    #[error("daemon command writer stopped")]
    WriterStopped,
    #[error("daemon command count budget stalled after {deadline:?}", deadline = PIPE_DEADLINE)]
    CountBudgetStalled,
}

#[derive(Clone)]
pub(crate) struct CommandSink {
    tx: mpsc::Sender<Request>,
    budget: Arc<Semaphore>,
    fatal: mpsc::Sender<CommandSinkError>,
    readiness: Readiness,
    active_ownership: Arc<AtomicU64>,
}

impl CommandSink {
    pub(super) fn new(
        capacity: usize,
        budget_bytes: usize,
        fatal: mpsc::Sender<CommandSinkError>,
        readiness: Readiness,
    ) -> (Self, CommandReader) {
        let (tx, requests) = mpsc::channel(capacity);
        let active_ownership = Arc::new(AtomicU64::new(0));
        (
            Self {
                tx,
                budget: Arc::new(Semaphore::new(budget_bytes)),
                fatal,
                readiness,
                active_ownership: Arc::clone(&active_ownership),
            },
            CommandReader {
                requests,
                active_ownership,
            },
        )
    }

    async fn send(&self, authority: Authority, command: Command) -> Result<(), CommandSinkError> {
        let encoded = command.encode();
        let length = encoded.len();
        let Ok(count) = u32::try_from(length) else {
            return Err(CommandSinkError::LengthOverflow { length });
        };
        let permit = match timeout(
            PIPE_DEADLINE,
            Arc::clone(&self.budget).acquire_many_owned(count),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_closed)) => return Err(CommandSinkError::ByteBudgetClosed),
            Err(_elapsed) => return Err(CommandSinkError::ByteBudgetStalled),
        };
        let request = Request {
            authority,
            encoded,
            permit,
        };
        match timeout(PIPE_DEADLINE, self.tx.send(request)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_closed)) => Err(CommandSinkError::WriterStopped),
            Err(_elapsed) => Err(CommandSinkError::CountBudgetStalled),
        }
    }

    fn fail(&self, error: CommandSinkError) -> CommandSinkError {
        self.readiness.stop();
        // A full queue already holds a fatal error; a closed queue means the
        // supervisor is stopping. Neither needs another shutdown notification,
        // and the original error still goes back to this caller.
        if let Err(notification_error) = self.fatal.try_send(error.clone()) {
            debug!(%notification_error, "fatal command notification no longer needed");
        }
        error
    }

    /// Sends a gateway-internal command.
    ///
    /// A closed writer is fatal, but temporary queue backpressure is not.
    pub(crate) async fn system(&self, command: Command) -> Result<(), CommandSinkError> {
        match self.send(Authority::System, command).await {
            Ok(()) => Ok(()),
            Err(
                error
                @ (CommandSinkError::ByteBudgetStalled | CommandSinkError::CountBudgetStalled),
            ) => Err(error),
            Err(error) => Err(self.fail(error)),
        }
    }

    /// Sends an input-owner command from a browser socket.
    ///
    /// Send failures are returned to the caller so a stalled command pipe ends
    /// only that socket instead of the whole session.
    pub(crate) async fn input(
        &self,
        ownership: OwnershipEpoch,
        command: Command,
    ) -> Result<(), CommandSinkError> {
        self.send(Authority::InputOwnership(ownership), command)
            .await
    }

    /// Sends a command with system authority on behalf of a browser socket.
    ///
    /// Send failures are returned to the caller so a stalled command pipe ends
    /// only that socket instead of the whole session.
    pub(crate) async fn browser_system(&self, command: Command) -> Result<(), CommandSinkError> {
        self.send(Authority::System, command).await
    }

    /// Sends a gateway-internal command without blocking.
    ///
    /// Returns `Ok(false)` when backpressure dropped the command. Use this only
    /// for idempotent state where losing an update under backpressure is fine.
    pub(crate) fn try_system(&self, command: &Command) -> Result<bool, CommandSinkError> {
        let encoded = command.encode();
        let length = encoded.len();
        let Ok(count) = u32::try_from(length) else {
            return Err(CommandSinkError::LengthOverflow { length });
        };
        let permit = match Arc::clone(&self.budget).try_acquire_many_owned(count) {
            Ok(permit) => permit,
            Err(TryAcquireError::Closed) => return Err(CommandSinkError::ByteBudgetClosed),
            Err(TryAcquireError::NoPermits) => return Ok(false),
        };
        let request = Request {
            authority: Authority::System,
            encoded,
            permit,
        };
        match self.tx.try_send(request) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Closed(_)) => Err(CommandSinkError::WriterStopped),
        }
    }

    pub(crate) fn set_active_ownership(&self, ownership: OwnershipEpoch) {
        // Release publishes the epoch before its commands enter the FIFO. The
        // reader's Acquire either sees this epoch or rejects the command.
        self.active_ownership
            .store(ownership.get(), Ordering::Release);
    }

    pub(crate) fn clear_active_ownership(&self) {
        // Handback invalidates the epoch after its command enters the FIFO. A reader that
        // observes the clear can only drop late input-owner commands; system commands
        // remain ordered by the channel.
        self.active_ownership.store(0, Ordering::Release);
    }
}

#[derive(Debug, Error)]
pub(super) enum CommandWriterError {
    #[error("command queue closed")]
    QueueClosed,
    #[error("write daemon command: {0}")]
    Write(#[source] std::io::Error),
    #[error("daemon command write stalled after {WRITE_DEADLINE:?}")]
    WriteStalled,
}

pub(super) async fn write_commands<W>(
    mut output: W,
    mut commands: CommandReader,
) -> Result<(), CommandWriterError>
where
    W: AsyncWrite + Unpin,
{
    while let Some(command) = commands.recv().await {
        let AuthorizedCommand { encoded, permit } = command;
        let write_result = timeout(WRITE_DEADLINE, output.write_all(&encoded)).await;
        drop(permit);
        write_result
            .map_err(|_elapsed| CommandWriterError::WriteStalled)?
            .map_err(CommandWriterError::Write)?;
    }
    Err(CommandWriterError::QueueClosed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::sync::mpsc;
    use waywire_protocol::Record;
    use waywire_protocol::browser::Feedback;
    use waywire_protocol::browser::FeedbackValues;
    use waywire_protocol::pipe::Fps;
    use waywire_protocol::pipe::Generation;
    use waywire_protocol::pipe::InputLease;
    use waywire_protocol::pipe::Kbps;
    use waywire_protocol::pipe::KeyframeReadiness;
    use waywire_protocol::pipe::KeyframeState;
    use waywire_protocol::pipe::Quality;
    use waywire_protocol::pipe::ReleaseAll;
    use waywire_protocol::pipe::ScalePercent;

    use super::*;
    use crate::session::InputOutcome;
    use crate::session::OwnershipState;
    use crate::session::Sessions;
    use crate::session::SocketId;
    use crate::video::Readiness;
    use crate::video::ReadinessState;

    fn command_sink(capacity: usize) -> (CommandSink, CommandReader) {
        let (fatal, _) = mpsc::channel(1);
        CommandSink::new(capacity, COMMAND_BYTES, fatal, Readiness::new())
    }

    #[tokio::test(start_paused = true)]
    async fn slow_pipe_write_remains_pending_and_resumes() {
        let (commands, reader) = command_sink(1);
        let (mut input, output) = tokio::io::duplex(1);
        let writer = tokio::spawn(write_commands(output, reader));
        let record = Command::ReleaseAll(ReleaseAll);
        let expected = record.encode();
        commands
            .browser_system(record)
            .await
            .expect("queue command");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !writer.is_finished(),
            "a blocked pipe must not stop the session"
        );
        let mut actual = vec![0; expected.len()];
        input
            .read_exact(&mut actual)
            .await
            .expect("read resumed command");
        assert_eq!(actual, expected);
        drop(commands);
        assert!(matches!(
            writer.await.expect("writer task"),
            Err(CommandWriterError::QueueClosed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_pipe_write_fails_after_thirty_seconds() {
        let (commands, reader) = command_sink(1);
        let (_input, output) = tokio::io::duplex(1);
        let writer = tokio::spawn(write_commands(output, reader));
        commands
            .browser_system(Command::ReleaseAll(ReleaseAll))
            .await
            .expect("queue command");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        let error = timeout(Duration::from_secs(1), writer)
            .await
            .expect("writer must stop")
            .expect("writer task")
            .expect_err("stalled write must fail");
        assert!(matches!(error, CommandWriterError::WriteStalled));
    }

    #[tokio::test]
    async fn ownership_transitions_keep_fifo_order_and_reject_stale_input() {
        let (commands, command_reader) = command_sink(8);
        let (mut output, daemon_input) = tokio::io::duplex(1024);
        let writer = tokio::spawn(write_commands(daemon_input, command_reader));
        let first = OwnershipEpoch::FIRST;
        let second = first.next();

        commands.set_active_ownership(first);
        let first_input = Command::KeyframeReadiness(KeyframeReadiness {
            generation: Generation::new(1).expect("test generation should be valid"),
            state: KeyframeState::Cached,
        });
        let first_bytes = first_input.encode();
        commands
            .input(first, first_input)
            .await
            .expect("first input-owner command should queue");
        let mut written = vec![0; first_bytes.len()];
        output
            .read_exact(&mut written)
            .await
            .expect("active input-owner command should be written");
        assert_eq!(written, first_bytes);

        commands.set_active_ownership(second);
        commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await
            .expect("ownership transition reset should queue");
        commands
            .input(
                first,
                Command::KeyframeReadiness(KeyframeReadiness {
                    generation: Generation::new(2).expect("test generation should be valid"),
                    state: KeyframeState::Missing,
                }),
            )
            .await
            .expect("stale input-owner command should enter the queue before filtering");
        let second_input = Command::Quality(Quality {
            bitrate_kbps: Kbps::new(8_000).expect("test bitrate should be valid"),
            fps: Fps::new(60).expect("test frame rate should be valid"),
            scale_percent: ScalePercent::new(100).expect("test scale should be valid"),
            crf: waywire_protocol::pipe::Crf::new(23).expect("test CRF should be valid"),
            chroma: waywire_protocol::pipe::Chroma::Yuv444,
        });
        let mut expected = Command::ReleaseAll(ReleaseAll).encode();
        expected.extend_from_slice(&second_input.encode());
        commands
            .input(second, second_input)
            .await
            .expect("second input-owner command should queue");
        drop(commands);

        written.clear();
        output
            .read_to_end(&mut written)
            .await
            .expect("command writer output should close");
        assert_eq!(written, expected);
        assert!(matches!(
            writer.await.expect("command writer task should run"),
            Err(CommandWriterError::QueueClosed)
        ));
    }

    #[tokio::test]
    async fn closed_command_budget_keeps_its_typed_terminal_reason() {
        let (commands, request_guard) = command_sink(1);
        commands.budget.close();
        let error = commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await
            .expect_err("closed byte budget should reject a command");
        assert_eq!(error, CommandSinkError::ByteBudgetClosed);
        assert_eq!(commands.readiness.state(), ReadinessState::Stopped);
        drop(request_guard);
    }

    #[tokio::test]
    async fn browser_command_errors_return_without_a_fatal_shutdown() {
        let (fatal, mut fatal_events) = mpsc::channel(1);
        let (commands, request_guard) = CommandSink::new(1, COMMAND_BYTES, fatal, Readiness::new());
        commands.budget.close();
        let input_error = commands
            .input(OwnershipEpoch::FIRST, Command::ReleaseAll(ReleaseAll))
            .await
            .expect_err("closed byte budget should reject a browser input command");
        let resize_error = commands
            .browser_system(Command::ReleaseAll(ReleaseAll))
            .await
            .expect_err("closed byte budget should reject a browser resize command");

        assert_eq!(input_error, CommandSinkError::ByteBudgetClosed);
        assert_eq!(resize_error, CommandSinkError::ByteBudgetClosed);
        assert_eq!(
            commands.readiness.state(),
            ReadinessState::WaitingForFirstFrame
        );
        assert!(
            timeout(Duration::from_millis(20), fatal_events.recv())
                .await
                .is_err()
        );
        drop(request_guard);
    }

    #[tokio::test]
    async fn quality_change_is_queued_as_system_work_before_ownership_release() {
        let (commands, mut command_reader) = command_sink(1);
        let sessions = Sessions::new(
            commands.clone(),
            Kbps::new(8_000).expect("test bitrate should be valid"),
            Fps::new(60).expect("test frame rate should be valid"),
        );
        let socket = SocketId::new(1);
        let feedback = Feedback::new(FeedbackValues {
            received: 50,
            presented: 50,
            queue_peak: 5,
            queue_busy_ms: 200.0,
            sample_ms: 1_000.0,
            dropped: 0,
            rtt: 20.0,
        })
        .expect("test feedback should be valid");

        assert!(matches!(
            sessions.acquire(socket).await.expect("acquire ownership"),
            OwnershipState::Active
        ));
        let initial_release = command_reader
            .recv()
            .await
            .expect("initial lease should queue");
        assert!(matches!(
            Command::decode(&initial_release.encoded),
            Ok(Command::InputLease(InputLease { held: true }))
        ));

        sessions
            .feedback(socket, feedback)
            .await
            .expect("first feedback should be accepted");
        commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await
            .expect("fill command queue");
        let feedback_sessions = sessions.clone();
        let feedback_task =
            tokio::spawn(async move { feedback_sessions.feedback(socket, feedback).await });
        timeout(Duration::from_secs(1), async {
            while sessions.quality().scale.get() == 100 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("quality update should reach the blocked command send");
        let release_sessions = sessions.clone();
        let release_task = tokio::spawn(async move { release_sessions.release(socket).await });
        tokio::task::yield_now().await;

        let filler = command_reader
            .recv()
            .await
            .expect("queue filler should remain first");
        assert!(matches!(
            Command::decode(&filler.encoded),
            Ok(Command::ReleaseAll(_))
        ));
        feedback_task
            .await
            .expect("feedback task should run")
            .expect("quality command should queue");
        let quality = command_reader
            .recv()
            .await
            .expect("quality command should queue");
        let decoded = Command::decode(&quality.encoded).expect("quality command should decode");
        assert_eq!(
            decoded,
            Command::Quality(Quality {
                bitrate_kbps: Kbps::new(8_000).expect("expected bitrate should be valid"),
                fps: Fps::new(60).expect("expected frame rate should be valid"),
                scale_percent: ScalePercent::new(75).expect("expected scale should be valid"),
                crf: waywire_protocol::pipe::Crf::new(27).expect("test CRF should be valid"),
                chroma: waywire_protocol::pipe::Chroma::Yuv420,
            })
        );
        release_task
            .await
            .expect("release task should run")
            .expect("release command should queue");
        let final_release = command_reader
            .recv()
            .await
            .expect("final handback should queue");
        assert!(matches!(
            Command::decode(&final_release.encoded),
            Ok(Command::InputLease(InputLease { held: false }))
        ));
        assert_eq!(
            sessions
                .input(socket, Command::ReleaseAll(ReleaseAll))
                .await
                .expect("released socket input check should succeed"),
            InputOutcome::NotInputOwner
        );
    }
}
