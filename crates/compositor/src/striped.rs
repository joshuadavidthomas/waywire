//! In-process H.264: the compositor's damaged rows go to I420 on the event loop, and a
//! worker thread encodes the stripes that changed with `OpenH264` (see `encoder`). The
//! encoded stripes reach the gateway as `Event::EncodedFrame`, so no encoder process,
//! raw-frame pipe or RTP hop sits between them.

mod encoder;

use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::mpsc;
use std::thread;

use calloop::ping::Ping;
use calloop::ping::PingSource;
use calloop::ping::make_ping;
use smithay::utils::Physical;
use smithay::utils::Rectangle;
use tracing::error;
use waywire_protocol::browser::push_stripe;
use waywire_protocol::pipe::Chroma;
use waywire_protocol::pipe::Crf;
use waywire_protocol::pipe::EncodedFrame;
use waywire_protocol::pipe::FrameMetadata;
use waywire_protocol::pipe::Generation;
use waywire_protocol::pipe::Kbps;

use self::encoder::Quantizers;
use self::encoder::Stripes;
use crate::video::VideoError;

/// Rows of the staging picture tracked together as damaged.
const BAND_ROWS: usize = 16;
/// The refresh quantizer. Recorded desktop frames refreshed at 14 came within a few
/// tenths of a dB of the 4:2:0 ceiling; 18 left a scrolled page 4 dB short of it.
const REFRESH_QP: u8 = 14;
/// Frames a still screen's refresh is spread over: every stripe gets one.
pub(crate) const REFRESH_FRAMES: usize = encoder::CLEANUP_STAGGER_FRAMES;

/// The newest output in I420, shared between the event loop, which converts damaged
/// rows into it, and the worker, which copies the changed stripes out.
struct Staging {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    /// One flag per `BAND_ROWS` rows changed since the worker last copied them.
    bands: Vec<bool>,
    /// Scaled BGRA rows on their way to I420.
    scaled: Vec<u8>,
    pending: Option<Pending>,
    /// Code the next frame's stripes as key frames.
    key: bool,
    generation: Generation,
    stopping: bool,
}

struct Pending {
    metadata: FrameMetadata,
    crf: Crf,
    bitrate: Kbps,
    refresh: bool,
}

struct Shared {
    staging: Mutex<Staging>,
    work: Condvar,
}

/// A frame for the in-process encoder: the output's BGRA pixels and the rectangles
/// that changed since the last submitted frame. The metadata's size is the encoded
/// size; a smaller one scales the output down before encoding.
pub(crate) struct StripedFrame<'a> {
    pub(crate) pixels: &'a [u8],
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) damage: &'a [Rectangle<i32, Physical>],
    pub(crate) metadata: FrameMetadata,
    pub(crate) crf: Crf,
    /// The bitrate target the stripes share.
    pub(crate) bitrate: Kbps,
    /// The screen held still: refresh stripes at the finer quantizer.
    pub(crate) refresh: bool,
}

pub(crate) struct StripedEncoder {
    shared: Arc<Shared>,
    notifications: mpsc::Receiver<Result<EncodedFrame, String>>,
    notification_source: Option<PingSource>,
    worker: Option<thread::JoinHandle<()>>,
}

impl StripedEncoder {
    pub(crate) fn start(stripes: usize, generation: Generation) -> Result<Self, VideoError> {
        let shared = Arc::new(Shared {
            staging: Mutex::new(Staging {
                width: 0,
                height: 0,
                y: Vec::new(),
                u: Vec::new(),
                v: Vec::new(),
                bands: Vec::new(),
                scaled: Vec::new(),
                pending: None,
                key: false,
                generation,
                stopping: false,
            }),
            work: Condvar::new(),
        });
        let (sender, notifications) = mpsc::sync_channel(64);
        let (wake, notification_source) = make_ping()?;
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("striped-h264".into())
            .spawn(move || run(&worker_shared, &sender, &wake, stripes))?;
        Ok(Self {
            shared,
            notifications,
            notification_source: Some(notification_source),
            worker: Some(worker),
        })
    }

