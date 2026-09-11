use std::num::NonZeroU32;
use std::time::Duration;
use std::time::Instant;

use sprite_desktop_protocol::browser::Feedback;
use sprite_desktop_protocol::browser::QualityLevels;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::Kbps;
use sprite_desktop_protocol::pipe::ScalePercent;

const QUALITY_CHANGE_COOLDOWN: Duration = Duration::from_secs(5);
const BAD_STREAK_THRESHOLD: u8 = 2;
const GOOD_STREAK_THRESHOLD: u8 = 8;
const BITRATE_FLOOR: Kbps = Kbps::MINIMUM;
// Evaluated at compile time: a floor outside the `Fps` range is a build error, not a fallback.
const FPS_FLOOR: Fps = match Fps::new(20) {
    Ok(value) => value,
    Err(_) => panic!("FPS_FLOOR is outside the Fps range"),
};
const SCALE_FLOOR: ScalePercent = ScalePercent::MINIMUM;
const FULL_SCALE: ScalePercent = ScalePercent::MAXIMUM;
const DOWN_STEP_PERCENT: u8 = 80;
const UP_STEP_PERCENT: u8 = 110;
const FPS_STEP: u32 = 10;
const SCALE_STEP: u32 = 25;
const BITRATE_FLOOR_DIVISOR: NonZeroU32 =
    NonZeroU32::new(2).expect("bitrate floor divisor must be nonzero");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Streak {
    None,
    Bad(u8),
    Good(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QualityChange {
    pub(super) old: QualityLevels,
    pub(super) new: QualityLevels,
}

/// Bad samples move down one ladder level; good samples move up one. Changes
/// are rate-limited, and the first change may happen immediately.
pub(super) struct Quality {
    levels: QualityLevels,
    max_bitrate: Kbps,
    max_fps: Fps,
    streak: Streak,
    last_change: Option<Instant>,
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
            streak: Streak::None,
            last_change: None,
        }
    }

    pub(super) fn levels(&self) -> QualityLevels {
        self.levels
    }

    pub(super) fn update(&mut self, feedback: &Feedback, now: Instant) -> Option<QualityChange> {
        if feedback.received() == 0 {
            self.streak = Streak::None;
            return None;
        }
        let queue_pressure = feedback.queue_busy_ms() / feedback.sample_ms();
        let is_bad =
            queue_pressure >= 0.10 || feedback.queue_peak() >= 24 || feedback.rtt() > 250.0;
        let is_good = queue_pressure < 0.05
            && feedback.queue_peak() < 24
            && feedback.dropped() == 0
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
        match self.streak {
            Streak::Bad(count) if count >= BAD_STREAK_THRESHOLD => {
                self.lower_one_level();
                self.streak = Streak::None;
            }
            Streak::Good(count) if count >= GOOD_STREAK_THRESHOLD => {
                self.raise_one_level();
                self.streak = Streak::None;
            }
            Streak::None | Streak::Bad(_) | Streak::Good(_) => return None,
        }
        if self.levels == old {
            return None;
        }
        self.last_change = Some(now);
        Some(QualityChange {
            old,
            new: self.levels,
        })
    }

    fn lower_one_level(&mut self) {
        let minimum_bitrate = self
            .max_bitrate
            .divided_by(BITRATE_FLOOR_DIVISOR)
            .at_least(BITRATE_FLOOR);
        if self.levels.bitrate > minimum_bitrate {
            self.levels.bitrate = self
                .levels
                .bitrate
                .scaled(DOWN_STEP_PERCENT)
                .at_least(minimum_bitrate);
        } else if self.levels.fps > FPS_FLOOR {
            self.levels.fps = self.levels.fps.lowered_by(FPS_STEP, FPS_FLOOR);
        } else if self.levels.scale > SCALE_FLOOR {
            self.levels.scale = self.levels.scale.lowered_by(SCALE_STEP, SCALE_FLOOR);
        }
    }

    fn raise_one_level(&mut self) {
        if self.levels.bitrate < self.max_bitrate {
            self.levels.bitrate = self
                .levels
                .bitrate
                .scaled(UP_STEP_PERCENT)
                .at_most(self.max_bitrate);
        } else if self.levels.scale < FULL_SCALE {
            self.levels.scale = self.levels.scale.raised_by(SCALE_STEP, FULL_SCALE);
        } else if self.levels.fps < self.max_fps {
            self.levels.fps = self.levels.fps.raised_by(FPS_STEP, self.max_fps);
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
                    bitrate: value(Kbps::new(6_400)),
                    fps: value(Fps::new(60)),
                    scale: value(ScalePercent::new(100)),
                },
            }
        );
    }

    #[test]
    fn bad_ladder_changes_one_level_at_a_time_and_stops_at_every_floor() {
        let mut quality = Quality::new(value(Kbps::new(1_000)), value(Fps::new(30)));
        let mut now = Instant::now();
        let expected = [
            (800, 30, 100),
            (640, 30, 100),
            (512, 30, 100),
            (500, 30, 100),
            (500, 20, 100),
            (500, 20, 75),
            (500, 20, 50),
        ];

        for (bitrate, frame_rate, scale) in expected {
            let change = trigger_bad(&mut quality, now)
                .expect("bad streak should move down one ladder level");
            assert_eq!(
                change.new,
                QualityLevels {
                    bitrate: value(Kbps::new(bitrate)),
                    fps: value(Fps::new(frame_rate)),
                    scale: value(ScalePercent::new(scale)),
                }
            );
            now += QUALITY_CHANGE_COOLDOWN;
        }

        assert_eq!(trigger_bad(&mut quality, now), None);
        assert_eq!(
            quality.levels(),
            QualityLevels {
                bitrate: value(Kbps::new(500)),
                fps: value(Fps::new(20)),
                scale: value(ScalePercent::new(50)),
            }
        );
    }

    #[test]
    fn bitrate_floor_is_three_hundred_when_half_the_maximum_is_lower() {
        let mut quality = Quality::new(value(Kbps::new(500)), value(Fps::new(20)));
        let mut now = Instant::now();

        for expected in [400, 320, 300] {
            let change = trigger_bad(&mut quality, now)
                .expect("bad streak should lower bitrate to its floor");
            assert_eq!(change.new.bitrate, value(Kbps::new(expected)));
            now += QUALITY_CHANGE_COOLDOWN;
        }

        let change =
            trigger_bad(&mut quality, now).expect("bitrate floor should expose the scale ladder");
        assert_eq!(change.new.bitrate, value(Kbps::new(300)));
        assert_eq!(change.new.scale, value(ScalePercent::new(75)));
    }

    #[test]
    fn good_ladder_restores_bitrate_then_scale_then_fps() {
        let mut quality = Quality::new(value(Kbps::new(1_000)), value(Fps::new(40)));
        quality.levels = QualityLevels {
            bitrate: value(Kbps::new(500)),
            fps: value(Fps::new(20)),
            scale: value(ScalePercent::new(50)),
        };
        let mut now = Instant::now();
        let expected = [
            (550, 20, 50),
            (605, 20, 50),
            (665, 20, 50),
            (731, 20, 50),
            (804, 20, 50),
            (884, 20, 50),
            (972, 20, 50),
            (1_000, 20, 50),
            (1_000, 20, 75),
            (1_000, 20, 100),
            (1_000, 30, 100),
            (1_000, 40, 100),
        ];

        for (bitrate, frame_rate, scale) in expected {
            let change = trigger_good(&mut quality, now)
                .expect("good streak should move up one ladder level");
            assert_eq!(
                change.new,
                QualityLevels {
                    bitrate: value(Kbps::new(bitrate)),
                    fps: value(Fps::new(frame_rate)),
                    scale: value(ScalePercent::new(scale)),
                }
            );
            now += QUALITY_CHANGE_COOLDOWN;
        }

        assert_eq!(trigger_good(&mut quality, now), None);
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
        assert_eq!(quality.levels().bitrate, value(Kbps::new(5_120)));
    }

    #[test]
    fn idle_and_mixed_samples_clear_the_streak() {
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let now = Instant::now();
        assert_eq!(quality.update(&bad_feedback(), now), None);
        assert_eq!(
            quality.update(&feedback(0, 0, 64, 1000.0, 10_000, 60_000.0), now),
            None
        );
        assert_eq!(quality.update(&bad_feedback(), now), None);
        assert_eq!(
            quality.update(&feedback(50, 50, 0, 0.0, 1, 20.0), now),
            None
        );
        assert_eq!(quality.update(&bad_feedback(), now), None);
        assert_eq!(quality.levels().bitrate, value(Kbps::new(8_000)));
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
