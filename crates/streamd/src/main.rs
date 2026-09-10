mod command_reader;
mod event_writer;
mod video;
mod wayland;

use anyhow::Result;
use clap::Parser;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::Kbps;

#[derive(Debug, Parser)]
#[command(
    name = "sprite-desktop-streamd",
    version,
    about = "Captures one Wayland output and streams H.264 RTP"
)]
struct Options {
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
    #[arg(long, default_value = "60", value_parser = parse_fps)]
    frame_rate: Fps,
    #[arg(long, default_value = "8000", value_parser = parse_kbps)]
    bitrate: Kbps,
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=65534))]
    rtp_port: u16,
    #[arg(long, default_value = "us", value_parser = parse_layout)]
    xkb_layout: String,
}

fn parse_fps(value: &str) -> Result<Fps, String> {
    let value = value
        .parse::<u32>()
        .map_err(|error| format!("invalid frame rate: {error}"))?;
    Fps::new(value).map_err(|error| error.to_string())
}

fn parse_kbps(value: &str) -> Result<Kbps, String> {
    let value = value
        .parse::<u32>()
        .map_err(|error| format!("invalid bitrate: {error}"))?;
    Kbps::new(value).map_err(|error| error.to_string())
}

fn parse_layout(value: &str) -> Result<String, String> {
    if wayland::input::valid_layout(value) {
        Ok(value.to_owned())
    } else {
        Err("layout must be 1-32 ASCII letters, digits, '_' or '-'".into())
    }
}

fn main() -> Result<()> {
    wayland::run(Options::parse())
}
