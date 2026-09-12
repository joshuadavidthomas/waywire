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
use tokio::sync::mpsc;
use tokio::time::timeout;
use waywire_protocol::Record;
use waywire_protocol::pipe::Command;

use crate::session::OwnershipEpoch;
use crate::video::Readiness;

pub(super) const COMMAND_COUNT: usize = 128;
pub(super) const COMMAND_BYTES: usize = 2 * 1024 * 1024;
const PIPE_DEADLINE: Duration = Duration::from_secs(2);

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
}

#[cfg(test)]
pub(crate) fn test_command_sink() -> (CommandSink, TestCommandReceiver) {
    let (fatal, _) = mpsc::channel(1);
    let (sink, receiver) = CommandSink::new(COMMAND_COUNT, COMMAND_BYTES, fatal, Readiness::new());
    (sink, TestCommandReceiver(receiver))
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

    fn fail<T>(&self, error: CommandSinkError) -> Result<T, CommandSinkError> {
        self.readiness.stop();
        // The first fatal error shuts down the session. Later failures need
        // neither additional queue space nor a second shutdown transition.
        let _ = self.fatal.try_send(error.clone());
        Err(error)
    }

    pub(crate) async fn system(&self, command: Command) -> Result<(), CommandSinkError> {
        match self.send(Authority::System, command).await {
            Ok(()) => Ok(()),
            Err(error) => self.fail(error),
        }
    }

    pub(crate) async fn input(
        &self,
        ownership: OwnershipEpoch,
        command: Command,
    ) -> Result<(), CommandSinkError> {
        match self
            .send(Authority::InputOwnership(ownership), command)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => self.fail(error),
        }
    }

    pub(crate) fn set_active_ownership(&self, ownership: OwnershipEpoch) {
        // Release publishes the epoch before its commands enter the FIFO. The
        // reader's Acquire either sees this epoch or rejects the command.
        self.active_ownership
            .store(ownership.get(), Ordering::Release);
    }

    pub(crate) fn clear_active_ownership(&self) {
        // Release invalidates the epoch after ReleaseAll enters the FIFO. A reader that
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
    #[error("daemon command write timed out after {deadline:?}", deadline = PIPE_DEADLINE)]
    WriteTimeout,
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
        let write_result = timeout(PIPE_DEADLINE, output.write_all(&encoded)).await;
        drop(permit);
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(CommandWriterError::Write(error)),
            Err(_elapsed) => return Err(CommandWriterError::WriteTimeout),
        }
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
            .expect("initial release should queue");
        assert!(matches!(
            Command::decode(&initial_release.encoded),
            Ok(Command::ReleaseAll(_))
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
            while sessions.quality_chroma() == waywire_protocol::pipe::Chroma::Yuv444 {
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
                scale_percent: ScalePercent::new(100).expect("expected scale should be valid"),
                crf: waywire_protocol::pipe::Crf::new(23).expect("test CRF should be valid"),
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
            .expect("final release should queue");
        assert!(matches!(
            Command::decode(&final_release.encoded),
            Ok(Command::ReleaseAll(_))
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
