use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use wayland_client::QueueHandle;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_head_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameSize;
use waywire_protocol::pipe::MAX_RAW_PIXELS;
use waywire_protocol::pipe::RequestId;
use waywire_protocol::pipe::ScaleV120;

use super::State;

const DISPLAY_RESET_DIMENSION_DELTA: u32 = 2;
const MAX_RESIZE_STEP_FAILURES: u8 = 3;
const OUTPUT_CONFIGURATION_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputSize {
    width: u32,
    height: u32,
}

impl OutputSize {
    pub(crate) fn new(width: u32, height: u32) -> Option<Self> {
        (width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_RAW_PIXELS)
            .then_some(Self { width, height })
    }

    fn external(size: FrameSize) -> Self {
        Self {
            width: size.width(),
            height: size.height(),
        }
    }

    fn display_reset_bounce(self) -> Result<Self, DisplayResetRefusal> {
        let Some(width) = self.width.checked_sub(DISPLAY_RESET_DIMENSION_DELTA) else {
            return Err(DisplayResetRefusal::OutputTooSmall);
        };
        let Some(height) = self.height.checked_sub(DISPLAY_RESET_DIMENSION_DELTA) else {
            return Err(DisplayResetRefusal::OutputTooSmall);
        };
        Self::new(width, height).ok_or(DisplayResetRefusal::OutputTooSmall)
    }

