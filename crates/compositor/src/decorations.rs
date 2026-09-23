//! Cached software decorations; layout is shared by painting and hit testing.
use anyhow::Result;
use pangocairo::{cairo, pango};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Buffer, Logical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

use super::State;

pub(super) const BAR: i32 = 32;
pub(super) const STRIP: i32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecorationAction {
    Move,
    Resize,
    Close,
}

struct Layout(Size<i32, Logical>);

impl Layout {
    fn bar(&self) -> Rectangle<i32, Logical> {
        Rectangle::new((0, -BAR).into(), (self.0.w, BAR).into())
    }

    fn close(&self) -> Rectangle<i32, Logical> {
        Rectangle::new(
            ((self.0.w - BAR).max(0), -BAR).into(),
            (self.0.w.min(BAR), BAR).into(),
        )
    }

    fn resize(&self) -> Rectangle<i32, Logical> {
        Rectangle::new(
            ((self.0.w - 24).max(0), self.0.h).into(),
            (self.0.w.min(24), STRIP).into(),
        )
    }

    fn hit(&self, point: Point<f64, Logical>) -> Option<DecorationAction> {
        if self.close().to_f64().contains(point) {
            Some(DecorationAction::Close)
        } else if self.bar().to_f64().contains(point) {
            Some(DecorationAction::Move)
        } else if self.resize().to_f64().contains(point) {
            Some(DecorationAction::Resize)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Appearance {
    title: String,
    width: i32,
    scale: f64,
    active: bool,
    hover: Option<DecorationAction>,
    pressed: bool,
}

pub(super) struct Decoration {
    appearance: Option<Appearance>,
    buffers: [MemoryRenderBuffer; 2],
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            appearance: None,
            buffers: std::array::from_fn(|_| {
                MemoryRenderBuffer::new(Fourcc::Argb8888, (0, 0), 1, Transform::Normal, None)
            }),
        }
    }
}

impl Decoration {
    #[expect(
        clippy::float_cmp,
        reason = "cache keys compare the exact configured output scale"
    )]
    fn update(&mut self, appearance: Appearance) -> Result<bool> {
        if self.appearance.as_ref() == Some(&appearance) {
            return Ok(false);
        }
        for (buffer, height) in self.buffers.iter_mut().zip([BAR, STRIP]) {
            if self.appearance.as_ref().is_some_and(|previous| {
                previous.scale == appearance.scale
                    && previous.active == appearance.active
                    && if height == BAR {
                        previous.width == appearance.width
                            && previous.title == appearance.title
                            && previous.pressed == appearance.pressed
                            && (previous.hover == Some(DecorationAction::Close))
                                == (appearance.hover == Some(DecorationAction::Close))
                    } else {
                        previous.width.min(24) == appearance.width.min(24)
                            && (previous.hover == Some(DecorationAction::Resize))
                                == (appearance.hover == Some(DecorationAction::Resize))
                    }
            }) {
                continue;
            }
            let mut surface = paint(&appearance, height)?;
            let size = (surface.width(), surface.height()).into();
            let mut target = buffer.render();
            target.resize(size);
            target.update_opaque_regions(Some(vec![Rectangle::from_size(size)]));
            let pixels = surface.data()?;
            target.draw(|bytes| {
                bytes.copy_from_slice(&pixels);
                Ok::<_, anyhow::Error>(vec![Rectangle::from_size(size)])
            })?;
        }
        self.appearance = Some(appearance);
        Ok(true)
    }

    fn elements(
        &self,
        renderer: &mut PixmanRenderer,
        layout: &Layout,
        location: Point<i32, Logical>,
        scale: f64,
    ) -> Result<Vec<MemoryRenderBufferRenderElement<PixmanRenderer>>> {
        self.buffers
            .iter()
            .zip([layout.bar(), layout.resize()])
            .map(|(buffer, rect)| {
                let pixels = raster_size(rect.size, scale);
                Ok(MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    (location + rect.loc)
                        .to_physical_precise_round::<_, i32>(scale)
                        .to_f64(),
                    buffer,
                    None,
                    // Buffer scale is integer. Map the complete fractional-scale raster
                    // explicitly; None would crop it to the logical destination size.
                    Some(Rectangle::from_size(
                        (f64::from(pixels.w), f64::from(pixels.h)).into(),
                    )),
                    Some(rect.size),
                    Kind::Unspecified,
                )?)
            })
            .collect()
    }
}

