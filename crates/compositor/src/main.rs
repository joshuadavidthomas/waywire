use std::ffi::OsString;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use waywire_compositor::Config;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameSize;
use waywire_protocol::pipe::Kbps;

#[derive(Debug, Parser)]
#[command(
    name = "waywire-compositor",
    version,
    about = "Headless Wayland compositor streaming H.264 RTP"
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
    #[arg(
        long,
        env = "WAYWIRE_RESOLUTION",
        default_value = "1920x1080",
        value_parser = parse_resolution
    )]
    resolution: FrameSize,
    /// Application argv after --. If absent, `WAYWIRE_SESSION` names one executable.
    /// No shell parsing is performed. Without either, only serve external clients.
    #[arg(last = true)]
    session: Vec<OsString>,
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
    if !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        Ok(value.to_owned())
    } else {
        Err("layout must be 1-32 ASCII letters, digits, '_' or '-'".into())
    }
}

fn parse_resolution(value: &str) -> Result<FrameSize, String> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| "resolution must use WIDTHxHEIGHT".to_owned())?;
    if height.contains('x') {
        return Err("resolution must use WIDTHxHEIGHT".into());
    }
    let width = width
        .parse()
        .map_err(|error| format!("resolution width must be an integer: {error}"))?;
    let height = height
        .parse()
        .map_err(|error| format!("resolution height must be an integer: {error}"))?;
    FrameSize::new(width, height).map_err(|error| error.to_string())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let options = Options::parse();
    waywire_compositor::run(Config {
        ffmpeg: options.ffmpeg,
        frame_rate: options.frame_rate,
        bitrate: options.bitrate,
        rtp_port: options.rtp_port,
        xkb_layout: options.xkb_layout,
        resolution: options.resolution,
        session: options.session,
    })
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn resolution_default_is_full_hd() {
        let command = Options::command();
        let argument = command
            .get_arguments()
            .find(|argument| argument.get_id() == "resolution")
            .expect("resolution argument should exist");
        assert_eq!(argument.get_default_values(), ["1920x1080"]);
    }

    #[test]
    fn resolution_parser_accepts_supported_dimensions() {
        let resolution = parse_resolution("2560x1440").expect("resolution should parse");
        assert_eq!((resolution.width(), resolution.height()), (2560, 1440));
    }

    #[test]
    fn resolution_parser_rejects_bad_shapes_and_unsupported_dimensions() {
        for value in ["1920", "1920X1080", "1920x1080x60", "1919x1080", "8000x100"] {
            assert!(parse_resolution(value).is_err(), "accepted {value}");
        }
    }
}
