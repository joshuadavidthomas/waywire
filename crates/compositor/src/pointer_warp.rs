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
use smithay::wayland::compositor::RegionAttributes;
use smithay::wayland::compositor::SurfaceAttributes;
use smithay::wayland::compositor::with_states;
use smithay::wayland::pointer_constraints::PointerConstraint;
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::pointer_constraints::with_pointer_constraint;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::seat::WaylandFocus;

use super::PointerMotionKind;
use super::State;

/// Advertise the pointer-constraints and relative-pointer globals Xwayland's
/// warp emulator requires.
pub(crate) fn install(display_handle: &DisplayHandle) {
    PointerConstraintsState::new::<State>(display_handle);
    RelativePointerManagerState::new::<State>(display_handle);
}

fn clip_motion(
    start: Point<f64, Logical>,
    target: Point<f64, Logical>,
    origin: Point<f64, Logical>,
    regions: &[&RegionAttributes],
    inside: impl Fn(Point<f64, Logical>) -> bool,
) -> Point<f64, Logical> {
    let dx = target.x - start.x;
    let dy = target.y - start.y;
    let mut boundaries = vec![0.0, 1.0];
    for region in regions {
        for (_, rect) in &region.rects {
            for edge in [
                f64::from(rect.loc.x),
                f64::from(rect.loc.x) + f64::from(rect.size.w),
            ] {
                if dx != 0.0 {
                    let t = (origin.x + edge - start.x) / dx;
                    if (0.0..1.0).contains(&t) {
                        boundaries.push(t);
                    }
                }
            }
            for edge in [
                f64::from(rect.loc.y),
                f64::from(rect.loc.y) + f64::from(rect.size.h),
            ] {
                if dy != 0.0 {
                    let t = (origin.y + edge - start.y) / dy;
                    if (0.0..1.0).contains(&t) {
                        boundaries.push(t);
                    }
                }
            }
        }
    }
    boundaries.sort_by(f64::total_cmp);
    let point = |t: f64| Point::from((start.x + dx * t, start.y + dy * t));
    let mut last = 0.0;
    for t in boundaries
        .windows(2)
        .map(|pair| f64::midpoint(pair[0], pair[1]))
        .chain(std::iter::once(1.0))
    {
        if !inside(point(t)) {
            let mut low = last;
            let mut high = t;
            for _ in 0..48 {
                let mid = f64::midpoint(low, high);
                if inside(point(mid)) {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            return point(low);
        }
        last = t;
    }
    target
}

impl State {
    pub(crate) fn activate_focused_constraint(&self, pointer: &PointerHandle<Self>) {
        let position = pointer.current_location();
        let Some((surface, origin)) = self.surface_under(position) else {
            return;
        };
        if !pointer
            .current_focus()
            .is_some_and(|focus| focus.is(&surface))
        {
            return;
        }
        let local = position - origin;
        with_pointer_constraint(&surface, pointer, |constraint| {
            if let Some(constraint) = constraint
                && constraint
                    .region()
                    .is_none_or(|region| region.contains(local.to_i32_floor::<i32>()))
            {
                constraint.activate();
            }
        });
    }

    pub(crate) fn constrain_pointer_motion(
        &self,
        pointer: &PointerHandle<Self>,
        target: Point<f64, Logical>,
    ) -> (Point<f64, Logical>, bool) {
        let current = pointer.current_location();
        let Some((surface, origin)) = self.surface_under(current) else {
            return (target, false);
        };
        if !pointer
            .current_focus()
            .is_some_and(|focus| focus.is(&surface))
        {
            return (target, false);
        }
        let active = with_pointer_constraint(&surface, pointer, |constraint| {
            let constraint = constraint.filter(|c| c.is_active())?;
            match &*constraint {
                PointerConstraint::Locked(_) => Some(None),
                PointerConstraint::Confined(confined) => Some(Some(confined.region().cloned())),
            }
        });
        match active {
            None => (target, false),
            Some(None) => (current, true),
            Some(Some(region)) => {
                let input = with_states(&surface, |states| {
                    states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .input_region
                        .clone()
                });
                let inside = |point: Point<f64, Logical>| {
                    let local = point - origin;
                    region
                        .as_ref()
                        .is_none_or(|region| region.contains(local.to_i32_floor::<i32>()))
                        && self
                            .surface_under(point)
                            .is_some_and(|(hit, _)| hit == surface)
                };
                if !inside(current) {
                    with_pointer_constraint(&surface, pointer, |constraint| {
                        if let Some(constraint) = constraint {
                            constraint.deactivate();
                        }
                    });
                    return (target, false);
                }
                let regions: Vec<_> = region.iter().chain(input.iter()).collect();
                (
                    clip_motion(current, target, origin, &regions, inside),
                    false,
                )
            }
        }
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if pointer
            .current_focus()
            .is_some_and(|focus| focus.is(surface))
        {
            self.activate_focused_constraint(pointer);
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
        if window.x11_surface().is_none() {
            return;
        }
        let Some(element) = self.space.element_location(&window) else {
            return;
        };
        // The surface sits inside the outer window geometry, whose origin the
        // space element location tracks (see `decorations::DecorationLayout`).
        let origin = (element - window.geometry().loc).to_f64();
        self.pointer_motion_kind = PointerMotionKind::WarpHint;
        self.pointer_motion(KeyboardSource::MAIN, origin + location);
        self.pointer_motion_kind = PointerMotionKind::Input;
    }
}
