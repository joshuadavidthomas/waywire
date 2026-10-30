//! Xwayland pointer warp support.
//!
//! Xwayland 24.1.13 does not speak `wp-pointer-warp-v1`. It emulates
//! `XWarpPointer` with `zwp_pointer_constraints_v1.lock_pointer` on the window
//! that already has pointer focus plus `zwp_locked_pointer_v1`
//! `set_cursor_position_hint` requests, and it only enables the emulator when
//! the compositor also advertises `zwp_relative_pointer_manager_v1`
//! (`hw/xwayland/xwayland-input.c`, `xwl_pointer_warp_emulator_*` in
//! xwayland 24.1.13). This module installs those two globals and moves the seat
//! pointer whenever Xwayland commits a position hint, which is how the
//! compositor learns about X11 warps.
//!
//! Xwayland's own gates still apply: the emulator runs only while the X11
//! cursor is hidden (`xwl_seat->x_cursor == NULL`) and warps only reach it when
//! the target position lies in the window that already has pointer focus, so
//! unadorned `xdotool mousemove` stays an X11-only sprite move.
use smithay::input::keyboard::KeyboardSource;
use smithay::input::pointer::PointerHandle;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::pointer_constraints::with_pointer_constraint;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::seat::WaylandFocus;

use super::State;

/// Advertise the pointer-constraints and relative-pointer globals Xwayland's
/// warp emulator requires.
pub(crate) fn install(display_handle: &DisplayHandle) {
    PointerConstraintsState::new::<State>(display_handle);
    RelativePointerManagerState::new::<State>(display_handle);
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Xwayland locks the surface of the window that has pointer focus, so
        // an up-front activation covers its persistent locks. Warps to windows
        // without pointer focus are dropped by Xwayland itself.
        let Some(focus) = pointer.current_focus() else {
            return;
        };
        if focus.wl_surface().as_deref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        // Only hints from an active constraint move the pointer; the protocol
        // requires the surface to hold pointer focus, which activation checks.
        let active = with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|constraint| constraint.is_active())
        });
        if !active {
            return;
        }
        let Some(window) = self
            .space
            .elements()
            .find(|window| window.wl_surface().as_deref() == Some(surface))
            .cloned()
        else {
            return;
        };
        let Some(element) = self.space.element_location(&window) else {
            return;
        };
        // The surface sits inside the outer window geometry, whose origin the
        // space element location tracks (see `decorations::DecorationLayout`).
        let origin = (element - window.geometry().loc).to_f64();
        self.pointer_motion(KeyboardSource::MAIN, origin + location);
    }
}
