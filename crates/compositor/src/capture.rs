//! ext-image-copy-capture-v1 capture, served from the already-rendered frame.
//!
//! Standard screenshot clients such as grim bind the output capture source
//! manager and the image copy capture manager, create a session from an output
//! source, and request frames into `wl_shm` buffers. Pending frames queue here
//! and are completed at the next render tick by copying pixels out of the
//! rendered Pixman image, so captures never trigger an extra render or change
//! video frame pacing. The cursor is never drawn into frames, so captures are
//! always cursor-free.

use std::time::Duration;

use smithay::output::Output;
use smithay::output::WeakOutput;
use smithay::reexports::pixman::Image;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::Transform;
use smithay::wayland::image_capture_source::ImageCaptureSource;
use smithay::wayland::image_capture_source::ImageCaptureSourceHandler;
use smithay::wayland::image_capture_source::OutputCaptureSourceHandler;
use smithay::wayland::image_capture_source::OutputCaptureSourceState;
use smithay::wayland::image_copy_capture::BufferConstraints;
use smithay::wayland::image_copy_capture::CaptureFailureReason;
use smithay::wayland::image_copy_capture::Frame;
use smithay::wayland::image_copy_capture::FrameRef;
use smithay::wayland::image_copy_capture::ImageCopyCaptureHandler;
use smithay::wayland::image_copy_capture::ImageCopyCaptureState;
use smithay::wayland::image_copy_capture::Session;
use smithay::wayland::image_copy_capture::SessionRef;
use smithay::wayland::shm::BufferData;
use smithay::wayland::shm::with_buffer_contents_mut;

use super::State;

/// Captured desktop state: the capture protocol globals and their sessions.
pub(crate) struct Capture {
    output_capture_source: OutputCaptureSourceState,
    image_copy_capture_state: ImageCopyCaptureState,
    sessions: Vec<Session>,
    pending: Vec<Frame>,
}

impl Capture {
    pub(crate) fn new(display_handle: &DisplayHandle) -> Self {
        Self {
            output_capture_source: OutputCaptureSourceState::new::<State>(display_handle),
            image_copy_capture_state: ImageCopyCaptureState::new::<State>(display_handle),
            sessions: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Re-send buffer constraints after the output was resized, so long-lived
    /// sessions learn the new buffer size before allocating their next frame.
    pub(crate) fn output_resized(&mut self) {
        for session in &self.sessions {
            let Some(constraints) = session_constraints(session) else {
                continue;
            };
            if session
                .current_constraints()
                .is_some_and(|current| current.size != constraints.size)
            {
                session.update_constraints(constraints);
            }
        }
    }
}

fn output_constraints(output: &Output) -> Option<BufferConstraints> {
    let mode = output.current_mode()?;
    Some(BufferConstraints {
        // The render buffer matches the mode one-to-one; scale and transform
        // only affect how clients see the output, not the rendered pixels.
        size: mode.size.to_logical(1).to_buffer(1, Transform::Normal),
        // ARGB8888 and XRGB8888 are the same byte layout as the rendered
        // image on little-endian hosts, so frames copy without conversion.
        shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
    })
}

fn session_constraints(session: &SessionRef) -> Option<BufferConstraints> {
    let output = session
        .source()
        .user_data()
        .get::<WeakOutput>()?
        .upgrade()?;
    output_constraints(&output)
}

fn shm_dimensions(shm: &BufferData) -> Option<(usize, usize, usize, usize)> {
    Some((
        usize::try_from(shm.width).ok()?,
        usize::try_from(shm.height).ok()?,
        usize::try_from(shm.stride).ok()?,
        usize::try_from(shm.offset).ok()?,
    ))
}

impl OutputCaptureSourceHandler for State {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.capture.output_capture_source
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCaptureSourceHandler for State {}

impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.capture.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let output = source.user_data().get::<WeakOutput>()?.upgrade()?;
        output_constraints(&output)
    }

    fn new_session(&mut self, session: Session) {
        // Owned sessions keep capture alive; dropping one would send `stopped`.
        self.capture.sessions.push(session);
    }

    fn frame(&mut self, _session: &SessionRef, frame: Frame) {
        // Complete from the next already-rendered frame instead of rendering.
        self.capture.pending.push(frame);
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        self.capture.pending.retain(|pending| *pending != frame);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.capture.sessions.retain(|owned| *owned != session);
    }
}

/// Complete queued capture frames by copying the rendered image into the
/// clients' `wl_shm` buffers. Call once per rendered frame, after rendering.
pub(crate) fn serve_pending(state: &mut State, image: &Image<'static, 'static>) {
    if state.capture.pending.is_empty() {
        return;
    }
    let width = image.width();
    let height = image.height();
    let stride = image.stride();
    // SAFETY: create_buffer owns and initializes the complete image storage;
    // the renderer's exclusive target borrow has ended before this read.
    let data = unsafe { image.data() };
    // SAFETY: stride * height covers this live image's allocation.
    let pixels = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), stride * height) };
    let presented = monotonic();
    for frame in std::mem::take(&mut state.capture.pending) {
        let buffer = frame.buffer();
        let copied = with_buffer_contents_mut(&buffer, |pool, pool_len, shm| {
            copy_pixels(pool, pool_len, shm, pixels, width, height, stride)
        });
        match copied {
            Ok(Ok(())) => {
                // Without damage regions the client assumes a full redraw.
                frame.success(Transform::Normal, None, presented);
            }
            Ok(Err(reason)) => frame.fail(reason),
            // A bad pool was already reported to the client by Smithay.
            Err(_) => frame.fail(CaptureFailureReason::Unknown),
        }
    }
}

