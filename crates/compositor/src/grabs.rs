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
    pub decoration_buffers:
        Option<[smithay::backend::renderer::element::solid::SolidColorBuffer; 3]>,
    pub restore: Option<Rectangle<i32, Logical>>,
    pub resize: Option<(u32, Rectangle<i32, Logical>, Option<Serial>)>,
}

pub(super) struct WindowGrab {
    start: GrabStartData<State>,
    window: Window,
    initial: Rectangle<i32, Logical>,
    edges: Option<u32>,
}

impl State {
    pub(super) fn start_decoration_grab(&mut self, window: Window, serial: Serial, resize: bool) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let start = GrabStartData {
            focus: None,
            button: 0x110,
            location: pointer.current_location(),
        };
        self.install_window_grab(window, serial, resize.then_some(10), start);
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
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(location) = self.space.element_location(&window) else {
            return;
        };
        let initial = Rectangle::new(location, window.geometry().size);
        if let Some(edges) = edges {
            self.windows.entry(window.clone()).or_default().resize = Some((edges, initial, None));
        } else {
            self.windows.entry(window.clone()).or_default().resize = None;
        }
        pointer.set_grab(
            self,
            WindowGrab {
                start,
                window,
                initial,
                edges,
            },
            serial,
            Focus::Clear,
        );
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

impl PointerGrab<State> for WindowGrab {
    fn motion(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
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
                .unwrap_or_default();
            let size = resize_size(self.initial.size, delta, edges, min, max);
            if let Some(top) = self.window.toplevel() {
                top.with_pending_state(|pending| {
                    pending.states.set(xdg_toplevel::State::Resizing);
                    pending.size = Some(size);
                });
                top.send_pending_configure();
            } else if let Some(surface) = self.window.x11_surface() {
                let mut rect = Rectangle::new(self.initial.loc, size);
                if edges & 4 != 0 {
                    rect.loc.x += self.initial.size.w - size.w;
                }
                if edges & 1 != 0 {
                    rect.loc.y += self.initial.size.h - size.h;
                }
                if let Err(error) = surface.configure(Some(rect)) {
                    tracing::warn!(%error, "configure X11 resize");
                }
                state
                    .space
                    .map_element(self.window.clone(), rect.loc, false);
            }
        } else {
            let location = (self.initial.loc.to_f64() + delta).to_i32_round();
            state.space.map_element(self.window.clone(), location, true);
            if let Some(surface) = self.window.x11_surface()
                && let Err(error) =
                    surface.configure(Some(Rectangle::new(location, self.initial.size)))
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
        if self.edges.is_some()
            && let Some(top) = self.window.toplevel()
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
