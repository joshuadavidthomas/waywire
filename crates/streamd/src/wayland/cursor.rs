mod shapes;

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
use tracing::debug;
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
use waywire_protocol::pipe::CursorPosition;
use waywire_protocol::pipe::CursorShape;
use waywire_protocol::pipe::CursorVisibility;
use waywire_protocol::pipe::Event;

use self::shapes::ImageKey;
pub(super) use self::shapes::ShapeTable;
use super::State;
use crate::event_writer::EventSink;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct CursorSize {
    width: u16,
    height: u16,
}

impl CursorSize {
    fn new(width: u32, height: u32) -> Result<Self> {
        if !(1..=256).contains(&width) || !(1..=256).contains(&height) {
            bail!("cursor dimensions must be between 1 and 256");
        }
        Ok(Self {
            width: u16::try_from(width)?,
            height: u16::try_from(height)?,
        })
    }

    fn byte_count(self) -> usize {
        usize::from(self.width) * usize::from(self.height) * 4
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ConstraintBatch {
    #[default]
    Idle,
    Collecting {
        argb: bool,
    },
}

pub(crate) struct Cursor {
    pub(crate) source_manager:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    pub(crate) capture_manager:
        Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    pub(crate) pointer_session:
        Option<ext_image_copy_capture_cursor_session_v1::ExtImageCopyCaptureCursorSessionV1>,
    pub(crate) session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    pub(crate) frame: Option<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    mapping: Option<MmapMut>,
    size: Option<CursorSize>,
    has_pointer: bool,
    pub(crate) batch_width: Option<u32>,
    pub(crate) batch_height: Option<u32>,
    constraint_batch: ConstraintBatch,
    transform_normal: bool,
    published_shape: Option<CursorShape>,
    shapes: ShapeTable,
    /// `None` until the compositor has reported the cursor either way.
    visibility: Option<CursorVisibility>,
    /// `None` until the cursor capture session has reported a position.
    position: Option<CursorPosition>,
    events: EventSink,
}

impl Cursor {
    pub(crate) fn new(events: EventSink, shapes: ShapeTable) -> Self {
        Self {
            source_manager: None,
            capture_manager: None,
            pointer_session: None,
            session: None,
            frame: None,
            buffer: None,
            mapping: None,
            size: None,
            has_pointer: false,
            batch_width: None,
            batch_height: None,
            constraint_batch: ConstraintBatch::Idle,
            transform_normal: false,
            published_shape: None,
            shapes,
            visibility: None,
            position: None,
            events,
        }
    }

    pub(crate) fn seat_capabilities(&mut self, capabilities: WEnum<wl_seat::Capability>) {
        self.has_pointer = match capabilities {
            WEnum::Value(value) => value.contains(wl_seat::Capability::Pointer),
            WEnum::Unknown(_) => false,
        };
    }

    pub(crate) fn start(
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

    pub(crate) fn begin_constraints(&mut self) {
        if self.constraint_batch == ConstraintBatch::Idle {
            self.batch_width = None;
            self.batch_height = None;
            self.constraint_batch = ConstraintBatch::Collecting { argb: false };
        }
    }

    pub(crate) fn accept_argb(&mut self) {
        self.begin_constraints();
        self.constraint_batch = ConstraintBatch::Collecting { argb: true };
    }

    pub(crate) fn finish_constraints(
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
        self.allocate(shm, CursorSize::new(width, height)?, qh)?;
        self.request(qh)
    }

    fn allocate(
        &mut self,
        shm: &wl_shm::WlShm,
        size: CursorSize,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let length = size.byte_count();
        if self.buffer.is_some() && self.size == Some(size) {
            return Ok(());
        }
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.mapping = None;
        let fd = memfd_create(c"waywire-cursor", MFdFlags::MFD_CLOEXEC)?;
        ftruncate(&fd, i64::try_from(length)?)?;
        let file = File::from(fd);
        // SAFETY: the map owns a duplicate of the valid memfd and uses its current length.
        let mapping = unsafe { MmapOptions::new().len(length).map_mut(&file)? };
        let pool = shm.create_pool(file.as_fd(), i32::try_from(length)?, qh, ());
        let buffer = pool.create_buffer(
            0,
            i32::from(size.width),
            i32::from(size.height),
            i32::from(size.width) * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        self.mapping = Some(mapping);
        self.buffer = Some(buffer);
        self.size = Some(size);
        Ok(())
    }

    pub(crate) fn request(&mut self, qh: &QueueHandle<State>) -> Result<()> {
        if self.frame.is_some() {
            bail!("cursor frame already pending");
        }
        let session = self
            .session
            .as_ref()
            .context("cursor session unavailable")?;
        let buffer = self.buffer.as_ref().context("cursor buffer unavailable")?;
        let size = self.size.context("cursor size unavailable")?;
        let frame = session.create_frame(qh, ());
        frame.attach_buffer(buffer);
        frame.damage_buffer(0, 0, i32::from(size.width), i32::from(size.height));
        frame.capture();
        self.transform_normal = false;
        self.frame = Some(frame);
        Ok(())
    }

    pub(crate) fn frame_ready(&mut self, qh: &QueueHandle<State>) -> Result<()> {
        if !self.transform_normal {
            bail!("cursor frame omitted normal transform");
        }
        self.destroy_frame();
        let pixels = self
            .mapping
            .as_ref()
            .context("cursor mapping unavailable")?;
        let size = self.size.context("cursor size unavailable")?;
        let key = ImageKey::new(size, pixels);
        let shape = self.shapes.get(&key).unwrap_or_else(|| {
            debug!(
                width = size.width,
                height = size.height,
                "captured cursor image has no shape match"
            );
            CursorShape::Default
        });
        if self.published_shape != Some(shape) {
            self.events.send(&Event::CursorShape(shape))?;
            self.published_shape = Some(shape);
        }
        self.request(qh)
    }

    pub(crate) fn frame_failed(
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
                let size = self
                    .size
                    .context("cursor size unavailable after frame failure")?;
                self.allocate(shm, size, qh)?;
                self.request(qh)
            }
        }
    }

    pub(crate) fn transform(&mut self, transform: WEnum<wl_output::Transform>) -> Result<()> {
        if transform != WEnum::Value(wl_output::Transform::Normal) {
            bail!("cursor transform is not normal");
        }
        self.transform_normal = true;
        Ok(())
    }

    /// The protocol reports positions from the transformed capture buffer's top-left in pixels;
    /// an output capture source therefore uses output pixels.
    pub(crate) fn position(&mut self, position: CursorPosition) -> Result<()> {
        if self.position == Some(position) {
            return Ok(());
        }
        self.events.send(&Event::CursorPosition(position))?;
        self.position = Some(position);
        Ok(())
    }

    pub(crate) fn visibility(&mut self, visibility: CursorVisibility) -> Result<()> {
        if self.visibility == Some(visibility) {
            return Ok(());
        }
        if visibility == CursorVisibility::Visible {
            self.published_shape = None;
        }
        self.events.send(&Event::CursorVisibility(visibility))?;
        self.visibility = Some(visibility);
        Ok(())
    }

    pub(crate) fn destroy_frame(&mut self) {
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
    fn cursor_size_enforces_capture_and_theme_bounds() {
        let size = CursorSize::new(256, 256).expect("maximum cursor size should be valid");
        assert_eq!(size.byte_count(), 256 * 256 * 4);
        assert!(CursorSize::new(0, 1).is_err());
        assert!(CursorSize::new(1, 0).is_err());
        assert!(CursorSize::new(257, 1).is_err());
        assert!(CursorSize::new(1, 257).is_err());
    }

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
