//! Fixed encoder input selection.
//!
//! Deliberately not a generic backend abstraction. `Desktop` captures the
//! fixed headless Wayland output; `Motion` and `Chart` remain test fixtures.

use clap::ValueEnum;
use serde::Serialize;

/// Frame width shared by every source and by the encoder output.
pub const FRAME_WIDTH: u32 = 1824;
/// Frame height shared by every source and by the encoder output.
pub const FRAME_HEIGHT: u32 = 848;
/// Frame rate shared by every source and by the encoder output.
pub const FRAME_RATE: u32 = 60;

/// Which FFmpeg input feeds the one owned encoder for the session's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The fixed `HEADLESS-1` Wayland desktop captured through wf-recorder.
    Desktop,
    /// Synthetic `testsrc2` motion pattern generated entirely by FFmpeg's
    /// `lavfi` device. Never touches the desktop or any private content.
    Motion,
    /// A static local image (`./chart.png`, resolved against the process's
    /// working directory) looped at [`FRAME_RATE`].
    Chart,
}

impl Source {
    /// FFmpeg input arguments, in the order FFmpeg requires: options that
    /// modify an input precede that input's `-i`.
    pub fn input_args(self) -> Vec<String> {
        match self {
            Source::Desktop => vec![
                "-f".into(),
                "rawvideo".into(),
                "-pixel_format".into(),
                "bgra".into(),
                "-video_size".into(),
                format!("{FRAME_WIDTH}x{FRAME_HEIGHT}"),
                "-framerate".into(),
                FRAME_RATE.to_string(),
                "-i".into(),
                "pipe:0".into(),
            ],
            Source::Motion => vec![
                "-f".into(),
                "lavfi".into(),
                "-re".into(),
                "-i".into(),
                format!("testsrc2=size={FRAME_WIDTH}x{FRAME_HEIGHT}:rate={FRAME_RATE}"),
            ],
            Source::Chart => vec![
                "-loop".into(),
                "1".into(),
                "-framerate".into(),
                FRAME_RATE.to_string(),
                "-re".into(),
                "-i".into(),
                "chart.png".into(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_input_is_unclocked_raw_bgra_from_stdin() {
        let args = Source::Desktop.input_args();
        assert_eq!(
            args,
            [
                "-f",
                "rawvideo",
                "-pixel_format",
                "bgra",
                "-video_size",
                "1824x848",
                "-framerate",
                "60",
                "-i",
                "pipe:0"
            ]
        );
        assert!(!args.iter().any(|arg| arg == "-re"));
    }

    #[test]
    fn motion_input_orders_re_before_i() {
        let args = Source::Motion.input_args();
        let re = args.iter().position(|a| a == "-re").unwrap();
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert!(re < i);
        assert!(args.last().unwrap().contains("1824x848"));
        assert!(args.last().unwrap().contains("rate=60"));
    }

    #[test]
    fn chart_input_reads_a_local_relative_path() {
        let args = Source::Chart.input_args();
        assert_eq!(args.last().unwrap(), "chart.png");
        assert!(args.iter().any(|a| a == "-loop"));
    }
}
