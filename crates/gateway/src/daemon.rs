use crate::{
    cursor::CursorState,
    protocol::{DaemonCommand, DaemonEvent, EventReader},
    video::{VideoHub, VideoPipeline, VideoWorker},
};
use anyhow::{Context, Result, anyhow};
use nix::{
    sys::{
        signal::{Signal, kill},
        wait::{Id, WaitPidFlag, WaitStatus, waitid},
    },
    unistd::Pid,
};
use serde_json::json;
use std::{
    os::unix::process::CommandExt,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::UdpSocket,
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc},
    time::{Instant, sleep, timeout},
};

const COMMAND_COUNT: usize = 128;
const COMMAND_BYTES: usize = 2 * 1024 * 1024;
const PIPE_DEADLINE: Duration = Duration::from_secs(2);
const STARTUP_DEADLINE: Duration = Duration::from_secs(15);
const GROUP_EXIT_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct RuntimeState {
    readiness: Readiness,
    pub active_lease: Arc<AtomicU64>,
    pub events: AppEvents,
}
impl RuntimeState {
    pub fn is_ready(&self) -> bool {
        self.readiness.is_ready()
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
impl Readiness {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ReadinessState::WaitingForFirstFrame)),
        }
    }
    pub(crate) fn mark_video_ready(&self) {
        let mut state = self.state.lock().unwrap();
        if *state != ReadinessState::Failed {
            *state = ReadinessState::Ready;
        }
    }
    pub(crate) fn await_keyframe(&self) {
        let mut state = self.state.lock().unwrap();
        if *state == ReadinessState::Ready {
            *state = ReadinessState::WaitingForKeyframe;
        }
    }
    pub(crate) fn failed(&self) {
        *self.state.lock().unwrap() = ReadinessState::Failed;
    }
    pub(crate) fn is_ready(&self) -> bool {
        *self.state.lock().unwrap() == ReadinessState::Ready
    }
    pub(crate) fn needs_startup_frame(&self) -> bool {
        *self.state.lock().unwrap() == ReadinessState::WaitingForFirstFrame
    }
}
#[derive(Clone)]
pub struct AppEvents {
    tx: broadcast::Sender<String>,
    latest_clipboard: Arc<Mutex<Option<String>>>,
    latest_cursor: Arc<Mutex<CursorState>>,
}
impl AppEvents {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            tx,
            latest_clipboard: Arc::new(Mutex::new(None)),
            latest_cursor: Arc::new(Mutex::new(CursorState::default())),
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }
    pub fn initial(&self) -> Vec<String> {
        let mut values = vec![serde_json::to_string(&*self.latest_cursor.lock().unwrap()).unwrap()];
        if let Some(value) = self.latest_clipboard.lock().unwrap().clone() {
            values.push(value);
        }
        values
    }
    fn publish(&self, value: String) {
        let _ = self.tx.send(value);
    }
    fn reset(&self) {
        *self.latest_clipboard.lock().unwrap() = None;
        *self.latest_cursor.lock().unwrap() = CursorState::default();
    }
}
enum Authority {
    System,
    Lease(u64),
}
struct Request {
    authority: Authority,
    command: DaemonCommand,
    _bytes: OwnedSemaphorePermit,
}
#[derive(Clone)]
pub struct CommandSink {
    tx: mpsc::Sender<Request>,
    budget: Arc<Semaphore>,
    fatal: mpsc::Sender<String>,
    readiness: Readiness,
}
impl CommandSink {
    async fn send(&self, authority: Authority, command: DaemonCommand) -> Result<()> {
        let count =
            u32::try_from(command.encoded_len()).map_err(|_| anyhow!("command length overflow"))?;
        let permit = timeout(PIPE_DEADLINE, self.budget.clone().acquire_many_owned(count))
            .await
            .map_err(|_| anyhow!("daemon command byte budget stalled"))?
            .map_err(|_| anyhow!("daemon command budget closed"))?;
        let request = Request {
            authority,
            command,
            _bytes: permit,
        };
        match timeout(PIPE_DEADLINE, self.tx.send(request)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(anyhow!("daemon command writer stopped")),
            Err(_) => Err(anyhow!("daemon command count budget stalled")),
        }
    }
    fn fail<T>(&self, message: &str) -> Result<T> {
        self.readiness.failed();
        // The first fatal error shuts down the session. Later failures need
        // neither additional queue space nor a second shutdown transition.
        let _ = self.fatal.try_send(message.to_owned());
        Err(anyhow!(message.to_owned()))
    }
    pub async fn system(&self, command: DaemonCommand) -> Result<()> {
        match self.send(Authority::System, command).await {
            Ok(()) => Ok(()),
            Err(error) => self.fail(&error.to_string()),
        }
    }
    pub async fn input(&self, lease: u64, command: DaemonCommand) -> Result<()> {
        match self.send(Authority::Lease(lease), command).await {
            Ok(()) => Ok(()),
            Err(error) => self.fail(&error.to_string()),
        }
    }
}

