//! Offline, identical-frame codec profiling. Not the desktop benchmark.
//!
//! ```text
//! cargo build --release -p waywire-encoder --example profile_openh264
//! profile_openh264 BGRA WIDTH HEIGHT FPS VARIANT QP CAP_BPS REPEATS [OUTPUT_PREFIX]
//! ```
//!
//! Variants: baseline, background, medium, screen, rc-off. `CAP_BPS=0` pins QP
//! with the production 100 Mbps ceiling; otherwise QP is a floor. RC-Off is
//! diagnostic only and rejects a cap. Timers surround only `encode_at`, excluding
//! conversion, packet copying, decoding, scoring, file IO and initialization.
//! Each repetition starts a new session, with one discarded initialization frame
//! followed by a forced IDR. One complete untimed repetition warms the codec.
//! Quality is against the exact I420 input, not against asynchronous screenshots.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "diagnostic counters and measured times are reported as floating-point statistics"
)]

use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use nix::time::ClockId;
use nix::time::clock_gettime;
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::decoder::Decoder;
use openh264::encoder::BitRate;
use openh264::encoder::Complexity;
use openh264::encoder::Encoder;
use openh264::encoder::EncoderConfig;
use openh264::encoder::FrameRate;
use openh264::encoder::IntraFramePeriod;
use openh264::encoder::QpRange;
use openh264::encoder::RateControlMode;
use openh264::encoder::UsageType;
use openh264::encoder::VuiConfig;
use openh264::formats::YUVSlices;
use openh264::formats::YUVSource;
use openh264_sys2::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT;
use openh264_sys2::RC_OFF_MODE;
use openh264_sys2::SEncParamExt;
use yuv::BufferStoreMut;
use yuv::YuvConversionMode;
use yuv::YuvPlanarImageMut;
use yuv::YuvRange;
use yuv::YuvStandardMatrix;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Picture {
    planes: Vec<u8>,
    edges: Vec<usize>,
    width: usize,
    height: usize,
}

impl Picture {
    fn source(&self) -> YUVSlices<'_> {
        let pixels = self.width * self.height;
        YUVSlices::new(
            (
                &self.planes[..pixels],
                &self.planes[pixels..pixels * 5 / 4],
                &self.planes[pixels * 5 / 4..],
            ),
            (self.width, self.height),
            (self.width, self.width / 2, self.width / 2),
        )
    }
}

fn pictures(path: &Path, width: usize, height: usize) -> Result<Vec<Picture>> {
    let mut input = BufReader::new(File::open(path)?);
    let pixels = width * height;
    let mut bgra = vec![0; pixels * 4];
    let mut pictures = Vec::new();
    loop {
        if input.read(&mut bgra[..1])? == 0 {
            break;
        }
        input.read_exact(&mut bgra[1..])?;
        let mut planes = vec![0; pixels * 3 / 2];
        let (y, uv) = planes.split_at_mut(pixels);
        let (u, v) = uv.split_at_mut(pixels / 4);
        let mut image = YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(y),
            u_plane: BufferStoreMut::Borrowed(u),
            v_plane: BufferStoreMut::Borrowed(v),
            y_stride: u32::try_from(width)?,
            u_stride: u32::try_from(width / 2)?,
            v_stride: u32::try_from(width / 2)?,
            width: u32::try_from(width)?,
            height: u32::try_from(height)?,
        };
        // Same color conversion as striped::encoder::convert_bgra_rows.
        yuv::bgra_to_yuv420(
            &mut image,
            &bgra,
            u32::try_from(width * 4)?,
            YuvRange::Full,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Fast,
        )?;
        let edges = (width..pixels)
            .filter(|&i| {
                i % width != 0
                    && (planes[i].abs_diff(planes[i - 1]) >= 4
                        || planes[i].abs_diff(planes[i - width]) >= 4)
            })
            .collect();
        pictures.push(Picture {
            planes,
            edges,
            width,
            height,
        });
    }
    if pictures.is_empty() {
        return Err("empty corpus".into());
    }
    Ok(pictures)
}

