use std::path::PathBuf;

use clap::Parser;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::Kbps;

use crate::daemon::XkbLayout;
use crate::http::Origin;

#[derive(Debug, Parser)]
#[command(
    name = "waywire-gateway",
    version,
    about = "Unauthenticated HTTP and WebSocket gateway for waywire-streamd; WebSocket upgrades require the configured Origin. Bind to loopback or put an authenticating proxy, such as the Sprite URL policy, in front."
)]
pub(super) struct Options {
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub(super) listen: String,
    #[arg(long)]
    pub(super) streamd: PathBuf,
    #[arg(long = "public-url", env = "PUBLIC_URL", value_parser = Origin::parse)]
    pub(super) origin: Origin,
    #[arg(long, default_value = "60", value_parser = parse_fps)]
    pub(super) frame_rate: Fps,
    #[arg(long, default_value = "16000", value_parser = parse_kbps)]
    pub(super) bitrate: Kbps,
    #[arg(long, default_value = "us", value_parser = XkbLayout::parse)]
    pub(super) xkb_layout: XkbLayout,
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
