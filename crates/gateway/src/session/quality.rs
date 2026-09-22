use std::time::Duration;
use std::time::Instant;

use waywire_protocol::browser::Feedback;
use waywire_protocol::browser::QualityLevels;
use waywire_protocol::browser::QualityPreset;
use waywire_protocol::pipe::Chroma;
use waywire_protocol::pipe::Crf;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameMetadata;
use waywire_protocol::pipe::Generation;
use waywire_protocol::pipe::Kbps;
use waywire_protocol::pipe::ScalePercent;

const QUALITY_CHANGE_COOLDOWN: Duration = Duration::from_secs(5);
const ENCODER_WARMUP_NANOS: u64 = 1_000_000_000;
const ENCODER_MIN_RENDERED: u64 = 20;
const ENCODER_BAD_DROP_PERCENT: u64 = 10;
const BAD_STREAK_THRESHOLD: u8 = 2;
const GOOD_STREAK_THRESHOLD: u8 = 8;
const GOOD_DROP_PERCENT: u64 = 2;
// Evaluated at compile time: a floor outside the `Fps` range is a build error, not a fallback.
const FPS_FLOOR: Fps = match Fps::new(20) {
    Ok(value) => value,
    Err(_) => panic!("FPS_FLOOR is outside the Fps range"),
};
const SCALE_FLOOR: ScalePercent = ScalePercent::MINIMUM;
const FULL_SCALE: ScalePercent = ScalePercent::MAXIMUM;
const AUTOMATIC_CRF: Crf = match Crf::new(23) {
    Ok(value) => value,
    Err(_) => panic!("AUTOMATIC_CRF is outside the CRF range"),
};
const HIGH_CRF: Crf = match Crf::new(18) {
    Ok(value) => value,
    Err(_) => panic!("HIGH_CRF is outside the CRF range"),
};
const MEDIUM_CRF: Crf = match Crf::new(28) {
    Ok(value) => value,
    Err(_) => panic!("MEDIUM_CRF is outside the CRF range"),
};
const LOW_CRF: Crf = match Crf::new(32) {
    Ok(value) => value,
    Err(_) => panic!("LOW_CRF is outside the CRF range"),
};
const DOWN_STEP_PERCENT: u8 = 80;
const UP_STEP_PERCENT: u8 = 110;
const FPS_STEP: u32 = 10;
const SCALE_STEP: u32 = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Streak {
    None,
    Bad(u8),
    Good(u8),
}

#[derive(Default)]
struct EncoderPressure {
    previous: Option<(Generation, u64, u64)>,
    warmup_until: u64,
    rendered: u64,
    dropped: u64,
}

impl EncoderPressure {
    fn observe(&mut self, metadata: &FrameMetadata) -> bool {
        let previous = self.previous.replace((
            metadata.generation,
            metadata.sequence,
            metadata.capture_nanos,
        ));
        let Some((generation, sequence, captured)) = previous else {
            self.warmup_until = metadata.capture_nanos.saturating_add(ENCODER_WARMUP_NANOS);
            return true;
        };
        if generation != metadata.generation || metadata.sequence <= sequence {
            self.rendered = 0;
            self.dropped = 0;
            self.warmup_until = metadata.capture_nanos.saturating_add(ENCODER_WARMUP_NANOS);
            return true;
        }
        // The first second can replace frames while ffmpeg starts. Keep moving
        // the baseline, so neither startup nor generation-discarded frames count.
        if captured <= self.warmup_until {
            return false;
        }
        let rendered = metadata.sequence - sequence;
        self.rendered = self.rendered.saturating_add(rendered);
        self.dropped = self.dropped.saturating_add(rendered - 1);
        false
    }

