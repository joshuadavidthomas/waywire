use std::time::Duration;
use std::time::Instant;

use tracing::debug;
use tracing::debug_span;
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
const BROWSER_BAD_DROP_PERCENT: u64 = 10;
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
// x264 ultrafast (with CABAC) needs about 4 more CRF than superfast for the
// same bitrate and PSNR on desktop text; these values keep the old budgets.
const AUTOMATIC_CRF: Crf = match Crf::new(27) {
    Ok(value) => value,
    Err(_) => panic!("AUTOMATIC_CRF is outside the CRF range"),
};
const HIGH_CRF: Crf = match Crf::new(22) {
    Ok(value) => value,
    Err(_) => panic!("HIGH_CRF is outside the CRF range"),
};
const MEDIUM_CRF: Crf = match Crf::new(32) {
    Ok(value) => value,
    Err(_) => panic!("MEDIUM_CRF is outside the CRF range"),
};
const LOW_CRF: Crf = match Crf::new(36) {
    Ok(value) => value,
    Err(_) => panic!("LOW_CRF is outside the CRF range"),
};
const DOWN_STEP_PERCENT: u8 = 80;
// The bitrate floor is 300 kbps, so recovery doubles: ten percent steps would take
// minutes to reach a ceiling 40 times higher.
const UP_STEP_PERCENT: u8 = 200;
// A slow link is measured, not searched for. Its buffers are already full when
// pressure shows, so the first target is half of what the viewer received: the
// backlog then drains at the same rate the video uses, and a measurement that
// over-reads the link by a large message still lands below it. Recovery returns
// to LINK_HOLD_PERCENT of the measured rate, which leaves room for key frames,
// still-screen refreshes and rate-control overshoot.
const LINK_DRAIN_PERCENT: u32 = 50;
// Video the encoder produced and the viewer has not received waits in buffers
// along the link, and every later frame waits behind it. More than half a second
// of what the link carries is a backlog. The byte minimum keeps one large key
// frame on either side of a sample boundary from counting.
const LINK_BACKLOG_MILLIS: u64 = 500;
const LINK_BACKLOG_MIN_BYTES: u64 = 16 << 10;
// One second can end with a large message still in flight and read a fraction of
// the link; two seconds of backlog smooth that out.
const LINK_MEASURE_MILLIS: u64 = 2000;
const LINK_HOLD_PERCENT: u32 = 85;
// Above the held rate a raise is a probe of a link that was too slow. Probes wait
// longer than ordinary recovery and back off after each failure, so a link that
// stays slow settles instead of saw-toothing.
const LINK_PROBE_PERCENT: u8 = 125;
const LINK_PROBE_PATIENCE: u8 = 15;
const LINK_PROBE_PATIENCE_LIMIT: u8 = 60;
const FPS_STEP: u32 = 10;
const SCALE_STEP: u32 = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Streak {
    None,
    Bad(u8),
    Good(u8),
}

impl Streak {
    fn next(self, verdict: SampleVerdict) -> Self {
        match (verdict, self) {
            (SampleVerdict::Bad, Self::Bad(count)) => Self::Bad(count.saturating_add(1)),
            (SampleVerdict::Bad, Self::None | Self::Good(_)) => Self::Bad(1),
            (SampleVerdict::Good, Self::Good(count)) => Self::Good(count.saturating_add(1)),
            (SampleVerdict::Good, Self::None | Self::Bad(_)) => Self::Good(1),
            (SampleVerdict::Neutral, _) => Self::None,
        }
    }
}

/// What one feedback sample says about quality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleVerdict {
    /// Something counts against quality.
    Bad,
    /// Nothing counts against quality and nothing vetoes recovery.
    Good,
    /// Nothing counts against quality, but something vetoes recovery.
    Neutral,
}

/// Whether the encoder kept up during a sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EncoderHealth {
    /// At least `ENCODER_MIN_RENDERED` frames, and at least
    /// `ENCODER_BAD_DROP_PERCENT` of them replaced before submission.
    Overloaded,
    /// At most `GOOD_DROP_PERCENT` replaced, including a sample with no frames.
    Healthy,
    /// Too few frames to call overloaded, or between the two thresholds.
    Inconclusive,
}

/// Frames the encoder attempted and replaced since the last sample.
#[derive(Clone, Copy, Debug)]
struct EncoderSample {
    attempted: u64,
    replaced: u64,
}

impl EncoderSample {
    fn health(self) -> EncoderHealth {
        let replaced = u128::from(self.replaced) * 100;
        let attempted = u128::from(self.attempted);
        if self.attempted >= ENCODER_MIN_RENDERED
            && replaced >= attempted * u128::from(ENCODER_BAD_DROP_PERCENT)
        {
            EncoderHealth::Overloaded
        } else if replaced <= attempted * u128::from(GOOD_DROP_PERCENT) {
            EncoderHealth::Healthy
        } else {
            EncoderHealth::Inconclusive
        }
    }
}

/// What a submitted frame did to the encoder's baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Baseline {
    /// The first frame, a new generation or a backwards sequence: earlier samples
    /// say nothing about this run.
    Restarted,
    /// Counted, or still warming up.
    Continued,
}

#[derive(Clone, Default)]
struct EncoderPressure {
    previous: Option<(Generation, u64, u64)>,
    warmup_until: u64,
    rendered: u64,
    dropped: u64,
}