fn encoder(variant: &str, qp: u8, fps: f32, cap: u32, first: &Picture) -> Result<Encoder> {
    // Keep baseline aligned with striped::encoder::build_encoder. No production
    // defaults are overridden or made configurable by this standalone example.
    let config = EncoderConfig::new()
        .max_frame_rate(FrameRate::from_hz(fps))
        .usage_type(if variant == "screen" {
            UsageType::ScreenContentRealTime
        } else {
            UsageType::CameraVideoRealTime
        })
        .complexity(if variant == "medium" {
            Complexity::Medium
        } else {
            Complexity::Low
        })
        .skip_frames(false)
        .adaptive_quantization(false)
        .background_detection(variant == "background")
        .scene_change_detect(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(300_000))
        .vui(VuiConfig::srgb())
        .num_threads(1)
        .bitrate(BitRate::from_bps(if cap == 0 { 100_000_000 } else { cap }))
        // RC callbacks are installed at initialization. Updating only the mode
        // through the extended-parameter setter leaves the old GOM callbacks.
        .rate_control_mode(if variant == "rc-off" {
            RateControlMode::Off
        } else {
            RateControlMode::Bitrate
        })
        .qp(QpRange::new(qp, if cap == 0 { qp } else { 51 }));
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;
    // Initialize through the safe wrapper before querying native parameters.
    let _ = encoder.encode(&first.source())?;
    let mut params = SEncParamExt::default();
    // SAFETY: Only layer QP changes below; dimensions, format and ownership stay
    // unchanged, so the wrapper's cached dimensions remain valid.
    let raw = unsafe { encoder.raw_api() };
    // SAFETY: The initialized encoder fills a correctly sized SEncParamExt.
    if unsafe {
        raw.get_option(
            ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
            (&raw mut params).cast(),
        )
    } != 0
    {
        return Err("native parameter query failed".into());
    }
    if variant == "rc-off" {
        params.iRCMode = RC_OFF_MODE;
        params.sSpatialLayers[0].iDLayerQp = i32::from(qp);
        // SAFETY: These parameters came from this encoder; only RC and QP change.
        if unsafe {
            raw.set_option(
                ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                (&raw mut params).cast(),
            )
        } != 0
        {
            return Err("native RC-Off update failed".into());
        }
        // SAFETY: As above, the output buffer has the native parameter type.
        if unsafe {
            raw.get_option(
                ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                (&raw mut params).cast(),
            )
        } != 0
            || params.iRCMode != RC_OFF_MODE
            || params.sSpatialLayers[0].iDLayerQp != i32::from(qp)
        {
            return Err("native RC-Off settings did not take effect".into());
        }
    }
    eprintln!(
        "effective: usage={} complexity={} background={} aq={} scene={} threads={} rc={} layer_qp={}",
        params.iUsageType,
        params.iComplexityMode,
        params.bEnableBackgroundDetection,
        params.bEnableAdaptiveQuant,
        params.bEnableSceneChangeDetect,
        params.iMultipleThreadIdc,
        params.iRCMode,
        params.sSpatialLayers[0].iDLayerQp,
    );
    encoder.force_intra_frame();
    Ok(encoder)
}

fn cpu_ms() -> Result<f64> {
    let time = clock_gettime(ClockId::CLOCK_PROCESS_CPUTIME_ID)?;
    Ok(time.tv_sec() as f64 * 1000.0 + time.tv_nsec() as f64 / 1_000_000.0)
}