    fn take(&mut self) -> (bool, bool) {
        let rendered = std::mem::take(&mut self.rendered);
        let dropped = std::mem::take(&mut self.dropped);
        (
            rendered >= ENCODER_MIN_RENDERED
                && u128::from(dropped) * 100
                    >= u128::from(rendered) * u128::from(ENCODER_BAD_DROP_PERCENT),
            u128::from(dropped) * 100 <= u128::from(rendered) * u128::from(GOOD_DROP_PERCENT),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QualityChange {
    pub(super) old: QualityLevels,
    pub(super) new: QualityLevels,
    pub(super) old_crf: Crf,
    pub(super) new_crf: Crf,
    pub(super) old_chroma: Chroma,
    pub(super) new_chroma: Chroma,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QualityBounds {
    bitrate_floor: Kbps,
    bitrate_ceiling: Kbps,
    fps_floor: Fps,
    fps_ceiling: Fps,
    scale_floor: ScalePercent,
    scale_ceiling: ScalePercent,
    crf: Crf,
}

impl QualityBounds {
    fn for_preset(preset: QualityPreset, max_bitrate: Kbps, max_fps: Fps) -> Self {
        let (bitrate_ceiling, crf) = match preset {
            QualityPreset::Automatic => (max_bitrate, AUTOMATIC_CRF),
            QualityPreset::High => (max_bitrate, HIGH_CRF),
            QualityPreset::Medium => (max_bitrate.scaled(50), MEDIUM_CRF),
            QualityPreset::Low => (max_bitrate.scaled(25), LOW_CRF),
        };
        Self {
            bitrate_floor: bitrate_ceiling.scaled(50),
            bitrate_ceiling,
            fps_floor: max_fps.min(FPS_FLOOR),
            fps_ceiling: max_fps,
            scale_floor: SCALE_FLOOR,
            scale_ceiling: FULL_SCALE,
            crf,
        }
    }
}

/// Bad samples move down one ladder level; good samples move up one. Changes
/// are rate-limited, and the first change may happen immediately.
pub(super) struct Quality {
    levels: QualityLevels,
    max_bitrate: Kbps,
    max_fps: Fps,
    crf: Crf,
    chroma: Chroma,
    preset: QualityPreset,
    streak: Streak,
    last_change: Option<Instant>,
    encoder: EncoderPressure,
    encoder_was_bad: bool,
}

impl Quality {
    pub(super) fn new(bitrate: Kbps, fps: Fps) -> Self {
        Self {
            levels: QualityLevels {
                bitrate,
                fps,
                scale: FULL_SCALE,
            },
            max_bitrate: bitrate,
            max_fps: fps,
            crf: AUTOMATIC_CRF,
            chroma: Chroma::Rgb,
            preset: QualityPreset::Automatic,
            streak: Streak::None,
            last_change: None,
            encoder: EncoderPressure::default(),
            encoder_was_bad: false,
        }
    }

    pub(super) fn levels(&self) -> QualityLevels {
        self.levels
    }

    pub(super) fn reset_samples(&mut self) {
        self.encoder = EncoderPressure::default();
        self.encoder_was_bad = false;
        self.streak = Streak::None;
    }

    pub(super) fn submitted(&mut self, metadata: &FrameMetadata) {
        if self.encoder.observe(metadata) {
            self.streak = Streak::None;
            self.encoder_was_bad = false;
        }
    }

    #[cfg(test)]
    pub(super) fn chroma(&self) -> Chroma {
        self.chroma
    }

    pub(super) fn select_preset(&mut self, preset: QualityPreset) -> Option<QualityChange> {
        if self.preset == preset {
            return None;
        }

        self.preset = preset;
        self.reset_samples();
        self.last_change = None;
        let old = self.levels;
        let old_crf = self.crf;
        let old_chroma = self.chroma;
        self.clamp_to_bounds();
        (self.levels != old || self.crf != old_crf).then_some(QualityChange {
            old,
            new: self.levels,
            old_crf,
            new_crf: self.crf,
            old_chroma,
            new_chroma: self.chroma,
        })
    }

    pub(super) fn update(&mut self, feedback: &Feedback, now: Instant) -> Option<QualityChange> {
        let (encoder_bad, encoder_good) = self.encoder.take();
        let encoder_was_bad = std::mem::replace(&mut self.encoder_was_bad, encoder_bad);
        // The stream is damage-driven: absence of frames says nothing about its
        // health. Idle feedback preserves recovery, but interrupts producer overload.
        if feedback.received() == 0 && !encoder_bad {
            if encoder_was_bad {
                self.streak = Streak::None;
            }
            return None;
        }
        let queue_pressure = feedback.queue_busy_ms() / feedback.sample_ms();
        let browser_bad = feedback.received() > 0
            && (queue_pressure >= 0.10 || feedback.queue_peak() >= 24 || feedback.rtt() > 250.0);
        let is_bad = encoder_bad || browser_bad;
        // An isolated drop need not mean ongoing congestion. Allow up to 2% loss
        // so one dropped frame does not erase an otherwise healthy recovery streak.
        let is_good = encoder_good
            && queue_pressure < 0.05
            && feedback.queue_peak() < 24
            && u64::from(feedback.dropped()) * 100
                <= u64::from(feedback.received()) * GOOD_DROP_PERCENT
            && feedback.rtt() < 120.0
            && u64::from(feedback.presented()) * 10 >= u64::from(feedback.received()) * 9;
        self.streak = match (is_bad, is_good, self.streak) {
            (true, _, Streak::Bad(count)) => Streak::Bad(count.saturating_add(1)),
            (true, _, Streak::None | Streak::Good(_)) => Streak::Bad(1),
            (false, true, Streak::Good(count)) => Streak::Good(count.saturating_add(1)),
            (false, true, Streak::None | Streak::Bad(_)) => Streak::Good(1),
            (false, false, _) => Streak::None,
        };
        if self.last_change.is_some_and(|last_change| {
            now.saturating_duration_since(last_change) < QUALITY_CHANGE_COOLDOWN
        }) {
            return None;
        }
        let old = self.levels;
        let old_chroma = self.chroma;
        match self.streak {
            Streak::Bad(count) if count >= BAD_STREAK_THRESHOLD => {
                self.lower_one_level(encoder_bad && !browser_bad);
                self.streak = Streak::None;
            }
            Streak::Good(count) if count >= GOOD_STREAK_THRESHOLD => {
                self.raise_one_level();
                self.streak = Streak::None;
            }
            Streak::None | Streak::Bad(_) | Streak::Good(_) => return None,
        }
        if self.levels == old && self.chroma == old_chroma {
            return None;
        }
        self.last_change = Some(now);
        Some(QualityChange {
            old,
            new: self.levels,
            old_crf: self.crf,
            new_crf: self.crf,
            old_chroma,
            new_chroma: self.chroma,
        })
    }

    fn bounds(&self) -> QualityBounds {
        QualityBounds::for_preset(self.preset, self.max_bitrate, self.max_fps)
    }

    fn clamp_to_bounds(&mut self) {
        let bounds = self.bounds();
        self.levels.bitrate = self
            .levels
            .bitrate
            .at_least(bounds.bitrate_floor)
            .at_most(bounds.bitrate_ceiling);
        self.levels.fps = self
            .levels
            .fps
            .max(bounds.fps_floor)
            .min(bounds.fps_ceiling);
        self.levels.scale = self
            .levels
            .scale
            .max(bounds.scale_floor)
            .min(bounds.scale_ceiling);
        self.crf = bounds.crf;
    }

    // Every encoder configuration change restarts it. For browser/network pressure,
    // chroma and scale soften the picture before a low bitrate cap makes it blocky.
    // Seamless bitrate changes need an in-process encoder rather than a child
    // process with fixed arguments; that must change before treating bitrate as free.
    fn lower_one_level(&mut self, encoder_only: bool) {
        let bounds = self.bounds();
        if encoder_only && self.chroma == Chroma::Rgb {
            // Both YUV conversion and packed RGB scaling were slower than full-size
            // RGB in CPU benchmarks. Reduce work by pacing, preserving resolution.
            self.levels.fps = self.levels.fps.lowered_by(FPS_STEP, bounds.fps_floor);
            return;
        }
        // 4:2:0 softens photos and gradients well, but small colored terminal text suffers.
        // Scale changes only the encoder frame, never the Wayland output.
        if self.chroma != Chroma::Yuv420 {
            self.chroma = Chroma::Yuv420;
        } else if self.levels.scale > bounds.scale_floor {
            self.levels.scale = self.levels.scale.lowered_by(SCALE_STEP, bounds.scale_floor);
        } else if self.levels.bitrate > bounds.bitrate_floor {
            self.levels.bitrate = self
                .levels
                .bitrate
                .scaled(DOWN_STEP_PERCENT)
                .at_least(bounds.bitrate_floor);
        } else if self.levels.fps > bounds.fps_floor {
            self.levels.fps = self.levels.fps.lowered_by(FPS_STEP, bounds.fps_floor);
        }
    }

    fn raise_one_level(&mut self) {
        let bounds = self.bounds();
        if self.levels.fps < bounds.fps_ceiling {
            self.levels.fps = self.levels.fps.raised_by(FPS_STEP, bounds.fps_ceiling);
        } else if self.levels.bitrate < bounds.bitrate_ceiling {
            self.levels.bitrate = self
                .levels
                .bitrate
                .scaled(UP_STEP_PERCENT)
                .at_most(bounds.bitrate_ceiling);
        } else if self.levels.scale < bounds.scale_ceiling {
            self.levels.scale = self
                .levels
                .scale
                .raised_by(SCALE_STEP, bounds.scale_ceiling);
        } else if self.chroma == Chroma::Yuv420 {
            self.chroma = Chroma::Rgb;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::bad_feedback;
    use crate::session::tests::feedback;
    use crate::session::tests::good_feedback;
    use crate::session::tests::value;

    fn warmed_encoder(quality: &mut Quality) -> FrameMetadata {
        let mut metadata = FrameMetadata {
            generation: value(Generation::new(1)),
            sequence: 1,
            capture_nanos: 0,
            width: value(waywire_protocol::pipe::FrameDimension::new(1280)),
            height: value(waywire_protocol::pipe::FrameDimension::new(720)),
            input_sequence: None,
            fps: value(Fps::new(60)),
            chroma: Chroma::Rgb,
        };
        quality.submitted(&metadata);
        metadata.sequence = 2;
        metadata.capture_nanos = 2_000_000_000;
        quality.submitted(&metadata);
        metadata
    }

    fn encoder_sample(
        quality: &mut Quality,
        metadata: &mut FrameMetadata,
        submitted: u64,
        dropped: u64,
    ) {
        metadata.sequence += dropped;
        for _ in 0..submitted {
            metadata.sequence += 1;
            metadata.capture_nanos += 20_000_000;
            quality.submitted(metadata);
        }
    }

    #[test]
    fn healthy_browser_cannot_hide_sustained_encoder_drops() {
        for (submitted, dropped, changes) in [
            (30, 0, false),
            (30, 30, true),
            (18, 2, true),
            (19, 1, false),
            (17, 2, false),
        ] {
            let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
            let mut metadata = warmed_encoder(&mut quality);
            let now = Instant::now();
            encoder_sample(&mut quality, &mut metadata, submitted, dropped);
            assert_eq!(quality.update(&good_feedback(), now), None);
            encoder_sample(&mut quality, &mut metadata, submitted, dropped);
            let change = quality.update(&good_feedback(), now);
            assert_eq!(
                change.is_some(),
                changes,
                "submitted={submitted}, dropped={dropped}"
            );
            assert_eq!(
                quality.levels(),
                QualityLevels {
                    bitrate: value(Kbps::new(8_000)),
                    fps: value(Fps::new(if changes { 50 } else { 60 })),
                    scale: FULL_SCALE,
                }
            );
            assert_eq!(quality.chroma, Chroma::Rgb);
        }
    }

    #[test]
    fn encoder_only_preserves_rgb_resolution_at_the_floor_but_browser_pressure_uses_yuv() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let mut metadata = warmed_encoder(&mut quality);
        let mut now = Instant::now();
        for expected in [50, 40, 30, 20, 20] {
            for _ in 0..2 {
                encoder_sample(&mut quality, &mut metadata, 30, 30);
                quality.update(&good_feedback(), now);
            }
            assert_eq!(quality.levels().fps.get(), expected);
            assert_eq!(quality.levels().scale, FULL_SCALE);
            assert_eq!(quality.levels().bitrate.get(), 8_000);
            assert_eq!(quality.chroma, Chroma::Rgb);
            now += QUALITY_CHANGE_COOLDOWN;
        }
        // Browser pressure must still get the more widely decoded format even
        // when the encoder is also overloaded. CPU pressure cannot mask it.
        for _ in 0..2 {
            encoder_sample(&mut quality, &mut metadata, 30, 30);
            quality.update(&bad_feedback(), now);
        }
        assert_eq!(quality.chroma, Chroma::Yuv420);
        assert_eq!(quality.levels().fps.get(), 20);
        assert_eq!(quality.levels().scale, FULL_SCALE);
    }

    #[test]
    fn isolated_overload_idle_and_sparse_damage_do_not_lower_quality() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let mut metadata = warmed_encoder(&mut quality);
        let now = Instant::now();
        for _ in 0..10 {
            encoder_sample(&mut quality, &mut metadata, 30, 30);
            assert_eq!(quality.update(&good_feedback(), now), None);
            assert_eq!(quality.update(&feedback(0, 0, 0, 0.0, 0, 20.0), now), None);
            // One damaged frame after a minute is not 3,599 lost frames.
            metadata.capture_nanos += 60_000_000_000;
            encoder_sample(&mut quality, &mut metadata, 1, 0);
            assert_eq!(quality.update(&feedback(1, 1, 0, 0.0, 0, 20.0), now), None);
        }
        assert_eq!(quality.chroma, Chroma::Rgb);
        assert_eq!(quality.levels().scale, FULL_SCALE);
        assert_eq!(quality.levels().fps.get(), 60);
    }

    #[test]
    fn generation_and_startup_gaps_discard_pressure_and_streaks() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let mut metadata = warmed_encoder(&mut quality);
        let now = Instant::now();
        encoder_sample(&mut quality, &mut metadata, 30, 30);
        assert_eq!(quality.update(&good_feedback(), now), None);
        encoder_sample(&mut quality, &mut metadata, 30, 30);
        metadata.generation = value(Generation::new(2));
        metadata.sequence += 10_000;
        quality.submitted(&metadata);
        encoder_sample(&mut quality, &mut metadata, 30, 300);
        assert_eq!(quality.update(&good_feedback(), now), None);
        // The interval crossing the grace boundary is also discarded.
        metadata.capture_nanos += 2_000_000_000;
        encoder_sample(&mut quality, &mut metadata, 1, 300);
        assert_eq!(quality.update(&good_feedback(), now), None);
        encoder_sample(&mut quality, &mut metadata, 30, 30);
        assert_eq!(quality.update(&good_feedback(), now), None);
        assert_eq!(quality.levels().fps.get(), 60);
        encoder_sample(&mut quality, &mut metadata, 30, 30);
        assert_eq!(
            quality
                .update(&good_feedback(), now)
                .expect("two fresh bad windows")
                .new
                .fps,
            value(Fps::new(50))
        );
        assert_eq!(quality.chroma, Chroma::Rgb);
    }

    #[test]
    fn encoder_pressure_obeys_cooldown_and_healthy_recovery() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let mut metadata = warmed_encoder(&mut quality);
        let now = Instant::now();
        encoder_sample(&mut quality, &mut metadata, 30, 30);
        assert_eq!(quality.update(&good_feedback(), now), None);
        encoder_sample(&mut quality, &mut metadata, 30, 30);
        assert_eq!(
            quality
                .update(&good_feedback(), now)
                .expect("lower frame rate without adding conversion or scaling")
                .new
                .fps,
            value(Fps::new(50))
        );
        for seconds in 1..5 {
            encoder_sample(&mut quality, &mut metadata, 30, 30);
            assert_eq!(
                quality.update(&good_feedback(), now + Duration::from_secs(seconds)),
                None
            );
        }
        encoder_sample(&mut quality, &mut metadata, 30, 30);
        let change = quality
            .update(&good_feedback(), now + Duration::from_secs(5))
            .expect("next rung at five seconds");
        assert_eq!(change.new.scale, FULL_SCALE);
        assert_eq!(change.new.bitrate, value(Kbps::new(8_000)));
        assert_eq!(change.new.fps, value(Fps::new(40)));
        for seconds in 6..13 {
            encoder_sample(&mut quality, &mut metadata, 49, 1);
            assert_eq!(
                quality.update(&good_feedback(), now + Duration::from_secs(seconds)),
                None
            );
        }
        // 3% replacement must block recovery even when the browser is healthy.
        encoder_sample(&mut quality, &mut metadata, 97, 3);
        assert_eq!(
            quality.update(&good_feedback(), now + Duration::from_secs(13)),
            None
        );
        for seconds in 14..21 {
            encoder_sample(&mut quality, &mut metadata, 49, 1);
            assert_eq!(
                quality.update(&good_feedback(), now + Duration::from_secs(seconds)),
                None
            );
        }
        encoder_sample(&mut quality, &mut metadata, 49, 1);
        assert_eq!(
            quality
                .update(&good_feedback(), now + Duration::from_secs(21))
                .expect("eight healthy windows")
                .new
                .fps,
            value(Fps::new(50))
        );
        assert_eq!(quality.chroma, Chroma::Rgb);
    }

    fn trigger_bad(quality: &mut Quality, now: Instant) -> Option<QualityChange> {
        assert_eq!(quality.update(&bad_feedback(), now), None);
        quality.update(&bad_feedback(), now)
    }

    fn trigger_good(quality: &mut Quality, now: Instant) -> Option<QualityChange> {
        for _ in 0..7 {
            assert_eq!(quality.update(&good_feedback(), now), None);
        }
        quality.update(&good_feedback(), now)
    }

    #[test]
    fn first_change_is_allowed_immediately() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let now = Instant::now();

        let change = trigger_bad(&mut quality, now)
            .expect("second bad sample should change quality immediately");

        assert_eq!(
            change,
            QualityChange {
                old: QualityLevels {
                    bitrate: value(Kbps::new(8_000)),
                    fps: value(Fps::new(60)),
                    scale: value(ScalePercent::new(100)),
                },
                new: QualityLevels {
                    bitrate: value(Kbps::new(8_000)),
                    fps: value(Fps::new(60)),
                    scale: value(ScalePercent::new(100)),
                },
                old_crf: AUTOMATIC_CRF,
                new_crf: AUTOMATIC_CRF,
                old_chroma: Chroma::Rgb,
                new_chroma: Chroma::Yuv420,
            }
        );
    }

    #[test]
    fn bad_ladder_changes_one_level_at_a_time_and_stops_at_every_floor() {
        let mut quality = Quality::new(value(Kbps::new(1_000)), value(Fps::new(30)));
        let mut now = Instant::now();
        let expected = [
            (1_000, 30, 100, Chroma::Yuv420),
            (1_000, 30, 75, Chroma::Yuv420),
            (1_000, 30, 50, Chroma::Yuv420),
            (800, 30, 50, Chroma::Yuv420),
            (640, 30, 50, Chroma::Yuv420),
            (512, 30, 50, Chroma::Yuv420),
            (500, 30, 50, Chroma::Yuv420),
            (500, 20, 50, Chroma::Yuv420),
        ];

        for (bitrate, frame_rate, scale, chroma) in expected {
            let change = trigger_bad(&mut quality, now)
                .expect("bad streak should move down one ladder level");
            assert_eq!(
                (change.new, change.new_chroma),
                (
                    QualityLevels {
                        bitrate: value(Kbps::new(bitrate)),
                        fps: value(Fps::new(frame_rate)),
                        scale: value(ScalePercent::new(scale)),
                    },
                    chroma,
                )
            );
            now += QUALITY_CHANGE_COOLDOWN;
        }

        assert_eq!(trigger_bad(&mut quality, now), None);
        assert_eq!(
            (quality.levels(), quality.chroma),
            (
                QualityLevels {
                    bitrate: value(Kbps::new(500)),
                    fps: value(Fps::new(20)),
                    scale: value(ScalePercent::new(50)),
                },
                Chroma::Yuv420,
            )
        );
    }

    #[test]
    fn bitrate_floor_is_three_hundred_when_half_the_maximum_is_lower() {
        let mut quality = Quality::new(value(Kbps::new(500)), value(Fps::new(20)));
        let mut now = Instant::now();

        for expected_scale in [100, 75, 50] {
            let change = trigger_bad(&mut quality, now)
                .expect("chroma and scale steps should precede the bitrate floor");
            assert_eq!(change.new.scale, value(ScalePercent::new(expected_scale)));
            assert_eq!(change.new_chroma, Chroma::Yuv420);
            now += QUALITY_CHANGE_COOLDOWN;
        }
        for expected in [400, 320, 300] {
            let change = trigger_bad(&mut quality, now)
                .expect("bad streak should lower bitrate to its floor");
            assert_eq!(change.new.bitrate, value(Kbps::new(expected)));
            now += QUALITY_CHANGE_COOLDOWN;
        }

        assert_eq!(trigger_bad(&mut quality, now), None);
    }

    #[test]
    fn good_ladder_reverses_the_bad_ladder_priority_exactly() {
        let mut quality = Quality::new(value(Kbps::new(1_000)), value(Fps::new(40)));
        quality.levels = QualityLevels {
            bitrate: value(Kbps::new(500)),
            fps: value(Fps::new(20)),
            scale: value(ScalePercent::new(50)),
        };
        quality.chroma = Chroma::Yuv420;
        let mut now = Instant::now();
        let expected = [
            (500, 30, 50, Chroma::Yuv420),
            (500, 40, 50, Chroma::Yuv420),
            (550, 40, 50, Chroma::Yuv420),
            (605, 40, 50, Chroma::Yuv420),
            (665, 40, 50, Chroma::Yuv420),
            (731, 40, 50, Chroma::Yuv420),
            (804, 40, 50, Chroma::Yuv420),
            (884, 40, 50, Chroma::Yuv420),
            (972, 40, 50, Chroma::Yuv420),
            (1_000, 40, 50, Chroma::Yuv420),
            (1_000, 40, 75, Chroma::Yuv420),
            (1_000, 40, 100, Chroma::Yuv420),
            (1_000, 40, 100, Chroma::Rgb),
        ];

        for (bitrate, frame_rate, scale, chroma) in expected {
            let change = trigger_good(&mut quality, now)
                .expect("good streak should move up one ladder level");
            assert_eq!(
                (change.new, change.new_chroma),
                (
                    QualityLevels {
                        bitrate: value(Kbps::new(bitrate)),
                        fps: value(Fps::new(frame_rate)),
                        scale: value(ScalePercent::new(scale)),
                    },
                    chroma,
                )
            );
            now += QUALITY_CHANGE_COOLDOWN;
        }

        assert_eq!(trigger_good(&mut quality, now), None);
    }

    #[test]
    fn presets_have_exact_bounds() {
        let quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let cases = [
            (
                QualityPreset::Automatic,
                (4_000, 8_000, 20, 60, 50, 100, 23),
            ),
            (QualityPreset::High, (4_000, 8_000, 20, 60, 50, 100, 18)),
            (QualityPreset::Medium, (2_000, 4_000, 20, 60, 50, 100, 28)),
            (QualityPreset::Low, (1_000, 2_000, 20, 60, 50, 100, 32)),
        ];

        for (
            preset,
            (
                bitrate_floor,
                bitrate_ceiling,
                fps_floor,
                fps_ceiling,
                scale_floor,
                scale_ceiling,
                crf,
            ),
        ) in cases
        {
            let bounds = QualityBounds::for_preset(preset, quality.max_bitrate, quality.max_fps);
            assert_eq!(
                bounds,
                QualityBounds {
                    bitrate_floor: value(Kbps::new(bitrate_floor)),
                    bitrate_ceiling: value(Kbps::new(bitrate_ceiling)),
                    fps_floor: value(Fps::new(fps_floor)),
                    fps_ceiling: value(Fps::new(fps_ceiling)),
                    scale_floor: value(ScalePercent::new(scale_floor)),
                    scale_ceiling: value(ScalePercent::new(scale_ceiling)),
                    crf: value(Crf::new(crf)),
                },
                "wrong bounds for {preset:?}"
            );
        }
    }

    #[test]
    fn preset_crf_values_follow_quality_ordering() {
        let max_bitrate = value(Kbps::new(8_000));
        let max_fps = value(Fps::new(60));
        let high = QualityBounds::for_preset(QualityPreset::High, max_bitrate, max_fps).crf;
        let automatic =
            QualityBounds::for_preset(QualityPreset::Automatic, max_bitrate, max_fps).crf;
        let medium = QualityBounds::for_preset(QualityPreset::Medium, max_bitrate, max_fps).crf;
        let low = QualityBounds::for_preset(QualityPreset::Low, max_bitrate, max_fps).crf;

        assert!(high.get() < automatic.get());
        assert!(automatic.get() < medium.get());
        assert!(medium.get() < low.get());
    }

    #[test]
    fn every_preset_adapts_monotonically_within_its_bounds() {
        for preset in [
            QualityPreset::Automatic,
            QualityPreset::High,
            QualityPreset::Medium,
            QualityPreset::Low,
        ] {
            let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
            quality.select_preset(preset);
            let bounds = quality.bounds();
            let mut now = Instant::now();

            while let Some(change) = trigger_bad(&mut quality, now) {
                assert!(change.new.bitrate <= change.old.bitrate);
                assert!(change.new.fps <= change.old.fps);
                assert!(change.new.scale <= change.old.scale);
                assert!(change.new.bitrate >= bounds.bitrate_floor);
                assert!(change.new.fps >= bounds.fps_floor);
                assert!(change.new.scale >= bounds.scale_floor);
                now += QUALITY_CHANGE_COOLDOWN;
            }
            assert_eq!(quality.levels().bitrate, bounds.bitrate_floor);
            assert_eq!(quality.levels().fps, bounds.fps_floor);
            assert_eq!(quality.levels().scale, bounds.scale_floor);
            assert_eq!(quality.chroma, Chroma::Yuv420);

            while let Some(change) = trigger_good(&mut quality, now) {
                assert!(change.new.bitrate >= change.old.bitrate);
                assert!(change.new.fps >= change.old.fps);
                assert!(change.new.scale >= change.old.scale);
                assert!(change.new.bitrate <= bounds.bitrate_ceiling);
                assert!(change.new.fps <= bounds.fps_ceiling);
                assert!(change.new.scale <= bounds.scale_ceiling);
                now += QUALITY_CHANGE_COOLDOWN;
            }
            assert_eq!(quality.levels().bitrate, bounds.bitrate_ceiling);
            assert_eq!(quality.levels().fps, bounds.fps_ceiling);
            assert_eq!(quality.levels().scale, bounds.scale_ceiling);
            assert_eq!(quality.chroma, Chroma::Rgb);
        }
    }

    #[test]
    fn switching_back_to_automatic_restores_session_ceilings() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        quality.select_preset(QualityPreset::Low);
        assert_eq!(quality.bounds().bitrate_ceiling, value(Kbps::new(2_000)));
        assert_eq!(quality.bounds().fps_ceiling, value(Fps::new(60)));

        quality.select_preset(QualityPreset::Automatic);

        assert_eq!(quality.bounds().bitrate_ceiling, value(Kbps::new(8_000)));
        assert_eq!(quality.bounds().fps_ceiling, value(Fps::new(60)));
        let mut now = Instant::now();
        while trigger_good(&mut quality, now).is_some() {
            now += QUALITY_CHANGE_COOLDOWN;
        }
        assert_eq!(
            quality.levels(),
            QualityLevels {
                bitrate: value(Kbps::new(8_000)),
                fps: value(Fps::new(60)),
                scale: value(ScalePercent::new(100)),
            }
        );
    }

