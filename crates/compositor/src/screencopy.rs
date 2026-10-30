//! wlr-screencopy-unstable-v1 capture, served from the already-rendered frame.
//!
//! Packaged screenshot tools such as Debian's grim 1.4 speak only this protocol,
//! and Smithay provides no server state for it, so the manager and frame objects
//! are implemented here. The design follows niri's
//! `src/protocols/screencopy.rs`: capture requests resolve to a physical output
//! rectangle, clients copy into buffers matching the advertised parameters, and
//! copies complete from the next already-rendered frame instead of triggering a
//! render, so captures never change video frame pacing. Unlike niri there is no
//! damage tracking, per-manager queueing or dmabuf: waywire has one output,
//! accepts only `wl_shm` buffers, and completes `copy_with_damage` at the next
//! render tick like `copy`, reporting the full region as damage. The cursor is
//! never drawn into frames, so `overlay_cursor` is ignored and captures are
//! always cursor-free.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::pixman::Image;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::Flags;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::DataInit;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::New;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::Physical;
use smithay::utils::Point;
use smithay::utils::Rectangle;
use smithay::utils::Size;
use smithay::utils::Transform;
use smithay::wayland::Dispatch2;
use smithay::wayland::GlobalDispatch2;
use smithay::wayland::shm;
use smithay::wayland::shm::with_buffer_contents_mut;

use super::State;
use super::capture::copy_pixels;
use super::capture::image_pixels;
use super::capture::monotonic;

/// Protocol version: `buffer_done` and the version 3 buffer negotiation.
const VERSION: u32 = 3;

/// Capture protocol global and the copies waiting for the next rendered frame.
pub(crate) struct Screencopy {
    pending: Vec<PendingCopy>,
}

impl Screencopy {
    pub(crate) fn new(display_handle: &DisplayHandle) -> Self {
        display_handle
            .create_global::<State, ZwlrScreencopyManagerV1, ManagerData>(VERSION, ManagerData);
        Self {
            pending: Vec::new(),
        }
    }
}

/// Global data for the screencopy manager; waywire has one output and no access
/// filter, so nothing is tracked per binding.
#[derive(Default)]
struct ManagerData;

/// Resource data for a frame, mirroring niri's `ScreencopyFrameState`.
enum FrameData {
    /// The capture request was rejected before the frame could serve a copy.
    Failed,
    Pending {
        /// Physical render-buffer rectangle this frame copies.
        region: Rectangle<i32, Physical>,
        copied: AtomicBool,
    },
}

/// A copy waiting for the next rendered frame.
struct PendingCopy {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
    region: Rectangle<i32, Physical>,
    with_damage: bool,
    /// Set once the frame received `ready` or `failed`; the protocol forbids
    /// events after either, so the drop guard below stays silent afterwards.
    submitted: bool,
}

impl Drop for PendingCopy {
    fn drop(&mut self) {
        if !self.submitted {
            // A queued copy outlived by the compositor still owes the client a
            // verdict. Sending to a destroyed frame is a no-op.
            self.frame.failed();
        }
    }
}

/// Resolve a capture region from output logical coordinates (see
/// `xdg_output.logical_size`) to the physical render-buffer rectangle, clipped
/// to the output. Returns `None` when the region misses the output entirely.
fn region_rect(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: f64,
    transform: Transform,
    output_size: Size<i32, Physical>,
) -> Option<Rectangle<i32, Physical>> {
    let physical = Rectangle::new(Point::from((x, y)), Size::from((width, height)))
        .to_physical_precise_round::<_, i32>(scale);
    let transformed_size = transform.transform_size(output_size);
    let clamped = physical.intersection(Rectangle::from_size(transformed_size))?;
    Some(
        transform
            .invert()
            .transform_rect_in(clamped, &transformed_size),
    )
}

/// Unsigned extents of a clipped region, for the `buffer` and `damage` events.
#[expect(
    clippy::cast_sign_loss,
    reason = "clipped region dimensions are positive render-buffer coordinates"
)]
fn region_extents(region: Rectangle<i32, Physical>) -> (u32, u32) {
    (region.size.w as u32, region.size.h as u32)
}