impl EncoderPressure {
    fn observe(&mut self, metadata: &FrameMetadata) -> Baseline {
        let previous = self.previous.replace((
            metadata.generation,
            metadata.sequence,
            metadata.capture_nanos,
        ));
        let Some((generation, sequence, captured)) = previous else {
            self.warmup_until = metadata.capture_nanos.saturating_add(ENCODER_WARMUP_NANOS);
            return Baseline::Restarted;
        };
        if generation != metadata.generation || metadata.sequence <= sequence {
            self.rendered = 0;
            self.dropped = 0;
            self.warmup_until = metadata.capture_nanos.saturating_add(ENCODER_WARMUP_NANOS);
            return Baseline::Restarted;
        }
        // The first second can replace frames while ffmpeg starts. Keep moving
        // the baseline, so neither startup nor generation-discarded frames count.
        if captured <= self.warmup_until {
            return Baseline::Continued;
        }
        let rendered = metadata.sequence - sequence;
        self.rendered = self.rendered.saturating_add(rendered);
        self.dropped = self.dropped.saturating_add(rendered - 1);
        Baseline::Continued
    }

    fn take(&mut self) -> EncoderSample {
        EncoderSample {
            attempted: std::mem::take(&mut self.rendered),
            replaced: std::mem::take(&mut self.dropped),
        }
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

/// What the viewer received over some time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Delivery {
    bytes: u64,
    millis: u64,
}

impl Delivery {
    fn kbps(self) -> Option<Kbps> {
        (self.millis >= LINK_MEASURE_MILLIS).then(|| {
            Kbps::saturating(u32::try_from(self.bytes * 8 / self.millis).unwrap_or(u32::MAX))
        })
    }
}

/// Frames the viewer lost in a sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loss {
    /// Less than `BROWSER_BAD_DROP_PERCENT`.
    None,
    /// At least `BROWSER_BAD_DROP_PERCENT` while little or no video waited for
    /// the link.
    Browser,
    /// At least `BROWSER_BAD_DROP_PERCENT` while video waited for the link: the
    /// gateway discarded what the link could not carry.
    Link,
}

/// What the viewer received against what the encoder produced in a sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    /// More arrived than was produced: the link is emptying its queue.
    Draining,
    Balanced,
    /// More was produced than arrived.
    Saturated,
}

/// One feedback sample's view of the viewer's link.
enum LinkSample {
    /// No video arrived and none waits for the link.
    Idle { delivered: u64 },
    /// Video arrived, or some waited for the link.
    Active(ActiveLink),
}

struct ActiveLink {
    /// More than `LINK_BACKLOG_MILLIS` of video waits for the link.
    backlog: bool,
    loss: Loss,
    flow: Flow,
    delivered: u64,
    /// What the link carries, once congestion has lasted long enough to tell.
    measured: Option<Kbps>,
}

impl LinkSample {
    fn delivered(&self) -> u64 {
        match self {
            Self::Idle { delivered } | Self::Active(ActiveLink { delivered, .. }) => *delivered,
        }
    }

    fn backlog(&self) -> bool {
        matches!(self, Self::Active(ActiveLink { backlog: true, .. }))
    }

    fn loss(&self) -> Loss {
        match self {
            Self::Idle { .. } => Loss::None,
            Self::Active(link) => link.loss,
        }
    }

    fn measured(&self) -> Option<Kbps> {
        match self {
            Self::Idle { .. } => None,
            Self::Active(link) => link.measured,
        }
    }
}

/// How a streak of pressure lowers quality.
#[derive(Clone, Copy)]
enum Lowering {
    /// A probe above the held rate failed: drain what it queued, then hold.
    FailedProbe,
    /// Meet the measured link.
    ToLink(Kbps),
    /// One step down the ladder, for pressure the link does not explain.
    Step,
    /// Wait until the link is measured.
    Measuring,
}

/// What the viewer's link carried when it last limited the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinkLimit {
    delivered: Kbps,
    patience: u8,
}

impl LinkLimit {
    fn held(self) -> Kbps {
        Kbps::saturating(self.delivered.get() * LINK_HOLD_PERCENT / 100)
    }

    fn drain(self) -> Kbps {
        Kbps::saturating(self.delivered.get() * LINK_DRAIN_PERCENT / 100)
    }
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
            bitrate_floor: Kbps::MINIMUM,
            bitrate_ceiling,
            fps_floor: max_fps.min(FPS_FLOOR),
            fps_ceiling: max_fps,
            scale_floor: SCALE_FLOOR,
            scale_ceiling: FULL_SCALE,
            crf,
        }
    }
}

/// Something in a sample that counts against quality or blocks its recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    EncoderReplacement,
    DecoderBusy,
    DecoderPeak,
    Rtt,
    BrowserLoss,
    LinkLoss,
    LinkBacklog,
    NoPresentation,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::EncoderReplacement => "encoder-replacement",
            Self::DecoderBusy => "decoder-busy",
            Self::DecoderPeak => "decoder-peak",
            Self::Rtt => "rtt",
            Self::BrowserLoss => "browser-loss",
            Self::LinkLoss => "link-loss",
            Self::LinkBacklog => "link-backlog",
            Self::NoPresentation => "no-presentation",
        }
    }

    /// The viewer's link, not the encoder or decoder, explains this reason.
    fn is_link(self) -> bool {
        match self {
            Self::LinkLoss | Self::LinkBacklog => true,
            Self::EncoderReplacement
            | Self::DecoderBusy
            | Self::DecoderPeak
            | Self::Rtt
            | Self::BrowserLoss
            | Self::NoPresentation => false,
        }
    }
}

/// The reasons that apply, in order.
fn applying<const N: usize>(candidates: [(bool, Reason); N]) -> Vec<Reason> {
    candidates
        .into_iter()
        .filter_map(|(applies, reason)| applies.then_some(reason))
        .collect()
}

