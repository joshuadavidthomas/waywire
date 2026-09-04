use std::{ffi::CString, fs::File, os::fd::AsFd};

use anyhow::{Context, Result, bail};
use memmap2::{MmapMut, MmapOptions};
use nix::{
    sys::memfd::{MFdFlags, memfd_create},
    unistd::ftruncate,
};
use wayland_client::{
    QueueHandle, WEnum,
    protocol::{wl_buffer, wl_output, wl_seat, wl_shm},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::ext_output_image_capture_source_manager_v1,
    image_copy_capture::v1::client::{
        ext_image_copy_capture_cursor_session_v1, ext_image_copy_capture_frame_v1,
        ext_image_copy_capture_manager_v1, ext_image_copy_capture_session_v1,
    },
};

use super::State;
use crate::{
    event_writer::EventSink,
    protocol::{Event, cursor_byte_count},
};

const MAX_CONSECUTIVE_CURSOR_FAILURES: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Hotspot {
    x: i32,
    y: i32,
}

pub struct Cursor {
    pub cursor_session:
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
    pub(super) batch_argb: bool,
    collecting_constraints: bool,
    pending_hotspot: Hotspot,
    committed_hotspot: Hotspot,
    transform_normal: bool,
    force_publish: bool,
    published: Option<(u32, u32, Hotspot, Vec<u8>)>,
    visibility: Option<bool>,
    events: EventSink,
    consecutive_unknown_failures: u8,
    consecutive_constraint_failures: u8,
}

impl Cursor {
    pub fn new(events: EventSink) -> Self {
        Self {
            cursor_session: None,
            session: None,
            frame: None,
            buffer: None,
            mapping: None,
            width: 0,
            height: 0,
            has_pointer: false,
            batch_width: None,
            batch_height: None,
            batch_argb: false,
            collecting_constraints: false,
            pending_hotspot: Hotspot::default(),
            committed_hotspot: Hotspot::default(),
            transform_normal: false,
            force_publish: true,
            published: None,
            visibility: None,
            events,
            consecutive_unknown_failures: 0,
            consecutive_constraint_failures: 0,
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
        source_manager: &ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
        capture_manager: &ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        if !self.has_pointer {
            bail!("seat has no pointer after virtual pointer creation");
        }
        let source = source_manager.create_source(output, qh, ());
        let pointer = seat.get_pointer(qh, ());
        let cursor_session =
            capture_manager.create_pointer_cursor_session(&source, &pointer, qh, ());
        let session = cursor_session.get_capture_session(qh, super::capture::CaptureTarget::Cursor);
        source.destroy();
        pointer.release();
        self.cursor_session = Some(cursor_session);
        self.session = Some(session);
        Ok(())
    }

    pub fn begin_constraints(&mut self) {
        if !self.collecting_constraints {
            self.batch_width = None;
            self.batch_height = None;
            self.batch_argb = false;
            self.collecting_constraints = true;
        }
    }

    pub fn finish_constraints(
        &mut self,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        self.collecting_constraints = false;
        let width = self
            .batch_width
            .take()
            .context("cursor constraints omitted width")?;
        let height = self
            .batch_height
            .take()
            .context("cursor constraints omitted height")?;
        if !std::mem::take(&mut self.batch_argb) {
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
        // The previous frame may have been cancelled before Ready. Its storage
        // must not become the destination of another capture.
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.mapping = None;
        let name = CString::new("sprite-desktop-cursor").unwrap();
        let fd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC)?;
        ftruncate(&fd, i64::try_from(length)?)?;
        let file = File::from(fd);
        // The map and Wayland buffer both retain the memfd storage independently.
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
        let frame = session.create_frame(qh, super::capture::CaptureTarget::Cursor);
        frame.attach_buffer(buffer);
        frame.damage_buffer(0, 0, self.width as i32, self.height as i32);
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
            self.events.send(Event::CursorImage {
                width: self.width,
                height: self.height,
                hotspot_x: self.committed_hotspot.x,
                hotspot_y: self.committed_hotspot.y,
                bgra: pixels.clone(),
            })?;
            self.published = Some((self.width, self.height, self.committed_hotspot, pixels));
            self.force_publish = false;
        }
        self.consecutive_unknown_failures = 0;
        self.consecutive_constraint_failures = 0;
        self.request(qh)
    }

