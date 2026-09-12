mod event_writer;
mod video;
mod wayland;

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameSize;
use waywire_protocol::pipe::Kbps;

#[derive(Debug, Parser)]
#[command(
    name = "waywire-streamd",
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
    #[arg(
        long,
        env = "WAYWIRE_RESOLUTION",
        default_value = "1920x1080",
        value_parser = parse_resolution
    )]
    resolution: FrameSize,
    #[arg(long, env = "XCURSOR_THEME", default_value = "breeze_cursors")]
    cursor_theme: String,
    #[arg(
        long,
        env = "XCURSOR_PATH",
        value_delimiter = ':',
        default_values_os_t = default_cursor_theme_paths()
    )]
    cursor_theme_path: Vec<PathBuf>,
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

fn default_cursor_theme_paths() -> Vec<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let mut paths = Vec::new();
    if let Some(data_home) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(data_home).join("icons"));
    } else if let Some(home) = &home {
        paths.push(home.join(".local/share/icons"));
    }
    if let Some(home) = &home {
        paths.push(home.join(".icons"));
    }
    if let Some(data_dirs) = env::var_os("XDG_DATA_DIRS").filter(|value| !value.is_empty()) {
        paths.extend(
            env::split_paths(&data_dirs)
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| path.join("icons")),
        );
    } else {
        paths.extend([
            PathBuf::from("/usr/local/share/icons"),
            PathBuf::from("/usr/share/icons"),
        ]);
    }
    paths.push(PathBuf::from("/usr/share/pixmaps"));
    if let Some(home) = home {
        paths.push(home.join(".cursors"));
    }
    paths.push(PathBuf::from("/usr/share/cursors/xorg-x11"));
    paths
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    wayland::run(Options::parse())
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
