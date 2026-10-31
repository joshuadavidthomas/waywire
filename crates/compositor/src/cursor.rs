//! Cursor surfaces are sent separately from the cursor-free video stream.
use cairo::Context;
use cairo::Format;
use cairo::ImageSurface;
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::input::pointer::CursorImageSurfaceData;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shm::with_buffer_contents;
use waywire_protocol::pipe::CursorImage;
use waywire_protocol::pipe::CursorVisibility;
use waywire_protocol::pipe::Event;

use crate::State;

fn image(surface: &WlSurface, output_width: u16) -> Option<CursorImage> {
    with_states(surface, |states| {
        let renderer = states
            .data_map
            .get::<RendererSurfaceStateUserData>()?
            .lock()
            .ok()?;
        let buffer = renderer.buffer()?;
        let logical = renderer.surface_size()?;
        let hotspot = states
            .data_map
            .get::<CursorImageSurfaceData>()?
            .lock()
            .ok()?
            .hotspot;
        if logical.w <= 0 || logical.h <= 0 {
            return None;
        }
        let max_side = i64::from(logical.w.max(logical.h));
        let target = max_side.min(128);
        let scale =
            |value: i32| i32::try_from((i64::from(value) * target + max_side / 2) / max_side).ok();
        let width = scale(logical.w)?.max(1);
        let height = scale(logical.h)?.max(1);
        let hotspot_x = scale(hotspot.x)?.clamp(0, width - 1);
        let hotspot_y = scale(hotspot.y)?.clamp(0, height - 1);
        let (data, source_width, source_height, stride, format) =
            with_buffer_contents(buffer, |pool, len, data| {
                let width = usize::try_from(data.width).ok()?;
                let height = usize::try_from(data.height).ok()?;
                let stride = usize::try_from(data.stride).ok()?;
                let offset = usize::try_from(data.offset).ok()?;
                if width == 0
                    || height == 0
                    || width > 2048
                    || height > 2048
                    || stride < width.checked_mul(4)?
                {
                    return None;
                }
                let bytes = stride.checked_mul(height)?;
                if bytes > 16 * 1024 * 1024 || offset.checked_add(bytes)? > len {
                    return None;
                }
                let format = if data.format == wl_shm::Format::Argb8888 {
                    Format::ARgb32
                } else if data.format == wl_shm::Format::Xrgb8888 {
                    Format::Rgb24
                } else {
                    return None;
                };
                let mut pixels = vec![0; bytes];
                // SAFETY: the validated range is within the mapped shared-memory pool.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pool.wrapping_add(offset),
                        pixels.as_mut_ptr(),
                        bytes,
                    );
                }
                Some((pixels, data.width, data.height, data.stride, format))
            })
            .ok()??;
        let source =
            ImageSurface::create_for_data(data, format, source_width, source_height, stride)
                .ok()?;
        let target = ImageSurface::create(Format::ARgb32, width, height).ok()?;
        let context = Context::new(&target).ok()?;
        context.scale(
            f64::from(width) / f64::from(source_width),
            f64::from(height) / f64::from(source_height),
        );
        context.set_source_surface(&source, 0.0, 0.0).ok()?;
        context.paint().ok()?;
        let mut png = Vec::new();
        target.write_to_png(&mut png).ok()?;
        Some(CursorImage {
            width: u8::try_from(width).ok()?,
            height: u8::try_from(height).ok()?,
            hotspot_x: u8::try_from(hotspot_x).ok()?,
            hotspot_y: u8::try_from(hotspot_y).ok()?,
            output_width,
            png,
        })
    })
}

impl State {
    pub(crate) fn update_cursor_image(&mut self) {
        let Some(surface) = &self.cursor_surface else {
            return;
        };
        let Some(output_width) = self
            .space
            .output_geometry(&self.output)
            .and_then(|geometry| u16::try_from(geometry.size.w).ok())
        else {
            return;
        };
        let Some(image) = image(surface, output_width) else {
            self.cursor_image = None;
            self.emit(Event::CursorVisibility(CursorVisibility::Hidden));
            return;
        };
        if self.cursor_image.as_ref() != Some(&image) {
            self.cursor_image = Some(image.clone());
            self.emit(Event::CursorImage(image));
        }
        self.emit(Event::CursorVisibility(CursorVisibility::Visible));
    }

    pub(crate) fn send_cursor_frame(&self) {
        if let Some(surface) = &self.cursor_surface {
            smithay::desktop::utils::send_frames_surface_tree(
                surface,
                &self.output,
                self.start.elapsed(),
                Some(std::time::Duration::ZERO),
                |_, _| Some(self.output.clone()),
            );
        }
    }
}
