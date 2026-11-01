/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Striped software H.264 with `OpenH264`, ported from pixelflux
//! (<https://github.com/selkies-project/pixelflux>, MPL-2.0) at commit
//! `b0c9ad65e8e9db1a5c8aae80e1c55316116d4b90`:
//!
//! - `pixelflux/src/encoders/software.rs`: the stripe count and geometry, the per-stripe
//!   state kept across frames, the `xxh3` content hash, and the striped encode loop that
//!   skips unchanged stripes and encodes the rest in parallel with `rayon`.
//! - `pixelflux/src/encoders/oh264.rs`: the `OpenH264` stripe encoder, its constant
//!   quantizer, infinite GOP and the rebuild that moves the quantizer, with hysteresis.
//! - `pixelflux/src/pipeline.rs`: the still-screen cleanup that refreshes a stripe at the
//!   paint-over quantizer once it holds still, spread over several frames.
//!
//! waywire's changes: compositor damage, not a per-frame capture loop, says which rows
//! changed, and the content hash only confirms that a damaged stripe really changed. The
//! compositor decides when the screen is still by wall-clock time and submits refresh
//! frames, so this module never counts idle frames. Every stripe uses `OpenH264`'s camera
//! usage, which encoded recorded desktop frames three times faster than its
//! screen-content usage. Output keeps waywire's color contract (BT.709 matrix, sRGB
//! transfer, full range) and goes into the browser stripe container rather than
//! `pixelflux`'s wire header. A changing stripe's quantizer is a floor, not pinned:
//! rate control holds the stripe to its share of the viewer's bitrate target and
//! coarsens past the floor when the target is tight. The refresh quantizer stays
//! pinned, and a stripe left at it moves back to the floor once it has spent a
//! second of its budget there, not only after `QP_HYSTERESIS_LIMIT` frames.

use std::ops::Range;

use openh264::OpenH264API;
use openh264::encoder::BitRate;
use openh264::encoder::Complexity;
use openh264::encoder::Encoder;
use openh264::encoder::EncoderConfig;
use openh264::encoder::FrameRate;
use openh264::encoder::FrameType;
use openh264::encoder::IntraFramePeriod;
use openh264::encoder::QpRange;
use openh264::encoder::RateControlMode;
use openh264::encoder::UsageType;
use openh264::encoder::VuiConfig;
use openh264::formats::YUVSlices;
use rayon::prelude::*;
use yuv::BufferStoreMut;
use yuv::YuvConversionMode;
use yuv::YuvPlanarImageMut;
use yuv::YuvRange;
use yuv::YuvStandardMatrix;

/// Most stripes a frame is cut into: the browser decodes one stream per stripe.
pub(crate) const MAX_STRIPES: usize = 8;
/// No stripe is shorter than this many rows. Motion search stops at a stripe's edge,
/// so every stripe intra-codes the rows that scroll into it. At 1280x720, waywire's
/// benchmark scroll sent 993 kbps in four 180-row stripes and 3294 kbps in eight
/// 90-row stripes, past its limit; pixelflux's 64 rows would be worse still.
const MIN_STRIPE_HEIGHT: usize = 180;
/// Dirty frames a stripe asks for a coarser quantizer before its encoder is rebuilt at
/// it. A rebuild costs a key frame, so a finer quantizer after a refresh stays through
/// short bursts of change.
const QP_HYSTERESIS_LIMIT: u32 = 60;
/// Suppresses `OpenH264`'s periodic IDR: key frames are requested, never scheduled.
const INFINITE_INTRA_PERIOD: u32 = 300_000;
/// The rate budget of a constant-quantizer session, large enough that the pinned
/// quantizer, not the rate, shapes every frame.
const BITRATE_CEILING_BPS: u32 = 100_000_000;
/// H.264's coarsest quantizer: the bound of a rate-controlled session.
const MAX_QP: u8 = 51;
/// The frames a still screen's stripe refreshes are spread over, so no one frame
/// carries the whole screen's refresh.
pub(crate) const CLEANUP_STAGGER_FRAMES: usize = 4;
/// `OpenH264`'s largest picture.
pub(crate) const MAX_WIDTH: usize = 3840;