/// What counts against quality in a sample, and what blocks its recovery.
fn signals(
    feedback: &Feedback,
    link: &LinkSample,
    encoder: EncoderHealth,
) -> (Vec<Reason>, Vec<Reason>) {
    let queue_pressure = feedback.queue_busy_ms() / feedback.sample_ms();
    let active = matches!(link, LinkSample::Active(_));
    let pressure = applying([
        (
            encoder == EncoderHealth::Overloaded,
            Reason::EncoderReplacement,
        ),
        (active && queue_pressure >= 0.10, Reason::DecoderBusy),
        (active && feedback.queue_peak() >= 24, Reason::DecoderPeak),
        (active && feedback.rtt() > 250.0, Reason::Rtt),
        (link.loss() == Loss::Browser, Reason::BrowserLoss),
        (link.loss() == Loss::Link, Reason::LinkLoss),
        (link.backlog(), Reason::LinkBacklog),
    ]);
    // An isolated drop need not mean ongoing congestion. Allow up to 2% loss
    // so one dropped frame does not erase an otherwise healthy recovery streak.
    // Feedback excludes newest-due presentation skips. A 60Hz display may
    // legitimately draw half a 120 FPS stream; zero draws cannot recover.
    let recovery_vetoes = applying([
        (
            encoder != EncoderHealth::Healthy,
            Reason::EncoderReplacement,
        ),
        (queue_pressure >= 0.05, Reason::DecoderBusy),
        (feedback.queue_peak() >= 24, Reason::DecoderPeak),
        (
            u64::from(feedback.dropped()) * 100
                > u64::from(feedback.received()) * GOOD_DROP_PERCENT,
            Reason::BrowserLoss,
        ),
        (feedback.rtt() >= 120.0, Reason::Rtt),
        (link.backlog(), Reason::LinkBacklog),
        (feedback.presented() == 0, Reason::NoPresentation),
    ]);
    (pressure, recovery_vetoes)
}