pub struct Daemon {
    pub commands: CommandSink,
    pub state: RuntimeState,
    shutdown: mpsc::Sender<()>,
}
pub struct Config {
    pub path: String,
    pub frame_rate: u32,
    pub bitrate: u32,
    pub xkb_layout: String,
}
impl Daemon {
    pub async fn start(
        config: Config,
        socket: UdpSocket,
        events: AppEvents,
        hub: VideoHub,
    ) -> Result<(Self, tokio::task::JoinHandle<Result<()>>)> {
        let readiness = Readiness::new();
        let active_lease = Arc::new(AtomicU64::new(0));
        let (command_tx, command_rx) = mpsc::channel(COMMAND_COUNT);
        let (fatal, fatal_rx) = mpsc::channel(1);
        let commands = CommandSink {
            tx: command_tx,
            budget: Arc::new(Semaphore::new(COMMAND_BYTES)),
            fatal,
            readiness: readiness.clone(),
        };
        let (shutdown, shutdown_rx) = mpsc::channel(1);
        let (pipeline, video_worker) = VideoPipeline::new(hub, commands.clone(), readiness.clone());
        let process = spawn_daemon(&config, socket.local_addr()?.port())?;
        events.reset();
        let state = RuntimeState {
            readiness,
            active_lease,
            events,
        };
        let task = tokio::spawn(supervise_daemon(
            process,
            socket,
            pipeline,
            video_worker,
            state.clone(),
            (command_rx, fatal_rx, shutdown_rx),
        ));
        Ok((
            Self {
                commands,
                state,
                shutdown,
            },
            task,
        ))
    }
    pub async fn shutdown(&self) {
        let _ = self.shutdown.send(()).await;
    }
}