/// How many stripes a frame of `height` rows is cut into. `requested` 0 follows
/// pixelflux: one stripe a core, at most `MAX_STRIPES`, none shorter than
/// `MIN_STRIPE_HEIGHT`.
pub(crate) fn stripe_count(height: usize, requested: usize) -> usize {
    let wanted = if requested == 0 {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    } else {
        requested
    };
    wanted
        .min(MAX_STRIPES)
        .min(height / MIN_STRIPE_HEIGHT)
        .max(1)
}

/// `height` rows cut into `n` contiguous stripes of even height, as row ranges. The
/// first stripes take the remainder two rows at a time; 4:2:0 pairs chroma rows, so no
/// stripe has an odd height. `height` is even.
pub(crate) fn stripe_rows(height: usize, n: usize) -> Vec<Range<usize>> {
    let base = (height / n) & !1;
    let extra = (height - base * n) / 2;
    let mut start = 0;
    (0..n)
        .map(|index| {
            let rows = base + if index < extra { 2 } else { 0 };
            let range = start..start + rows;
            start += rows;
            range
        })
        .collect()
}

/// Convert packed BGRA rows to full-range BT.709 I420 planes `width` wide.
pub(crate) fn convert_rows(
    bgra: &[u8],
    width: usize,
    rows: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) -> Result<(), yuv::YuvError> {
    let mut image = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y),
        y_stride: u32::try_from(width).unwrap_or(u32::MAX),
        u_plane: BufferStoreMut::Borrowed(u),
        u_stride: u32::try_from(width / 2).unwrap_or(u32::MAX),
        v_plane: BufferStoreMut::Borrowed(v),
        v_stride: u32::try_from(width / 2).unwrap_or(u32::MAX),
        width: u32::try_from(width).unwrap_or(u32::MAX),
        height: u32::try_from(rows).unwrap_or(u32::MAX),
    };
    yuv::bgra_to_yuv420(
        &mut image,
        bgra,
        u32::try_from(width * 4).unwrap_or(u32::MAX),
        YuvRange::Full,
        YuvStandardMatrix::Bt709,
        YuvConversionMode::Fast,
    )
}

/// One stripe's `OpenH264` session, kept across frames for inter prediction.
struct StripeEncoder {
    encoder: Encoder,
    qp: u8,
    hysteresis: u32,
    /// Bytes coded since the quantizer last moved, while it was finer than asked for.
    fine_bytes: usize,
    fps: f32,
}

impl StripeEncoder {
    fn new(qp: u8, fps: f32, cap_bps: Option<u32>) -> Option<Self> {
        Some(Self {
            encoder: build_encoder(qp, fps, cap_bps)?,
            qp,
            hysteresis: 0,
            fine_bytes: 0,
            fps,
        })
    }

    /// Move the quantizer to `qp`, rate-capped at `cap_bps` or pinned without a cap.
    /// `OpenH264` applies a quantizer range only to a new session, so a move rebuilds the
    /// encoder and its next frame is a key frame. A finer quantizer applies at once; a
    /// coarser one only after `QP_HYSTERESIS_LIMIT` consecutive requests, or sooner once
    /// the finer one has spent a second of `cap_bps`. True when it rebuilt.
    fn update_qp(&mut self, qp: u8, cap_bps: Option<u32>) -> bool {
        if qp == self.qp {
            self.hysteresis = 0;
            return false;
        }
        if qp > self.qp {
            self.hysteresis += 1;
            let budget = cap_bps.map_or(usize::MAX, |bps| bps as usize / 8);
            if self.hysteresis <= QP_HYSTERESIS_LIMIT && self.fine_bytes <= budget {
                return false;
            }
        }
        self.hysteresis = 0;
        self.fine_bytes = 0;
        match build_encoder(qp, self.fps, cap_bps) {
            Some(encoder) => {
                self.encoder = encoder;
                self.qp = qp;
                true
            }
            None => false,
        }
    }
}

