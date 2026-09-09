use std::fs::File;
use std::os::fd::AsFd;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use memmap2::MmapMut;
use memmap2::MmapOptions;
use nix::sys::memfd::MFdFlags;
use nix::sys::memfd::memfd_create;
use nix::unistd::ftruncate;
use wayland_client::QueueHandle;
use wayland_client::WEnum;
use wayland_client::protocol::wl_buffer;
use wayland_client::protocol::wl_output;
use wayland_client::protocol::wl_seat;
use wayland_client::protocol::wl_shm;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_cursor_session_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1;

use super::State;
use crate::event_writer::EventSink;
use crate::protocol::Event;
use crate::protocol::cursor_byte_count;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Hotspot {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ConstraintBatch {
    #[default]
    Idle,
    Collecting {
        argb: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Visibility {
    #[default]
    Unknown,
    Visible,
    Hidden,
}

pub struct Cursor {
    pub source_manager:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    pub capture_manager: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    pub pointer_session:
        Option<ext_image_copy_capture_cursor_session_v1::ExtImageCopyCaptureCursorSessionV1>,
    pub session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    pub frame: Option<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    mapping: Option<MmapMut>,
    width: u32,
    height: u32,
    has_pointer: bool,
    pub(super) batch_width: Option<u32>,
    pub(super) batch_height: Option<u32>,
    constraint_batch: ConstraintBatch,
    pending_hotspot: Hotspot,
    committed_hotspot: Hotspot,
    transform_normal: bool,
    force_publish: bool,
    published: Option<(u32, u32, Hotspot, Vec<u8>)>,
    visibility: Visibility,
    events: EventSink,
}

impl Cursor {
    pub fn new(events: EventSink) -> Self {
        Self {
            source_manager: None,
            capture_manager: None,
            pointer_session: None,
            session: None,
            frame: None,
            buffer: None,
            mapping: None,
            width: 0,
            height: 0,
            has_pointer: false,
            batch_width: None,
            batch_height: None,
            constraint_batch: ConstraintBatch::Idle,
            pending_hotspot: Hotspot::default(),
            committed_hotspot: Hotspot::default(),
            transform_normal: false,
            force_publish: true,
            published: None,
            visibility: Visibility::Unknown,
            events,
        }
    }

    pub fn seat_capabilities(&mut self, capabilities: WEnum<wl_seat::Capability>) {
        self.has_pointer = match capabilities {
            WEnum::Value(value) => value.contains(wl_seat::Capability::Pointer),
            WEnum::Unknown(_) => false,
        };
    }

    pub fn start(
        &mut self,
        seat: &wl_seat::WlSeat,
        output: &wl_output::WlOutput,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        if !self.has_pointer {
            bail!("seat has no pointer after virtual pointer creation");
        }
        let source_manager = self
            .source_manager
            .as_ref()
            .context("missing cursor source manager")?;
        let capture_manager = self
            .capture_manager
            .as_ref()
            .context("missing image copy capture manager")?;
        let source = source_manager.create_source(output, qh, ());
        let pointer = seat.get_pointer(qh, ());
        let pointer_session =
            capture_manager.create_pointer_cursor_session(&source, &pointer, qh, ());
        let session = pointer_session.get_capture_session(qh, ());
        source.destroy();
        pointer.release();
        self.pointer_session = Some(pointer_session);
        self.session = Some(session);
        Ok(())
    }

    pub fn begin_constraints(&mut self) {
        if self.constraint_batch == ConstraintBatch::Idle {
            self.batch_width = None;
            self.batch_height = None;
            self.constraint_batch = ConstraintBatch::Collecting { argb: false };
        }
    }

    pub fn accept_argb(&mut self) {
        self.begin_constraints();
        self.constraint_batch = ConstraintBatch::Collecting { argb: true };
    }

    pub fn finish_constraints(
        &mut self,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let constraint_batch = std::mem::take(&mut self.constraint_batch);
        let width = self
            .batch_width
            .take()
            .context("cursor constraints omitted width")?;
        let height = self
            .batch_height
            .take()
            .context("cursor constraints omitted height")?;
        if !matches!(constraint_batch, ConstraintBatch::Collecting { argb: true }) {
            bail!("cursor capture does not support ARGB8888 SHM");
        }
        self.destroy_frame();
        self.allocate(shm, width, height, qh)?;
        self.request(qh)
    }

    fn allocate(
        &mut self,
        shm: &wl_shm::WlShm,
        width: u32,
        height: u32,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let length = cursor_byte_count(width, height)?;
        if self.buffer.is_some() && self.width == width && self.height == height {
            return Ok(());
        }
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.mapping = None;
        let fd = memfd_create(c"sprite-desktop-cursor", MFdFlags::MFD_CLOEXEC)?;
        ftruncate(&fd, i64::try_from(length)?)?;
        let file = File::from(fd);
        // SAFETY: the map owns a duplicate of the valid memfd and uses its current length.
        let mapping = unsafe { MmapOptions::new().len(length).map_mut(&file)? };
        let pool = shm.create_pool(file.as_fd(), i32::try_from(length)?, qh, ());
        let buffer = pool.create_buffer(
            0,
            i32::try_from(width)?,
            i32::try_from(height)?,
            i32::try_from(width.checked_mul(4).context("cursor stride overflow")?)?,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        self.mapping = Some(mapping);
        self.buffer = Some(buffer);
        self.width = width;
        self.height = height;
        Ok(())
    }

    pub fn request(&mut self, qh: &QueueHandle<State>) -> Result<()> {
        if self.frame.is_some() {
            bail!("cursor frame already pending");
        }
        let session = self
            .session
            .as_ref()
            .context("cursor session unavailable")?;
        let buffer = self.buffer.as_ref().context("cursor buffer unavailable")?;
        let frame = session.create_frame(qh, ());
        frame.attach_buffer(buffer);
        frame.damage_buffer(
            0,
            0,
            i32::try_from(self.width).context("cursor width exceeds protocol range")?,
            i32::try_from(self.height).context("cursor height exceeds protocol range")?,
        );
        frame.capture();
        self.transform_normal = false;
        self.frame = Some(frame);
        Ok(())
    }

    pub fn frame_ready(&mut self, qh: &QueueHandle<State>) -> Result<()> {
        if !self.transform_normal {
            bail!("cursor frame omitted normal transform");
        }
        self.destroy_frame();
        self.committed_hotspot = self.pending_hotspot;
        let pixels = self
            .mapping
            .as_ref()
            .context("cursor mapping unavailable")?
            .to_vec();
        let changed = self.published.as_ref().is_none_or(|published| {
            published.0 != self.width
                || published.1 != self.height
                || published.2 != self.committed_hotspot
                || published.3 != pixels
        });
        if changed || self.force_publish {
            self.events.send(&Event::CursorImage {
                width: self.width,
                height: self.height,
                hotspot_x: self.committed_hotspot.x,
                hotspot_y: self.committed_hotspot.y,
                bgra: pixels.clone(),
            })?;
            self.published = Some((self.width, self.height, self.committed_hotspot, pixels));
            self.force_publish = false;
        }
        self.request(qh)
    }

    pub fn frame_failed(
        &mut self,
        constraints_changed: bool,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        self.destroy_frame();
        match failure_recovery(
            constraints_changed,
            self.constraint_batch != ConstraintBatch::Idle,
        ) {
            FailureRecovery::WaitForConstraints => Ok(()),
            FailureRecovery::Retry => self.request(qh),
            FailureRecovery::Reallocate => {
                if let Some(buffer) = self.buffer.take() {
                    buffer.destroy();
                }
                self.mapping = None;
                self.allocate(shm, self.width, self.height, qh)?;
                self.request(qh)
            }
        }
    }

    pub fn transform(&mut self, transform: WEnum<wl_output::Transform>) -> Result<()> {
        if transform != WEnum::Value(wl_output::Transform::Normal) {
            bail!("cursor transform is not normal");
        }
        self.transform_normal = true;
        Ok(())
    }

    pub fn hotspot(&mut self, x: i32, y: i32) {
        self.pending_hotspot = Hotspot { x, y };
    }

    pub fn visibility(&mut self, visible: bool) -> Result<()> {
        let visibility = if visible {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if self.visibility == visibility {
            return Ok(());
        }
        if visible {
            self.force_publish = true;
        }
        self.events.send(&Event::CursorVisibility(visible))?;
        self.visibility = visibility;
        Ok(())
    }

    pub fn destroy_frame(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.transform_normal = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureRecovery {
    WaitForConstraints,
    Reallocate,
    Retry,
}

fn failure_recovery(constraints_changed: bool, collecting_constraints: bool) -> FailureRecovery {
    match (constraints_changed, collecting_constraints) {
        (true, true) => FailureRecovery::WaitForConstraints,
        (true, false) => FailureRecovery::Reallocate,
        (false, _) => FailureRecovery::Retry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraint_failure_without_a_new_batch_reallocates_and_retries() {
        assert_eq!(failure_recovery(true, false), FailureRecovery::Reallocate);
        assert_eq!(
            failure_recovery(true, true),
            FailureRecovery::WaitForConstraints
        );
        assert_eq!(failure_recovery(false, false), FailureRecovery::Retry);
    }
}
