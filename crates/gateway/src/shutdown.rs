use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use axum::serve::ListenerExt;
use socket2::SockRef;
use tokio::net::TcpListener;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::timeout_at;
use tracing::warn;

use crate::daemon::Daemon;
use crate::http::SocketConnections;

const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(8);

type TaskResult = std::result::Result<Result<()>, JoinError>;

pub(super) enum Stop {
    Signal(Result<()>),
    Daemon(TaskResult),
    Server(TaskResult),
}

impl Stop {
    pub(super) const fn reason(&self) -> &'static str {
        match self {
            Self::Signal(_) => "shutdown signal",
            Self::Daemon(_) => "daemon supervisor stopped",
            Self::Server(_) => "HTTP server stopped",
        }
    }
}

pub(super) async fn finish_shutdown(
    stop: Stop,
    connections: SocketConnections,
    daemon: &Daemon,
    daemon_task: &mut JoinHandle<Result<()>>,
    server: &mut JoinHandle<Result<()>>,
) -> Result<()> {
    connections.begin_shutdown();
    let deadline = Instant::now() + SHUTDOWN_DEADLINE;
    daemon.shutdown().await;
    let sockets_result = timeout_at(deadline, connections.wait())
        .await
        .map_err(|_elapsed| anyhow!("WebSocket shutdown exceeded {SHUTDOWN_DEADLINE:?}"));

    let results = match stop {
        Stop::Signal(signal_result) => {
            let daemon_result = join_task(deadline, "daemon supervisor", daemon_task).await;
            let server_result = join_task(deadline, "HTTP server", server).await;
            [
                ("shutdown trigger", signal_result),
                ("WebSocket connections", sockets_result),
                ("daemon supervisor", daemon_result),
                ("HTTP server", server_result),
            ]
        }
        Stop::Daemon(task_result) => {
            let daemon_result = completed_task("daemon supervisor", task_result);
            let server_result = join_task(deadline, "HTTP server", server).await;
            [
                ("daemon supervisor", daemon_result),
                ("WebSocket connections", sockets_result),
                ("HTTP server", server_result),
                ("shutdown trigger", Ok(())),
            ]
        }
        Stop::Server(task_result) => {
            let daemon_result = join_task(deadline, "daemon supervisor", daemon_task).await;
            let server_result = completed_task("HTTP server", task_result);
            [
                ("HTTP server", server_result),
                ("WebSocket connections", sockets_result),
                ("daemon supervisor", daemon_result),
                ("shutdown trigger", Ok(())),
            ]
        }
    };
    let mut first_failure = None;
    for (component, result) in results {
        if let Err(error) = result {
            warn!(component, reason = %format_args!("{error:#}"), "shutdown component failed");
            if first_failure.is_none() {
                first_failure = Some((component, error));
            }
        }
    }

    match first_failure {
        Some((component, error)) => {
            Err(error.context(format!("{component} did not shut down cleanly")))
        }
        None => Ok(()),
    }
}

fn completed_task(name: &str, result: TaskResult) -> Result<()> {
    result.with_context(|| format!("join {name}"))?
}

async fn join_task(deadline: Instant, name: &str, task: &mut JoinHandle<Result<()>>) -> Result<()> {
    timeout_at(deadline, task)
        .await
        .map_err(|_elapsed| anyhow!("{name} did not stop before the shutdown deadline"))?
        .with_context(|| format!("join {name}"))?
}

pub(super) async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        value = tokio::signal::ctrl_c() => value.map_err(anyhow::Error::from),
        _ = term.recv() => Ok(()),
    }
}

/// Unsent bytes a connection may queue in the kernel. A slow viewer's backlog then
/// stays in the gateway's per-viewer queue, which drops frames the viewer reports as
/// loss, instead of seconds of video hiding in an autotuned send buffer.
const UNSENT_BYTES: u32 = 64 << 10;

