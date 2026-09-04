use std::{ffi::CString, fs::File, os::fd::AsFd};

use anyhow::{Context, Result, bail};
use memmap2::{MmapMut, MmapOptions};
use nix::sys::memfd::{MFdFlags, memfd_create};
use nix::unistd::ftruncate;
use wayland_client::{
    QueueHandle, WEnum,
    protocol::{wl_buffer, wl_output, wl_shm},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
        ext_image_copy_capture_session_v1,
    },
};

use super::State;
use crate::{
    protocol::{FrameMetadata, MAX_RAW_PIXELS},
    video::{CapturedFrame, EncoderConfig, encoded_dimensions},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaptureTarget {
    Output,
    Cursor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaptureFailureRecovery {
    WaitForConstraints,
    RestartSession,
}

const MAX_CONSECUTIVE_CONSTRAINT_FAILURES: u8 = 3;

pub struct Capture {
    pub source: Option<ext_image_capture_source_v1::ExtImageCaptureSourceV1>,
    pub session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    pub frame: Option<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    mapping: Option<MmapMut>,
    pub width: u32,
    pub height: u32,
    stride: u32,
    format: Option<wl_shm::Format>,
    batch_width: Option<u32>,
    batch_height: Option<u32>,
    batch_argb: bool,
    batch_xrgb: bool,
    collecting_constraints: bool,
    transform_normal: bool,
    presentation_nanos: Option<u64>,
    pub cursor_overlay: bool,
    pub sequence: u64,
    consecutive_constraint_failures: u8,
}

impl Capture {
    pub fn new() -> Self {
        Self {
            source: None,
            session: None,
            frame: None,
            buffer: None,
            mapping: None,
            width: 0,
            height: 0,
            stride: 0,
            format: None,
            batch_width: None,
            batch_height: None,
            batch_argb: false,
            batch_xrgb: false,
            collecting_constraints: false,
            transform_normal: false,
            presentation_nanos: None,
            cursor_overlay: false,
            sequence: 0,
            consecutive_constraint_failures: 0,
        }
    }

    pub fn start(
        &mut self,
        output: &wl_output::WlOutput,
        source_manager: &ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
        capture_manager: &ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        if self.session.is_some() || self.frame.is_some() {
            bail!("output capture session is already active");
        }
        let source = source_manager.create_source(output, qh, ());
        let options = if self.cursor_overlay {
            ext_image_copy_capture_manager_v1::Options::PaintCursors
        } else {
            ext_image_copy_capture_manager_v1::Options::empty()
        };
        let session = capture_manager.create_session(&source, options, qh, CaptureTarget::Output);
        self.source = Some(source);
        self.session = Some(session);
        Ok(())
    }

    pub fn cancel(&mut self) {
        self.destroy_frame();
        if let Some(session) = self.session.take() {
            session.destroy();
        }
        if let Some(source) = self.source.take() {
            source.destroy();
        }
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.mapping = None;
        self.collecting_constraints = false;
        self.clear_batch();
    }

    pub fn begin_constraints(&mut self) {
        if !self.collecting_constraints {
            self.clear_batch();
            self.collecting_constraints = true;
        }
    }

    pub fn constraint_size(&mut self, width: u32, height: u32) {
        self.begin_constraints();
        self.batch_width = Some(width);
        self.batch_height = Some(height);
    }

    pub fn constraint_format(&mut self, format: WEnum<wl_shm::Format>) {
        self.begin_constraints();
        match format {
            WEnum::Value(wl_shm::Format::Argb8888) => self.batch_argb = true,
            WEnum::Value(wl_shm::Format::Xrgb8888) => self.batch_xrgb = true,
            _ => {}
        }
    }

    pub fn finish_constraints(
        &mut self,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        if !self.collecting_constraints {
            bail!("output capture done omitted a constraint batch");
        }
        self.collecting_constraints = false;
        let width = self
            .batch_width
            .take()
            .context("output constraints omitted width")?;
        let height = self
            .batch_height
            .take()
            .context("output constraints omitted height")?;
        let format = choose_format(
            std::mem::take(&mut self.batch_argb),
            std::mem::take(&mut self.batch_xrgb),
        )?;
        self.destroy_frame();
        self.allocate(shm, width, height, format, qh)
    }

    fn clear_batch(&mut self) {
        self.batch_width = None;
        self.batch_height = None;
        self.batch_argb = false;
        self.batch_xrgb = false;
    }

    fn allocate(
        &mut self,
        shm: &wl_shm::WlShm,
        width: u32,
        height: u32,
        format: wl_shm::Format,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let (stride, length) = capture_layout(width, height)?;
        // A new constraint batch can cancel a frame still being copied. Retire
        // its storage even when the dimensions match; destruction is no Ready.
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.mapping = None;
        let name = CString::new("sprite-desktop-capture").unwrap();
        let fd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC)?;
        ftruncate(&fd, i64::try_from(length)?)?;
        let file = File::from(fd);
        // The map and Wayland buffer retain the memfd storage independently.
        let mapping = unsafe { MmapOptions::new().len(length).map_mut(&file)? };
        let pool = shm.create_pool(file.as_fd(), i32::try_from(length)?, qh, ());
        let buffer = pool.create_buffer(
            0,
            i32::try_from(width)?,
            i32::try_from(height)?,
            i32::try_from(stride)?,
            format,
            qh,
            (),
        );
        pool.destroy();
        self.buffer = Some(buffer);
        self.mapping = Some(mapping);
        self.width = width;
        self.height = height;
        self.stride = stride;
        self.format = Some(format);
        Ok(())
    }

    pub fn request(&mut self, qh: &QueueHandle<State>) -> Result<()> {
        if self.frame.is_some() {
            bail!("output capture frame is already pending");
        }
        let session = self
            .session
            .as_ref()
            .context("output capture session unavailable")?;
        let buffer = self
            .buffer
            .as_ref()
            .context("output capture buffer unavailable")?;
        let width = i32::try_from(self.width).context("capture width exceeds protocol range")?;
        let height = i32::try_from(self.height).context("capture height exceeds protocol range")?;
        let frame = session.create_frame(qh, CaptureTarget::Output);
        frame.attach_buffer(buffer);
        // This ablation keeps one buffer, so each capture refreshes every pixel.
        frame.damage_buffer(0, 0, width, height);
        frame.capture();
        self.transform_normal = false;
        self.presentation_nanos = None;
        self.frame = Some(frame);
        Ok(())
    }

    pub fn transform(&mut self, transform: WEnum<wl_output::Transform>) -> Result<()> {
        if transform != WEnum::Value(wl_output::Transform::Normal) {
            bail!("output capture transform is not normal");
        }
        self.transform_normal = true;
        Ok(())
    }

    pub fn presentation_time(
        &mut self,
        tv_sec_hi: u32,
        tv_sec_lo: u32,
        tv_nsec: u32,
    ) -> Result<()> {
        self.presentation_nanos = Some(
            super::protocol_ready_nanos(tv_sec_hi, tv_sec_lo, tv_nsec)
                .context("invalid output capture presentation timestamp")?,
        );
        Ok(())
    }

    pub fn ready(&mut self) -> Result<u64> {
        if !self.transform_normal {
            bail!("output capture frame omitted normal transform");
        }
        let presentation_nanos = self
            .presentation_nanos
            .context("output capture frame omitted presentation time")?;
        self.destroy_frame();
        self.consecutive_constraint_failures = 0;
        Ok(presentation_nanos)
    }

    pub fn failed(
        &mut self,
        reason: WEnum<ext_image_copy_capture_frame_v1::FailureReason>,
    ) -> Result<CaptureFailureRecovery> {
        self.destroy_frame();
        match reason {
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints) => {
                self.consecutive_constraint_failures =
                    self.consecutive_constraint_failures.saturating_add(1);
                if self.consecutive_constraint_failures > MAX_CONSECUTIVE_CONSTRAINT_FAILURES {
                    bail!("output capture buffer constraints failed repeatedly")
                } else if self.collecting_constraints {
                    Ok(CaptureFailureRecovery::WaitForConstraints)
                } else {
                    // Some compositors send no replacement batch. A new session asks
                    // for authoritative constraints without reusing the old buffer.
                    Ok(CaptureFailureRecovery::RestartSession)
                }
            }
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::Unknown) => {
                bail!("output capture failed")
            }
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::Stopped) => {
                bail!("output capture session stopped")
            }
            WEnum::Unknown(value) => bail!("output capture failed with unknown reason {value}"),
            _ => bail!("output capture failed with unsupported reason"),
        }
    }

    pub fn destroy_frame(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.transform_normal = false;
        self.presentation_nanos = None;
    }

    pub fn completed_frame(
        &mut self,
        capture_nanos: u64,
        generation: u32,
        input_sequence: u32,
        fps: u32,
        bitrate_kbps: u32,
        scale_percent: u32,
    ) -> Result<CapturedFrame<'_>> {
        let mapping = self.mapping.as_ref().context("missing capture mapping")?;
        let expected = usize::try_from(
            u64::from(self.width)
                .checked_mul(u64::from(self.height))
                .and_then(|pixels| pixels.checked_mul(4))
                .context("capture pixel length overflow")?,
        )?;
        if mapping.len() != expected || self.stride != self.width.checked_mul(4).unwrap_or(0) {
            bail!("capture mapping dimensions or stride changed");
        }
        if !matches!(
            self.format,
            Some(wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888)
        ) {
            bail!("capture mapping format changed");
        }
        let (encoded_width, encoded_height) =
            encoded_dimensions(self.width, self.height, scale_percent, fps);
        let metadata = FrameMetadata {
            generation,
            width: encoded_width,
            height: encoded_height,
            capture_nanos,
            sequence: self.sequence,
            input_sequence,
            fps,
        };
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("frame sequence exhausted")?;
        Ok(CapturedFrame::new(
            mapping,
            metadata,
            EncoderConfig {
                raw_width: self.width,
                raw_height: self.height,
                encoded_width,
                encoded_height,
                fps,
                bitrate_kbps,
            },
        ))
    }
}

