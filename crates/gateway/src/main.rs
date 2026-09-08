mod cursor;
mod daemon;
mod gateway_timings;
mod http;
mod protocol;
mod session;
mod video;

use crate::{
    daemon::{AppEvents, Config, Daemon},
    gateway_timings::GatewayTimings,
    http::{AppState, SocketConnections},
    session::Sessions,
    video::VideoHub,
};
use anyhow::{Context, Result, anyhow};
use axum::serve::ListenerExt;
use clap::Parser;
use socket2::SockRef;
use std::{path::PathBuf, time::Duration};
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
    Timings(TaskResult),
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
    #[arg(long, env = "SPRITE_DESKTOP_GATEWAY_STAGE_TIMINGS")]
    gateway_stage_timings: Option<PathBuf>,
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
    let timings = GatewayTimings::open(options.gateway_stage_timings.as_deref())
        .context("open gateway stage timing file")?;
    // Register these handlers before the listener can report service readiness.
    let timing_signals = timing_signals(timings.is_some())?;
    let listener = http_listener(&options.listen).await?;
    let rtp = UdpSocket::bind("127.0.0.1:0")
        .await
        .context("bind loopback RTP before daemon startup")?;
    SockRef::from(&rtp)
        .set_recv_buffer_size(4 << 20)
        .context("set loopback RTP receive buffer to 4 MiB")?;
    let hub = VideoHub::new(timings.clone());
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
        timings.clone(),
        connections.clone(),
    ));
    let (timing_shutdown, timing_shutdown_rx) = watch::channel(false);
    let mut timing_task = tokio::spawn(run_timing_signals(
        timings.clone(),
        timing_signals,
        timing_shutdown_rx,
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
        result = &mut timing_task => Stop::Timings(result),
    };

    connections.begin_shutdown();
    let _ = tx.send(true);
    let deadline = Instant::now() + SHUTDOWN_DEADLINE;
    let sockets_result = timeout_at(deadline, connections.wait())
        .await
        .map_err(|_| anyhow!("WebSocket shutdown exceeded eight seconds"));
    daemon.shutdown().await;
    let _ = timing_shutdown.send(true);

    match stop {
        Stop::Signal(signal) => {
            let daemon_result = join_task(deadline, "daemon supervisor", &mut daemon_task).await;
            let server_result = join_task(deadline, "HTTP server", &mut server).await;
            let timing_result =
                join_task(deadline, "gateway timing signal worker", &mut timing_task).await;
            signal
                .and(sockets_result)
                .and(daemon_result)
                .and(server_result)
                .and(timing_result)
        }
        Stop::Daemon(daemon_result) => {
            let server_result = join_task(deadline, "HTTP server", &mut server).await;
            let timing_result =
                join_task(deadline, "gateway timing signal worker", &mut timing_task).await;
            sockets_result?;
            server_result?;
            timing_result?;
            daemon_result??;
            Err(anyhow!("daemon supervisor stopped"))
        }
        Stop::Server(server_result) => {
            let daemon_result = join_task(deadline, "daemon supervisor", &mut daemon_task).await;
            let timing_result =
                join_task(deadline, "gateway timing signal worker", &mut timing_task).await;
            sockets_result?;
            daemon_result?;
            timing_result?;
            server_result??;
            Err(anyhow!("HTTP server stopped"))
        }
        Stop::Timings(timing_result) => {
            let daemon_result = join_task(deadline, "daemon supervisor", &mut daemon_task).await;
            let server_result = join_task(deadline, "HTTP server", &mut server).await;
            sockets_result?;
            daemon_result?;
            server_result?;
            timing_result.context("join gateway timing signal worker")??;
            Err(anyhow!("gateway timing signal worker stopped"))
        }
    }
}

async fn join_task(deadline: Instant, name: &str, task: &mut JoinHandle<Result<()>>) -> Result<()> {
    timeout_at(deadline, task)
        .await
        .map_err(|_| anyhow!("{name} did not stop before the shutdown deadline"))?
        .with_context(|| format!("join {name}"))?
}

type TimingSignals = (tokio::signal::unix::Signal, tokio::signal::unix::Signal);

fn timing_signals(enabled: bool) -> Result<Option<TimingSignals>> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some((
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?,
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2())?,
    )))
}

async fn run_timing_signals(
    timings: Option<GatewayTimings>,
    signals: Option<TimingSignals>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    if let (Some(timings), Some((mut start, mut stop))) = (timings, signals) {
        loop {
            tokio::select! {
                value = start.recv() => {
                    value.context("gateway timing start signal stream closed")?;
                    timings.start();
                }
                value = stop.recv() => {
                    value.context("gateway timing stop signal stream closed")?;
                    let dump = timings.clone();
                    // Await each blocking dump before accepting another signal.
                    // This limits the process to one filesystem write at a time;
                    // filesystem completion itself has no hard time bound.
                    tokio::task::spawn_blocking(move || dump.stop_and_dump())
                        .await
                        .context("join gateway timing dump")??;
                }
                changed = shutdown.changed() => {
                    changed.context("gateway timing shutdown sender closed")?;
                    if *shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
    Ok(())
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