pub(super) async fn http_listener(
    address: &str,
) -> Result<impl axum::serve::Listener<Io = tokio::net::TcpStream, Addr = std::net::SocketAddr>> {
    Ok(TcpListener::bind(address)
        .await
        .with_context(|| format!("bind HTTP listener {address}"))?
        .tap_io(|stream| {
            if let Err(error) = stream.set_nodelay(true) {
                warn!(%error, "failed to set TCP_NODELAY on accepted HTTP connection");
            }
            if let Err(error) = SockRef::from(&*stream).set_tcp_notsent_lowat(UNSENT_BYTES) {
                warn!(%error, "failed to set TCP_NOTSENT_LOWAT on accepted HTTP connection");
            }
        }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use axum::serve::Listener;
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    use tokio::time::sleep;
    use tokio::time::timeout;
    use waywire_protocol::pipe::Fps;
    use waywire_protocol::pipe::FrameSize;
    use waywire_protocol::pipe::Kbps;

    use super::Stop;
    use super::finish_shutdown;
    use super::http_listener;
    use crate::daemon::Config;
    use crate::daemon::Daemon;
    use crate::daemon::XkbLayout;
    use crate::http::SocketConnections;
    use crate::video::VideoHub;

    #[tokio::test]
    async fn slow_socket_drain_does_not_delay_process_group_cleanup() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("waywire-shutdown-{unique}"));
        fs::create_dir(&directory).expect("test directory");
        let script = directory.join("compositor");
        let pid_file = directory.join("pid");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ntrap '' TERM\necho $$ > '{}'\nexec sleep 60\n",
                pid_file.display()
            ),
        )
        .expect("test compositor script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("executable script");
        let fps = Fps::new(60).expect("fps");
        let rtp = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("RTP socket");
        let started = Daemon::start(
            &Config {
                path: script,
                frame_rate: fps,
                bitrate: Kbps::new(8_000).expect("bitrate"),
                xkb_layout: XkbLayout::parse("us").expect("layout"),
                resolution: FrameSize::new(800, 600).expect("resolution"),
                pins: vec![],
                session: vec![],
            },
            rtp,
            VideoHub::new(fps),
        )
        .expect("start daemon");
        timeout(Duration::from_secs(2), async {
            while !pid_file.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("daemon started");
        let pid = Pid::from_raw(
            fs::read_to_string(&pid_file)
                .expect("pid file")
                .trim()
                .parse()
                .expect("pid"),
        );
        let connections = SocketConnections::new();
        let held_socket = connections.hold_for_test();
        let shutdown = tokio::spawn(async move {
            let mut daemon_task = started.supervisor;
            let mut server = tokio::spawn(async { Ok(()) });
            finish_shutdown(
                Stop::Signal(Ok(())),
                connections,
                &started.daemon,
                &mut daemon_task,
                &mut server,
            )
            .await
        });
        let cleaned = timeout(Duration::from_secs(5), async {
            loop {
                if matches!(kill(pid, None), Err(Errno::ESRCH)) {
                    break;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        drop(held_socket);
        let outcome = shutdown.await.expect("shutdown task");
        fs::remove_dir_all(directory).expect("remove test directory");
        assert!(
            cleaned.is_ok(),
            "process group cleanup waited for the socket drain"
        );
        outcome.expect("shutdown succeeds");
    }

    #[tokio::test]
    async fn production_http_listener_sets_nodelay() {
        let mut listener = http_listener("127.0.0.1:0")
            .await
            .expect("test HTTP listener should bind to a loopback port");
        let address = listener
            .local_addr()
            .expect("bound test HTTP listener should have a local address");
        let client = tokio::spawn(tokio::net::TcpStream::connect(address));
        let (accepted, _) = listener.accept().await;
        client
            .await
            .expect("test TCP client task should finish")
            .expect("test TCP client should connect");
        assert!(
            accepted
                .nodelay()
                .expect("accepted test connection should report TCP_NODELAY")
        );
    }
}
