//! The compositor's side of server-side decorations: xdg-decoration mode
//! negotiation, finding the decoration under the pointer, and assembling the
//! rendered scene with `waywire_decorations`' title bars, taskbar and menu.

use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::Window;
use smithay::desktop::WindowSurfaceType;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::Size;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use waywire_decorations::Appearance;
use waywire_decorations::CloseButton;
use waywire_decorations::Decoration;
use waywire_decorations::DecorationAction;
use waywire_decorations::Node;
use waywire_decorations::SceneElement;
use waywire_decorations::above_taskbar;
use waywire_decorations::decorated;
use waywire_decorations::decoration_width;
use waywire_decorations::node_at;
use waywire_decorations::window_flags;
use waywire_protocol::pipe::CursorShape;

use crate::State;

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, top: ToplevelSurface) {
        self.request_mode(top, Mode::ServerSide);
    }
    fn request_mode(&mut self, top: ToplevelSurface, mode: Mode) {
        let window = self
            .space
            .elements()
            .find(|window| window.toplevel() == Some(&top))
            .cloned();
        if let Some(window) = &window {
            self.windows
                .entry(window.clone())
                .or_default()
                .decoration_mode = Some(mode);
        }
        top.with_pending_state(|state| state.decoration_mode = Some(mode));
        if let Some(bounds) = window
            .as_ref()
            .and_then(|window| self.window_output_geometry(window))
        {
            top.with_pending_state(|state| state.bounds = Some(bounds.size));
        }
        if top.is_initial_configure_sent() {
            top.send_pending_configure();
        }
        self.dirty = true;
    }
    fn unset_mode(&mut self, top: ToplevelSurface) {
        self.request_mode(top, Mode::ServerSide);
    }
}

impl State {
    pub(crate) fn decoration_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(Window, DecorationAction)> {
        self.decoration_node_under(point)
            .and_then(|(window, node)| Some((window, node.action()?)))
    }

    pub(crate) fn decoration_cursor_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<CursorShape> {
        self.decoration_node_under(point)
            .map(|(_, node)| node.cursor())
    }

    fn decoration_node_under(&self, point: Point<f64, Logical>) -> Option<(Window, Node)> {
        if self
            .work_area()
            .is_some_and(|area| point.y >= f64::from(area.loc.y + area.size.h))
        {
            return None;
        }
        for window in self.space.elements().rev() {
            if self
                .windows
                .get(window)
                .is_some_and(|state| state.minimized)
            {
                continue;
            }
            let location = self.space.element_location(window)?;
            let surface_location = location - window.geometry().loc;
            if !decorated(window)
                || window.geometry().size.is_empty()
                || !self.windows.get(window).is_some_and(|state| state.mapped)
            {
                if window
                    .surface_under(point - surface_location.to_f64(), WindowSurfaceType::ALL)
                    .is_some()
                {
                    return None;
                }
                continue;
            }
            let size: Size<i32, Logical> = (
                decoration_width(window.geometry().size.w, self.size.width()),
                window.geometry().size.h,
            )
                .into();
            if let Some(node) = node_at(size, window_flags(window), point - location.to_f64()) {
                return Some((window.clone(), node));
            }
            if window
                .surface_under(point - surface_location.to_f64(), WindowSurfaceType::ALL)
                .is_some()
            {
                return None;
            }
        }
        None
    }

