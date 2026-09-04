use crate::{
    daemon::{CommandSink, RuntimeState},
    protocol::{BrowserCommand, DaemonCommand, Feedback},
};
use anyhow::Result;
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone)]
pub struct Sessions {
    state: RuntimeState,
    commands: CommandSink,
    lease: Arc<AsyncMutex<LeaseBook>>,
    quality: Arc<Mutex<Quality>>,
}
struct LeaseBook {
    owner: Option<Lease>,
    next_epoch: u64,
}
#[derive(Clone, Copy)]
struct Lease {
    socket: u64,
    epoch: u64,
}
impl Sessions {
    pub fn new(state: RuntimeState, commands: CommandSink, bitrate: u32, fps: u32) -> Self {
        Self {
            state,
            commands,
            lease: Arc::new(AsyncMutex::new(LeaseBook {
                owner: None,
                next_epoch: 1,
            })),
            quality: Arc::new(Mutex::new(Quality::new(bitrate, fps))),
        }
    }
    pub async fn acquire(&self, socket: u64) -> Result<LeaseState> {
        let mut book = self.lease.lock().await;
        if let Some(owner) = book.owner {
            return Ok(if owner.socket == socket {
                LeaseState::Active
            } else {
                LeaseState::Busy
            });
        }
        self.commands.system(DaemonCommand::ReleaseAll).await?;
        let epoch = book.next_epoch;
        book.next_epoch = book.next_epoch.wrapping_add(1).max(1);
        book.owner = Some(Lease { socket, epoch });
        self.state.active_lease.store(epoch, Ordering::Release);
        Ok(LeaseState::Active)
    }
    pub async fn release(&self, socket: u64) -> Result<()> {
        let mut book = self.lease.lock().await;
        let Some(owner) = book.owner else {
            return Ok(());
        };
        if owner.socket != socket {
            return Ok(());
        }
        self.commands.system(DaemonCommand::ReleaseAll).await?;
        self.state.active_lease.store(0, Ordering::Release);
        book.owner = None;
        Ok(())
    }
    async fn epoch(&self, socket: u64) -> Option<u64> {
        self.lease
            .lock()
            .await
            .owner
            .filter(|owner| owner.socket == socket)
            .map(|owner| owner.epoch)
    }
    pub async fn owns(&self, socket: u64) -> bool {
        self.epoch(socket).await.is_some()
    }
    pub async fn input(&self, socket: u64, command: BrowserCommand) -> Result<()> {
        if let Some(epoch) = self.epoch(socket).await {
            self.commands
                .input(epoch, DaemonCommand::Browser(command))
                .await?;
        }
        Ok(())
    }
    pub async fn text(
        &self,
        socket: u64,
        preedit: bool,
        text: String,
        sequence: u32,
    ) -> Result<()> {
        if let Some(epoch) = self.epoch(socket).await {
            self.commands
                .input(
                    epoch,
                    DaemonCommand::Text {
                        preedit,
                        text,
                        sequence,
                    },
                )
                .await?;
        }
        Ok(())
    }
    pub async fn clipboard(&self, socket: u64, text: String) -> Result<()> {
        if let Some(epoch) = self.epoch(socket).await {
            self.commands
                .input(epoch, DaemonCommand::Clipboard(text))
                .await?;
        }
        Ok(())
    }
    pub fn quality(&self) -> (u32, u32, u32) {
        self.quality.lock().unwrap().values()
    }
    pub async fn feedback(
        &self,
        socket: u64,
        feedback: Feedback,
    ) -> Result<Option<(u32, u32, u32)>> {
        let Some(epoch) = self.epoch(socket).await else {
            return Ok(None);
        };
        let command = {
            let mut quality = self.quality.lock().unwrap();
            quality
                .update(&feedback, Instant::now())
                .then(|| DaemonCommand::Quality {
                    bitrate: quality.bitrate,
                    fps: quality.fps,
                    scale: quality.scale,
                })
        };
        if let Some(command) = command {
            self.commands.input(epoch, command).await?;
        }
        Ok(Some(self.quality()))
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Active,
    Busy,
}
struct Quality {
    bitrate: u32,
    fps: u32,
    scale: u32,
    max_bitrate: u32,
    max_fps: u32,
    bad: u8,
    good: u8,
    last: Instant,
}
impl Quality {
    fn new(bitrate: u32, fps: u32) -> Self {
        Self {
            bitrate,
            fps,
            scale: 100,
            max_bitrate: bitrate,
            max_fps: fps,
            bad: 0,
            good: 0,
            last: Instant::now() - Duration::from_secs(6),
        }
    }
    fn values(&self) -> (u32, u32, u32) {
        (self.bitrate, self.fps, self.scale)
    }
    fn update(&mut self, feedback: &Feedback, now: Instant) -> bool {
        if feedback.received == 0 {
            self.bad = 0;
            self.good = 0;
            return false;
        }
        let queue_pressure = feedback.queue_busy_ms / feedback.sample_ms;
        let bad = queue_pressure >= 0.10 || feedback.queue_peak >= 24 || feedback.rtt > 250.0;
        let good = queue_pressure < 0.05
            && feedback.queue_peak < 24
            && feedback.dropped == 0
            && feedback.rtt < 120.0
            && u64::from(feedback.presented) * 10 >= u64::from(feedback.received) * 9;
        if bad {
            self.bad = self.bad.saturating_add(1);
            self.good = 0;
        } else if good {
            self.good = self.good.saturating_add(1);
            self.bad = 0;
        } else {
            self.bad = 0;
            self.good = 0;
        }
        if now.duration_since(self.last) < Duration::from_secs(5) {
            return false;
        }
        let before = self.values();
        if self.bad >= 2 {
            let minimum = self.max_bitrate.saturating_div(2).max(300);
            if self.bitrate > minimum {
                self.bitrate = (self.bitrate * 80 / 100).max(minimum);
            } else if self.fps > 20 {
                self.fps -= 10;
            } else if self.scale > 50 {
                self.scale -= 25;
            }
            self.bad = 0;
        } else if self.good >= 8 {
            self.bitrate = (self.bitrate * 110 / 100).min(self.max_bitrate);
            if self.scale < 100 {
                self.scale += 25;
            } else if self.fps < self.max_fps {
                self.fps = (self.fps + 10).min(self.max_fps);
            }
            self.good = 0;
        }
        let changed = self.values() != before;
        if changed {
            self.last = now;
        }
        changed
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
        Feedback {
            received,
            presented,
            queue_peak,
            queue_busy_ms,
            sample_ms: 1000.0,
            dropped,
            rtt,
        }
    }

