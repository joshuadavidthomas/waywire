mod cursor;
mod daemon;
mod http;
mod protocol;
mod session;
mod video;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use axum::serve::ListenerExt;
use clap::Parser;
use socket2::SockRef;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::Kbps;
use tokio::net::TcpListener;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::timeout_at;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;
use url::Url;

use crate::daemon::Config;
use crate::daemon::Daemon;
use crate::http::AppState;
use crate::http::SocketConnections;
use crate::session::Sessions;
use crate::video::VideoHub;

const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(8);

type TaskResult = std::result::Result<Result<()>, JoinError>;

enum Stop {
    Signal(Result<()>),
    Daemon(TaskResult),
    Server(TaskResult),
}

impl Stop {
    const fn reason(&self) -> &'static str {
        match self {
            Self::Signal(_) => "shutdown signal",
            Self::Daemon(_) => "daemon supervisor stopped",
            Self::Server(_) => "HTTP server stopped",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "sprite-desktop-gateway",
    version,
    about = "Private HTTP and WebSocket gateway for sprite-desktop-streamd"
)]
struct Options {
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: String,
    #[arg(long)]
    streamd: String,
    #[arg(long = "public-url", env = "PUBLIC_URL", value_parser = parse_origin)]
    origin: String,
    #[arg(long, default_value = "60", value_parser = parse_fps)]
    frame_rate: Fps,
    #[arg(long, default_value = "16000", value_parser = parse_kbps)]
    bitrate: Kbps,
    #[arg(long, default_value = "us", value_parser = parse_layout)]
    xkb_layout: String,
}

fn parse_origin(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("must be an absolute HTTP(S) URL without credentials".into());
    }
    Ok(url.origin().ascii_serialization())
}

fn parse_fps(value: &str) -> Result<Fps, String> {
    let value = value
        .parse()
        .map_err(|error| format!("frame rate must be an integer: {error}"))?;
    Fps::new(value).map_err(|error| error.to_string())
}

fn parse_kbps(value: &str) -> Result<Kbps, String> {
    let value = value
        .parse()
        .map_err(|error| format!("bitrate must be an integer: {error}"))?;
    Kbps::new(value).map_err(|error| error.to_string())
}

fn parse_layout(value: &str) -> Result<String, String> {
    if !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Ok(value.into())
    } else {
        Err("layout must be 1-32 ASCII letters, digits, '_' or '-'".into())
    }
}

async fn http_listener(
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let options = Options::parse();
    let listener = http_listener(&options.listen).await?;
    let rtp = UdpSocket::bind("127.0.0.1:0")
        .await
        .context("bind loopback RTP before daemon startup")?;
    SockRef::from(&rtp)
        .set_recv_buffer_size(4 << 20)
        .context("set loopback RTP receive buffer to 4 MiB")?;
    let hub = VideoHub::new();
    let started = Daemon::start(
        &Config {
            path: options.streamd,
            frame_rate: options.frame_rate,
            bitrate: options.bitrate,
            xkb_layout: options.xkb_layout,
        },
        rtp,
        hub.clone(),
    )?;
    let daemon = started.daemon;
    let mut daemon_task = started.supervisor;
    let sessions = Sessions::new(daemon.commands.clone(), options.bitrate, options.frame_rate);
    let connections = SocketConnections::new();
    let app = http::router(AppState {
        readiness: daemon.readiness.clone(),
        events: daemon.events.clone(),
        sessions,
        hub,
        origin: options.origin.into(),
        frame_rate: options.frame_rate,
        connections: connections.clone(),
        next_socket_id: Arc::new(AtomicU64::new(1)),
    });
    let (tx, rx) = watch::channel(false);
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut rx = rx;
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
            .map_err(anyhow::Error::from)
    });
    let stop = tokio::select! {
        signal = shutdown_signal() => Stop::Signal(signal),
        result = &mut daemon_task => Stop::Daemon(result),
        result = &mut server => Stop::Server(result),
    };

    info!(reason = stop.reason(), "shutdown beginning");
    let result = finish_shutdown(
        stop,
        connections,
        tx,
        &daemon,
        &mut daemon_task,
        &mut server,
    )
    .await;
    if result.is_ok() {
        info!(outcome = "clean", "shutdown ended");
    } else {
        info!(outcome = "failed", "shutdown ended");
    }
    result
}

async fn finish_shutdown(
    stop: Stop,
    connections: SocketConnections,
    shutdown_server: watch::Sender<bool>,
    daemon: &Daemon,
    daemon_task: &mut JoinHandle<Result<()>>,
    server: &mut JoinHandle<Result<()>>,
) -> Result<()> {
    connections.begin_shutdown();
    let _ = shutdown_server.send(true);
    let deadline = Instant::now() + SHUTDOWN_DEADLINE;
    let sockets_result = timeout_at(deadline, connections.wait())
        .await
        .map_err(|_elapsed| anyhow!("WebSocket shutdown exceeded eight seconds"));
    daemon.shutdown().await;

    let (trigger_result, daemon_result, server_result) = match stop {
        Stop::Signal(signal_result) => {
            let trigger_result = signal_result;
            let daemon_result = join_task(deadline, "daemon supervisor", daemon_task).await;
            let server_result = join_task(deadline, "HTTP server", server).await;
            (trigger_result, daemon_result, server_result)
        }
        Stop::Daemon(task_result) => {
            let trigger_result = Err(anyhow!("daemon supervisor stopped"));
            let daemon_result = completed_task("daemon supervisor", task_result);
            let server_result = join_task(deadline, "HTTP server", server).await;
            (trigger_result, daemon_result, server_result)
        }
        Stop::Server(task_result) => {
            let trigger_result = Err(anyhow!("HTTP server stopped"));
            let daemon_result = join_task(deadline, "daemon supervisor", daemon_task).await;
            let server_result = completed_task("HTTP server", task_result);
            (trigger_result, daemon_result, server_result)
        }
    };

    let results = [
        ("WebSocket connections", sockets_result),
        ("daemon supervisor", daemon_result),
        ("HTTP server", server_result),
        ("shutdown trigger", trigger_result),
    ];
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

async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        value = tokio::signal::ctrl_c() => value.map_err(anyhow::Error::from),
        _ = term.recv() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use axum::serve::Listener;

    use super::*;

    #[test]
    fn canonical_origin_strips_path_and_rejects_foreign_shapes() {
        assert_eq!(
            parse_origin("https://example.test/a").expect("absolute HTTPS test URL should parse"),
            "https://example.test"
        );
        assert!(parse_origin("example.test").is_err());
        assert!(parse_origin("https://u@example.test").is_err());
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
