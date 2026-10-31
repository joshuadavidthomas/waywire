//! Floating window grabs. Locations change on buffer commit during a top/left
//! resize, not on configure, preserving the opposite edge for slow clients.
use smithay::desktop::Window;
use smithay::input::pointer::AxisFrame;
use smithay::input::pointer::ButtonEvent;
use smithay::input::pointer::Focus;
use smithay::input::pointer::GestureHoldBeginEvent;
use smithay::input::pointer::GestureHoldEndEvent;
use smithay::input::pointer::GesturePinchBeginEvent;
use smithay::input::pointer::GesturePinchEndEvent;
use smithay::input::pointer::GesturePinchUpdateEvent;
use smithay::input::pointer::GestureSwipeBeginEvent;
use smithay::input::pointer::GestureSwipeEndEvent;
use smithay::input::pointer::GestureSwipeUpdateEvent;
use smithay::input::pointer::GrabStartData;
use smithay::input::pointer::MotionEvent;
use smithay::input::pointer::PointerGrab;
use smithay::input::pointer::PointerInnerHandle;
use smithay::input::pointer::RelativeMotionEvent;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::Rectangle;
use smithay::utils::Serial;
use smithay::utils::Size;
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use super::State;

#[derive(Default)]
pub(super) struct WindowState {
    pub mapped: bool,
    pub minimized: bool,
    pub always_on_top: bool,
    pub decoration: Option<super::decorations::Decoration>,
    pub decoration_mode: Option<smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode>,
    pub restore: Option<Rectangle<i32, Logical>>,
    pub resize: Option<(u32, Rectangle<i32, Logical>, Option<Serial>)>,
    pub grab_serial: Option<Serial>,
}

pub(super) struct WindowGrab {
    start: GrabStartData<State>,
    window: Window,
    initial: Rectangle<i32, Logical>,
    edges: Option<u32>,
}

impl State {
    #[cfg(test)]
    pub(super) fn start_decoration_grab(&mut self, window: Window, serial: Serial, resize: bool) {
        self.start_decoration_action(
            window,
            serial,
            resize.then_some(xdg_toplevel::ResizeEdge::BottomRight),
        );
    }

    pub(super) fn start_decoration_action(
        &mut self,
        window: Window,
        serial: Serial,
        edges: Option<xdg_toplevel::ResizeEdge>,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let start = GrabStartData {
            focus: None,
            button: 0x110,
            location: pointer.current_location(),
        };
        self.install_window_grab(window, serial, edges.map(|edge| edge as u32), start);
    }

