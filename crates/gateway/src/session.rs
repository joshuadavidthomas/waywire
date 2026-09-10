use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use sprite_desktop_protocol::browser::Feedback;
#[cfg(test)]
use sprite_desktop_protocol::browser::FeedbackValues;
use sprite_desktop_protocol::browser::QualityLevels;
use sprite_desktop_protocol::pipe::Command;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::Kbps;
use sprite_desktop_protocol::pipe::Quality as QualityCommand;
use sprite_desktop_protocol::pipe::ReleaseAll;
use sprite_desktop_protocol::pipe::ScalePercent;
use tokio::sync::Mutex as AsyncMutex;
use tracing::info;

use crate::daemon::CommandSink;
use crate::daemon::lock;

const QUALITY_CHANGE_COOLDOWN: Duration = Duration::from_secs(5);
const BAD_STREAK_THRESHOLD: u8 = 2;
const GOOD_STREAK_THRESHOLD: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LeaseEpoch(u64);

impl LeaseEpoch {
    pub(crate) const FIRST: Self = Self(1);

    #[must_use]
    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn next(self) -> Self {
        Self(self.0.wrapping_add(1).max(Self::FIRST.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SocketId(u64);

impl SocketId {
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
pub(crate) struct Sessions {
    commands: CommandSink,
    lease: Arc<AsyncMutex<LeaseBook>>,
    quality: Arc<Mutex<Quality>>,
}

struct LeaseBook {
    owner: Option<Lease>,
    next_epoch: LeaseEpoch,
}

#[derive(Clone, Copy)]
struct Lease {
    socket: SocketId,
    epoch: LeaseEpoch,
}

impl Sessions {
    pub(crate) fn new(commands: CommandSink, bitrate: Kbps, fps: Fps) -> Self {
        Self {
            commands,
            lease: Arc::new(AsyncMutex::new(LeaseBook {
                owner: None,
                next_epoch: LeaseEpoch::FIRST,
            })),
            quality: Arc::new(Mutex::new(Quality::new(bitrate, fps))),
        }
    }

    pub(crate) async fn acquire(&self, socket: SocketId) -> Result<LeaseState> {
        let mut book = self.lease.lock().await;
        if let Some(owner) = book.owner {
            return Ok(if owner.socket == socket {
                LeaseState::Active
            } else {
                LeaseState::Busy
            });
        }

        // ReleaseAll enters the FIFO before the new epoch becomes valid, so
        // no command from the new owner can overtake the reset.
        self.commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await?;
        let epoch = book.next_epoch;
        book.next_epoch = epoch.next();
        book.owner = Some(Lease { socket, epoch });
        self.commands.set_active_lease(epoch);
        info!(?socket, ?epoch, "input lease acquired");
        Ok(LeaseState::Active)
    }

    pub(crate) async fn release(&self, socket: SocketId) -> Result<()> {
        let mut book = self.lease.lock().await;
        let Some(owner) = book.owner else {
            return Ok(());
        };
        if owner.socket != socket {
            return Ok(());
        }

        // The reset follows every command already queued by this epoch. Once
        // the authority is cleared, the writer drops any late stale command.
        self.commands
            .system(Command::ReleaseAll(ReleaseAll))
            .await?;
        self.commands.clear_active_lease();
        book.owner = None;
        info!(socket = ?owner.socket, epoch = ?owner.epoch, "input lease released");
        Ok(())
    }

    async fn epoch(&self, socket: SocketId) -> Option<LeaseEpoch> {
        self.lease
            .lock()
            .await
            .owner
            .filter(|owner| owner.socket == socket)
            .map(|owner| owner.epoch)
    }

    pub(crate) async fn owns(&self, socket: SocketId) -> bool {
        self.epoch(socket).await.is_some()
    }

    pub(crate) async fn input(&self, socket: SocketId, command: Command) -> Result<()> {
        if let Some(epoch) = self.epoch(socket).await {
            self.commands.input(epoch, command).await?;
        }
        Ok(())
    }

    pub(crate) fn quality(&self) -> QualityLevels {
        lock(&self.quality, "session quality").levels()
    }

    pub(crate) async fn feedback(
        &self,
        socket: SocketId,
        feedback: Feedback,
    ) -> Result<Option<QualityLevels>> {
        let book = self.lease.lock().await;
        let Some(owner) = book.owner else {
            return Ok(None);
        };
        if owner.socket != socket {
            return Ok(None);
        }

        let (change, levels) = {
            let mut quality = lock(&self.quality, "session quality");
            let change = quality.update(&feedback, Instant::now());
            (change, quality.levels())
        };
        if let Some(change) = change {
            info!(
                ?socket,
                old = ?change.old,
                new = ?change.new,
                "quality levels changed"
            );
            // Quality is daemon state, not user input. Holding the lease lock
            // puts this command before a release and any later acquisition.
            self.commands
                .system(Command::Quality(QualityCommand {
                    bitrate_kbps: change.new.bitrate,
                    fps: change.new.fps,
                    scale_percent: change.new.scale,
                }))
                .await?;
        }

        Ok(Some(levels))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseState {
    Active,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Streak {
    None,
    Bad(u8),
    Good(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QualityChange {
    old: QualityLevels,
    new: QualityLevels,
}

/// Bad samples have at least 10% queue pressure, a queue peak of 24, or RTT
/// above 250 ms. Two bad samples in a row cut bitrate by 20% to half its
/// configured maximum (never below 300 Kbps), then cut FPS by 10 to 20, then
/// scale by 25 points to 50%. Good samples have under 5% queue pressure, a
/// queue peak under 24, no drops, RTT under 120 ms, and present at least 90% of
/// received frames. Eight good samples in a row raise bitrate by 10% to its
/// configured maximum, then scale by 25 points, then FPS by 10. Each change
/// moves one ladder level. Changes are at least five seconds apart; the first
/// change may happen immediately.
struct Quality {
    levels: QualityLevels,
    max_bitrate: Kbps,
    max_fps: Fps,
    streak: Streak,
    last_change: Option<Instant>,
}

impl Quality {
    fn new(bitrate: Kbps, fps: Fps) -> Self {
        Self {
            levels: QualityLevels {
                bitrate,
                fps,
                scale: scale_percent(100),
            },
            max_bitrate: bitrate,
            max_fps: fps,
            streak: Streak::None,
            last_change: None,
        }
    }

    fn levels(&self) -> QualityLevels {
        self.levels
    }

    fn update(&mut self, feedback: &Feedback, now: Instant) -> Option<QualityChange> {
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
        let minimum_bitrate = self.max_bitrate.get().saturating_div(2).max(300);
        if self.levels.bitrate.get() > minimum_bitrate {
            let bitrate = (self.levels.bitrate.get() * 80 / 100).max(minimum_bitrate);
            self.levels.bitrate = kbps(bitrate);
        } else if self.levels.fps.get() > 20 {
            self.levels.fps = fps(self.levels.fps.get().saturating_sub(10).max(20));
        } else if self.levels.scale.get() > 50 {
            self.levels.scale = scale_percent(self.levels.scale.get().saturating_sub(25).max(50));
        }
    }

    fn raise_one_level(&mut self) {
        if self.levels.bitrate.get() < self.max_bitrate.get() {
            let bitrate = (self.levels.bitrate.get() * 110 / 100).min(self.max_bitrate.get());
            self.levels.bitrate = kbps(bitrate);
        } else if self.levels.scale.get() < 100 {
            self.levels.scale = scale_percent((self.levels.scale.get() + 25).min(100));
        } else if self.levels.fps.get() < self.max_fps.get() {
            self.levels.fps = fps((self.levels.fps.get() + 10).min(self.max_fps.get()));
        }
    }
}

fn kbps(value: u32) -> Kbps {
    match Kbps::new(value) {
        Ok(bitrate) => bitrate,
        Err(error) => panic!("quality produced invalid bitrate: {error}"),
    }
}

fn fps(value: u32) -> Fps {
    match Fps::new(value) {
        Ok(frame_rate) => frame_rate,
        Err(error) => panic!("quality produced invalid frame rate: {error}"),
    }
}

fn scale_percent(value: u32) -> ScalePercent {
    match ScalePercent::new(value) {
        Ok(scale) => scale,
        Err(error) => panic!("quality produced invalid scale: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feedback(
        received: u32,
        presented: u32,
        queue_peak: u32,
        queue_busy_ms: f64,
        dropped: u32,
        rtt: f64,
    ) -> Feedback {
        Feedback::new(FeedbackValues {
            received,
            presented,
            queue_peak,
            queue_busy_ms,
            sample_ms: 1000.0,
            dropped,
            rtt,
        })
        .expect("test feedback should be valid")
    }

    fn bad_feedback() -> Feedback {
        feedback(50, 50, 5, 200.0, 0, 20.0)
    }

    fn good_feedback() -> Feedback {
        feedback(50, 45, 0, 0.0, 0, 20.0)
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
        let mut quality = Quality::new(kbps(8_000), fps(60));
        let now = Instant::now();

        let change = trigger_bad(&mut quality, now)
            .expect("second bad sample should change quality immediately");

        assert_eq!(
            change,
            QualityChange {
                old: QualityLevels {
                    bitrate: kbps(8_000),
                    fps: fps(60),
                    scale: scale_percent(100),
                },
                new: QualityLevels {
                    bitrate: kbps(6_400),
                    fps: fps(60),
                    scale: scale_percent(100),
                },
            }
        );
    }

    #[test]
    fn bad_ladder_changes_one_level_at_a_time_and_stops_at_every_floor() {
        let mut quality = Quality::new(kbps(1_000), fps(30));
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
                    bitrate: kbps(bitrate),
                    fps: fps(frame_rate),
                    scale: scale_percent(scale),
                }
            );
            now += QUALITY_CHANGE_COOLDOWN;
        }

        assert_eq!(trigger_bad(&mut quality, now), None);
        assert_eq!(
            quality.levels(),
            QualityLevels {
                bitrate: kbps(500),
                fps: fps(20),
                scale: scale_percent(50),
            }
        );
    }

    #[test]
    fn bitrate_floor_is_three_hundred_when_half_the_maximum_is_lower() {
        let mut quality = Quality::new(kbps(500), fps(20));
        let mut now = Instant::now();

        for expected in [400, 320, 300] {
            let change = trigger_bad(&mut quality, now)
                .expect("bad streak should lower bitrate to its floor");
            assert_eq!(change.new.bitrate, kbps(expected));
            now += QUALITY_CHANGE_COOLDOWN;
        }

        let change =
            trigger_bad(&mut quality, now).expect("bitrate floor should expose the scale ladder");
        assert_eq!(change.new.bitrate, kbps(300));
        assert_eq!(change.new.scale, scale_percent(75));
    }

    #[test]
    fn good_ladder_restores_bitrate_then_scale_then_fps() {
        let mut quality = Quality::new(kbps(1_000), fps(40));
        quality.levels = QualityLevels {
            bitrate: kbps(500),
            fps: fps(20),
            scale: scale_percent(50),
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
                    bitrate: kbps(bitrate),
                    fps: fps(frame_rate),
                    scale: scale_percent(scale),
                }
            );
            now += QUALITY_CHANGE_COOLDOWN;
        }

        assert_eq!(trigger_good(&mut quality, now), None);
    }

    #[test]
    fn cooldown_spaces_changes_by_five_seconds() {
        let mut quality = Quality::new(kbps(8_000), fps(60));
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
        assert_eq!(quality.levels().bitrate, kbps(5_120));
    }

    #[test]
    fn idle_and_mixed_samples_clear_the_streak() {
        let mut quality = Quality::new(kbps(8_000), fps(60));
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
        assert_eq!(quality.levels().bitrate, kbps(8_000));
    }

    #[test]
    fn high_rtt_counts_as_bad() {
        let mut quality = Quality::new(kbps(8_000), fps(60));
        let now = Instant::now();
        let high_rtt = feedback(50, 50, 0, 0.0, 0, 251.0);

        assert_eq!(quality.update(&high_rtt, now), None);
        assert!(quality.update(&high_rtt, now).is_some());
    }
}