    pub(crate) fn submit(&self, frame: &StripedFrame<'_>) -> Result<(), VideoError> {
        let width = usize::from(frame.metadata.width.get());
        let height = usize::from(frame.metadata.height.get());
        if width > encoder::MAX_WIDTH
            || width > frame.width
            || height > frame.height
            || frame.pixels.len() != frame.width * frame.height * 4
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
        {
            return Err(VideoError::InvalidFrame);
        }
        let submitted_ns = crate::capture::monotonic_nanos();
        let mut staging = self
            .shared
            .staging
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if staging.stopping {
            return Err(VideoError::Stopped);
        }
        if frame.metadata.generation != staging.generation {
            return Err(VideoError::InvalidFrame);
        }
        let resized = staging.width != width || staging.height != height;
        if resized {
            let pixels = width * height;
            staging.width = width;
            staging.height = height;
            staging.y = vec![0; pixels];
            staging.u = vec![0; pixels / 4];
            staging.v = vec![0; pixels / 4];
            staging.bands = vec![false; height.div_ceil(BAND_ROWS)];
        }
        let full = [Rectangle::from_size(
            (
                i32::try_from(frame.width).unwrap_or(i32::MAX),
                i32::try_from(frame.height).unwrap_or(i32::MAX),
            )
                .into(),
        )];
        let damage = if resized { &full[..] } else { frame.damage };
        let scaled = (width, height) != (frame.width, frame.height);
        for rectangle in damage {
            let top = usize::try_from(rectangle.loc.y.max(0)).unwrap_or(0);
            let bottom = usize::try_from(rectangle.loc.y.saturating_add(rectangle.size.h).max(0))
                .unwrap_or(0)
                .min(frame.height);
            // The encoded rows whose source boxes overlap the damaged rows.
            let (top, bottom) = if scaled {
                (
                    (top * height / frame.height).saturating_sub(1),
                    ((bottom * height).div_ceil(frame.height) + 1).min(height),
                )
            } else {
                (top, bottom)
            };
            // 4:2:0 pairs rows; convert whole pairs.
            let (top, bottom) = (top & !1, (bottom + 1) & !1);
            if top >= bottom {
                continue;
            }
            let rows = bottom - top;
            let staging = &mut *staging;
            let bgra = if scaled {
                staging.scaled.resize(rows * width * 4, 0);
                downscale_rows(frame, width, height, top..bottom, &mut staging.scaled);
                &staging.scaled[..]
            } else {
                &frame.pixels[top * width * 4..bottom * width * 4]
            };
            encoder::convert_rows(
                bgra,
                width,
                rows,
                &mut staging.y[top * width..bottom * width],
                &mut staging.u[top * width / 4..bottom * width / 4],
                &mut staging.v[top * width / 4..bottom * width / 4],
            )
            .map_err(|_conversion| VideoError::InvalidFrame)?;
            for band in &mut staging.bands[top / BAND_ROWS..bottom.div_ceil(BAND_ROWS)] {
                *band = true;
            }
        }
        crate::capture::trace_frame("converted", frame.metadata.sequence, submitted_ns);
        staging.pending = Some(Pending {
            metadata: frame.metadata.clone(),
            crf: frame.crf,
            bitrate: frame.bitrate,
            refresh: frame.refresh,
        });
        self.shared.work.notify_one();
        Ok(())
    }

    /// Code the next submitted frame's stripes as key frames.
    pub(crate) fn request_key(&self) {
        self.shared
            .staging
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .key = true;
    }

    pub(crate) fn set_generation(&self, generation: Generation) -> Result<(), VideoError> {
        let mut staging = self
            .shared
            .staging
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if staging.stopping {
            return Err(VideoError::Stopped);
        }
        staging.generation = generation;
        if staging
            .pending
            .as_ref()
            .is_some_and(|pending| pending.metadata.generation != generation)
        {
            staging.pending = None;
        }
        Ok(())
    }

    pub(crate) fn take_notification_source(&mut self) -> Option<PingSource> {
        self.notification_source.take()
    }

