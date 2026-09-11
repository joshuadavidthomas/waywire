use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use axum::serve::ListenerExt;
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
    let sockets_result = timeout_at(deadline, connections.wait())
        .await
        .map_err(|_elapsed| anyhow!("WebSocket shutdown exceeded {SHUTDOWN_DEADLINE:?}"));
    daemon.shutdown().await;

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
        }))
}

#[cfg(test)]
mod tests {
    use axum::serve::Listener;

    use super::http_listener;

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