    #[expect(clippy::too_many_lines, reason = "one pass builds the ordered scene")]
    pub(crate) fn scene_elements(
        &mut self,
        renderer: &mut PixmanRenderer,
    ) -> Vec<SceneElement<PixmanRenderer>> {
        let mut elements = Vec::new();
        let scale = self.output.current_scale().fractional_scale();
        if let (Some(drag), Some(pointer)) = (&self.drag, self.seat.get_pointer())
            && let Some(icon) = drag.icon()
        {
            // The icon is part of the picture, unlike the cursor, which the
            // viewer draws itself.
            let location = drag
                .icon_location(pointer.current_location())
                .to_physical_precise_round(scale);
            elements.extend(
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    icon,
                    location,
                    scale,
                    1.0,
                    Kind::Unspecified,
                ),
            );
        }
        if let Some(menu) = self.chrome.window_menu_mut() {
            match menu.element(renderer, scale) {
                Ok(element) => elements.push(element.into()),
                Err(error) => tracing::warn!(%error, "window menu rendering failed"),
            }
        }
        let output_width = self.size.width();
        let pointer = self.seat.get_pointer();
        let hit = pointer
            .as_ref()
            .and_then(|p| self.decoration_under(p.current_location()));
        if self.chrome.armed_close().is_some_and(|(_, window)| {
            hit.as_ref() != Some(&(window.clone(), DecorationAction::Close))
        }) {
            self.chrome.disarm_close();
        }
        let hover = hit
            .filter(|(_, action)| *action != DecorationAction::Move)
            .filter(|_| {
                pointer.as_ref().is_some_and(|p| !p.is_grabbed())
                    || self.chrome.armed_close().is_some()
            });
        let mut upper = Vec::new();
        let mut lower = Vec::new();
        let mut above_bar = true;
        for window in self.space.elements().rev() {
            if self
                .windows
                .get(window)
                .is_some_and(|state| state.minimized)
            {
                continue;
            }
            let Some(location) = self.space.element_location(window) else {
                continue;
            };
            above_bar &= above_taskbar(window);
            let target = if above_bar { &mut upper } else { &mut lower };
            target.extend(window.render_elements::<SceneElement<PixmanRenderer>>(
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
                    top.with_committed_state(|state| {
                        state.is_some_and(|state| {
                            state.states.contains(xdg_toplevel::State::Activated)
                        })
                    }),
                )
            } else if let Some(x11) = window.x11_surface() {
                (x11.title(), x11.is_activated())
            } else {
                continue;
            };
            let state = self.windows.entry(window.clone()).or_default();
            let size = window.geometry().size;
            let width = decoration_width(size.w, output_width);
            let decoration = state.decoration.get_or_insert_with(Decoration::default);
            let appearance = Appearance::new(
                if title.is_empty() {
                    "Untitled".into()
                } else {
                    title
                },
                width,
                scale,
                if active {
                    waywire_decorations::Activation::Active
                } else {
                    waywire_decorations::Activation::Inactive
                },
                window_flags(window),
                if !hover.as_ref().is_some_and(|(target, action)| {
                    target == window && *action == DecorationAction::Close
                }) {
                    CloseButton::Idle
                } else if self
                    .chrome
                    .armed_close()
                    .is_some_and(|(_, target)| target == window)
                {
                    CloseButton::Pressed
                } else {
                    CloseButton::Hovered
                },
            );
            // One broken decoration must not fail the render — and the session
            // — so log it and skip this window's decoration.
            if let Err(error) = decoration.update(appearance) {
                tracing::warn!(%error, "decoration update failed; skipping window decoration");
                continue;
            }
            match decoration.elements(renderer, (width, size.h).into(), location, scale) {
                Ok(parts) => target.extend(parts.into_iter().map(Into::into)),
                Err(error) => {
                    tracing::warn!(%error, "decoration elements failed; skipping window decoration");
                }
            }
        }
        elements.extend(upper);
        if let Some(output) = self.space.output_geometry(&self.output) {
            let chips = self.taskbar_chips();
            let control = crate::chrome::control_text(self.input.browser_holds_input());
            let clock = self.chrome.clock().to_owned();
            match self
                .chrome
                .taskbar_mut()
                .element(renderer, output, scale, chips, control, clock)
            {
                Ok(bar) => elements.push(bar.into()),
                Err(error) => tracing::warn!(%error, "taskbar rendering failed"),
            }
        }
        elements.extend(lower);
        elements
    }
}