fn raster_size(size: Size<i32, Logical>, scale: f64) -> Size<i32, Buffer> {
    let size = size.to_physical_precise_round::<_, i32>(scale);
    (size.w.max(1), size.h.max(1)).into()
}

fn color(context: &cairo::Context, rgb: u32) {
    context.set_source_rgb(
        f64::from((rgb >> 16) & 255) / 255.0,
        f64::from((rgb >> 8) & 255) / 255.0,
        f64::from(rgb & 255) / 255.0,
    );
}

fn paint(appearance: &Appearance, height: i32) -> Result<cairo::ImageSurface> {
    let width = if height == BAR {
        appearance.width
    } else {
        appearance.width.min(24)
    };
    let size: Size<i32, Logical> = (width, height).into();
    let pixels = raster_size(size, appearance.scale);
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, pixels.w, pixels.h)?;
    let context = cairo::Context::new(&surface)?;
    context.scale(
        f64::from(pixels.w) / f64::from(size.w),
        f64::from(pixels.h) / f64::from(size.h),
    );
    let (background, foreground, border) = if appearance.active {
        (0x0027_2e39, 0x00e7_ebf0, 0x0046_5164)
    } else {
        (0x0020_252d, 0x0098_a2b2, 0x0030_3743)
    };
    color(&context, background);
    context.paint()?;
    let layout = Layout((width, 0).into());
    if height == BAR {
        let close = layout.close();
        if appearance.hover == Some(DecorationAction::Close) {
            color(
                &context,
                if appearance.pressed {
                    0x85_29_36
                } else {
                    0xb8_32_45
                },
            );
            context.rectangle(
                f64::from(close.loc.x),
                0.0,
                f64::from(close.size.w),
                f64::from(BAR),
            );
            context.fill()?;
        }
        color(&context, foreground);
        draw_title(&context, &appearance.title, (close.loc.x - 24).max(0))?;
        context.save()?;
        context.rectangle(
            f64::from(close.loc.x),
            0.0,
            f64::from(close.size.w),
            f64::from(BAR),
        );
        context.clip();
        let x = f64::from(close.loc.x) + f64::from(close.size.w) / 2.0;
        color(
            &context,
            if appearance.hover == Some(DecorationAction::Close) {
                0xff_ff_ff
            } else {
                foreground
            },
        );
        context.set_line_width(1.5);
        context.set_line_cap(cairo::LineCap::Round);
        context.move_to(x - 4.0, 12.0);
        context.line_to(x + 4.0, 20.0);
        context.move_to(x + 4.0, 12.0);
        context.line_to(x - 4.0, 20.0);
        context.stroke()?;
        context.restore()?;
    } else {
        color(
            &context,
            if appearance.hover == Some(DecorationAction::Resize) {
                0xe7_eb_f0
            } else {
                0x76_83_99
            },
        );
        context.set_line_width(1.0);
        let right = f64::from(layout.resize().loc.x + layout.resize().size.w) - 4.0;
        for offset in [0.0, 4.0, 8.0] {
            context.move_to(right - offset - 3.0, 5.5);
            context.line_to(right - offset, 2.5);
        }
        context.stroke()?;
    }
    color(&context, border);
    context.rectangle(0.0, f64::from(height - 1), f64::from(width), 1.0);
    context.fill()?;
    context.status()?;
    Ok(surface)
}

