mod event_writer;
mod protocol;
mod video;
mod wayland;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "sprite-desktop-streamd",
    version,
    about = "Captures one Wayland output and streams H.264 RTP"
)]
struct Options {
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(10..=120))]
    frame_rate: u32,
    #[arg(long, default_value_t = 8_000, value_parser = clap::value_parser!(u32).range(300..=50_000))]
    bitrate: u32,
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=65534))]
    rtp_port: u16,
    #[arg(long, default_value = "us", value_parser = parse_layout)]
    xkb_layout: String,
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