/// Bad samples move down one ladder level; good samples move up one. Changes
/// are rate-limited, and the first change may happen immediately.
#[derive(Clone)]
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
    /// The encoder's health in the last sample, if one was taken since its
    /// baseline restarted.
    last_encoder: Option<EncoderHealth>,
    /// What the viewer received while video backed up behind its link.
    congestion: Option<Delivery>,
    link: Option<LinkLimit>,
    /// Encoded bytes since the last feedback. The viewer also counts each frame's
    /// header, about 40 bytes, so the queue estimate errs low.
    produced: u64,
    /// Encoded bytes the viewer has not yet received.
    queued: u64,
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
            chroma: Chroma::Yuv420,
            preset: QualityPreset::Automatic,
            streak: Streak::None,
            last_change: None,
            encoder: EncoderPressure::default(),
            last_encoder: None,
            congestion: None,
            link: None,
            produced: 0,
            queued: 0,
        }
    }

    pub(super) fn levels(&self) -> QualityLevels {
        self.levels
    }

    pub(super) fn reset_samples(&mut self) {
        self.encoder = EncoderPressure::default();
        self.last_encoder = None;
        self.streak = Streak::None;
        // Another viewer's link says nothing about this one.
        self.congestion = None;
        self.link = None;
        self.produced = 0;
        self.queued = 0;
    }

    pub(super) fn produced(&mut self, bytes: usize) {
        self.produced = self.produced.saturating_add(bytes as u64);
    }

    pub(super) fn submitted(&mut self, metadata: &FrameMetadata) {
        if self.encoder.observe(metadata) == Baseline::Restarted {
            self.streak = Streak::None;
            self.last_encoder = None;
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
        let sample = self.encoder.take();
        let encoder = sample.health();
        let last_encoder = self.last_encoder.replace(encoder);
        let link = self.observe_link(feedback);
        let (pressure, recovery_vetoes) = signals(feedback, &link, encoder);
        let cooldown = self.last_change.map_or(Duration::ZERO, |last| {
            QUALITY_CHANGE_COOLDOWN.saturating_sub(now.saturating_duration_since(last))
        });
        let _sample = debug_span!(
            "quality_sample",
            generation = ?self.encoder.previous.map(|(generation, _, _)| generation),
            capture_nanos = ?self.encoder.previous.map(|(_, _, captured)| captured),
            warmup_until = self.encoder.warmup_until,
            attempted = sample.attempted,
            submitted = sample.attempted - sample.replaced,
            replaced = sample.replaced,
            sufficient_encoder_sample = sample.attempted >= ENCODER_MIN_RENDERED,
            ?feedback,
            bad_reasons = ?pressure.iter().copied().map(Reason::as_str).collect::<Vec<_>>(),
            recovery_vetoes = ?recovery_vetoes.iter().copied().map(Reason::as_str).collect::<Vec<_>>(),
            streak_before = ?self.streak,
            delivered = link.delivered(),
            queued = self.queued,
            link = ?self.link,
            levels = ?self.levels,
            cooldown_ms = cooldown.as_millis(),
        )
        .entered();
        // The stream is damage-driven: absence of frames says nothing about its
        // health. Idle feedback preserves recovery, but interrupts producer overload.
        if matches!(link, LinkSample::Idle { .. }) && encoder != EncoderHealth::Overloaded {
            if last_encoder == Some(EncoderHealth::Overloaded) {
                self.streak = Streak::None;
            }
            debug!(decision = "idle", streak_after = ?self.streak, "quality decision");
            return None;
        }
        // After a jump the link still carries video queued before it. Its late and
        // lost frames say nothing about the new levels, so wait until it empties.
        if self.link.is_some()
            && matches!(
                link,
                LinkSample::Active(ActiveLink {
                    flow: Flow::Draining,
                    ..
                })
            )
            && encoder != EncoderHealth::Overloaded
        {
            debug!(decision = "draining", streak_after = ?self.streak, "quality decision");
            return None;
        }
        let local_pressure = pressure.iter().any(|reason| !reason.is_link());
        let verdict = if !pressure.is_empty() {
            SampleVerdict::Bad
        } else if recovery_vetoes.is_empty() {
            SampleVerdict::Good
        } else {
            SampleVerdict::Neutral
        };
        self.streak = self.streak.next(verdict);
        let streak_lowers =
            matches!(self.streak, Streak::Bad(count) if count >= BAD_STREAK_THRESHOLD);
        let lowering = self.lowering(streak_lowers, &link, local_pressure);
        // A link measured too slow is met at once, and so is a failed probe: waiting
        // out the cooldown would only queue more video behind it.
        let link_jump = matches!(lowering, Some(Lowering::FailedProbe | Lowering::ToLink(_)));
        if !cooldown.is_zero() && !link_jump {
            debug!(decision = "cooldown", streak_after = ?self.streak, "quality decision");
            return None;
        }
        let old = self.levels;
        let old_chroma = self.chroma;
        let direction = match (lowering, self.streak) {
            (Some(Lowering::Measuring), _) => {
                debug!(decision = "measuring", streak_after = ?self.streak, "quality decision");
                return None;
            }
            (Some(lowering), _) => {
                self.lower(lowering);
                "lower"
            }
            (None, Streak::Good(count)) if count >= self.raise_patience() => {
                self.raise_one_level();
                self.streak = Streak::None;
                "raise"
            }
            (None, Streak::None | Streak::Bad(_) | Streak::Good(_)) => {
                debug!(decision = "hold", streak_after = ?self.streak, "quality decision");
                return None;
            }
        };
        if self.levels == old && self.chroma == old_chroma {
            debug!(decision = "bound", direction, streak_after = ?self.streak, "quality decision");
            return None;
        }
        debug!(decision = direction, streak_after = ?self.streak, new = ?self.levels, "quality decision");
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

    /// Account for what the encoder produced and the viewer received since the last
    /// sample.
    fn observe_link(&mut self, feedback: &Feedback) -> LinkSample {
        let produced = std::mem::take(&mut self.produced);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "feedback validates sampleMs between 0 and 60,000"
        )]
        let delivered = Delivery {
            bytes: u64::from(feedback.bytes()),
            millis: (feedback.sample_ms().round() as u64).max(1),
        };
        let queued = (self.queued + produced).saturating_sub(delivered.bytes);
        // Frames the gateway discards for a slow link will never arrive; the viewer
        // reports them as lost.
        self.queued = if feedback.dropped() > 0 { 0 } else { queued };
        // A second without video is idle only when none is waiting: otherwise the
        // link stalled.
        if feedback.received() == 0 && self.queued < LINK_BACKLOG_MIN_BYTES {
            self.congestion = None;
            return LinkSample::Idle {
                delivered: delivered.bytes,
            };
        }
        let flow = if delivered.bytes >= produced.saturating_add(LINK_BACKLOG_MIN_BYTES) {
            Flow::Draining
        } else if produced >= delivered.bytes.saturating_add(LINK_BACKLOG_MIN_BYTES) {
            Flow::Saturated
        } else {
            Flow::Balanced
        };
        // A probe tests a link already measured, by a small overshoot that queues
        // slowly; waiting for half a second of backlog would let it build for many
        // seconds. Any steady queue past the byte minimum fails it.
        let backlog_bytes = if self.probing() {
            LINK_BACKLOG_MIN_BYTES
        } else {
            LINK_BACKLOG_MIN_BYTES.max(delivered.bytes * LINK_BACKLOG_MILLIS / delivered.millis)
        };
        // Once congested, a link stays congested while less arrives than was
        // produced, even after the gateway discards its queue.
        let backlog =
            self.queued >= backlog_bytes || self.congestion.is_some() && flow == Flow::Saturated;
        let loss = if feedback.received() == 0
            || u64::from(feedback.dropped()) * 100
                < u64::from(feedback.received()) * BROWSER_BAD_DROP_PERCENT
        {
            Loss::None
        } else if queued >= LINK_BACKLOG_MIN_BYTES {
            Loss::Link
        } else {
            Loss::Browser
        };
        // Congestion means the link, not the encoder or decoder, sets the rate, and
        // what arrives while it lasts measures the link, more exactly the longer it
        // lasts: the viewer counts each message when its last byte arrives. The
        // first second can include video sent before the link slowed, so it only
        // starts the measurement.
        self.congestion = (backlog || loss == Loss::Link).then(|| {
            self.congestion
                .map_or(Delivery::default(), |congestion| Delivery {
                    bytes: congestion.bytes + delivered.bytes,
                    millis: congestion.millis + delivered.millis,
                })
        });
        LinkSample::Active(ActiveLink {
            backlog,
            loss,
            flow,
            delivered: delivered.bytes,
            measured: self.congestion.and_then(Delivery::kbps),
        })
    }

    fn lowering(
        &self,
        streak_lowers: bool,
        sample: &LinkSample,
        local_pressure: bool,
    ) -> Option<Lowering> {
        // A restart's key frame alone can back the link up for a second, but the
        // gateway discards video only when the link cannot keep up.
        if self.probing() && !local_pressure && (streak_lowers || sample.loss() == Loss::Link) {
            return Some(Lowering::FailedProbe);
        }
        if !streak_lowers {
            return None;
        }
        // Congestion that lasts while it is measured shows the link carries less
        // than the encoder produces, whatever the target says.
        Some(match sample.measured() {
            Some(delivered) => Lowering::ToLink(delivered),
            None if local_pressure => Lowering::Step,
            None => Lowering::Measuring,
        })
    }

    fn probing(&self) -> bool {
        self.link
            .is_some_and(|link| self.levels.bitrate > link.held())
    }

    fn lower(&mut self, lowering: Lowering) {
        match lowering {
            Lowering::FailedProbe => self.fail_probe(),
            Lowering::ToLink(delivered) => self.lower_to_link(delivered),
            Lowering::Step | Lowering::Measuring => self.lower_one_level(),
        }
        // Video at the new levels is measured afresh.
        self.congestion = None;
        self.streak = Streak::None;
    }

    /// The link still carries what it did. The probe's overshoot queued video that
    /// the held rate, just under the link, would take many seconds to drain, so
    /// drain it as the first jump does; recovery returns to the held rate. The next
    /// probe waits twice as long.
    fn fail_probe(&mut self) {
        let floor = self.bounds().bitrate_floor;
        if let Some(link) = self.link.as_mut() {
            self.levels.bitrate = link.drain().at_least(floor).at_most(self.levels.bitrate);
            link.patience = link
                .patience
                .saturating_mul(2)
                .min(LINK_PROBE_PATIENCE_LIMIT);
        }
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

    /// Jump to the measured link instead of stepping down to it.
    fn lower_to_link(&mut self, delivered: Kbps) {
        let bounds = self.bounds();
        // A link that just became slow also gets fewer pixels, so its few bits per
        // pixel still draw legible text. Later jumps change only the bitrate.
        let patience = if let Some(link) = self.link {
            link.patience
        } else {
            self.levels.scale = self.levels.scale.lowered_by(SCALE_STEP, bounds.scale_floor);
            LINK_PROBE_PATIENCE
        };
        let link = LinkLimit {
            delivered,
            patience,
        };
        self.link = Some(link);
        self.levels.bitrate = link
            .drain()
            .at_least(bounds.bitrate_floor)
            .at_most(self.levels.bitrate);
    }

    /// A probe waits at the held rate, longer after each failure. Its verdict waits
    /// `LINK_PROBE_PATIENCE`: a small overshoot takes that long to fill the gateway's
    /// queue for the viewer.
    fn raise_patience(&self) -> u8 {
        match self.link {
            Some(link) if self.levels.fps >= self.bounds().fps_ceiling => {
                match self.levels.bitrate.cmp(&link.held()) {
                    std::cmp::Ordering::Greater => LINK_PROBE_PATIENCE,
                    std::cmp::Ordering::Equal => link.patience,
                    std::cmp::Ordering::Less => GOOD_STREAK_THRESHOLD,
                }
            }
            Some(_) | None => GOOD_STREAK_THRESHOLD,
        }
    }

    // Keep YUV420 through adaptation and recovery for browser compatibility.
    // Every encoder configuration change restarts it. Lower scale before bitrate
    // and frame rate, retaining the existing YUV adaptation order.
    // Seamless bitrate changes need an in-process encoder rather than a child
    // process with fixed arguments; that must change before treating bitrate as free.
    fn lower_one_level(&mut self) {
        let bounds = self.bounds();
        // Scale changes only the encoder frame, never the Wayland output.
        if self.levels.scale > bounds.scale_floor {
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
            let raised = match self.link {
                // Return to the held rate the link already carried.
                Some(link) if self.levels.bitrate < link.held() => self
                    .levels
                    .bitrate
                    .scaled(UP_STEP_PERCENT)
                    .at_most(link.held()),
                // The probe held for a whole recovery streak, so the link cleared.
                Some(link) if self.levels.bitrate > link.delivered => {
                    self.link = None;
                    self.levels.bitrate.scaled(UP_STEP_PERCENT)
                }
                Some(_) => self.levels.bitrate.scaled(LINK_PROBE_PERCENT),
                None => self.levels.bitrate.scaled(UP_STEP_PERCENT),
            };
            self.levels.bitrate = raised.at_most(bounds.bitrate_ceiling);
        } else if self.levels.scale < bounds.scale_ceiling {
            self.levels.scale = self
                .levels
                .scale
                .raised_by(SCALE_STEP, bounds.scale_ceiling);
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

    #[test]
    fn diagnostics_preserve_decisions_and_explain_pressure_recovery_and_idle() {
        #[derive(Clone, Default)]
        struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Log {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .map_err(|error| std::io::Error::other(error.to_string()))?
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
            let mut metadata = warmed_encoder(&mut quality);
            let now = Instant::now();
            let overloaded = feedback(50, 20, 24, 200.0, 10, 251.0);
            encoder_sample(&mut quality, &mut metadata, 30, 30);
            assert_eq!(quality.update(&overloaded, now), None);
            encoder_sample(&mut quality, &mut metadata, 30, 30);
            assert_eq!(
                quality
                    .update(&overloaded, now)
                    .expect("downshift")
                    .new
                    .scale
                    .get(),
                75
            );
            assert_eq!(
                quality.update(
                    &feedback(50, 50, 0, 0.0, 0, 120.0),
                    now + Duration::from_secs(1)
                ),
                None
            );
            let recovered_at = now + QUALITY_CHANGE_COOLDOWN;
            assert_eq!(quality.update(&good_feedback(), recovered_at), None);
            assert_eq!(
                quality.update(&feedback(0, 0, 0, 0.0, 0, 20.0), recovered_at),
                None
            );
            assert_eq!(quality.streak, Streak::Good(1));
            for _ in 0..6 {
                assert_eq!(quality.update(&good_feedback(), recovered_at), None);
            }
            assert_eq!(
                quality
                    .update(&good_feedback(), recovered_at)
                    .expect("recovery")
                    .new
                    .scale
                    .get(),
                100
            );
            assert_eq!(
                trigger_good(&mut quality, recovered_at + QUALITY_CHANGE_COOLDOWN),
                None
            );
        });
        let output = String::from_utf8(log.0.lock().expect("log lock").clone()).expect("UTF-8 log");
        let lines: Vec<_> = output.lines().collect();
        assert!(lines[0].contains("attempted=60 submitted=30 replaced=30"));
        assert!(lines[0].contains("sufficient_encoder_sample=true"));
        assert!(lines[0].contains("bad_reasons=[\"encoder-replacement\", \"decoder-busy\", \"decoder-peak\", \"rtt\", \"browser-loss\"]"));
        assert!(lines[1].contains("decision=\"lower\""));
        assert!(lines[2].contains("bad_reasons=[] recovery_vetoes=[\"rtt\"]"));
        assert!(lines[2].contains("cooldown_ms=4000"));
        assert!(lines[2].contains("decision=\"cooldown\" streak_after=None"));
        assert!(lines[4].contains("decision=\"idle\" streak_after=Good(1)"));
        assert!(lines.iter().any(|line| line.contains("decision=\"raise\"")));
        assert!(
            lines
                .last()
                .expect("bound log")
                .contains("decision=\"bound\" direction=\"raise\"")
        );
    }

    fn warmed_encoder(quality: &mut Quality) -> FrameMetadata {
        let mut metadata = FrameMetadata {
            generation: value(Generation::new(1)),
            sequence: 1,
            capture_nanos: 0,
            width: value(waywire_protocol::pipe::FrameDimension::new(1280)),
            height: value(waywire_protocol::pipe::FrameDimension::new(720)),
            input_sequence: None,
            fps: value(Fps::new(60)),
            chroma: Chroma::Yuv420,
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
                    fps: value(Fps::new(60)),
                    scale: value(ScalePercent::new(if changes { 75 } else { 100 })),
                }
            );
            assert_eq!(quality.chroma, Chroma::Yuv420);
        }
    }

    #[test]
    fn encoder_and_browser_pressure_use_the_same_yuv420_ladder() {
        for sample in [good_feedback(), bad_feedback()] {
            let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
            let mut metadata = warmed_encoder(&mut quality);
            let mut now = Instant::now();
            for (scale, bitrate) in [(75, 8_000), (50, 8_000), (50, 6_400)] {
                for _ in 0..2 {
                    encoder_sample(&mut quality, &mut metadata, 30, 30);
                    quality.update(&sample, now);
                }
                assert_eq!(quality.levels().fps.get(), 60);
                assert_eq!(quality.levels().scale.get(), scale);
                assert_eq!(quality.levels().bitrate.get(), bitrate);
                assert_eq!(quality.chroma, Chroma::Yuv420);
                now += QUALITY_CHANGE_COOLDOWN;
            }
        }
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
        assert_eq!(quality.chroma, Chroma::Yuv420);
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
                .scale,
            value(ScalePercent::new(75))
        );
        assert_eq!(quality.chroma, Chroma::Yuv420);
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
                .expect("lower encoded scale under sustained pressure")
                .new
                .scale,
            value(ScalePercent::new(75))
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
        assert_eq!(change.new.scale, value(ScalePercent::new(50)));
        assert_eq!(change.new.bitrate, value(Kbps::new(8_000)));
        assert_eq!(change.new.fps, value(Fps::new(60)));
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
                .scale,
            value(ScalePercent::new(75))
        );
        assert_eq!(quality.chroma, Chroma::Yuv420);
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
    fn recovery_requires_drawing_and_low_loss_not_one_draw_per_received_frame() {
        for (received, presented, dropped, recovers) in [
            (90, 60, 0, true),
            (120, 60, 0, true),
            (120, 30, 0, true),
            (120, 60, 2, true),
            (120, 60, 3, false),
            (120, 0, 0, false),
        ] {
            let mut quality = Quality::new(value(Kbps::new(16_000)), value(Fps::new(120)));
            quality.levels.fps = value(Fps::new(90));
            let sample = feedback(received, presented, 1, 0.0, dropped, 20.0);
            let now = Instant::now();
            for _ in 0..7 {
                assert_eq!(quality.update(&sample, now), None);
            }
            assert_eq!(
                quality.update(&sample, now).is_some(),
                recovers,
                "received={received}, presented={presented}, dropped={dropped}"
            );
        }
    }

    #[test]
    fn sustained_capacity_loss_is_pressure_even_with_an_empty_decoder_queue() {
        for (dropped, changes) in [(11, false), (12, true), (120, true)] {
            let mut quality = Quality::new(value(Kbps::new(16_000)), value(Fps::new(120)));
            let sample = feedback(120, 0, 0, 0.0, dropped, 20.0);
            let now = Instant::now();
            assert_eq!(quality.update(&sample, now), None);
            assert_eq!(quality.update(&sample, now).is_some(), changes);
        }
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
                    scale: value(ScalePercent::new(75)),
                },
                old_crf: AUTOMATIC_CRF,
                new_crf: AUTOMATIC_CRF,
                old_chroma: Chroma::Yuv420,
                new_chroma: Chroma::Yuv420,
            }
        );
    }

    #[test]
    fn bad_ladder_changes_one_level_at_a_time_and_stops_at_every_floor() {
        let mut quality = Quality::new(value(Kbps::new(1_000)), value(Fps::new(30)));
        let mut now = Instant::now();
        let expected = [
            (1_000, 30, 75, Chroma::Yuv420),
            (1_000, 30, 50, Chroma::Yuv420),
            (800, 30, 50, Chroma::Yuv420),
            (640, 30, 50, Chroma::Yuv420),
            (512, 30, 50, Chroma::Yuv420),
            (409, 30, 50, Chroma::Yuv420),
            (327, 30, 50, Chroma::Yuv420),
            (300, 30, 50, Chroma::Yuv420),
            (300, 20, 50, Chroma::Yuv420),
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
                    bitrate: value(Kbps::new(300)),
                    fps: value(Fps::new(20)),
                    scale: value(ScalePercent::new(50)),
                },
                Chroma::Yuv420,
            )
        );
    }

    #[test]
    fn bitrate_floor_is_three_hundred() {
        let mut quality = Quality::new(value(Kbps::new(500)), value(Fps::new(20)));
        let mut now = Instant::now();

        for expected_scale in [75, 50] {
            let change = trigger_bad(&mut quality, now)
                .expect("scale steps should precede the bitrate floor");
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
        let mut now = Instant::now();
        let expected = [
            (500, 30, 50, Chroma::Yuv420),
            (500, 40, 50, Chroma::Yuv420),
            (1_000, 40, 50, Chroma::Yuv420),
            (1_000, 40, 75, Chroma::Yuv420),
            (1_000, 40, 100, Chroma::Yuv420),
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
            (QualityPreset::Automatic, (300, 8_000, 20, 60, 50, 100, 27)),
            (QualityPreset::High, (300, 8_000, 20, 60, 50, 100, 22)),
            (QualityPreset::Medium, (300, 4_000, 20, 60, 50, 100, 32)),
            (QualityPreset::Low, (300, 2_000, 20, 60, 50, 100, 36)),
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
            assert_eq!(quality.chroma, Chroma::Yuv420);
            quality.select_preset(preset);
            let bounds = quality.bounds();
            let mut now = Instant::now();

            while let Some(change) = trigger_bad(&mut quality, now) {
                assert_eq!(change.new_chroma, Chroma::Yuv420);
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
                assert_eq!(change.new_chroma, Chroma::Yuv420);
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
            assert_eq!(quality.chroma, Chroma::Yuv420);
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
        trigger_bad(&mut quality, now).expect("bad streak should lower scale first");
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
        assert_eq!(quality.levels().scale, value(ScalePercent::new(50)));
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
        assert_eq!(quality.chroma, Chroma::Yuv420);
        assert_eq!(quality.levels().scale, FULL_SCALE);
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
        assert_eq!(quality.chroma, Chroma::Yuv420);
        assert_eq!(quality.levels().scale, FULL_SCALE);
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
            assert_eq!(quality.chroma, Chroma::Yuv420);
            assert_eq!(quality.levels().scale, FULL_SCALE);
        }
    }

    #[test]
    fn drop_rates_above_two_percent_discard_recovery_progress() {
        for (received, dropped, streak) in [
            (49, 1, Streak::None),
            (100, 3, Streak::None),
            (1, 1, Streak::Bad(1)),
        ] {
            let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
            let now = Instant::now();
            assert!(trigger_bad(&mut quality, now).is_some());
            let now = now + QUALITY_CHANGE_COOLDOWN;
            for _ in 1..GOOD_STREAK_THRESHOLD {
                assert_eq!(quality.update(&good_feedback(), now), None);
            }
            let lossy = feedback(received, received - dropped, 0, 0.0, dropped, 20.0);
            assert_eq!(quality.update(&lossy, now), None);
            assert_eq!(quality.streak, streak);
            assert_eq!(quality.update(&good_feedback(), now), None);
            assert_eq!(quality.chroma, Chroma::Yuv420);
        }
    }

    /// A link that carries `kbps` a second, or anything when it is `None`, and
    /// queues the rest.
    #[derive(Default)]
    struct Link {
        kbps: Option<u32>,
        queued: u32,
    }

    impl Link {
        /// One second of video that fills its bitrate target. Nothing is late or
        /// lost yet: only the bytes show the link.
        fn second(&mut self, quality: &mut Quality) -> Feedback {
            let produced = quality.levels().bitrate.get() * 125;
            quality.produced(produced as usize);
            self.queued += produced;
            let delivered = self
                .kbps
                .map_or(self.queued, |kbps| self.queued.min(kbps * 125));
            self.queued -= delivered;
            // The gateway holds about half a second of frames for a viewer, then
            // discards them and the viewer reports them lost.
            let dropped = if self.queued > produced / 2 {
                self.queued = 0;
                15
            } else {
                0
            };
            Feedback::new(waywire_protocol::browser::FeedbackValues {
                received: 30,
                presented: 30 - dropped,
                queue_peak: 0,
                queue_busy_ms: 0.0,
                sample_ms: 1000.0,
                dropped,
                rtt: 20.0,
                bytes: delivered,
            })
            .expect("link feedback should be valid")
        }
    }

    /// Each second's change as (second, kbps, fps, scale).
    fn drive(
        quality: &mut Quality,
        link: &mut Link,
        start: Instant,
        seconds: std::ops::RangeInclusive<u64>,
    ) -> Vec<(u64, u32, u32, u32)> {
        seconds
            .filter_map(|second| {
                let sample = link.second(quality);
                let change = quality.update(&sample, start + Duration::from_secs(second))?;
                Some((
                    second,
                    change.new.bitrate.get(),
                    change.new.fps.get(),
                    change.new.scale.get(),
                ))
            })
            .collect()
    }

    #[test]
    fn a_slow_link_is_met_at_once_held_steadily_and_left_when_it_clears() {
        let mut quality = Quality::new(value(Kbps::new(12_000)), value(Fps::new(30)));
        let mut link = Link::default();
        let start = Instant::now();

        // Half of the 500 kbps it carried is below the floor, then 85% of it holds.
        // A probe above it fails when the gateway discards video for the link. It
        // drains what the probe queued at the first jump's rate, then holds again;
        // the next probe waits twice as long.
        assert_eq!(
            {
                link.kbps = Some(500);
                drive(&mut quality, &mut link, start, 1..=60)
            },
            [
                (3, 300, 30, 75),
                (11, 425, 30, 75),
                (26, 531, 30, 75),
                (32, 300, 30, 75),
                (41, 425, 30, 75),
            ]
        );
        // Once the link clears, the waiting probe holds and recovery doubles.
        assert_eq!(
            {
                link.kbps = None;
                drive(&mut quality, &mut link, start, 61..=200)
            },
            [
                (71, 531, 30, 75),
                (86, 1_062, 30, 75),
                (94, 2_124, 30, 75),
                (102, 4_248, 30, 75),
                (110, 8_496, 30, 75),
                (118, 12_000, 30, 75),
                (126, 12_000, 30, 100),
            ]
        );
    }

    #[test]
    fn a_link_that_stays_slow_is_probed_less_and_less_often() {
        let mut quality = Quality::new(value(Kbps::new(12_000)), value(Fps::new(30)));
        let mut link = Link {
            kbps: Some(500),
            queued: 0,
        };
        let probes: Vec<_> = drive(&mut quality, &mut link, Instant::now(), 1..=400)
            .into_iter()
            .filter(|&(_, kbps, _, _)| kbps > 500)
            .map(|(second, ..)| second)
            .collect();
        assert_eq!(probes, [26, 71, 146, 221, 296, 371]);
        assert_eq!(quality.levels().bitrate, value(Kbps::new(425)));
    }

    #[test]
    fn a_new_input_owner_does_not_inherit_the_last_owners_link() {
        let mut quality = Quality::new(value(Kbps::new(12_000)), value(Fps::new(30)));
        let start = Instant::now();
        let mut link = Link {
            kbps: Some(500),
            queued: 0,
        };
        drive(&mut quality, &mut link, start, 1..=11);
        assert_eq!(quality.levels().bitrate, value(Kbps::new(425)));
        quality.reset_samples();
        // The new owner's link is clear, so recovery doubles without probing.
        let mut clear = Link::default();
        assert_eq!(
            drive(&mut quality, &mut clear, start, 12..=27),
            [(19, 850, 30, 75), (27, 1_700, 30, 75)]
        );
    }

    #[test]
    fn a_new_slow_link_is_measured_again() {
        let mut quality = Quality::new(value(Kbps::new(12_000)), value(Fps::new(30)));
        let start = Instant::now();
        let mut link = Link {
            kbps: Some(2_000),
            queued: 0,
        };
        drive(&mut quality, &mut link, start, 1..=20);
        assert_eq!(quality.levels().bitrate, value(Kbps::new(1_700)));
        // The link narrows further: jump to half of what it now carries.
        assert_eq!(
            {
                link.kbps = Some(800);
                drive(&mut quality, &mut link, start, 21..=31)
            },
            [(23, 400, 30, 75), (31, 680, 30, 75)]
        );
    }

    #[test]
    fn a_backlog_is_measured_over_two_seconds_after_its_first_and_then_drains_quietly() {
        let sample = |bytes, dropped| {
            Feedback::new(waywire_protocol::browser::FeedbackValues {
                received: 30,
                presented: 30,
                queue_peak: 0,
                queue_busy_ms: 0.0,
                sample_ms: 1000.0,
                dropped,
                rtt: 20.0,
                bytes,
            })
            .expect("link feedback should be valid")
        };
        let mut quality = Quality::new(value(Kbps::new(12_000)), value(Fps::new(30)));
        let now = Instant::now();
        // The link slows to 1 Mbps during the first congested second, which still
        // carries 2 Mbps of the 4 Mbps produced. Later seconds count whole messages
        // as they complete: one second alone would read 1.6 Mbps or 400 kbps.
        quality.produced(500_000);
        assert_eq!(quality.update(&sample(250_000, 0), now), None);
        quality.produced(500_000);
        assert_eq!(quality.update(&sample(200_000, 0), now), None);
        quality.produced(500_000);
        let change = quality
            .update(&sample(50_000, 0), now)
            .expect("two measured seconds jump");
        assert_eq!(change.new.bitrate, value(Kbps::new(500)));
        assert_eq!(change.new.scale, value(ScalePercent::new(75)));
        // The link then carries video queued before the jump faster than the encoder
        // produces it, and frames the gateway dropped then show up as losses.
        for second in 1..=12 {
            quality.produced(62_500);
            assert_eq!(
                quality.update(&sample(125_000, 10), now + Duration::from_secs(second)),
                None
            );
        }
        // A two-second burst above what the link carries is no reason to lower.
        for second in 13..=14 {
            quality.produced(100_000);
            assert_eq!(
                quality.update(&sample(70_000, 0), now + Duration::from_secs(second)),
                None
            );
        }
        assert_eq!(quality.levels().bitrate, value(Kbps::new(500)));
        assert_eq!(quality.levels().scale, value(ScalePercent::new(75)));
    }

    #[test]
    fn local_pressure_steps_down_even_when_little_was_delivered() {
        // A busy decoder on a quiet screen: the low delivery rate is the screen,
        // not the link, so the bitrate target keeps its value.
        let mut quality = Quality::new(value(Kbps::new(8_000)), value(Fps::new(60)));
        let busy = Feedback::new(waywire_protocol::browser::FeedbackValues {
            received: 50,
            presented: 50,
            queue_peak: 5,
            queue_busy_ms: 200.0,
            sample_ms: 1000.0,
            dropped: 0,
            rtt: 20.0,
            bytes: 12_500,
        })
        .expect("decoder feedback should be valid");
        let now = Instant::now();
        assert_eq!(quality.update(&busy, now), None);
        let change = quality.update(&busy, now).expect("decoder pressure lowers");
        assert_eq!(change.new.bitrate, value(Kbps::new(8_000)));
        assert_eq!(change.new.scale, value(ScalePercent::new(75)));
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