    #[test]
    fn fifty_fps_source_downshifts_then_recovers_without_sixty_fps_target() {
        let mut quality = Quality::new(8000, 60);
        let now = Instant::now();
        assert!(!quality.update(&feedback(50, 50, 5, 200.0, 0, 20.0), now));
        assert!(quality.update(&feedback(50, 50, 5, 200.0, 0, 20.0), now));
        assert_eq!(quality.values(), (6400, 60, 100));
        quality.last = now - Duration::from_secs(6);
        for _ in 0..7 {
            assert!(!quality.update(&feedback(50, 45, 0, 0.0, 0, 20.0), now));
        }
        assert!(quality.update(&feedback(50, 45, 0, 0.0, 0, 20.0), now));
        assert_eq!(quality.values(), (7040, 60, 100));
    }

    #[test]
    fn idle_and_isolated_short_bursts_do_not_adapt() {
        let mut quality = Quality::new(8000, 60);
        let now = Instant::now();
        for _ in 0..10 {
            assert!(!quality.update(&feedback(0, 0, 64, 1000.0, 10_000, 60000.0), now));
        }
        for _ in 0..2 {
            assert!(!quality.update(&feedback(50, 50, 12, 20.0, 0, 20.0), now));
        }
        assert!(!quality.update(&feedback(50, 50, 0, 0.0, 1, 20.0), now));
        assert_eq!(quality.values(), (8000, 60, 100));
    }

    #[test]
    fn sustained_queue_pressure_and_hysteresis_require_streaks_and_change_spacing() {
        let mut quality = Quality::new(8000, 60);
        let now = Instant::now();
        assert!(!quality.update(&feedback(50, 50, 5, 200.0, 0, 20.0), now));
        assert!(quality.update(&feedback(50, 50, 5, 200.0, 0, 20.0), now));
        for _ in 0..2 {
            assert!(!quality.update(&feedback(50, 50, 24, 0.0, 0, 20.0), now));
        }
        assert_eq!(quality.values(), (6400, 60, 100));
        let later = now + Duration::from_secs(5);
        assert!(quality.update(&feedback(50, 50, 24, 0.0, 0, 20.0), later));
        assert_eq!(quality.values(), (5120, 60, 100));
    }

    #[test]
    fn high_rtt_still_triggers_congestion() {
        let mut quality = Quality::new(8000, 60);
        let now = Instant::now();
        assert!(!quality.update(&feedback(50, 50, 0, 0.0, 0, 251.0), now));
        assert!(quality.update(&feedback(50, 50, 0, 0.0, 0, 251.0), now));
    }
}