fn choose_format(argb: bool, xrgb: bool) -> Result<wl_shm::Format> {
    if xrgb {
        Ok(wl_shm::Format::Xrgb8888)
    } else if argb {
        Ok(wl_shm::Format::Argb8888)
    } else {
        bail!("output capture has no ARGB8888 or XRGB8888 SHM format")
    }
}

fn capture_layout(width: u32, height: u32) -> Result<(u32, usize)> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || pixels > MAX_RAW_PIXELS {
        bail!("unsupported or oversized capture buffer {width}x{height}");
    }
    let stride = width.checked_mul(4).context("capture stride overflow")?;
    let length = u64::from(stride)
        .checked_mul(u64::from(height))
        .context("capture buffer length overflow")?;
    Ok((
        stride,
        usize::try_from(length).context("capture allocation exceeds address space")?,
    ))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use wayland_client::{Connection, Proxy};

    use super::*;

    struct ClientObjects {
        _connection: Connection,
        _server: UnixStream,
        qh: QueueHandle<State>,
        output: wl_output::WlOutput,
        source_manager:
            ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
        capture_manager: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        shm: wl_shm::WlShm,
    }

    fn client_objects() -> ClientObjects {
        let (client, server) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let queue = connection.new_event_queue::<State>();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        ClientObjects {
            output: registry.bind(1, 1, &qh, ()),
            source_manager: registry.bind(2, 1, &qh, ()),
            capture_manager: registry.bind(3, 1, &qh, ()),
            shm: registry.bind(4, 1, &qh, ()),
            _connection: connection,
            _server: server,
            qh,
        }
    }

    fn configure(capture: &mut Capture, objects: &ClientObjects) {
        capture.constraint_size(10, 20);
        capture.constraint_format(WEnum::Value(wl_shm::Format::Xrgb8888));
        capture
            .finish_constraints(&objects.shm, &objects.qh)
            .unwrap();
    }

    fn start(capture: &mut Capture, objects: &ClientObjects) {
        capture
            .start(
                &objects.output,
                &objects.source_manager,
                &objects.capture_manager,
                &objects.qh,
            )
            .unwrap();
    }

    #[test]
    fn constraint_batches_reset_all_previous_values() {
        let mut capture = Capture::new();
        capture.constraint_size(10, 20);
        capture.constraint_format(WEnum::Value(wl_shm::Format::Argb8888));
        capture.collecting_constraints = false;
        capture.begin_constraints();
        assert_eq!(capture.batch_width, None);
        assert_eq!(capture.batch_height, None);
        assert!(!capture.batch_argb);
    }

    #[test]
    fn capture_lifecycle_owns_one_frame_and_retires_cancelled_objects() {
        let objects = client_objects();
        let mut capture = Capture::new();
        start(&mut capture, &objects);
        let first_session = capture.session.clone().unwrap();
        configure(&mut capture, &objects);
        capture.request(&objects.qh).unwrap();
        let first_frame = capture.frame.clone().unwrap();
        assert!(capture.request(&objects.qh).is_err());

        capture.cancel();
        assert!(capture.frame.is_none());
        assert!(capture.session.is_none());
        assert!(capture.buffer.is_none());
        assert!(capture.mapping.is_none());

        start(&mut capture, &objects);
        assert_ne!(capture.session.as_ref().unwrap().id(), first_session.id());
        configure(&mut capture, &objects);
        capture.request(&objects.qh).unwrap();
        assert_ne!(capture.frame.as_ref().unwrap().id(), first_frame.id());
        assert_ne!(capture.frame.as_ref(), Some(&first_frame));

        capture
            .transform(WEnum::Value(wl_output::Transform::Normal))
            .unwrap();
        capture.presentation_time(0, 1, 2).unwrap();
        assert_eq!(capture.ready().unwrap(), 1_000_000_002);
        assert!(capture.frame.is_none());
        capture.request(&objects.qh).unwrap();
    }

    #[test]
    fn new_constraint_batch_retires_pending_buffer_even_at_the_same_size() {
        let objects = client_objects();
        let mut capture = Capture::new();
        start(&mut capture, &objects);
        configure(&mut capture, &objects);
        capture.request(&objects.qh).unwrap();
        let old_buffer = capture.buffer.as_ref().unwrap().id();
        configure(&mut capture, &objects);
        assert!(capture.frame.is_none());
        assert_ne!(capture.buffer.as_ref().unwrap().id(), old_buffer);
        capture.request(&objects.qh).unwrap();
    }

    #[test]
    fn constraint_failures_restart_with_a_bound_and_ready_resets_the_bound() {
        let objects = client_objects();
        let mut capture = Capture::new();
        start(&mut capture, &objects);
        configure(&mut capture, &objects);

        for _ in 0..MAX_CONSECUTIVE_CONSTRAINT_FAILURES {
            capture.request(&objects.qh).unwrap();
            assert_eq!(
                capture
                    .failed(WEnum::Value(
                        ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints,
                    ))
                    .unwrap(),
                CaptureFailureRecovery::RestartSession
            );
            capture.cancel();
            start(&mut capture, &objects);
            configure(&mut capture, &objects);
        }
        capture.request(&objects.qh).unwrap();
        assert!(
            capture
                .failed(WEnum::Value(
                    ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints,
                ))
                .is_err()
        );

        capture.request(&objects.qh).unwrap();
        capture
            .transform(WEnum::Value(wl_output::Transform::Normal))
            .unwrap();
        capture.presentation_time(0, 0, 1).unwrap();
        capture.ready().unwrap();
        capture.request(&objects.qh).unwrap();
        assert_eq!(
            capture
                .failed(WEnum::Value(
                    ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints,
                ))
                .unwrap(),
            CaptureFailureRecovery::RestartSession
        );

        capture.begin_constraints();
        assert_eq!(
            capture
                .failed(WEnum::Value(
                    ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints,
                ))
                .unwrap(),
            CaptureFailureRecovery::WaitForConstraints
        );
    }

    #[test]
    fn dimensions_and_formats_are_rejected_or_exact() {
        assert_eq!(capture_layout(10, 20).unwrap(), (40, 800));
        assert!(capture_layout(0, 20).is_err());
        assert!(capture_layout(u32::MAX, u32::MAX).is_err());
        assert_eq!(
            choose_format(true, false).unwrap(),
            wl_shm::Format::Argb8888
        );
        assert_eq!(choose_format(true, true).unwrap(), wl_shm::Format::Xrgb8888);
        assert!(choose_format(false, false).is_err());
    }
}