fn copy_pixels(
    pool: *mut u8,
    pool_len: usize,
    shm: BufferData,
    pixels: &[u8],
    width: usize,
    height: usize,
    src_stride: usize,
) -> Result<(), CaptureFailureReason> {
    let row_bytes = width * 4;
    if !matches!(
        shm.format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    ) {
        return Err(CaptureFailureReason::BufferConstraints);
    }
    let dimensions = shm_dimensions(&shm);
    let Some((shm_width, shm_height, shm_stride, offset)) = dimensions else {
        return Err(CaptureFailureReason::BufferConstraints);
    };
    if shm_width < width || shm_height < height {
        return Err(CaptureFailureReason::BufferConstraints);
    }
    let end = offset
        .checked_add(
            shm_stride
                .checked_mul(height - 1)
                .ok_or(CaptureFailureReason::BufferConstraints)?,
        )
        .and_then(|last_row| last_row.checked_add(row_bytes))
        .ok_or(CaptureFailureReason::BufferConstraints)?;
    if end > pool_len {
        return Err(CaptureFailureReason::BufferConstraints);
    }
    // SAFETY: the pool pointer maps pool_len bytes of client memory for the
    // duration of this callback, and offset..end stays inside it.
    let pool = unsafe { std::slice::from_raw_parts_mut(pool, pool_len) };
    for row in 0..height {
        let src = &pixels[row * src_stride..row * src_stride + row_bytes];
        let dst = offset + row * shm_stride;
        pool[dst..dst + row_bytes].copy_from_slice(src);
    }
    Ok(())
}

fn monotonic() -> Duration {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(
        Duration::ZERO,
        |timestamp| {
            Duration::new(
                u64::try_from(timestamp.tv_sec()).unwrap_or(0),
                u32::try_from(timestamp.tv_nsec()).unwrap_or(0),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_source_constrains_capture_to_the_rendered_buffer() {
        let (_display, mut state) = crate::tests::test_state();
        let output = state.output.clone();
        let source = ImageCaptureSource::new();
        state.output_source_created(source.clone(), &output);
        let constraints = state
            .capture_constraints(&source)
            .expect("output source accepted");
        assert_eq!(constraints.size.w, 1920);
        assert_eq!(constraints.size.h, 1080);
        assert!(constraints.shm.contains(&wl_shm::Format::Argb8888));
        assert!(constraints.shm.contains(&wl_shm::Format::Xrgb8888));
        // A source the compositor never attached an output to is rejected.
        assert!(
            state
                .capture_constraints(&ImageCaptureSource::new())
                .is_none()
        );
    }
}