/// Initialize a frame and advertise the one supported buffer type. The buffer
/// size equals the captured region; the stride assumes tightly packed BGRA.
fn start_frame(
    frame: New<ZwlrScreencopyFrameV1>,
    region: Rectangle<i32, Physical>,
    data_init: &mut DataInit<'_, State>,
) {
    let frame = data_init.init(
        frame,
        FrameData::Pending {
            region,
            copied: AtomicBool::new(false),
        },
    );
    let (width, height) = region_extents(region);
    frame.buffer(wl_shm::Format::Xrgb8888, width, height, width * 4);
    if frame.version() >= 3 {
        // All supported buffer types were enumerated (wl_shm only here).
        frame.buffer_done();
    }
}

/// Reject a capture request before its frame can serve a copy.
fn reject(frame: New<ZwlrScreencopyFrameV1>, data_init: &mut DataInit<'_, State>) {
    let frame = data_init.init(frame, FrameData::Failed);
    frame.failed();
}

impl GlobalDispatch2<ZwlrScreencopyManagerV1, State> for ManagerData {
    fn bind(
        &self,
        _state: &mut State,
        _display_handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ManagerData);
    }
}

impl Dispatch2<ZwlrScreencopyManagerV1, State> for ManagerData {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "the generated request enum is non-exhaustive across versions"
    )]
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _display_handle: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                // The compositor never draws a cursor into frames.
                overlay_cursor: _,
                output,
            } => {
                let Some(output) = Output::from_resource(&output) else {
                    reject(frame, data_init);
                    return;
                };
                let Some(mode) = output.current_mode() else {
                    reject(frame, data_init);
                    return;
                };
                // The render buffer matches the mode one-to-one, so the whole
                // capture is the mode rectangle in render-buffer coordinates.
                start_frame(frame, Rectangle::from_size(mode.size), data_init);
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor: _,
                output,
                x,
                y,
                width,
                height,
            } => {
                if width <= 0 || height <= 0 {
                    reject(frame, data_init);
                    return;
                }
                let Some(output) = Output::from_resource(&output) else {
                    reject(frame, data_init);
                    return;
                };
                let Some(mode) = output.current_mode() else {
                    reject(frame, data_init);
                    return;
                };
                let Some(region) = region_rect(
                    x,
                    y,
                    width,
                    height,
                    output.current_scale().fractional_scale(),
                    output.current_transform(),
                    mode.size,
                ) else {
                    reject(frame, data_init);
                    return;
                };
                start_frame(frame, region, data_init);
            }
            // A destructor; objects created by the manager remain valid. The
            // enum is non-exhaustive, so the wildcard also covers it and any
            // future request.
            _ => {}
        }
    }
}

impl Dispatch2<ZwlrScreencopyFrameV1, State> for FrameData {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "the generated request enum is non-exhaustive across versions"
    )]
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _display_handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        let FrameData::Pending { region, copied } = self else {
            return;
        };
        if copied.load(Ordering::SeqCst) {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "copy was already requested",
            );
            return;
        }

        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            // Destroy and any future request leave the frame alone; Destroy is
            // a destructor handled by the client.
            _ => return,
        };

        // Only buffers matching the advertised parameters are accepted.
        let valid = shm::with_buffer_contents(&buffer, |_, _, data| {
            data.format == wl_shm::Format::Xrgb8888
                && data.width == region.size.w
                && data.height == region.size.h
                && data.stride == region.size.w * 4
        })
        .unwrap_or(false);
        if !valid {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                "invalid buffer",
            );
            return;
        }

        copied.store(true, Ordering::SeqCst);
        state.screencopy.pending.push(PendingCopy {
            frame: frame.clone(),
            buffer,
            region: *region,
            with_damage,
            submitted: false,
        });
    }

    fn destroyed(&self, state: &mut State, _client: ClientId, frame: &ZwlrScreencopyFrameV1) {
        state
            .screencopy
            .pending
            .retain(|pending| pending.frame != *frame);
    }
}

