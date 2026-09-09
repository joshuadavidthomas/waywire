mod cursor;
mod daemon;
mod http;
mod protocol;
mod session;
mod video;

use crate::{
    daemon::{AppEvents, Config, Daemon},
    http::{AppState, SocketConnections},
    session::Sessions,
    video::VideoHub,
};
use anyhow::{Context, Result, anyhow};
use axum::serve::ListenerExt;
use clap::Parser;
use socket2::SockRef;
use std::time::Duration;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::watch,
    task::{JoinError, JoinHandle},
    time::{Instant, timeout_at},
};
use url::Url;

const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(8);

type TaskResult = std::result::Result<Result<()>, JoinError>;
enum Stop {
    Signal(Result<()>),
    Daemon(TaskResult),
    Server(TaskResult),
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
    #[arg(long,env="PUBLIC_URL",value_parser=parse_origin)]
    public_url: String,
    #[arg(long,default_value_t=60,value_parser=clap::value_parser!(u32).range(10..=120))]
    frame_rate: u32,
    #[arg(long,default_value_t=16_000,value_parser=clap::value_parser!(u32).range(300..=50_000))]
    bitrate: u32,
    #[arg(long,default_value="us",value_parser=parse_layout)]
    xkb_layout: String,
}
fn parse_origin(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("must be an absolute HTTP(S) URL without credentials".into());
    }
    Ok(url.origin().ascii_serialization())
}
fn parse_layout(value: &str) -> Result<String, String> {
    if !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'_' || v == b'-')
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
                eprintln!("failed to set TCP_NODELAY on accepted HTTP connection: {error}");
            }
        }))
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    let listener = http_listener(&options.listen).await?;
    let rtp = UdpSocket::bind("127.0.0.1:0")
        .await
        .context("bind loopback RTP before daemon startup")?;
    SockRef::from(&rtp)
        .set_recv_buffer_size(4 << 20)
        .context("set loopback RTP receive buffer to 4 MiB")?;
    let hub = VideoHub::new();
    let events = AppEvents::new();
    let (daemon, mut daemon_task) = Daemon::start(
        Config {
            path: options.streamd,
            frame_rate: options.frame_rate,
            bitrate: options.bitrate,
            xkb_layout: options.xkb_layout,
        },
        rtp,
        events,
        hub.clone(),
    )
    .await?;
    let sessions = Sessions::new(
        daemon.state.clone(),
        daemon.commands.clone(),
        options.bitrate,
        options.frame_rate,
    );
    let connections = SocketConnections::new();
    let app = http::router(AppState::new(
        daemon.state.clone(),
        sessions,
        hub,
        options.public_url,
        options.frame_rate,
        connections.clone(),
    ));
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

    finish_shutdown(
        stop,
        connections,
        tx,
        &daemon,
        &mut daemon_task,
        &mut server,
    )
    .await
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
        .map_err(|_| anyhow!("WebSocket shutdown exceeded eight seconds"));
    daemon.shutdown().await;

    match stop {
        Stop::Signal(signal) => {
            let daemon_result = join_task(deadline, "daemon supervisor", daemon_task).await;
            let server_result = join_task(deadline, "HTTP server", server).await;
            signal
                .and(sockets_result)
                .and(daemon_result)
                .and(server_result)
        }
        Stop::Daemon(daemon_result) => {
            let server_result = join_task(deadline, "HTTP server", server).await;
            sockets_result?;
            server_result?;
            daemon_result??;
            Err(anyhow!("daemon supervisor stopped"))
        }
        Stop::Server(server_result) => {
            let daemon_result = join_task(deadline, "daemon supervisor", daemon_task).await;
            sockets_result?;
            daemon_result?;
            server_result??;
            Err(anyhow!("HTTP server stopped"))
        }
    }
}

async fn join_task(deadline: Instant, name: &str, task: &mut JoinHandle<Result<()>>) -> Result<()> {
    timeout_at(deadline, task)
        .await
        .map_err(|_| anyhow!("{name} did not stop before the shutdown deadline"))?
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
    use super::*;
    use axum::serve::Listener;

    #[test]
    fn desktop_defaults_use_sixty_fps_and_sixteen_megabit_video() {
        let options = Options::try_parse_from([
            "sprite-desktop-gateway",
            "--streamd",
            "sprite-desktop-streamd",
            "--public-url",
            "https://example.test",
        ])
        .unwrap();
        assert_eq!(options.frame_rate, 60);
        assert_eq!(options.bitrate, 16_000);
    }

    #[test]
    fn canonical_origin_strips_path_and_rejects_foreign_shapes() {
        assert_eq!(
            parse_origin("https://example.test/a").unwrap(),
            "https://example.test"
        );
        assert!(parse_origin("example.test").is_err());
        assert!(parse_origin("https://u@example.test").is_err());
    }

    #[tokio::test]
    async fn production_http_listener_sets_nodelay() {
        let mut listener = http_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(tokio::net::TcpStream::connect(address));
        let (accepted, _) = listener.accept().await;
        client.await.unwrap().unwrap();
        assert!(accepted.nodelay().unwrap());
    }
}
