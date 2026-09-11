mod commands;
mod events;
mod process;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use nix::unistd::Pid;
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::info;
use tracing::warn;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::Kbps;

use self::commands::CommandReader;
pub(crate) use self::commands::CommandSink;
use self::commands::CommandSinkError;
#[cfg(test)]
pub(crate) use self::commands::TestCommandReceiver;
#[cfg(test)]
pub(crate) use self::commands::test_command_sink;
use self::commands::write_commands;
pub(crate) use self::events::AppEvents;
use self::events::read_events;
use self::process::cleanup_group;
use self::process::spawn_daemon;
use self::process::wait_for_leader_exit;
use crate::video::Readiness;
use crate::video::VideoHub;
use crate::video::VideoPipeline;
use crate::video::VideoWorker;

const STARTUP_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub(crate) struct XkbLayout(String);

impl XkbLayout {
    pub(crate) fn parse(value: &str) -> std::result::Result<Self, XkbLayoutError> {
        if value.is_empty() {
            return Err(XkbLayoutError::Empty);
        }
        if value.len() > 32 {
            return Err(XkbLayoutError::TooLong);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(XkbLayoutError::InvalidCharacter);
        }
        Ok(Self(value.into()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub(crate) enum XkbLayoutError {
    #[error("layout must not be empty")]
    Empty,
    #[error("layout must be at most 32 bytes")]
    TooLong,
    #[error("layout may contain only ASCII letters, digits, '_' or '-'")]
    InvalidCharacter,
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
    pub(crate) path: PathBuf,
    pub(crate) frame_rate: Fps,
    pub(crate) bitrate: Kbps,
    pub(crate) xkb_layout: XkbLayout,
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
        let (commands, command_reader) = CommandSink::new(
            commands::COMMAND_COUNT,
            commands::COMMAND_BYTES,
            fatal,
            readiness.clone(),
        );
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
    readiness.stop();
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
