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
    pending: Option<Pending>,
    /// Code the next frame's stripes as key frames.
    key: bool,
    generation: Generation,
    stopping: bool,
}

struct Pending {
    metadata: FrameMetadata,
    crf: Crf,
    refresh: bool,
}

struct Shared {
    staging: Mutex<Staging>,
    work: Condvar,
}

/// A frame for the in-process encoder: the output's BGRA pixels and the rectangles
/// that changed since the last submitted frame.
pub(crate) struct StripedFrame<'a> {
    pub(crate) pixels: &'a [u8],
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) damage: &'a [Rectangle<i32, Physical>],
    pub(crate) metadata: FrameMetadata,
    pub(crate) crf: Crf,
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
        if frame.width > encoder::MAX_WIDTH
            || frame.pixels.len() != frame.width * frame.height * 4
            || !frame.width.is_multiple_of(2)
            || !frame.height.is_multiple_of(2)
        {
            return Err(VideoError::InvalidFrame);
        }
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
        let resized = staging.width != frame.width || staging.height != frame.height;
        if resized {
            let pixels = frame.width * frame.height;
            staging.width = frame.width;
            staging.height = frame.height;
            staging.y = vec![0; pixels];
            staging.u = vec![0; pixels / 4];
            staging.v = vec![0; pixels / 4];
            staging.bands = vec![false; frame.height.div_ceil(BAND_ROWS)];
        }
        let full = [Rectangle::from_size(
            (
                i32::try_from(frame.width).unwrap_or(i32::MAX),
                i32::try_from(frame.height).unwrap_or(i32::MAX),
            )
                .into(),
        )];
        let damage = if resized { &full[..] } else { frame.damage };
        for rectangle in damage {
            let top = usize::try_from(rectangle.loc.y.max(0)).unwrap_or(0);
            let bottom = usize::try_from(rectangle.loc.y.saturating_add(rectangle.size.h).max(0))
                .unwrap_or(0)
                .min(frame.height);
            // 4:2:0 pairs rows; convert whole pairs.
            let (top, bottom) = (top & !1, (bottom + 1) & !1);
            if top >= bottom {
                continue;
            }
            let width = frame.width;
            let rows = bottom - top;
            let staging = &mut *staging;
            encoder::convert_rows(
                &frame.pixels[top * width * 4..bottom * width * 4],
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
        staging.pending = Some(Pending {
            metadata: frame.metadata.clone(),
            crf: frame.crf,
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
            let current = stripes
                .get_or_insert_with(|| Stripes::new(staging.width, staging.height, requested, fps));
            if current.width != staging.width || current.height != staging.height {
                *current = Stripes::new(staging.width, staging.height, requested, fps);
            }
            if generation != Some(pending.metadata.generation) {
                generation = Some(pending.metadata.generation);
                current.reset(fps);
            }
            copy_changed_stripes(&mut staging, current);
            (pending, key)
        };
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