    /// The next encoded frame, or the error that stopped the encoder.
    pub(crate) fn try_frame(&self) -> Option<Result<EncodedFrame, String>> {
        match self.notifications.try_recv() {
            Ok(frame) => Some(frame),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                let stopping = self
                    .shared
                    .staging
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .stopping;
                (!stopping).then(|| Err("video worker stopped unexpectedly".into()))
            }
        }
    }

    pub(crate) fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.shared
            .staging
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopping = true;
        self.shared.work.notify_all();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            error!("striped video worker panicked; continuing shutdown");
        }
    }
}

impl Drop for StripedEncoder {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

fn run(
    shared: &Shared,
    sender: &mpsc::SyncSender<Result<EncodedFrame, String>>,
    wake: &Ping,
    requested: usize,
) {
    let mut stripes: Option<Stripes> = None;
    let mut generation: Option<Generation> = None;
    loop {
        let (pending, key) = {
            let mut staging = shared
                .staging
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            while staging.pending.is_none() && !staging.stopping {
                staging = shared
                    .work
                    .wait(staging)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if staging.stopping {
                return;
            }
            let Some(pending) = staging.pending.take() else {
                continue;
            };
            let key = std::mem::take(&mut staging.key);
            #[expect(clippy::cast_precision_loss, reason = "a frame rate of at most 120")]
            let fps = pending.metadata.fps.get() as f32;
            let bitrate = pending.bitrate.get().saturating_mul(1000);
            let (width, height) = (staging.width, staging.height);
            let current =
                stripes.get_or_insert_with(|| Stripes::new(width, height, requested, fps, bitrate));
            if current.width != width || current.height != height {
                *current = Stripes::new(width, height, requested, fps, bitrate);
            }
            if generation != Some(pending.metadata.generation) || current.bitrate_bps != bitrate {
                generation = Some(pending.metadata.generation);
                current.reset(fps, bitrate);
            }
            copy_changed_stripes(&mut staging, current);
            (pending, key)
        };
        let taken_ns = crate::capture::monotonic_nanos();
        let Some(current) = stripes.as_mut() else {
            continue;
        };
        let base = pending.crf.get().saturating_sub(1).clamp(12, 51);
        let quantizers = Quantizers {
            base,
            refresh: REFRESH_QP.min(base),
        };
        let notification = match current.encode(key, pending.refresh, quantizers) {
            Ok(encoded) => {
                let count = current.states.len();
                let key = encoded.len() == count && encoded.iter().all(|stripe| stripe.key);
                let mut data = Vec::with_capacity(encoded.iter().map(|s| s.data.len() + 16).sum());
                for stripe in &encoded {
                    push_stripe(
                        &mut data,
                        u16::try_from(stripe.rows.start).unwrap_or(u16::MAX),
                        u16::try_from(stripe.rows.len()).unwrap_or(u16::MAX),
                        stripe.key,
                        &stripe.data,
                    );
                }
                let mut metadata = pending.metadata;
                metadata.chroma = Chroma::Yuv420;
                Ok(EncodedFrame {
                    key,
                    metadata,
                    data,
                })
            }
            Err(error) => Err(error),
        };
        if let Ok(frame) = &notification {
            crate::capture::trace_frame("encoder_output", frame.metadata.sequence, taken_ns);
        }
        let fatal = notification.is_err();
        if sender.send(notification).is_err() {
            return;
        }
        wake.ping();
        if fatal {
            return;
        }
    }
}

/// Scale `frame` down to `width` x `height` and write the BGRA of output `rows` to
/// `out`. Each output pixel averages the box of source pixels it covers, so text
/// stays legible rather than aliasing as point sampling would.
fn downscale_rows(
    frame: &StripedFrame<'_>,
    width: usize,
    height: usize,
    rows: std::ops::Range<usize>,
    out: &mut [u8],
) {
    let span = |index: usize, source: usize, target: usize| {
        let start = index * source / target;
        start..((index + 1) * source / target).max(start + 1)
    };
    let columns: Vec<_> = (0..width).map(|x| span(x, frame.width, width)).collect();
    let mut sums = vec![0_u32; width * 4];
    for (row, out) in rows.zip(out.chunks_exact_mut(width * 4)) {
        sums.fill(0);
        let source_rows = span(row, frame.height, height);
        let area_rows = source_rows.len();
        for source_row in source_rows {
            let line = &frame.pixels[source_row * frame.width * 4..][..frame.width * 4];
            for (sum, columns) in sums.chunks_exact_mut(4).zip(&columns) {
                for pixel in line[columns.start * 4..columns.end * 4].chunks_exact(4) {
                    for (channel, value) in sum.iter_mut().zip(pixel) {
                        *channel += u32::from(*value);
                    }
                }
            }
        }
        for ((pixel, sum), columns) in out
            .chunks_exact_mut(4)
            .zip(sums.chunks_exact(4))
            .zip(&columns)
        {
            let area = u32::try_from(area_rows * columns.len()).unwrap_or(u32::MAX);
            for (value, channel) in pixel.iter_mut().zip(sum) {
                *value = u8::try_from((channel + area / 2) / area).unwrap_or(u8::MAX);
            }
        }
    }
}

/// Copy the stripes whose rows changed out of the staging picture.
fn copy_changed_stripes(staging: &mut Staging, stripes: &mut Stripes) {
    let width = staging.width;
    for state in &mut stripes.states {
        let first = state.rows.start / BAND_ROWS;
        let last = state.rows.end.div_ceil(BAND_ROWS);
        if !staging.bands[first..last].iter().any(|band| *band) && !state.dirty {
            continue;
        }
        let rows = state.rows.clone();
        state
            .y
            .copy_from_slice(&staging.y[rows.start * width..rows.end * width]);
        state
            .u
            .copy_from_slice(&staging.u[rows.start * width / 4..rows.end * width / 4]);
        state
            .v
            .copy_from_slice(&staging.v[rows.start * width / 4..rows.end * width / 4]);
        state.dirty = true;
    }
    staging.bands.fill(false);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::Instant;

    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;
    use waywire_protocol::pipe::Fps;
    use waywire_protocol::pipe::FrameDimension;

    use super::*;

    #[test]
    fn a_scaled_frame_is_encoded_at_its_encoded_size_from_averaged_pixels() {
        // A one-pixel checkerboard: point sampling at half size keeps only black or only
        // white; averaging each 2x2 box gives mid grey.
        let (width, height) = (640, 360);
        let pixels: Vec<u8> = (0..width * height)
            .flat_map(|index| {
                let value = if (index % width + index / width) % 2 == 0 {
                    0
                } else {
                    255
                };
                [value, value, value, 255]
            })
            .collect();
        let generation = Generation::new(1).expect("generation");
        let encoder = StripedEncoder::start(1, generation).expect("start");
        encoder
            .submit(&StripedFrame {
                pixels: &pixels,
                width,
                height,
                damage: &[],
                metadata: FrameMetadata {
                    generation,
                    width: FrameDimension::new(320).expect("width"),
                    height: FrameDimension::new(180).expect("height"),
                    capture_nanos: 1,
                    sequence: 1,
                    input_sequence: None,
                    fps: Fps::new(30).expect("fps"),
                    chroma: Chroma::Yuv420,
                },
                crf: Crf::new(27).expect("crf"),
                bitrate: Kbps::new(12_000).expect("bitrate"),
                refresh: false,
            })
            .expect("submit");
        let deadline = Instant::now() + Duration::from_secs(10);
        let frame = loop {
            if let Some(frame) = encoder.try_frame() {
                break frame.expect("encode");
            }
            assert!(Instant::now() < deadline, "no encoded frame");
            thread::sleep(Duration::from_millis(5));
        };
        assert!(frame.key);
        // One stripe: its row, its height, its key flag, its length, then its access unit.
        assert_eq!(&frame.data[..5], &[0, 0, 180, 0, 1]);
        let mut decoder = Decoder::new().expect("decoder");
        let picture = decoder
            .decode(&frame.data[9..])
            .expect("decode")
            .expect("picture");
        assert_eq!(picture.dimensions(), (320, 180));
        let pitch = picture.strides().0;
        let plane = picture.y();
        let luma: u64 = (0..180)
            .flat_map(|row| &plane[row * pitch..][..320])
            .map(|value| u64::from(*value))
            .sum();
        let mean = luma / (320 * 180);
        assert!((120..=136).contains(&mean), "mean luma {mean}");
        encoder.stop();
    }
}
