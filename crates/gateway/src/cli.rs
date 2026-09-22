use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameSize;
use waywire_protocol::pipe::Kbps;

use crate::daemon::XkbLayout;
use crate::http::Origin;

#[derive(Debug, Parser)]
#[command(
    name = "waywire-gateway",
    version,
    about = "Unauthenticated HTTP and WebSocket gateway for waywire-compositor; WebSocket upgrades require the configured Origin. Bind to loopback or put an authenticating proxy, such as the Sprite URL policy, in front."
)]
pub(super) struct Options {
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub(super) listen: String,
    #[arg(long, default_value = "waywire-compositor")]
    pub(super) compositor: PathBuf,
    #[arg(long = "public-url", env = "PUBLIC_URL", value_parser = Origin::parse)]
    pub(super) origin: Origin,
    #[arg(long, default_value = "60", value_parser = parse_fps)]
    pub(super) frame_rate: Fps,
    #[arg(long, default_value = "16000", value_parser = parse_kbps)]
    pub(super) bitrate: Kbps,
    #[arg(long, default_value = "us", value_parser = XkbLayout::parse)]
    pub(super) xkb_layout: XkbLayout,
    #[arg(
        long,
        env = "WAYWIRE_RESOLUTION",
        default_value = "1920x1080",
        value_parser = parse_resolution
    )]
    pub(super) resolution: FrameSize,
    /// Application to launch inside the desktop, followed by its arguments.
    #[arg(last = true, value_name = "COMMAND")]
    pub(super) session: Vec<OsString>,
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

    #[test]
    fn session_arguments_are_not_interpreted_as_gateway_options() {
        let options = Options::try_parse_from([
            "waywire-gateway",
            "--public-url",
            "https://desktop.example.com",
            "--",
            "foot",
            "--title",
            "A desktop terminal",
        ])
        .expect("session argv should parse");
        assert_eq!(options.compositor, PathBuf::from("waywire-compositor"));
        assert_eq!(
            options.session,
            ["foot", "--title", "A desktop terminal"].map(OsString::from)
        );
    }
}