fn spawn_daemon(config: &Config, rtp_port: u16) -> Result<(Child, i32, ChildStdin, ChildStdout)> {
    let mut command = Command::new(&config.path);
    command
        .args([
            "--frame-rate",
            &config.frame_rate.to_string(),
            "--bitrate",
            &config.bitrate.to_string(),
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
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("daemon has no process ID"))? as i32;
    let stdin = child.stdin.take().context("open daemon stdin")?;
    let stdout = child.stdout.take().context("open daemon stdout")?;
    Ok((child, pid, stdin, stdout))
}

async fn supervise_daemon(
    process: (Child, i32, ChildStdin, ChildStdout),
    socket: UdpSocket,
    pipeline: VideoPipeline,
    video_worker: VideoWorker,
    state: RuntimeState,
    channels: (
        mpsc::Receiver<Request>,
        mpsc::Receiver<String>,
        mpsc::Receiver<()>,
    ),
) -> Result<()> {
    let (mut child, pid, stdin, stdout) = process;
    let (command_rx, mut fatal_rx, mut shutdown_rx) = channels;
    // Drop the pipe futures before signalling the process group so native EOF
    // cleanup can make progress.
    let result = async {
        let writer = write_commands(stdin, command_rx, state.active_lease.clone());
        let reader = read_events(stdout, state.events.clone(), pipeline.clone());
        let rtp = pipeline.receive(socket);
        let video = video_worker.run();
        let exited = wait_for_leader_exit(pid);
        let startup = async {
            sleep(STARTUP_DEADLINE).await;
            if state.readiness.needs_startup_frame() {
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
                value?;
                Err(anyhow!("daemon exited"))
            }
            value = &mut startup => value,
            message = fatal_rx.recv() => Err(anyhow!(
                message.unwrap_or_else(|| "fatal command channel closed".into())
            )),
            _ = shutdown_rx.recv() => Ok(()),
        }
    }
    .await;
    state.readiness.failed();
    cleanup_group(&mut child, pid).await?;
    result
}

async fn write_commands(
    mut output: tokio::process::ChildStdin,
    mut requests: mpsc::Receiver<Request>,
    active_lease: Arc<AtomicU64>,
) -> Result<()> {
    while let Some(request) = requests.recv().await {
        if let Authority::Lease(expected) = request.authority
            && active_lease.load(Ordering::Acquire) != expected
        {
            continue;
        }
        let bytes = request.command.encode();
        timeout(PIPE_DEADLINE, output.write_all(&bytes))
            .await
            .map_err(|_| anyhow!("daemon stdin stalled"))??;
    }
    Err(anyhow!("command queue closed"))
}
async fn read_events(
    stdout: tokio::process::ChildStdout,
    events: AppEvents,
    pipeline: VideoPipeline,
) -> Result<()> {
    let mut reader = EventReader::new(stdout);
    while let Some(event) = reader.next().await? {
        match event {
            DaemonEvent::Frame(metadata) => pipeline.metadata(metadata).await?,
            DaemonEvent::Clipboard(text) => {
                let value = json!({"type":"clipboard","text":text}).to_string();
                *events.latest_clipboard.lock().unwrap() = Some(value.clone());
                events.publish(value);
            }
            DaemonEvent::ResizeApplied {
                request,
                width,
                height,
                scale,
                generation,
            } => events.publish(
                json!({"type":"resize-applied","request":request,"width":width,"height":height,"scale":scale,"generation":generation})
                    .to_string(),
            ),
            DaemonEvent::CursorVisibility(visible) => {
                let next = events
                    .latest_cursor
                    .lock()
                    .unwrap()
                    .with_visibility(visible);
                *events.latest_cursor.lock().unwrap() = next.clone();
                events.publish(serde_json::to_string(&next)?);
            }
            DaemonEvent::CursorImage {
                width,
                height,
                hotspot_x,
                hotspot_y,
                bgra,
            } => {
                let current = events.latest_cursor.lock().unwrap().clone();
                let next = current
                    .with_image(width, height, hotspot_x, hotspot_y, bgra)
                    .await?;
                *events.latest_cursor.lock().unwrap() = next.clone();
                events.publish(serde_json::to_string(&next)?);
            }
        }
    }
    Err(anyhow!("daemon event pipe closed"))
}
async fn wait_for_leader_exit(pid: i32) -> Result<()> {
    let pid = Pid::from_raw(pid);
    loop {
        match waitid(
            Id::Pid(pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG,
        )? {
            WaitStatus::StillAlive => sleep(Duration::from_millis(25)).await,
            _ => return Ok(()),
        }
    }
}
async fn cleanup_group(child: &mut Child, pid: i32) -> Result<()> {
    let group = Pid::from_raw(-pid);
    let _ = kill(group, Signal::SIGTERM);
    let deadline = Instant::now() + GROUP_EXIT_DEADLINE;
    loop {
        if kill(group, None).is_err() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = kill(group, Signal::SIGKILL);
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    let _ = timeout(GROUP_EXIT_DEADLINE, child.wait())
        .await
        .map_err(|_| anyhow!("daemon leader could not be reaped"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