    pub(crate) fn start_move(&mut self, window: Window, serial: Serial) {
        self.start_window_grab(window, serial, None);
    }
    pub(crate) fn start_resize(
        &mut self,
        window: Window,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        if edges != xdg_toplevel::ResizeEdge::None {
            self.start_window_grab(window, serial, Some(edges as u32));
        }
    }
    fn start_window_grab(&mut self, window: Window, serial: Serial, edges: Option<u32>) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start) = pointer.grab_start_data() else {
            return;
        };
        let Some((focus, _)) = &start.focus else {
            return;
        };
        let Some(surface) = window.wl_surface() else {
            return;
        };
        if !focus.id().same_client_as(&surface.id()) {
            return;
        }
        self.install_window_grab(window, serial, edges, start);
    }

    fn install_window_grab(
        &mut self,
        window: Window,
        serial: Serial,
        edges: Option<u32>,
        start: GrabStartData<State>,
    ) {
        if edges.is_some()
            && (window.toplevel().is_some_and(|top| {
                top.with_pending_state(|state| {
                    state.states.contains(xdg_toplevel::State::Maximized)
                        || state.states.contains(xdg_toplevel::State::Fullscreen)
                })
            }) || window
                .x11_surface()
                .is_some_and(|surface| surface.is_maximized() || surface.is_fullscreen()))
        {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(location) = self.space.element_location(&window) else {
            return;
        };
        let size = window
            .x11_surface()
            .map_or(window.geometry().size, |surface| {
                (surface.last_configure() - surface.frame_extents()).size
            });
        let initial = Rectangle::new(location, size);
        if let Some(edges) = edges {
            self.windows.entry(window.clone()).or_default().resize = Some((edges, initial, None));
        } else {
            self.windows.entry(window.clone()).or_default().resize = None;
        }
        pointer.set_grab(
            self,
            WindowGrab {
                start,
                window: window.clone(),
                initial,
                edges,
            },
            serial,
            Focus::Clear,
        );
        self.windows.entry(window).or_default().grab_serial = Some(serial);
    }

    pub(super) fn end_window_grab(&mut self, window: &Window) {
        let serial = self.windows.get(window).and_then(|state| state.grab_serial);
        if let (Some(pointer), Some(serial)) = (self.seat.get_pointer(), serial)
            && pointer.has_grab(serial)
        {
            pointer.unset_grab(
                self,
                smithay::utils::SERIAL_COUNTER.next_serial(),
                smithay::backend::input::InputTime::now(),
            );
        }
    }

    pub(super) fn resize_committed(&mut self, window: &Window) {
        let Some(state) = self.windows.get_mut(window) else {
            return;
        };
        let Some((edges, initial, final_serial)) = state.resize else {
            return;
        };
        let mut location = initial.loc;
        let size = window.geometry().size;
        if edges & 4 != 0 {
            location.x += initial.size.w - size.w;
        }
        if edges & 1 != 0 {
            location.y += initial.size.h - size.h;
        }
        self.space.map_element(window.clone(), location, false);
        if let (Some(serial), Some(top)) = (final_serial, window.toplevel()) {
            let committed = with_states(top.wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .and_then(|data| data.lock().ok())
                    .and_then(|data| data.last_acked.as_ref().map(|c| c.serial >= serial))
                    .unwrap_or(false)
            });
            if committed {
                state.resize = None;
            }
        }
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::verbose_bit_mask,
    reason = "rounded pointer deltas become integer logical sizes; masks are xdg resize edge bits"
)]
fn resize_size(
    initial: Size<i32, Logical>,
    delta: Point<f64, Logical>,
    edges: u32,
    min: Size<i32, Logical>,
    max: Size<i32, Logical>,
) -> Size<i32, Logical> {
    let width = if edges & 0b1100 == 0 {
        initial.w
    } else {
        (f64::from(initial.w) + if edges & 4 != 0 { -delta.x } else { delta.x }).round() as i32
    };
    let height = if edges & 3 == 0 {
        initial.h
    } else {
        (f64::from(initial.h) + if edges & 1 != 0 { -delta.y } else { delta.y }).round() as i32
    };
    (
        width.max(min.w.max(1)).min(if max.w == 0 {
            i32::MAX
        } else {
            max.w.max(min.w.max(1))
        }),
        height.max(min.h.max(1)).min(if max.h == 0 {
            i32::MAX
        } else {
            max.h.max(min.h.max(1))
        }),
    )
        .into()
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "X11 hints use integer client pixels"
)]
fn x11_resize_size(
    mut size: Size<i32, Logical>,
    edges: u32,
    surface: &smithay::xwayland::X11Surface,
    scale: f64,
) -> Size<i32, Logical> {
    let Some(hints) = surface.size_hints() else {
        return size;
    };
    if let Some((inc_w, inc_h)) = hints.size_increment {
        let base = surface.base_size().unwrap_or_default();
        let snap = |value: i32, base: i32, increment: i32| {
            let increment = (f64::from(increment) / scale).round() as i32;
            if increment <= 0 {
                value
            } else {
                (i64::from(base)
                    + (i64::from(value) - i64::from(base)).div_euclid(i64::from(increment))
                        * i64::from(increment))
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
            }
        };
        size = (snap(size.w, base.w, inc_w), snap(size.h, base.h, inc_h)).into();
    }
    let min = surface.min_size().unwrap_or_default();
    let max = surface.max_size().unwrap_or_default();
    size = resize_size(size, (0.0, 0.0).into(), 0, min, max);
    if let Some((minimum, maximum)) = hints.aspect
        && minimum.numerator > 0
        && minimum.denominator > 0
        && maximum.numerator > 0
        && maximum.denominator > 0
    {
        let min_ratio = f64::from(minimum.numerator) / f64::from(minimum.denominator);
        let max_ratio = f64::from(maximum.numerator) / f64::from(maximum.denominator);
        if min_ratio <= max_ratio {
            if edges & 0b1100 == 0 {
                size.w = f64::from(size.w)
                    .clamp(f64::from(size.h) * min_ratio, f64::from(size.h) * max_ratio)
                    .round() as i32;
            } else {
                size.h = f64::from(size.h)
                    .clamp(f64::from(size.w) / max_ratio, f64::from(size.w) / min_ratio)
                    .round() as i32;
            }
        }
    }
    resize_size(size, (0.0, 0.0).into(), 0, min, max)
}

