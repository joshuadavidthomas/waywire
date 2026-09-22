mod cli;
mod daemon;
mod http;
mod session;
mod shutdown;
mod video;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use socket2::SockRef;
use tokio::net::UdpSocket;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::cli::Options;
use crate::daemon::Config;
use crate::daemon::Daemon;
use crate::http::AppState;
use crate::http::SocketConnections;
use crate::session::Sessions;
use crate::shutdown::Stop;
use crate::shutdown::finish_shutdown;
use crate::shutdown::http_listener;
use crate::shutdown::shutdown_signal;
use crate::video::VideoHub;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // The service captures stderr into a log file, never a terminal.
        .with_ansi(false)
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
    let hub = VideoHub::new(options.frame_rate);
    let started = Daemon::start(
        &Config {
            path: options.compositor,
            frame_rate: options.frame_rate,
            bitrate: options.bitrate,
            xkb_layout: options.xkb_layout,
            resolution: options.resolution,
            session: options.session,
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
        origin: options.origin,
        connections: connections.clone(),
        next_socket_id: Arc::new(AtomicU64::new(1)),
    });
    let server_shutdown = connections.cancellation();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                server_shutdown.cancelled().await;
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
    let result = finish_shutdown(stop, connections, &daemon, &mut daemon_task, &mut server).await;
    if result.is_ok() {
        info!(outcome = "clean", "shutdown ended");
    } else {
        info!(outcome = "failed", "shutdown ended");
    }
    result
}
