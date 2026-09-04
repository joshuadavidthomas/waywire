//! Private test tool: one peer, joined control sessions, owned capture and encoder.
//! Sprite access uses an authenticated loopback tunnel; media uses WebRTC.

mod control;
mod encoder;
mod http;
mod session;
mod source;

use std::time::Duration;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use source::Source;

#[derive(Parser)]
#[command(name = "webrtc-exploration")]
#[command(about = "Private WebRTC video test: one session with owned capture and encoding.")]
struct Cli {
    #[arg(long, value_enum)]
    source: Source,
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let listener = tokio::net::TcpListener::bind(http::BIND_ADDR).await?;
    let shutdown = CancellationToken::new();
    let (request_tx, request_rx) = mpsc::channel(1);
    let mut actor = tokio::spawn(session::run_actor(
        session::ActorConfig {
            ffmpeg_bin: cli.ffmpeg,
            source: cli.source,
        },
        request_rx,
        shutdown.clone(),
    ));

    println!(
        "webrtc-exploration: listening on http://{}",
        http::BIND_ADDR
    );
    let server_shutdown = shutdown.clone();
    let state = http::AppState::new(request_tx, cli.source, shutdown.clone());
    let control_tasks = state.control_tasks.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, http::router(state))
            .with_graceful_shutdown(server_shutdown.cancelled_owned())
            .await
    });

    let mut sigterm = signal(SignalKind::terminate())?;
    let actor_result = tokio::select! {
        result = &mut actor => {
            println!("webrtc-exploration: session actor complete, shutting down");
            Some(result)
        }
        _ = tokio::signal::ctrl_c() => {
            println!("webrtc-exploration: SIGINT received, shutting down");
            None
        }
        _ = sigterm.recv() => {
            println!("webrtc-exploration: SIGTERM received, shutting down");
            None
        }
    };

    shutdown.cancel();
    let actor_finished = match actor_result {
        Some(result) => result.is_ok(),
        None => match tokio::time::timeout(Duration::from_secs(5), &mut actor).await {
            Ok(result) => result.is_ok(),
            Err(_) => {
                actor.abort();
                let _ = actor.await;
                false
            }
        },
    };
    let server_finished = match tokio::time::timeout(Duration::from_secs(3), &mut server).await {
        Ok(result) => matches!(result, Ok(Ok(()))),
        Err(_) => {
            server.abort();
            let _ = server.await;
            false
        }
    };
    control_tasks.close();
    let controls_finished = tokio::time::timeout(Duration::from_secs(7), control_tasks.wait())
        .await
        .is_ok();
    anyhow::ensure!(
        actor_finished && server_finished && controls_finished,
        "prototype shutdown did not complete cleanly"
    );
    Ok(())
}
