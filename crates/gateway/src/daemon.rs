use std::future::pending;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::sys::signal::kill;
use nix::sys::wait::Id;
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitid;
use nix::unistd::Pid;
use sprite_desktop_protocol::Record;
use sprite_desktop_protocol::browser::ClientEvent;
use sprite_desktop_protocol::browser::CursorState;
use sprite_desktop_protocol::pipe::ClipboardText;
use sprite_desktop_protocol::pipe::Command;
use sprite_desktop_protocol::pipe::CursorPosition;
use sprite_desktop_protocol::pipe::Event;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::Kbps;
use thiserror::Error;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command as ProcessCommand;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::sleep_until;
use tokio::time::timeout;
use tracing::info;
use tracing::warn;

use crate::protocol::EventReader;
use crate::session::LeaseEpoch;
use crate::video::VideoHub;
use crate::video::VideoPipeline;
use crate::video::VideoWorker;

const COMMAND_COUNT: usize = 128;
const COMMAND_BYTES: usize = 2 * 1024 * 1024;
const PIPE_DEADLINE: Duration = Duration::from_secs(2);
const STARTUP_DEADLINE: Duration = Duration::from_secs(15);
const GROUP_EXIT_DEADLINE: Duration = Duration::from_secs(2);
const CURSOR_POSITION_PERIOD: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorPositionGate {
    Open,
    Quiet {
        until: Instant,
        pending: Option<CursorPosition>,
    },
}

impl CursorPositionGate {
    fn push(&mut self, position: CursorPosition, now: Instant) -> Option<CursorPosition> {
        match self {
            Self::Open => {
                *self = Self::Quiet {
                    until: now + CURSOR_POSITION_PERIOD,
                    pending: None,
                };
                Some(position)
            }
            Self::Quiet { pending, .. } => {
                *pending = Some(position);
                None
            }
        }
    }

    const fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Open => None,
            Self::Quiet { until, .. } => Some(*until),
        }
    }

    fn flush(&mut self, now: Instant) -> Option<CursorPosition> {
        match std::mem::replace(self, Self::Open) {
            Self::Open | Self::Quiet { pending: None, .. } => None,
            Self::Quiet {
                pending: Some(position),
                ..
            } => {
                *self = Self::Quiet {
                    until: now + CURSOR_POSITION_PERIOD,
                    pending: None,
                };
                Some(position)
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct Readiness {
    state: Arc<Mutex<ReadinessState>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadinessState {
    WaitingForFirstFrame,
    Ready,
    WaitingForKeyframe,
    Failed,
}

pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>, purpose: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(value) => value,
        Err(error) => panic!("{purpose} mutex poisoned: {error}"),
    }
}

impl Readiness {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ReadinessState::WaitingForFirstFrame)),
        }
    }

    pub(crate) fn mark_video_ready(&self) {
        let mut state = lock(&self.state, "daemon readiness");
        if *state != ReadinessState::Failed {
            *state = ReadinessState::Ready;
        }
    }

    pub(crate) fn await_keyframe(&self) {
        let mut state = lock(&self.state, "daemon readiness");
        if *state == ReadinessState::Ready {
            *state = ReadinessState::WaitingForKeyframe;
        }
    }

    pub(crate) fn failed(&self) {
        *lock(&self.state, "daemon readiness") = ReadinessState::Failed;
    }

    pub(crate) fn is_ready(&self) -> bool {
        *lock(&self.state, "daemon readiness") == ReadinessState::Ready
    }

    pub(crate) fn needs_startup_frame(&self) -> bool {
        *lock(&self.state, "daemon readiness") == ReadinessState::WaitingForFirstFrame
    }
}

#[derive(Clone)]
pub(crate) struct AppEvents {
    tx: broadcast::Sender<ClientEvent>,
    latest_clipboard: Arc<Mutex<Option<ClipboardText>>>,
    latest_cursor: Arc<Mutex<CursorState>>,
}

