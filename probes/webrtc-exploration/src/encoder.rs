//! One owned encoder per session: fixed VP9 profile 1/RTP output, with an
//! owned wf-recorder child for desktop capture.

use std::io;
use std::process::{ExitStatus, Stdio};

use tokio::process::{Child, Command};

use crate::source::Source;

pub struct EncoderConfig {
    pub ssrc: u32,
    pub rtp_port: u16,
}

/// Build the sole encoder shape used by this probe. VP9 profile 1 preserves
/// full chroma; this software encoder path makes no hardware claim.
pub fn ffmpeg_args(source: Source, config: &EncoderConfig) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-nostdin".into(),
    ];
    args.extend(source.input_args());
    args.extend([
        "-an".into(),
        "-vf".into(),
        "scale=1824:848:out_color_matrix=bt709:out_range=pc".into(),
        "-c:v".into(),
        "libvpx-vp9".into(),
        "-deadline".into(),
        "realtime".into(),
        "-cpu-used".into(),
        "8".into(),
        "-row-mt".into(),
        "1".into(),
        "-lag-in-frames".into(),
        "0".into(),
        "-error-resilient".into(),
        "1".into(),
        "-threads".into(),
        "4".into(),
        "-profile:v".into(),
        "1".into(),
        "-pix_fmt".into(),
        "yuv444p".into(),
        "-g".into(),
        "15".into(),
        "-b:v".into(),
        "8M".into(),
        "-maxrate".into(),
        "16M".into(),
        "-bufsize".into(),
        "16M".into(),
        "-colorspace".into(),
        "bt709".into(),
        "-color_primaries".into(),
        "bt709".into(),
        "-color_trc".into(),
        "iec61966-2-1".into(),
        "-color_range".into(),
        "pc".into(),
        "-payload_type".into(),
        "96".into(),
        "-ssrc".into(),
        config.ssrc.to_string(),
        // FFmpeg still gates its VP9 RTP packetizer as experimental.
        "-strict".into(),
        "experimental".into(),
        "-f".into(),
        "rtp".into(),
        format!("rtp://127.0.0.1:{}?pkt_size=1200", config.rtp_port),
    ]);
    args
}

pub fn wf_recorder_args() -> Vec<String> {
    vec![
        "--no-damage".into(),
        "--no-dmabuf".into(),
        "--output".into(),
        "HEADLESS-1".into(),
        "--codec".into(),
        "rawvideo".into(),
        "--pixel-format".into(),
        "bgra".into(),
        "--muxer".into(),
        "rawvideo".into(),
        "--framerate".into(),
        "60".into(),
        "--filter".into(),
        "scale=1824:848".into(),
        "--file".into(),
        "/dev/stdout".into(),
        "--overwrite".into(),
    ]
}

pub struct Encoder {
    recorder: Option<Child>,
    ffmpeg: Child,
}

impl Encoder {
    pub async fn spawn(
        ffmpeg_bin: &str,
        source: Source,
        config: &EncoderConfig,
    ) -> io::Result<Self> {
        let mut recorder = if source == Source::Desktop {
            let mut command = Command::new("wf-recorder");
            command
                .args(wf_recorder_args())
                .env("XDG_RUNTIME_DIR", "/tmp/sprite-desktop-rust-session")
                .env("WAYLAND_DISPLAY", "wayland-0")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true);
            let child = command.spawn()?;
            if let Some(pid) = child.id() {
                println!("webrtc-exploration: capture pid={pid}");
            }
            Some(child)
        } else {
            None
        };

        let stdin = match recorder.as_mut() {
            Some(child) => match child.stdout.take().expect("piped stdout").try_into() {
                Ok(stdout) => stdout,
                Err(error) => {
                    stop_child(child, "wf-recorder").await;
                    return Err(error);
                }
            },
            None => Stdio::null(),
        };
        let mut command = Command::new(ffmpeg_bin);
        command
            .args(ffmpeg_args(source, config))
            .env("XDG_RUNTIME_DIR", "/tmp/sprite-desktop-rust-session")
            .env("WAYLAND_DISPLAY", "wayland-0")
            .stdin(stdin)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let ffmpeg = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(child) = recorder.as_mut() {
                    stop_child(child, "wf-recorder").await;
                }
                return Err(error);
            }
        };
        Ok(Self { recorder, ffmpeg })
    }

    pub fn pid(&self) -> Option<u32> {
        self.ffmpeg.id()
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        match self.recorder.as_mut() {
            Some(recorder) => tokio::select! {
                status = recorder.wait() => status,
                status = self.ffmpeg.wait() => status,
            },
            None => self.ffmpeg.wait().await,
        }
    }

    /// Stop the capture producer first, then explicitly kill and reap FFmpeg.
    pub async fn shutdown(&mut self) {
        if let Some(recorder) = self.recorder.as_mut() {
            stop_child(recorder, "wf-recorder").await;
        }
        stop_child(&mut self.ffmpeg, "ffmpeg").await;
    }
}

async fn stop_child(child: &mut Child, name: &str) {
    if let Err(error) = child.start_kill()
        && error.kind() != io::ErrorKind::InvalidInput
    {
        eprintln!("webrtc-exploration: {name} kill failed: {error}");
    }
    if let Err(error) = child.wait().await {
        eprintln!("webrtc-exploration: {name} wait failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EncoderConfig {
        EncoderConfig {
            ssrc: 12345,
            rtp_port: 5555,
        }
    }

    #[test]
    fn desktop_capture_args_request_rawvideo_on_stdout() {
        let args = wf_recorder_args();
        assert!(has(&args, "--codec", "rawvideo"));
        assert!(has(&args, "--muxer", "rawvideo"));
        assert!(has(&args, "--file", "/dev/stdout"));
        assert!(has(&args, "--output", "HEADLESS-1"));
        assert!(has(&args, "--filter", "scale=1824:848"));
        assert!(args.iter().any(|arg| arg == "--overwrite"));
    }

    #[test]
    fn args_fix_vp9_profile_one_full_chroma_and_realtime_settings() {
        let args = ffmpeg_args(Source::Motion, &config());
        assert!(has(&args, "-c:v", "libvpx-vp9"));
        assert!(has(&args, "-profile:v", "1"));
        assert!(has(&args, "-pix_fmt", "yuv444p"));
        assert!(has(&args, "-deadline", "realtime"));
        assert!(has(&args, "-cpu-used", "8"));
        assert!(has(&args, "-row-mt", "1"));
        assert!(has(&args, "-lag-in-frames", "0"));
        assert!(has(&args, "-error-resilient", "1"));
        assert!(has(&args, "-g", "15"));
        assert!(has(
            &args,
            "-vf",
            "scale=1824:848:out_color_matrix=bt709:out_range=pc"
        ));
        assert!(has(&args, "-color_range", "pc"));
        assert!(!args.iter().any(|arg| arg.contains("x264")));
        assert!(!args.iter().any(|arg| arg == "-level:v" || arg == "-bf"));
    }

    #[test]
    fn args_declare_expected_rtp_identity_and_packet_size() {
        let args = ffmpeg_args(Source::Motion, &config());
        assert!(has(&args, "-payload_type", "96"));
        assert!(has(&args, "-ssrc", "12345"));
        assert_eq!(args.last().unwrap(), "rtp://127.0.0.1:5555?pkt_size=1200");
    }

    fn has(args: &[String], flag: &str, value: &str) -> bool {
        args.iter()
            .position(|arg| arg == flag)
            .is_some_and(|index| args.get(index + 1).is_some_and(|arg| arg == value))
    }
}