/// An `OpenH264` session for one stripe: real-time camera usage at low complexity, no
/// frame skipping, and no periodic key frames. With `cap_bps` rate control holds the
/// stripe to that budget, never finer than `qp`; without it the quantizer is pinned to
/// `qp` by a budget it never reaches.
fn build_encoder(qp: u8, fps: f32, cap_bps: Option<u32>) -> Option<Encoder> {
    let config = EncoderConfig::new()
        .max_frame_rate(FrameRate::from_hz(fps))
        .usage_type(UsageType::CameraVideoRealTime)
        .complexity(Complexity::Low)
        .skip_frames(false)
        .adaptive_quantization(false)
        .background_detection(false)
        .scene_change_detect(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(INFINITE_INTRA_PERIOD))
        .vui(VuiConfig::srgb())
        .num_threads(1)
        .bitrate(BitRate::from_bps(cap_bps.unwrap_or(BITRATE_CEILING_BPS)))
        .rate_control_mode(RateControlMode::Bitrate)
        .qp(QpRange::new(
            qp,
            if cap_bps.is_some() { MAX_QP } else { qp },
        ));
    Encoder::with_api_config(OpenH264API::from_source(), config).ok()
}

/// Everything one stripe keeps between frames: its planes, its encoder, the hash of
/// the content it last encoded, and whether it was refreshed since it last changed.
pub(crate) struct StripeState {
    pub(crate) rows: Range<usize>,
    pub(crate) y: Vec<u8>,
    pub(crate) u: Vec<u8>,
    pub(crate) v: Vec<u8>,
    /// Set when the planes changed since the stripe was last considered.
    pub(crate) dirty: bool,
    encoder: Option<StripeEncoder>,
    last_hash: u64,
    clean: bool,
}

impl StripeState {
    fn new(rows: Range<usize>, width: usize) -> Self {
        let pixels = width * rows.len();
        Self {
            rows,
            y: vec![0; pixels],
            u: vec![0; pixels / 4],
            v: vec![0; pixels / 4],
            dirty: true,
            encoder: None,
            last_hash: 0,
            clean: true,
        }
    }

    fn content_hash(&self) -> u64 {
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        hasher.update(&self.y);
        hasher.update(&self.u);
        hasher.update(&self.v);
        hasher.digest()
    }
}

/// One stripe's encoded access unit.
pub(crate) struct EncodedStripe {
    pub(crate) rows: Range<usize>,
    pub(crate) key: bool,
    pub(crate) data: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Quantizers {
    /// The quantizer a changing stripe is coded at.
    pub(crate) base: u8,
    /// The finer quantizer a still stripe is refreshed at.
    pub(crate) refresh: u8,
}

/// The stripes of one output, each with its own `OpenH264` stream.
pub(crate) struct Stripes {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) states: Vec<StripeState>,
    fps: f32,
    /// The whole picture's bitrate target; each stripe gets its share by rows.
    pub(crate) bitrate_bps: u32,
}

impl Stripes {
    pub(crate) fn new(
        width: usize,
        height: usize,
        requested: usize,
        fps: f32,
        bitrate_bps: u32,
    ) -> Self {
        let count = stripe_count(height, requested);
        Self {
            width,
            height,
            states: stripe_rows(height, count)
                .into_iter()
                .map(|rows| StripeState::new(rows, width))
                .collect(),
            fps,
            bitrate_bps,
        }
    }

    /// Drop every encoder, so each stripe's next frame starts a new stream on a key frame.
    pub(crate) fn reset(&mut self, fps: f32, bitrate_bps: u32) {
        self.fps = fps;
        self.bitrate_bps = bitrate_bps;
        for state in &mut self.states {
            state.encoder = None;
            state.clean = true;
            state.dirty = true;
        }
    }

    /// Encode the stripes this frame needs, in parallel, top to bottom.
    ///
    /// A damaged stripe is sent when its content hash changed. With `key` every stripe
    /// is sent as a key frame. With `refresh` (the screen holds still) up to a
    /// staggered share of the stripes that changed since their last refresh are coded
    /// again as a key frame at the refresh quantizer, and stay there until they change.
    pub(crate) fn encode(
        &mut self,
        key: bool,
        refresh: bool,
        quantizers: Quantizers,
    ) -> Result<Vec<EncodedStripe>, String> {
        let per_frame = self.states.len().div_ceil(CLEANUP_STAGGER_FRAMES);
        let mut granted = 0;
        let refreshing: Vec<bool> = self
            .states
            .iter()
            .map(|state| {
                let due = refresh
                    && !key
                    && !state.dirty
                    && !state.clean
                    && state.encoder.is_some()
                    && granted < per_frame;
                granted += usize::from(due);
                due
            })
            .collect();
        let width = self.width;
        let fps = self.fps;
        let (height, bitrate) = (self.height as u64, u64::from(self.bitrate_bps));
        let body = |(state, refreshing): (&mut StripeState, bool)| {
            let share = bitrate * state.rows.len() as u64 / height;
            let cap_bps = u32::try_from(share).unwrap_or(u32::MAX).max(1);
            encode_stripe(state, width, fps, key, refreshing, quantizers, cap_bps)
        };
        let results: Vec<Result<Option<EncodedStripe>, String>> = if self.states.len() > 1 {
            self.states
                .par_iter_mut()
                .zip(refreshing)
                .map(body)
                .collect()
        } else {
            self.states.iter_mut().zip(refreshing).map(body).collect()
        };
        results.into_iter().filter_map(Result::transpose).collect()
    }
}