fn draw_title(context: &cairo::Context, title: &str, width: i32) -> Result<()> {
    if width == 0 {
        return Ok(());
    }
    // Absolute logical-pixel size avoids depending on the host's font DPI.
    let mut font = pango::FontDescription::from_string("Sans");
    font.set_absolute_size(13.0 * f64::from(pango::SCALE));
    let text = pangocairo::functions::create_layout(context);
    let mut options = cairo::FontOptions::new()?;
    options.set_antialias(cairo::Antialias::Gray);
    pangocairo::functions::context_set_font_options(&text.context(), Some(&options));
    text.set_font_description(Some(&font));
    text.set_single_paragraph_mode(true);
    text.set_ellipsize(pango::EllipsizeMode::End);
    text.set_width(width * pango::SCALE);
    text.set_text(title);
    context.save()?;
    context.rectangle(12.0, 0.0, f64::from(width), f64::from(BAR));
    context.clip();
    context.move_to(12.0, f64::from(BAR - text.pixel_size().1) / 2.0);
    pangocairo::functions::show_layout(context, &text);
    context.restore()?;
    Ok(())
}

smithay::backend::renderer::element::render_elements! {
    pub(super) SceneElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Decoration=MemoryRenderBufferRenderElement<R>,
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, top: ToplevelSurface) {
        self.request_mode(top, Mode::ServerSide);
    }
    fn request_mode(&mut self, top: ToplevelSurface, mode: Mode) {
        top.with_pending_state(|state| state.decoration_mode = Some(mode));
        top.send_pending_configure();
        self.dirty = true;
    }
    fn unset_mode(&mut self, top: ToplevelSurface) {
        self.request_mode(top, Mode::ServerSide);
    }
}

pub(super) fn decorated(window: &Window) -> bool {
    if let Some(top) = window.toplevel() {
        top.with_pending_state(|state| {
            state.decoration_mode == Some(Mode::ServerSide)
                && !state.states.contains(xdg_toplevel::State::Fullscreen)
        })
    } else {
        window
            .x11_surface()
            .is_some_and(|x| !x.is_override_redirect() && !x.is_decorated() && !x.is_fullscreen())
    }
}

impl State {
    pub(super) fn decoration_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(Window, DecorationAction)> {
        for window in self.space.elements().rev() {
            let location = self.space.element_location(window)?;
            let surface_location = location - window.geometry().loc;
            if window
                .surface_under(point - surface_location.to_f64(), WindowSurfaceType::ALL)
                .is_some()
            {
                return None;
            }
            if !decorated(window)
                || window.geometry().size.is_empty()
                || !self.windows.get(window).is_some_and(|state| state.mapped)
            {
                continue;
            }
            if let Some(action) = Layout(window.geometry().size).hit(point - location.to_f64()) {
                return Some((window.clone(), action));
            }
        }
        None
    }