impl AppEvents {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            tx,
            latest_clipboard: Arc::new(Mutex::new(None)),
            latest_cursor: Arc::new(Mutex::new(CursorState::default())),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.tx.subscribe()
    }

    pub(crate) fn initial(&self) -> Vec<ClientEvent> {
        let cursor = lock(&self.latest_cursor, "daemon cursor state").clone();
        let mut values = vec![ClientEvent::Cursor(cursor)];
        if let Some(text) = lock(&self.latest_clipboard, "daemon clipboard state").clone() {
            values.push(ClientEvent::Clipboard { text });
        }
        values
    }

    fn publish(&self, value: ClientEvent) {
        let _ = self.tx.send(value);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Authority {
    System,
    Lease(LeaseEpoch),
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

struct CommandReader {
    requests: mpsc::Receiver<Request>,
    active_lease: Arc<AtomicU64>,
}

impl CommandReader {
    async fn recv(&mut self) -> Option<AuthorizedCommand> {
        while let Some(request) = self.requests.recv().await {
            let authorized = match request.authority {
                Authority::System => true,
                Authority::Lease(lease) => self.active_lease.load(Ordering::Acquire) == lease.get(),
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

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(crate) enum CommandSinkError {
    #[error("daemon command length {length} exceeds the pipe budget's integer limit")]
    LengthOverflow { length: usize },
    #[error("daemon command byte budget closed")]
    ByteBudgetClosed,
    #[error("daemon command byte budget stalled for two seconds")]
    ByteBudgetStalled,
    #[error("daemon command writer stopped")]
    WriterStopped,
    #[error("daemon command count budget stalled for two seconds")]
    CountBudgetStalled,
}

#[derive(Clone)]
pub(crate) struct CommandSink {
    tx: mpsc::Sender<Request>,
    budget: Arc<Semaphore>,
    fatal: mpsc::Sender<CommandSinkError>,
    readiness: Readiness,
    active_lease: Arc<AtomicU64>,
}

impl CommandSink {
    fn new(
        capacity: usize,
        budget_bytes: usize,
        fatal: mpsc::Sender<CommandSinkError>,
        readiness: Readiness,
    ) -> (Self, CommandReader) {
        let (tx, requests) = mpsc::channel(capacity);
        let active_lease = Arc::new(AtomicU64::new(0));
        (
            Self {
                tx,
                budget: Arc::new(Semaphore::new(budget_bytes)),
                fatal,
                readiness,
                active_lease: Arc::clone(&active_lease),
            },
            CommandReader {
                requests,
                active_lease,
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
        self.readiness.failed();
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
        lease: LeaseEpoch,
        command: Command,
    ) -> Result<(), CommandSinkError> {
        match self.send(Authority::Lease(lease), command).await {
            Ok(()) => Ok(()),
            Err(error) => self.fail(error),
        }
    }

    pub(crate) fn set_active_lease(&self, lease: LeaseEpoch) {
        // Release publishes the epoch before its commands enter the FIFO. The
        // reader's Acquire either sees this epoch or rejects the command.
        self.active_lease.store(lease.get(), Ordering::Release);
    }

    pub(crate) fn clear_active_lease(&self) {
        // Release invalidates the epoch after ReleaseAll enters the FIFO. A
        // reader that observes the clear can only drop late leased commands;
        // system commands remain ordered by the channel.
        self.active_lease.store(0, Ordering::Release);
    }
}

pub(crate) struct Daemon {
    pub(crate) commands: CommandSink,
    pub(crate) readiness: Readiness,
    pub(crate) events: AppEvents,
    shutdown: mpsc::Sender<()>,
}

pub(crate) struct StartedDaemon {
    pub(crate) daemon: Daemon,
    pub(crate) supervisor: tokio::task::JoinHandle<Result<()>>,
}

pub(crate) struct Config {
    pub(crate) path: String,
    pub(crate) frame_rate: Fps,
    pub(crate) bitrate: Kbps,
    pub(crate) xkb_layout: String,
}

impl Daemon {
    pub(crate) fn start(
        config: &Config,
        socket: UdpSocket,
        hub: VideoHub,
    ) -> Result<StartedDaemon> {
        let readiness = Readiness::new();
        let events = AppEvents::new();
        let (fatal, fatal_rx) = mpsc::channel(1);
        let (commands, command_reader) =
            CommandSink::new(COMMAND_COUNT, COMMAND_BYTES, fatal, readiness.clone());
        let (shutdown, shutdown_rx) = mpsc::channel(1);
        let (pipeline, video_worker) = VideoPipeline::new(hub, commands.clone(), readiness.clone());
        let process = spawn_daemon(config, socket.local_addr()?.port())?;
        let receivers = DaemonReceivers {
            commands: command_reader,
            fatal: fatal_rx,
            shutdown: shutdown_rx,
        };
        let supervisor = tokio::spawn(supervise_daemon(
            process,
            socket,
            pipeline,
            video_worker,
            readiness.clone(),
            events.clone(),
            receivers,
        ));
        Ok(StartedDaemon {
            daemon: Self {
                commands,
                readiness,
                events,
                shutdown,
            },
            supervisor,
        })
    }

    pub(crate) async fn shutdown(&self) {
        let _ = self.shutdown.send(()).await;
    }
}

struct SpawnedDaemon {
    child: Child,
    pid: Pid,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

struct DaemonReceivers {
    commands: CommandReader,
    fatal: mpsc::Receiver<CommandSinkError>,
    shutdown: mpsc::Receiver<()>,
}

fn spawn_daemon(config: &Config, rtp_port: u16) -> Result<SpawnedDaemon> {
    let mut command = ProcessCommand::new(&config.path);
    command
        .args([
            "--frame-rate",
            &config.frame_rate.get().to_string(),
            "--bitrate",
            &config.bitrate.get().to_string(),
            "--rtp-port",
            &rtp_port.to_string(),
            "--xkb-layout",
            &config.xkb_layout,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn private daemon {}", config.path))?;
    let raw_pid = i32::try_from(
        child
            .id()
            .ok_or_else(|| anyhow!("daemon has no process ID"))?,
    )
    .context("daemon process ID exceeds i32")?;
    let pid = Pid::from_raw(raw_pid);
    let stdin = child.stdin.take().context("open daemon stdin")?;
    let stdout = child.stdout.take().context("open daemon stdout")?;
    info!(%pid, path = %config.path, "daemon spawned");
    Ok(SpawnedDaemon {
        child,
        pid,
        stdin,
        stdout,
    })
}

async fn supervise_daemon(
    process: SpawnedDaemon,
    socket: UdpSocket,
    pipeline: VideoPipeline,
    video_worker: VideoWorker,
    readiness: Readiness,
    events: AppEvents,
    receivers: DaemonReceivers,
) -> Result<()> {
    let SpawnedDaemon {
        mut child,
        pid,
        stdin,
        stdout,
    } = process;
    let DaemonReceivers {
        commands: command_reader,
        fatal: mut fatal_rx,
        shutdown: mut shutdown_rx,
    } = receivers;
    // Drop the pipe futures before signalling the process group so native EOF
    // cleanup can make progress.
    let result = async {
        let writer = write_commands(stdin, command_reader);
        let reader = read_events(stdout, events, pipeline.clone());
        let rtp = pipeline.receive(socket);
        let video = video_worker.run();
        let exited = wait_for_leader_exit(pid);
        let startup = async {
            sleep(STARTUP_DEADLINE).await;
            if readiness.needs_startup_frame() {
                Err(anyhow!(
                    "daemon did not produce a correlated keyframe before startup deadline"
                ))
            } else {
                std::future::pending::<Result<()>>().await
            }
        };
        tokio::pin!(writer, reader, rtp, video, exited, startup);
        tokio::select! {
            value = &mut writer => value.context("daemon command writer"),
            value = &mut reader => value.context("daemon event reader"),
            value = &mut rtp => value.context("RTP receiver"),
            value = &mut video => value.context("video pipeline"),
            value = &mut exited => {
                let status = value?;
                Err(anyhow!("daemon exited with status {status:?}"))
            }
            value = &mut startup => value,
            error = fatal_rx.recv() => match error {
                Some(error) => Err(error.into()),
                None => Err(anyhow!("fatal command channel closed")),
            },
            _ = shutdown_rx.recv() => Ok(()),
        }
    }
    .await;
    readiness.failed();
    let result = match cleanup_group(&mut child, pid).await {
        Ok(()) => result,
        Err(cleanup_error) => match result {
            Ok(()) => Err(cleanup_error.context("clean up daemon process group")),
            Err(terminal) => Err(terminal.context(format!(
                "daemon process group cleanup also failed: {cleanup_error:#}"
            ))),
        },
    };
    match &result {
        Ok(()) => info!(%pid, reason = "shutdown requested", "daemon supervisor stopped"),
        Err(error) => {
            warn!(%pid, reason = %format_args!("{error:#}"), "daemon supervisor stopped");
        }
    }
    result
}

#[derive(Debug, Error)]
enum CommandWriterError {
    #[error("command queue closed")]
    QueueClosed,
    #[error("write daemon command: {0}")]
    Write(#[source] std::io::Error),
    #[error("daemon command write timed out after two seconds")]
    WriteTimeout,
}

async fn write_commands<W>(
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

async fn read_events(
    stdout: tokio::process::ChildStdout,
    events: AppEvents,
    pipeline: VideoPipeline,
) -> Result<()> {
    let mut reader = EventReader::new(stdout);
    let mut position_gate = CursorPositionGate::Open;
    loop {
        let position_deadline = position_gate.deadline();
        tokio::select! {
            event = reader.next() => {
                let Some(event) = event? else {
                    return Err(anyhow!("daemon event pipe closed"));
                };
                match event {
                    Event::Frame(metadata) => pipeline.metadata(metadata).await?,
                    Event::Clipboard(text) => {
                        *lock(&events.latest_clipboard, "daemon clipboard state") = Some(text.clone());
                        events.publish(ClientEvent::Clipboard { text });
                    }
                    Event::ResizeApplied(applied) => {
                        events.publish(ClientEvent::ResizeApplied(applied));
                    }
                    Event::CursorShape(shape) => {
                        publish_cursor(&events, |cursor| cursor.shape = shape);
                    }
                    Event::CursorVisibility(visibility) => {
                        publish_cursor(&events, |cursor| cursor.visibility = visibility);
                    }
                    Event::CursorPosition(position) => {
                        if let Some(position) = position_gate.push(position, Instant::now()) {
                            publish_cursor(&events, |cursor| cursor.position = Some(position));
                        }
                    }
                }
            }
            () = async move {
                match position_deadline {
                    Some(until) => sleep_until(until).await,
                    None => pending::<()>().await,
                }
            } => {
                if let Some(position) = position_gate.flush(Instant::now()) {
                    publish_cursor(&events, |cursor| cursor.position = Some(position));
                }
            }
        }
    }
}

fn publish_cursor<F>(events: &AppEvents, update: F)
where
    F: FnOnce(&mut CursorState),
{
    let event = {
        let mut cursor = lock(&events.latest_cursor, "daemon cursor state");
        update(&mut cursor);
        ClientEvent::Cursor(cursor.clone())
    };
    events.publish(event);
}

async fn wait_for_leader_exit(pid: Pid) -> Result<WaitStatus> {
    loop {
        match waitid(
            Id::Pid(pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG,
        )? {
            WaitStatus::StillAlive => sleep(Duration::from_millis(25)).await,
            status @ (WaitStatus::Exited(..)
            | WaitStatus::Signaled(..)
            | WaitStatus::Stopped(..)
            | WaitStatus::PtraceEvent(..)
            | WaitStatus::PtraceSyscall(_)
            | WaitStatus::Continued(_)) => return Ok(status),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessGroupState {
    Alive,
    Gone,
}

fn process_group_state(result: std::result::Result<(), Errno>) -> Result<ProcessGroupState> {
    match result {
        Ok(()) => Ok(ProcessGroupState::Alive),
        Err(Errno::ESRCH) => Ok(ProcessGroupState::Gone),
        Err(error) => Err(error.into()),
    }
}

async fn cleanup_group(child: &mut Child, pid: Pid) -> Result<()> {
    let group = Pid::from_raw(-pid.as_raw());
    let state = process_group_state(kill(group, Signal::SIGTERM))
        .context("send SIGTERM to daemon process group")?;
    if state == ProcessGroupState::Alive {
        let deadline = Instant::now() + GROUP_EXIT_DEADLINE;
        loop {
            match process_group_state(kill(group, None))
                .context("probe daemon process group after SIGTERM")?
            {
                ProcessGroupState::Gone => break,
                ProcessGroupState::Alive if Instant::now() < deadline => {
                    sleep(Duration::from_millis(25)).await;
                }
                ProcessGroupState::Alive => {
                    let _ = process_group_state(kill(group, Signal::SIGKILL))
                        .context("send SIGKILL to daemon process group")?;
                    break;
                }
            }
        }
    }
    timeout(GROUP_EXIT_DEADLINE, child.wait())
        .await
        .context("daemon leader could not be reaped")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sprite_desktop_protocol::browser::Feedback;
    use sprite_desktop_protocol::browser::FeedbackValues;
    use sprite_desktop_protocol::pipe::Generation;
    use sprite_desktop_protocol::pipe::KeyframeReadiness;
    use sprite_desktop_protocol::pipe::KeyframeState;
    use sprite_desktop_protocol::pipe::Quality;
    use sprite_desktop_protocol::pipe::ReleaseAll;
    use sprite_desktop_protocol::pipe::ScalePercent;
    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::session::LeaseState;
    use crate::session::Sessions;
    use crate::session::SocketId;

    fn command_sink(capacity: usize) -> (CommandSink, CommandReader) {
        let (fatal, _) = mpsc::channel(1);
        CommandSink::new(capacity, COMMAND_BYTES, fatal, Readiness::new())
    }

    #[test]
    fn cursor_position_gate_publishes_first_and_latest_without_idle_deadline() {
        let now = Instant::now();
        let first = CursorPosition { x: 1, y: 2 };
        let replaced = CursorPosition { x: 3, y: 4 };
        let latest = CursorPosition { x: 5, y: 6 };
        let mut gate = CursorPositionGate::Open;

        assert_eq!(gate.push(first, now), Some(first));
        assert_eq!(gate.deadline(), Some(now + CURSOR_POSITION_PERIOD));
        assert_eq!(gate.push(replaced, now), None);
        assert_eq!(gate.push(latest, now), None);
        let first_deadline = gate.deadline().expect("quiet gate should have a deadline");
        assert_eq!(gate.flush(first_deadline), Some(latest));
        assert_eq!(
            gate.deadline(),
            Some(first_deadline + CURSOR_POSITION_PERIOD)
        );
        let final_deadline = gate
            .deadline()
            .expect("rearmed gate should have a deadline");
        assert_eq!(gate.flush(final_deadline), None);
        assert_eq!(gate.deadline(), None);
    }

    #[tokio::test]
    async fn lease_transitions_keep_fifo_order_and_reject_stale_input() {
        let (commands, command_reader) = command_sink(8);
        let (mut output, daemon_input) = tokio::io::duplex(1024);
        let writer = tokio::spawn(write_commands(daemon_input, command_reader));
        let first = LeaseEpoch::FIRST;
        let second = first.next();

        commands.set_active_lease(first);
        let first_input = Command::KeyframeReadiness(KeyframeReadiness {
            generation: Generation::new(1).expect("test generation should be valid"),
            state: KeyframeState::Cached,
        });
        let first_bytes = first_input.encode();
        commands
            .input(first, first_input)
            .await
            .expect("first lease input should queue");
        let mut written = vec![0; first_bytes.len()];
        output
            .read_exact(&mut written)
            .await
            .expect("active lease input should be written");
        assert_eq!(written, first_bytes);

        commands.set_active_lease(second);
        commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await
            .expect("lease transition reset should queue");
        commands
            .input(
                first,
                Command::KeyframeReadiness(KeyframeReadiness {
                    generation: Generation::new(2).expect("test generation should be valid"),
                    state: KeyframeState::Missing,
                }),
            )
            .await
            .expect("stale lease input should enter the queue before filtering");
        let second_input = Command::Quality(Quality {
            bitrate_kbps: Kbps::new(8_000).expect("test bitrate should be valid"),
            fps: Fps::new(60).expect("test frame rate should be valid"),
            scale_percent: ScalePercent::new(100).expect("test scale should be valid"),
        });
        let mut expected = Command::ReleaseAll(ReleaseAll).encode();
        expected.extend_from_slice(&second_input.encode());
        commands
            .input(second, second_input)
            .await
            .expect("second lease input should queue");
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
    async fn quality_change_is_queued_as_system_work_before_lease_release() {
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
            sessions.acquire(socket).await.expect("acquire lease"),
            LeaseState::Active
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
            while sessions.quality().bitrate.get() == 8_000 {
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
        let Command::Quality(payload) = decoded else {
            return;
        };
        assert_eq!(payload.bitrate_kbps.get(), 6_400);
        assert_eq!(payload.fps.get(), 60);
        assert_eq!(payload.scale_percent.get(), 100);

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
        assert!(!sessions.owns(socket).await);
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
        assert!(!commands.readiness.is_ready());
        assert!(!commands.readiness.needs_startup_frame());
        drop(request_guard);
    }

    #[test]
    fn only_esrch_means_a_process_group_is_gone() {
        assert_eq!(
            process_group_state(Err(Errno::ESRCH)).expect("ESRCH should mean the group is gone"),
            ProcessGroupState::Gone
        );
        assert_eq!(
            process_group_state(Ok(())).expect("a successful probe should mean the group is alive"),
            ProcessGroupState::Alive
        );

        let error = process_group_state(Err(Errno::EPERM))
            .expect_err("EPERM must remain a process-group cleanup error");
        assert_eq!(error.downcast_ref::<Errno>(), Some(&Errno::EPERM));
    }

    #[test]
    fn video_ready_cannot_revive_a_failed_runtime() {
        let readiness = Readiness::new();
        readiness.failed();
        readiness.mark_video_ready();
        assert!(!readiness.is_ready());
        assert!(!readiness.needs_startup_frame());
    }

    #[test]
    fn recovery_wait_does_not_restore_the_startup_deadline() {
        let readiness = Readiness::new();
        readiness.mark_video_ready();
        readiness.await_keyframe();
        assert!(!readiness.is_ready());
        assert!(!readiness.needs_startup_frame());
    }

    #[test]
    fn ready_runtime_stays_healthy_while_video_is_idle() {
        let readiness = Readiness::new();
        readiness.mark_video_ready();
        assert!(readiness.is_ready());
        assert!(!readiness.needs_startup_frame());
    }
}
