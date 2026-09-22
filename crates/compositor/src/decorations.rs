//! Minimal server decorations: a drag bar and a bottom-right resize strip.
use smithay::backend::renderer::ImportAll;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

use super::State;

pub(super) const BAR: i32 = 24;
pub(super) const STRIP: i32 = 6;

pub(super) enum DecorationAction {
    Move,
    Resize,
    Close,
}

smithay::backend::renderer::element::render_elements! {
    pub(super) SceneElement<R> where R: ImportAll;
    Surface=WaylandSurfaceRenderElement<R>,
    Solid=SolidColorRenderElement,
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
            .is_some_and(|x| !x.is_decorated() && !x.is_fullscreen())
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
            if !decorated(window) || window.geometry().size.is_empty() {
                continue;
            }
            let size = window.geometry().size;
            let bar = Rectangle::new(location - Point::from((0, BAR)), (size.w, BAR).into());
            let strip = Rectangle::new(location + Point::from((0, size.h)), (size.w, STRIP).into());
            if bar.to_f64().contains(point) {
                let action = if point.x >= f64::from(location.x + size.w - BAR) {
                    DecorationAction::Close
                } else {
                    DecorationAction::Move
                };
                return Some((window.clone(), action));
            }
            if strip.to_f64().contains(point) {
                return Some((window.clone(), DecorationAction::Resize));
            }
        }
        None
    }

    pub(super) fn scene_elements(
        &mut self,
        renderer: &mut PixmanRenderer,
    ) -> Vec<SceneElement<PixmanRenderer>> {
        let mut elements = Vec::new();
        let scale = self.output.current_scale().fractional_scale();
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
            if !decorated(window) || window.geometry().size.is_empty() {
                continue;
            }
            let state = self.windows.entry(window.clone()).or_default();
            let size = window.geometry().size;
            let [bar, strip, close] = state
                .decoration_buffers
                .get_or_insert_with(|| std::array::from_fn(|_| SolidColorBuffer::default()));
            bar.update((size.w, BAR), [0.22, 0.27, 0.36, 1.0]);
            strip.update((size.w, STRIP), [0.36, 0.44, 0.58, 1.0]);
            close.update((BAR.min(size.w), BAR), [0.72, 0.20, 0.22, 1.0]);
            for (buffer, position) in [
                (close, location + Point::from(((size.w - BAR).max(0), -BAR))),
                (bar, location - Point::from((0, BAR))),
                (strip, location + Point::from((0, size.h))),
            ] {
                elements.push(
                    SolidColorRenderElement::from_buffer(
                        buffer,
                        position.to_physical_precise_round(scale),
                        scale,
                        1.0,
                        Kind::Unspecified,
                    )
                    .into(),
                );
            }
        }
        elements
    }
}