    pub(super) fn scene_elements(
        &mut self,
        renderer: &mut PixmanRenderer,
    ) -> Result<Vec<SceneElement<PixmanRenderer>>> {
        let mut elements = Vec::new();
        let scale = self.output.current_scale().fractional_scale();
        let pointer = self.seat.get_pointer();
        let hit = pointer
            .as_ref()
            .and_then(|p| self.decoration_under(p.current_location()));
        if self.decoration_press.as_ref().is_some_and(|(_, window)| {
            hit.as_ref() != Some(&(window.clone(), DecorationAction::Close))
        }) {
            self.decoration_press = None;
        }
        let hover = hit
            .filter(|(_, action)| *action != DecorationAction::Move)
            .filter(|_| {
                pointer.as_ref().is_some_and(|p| !p.is_grabbed()) || self.decoration_press.is_some()
            });
        for window in self.space.elements().rev() {
            let Some(location) = self.space.element_location(window) else {
                continue;
            };
            elements.extend(window.render_elements::<SceneElement<PixmanRenderer>>(
                renderer,
                (location - window.geometry().loc).to_physical_precise_round(scale),
                scale.into(),
                1.0,
            ));
            if !decorated(window)
                || window.geometry().size.is_empty()
                || !self.windows.get(window).is_some_and(|state| state.mapped)
            {
                continue;
            }
            let (title, active) = if let Some(top) = window.toplevel() {
                let title = with_states(top.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|data| data.lock().ok()?.title.clone())
                        .unwrap_or_default()
                });
                (
                    title,
                    top.with_pending_state(|state| {
                        state.states.contains(xdg_toplevel::State::Activated)
                    }),
                )
            } else if let Some(x11) = window.x11_surface() {
                (x11.title(), x11.is_activated())
            } else {
                continue;
            };
            let state = self.windows.entry(window.clone()).or_default();
            let size = window.geometry().size;
            let decoration = state.decoration.get_or_insert_with(Decoration::default);
            decoration.update(Appearance {
                title: if title.is_empty() {
                    "Untitled".into()
                } else {
                    title
                },
                width: size.w,
                scale,
                active,
                hover: hover
                    .as_ref()
                    .filter(|(target, _)| target == window)
                    .map(|(_, action)| *action),
                pressed: self
                    .decoration_press
                    .as_ref()
                    .is_some_and(|(_, target)| target == window),
            })?;
            elements.extend(
                decoration
                    .elements(renderer, &Layout(size), location, scale)?
                    .into_iter()
                    .map(Into::into),
            );
        }
        Ok(elements)
    }
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::Bind;
    use smithay::backend::renderer::Offscreen;
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::element::Element;

    use super::*;

    fn appearance(scale: f64) -> Appearance {
        Appearance {
            title: "A readable title — العربية".into(),
            width: 501,
            scale,
            active: true,
            hover: None,
            pressed: false,
        }
    }

    #[test]
    fn hit_regions_match_painted_edges_including_narrow_windows() {
        let layout = Layout((101, 57).into());
        for (point, expected) in [
            ((-0.1, -1.0), None),
            ((0.0, -32.1), None),
            ((0.0, -32.0), Some(DecorationAction::Move)),
            ((68.9, -0.1), Some(DecorationAction::Move)),
            ((69.0, -32.0), Some(DecorationAction::Close)),
            ((100.9, -0.1), Some(DecorationAction::Close)),
            ((101.0, -1.0), None),
            ((99.0, 0.0), None),
            ((76.9, 57.0), None),
            ((77.0, 57.0), Some(DecorationAction::Resize)),
            ((100.9, 64.9), Some(DecorationAction::Resize)),
            ((99.0, 65.0), None),
            ((101.0, 58.0), None),
        ] {
            assert_eq!(layout.hit(point.into()), expected, "{point:?}");
        }
        let narrow = Layout((13, 57).into());
        assert_eq!(narrow.close(), narrow.bar());
        assert_eq!(
            narrow.resize(),
            Rectangle::new((0, 57).into(), (13, 8).into())
        );
        assert_eq!(
            narrow.hit((12.9, -1.0).into()),
            Some(DecorationAction::Close)
        );
        assert_eq!(narrow.hit((13.0, -1.0).into()), None);
    }

    #[test]
    fn raster_is_opaque_argb_and_clips_text_before_close() {
        for scale in [1.0, 1.25, 2.0] {
            for width in [1, 13, 32, 501] {
                let mut look = appearance(scale);
                look.width = width;
                for (hover, pressed, expected) in [
                    (None, false, 0xff27_2e39),
                    (Some(DecorationAction::Close), false, 0xffb8_3245),
                    (Some(DecorationAction::Close), true, 0xff85_2936),
                ] {
                    look.hover = hover;
                    look.pressed = pressed;
                    let mut surface = paint(&look, BAR).expect("titlebar");
                    let stride = usize::try_from(surface.stride()).expect("stride");
                    let pixels = surface.data().expect("pixels");
                    assert_eq!(
                        u32::from_ne_bytes(pixels[stride - 4..stride].try_into().expect("pixel")),
                        expected
                    );
                    assert!(
                        pixels
                            .chunks_exact(4)
                            .all(|p| u32::from_ne_bytes(p.try_into().expect("pixel")) >> 24 == 255)
                    );
                    if width == 501 {
                        let close_start =
                            usize::try_from(raster_size((width - BAR, 0).into(), scale).w)
                                .expect("close start")
                                * 4;
                        let close_pixels: Vec<_> = pixels
                            .chunks_exact(stride)
                            .flat_map(|row| row[close_start..].iter().copied())
                            .collect();
                        drop(pixels);
                        look.title = "Very long title لا يزال هذا نصاً shaped text ".repeat(80);
                        let mut long = paint(&look, BAR).expect("ellipsized titlebar");
                        let long_pixels = long.data().expect("pixels");
                        assert_eq!(
                            close_pixels,
                            long_pixels
                                .chunks_exact(stride)
                                .flat_map(|row| row[close_start..].iter().copied())
                                .collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cached_buffers_map_full_fractional_raster_and_leave_idle_output_undamaged() {
        for (scale, widths, bar_height, grip_width) in [
            (1.0, [501.0, 533.0], 32.0, 24.0),
            (1.25, [626.0, 666.0], 40.0, 30.0),
            (2.0, [1002.0, 1066.0], 64.0, 48.0),
        ] {
            let mut renderer = PixmanRenderer::new().expect("renderer");
            let mut image = renderer
                .create_buffer(Fourcc::Argb8888, (1200, 500).into())
                .expect("target");
            let mut tracker = OutputDamageTracker::new((1200, 500), scale, Transform::Normal);
            let mut cache = Decoration::default();
            let mut look = appearance(scale);
            assert!(cache.update(look.clone()).expect("first paint"));
            let mut previous: Option<([_; 2], [_; 2])> = None;
            for step in 0..7 {
                let changed = match step {
                    0 => [true, true],
                    1 => {
                        look.hover = Some(DecorationAction::Resize);
                        [false, true]
                    }
                    2 => {
                        look.title = "Changed without a client buffer commit".into();
                        [true, false]
                    }
                    3 => {
                        look.hover = Some(DecorationAction::Close);
                        [true, true]
                    }
                    4 => {
                        look.pressed = true;
                        [true, false]
                    }
                    5 => {
                        look.active = false;
                        [true, true]
                    }
                    _ => {
                        look.width = 533;
                        [true, false]
                    }
                };
                cache.update(look.clone()).expect("paint update");
                assert!(!cache.update(look.clone()).expect("idle"));
                let parts = cache
                    .elements(
                        &mut renderer,
                        &Layout((look.width, 53).into()),
                        (10, 42).into(),
                        scale,
                    )
                    .expect("elements");
                let commits = [parts[0].current_commit(), parts[1].current_commit()];
                let ids = [parts[0].id().clone(), parts[1].id().clone()];
                if let Some((old_ids, old_commits)) = previous {
                    assert_eq!(ids, old_ids, "stable buffer IDs");
                    assert_eq!(
                        [commits[0] != old_commits[0], commits[1] != old_commits[1]],
                        changed,
                        "step {step}"
                    );
                }
                previous = Some((ids, commits));
                assert_eq!(
                    parts[0].src().size,
                    (widths[usize::from(step == 6)], bar_height).into()
                );
                assert_eq!(parts[1].src().size, (grip_width, bar_height / 4.0).into());
                let mut target = renderer.bind(&mut image).expect("bind");
                assert!(
                    tracker
                        .render_output(
                            &mut renderer,
                            &mut target,
                            usize::from(step != 0),
                            &parts,
                            [0.0, 0.0, 0.0, 1.0]
                        )
                        .expect("changed render")
                        .damage
                        .is_some()
                );
                assert!(
                    tracker
                        .render_output(&mut renderer, &mut target, 1, &parts, [0.0, 0.0, 0.0, 1.0])
                        .expect("idle render")
                        .damage
                        .is_none()
                );
            }
        }
    }
}