    #[test]
    fn preset_switching_never_changes_chroma() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let now = Instant::now();
        trigger_bad(&mut quality, now).expect("bad streak should lower chroma first");
        assert_eq!(quality.chroma, Chroma::Yuv420);

        for preset in [
            QualityPreset::Low,
            QualityPreset::Medium,
            QualityPreset::High,
            QualityPreset::Automatic,
        ] {
            let _ = quality.select_preset(preset);
            assert_eq!(quality.chroma, Chroma::Yuv420);
        }
    }

    #[test]
    fn changed_preset_resets_streak_and_cooldown_but_same_preset_is_idempotent() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let now = Instant::now();
        assert_eq!(quality.update(&bad_feedback(), now), None);
        assert_eq!(
            quality
                .select_preset(QualityPreset::Medium)
                .map(|change| change.new.bitrate),
            Some(value(Kbps::new(4_000)))
        );
        assert_eq!(quality.update(&bad_feedback(), now), None);
        assert!(quality.update(&bad_feedback(), now).is_some());

        assert_eq!(quality.update(&bad_feedback(), now), None);
        assert_eq!(quality.select_preset(QualityPreset::Medium), None);
        assert!(
            quality
                .update(&bad_feedback(), now + QUALITY_CHANGE_COOLDOWN)
                .is_some()
        );
    }

    #[test]
    fn cooldown_spaces_changes_by_five_seconds() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let now = Instant::now();
        assert!(trigger_bad(&mut quality, now).is_some());

        assert_eq!(
            trigger_bad(&mut quality, now + Duration::from_secs(4)),
            None
        );
        assert!(
            quality
                .update(&bad_feedback(), now + QUALITY_CHANGE_COOLDOWN)
                .is_some()
        );
        assert_eq!(quality.levels().bitrate, value(Kbps::new(8_000)));
        assert_eq!(quality.levels().scale, value(ScalePercent::new(75)));
        assert_eq!(quality.chroma, Chroma::Yuv420);
    }

    #[test]
    fn mixed_samples_clear_the_streak() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let now = Instant::now();
        assert_eq!(quality.update(&bad_feedback(), now), None);
        assert_eq!(
            quality.update(&feedback(50, 48, 0, 0.0, 2, 20.0), now),
            None
        );
        assert_eq!(quality.streak, Streak::None);
        assert_eq!(quality.update(&bad_feedback(), now), None);
        assert_eq!(quality.chroma, Chroma::Rgb);
    }

    #[test]
    fn idle_feedback_preserves_streaks_without_advancing_or_changing_quality() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let mut now = Instant::now();
        let idle = feedback(0, 0, 64, 1000.0, 10_000, 60_000.0);
        assert_eq!(quality.update(&idle, now), None);
        assert_eq!(quality.streak, Streak::None);
        assert_eq!(quality.update(&bad_feedback(), now), None);
        now += Duration::from_mins(1);
        assert_eq!(quality.update(&idle, now), None);
        assert_eq!(quality.streak, Streak::Bad(1));
        assert!(quality.update(&bad_feedback(), now).is_some());
        assert_eq!(quality.chroma, Chroma::Yuv420);
        let last_change = quality.last_change;
        let levels = quality.levels();

        for count in 1..GOOD_STREAK_THRESHOLD {
            now += Duration::from_secs(1);
            assert_eq!(quality.update(&good_feedback(), now), None);
            now += Duration::from_mins(1);
            assert_eq!(quality.update(&idle, now), None);
            assert_eq!(quality.streak, Streak::Good(count));
            assert_eq!(quality.levels(), levels);
            assert_eq!(quality.chroma, Chroma::Yuv420);
            assert_eq!(quality.last_change, last_change);
        }
        assert!(quality.update(&good_feedback(), now).is_some());
        assert_eq!(quality.chroma, Chroma::Rgb);
    }

    #[test]
    fn recovery_accepts_drop_rates_up_to_two_percent() {
        for (received, dropped) in [(50, 1), (100, 2), (1, 0)] {
            let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
            let now = Instant::now();
            assert!(trigger_bad(&mut quality, now).is_some());
            let now = now + QUALITY_CHANGE_COOLDOWN;
            for _ in 1..GOOD_STREAK_THRESHOLD {
                assert_eq!(quality.update(&good_feedback(), now), None);
            }
            let recovered = feedback(received, received - dropped, 0, 0.0, dropped, 20.0);
            assert!(quality.update(&recovered, now).is_some());
            assert_eq!(quality.chroma, Chroma::Rgb);
        }
    }

    #[test]
    fn drop_rates_above_two_percent_discard_recovery_progress() {
        for (received, dropped) in [(49, 1), (100, 3), (1, 1)] {
            let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
            let now = Instant::now();
            assert!(trigger_bad(&mut quality, now).is_some());
            let now = now + QUALITY_CHANGE_COOLDOWN;
            for _ in 1..GOOD_STREAK_THRESHOLD {
                assert_eq!(quality.update(&good_feedback(), now), None);
            }
            let lossy = feedback(received, received - dropped, 0, 0.0, dropped, 20.0);
            assert_eq!(quality.update(&lossy, now), None);
            assert_eq!(quality.streak, Streak::None);
            assert_eq!(quality.update(&good_feedback(), now), None);
            assert_eq!(quality.chroma, Chroma::Yuv420);
        }
    }

    #[test]
    fn high_rtt_counts_as_bad() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let now = Instant::now();
        let high_rtt = feedback(50, 50, 0, 0.0, 0, 251.0);

        assert_eq!(quality.update(&high_rtt, now), None);
        assert!(quality.update(&high_rtt, now).is_some());
    }
}