/// A clipped region as `(x, y, width, height)` source coordinates. The
/// intersection in `region_rect` guarantees non-negative components.
fn region_args(region: Rectangle<i32, Physical>) -> Option<(usize, usize, usize, usize)> {
    Some((
        usize::try_from(region.loc.x).ok()?,
        usize::try_from(region.loc.y).ok()?,
        usize::try_from(region.size.w).ok()?,
        usize::try_from(region.size.h).ok()?,
    ))
}

/// Split a monotonic timestamp into the protocol's `tv_sec_hi`, `tv_sec_lo`,
/// `tv_nsec` triple.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the seconds halves are masked or shifted into 32-bit range"
)]
fn timestamp_parts(timestamp: Duration) -> (u32, u32, u32) {
    let secs = timestamp.as_secs();
    ((secs >> 32) as u32, secs as u32, timestamp.subsec_nanos())
}

/// Complete queued copies by copying the rendered image into the clients'
/// `wl_shm` buffers. Call once per rendered frame, after rendering.
pub(crate) fn serve_pending(state: &mut State, image: &Image<'static, 'static>) {
    if state.screencopy.pending.is_empty() {
        return;
    }
    let pixels = image_pixels(image);
    let presented = monotonic();
    let (tv_sec_hi, tv_sec_lo, tv_nsec) = timestamp_parts(presented);
    for mut pending in std::mem::take(&mut state.screencopy.pending) {
        let (width, height) = region_extents(pending.region);
        let copied = region_args(pending.region).is_some_and(|region| {
            with_buffer_contents_mut(&pending.buffer, |pool, pool_len, shm| {
                copy_pixels(pool, pool_len, shm, pixels, image.stride(), region)
            })
            .is_ok_and(|result| result.is_ok())
        });
        if copied {
            if pending.with_damage {
                // No damage tracking: report the whole captured region as the
                // damage since the previous copy from this manager.
                pending.frame.damage(0, 0, width, height);
            }
            // BGRA row bytes match the render buffer's orientation, so the
            // contents are not y-inverted.
            pending.frame.flags(Flags::empty());
            pending.frame.ready(tv_sec_hi, tv_sec_lo, tv_nsec);
        } else {
            pending.frame.failed();
        }
        pending.submitted = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_output_region_maps_one_to_one_at_unit_scale() {
        let output_size = Size::from((1920, 1080));
        let region = region_rect(0, 0, 1920, 1080, 1.0, Transform::Normal, output_size)
            .expect("full output intersects");
        assert_eq!(region, Rectangle::from_size(output_size));
        // An interior region passes through unscaled and unclipped.
        let region = region_rect(100, 200, 400, 300, 1.0, Transform::Normal, output_size)
            .expect("interior region intersects");
        assert_eq!(region.loc, Point::from((100, 200)));
        assert_eq!(region.size, Size::from((400, 300)));
    }

    #[test]
    fn region_scales_from_logical_to_physical_coordinates() {
        let output_size = Size::from((1920, 1080));
        let region = region_rect(0, 0, 100, 100, 1.5, Transform::Normal, output_size)
            .expect("scaled region intersects");
        assert_eq!(region.loc, Point::from((0, 0)));
        assert_eq!(region.size, Size::from((150, 150)));
    }

    #[test]
    fn region_clips_to_the_output_extents() {
        let output_size = Size::from((1920, 1080));
        let region = region_rect(1000, 900, 1920, 1080, 1.0, Transform::Normal, output_size)
            .expect("overlapping region intersects");
        assert_eq!(region.loc, Point::from((1000, 900)));
        assert_eq!(region.size, Size::from((920, 180)));
        // A region entirely beyond the output cannot capture anything.
        assert!(region_rect(2000, 0, 100, 100, 1.0, Transform::Normal, output_size).is_none());
        assert!(region_rect(-100, -100, 50, 50, 1.0, Transform::Normal, output_size).is_none());
    }

    #[test]
    fn timestamp_parts_splits_the_monotonic_clock() {
        let (hi, lo, nanos) = timestamp_parts(Duration::new(0x1_0000_0001, 42));
        assert_eq!(hi, 1);
        assert_eq!(lo, 1);
        assert_eq!(nanos, 42);
    }
}