    pub fn frame_failed(
        &mut self,
        reason: WEnum<ext_image_copy_capture_frame_v1::FailureReason>,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        self.destroy_frame();
        match reason {
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints) => {
                Self::record_failure(
                    &mut self.consecutive_constraint_failures,
                    "cursor capture buffer constraints failed repeatedly",
                )?;
                if self.collecting_constraints {
                    Ok(())
                } else {
                    self.allocate(shm, self.width, self.height, qh)?;
                    self.request(qh)
                }
            }
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::Unknown) => {
                Self::record_failure(
                    &mut self.consecutive_unknown_failures,
                    "cursor capture failed repeatedly",
                )?;
                self.request(qh)
            }
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::Stopped) => {
                bail!("cursor capture session stopped")
            }
            WEnum::Unknown(value) => {
                Self::record_failure(
                    &mut self.consecutive_unknown_failures,
                    "cursor capture failed repeatedly with an unknown reason",
                )?;
                eprintln!(
                    "sprite-desktop-streamd: cursor capture failed with unknown reason {value}"
                );
                self.request(qh)
            }
            _ => bail!("cursor capture failed with an unsupported reason"),
        }
    }

    fn record_failure(counter: &mut u8, repeated_message: &'static str) -> Result<()> {
        *counter = counter.saturating_add(1);
        if *counter > MAX_CONSECUTIVE_CURSOR_FAILURES {
            bail!(repeated_message);
        }
        Ok(())
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
        if self.visibility == Some(visible) {
            return Ok(());
        }
        if visible {
            self.force_publish = true;
        }
        self.events.send(Event::CursorVisibility(visible))?;
        self.visibility = Some(visible);
        Ok(())
    }

    pub fn destroy_frame(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.transform_normal = false;
    }
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
        seat: wl_seat::WlSeat,
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
            seat: registry.bind(2, 1, &qh, ()),
            source_manager: registry.bind(3, 1, &qh, ()),
            capture_manager: registry.bind(4, 1, &qh, ()),
            shm: registry.bind(5, 1, &qh, ()),
            _connection: connection,
            _server: server,
            qh,
        }
    }

    fn configured_cursor(objects: &ClientObjects) -> Cursor {
        let mut cursor = Cursor::new(EventSink::test_sink());
        cursor.seat_capabilities(WEnum::Value(wl_seat::Capability::Pointer));
        cursor
            .start(
                &objects.seat,
                &objects.output,
                &objects.source_manager,
                &objects.capture_manager,
                &objects.qh,
            )
            .unwrap();
        cursor.begin_constraints();
        cursor.batch_width = Some(2);
        cursor.batch_height = Some(2);
        cursor.batch_argb = true;
        cursor
            .finish_constraints(&objects.shm, &objects.qh)
            .unwrap();
        cursor
    }

    fn reason(
        reason: ext_image_copy_capture_frame_v1::FailureReason,
    ) -> WEnum<ext_image_copy_capture_frame_v1::FailureReason> {
        WEnum::Value(reason)
    }

    #[test]
    fn unknown_failures_retry_with_a_bound_and_ready_resets_the_bound() {
        let objects = client_objects();
        let mut cursor = configured_cursor(&objects);

        cursor
            .frame_failed(
                reason(ext_image_copy_capture_frame_v1::FailureReason::Unknown),
                &objects.shm,
                &objects.qh,
            )
            .unwrap();
        assert_eq!(cursor.consecutive_unknown_failures, 1);
        cursor
            .frame_failed(
                reason(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints),
                &objects.shm,
                &objects.qh,
            )
            .unwrap();
        assert_eq!(cursor.consecutive_constraint_failures, 1);
        let retried_frame = cursor.frame.as_ref().unwrap().id();

        cursor
            .transform(WEnum::Value(wl_output::Transform::Normal))
            .unwrap();
        cursor.frame_ready(&objects.qh).unwrap();
        assert_eq!(cursor.consecutive_unknown_failures, 0);
        assert_eq!(cursor.consecutive_constraint_failures, 0);
        assert_ne!(cursor.frame.as_ref().unwrap().id(), retried_frame);

        for _ in 0..MAX_CONSECUTIVE_CURSOR_FAILURES {
            cursor
                .frame_failed(
                    reason(ext_image_copy_capture_frame_v1::FailureReason::Unknown),
                    &objects.shm,
                    &objects.qh,
                )
                .unwrap();
            assert!(cursor.frame.is_some());
        }
        assert!(
            cursor
                .frame_failed(
                    reason(ext_image_copy_capture_frame_v1::FailureReason::Unknown),
                    &objects.shm,
                    &objects.qh,
                )
                .is_err()
        );
        assert!(cursor.frame.is_none());
    }

    #[test]
    fn unknown_enum_failures_are_bounded() {
        let objects = client_objects();
        let mut cursor = configured_cursor(&objects);

        for _ in 0..MAX_CONSECUTIVE_CURSOR_FAILURES {
            cursor
                .frame_failed(WEnum::Unknown(99), &objects.shm, &objects.qh)
                .unwrap();
            assert!(cursor.frame.is_some());
        }
        assert!(
            cursor
                .frame_failed(WEnum::Unknown(99), &objects.shm, &objects.qh)
                .is_err()
        );
        assert!(cursor.frame.is_none());
    }

    #[test]
    fn same_size_constraint_batch_retires_pending_buffer() {
        let objects = client_objects();
        let mut cursor = configured_cursor(&objects);
        let old_buffer = cursor.buffer.as_ref().unwrap().id();
        cursor.begin_constraints();
        cursor.batch_width = Some(2);
        cursor.batch_height = Some(2);
        cursor.batch_argb = true;
        cursor
            .finish_constraints(&objects.shm, &objects.qh)
            .unwrap();
        assert_ne!(cursor.buffer.as_ref().unwrap().id(), old_buffer);
        assert!(cursor.frame.is_some());
    }

    #[test]
    fn stopped_failure_is_terminal_without_another_request() {
        let objects = client_objects();
        let mut cursor = configured_cursor(&objects);

        let error = cursor
            .frame_failed(
                reason(ext_image_copy_capture_frame_v1::FailureReason::Stopped),
                &objects.shm,
                &objects.qh,
            )
            .unwrap_err();

        assert_eq!(error.to_string(), "cursor capture session stopped");
        assert!(cursor.frame.is_none());
    }

    #[test]
    fn constraint_failures_reallocate_or_wait_for_the_batch_and_are_bounded() {
        let objects = client_objects();
        let mut cursor = configured_cursor(&objects);
        let old_buffer = cursor.buffer.as_ref().unwrap().id();

        cursor
            .frame_failed(
                reason(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints),
                &objects.shm,
                &objects.qh,
            )
            .unwrap();
        assert_ne!(cursor.buffer.as_ref().unwrap().id(), old_buffer);
        assert!(cursor.frame.is_some());

        cursor.begin_constraints();
        cursor.batch_width = Some(3);
        cursor.batch_height = Some(4);
        cursor.batch_argb = true;
        cursor
            .frame_failed(
                reason(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints),
                &objects.shm,
                &objects.qh,
            )
            .unwrap();
        assert!(cursor.frame.is_none());
        cursor
            .finish_constraints(&objects.shm, &objects.qh)
            .unwrap();
        assert_eq!((cursor.width, cursor.height), (3, 4));
        assert!(cursor.frame.is_some());

        while cursor.consecutive_constraint_failures <= MAX_CONSECUTIVE_CURSOR_FAILURES {
            if cursor
                .frame_failed(
                    reason(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints),
                    &objects.shm,
                    &objects.qh,
                )
                .is_err()
            {
                break;
            }
        }
        assert_eq!(
            cursor.consecutive_constraint_failures,
            MAX_CONSECUTIVE_CURSOR_FAILURES + 1
        );
        assert!(cursor.frame.is_none());
    }
}
