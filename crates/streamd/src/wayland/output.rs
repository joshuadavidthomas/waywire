use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use wayland_client::QueueHandle;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_head_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1;

use super::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputMode {
    pub width: u32,
    pub height: u32,
    pub scale_v120: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeRequest {
    pub mode: OutputMode,
    pub request_id: u16,
}

pub struct Head {
    pub proxy: zwlr_output_head_v1::ZwlrOutputHeadV1,
    pub name: Option<String>,
    pub enabled: bool,
    pub finished: bool,
}

pub struct PendingResize {
    pub configuration: zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
    request: ResizeRequest,
    serial: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueuedResize {
    request: ResizeRequest,
    retry_after: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedResize {
    pub mode: OutputMode,
    pub request_id: u16,
    pub dimensions_changed: bool,
}

fn output_dimensions_changed(current: Option<OutputMode>, next: OutputMode) -> bool {
    current.is_none_or(|current| current.width != next.width || current.height != next.height)
}

pub struct OutputManager {
    pub manager: Option<zwlr_output_manager_v1::ZwlrOutputManagerV1>,
    pub heads: Vec<Head>,
    pub serial: Option<u32>,
    pub finished: bool,
    pub pending: Option<PendingResize>,
    queued: Option<QueuedResize>,
    pub current: Option<OutputMode>,
}

impl OutputManager {
    pub fn new() -> Self {
        Self {
            manager: None,
            heads: Vec::new(),
            serial: None,
            finished: false,
            pending: None,
            queued: None,
            current: None,
        }
    }

    pub fn configure(
        &mut self,
        output_name: Option<&str>,
        mode: OutputMode,
        request_id: u16,
        fps: u32,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let request = ResizeRequest { mode, request_id };
        if self.pending.is_some() || self.queued.is_some() {
            self.queue_request(request);
            return Ok(());
        }
        self.configure_now(output_name, request, fps, qh)
    }

    fn configure_now(
        &mut self,
        output_name: Option<&str>,
        request: ResizeRequest,
        fps: u32,
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

        let width =
            i32::try_from(request.mode.width).context("output width exceeds protocol range")?;
        let height =
            i32::try_from(request.mode.height).context("output height exceeds protocol range")?;
        let refresh = i32::try_from(
            fps.checked_mul(1_000)
                .context("output refresh rate overflow")?,
        )
        .context("output refresh rate exceeds protocol range")?;
        let configuration = manager.create_configuration(serial, qh, ());
        for head in self.heads.iter().filter(|head| !head.finished) {
            if !head.enabled {
                configuration.disable_head(&head.proxy);
                continue;
            }
            let configured = configuration.enable_head(&head.proxy, qh, ());
            if output_name.is_none_or(|name| head.name.as_deref() == Some(name)) {
                configured.set_custom_mode(width, height, refresh);
                configured.set_scale(f64::from(request.mode.scale_v120) / 120.0);
            }
        }
        configuration.apply();
        self.pending = Some(PendingResize {
            configuration,
            request,
            serial,
        });
        Ok(())
    }

    fn queue_request(&mut self, request: ResizeRequest) {
        let retry_after = self.queued.and_then(|queued| queued.retry_after);
        self.queued = Some(QueuedResize {
            request,
            retry_after,
        });
    }

    pub fn take_succeeded(&mut self) -> Option<AppliedResize> {
        let pending = self.pending.take()?;
        pending.configuration.destroy();
        let dimensions_changed = output_dimensions_changed(self.current, pending.request.mode);
        self.current = Some(pending.request.mode);
        Some(AppliedResize {
            mode: pending.request.mode,
            request_id: pending.request.request_id,
            dimensions_changed,
        })
    }

    pub fn reject_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.configuration.destroy();
        }
    }

    pub fn retry_cancelled(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        pending.configuration.destroy();
        self.queue_cancelled_request(pending.request, pending.serial);
    }

    fn queue_cancelled_request(&mut self, request: ResizeRequest, stale_serial: u32) {
        let request = self.queued.take().map_or(request, |queued| queued.request);
        self.queued = Some(QueuedResize {
            request,
            retry_after: Some(stale_serial),
        });
    }

    pub fn publish_serial(&mut self, serial: u32) -> bool {
        self.serial = Some(serial);
        self.pending.is_none()
            && self
                .queued
                .is_some_and(|queued| queued.retry_after.is_some_and(|stale| stale != serial))
    }

    pub fn take_ready_queued(&mut self) -> Option<ResizeRequest> {
        let queued = self.queued?;
        if queued
            .retry_after
            .is_some_and(|stale| self.serial == Some(stale))
        {
            return None;
        }
        self.queued.take().map(|queued| queued.request)
    }

    pub fn has_queued(&self) -> bool {
        self.queued.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(request_id: u16, width: u32, height: u32) -> ResizeRequest {
        ResizeRequest {
            mode: OutputMode {
                width,
                height,
                scale_v120: 120,
            },
            request_id,
        }
    }

    #[test]
    fn resize_burst_keeps_only_the_latest_request() {
        let mut outputs = OutputManager::new();
        outputs.queue_request(request(1, 1280, 720));
        outputs.queue_request(request(2, 1920, 1080));

        assert_eq!(outputs.take_ready_queued(), Some(request(2, 1920, 1080)));
        assert!(outputs.take_ready_queued().is_none());
    }

    #[test]
    fn applied_dimension_change_does_not_depend_on_capture_damage_state() {
        let previous = request(1, 1920, 1080).mode;

        assert!(!output_dimensions_changed(
            Some(previous),
            request(2, 1920, 1080).mode
        ));
        assert!(output_dimensions_changed(
            Some(previous),
            request(2, 1280, 720).mode
        ));
        assert!(output_dimensions_changed(None, request(2, 1280, 720).mode));
    }

    #[test]
    fn cancelled_resize_keeps_latest_request_until_a_fresh_serial() {
        let mut outputs = OutputManager::new();
        outputs.serial = Some(7);
        outputs.queue_request(request(2, 1600, 900));
        outputs.queue_cancelled_request(request(1, 1280, 720), 7);

        assert!(outputs.take_ready_queued().is_none());
        assert!(!outputs.publish_serial(7));
        assert!(outputs.take_ready_queued().is_none());
        assert!(outputs.publish_serial(8));
        assert_eq!(outputs.take_ready_queued(), Some(request(2, 1600, 900)));
    }

    #[test]
    fn newer_request_inherits_cancelled_resizes_fresh_serial_wait() {
        let mut outputs = OutputManager::new();
        outputs.serial = Some(11);
        outputs.queue_cancelled_request(request(1, 1280, 720), 11);
        outputs.queue_request(request(2, 1920, 1080));

        assert!(outputs.take_ready_queued().is_none());
        assert!(outputs.publish_serial(12));
        assert_eq!(outputs.take_ready_queued(), Some(request(2, 1920, 1080)));
    }
}