fn psnr(error: u64, count: usize) -> f64 {
    if error == 0 {
        100.0
    } else {
        10.0 * (65025.0 * count as f64 / error as f64).log10()
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "keep the sequential timed replay and untimed scoring visible together"
)]
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if !(9..=10).contains(&args.len()) {
        return Err("usage: profile_openh264 BGRA WIDTH HEIGHT FPS VARIANT QP CAP_BPS REPEATS [OUTPUT_PREFIX]".into());
    }
    let width: usize = args[2].parse()?;
    let height: usize = args[3].parse()?;
    let fps: f32 = args[4].parse()?;
    let variant = args[5].as_str();
    let qp: u8 = args[6].parse()?;
    let cap: u32 = args[7].parse()?;
    let repeats: usize = args[8].parse()?;
    if width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || fps <= 0.0
        || !fps.is_finite()
        || qp > 51
        || repeats == 0
        || !["baseline", "background", "medium", "screen", "rc-off"].contains(&variant)
        || (variant == "rc-off" && cap != 0)
    {
        return Err("invalid geometry, variant, quantizer, cap or repetition count".into());
    }
    let pictures = pictures(Path::new(&args[1]), width, height)?;
    let frames = pictures.len();
    let mut times = Vec::new();
    let mut wall_times = Vec::new();
    let mut cpu_runs = Vec::new();
    let mut packets = Vec::new();
    for repeat in 0..=repeats {
        let mut encoder = encoder(variant, qp, fps, cap, &pictures[0])?;
        let mut total = 0.0;
        for (i, picture) in pictures.iter().enumerate() {
            let timestamp = Timestamp::from_millis(u64::try_from(
                Duration::from_secs_f64((i + 1) as f64 / f64::from(fps)).as_millis(),
            )?);
            let start_cpu = cpu_ms()?;
            let start_wall = Instant::now();
            let bitstream = encoder.encode_at(&picture.source(), timestamp)?;
            let wall = start_wall.elapsed().as_secs_f64() * 1000.0;
            let cpu = cpu_ms()? - start_cpu;
            if repeat != 0 {
                total += cpu;
                times.push(cpu);
                wall_times.push(wall);
            }
            if repeat == 1 {
                packets.push(bitstream.to_vec());
            }
        }
        if repeat != 0 {
            cpu_runs.push(total / frames as f64);
        }
    }
    // Decode only after every timed repetition is finished.
    let mut reconstruction = Decoder::new()?;
    let mut errors = [0_u64; 6];
    let mut counts = [0; 6];
    let mut worst = 100.0_f64;
    let mut stream = args
        .get(9)
        .map(|p| File::create(format!("{p}.h264")))
        .transpose()?;
    let mut reference = args
        .get(9)
        .map(|p| File::create(format!("{p}.i420")))
        .transpose()?;
    for (index, (picture, packet)) in pictures.iter().zip(&packets).enumerate() {
        let decoded = reconstruction
            .decode(packet)?
            .ok_or("missing decoded frame")?;
        if decoded.dimensions() != (width, height) {
            return Err("decoded geometry differs".into());
        }
        let source = picture.source();
        let previous = index.checked_sub(1).map(|i| pictures[i].source());
        let strides = decoded.strides();
        let mut frame_errors = 0;
        for (plane, (original, output, stride, w, h)) in [
            (source.y(), decoded.y(), strides.0, width, height),
            (source.u(), decoded.u(), strides.1, width / 2, height / 2),
            (source.v(), decoded.v(), strides.2, width / 2, height / 2),
        ]
        .into_iter()
        .enumerate()
        {
            for row in 0..h {
                for col in 0..w {
                    let diff =
                        i64::from(original[row * w + col]) - i64::from(output[row * stride + col]);
                    let error = u64::try_from(diff * diff)?;
                    errors[plane] += error;
                    frame_errors += error;
                    if let Some(previous) = &previous {
                        let prior = [previous.y(), previous.u(), previous.v()][plane];
                        if original[row * w + col] != prior[row * w + col] {
                            let changed = if plane == 0 { 4 } else { 5 };
                            errors[changed] += error;
                            counts[changed] += 1;
                        }
                    }
                }
            }
            counts[plane] += w * h;
        }
        worst = worst.min(psnr(frame_errors, width * height * 3 / 2));
        for &i in &picture.edges {
            let diff = i64::from(source.y()[i])
                - i64::from(decoded.y()[i / width * strides.0 + i % width]);
            errors[3] += u64::try_from(diff * diff)?;
        }
        counts[3] += picture.edges.len();
        if let Some(output) = &mut stream {
            output.write_all(packet)?;
        }
        if let Some(output) = &mut reference {
            output.write_all(&picture.planes)?;
        }
    }
    times.sort_by(f64::total_cmp);
    wall_times.sort_by(f64::total_cmp);
    cpu_runs.sort_by(f64::total_cmp);
    let bytes: usize = packets.iter().map(Vec::len).sum();
    println!(
        "variant,qp,cap_bps,frames,bytes,kbps_at_fps,cpu_ms_frame,cpu_min,cpu_max,encode_p95_ms,wall_p95_ms,psnr_y,psnr_u,psnr_v,psnr_edge,psnr_changed_y,psnr_changed_uv,psnr_worst"
    );
    println!(
        "{variant},{qp},{cap},{frames},{bytes},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{worst:.6}",
        bytes as f64 * 8.0 * f64::from(fps) / frames as f64 / 1000.0,
        cpu_runs[repeats / 2],
        cpu_runs[0],
        cpu_runs[repeats - 1],
        times[(times.len() - 1) * 95 / 100],
        wall_times[(wall_times.len() - 1) * 95 / 100],
        psnr(errors[0], counts[0]),
        psnr(errors[1], counts[1]),
        psnr(errors[2], counts[2]),
        psnr(errors[3], counts[3]),
        psnr(errors[4], counts[4]),
        psnr(errors[5], counts[5]),
    );
    Ok(())
}