fn encode_stripe(
    state: &mut StripeState,
    width: usize,
    fps: f32,
    key: bool,
    refreshing: bool,
    quantizers: Quantizers,
    cap_bps: u32,
) -> Result<Option<EncodedStripe>, String> {
    let mut changed = false;
    if state.dirty {
        state.dirty = false;
        let hash = state.content_hash();
        changed = hash != state.last_hash || state.encoder.is_none();
        state.last_hash = hash;
    }
    if !(changed || key || refreshing) {
        return Ok(None);
    }
    let mut force_key = key;
    match state.encoder.as_mut() {
        None => {
            state.encoder = Some(
                StripeEncoder::new(quantizers.base, fps, Some(cap_bps)).ok_or_else(|| {
                    format!("OpenH264 rejected a {width}x{} stripe", state.rows.len())
                })?,
            );
            force_key = true;
        }
        Some(encoder) if refreshing => {
            force_key |= encoder.update_qp(quantizers.refresh, None);
        }
        Some(encoder) if changed => {
            force_key |= encoder.update_qp(quantizers.base, Some(cap_bps));
        }
        Some(_) => {}
    }
    if changed {
        state.clean = false;
    }
    if refreshing {
        state.clean = true;
    }
    let Some(encoder) = state.encoder.as_mut() else {
        return Ok(None);
    };
    if force_key {
        encoder.encoder.force_intra_frame();
    }
    let height = state.rows.len();
    let planes = YUVSlices::new(
        (&state.y, &state.u, &state.v),
        (width, height),
        (width, width / 2, width / 2),
    );
    let bitstream = encoder
        .encoder
        .encode(&planes)
        .map_err(|error| format!("OpenH264 encode failed: {error}"))?;
    let key = matches!(bitstream.frame_type(), FrameType::IDR);
    let data = bitstream.to_vec();
    if encoder.qp < quantizers.base {
        encoder.fine_bytes += data.len();
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(EncodedStripe {
        rows: state.rows.clone(),
        key,
        data,
    }))
}

#[cfg(test)]
mod tests {
    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;

    use super::*;

    const WIDTH: usize = 320;
    const HEIGHT: usize = 384;
    const HALF: usize = HEIGHT / 2;
    const QUANTIZERS: Quantizers = Quantizers {
        base: 20,
        refresh: 14,
    };

    /// A textured luma plane, moved by `shift` columns, with flat chroma.
    fn picture(shift: usize) -> Vec<u8> {
        (0..WIDTH * HEIGHT)
            .map(|index| {
                let (x, y) = (index % WIDTH + shift, index / WIDTH);
                u8::try_from((x * 7 + y * 3) % 200 + 20).expect("byte")
            })
            .collect()
    }

    /// Load `luma` into the stripes as the compositor's damage copy would, every
    /// stripe marked as possibly changed.
    fn load(stripes: &mut Stripes, luma: &[u8]) {
        for state in &mut stripes.states {
            state
                .y
                .copy_from_slice(&luma[state.rows.start * WIDTH..state.rows.end * WIDTH]);
            state.u.fill(128);
            state.v.fill(128);
            state.dirty = true;
        }
    }

    /// Decode `stripe` with the stripe's own decoder and return the mean absolute
    /// luma error against `luma`.
    fn luma_error(decoder: &mut Decoder, stripe: &EncodedStripe, luma: &[u8]) -> f64 {
        let picture = decoder
            .decode(&stripe.data)
            .expect("decode stripe")
            .expect("decoded picture");
        assert_eq!(picture.dimensions(), (WIDTH, stripe.rows.len()));
        let pitch = picture.strides().0;
        let mut error = 0_u32;
        for row in 0..stripe.rows.len() {
            let source = &luma[(stripe.rows.start + row) * WIDTH..][..WIDTH];
            for (column, value) in source.iter().enumerate() {
                error += u32::from(picture.y()[row * pitch + column].abs_diff(*value));
            }
        }
        f64::from(error) / f64::from(u32::try_from(WIDTH * stripe.rows.len()).expect("pixels"))
    }