    pub(crate) fn external_size(self) -> Result<FrameSize> {
        FrameSize::new(self.width, self.height).map_err(anyhow::Error::new)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentMode {
    Unknown,
    Known(OutputSize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScaleChange {
    Keep,
    Set(ScaleV120),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputMode {
    size: OutputSize,
    scale: ScaleChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestPurpose {
    External {
        request_id: RequestId,
        scale_v120: ScaleV120,
        failures: u8,
    },
    DisplayResetBounce {
        original: OutputSize,
        failures: u8,
    },
    DisplayResetRestore {
        failures: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizeRequest {
    mode: OutputMode,
    purpose: RequestPurpose,
}

impl ResizeRequest {
    pub(crate) const fn operation(self) -> ResizeOperation {
        match self.purpose {
            RequestPurpose::External { .. } => ResizeOperation::External,
            RequestPurpose::DisplayResetBounce { .. }
            | RequestPurpose::DisplayResetRestore { .. } => ResizeOperation::DisplayReset,
        }
    }
}

impl ResizeRequest {
    fn external(size: FrameSize, scale_v120: ScaleV120, request_id: RequestId) -> Self {
        Self {
            mode: OutputMode {
                size: OutputSize::external(size),
                scale: ScaleChange::Set(scale_v120),
            },
            purpose: RequestPurpose::External {
                request_id,
                scale_v120,
                failures: 0,
            },
        }
    }
}

pub(crate) struct Head {
    pub(crate) proxy: zwlr_output_head_v1::ZwlrOutputHeadV1,
    pub(crate) name: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) finished: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManagerSerial(u32);

impl ManagerSerial {
    pub(crate) const fn new(serial: u32) -> Self {
        Self(serial)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SerialPublication {
    Recorded,
    RetryReady,
}

pub(crate) struct PendingResize {
    pub(crate) configuration: zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
    request: ResizeRequest,
    serial: ManagerSerial,
    dimension_change: DimensionChange,
    submitted_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryReadiness {
    Ready,
    AwaitingFreshSerial {
        stale: ManagerSerial,
        since: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueuedResize {
    request: ResizeRequest,
    readiness: RetryReadiness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayResetSequence {
    Idle,
    AwaitingBounce {
        original: OutputSize,
        bounce: OutputSize,
    },
    BounceInFlight,
    AwaitingRestore {
        original: OutputSize,
    },
    RestoreInFlight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayResetStart {
    Start,
    AlreadyRunning,
    Refused(DisplayResetRefusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayResetRefusal {
    CurrentModeUnknown,
    OutputTooSmall,
    CompositorRejected,
    CompositorCancelled,
    CompositorTimedOut,
    OutputUnavailable,
}

impl DisplayResetRefusal {
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::CurrentModeUnknown => "current output dimensions are not known yet",
            Self::OutputTooSmall => "current output dimensions are too small to bounce",
            Self::CompositorRejected => "compositor rejected the display reset output mode",
            Self::CompositorCancelled => "compositor cancelled the display reset output mode",
            Self::CompositorTimedOut => "compositor did not finish the display reset output mode",
            Self::OutputUnavailable => "output configuration is unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DimensionChange {
    Changed,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppliedResize {
    External {
        size: OutputSize,
        scale_v120: ScaleV120,
        request_id: RequestId,
        dimension_change: DimensionChange,
    },
    DisplayResetStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResizeOperation {
    External,
    DisplayReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectOutcome {
    SupersededExternal,
    Retrying {
        operation: ResizeOperation,
        failures: u8,
    },
    Exhausted {
        operation: ResizeOperation,
        failures: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputTimeout {
    Configuration { operation: ResizeOperation },
    FreshSerial,
}

fn output_dimensions_changed(current: CurrentMode, next: OutputSize) -> DimensionChange {
    match current {
        CurrentMode::Known(current) if current == next => DimensionChange::Unchanged,
        CurrentMode::Unknown | CurrentMode::Known(_) => DimensionChange::Changed,
    }
}

pub(crate) struct OutputManager {
    pub(crate) manager: Option<zwlr_output_manager_v1::ZwlrOutputManagerV1>,
    pub(crate) heads: Vec<Head>,
    serial: Option<ManagerSerial>,
    pub(crate) pending: Option<PendingResize>,
    queued: Option<QueuedResize>,
    retry: Option<QueuedResize>,
    display_reset: DisplayResetSequence,
    current: CurrentMode,
}

impl OutputManager {
    pub(crate) fn new() -> Self {
        Self {
            manager: None,
            heads: Vec::new(),
            serial: None,
            pending: None,
            queued: None,
            retry: None,
            display_reset: DisplayResetSequence::Idle,
            current: CurrentMode::Unknown,
        }
    }

    pub(crate) fn record_capture_size(&mut self, size: OutputSize) {
        if !self.capture_blocked() {
            self.current = CurrentMode::Known(size);
        }
    }

    pub(crate) fn configure(
        &mut self,
        output_name: Option<&str>,
        size: FrameSize,
        scale_v120: ScaleV120,
        request_id: RequestId,
        fps: Fps,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let request = ResizeRequest::external(size, scale_v120, request_id);
        if self.pending.is_some()
            || self.retry.is_some()
            || !matches!(self.display_reset, DisplayResetSequence::Idle)
            || self.queued.is_some()
        {
            self.queue_external(request);
            return Ok(());
        }
        self.configure_now(output_name, request, fps, qh)
    }

    pub(crate) fn begin_display_reset(&mut self) -> DisplayResetStart {
        if !matches!(self.display_reset, DisplayResetSequence::Idle) {
            return DisplayResetStart::AlreadyRunning;
        }
        let CurrentMode::Known(original) = self.current else {
            return DisplayResetStart::Refused(DisplayResetRefusal::CurrentModeUnknown);
        };
        let bounce = match original.display_reset_bounce() {
            Ok(bounce) => bounce,
            Err(reason) => return DisplayResetStart::Refused(reason),
        };
        self.display_reset = DisplayResetSequence::AwaitingBounce { original, bounce };
        DisplayResetStart::Start
    }

    pub(crate) fn abort_display_reset(&mut self) {
        self.display_reset = DisplayResetSequence::Idle;
        if self.retry.is_some_and(|retry| {
            matches!(
                retry.request.purpose,
                RequestPurpose::DisplayResetBounce { .. }
                    | RequestPurpose::DisplayResetRestore { .. }
            )
        }) {
            self.retry = None;
        }
    }

    pub(crate) fn configure_request(
        &mut self,
        output_name: Option<&str>,
        request: ResizeRequest,
        fps: Fps,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        self.configure_now(output_name, request, fps, qh)
    }

    fn configure_now(
        &mut self,
        output_name: Option<&str>,
        request: ResizeRequest,
        fps: Fps,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let manager = self
            .manager
            .as_ref()
            .context("output management is unavailable")?;
        let serial = self
            .serial
            .context("output manager has not published a serial")?;
        let target_count = self
            .heads
            .iter()
            .filter(|head| {
                !head.finished
                    && head.enabled
                    && output_name.is_none_or(|name| head.name.as_deref() == Some(name))
            })
            .count();
        if target_count != 1 {
            bail!("could not select exactly one enabled output head");
        }

        let width = i32::try_from(request.mode.size.width)
            .context("output width exceeds protocol range")?;
        let height = i32::try_from(request.mode.size.height)
            .context("output height exceeds protocol range")?;
        let refresh = i32::try_from(
            fps.get()
                .checked_mul(1_000)
                .context("output refresh rate overflow")?,
        )
        .context("output refresh rate exceeds protocol range")?;
        let configuration = manager.create_configuration(serial.0, qh, ());
        for head in self.heads.iter().filter(|head| !head.finished) {
            if !head.enabled {
                configuration.disable_head(&head.proxy);
                continue;
            }
            let configured = configuration.enable_head(&head.proxy, qh, ());
            if output_name.is_none_or(|name| head.name.as_deref() == Some(name)) {
                configured.set_custom_mode(width, height, refresh);
                match request.mode.scale {
                    ScaleChange::Keep => {}
                    ScaleChange::Set(scale) => {
                        configured.set_scale(f64::from(scale.get()) / 120.0);
                    }
                }
            }
        }
        configuration.apply();
        self.pending = Some(PendingResize {
            configuration,
            request,
            serial,
            dimension_change: output_dimensions_changed(self.current, request.mode.size),
            submitted_at: Instant::now(),
        });
        Ok(())
    }

    fn queue_external(&mut self, request: ResizeRequest) {
        if let Some(retry) = &mut self.retry {
            match retry.request.purpose {
                RequestPurpose::External { .. } => {
                    retry.request = request;
                    return;
                }
                RequestPurpose::DisplayResetBounce { .. }
                | RequestPurpose::DisplayResetRestore { .. } => {}
            }
        }
        self.queued = Some(QueuedResize {
            request,
            readiness: RetryReadiness::Ready,
        });
    }

    pub(crate) fn take_succeeded(&mut self) -> Option<AppliedResize> {
        let pending = self.pending.take()?;
        pending.configuration.destroy();
        Some(self.accept_succeeded(pending.request, pending.dimension_change))
    }

    fn accept_succeeded(
        &mut self,
        request: ResizeRequest,
        dimension_change: DimensionChange,
    ) -> AppliedResize {
        self.current = CurrentMode::Known(request.mode.size);
        match request.purpose {
            RequestPurpose::External {
                request_id,
                scale_v120,
                ..
            } => {
                if let DisplayResetSequence::AwaitingBounce { .. } = self.display_reset {
                    self.display_reset = match request.mode.size.display_reset_bounce() {
                        Ok(bounce) => DisplayResetSequence::AwaitingBounce {
                            original: request.mode.size,
                            bounce,
                        },
                        Err(
                            DisplayResetRefusal::CurrentModeUnknown
                            | DisplayResetRefusal::OutputTooSmall
                            | DisplayResetRefusal::CompositorRejected
                            | DisplayResetRefusal::CompositorCancelled
                            | DisplayResetRefusal::CompositorTimedOut
                            | DisplayResetRefusal::OutputUnavailable,
                        ) => DisplayResetSequence::Idle,
                    };
                }
                AppliedResize::External {
                    size: request.mode.size,
                    scale_v120,
                    request_id,
                    dimension_change,
                }
            }
            RequestPurpose::DisplayResetBounce { original, .. } => {
                self.display_reset = DisplayResetSequence::AwaitingRestore { original };
                AppliedResize::DisplayResetStep
            }
            RequestPurpose::DisplayResetRestore { .. } => {
                self.display_reset = DisplayResetSequence::Idle;
                AppliedResize::DisplayResetStep
            }
        }
    }

    pub(crate) fn reject_pending(&mut self) -> Option<RejectOutcome> {
        let pending = self.pending.take()?;
        pending.configuration.destroy();
        Some(self.retry_failed_request(pending.request))
    }

    fn retry_failed_request(&mut self, request: ResizeRequest) -> RejectOutcome {
        let (retry, operation, failures) = match request.purpose {
            RequestPurpose::External {
                request_id,
                scale_v120,
                failures,
            } => {
                if self.queued.is_some() {
                    return RejectOutcome::SupersededExternal;
                }
                let failures = failures.saturating_add(1);
                let retry = ResizeRequest {
                    purpose: RequestPurpose::External {
                        request_id,
                        scale_v120,
                        failures,
                    },
                    ..request
                };
                (retry, ResizeOperation::External, failures)
            }
            RequestPurpose::DisplayResetBounce { original, failures } => {
                let failures = failures.saturating_add(1);
                let retry = ResizeRequest {
                    purpose: RequestPurpose::DisplayResetBounce { original, failures },
                    ..request
                };
                (retry, ResizeOperation::DisplayReset, failures)
            }
            RequestPurpose::DisplayResetRestore { failures } => {
                let failures = failures.saturating_add(1);
                let retry = ResizeRequest {
                    purpose: RequestPurpose::DisplayResetRestore { failures },
                    ..request
                };
                (retry, ResizeOperation::DisplayReset, failures)
            }
        };
        if failures >= MAX_RESIZE_STEP_FAILURES {
            if matches!(operation, ResizeOperation::DisplayReset) {
                self.display_reset = DisplayResetSequence::Idle;
            }
            return RejectOutcome::Exhausted {
                operation,
                failures,
            };
        }
        self.retry = Some(QueuedResize {
            request: retry,
            readiness: RetryReadiness::Ready,
        });
        RejectOutcome::Retrying {
            operation,
            failures,
        }
    }

    pub(crate) fn retry_cancelled(&mut self) -> Option<ResizeOperation> {
        let pending = self.pending.take()?;
        pending.configuration.destroy();
        match pending.request.purpose {
            RequestPurpose::External { .. } => {
                let request = self
                    .queued
                    .take()
                    .map_or(pending.request, |queued| queued.request);
                self.retry = Some(QueuedResize {
                    request,
                    readiness: RetryReadiness::AwaitingFreshSerial {
                        stale: pending.serial,
                        since: Instant::now(),
                    },
                });
                Some(ResizeOperation::External)
            }
            RequestPurpose::DisplayResetBounce { .. }
            | RequestPurpose::DisplayResetRestore { .. } => {
                self.display_reset = DisplayResetSequence::Idle;
                Some(ResizeOperation::DisplayReset)
            }
        }
    }

    pub(crate) fn publish_serial(&mut self, serial: ManagerSerial) -> SerialPublication {
        self.serial = Some(serial);
        if self.pending.is_none()
            && self.retry.is_some_and(|queued| {
                matches!(
                    queued.readiness,
                    RetryReadiness::AwaitingFreshSerial { stale, .. } if stale != serial
                )
            })
        {
            SerialPublication::RetryReady
        } else {
            SerialPublication::Recorded
        }
    }

    pub(crate) fn take_ready_queued(&mut self) -> Option<ResizeRequest> {
        if self.pending.is_some() {
            return None;
        }
        if let Some(retry) = self.retry {
            match retry.readiness {
                RetryReadiness::Ready => return self.retry.take().map(|queued| queued.request),
                RetryReadiness::AwaitingFreshSerial { stale, .. } if self.serial != Some(stale) => {
                    return self.retry.take().map(|queued| queued.request);
                }
                RetryReadiness::AwaitingFreshSerial { .. } => return None,
            }
        }
        match self.display_reset {
            DisplayResetSequence::AwaitingBounce { original, bounce } => {
                self.display_reset = DisplayResetSequence::BounceInFlight;
                Some(ResizeRequest {
                    mode: OutputMode {
                        size: bounce,
                        scale: ScaleChange::Keep,
                    },
                    purpose: RequestPurpose::DisplayResetBounce {
                        original,
                        failures: 0,
                    },
                })
            }
            DisplayResetSequence::AwaitingRestore { original } => {
                self.display_reset = DisplayResetSequence::RestoreInFlight;
                Some(ResizeRequest {
                    mode: OutputMode {
                        size: original,
                        scale: ScaleChange::Keep,
                    },
                    purpose: RequestPurpose::DisplayResetRestore { failures: 0 },
                })
            }
            DisplayResetSequence::Idle => self.queued.take().map(|queued| queued.request),
            DisplayResetSequence::BounceInFlight | DisplayResetSequence::RestoreInFlight => None,
        }
    }

    pub(crate) fn capture_blocked(&self) -> bool {
        self.pending.is_some()
            || self.retry.is_some()
            || self.queued.is_some()
            || !matches!(self.display_reset, DisplayResetSequence::Idle)
    }

    pub(crate) const fn configuration_deadline() -> Duration {
        OUTPUT_CONFIGURATION_DEADLINE
    }

    pub(crate) fn wait_timeout(&self, now: Instant) -> Option<Duration> {
        let deadline = self
            .pending
            .as_ref()
            .map(|pending| pending.submitted_at + OUTPUT_CONFIGURATION_DEADLINE)
            .or_else(|| {
                self.retry.and_then(|retry| match retry.readiness {
                    RetryReadiness::Ready => None,
                    RetryReadiness::AwaitingFreshSerial { since, .. } => {
                        Some(since + OUTPUT_CONFIGURATION_DEADLINE)
                    }
                })
            })?;
        Some(deadline.saturating_duration_since(now))
    }

    pub(crate) fn recover_timeout(&mut self, now: Instant) -> Option<OutputTimeout> {
        if self.pending.as_ref().is_some_and(|pending| {
            now.duration_since(pending.submitted_at) >= OUTPUT_CONFIGURATION_DEADLINE
        }) {
            let pending = self.pending.take()?;
            pending.configuration.destroy();
            let operation = match pending.request.purpose {
                RequestPurpose::External { .. } => ResizeOperation::External,
                RequestPurpose::DisplayResetBounce { .. }
                | RequestPurpose::DisplayResetRestore { .. } => {
                    self.display_reset = DisplayResetSequence::Idle;
                    ResizeOperation::DisplayReset
                }
            };
            return Some(OutputTimeout::Configuration { operation });
        }
        if self.retry.is_some_and(|retry| {
            matches!(
                retry.readiness,
                RetryReadiness::AwaitingFreshSerial { since, .. }
                    if now.duration_since(since) >= OUTPUT_CONFIGURATION_DEADLINE
            )
        }) {
            self.retry = None;
            return Some(OutputTimeout::FreshSerial);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(request_id: u16, width: u32, height: u32) -> ResizeRequest {
        ResizeRequest::external(
            FrameSize::new(width, height).expect("valid test frame size"),
            ScaleV120::new(120).expect("valid test output scale"),
            RequestId::new(request_id).expect("valid test request ID"),
        )
    }

    fn size(width: u32, height: u32) -> OutputSize {
        OutputSize::new(width, height).expect("valid test output size")
    }

    fn accept(outputs: &mut OutputManager, request: ResizeRequest) -> AppliedResize {
        let change = output_dimensions_changed(outputs.current, request.mode.size);
        outputs.accept_succeeded(request, change)
    }

    #[test]
    fn capture_dimensions_seed_display_reset_without_head_mode_state() {
        let mut outputs = OutputManager::new();
        outputs.record_capture_size(size(1366, 768));

        assert_eq!(outputs.begin_display_reset(), DisplayResetStart::Start);
        let bounce = outputs.take_ready_queued().expect("bounce should start");
        assert_eq!(bounce.mode.size, size(1364, 766));
        assert_eq!(bounce.mode.scale, ScaleChange::Keep);
        accept(&mut outputs, bounce);
        let restore = outputs.take_ready_queued().expect("restore should follow");
        assert_eq!(restore.mode.size, size(1366, 768));
        assert_eq!(restore.mode.scale, ScaleChange::Keep);
        accept(&mut outputs, restore);
        assert_eq!(outputs.current, CurrentMode::Known(size(1366, 768)));
        assert!(!outputs.capture_blocked());
    }

    #[test]
    fn output_work_blocks_stale_capture_dimension_seeds() {
        let mut outputs = OutputManager::new();
        outputs.record_capture_size(size(1280, 720));
        assert_eq!(outputs.begin_display_reset(), DisplayResetStart::Start);

        outputs.record_capture_size(size(1600, 900));
        outputs.abort_display_reset();

        assert_eq!(outputs.begin_display_reset(), DisplayResetStart::Start);
        assert_eq!(
            outputs.take_ready_queued().map(|request| request.mode.size),
            Some(size(1278, 718))
        );
    }

    #[test]
    fn display_reset_with_unknown_dimensions_is_refused_without_parking() {
        let mut outputs = OutputManager::new();
        assert_eq!(
            outputs.begin_display_reset(),
            DisplayResetStart::Refused(DisplayResetRefusal::CurrentModeUnknown)
        );
        outputs.queue_external(request(1, 1280, 720));
        assert_eq!(outputs.take_ready_queued(), Some(request(1, 1280, 720)));
    }

    #[test]
    fn too_small_display_reset_is_refused_without_parking() {
        let mut outputs = OutputManager::new();
        outputs.record_capture_size(size(2, 2));
        assert_eq!(
            outputs.begin_display_reset(),
            DisplayResetStart::Refused(DisplayResetRefusal::OutputTooSmall)
        );
        assert!(!outputs.capture_blocked());
    }

    #[test]
    fn external_success_during_display_reset_updates_the_definite_restore_size() {
        let mut outputs = OutputManager::new();
        outputs.record_capture_size(size(1280, 720));
        assert_eq!(outputs.begin_display_reset(), DisplayResetStart::Start);
        accept(&mut outputs, request(7, 1600, 900));

        let bounce = outputs.take_ready_queued().expect("bounce should start");
        assert_eq!(bounce.mode.size, size(1598, 898));
        accept(&mut outputs, bounce);
        let restore = outputs.take_ready_queued().expect("restore should follow");
        assert_eq!(restore.mode.size, size(1600, 900));
        accept(&mut outputs, restore);
        assert_eq!(outputs.current, CurrentMode::Known(size(1600, 900)));
    }

    #[test]
    fn display_reset_finishes_before_a_queued_external_resize() {
        let mut outputs = OutputManager::new();
        outputs.record_capture_size(size(1280, 720));
        assert_eq!(outputs.begin_display_reset(), DisplayResetStart::Start);
        outputs.queue_external(request(9, 1920, 1080));

        let bounce = outputs.take_ready_queued().expect("bounce should start");
        accept(&mut outputs, bounce);
        let restore = outputs.take_ready_queued().expect("restore should follow");
        accept(&mut outputs, restore);
        assert_eq!(outputs.take_ready_queued(), Some(request(9, 1920, 1080)));
        assert!(!outputs.capture_blocked());
    }

    #[test]
    fn aborting_a_display_reset_keeps_queued_external_work_reachable() {
        let mut outputs = OutputManager::new();
        outputs.record_capture_size(size(1280, 720));
        assert_eq!(outputs.begin_display_reset(), DisplayResetStart::Start);
        outputs.queue_external(request(9, 1920, 1080));

        assert_eq!(
            outputs.take_ready_queued().map(ResizeRequest::operation),
            Some(ResizeOperation::DisplayReset)
        );
        outputs.abort_display_reset();
        assert!(outputs.capture_blocked());
        assert_eq!(outputs.take_ready_queued(), Some(request(9, 1920, 1080)));
        assert!(!outputs.capture_blocked());
    }

    #[test]
    fn aborting_a_display_reset_keeps_an_external_retry_reachable() {
        let mut outputs = OutputManager::new();
        outputs.record_capture_size(size(1280, 720));
        assert_eq!(outputs.begin_display_reset(), DisplayResetStart::Start);
        outputs.retry = Some(QueuedResize {
            request: request(9, 1920, 1080),
            readiness: RetryReadiness::Ready,
        });

        outputs.abort_display_reset();

        assert_eq!(outputs.take_ready_queued(), Some(request(9, 1920, 1080)));
        assert!(!outputs.capture_blocked());
    }

    #[test]
    fn failed_final_request_retries_but_does_not_claim_the_live_incident_cause() {
        let mut outputs = OutputManager::new();
        outputs.queue_external(request(1, 1200, 700));
        outputs.queue_external(request(2, 1840, 900));
        outputs.queue_external(request(3, 1180, 690));
        outputs.queue_external(request(4, 1840, 900));
        let final_request = outputs.take_ready_queued().expect("final request");

        assert_eq!(
            outputs.retry_failed_request(final_request),
            RejectOutcome::Retrying {
                operation: ResizeOperation::External,
                failures: 1,
            }
        );
        assert_eq!(
            outputs.take_ready_queued(),
            Some(ResizeRequest {
                purpose: RequestPurpose::External {
                    request_id: RequestId::new(4).expect("valid request ID"),
                    scale_v120: ScaleV120::new(120).expect("valid scale"),
                    failures: 1,
                },
                ..request(4, 1840, 900)
            })
        );
    }

    #[test]
    fn failed_display_reset_clears_at_the_nonfatal_bound() {
        let mut outputs = OutputManager::new();
        outputs.record_capture_size(size(1280, 720));
        assert_eq!(outputs.begin_display_reset(), DisplayResetStart::Start);
        let mut request = outputs.take_ready_queued().expect("bounce should start");
        for failures in 1..MAX_RESIZE_STEP_FAILURES {
            assert_eq!(
                outputs.retry_failed_request(request),
                RejectOutcome::Retrying {
                    operation: ResizeOperation::DisplayReset,
                    failures,
                }
            );
            request = outputs.take_ready_queued().expect("retry should start");
        }
        assert_eq!(
            outputs.retry_failed_request(request),
            RejectOutcome::Exhausted {
                operation: ResizeOperation::DisplayReset,
                failures: MAX_RESIZE_STEP_FAILURES,
            }
        );
        assert!(!outputs.capture_blocked());
    }

    #[test]
    fn cancelled_external_waits_for_a_fresh_typed_serial() {
        let mut outputs = OutputManager::new();
        outputs.serial = Some(ManagerSerial::new(7));
        outputs.retry = Some(QueuedResize {
            request: request(2, 1600, 900),
            readiness: RetryReadiness::AwaitingFreshSerial {
                stale: ManagerSerial::new(7),
                since: Instant::now(),
            },
        });
        assert!(outputs.take_ready_queued().is_none());
        assert!(outputs.capture_blocked());
        assert_eq!(
            outputs.publish_serial(ManagerSerial::new(7)),
            SerialPublication::Recorded
        );
        assert_eq!(
            outputs.publish_serial(ManagerSerial::new(8)),
            SerialPublication::RetryReady
        );
        assert_eq!(outputs.take_ready_queued(), Some(request(2, 1600, 900)));
        assert!(!outputs.capture_blocked());
    }

    #[test]
    fn cancelled_resize_serial_wait_expires_nonfatally() {
        let mut outputs = OutputManager::new();
        let now = Instant::now();
        outputs.serial = Some(ManagerSerial::new(7));
        outputs.retry = Some(QueuedResize {
            request: request(2, 1600, 900),
            readiness: RetryReadiness::AwaitingFreshSerial {
                stale: ManagerSerial::new(7),
                since: now
                    .checked_sub(OUTPUT_CONFIGURATION_DEADLINE)
                    .expect("test deadline should fit before now"),
            },
        });

        assert_eq!(
            outputs.recover_timeout(now),
            Some(OutputTimeout::FreshSerial)
        );
        assert!(!outputs.capture_blocked());
    }

    #[test]
    fn dimension_change_is_typed() {
        assert_eq!(
            output_dimensions_changed(CurrentMode::Known(size(1920, 1080)), size(1920, 1080)),
            DimensionChange::Unchanged
        );
        assert_eq!(
            output_dimensions_changed(CurrentMode::Known(size(1920, 1080)), size(1280, 720)),
            DimensionChange::Changed
        );
        assert_eq!(
            output_dimensions_changed(CurrentMode::Unknown, size(1280, 720)),
            DimensionChange::Changed
        );
    }
}