impl PointerGrab<State> for WindowGrab {
    fn motion(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        if state.pointer_motion_kind == super::PointerMotionKind::RefreshGrabFocus {
            return;
        }
        handle.motion(state, None, event);
        let delta = event.location - self.start.location;
        if let Some(edges) = self.edges {
            let (min, max) = self
                .window
                .toplevel()
                .map(|top| {
                    with_states(top.wl_surface(), |states| {
                        let mut cached = states.cached_state.get::<SurfaceCachedState>();
                        let cached = cached.current();
                        (cached.min_size, cached.max_size)
                    })
                })
                .or_else(|| {
                    self.window.x11_surface().map(|surface| {
                        (
                            surface.min_size().unwrap_or_default(),
                            surface.max_size().unwrap_or_default(),
                        )
                    })
                })
                .unwrap_or_default();
            let size = resize_size(self.initial.size, delta, edges, min, max);
            if let Some(top) = self.window.toplevel() {
                top.with_pending_state(|pending| {
                    pending.states.set(xdg_toplevel::State::Resizing);
                    pending.size = Some(size);
                });
                top.send_pending_configure();
            } else if let Some(surface) = self.window.x11_surface() {
                let scale = state
                    .xwayland_client
                    .as_ref()
                    .and_then(|client| client.get_data::<smithay::xwayland::XWaylandClientData>())
                    .map_or(1.0, |data| data.compositor_state.client_scale());
                let requested = resize_size(
                    self.initial.size,
                    delta,
                    edges,
                    (0, 0).into(),
                    (0, 0).into(),
                );
                let size = x11_resize_size(requested, edges, surface, scale);
                let mut rect = Rectangle::new(self.initial.loc, size);
                if edges & 4 != 0 {
                    rect.loc.x += self.initial.size.w - size.w;
                }
                if edges & 1 != 0 {
                    rect.loc.y += self.initial.size.h - size.h;
                }
                if let Err(error) = surface.configure(Some(rect + surface.frame_extents())) {
                    tracing::warn!(%error, "configure X11 resize");
                }
                state
                    .space
                    .map_element(self.window.clone(), rect.loc, false);
            }
        } else {
            let mut location = (self.initial.loc.to_f64() + delta).to_i32_round::<i32>();
            if let Some(area) = state.work_area() {
                location.y = location.y.min(area.loc.y + area.size.h);
            }
            state.space.map_element(self.window.clone(), location, true);
            state.enforce_topmost();
            if let Some(surface) = self.window.x11_surface()
                && let Err(error) = surface.configure(Some(Rectangle::new(
                    location - self.window.geometry().loc,
                    surface.last_configure().size,
                )))
            {
                tracing::warn!(%error, "configure X11 move");
            }
        }
        state.dirty = true;
    }
    fn button(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(state, event);
        if !handle.current_pressed().contains(&self.start.button) {
            handle.unset_grab(self, state, event.serial, event.time, true);
        }
    }
    fn unset(&mut self, state: &mut State) {
        if let Some(window_state) = state.windows.get_mut(&self.window) {
            window_state.grab_serial = None;
        }
        if self.edges.is_some()
            && let Some(top) = self.window.toplevel()
            && top.alive()
            && top.is_initial_configure_sent()
        {
            top.with_pending_state(|pending| pending.states.unset(xdg_toplevel::State::Resizing));
            let serial = top.send_configure();
            if let Some((_, _, final_serial)) = state
                .windows
                .entry(self.window.clone())
                .or_default()
                .resize
                .as_mut()
            {
                *final_serial = Some(serial);
            }
        } else if self.edges.is_some()
            && let Some(surface) = self.window.x11_surface()
            && surface.alive()
        {
            // X11 has no xdg configure acknowledgment to retire the resize anchor.
            // Keep the final configured position even if an older buffer commits later.
            state.windows.entry(self.window.clone()).or_default().resize = None;
            let location = surface.last_configure().loc + surface.geometry().loc;
            state
                .space
                .map_element(self.window.clone(), location, false);
        }
    }
    fn start_data(&self) -> &GrabStartData<State> {
        &self.start
    }
    fn relative_motion(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(state, focus, event);
    }
    fn axis(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        frame: AxisFrame,
    ) {
        handle.axis(state, frame);
    }
    fn frame(&mut self, state: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(state);
    }
    fn gesture_swipe_begin(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(state, event);
    }
    fn gesture_swipe_update(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(state, event);
    }
    fn gesture_swipe_end(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(state, event);
    }
    fn gesture_pinch_begin(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(state, event);
    }
    fn gesture_pinch_update(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(state, event);
    }
    fn gesture_pinch_end(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(state, event);
    }
    fn gesture_hold_begin(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(state, event);
    }
    fn gesture_hold_end(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(state, event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asymmetric_top_left_resize_honors_each_axis_and_constraints() {
        assert_eq!(
            resize_size(
                (300, 170).into(),
                (51.0, -33.0).into(),
                5,
                (100, 80).into(),
                (280, 190).into()
            ),
            (249, 190).into()
        );
        assert_eq!(
            resize_size(
                (300, 170).into(),
                (999.0, 999.0).into(),
                4,
                (100, 80).into(),
                (0, 0).into()
            ),
            (100, 170).into()
        );
        assert_eq!(
            resize_size(
                (300, 170).into(),
                (-50.0, 60.0).into(),
                10,
                (100, 80).into(),
                (0, 0).into()
            ),
            (250, 230).into()
        );
    }
}