    #[test]
    fn only_changed_stripes_are_sent_and_each_decodes_to_its_rows() {
        let mut stripes = Stripes::new(WIDTH, HEIGHT, 2, 30.0, 100_000_000);
        let mut decoders = [
            Decoder::new().expect("decoder"),
            Decoder::new().expect("decoder"),
        ];
        let first = picture(0);
        load(&mut stripes, &first);
        let sent = stripes.encode(false, false, QUANTIZERS).expect("encode");
        assert_eq!(
            sent.iter()
                .map(|stripe| (stripe.rows.clone(), stripe.key))
                .collect::<Vec<_>>(),
            [(0..HALF, true), (HALF..HEIGHT, true)]
        );
        for (decoder, stripe) in decoders.iter_mut().zip(&sent) {
            assert!(luma_error(decoder, stripe, &first) < 3.0);
        }

        // Only the lower half changes, though both stripes are reported damaged.
        let mut second = first.clone();
        second[HALF * WIDTH..].copy_from_slice(&picture(5)[HALF * WIDTH..]);
        load(&mut stripes, &second);
        let sent = stripes.encode(false, false, QUANTIZERS).expect("encode");
        assert_eq!(sent.len(), 1, "an unchanged stripe was sent");
        assert_eq!((sent[0].rows.clone(), sent[0].key), (HALF..HEIGHT, false));
        assert!(luma_error(&mut decoders[1], &sent[0], &second) < 3.0);

        // A still screen refreshes one stripe a frame here, each once.
        let refreshed: Vec<Vec<usize>> = (0..3)
            .map(|_| {
                stripes
                    .encode(false, true, QUANTIZERS)
                    .expect("refresh")
                    .into_iter()
                    .map(|stripe| {
                        assert!(stripe.key, "a refresh moves the quantizer by a key frame");
                        let decoder = &mut decoders[stripe.rows.start / HALF];
                        assert!(luma_error(decoder, &stripe, &second) < 1.5);
                        stripe.rows.start
                    })
                    .collect()
            })
            .collect();
        assert_eq!(refreshed, [vec![0], vec![HALF], vec![]]);
    }

    #[test]
    fn a_changing_stripe_keeps_to_its_share_of_the_bitrate_target() {
        let bytes = |bitrate_bps| {
            let mut stripes = Stripes::new(WIDTH, HEIGHT, 2, 30.0, bitrate_bps);
            (0..60)
                .map(|frame| {
                    // Fresh texture each frame, so prediction cannot make it cheap.
                    let noise: Vec<u8> = (0..WIDTH * HEIGHT)
                        .map(|index| {
                            let mixed = (index * 2_654_435_761 + frame * 40_503) >> 7;
                            u8::try_from(mixed % 160 + 48).expect("byte")
                        })
                        .collect();
                    load(&mut stripes, &noise);
                    let sent = stripes.encode(false, false, QUANTIZERS).expect("encode");
                    sent.iter().map(|stripe| stripe.data.len()).sum::<usize>()
                })
                .sum::<usize>()
        };
        let (ample, tight) = (bytes(100_000_000), bytes(1_000_000));
        // Two seconds at 1 Mbps is 250,000 bytes; the first key frame may overshoot.
        assert!(tight < 312_500, "{tight} bytes at 1 Mbps");
        assert!(
            ample > tight * 4,
            "the quantizer floor alone sent {ample} bytes"
        );
    }

    #[test]
    fn stripes_cover_the_frame_in_even_rows() {
        assert_eq!(
            stripe_rows(720, 8),
            [
                0..90,
                90..180,
                180..270,
                270..360,
                360..450,
                450..540,
                540..630,
                630..720
            ]
        );
        assert_eq!(stripe_rows(722, 4), [0..182, 182..362, 362..542, 542..722]);
        assert_eq!(stripe_count(720, 3), 3);
        assert_eq!(stripe_count(720, 8), 4);
        assert_eq!(stripe_count(100, 8), 1);
        assert_eq!(stripe_count(4000, 32), MAX_STRIPES);
    }
}
