//! Window management: per-window state, the taskbar and minimize orders, and
//! the stacking, dialog and focus rules.
//!
//! `Windows` owns this bookkeeping and never touches the space, the seat or
//! the X11 window manager. It reads the space's stacking order where a rule
//! needs it, and returns decisions, such as the windows to raise or the
//! window that should take focus, for the caller to apply.
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use smithay::desktop::Space;
use smithay::desktop::Window;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::Rectangle;
use smithay::utils::Serial;
use smithay::utils::Size;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::shell::xdg::dialog::ToplevelDialogHint;

#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mapping, minimizing, stacking and urgency change independently"
)]
pub(crate) struct WindowState {
    pub mapped: bool,
    pub minimized: bool,
    pub always_on_top: bool,
    /// An activation request without a valid token asked for attention.
    pub urgent: bool,
    pub decoration: Option<waywire_decorations::Decoration>,
    pub decoration_mode: Option<smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode>,
    pub restore: Option<Rectangle<i32, Logical>>,
    pub resize: Option<(u32, Rectangle<i32, Logical>, Option<Serial>)>,
    pub grab_serial: Option<Serial>,
}

#[derive(Default)]
pub(crate) struct Windows {
    states: HashMap<Window, WindowState>,
    /// Windows in the order they were first mapped, which is the taskbar's.
    mapped_order: Vec<Window>,
    /// Minimized windows, most recently minimized last.
    minimized_order: Vec<Window>,
}

impl Windows {
    pub(crate) fn get(&self, window: &Window) -> Option<&WindowState> {
        self.states.get(window)
    }

    pub(crate) fn get_mut(&mut self, window: &Window) -> Option<&mut WindowState> {
        self.states.get_mut(window)
    }

    pub(crate) fn entry(&mut self, window: Window) -> Entry<'_, Window, WindowState> {
        self.states.entry(window)
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, window: &Window) -> bool {
        self.states.contains_key(window)
    }

    pub(crate) fn remove(&mut self, window: &Window) {
        self.states.remove(window);
    }

    pub(crate) fn is_minimized(&self, window: &Window) -> bool {
        self.states.get(window).is_some_and(|state| state.minimized)
    }

    pub(crate) fn mapped_order(&self) -> &[Window] {
        &self.mapped_order
    }

    pub(crate) fn minimized_order(&self) -> &[Window] {
        &self.minimized_order
    }

    /// Add a window to the taskbar order when it maps for the first time.
    pub(crate) fn add_mapped(&mut self, window: &Window) {
        if !self.mapped_order.contains(window) {
            self.mapped_order.push(window.clone());
        }
    }

    /// Forget an unmapped window's place in both orders and reset its state.
    pub(crate) fn unmapped(&mut self, window: &Window) {
        self.mapped_order.retain(|item| item != window);
        self.minimized_order.retain(|item| item != window);
        if let Some(state) = self.states.get_mut(window) {
            state.mapped = false;
            state.minimized = false;
            state.resize = None;
            state.decoration = None;
        }
    }

    /// Drop the state of windows whose surfaces are gone.
    pub(crate) fn retain_alive(&mut self) {
        self.states
            .retain(|window, _| smithay::utils::IsAlive::alive(window));
        self.minimized_order.retain(smithay::utils::IsAlive::alive);
    }

    /// Mark a window minimized. Returns whether it was not minimized before.
    pub(crate) fn minimize(&mut self, window: &Window) -> bool {
        let state = self.states.entry(window.clone()).or_default();
        if state.minimized {
            return false;
        }
        state.minimized = true;
        self.minimized_order.push(window.clone());
        true
    }

    /// Clear a window's minimized state. Returns whether it was minimized.
    pub(crate) fn unminimize(&mut self, window: &Window) -> bool {
        let Some(state) = self.states.get_mut(window).filter(|state| state.minimized) else {
            return false;
        };
        state.minimized = false;
        self.minimized_order.retain(|item| item != window);
        true
    }

    pub(crate) fn toggle_always_on_top(&mut self, window: &Window) {
        let state = self.states.entry(window.clone()).or_default();
        state.always_on_top = !state.always_on_top;
    }

    pub(crate) fn always_on_top(&self, window: &Window) -> bool {
        self.states
            .get(window)
            .is_some_and(|state| state.always_on_top)
    }

    pub(crate) fn clear_urgent(&mut self, window: &Window) {
        if let Some(state) = self.states.get_mut(window) {
            state.urgent = false;
        }
    }

    /// The topmost mapped window that can take keyboard focus, skipping `except`.
    pub(crate) fn topmost_focusable(
        &self,
        space: &Space<Window>,
        except: Option<&Window>,
    ) -> Option<Window> {
        space
            .elements()
            .rev()
            .find(|window| {
                Some(*window) != except
                    && !is_override_redirect(window)
                    && self
                        .states
                        .get(*window)
                        .is_some_and(|state| state.mapped && !state.minimized)
            })
            .cloned()
    }

    /// The visible dialogs whose parent is `parent`, bottom to top.
    fn dialog_children(&self, space: &Space<Window>, parent: &Window) -> Vec<Window> {
        space
            .elements()
            .filter(|candidate| {
                self.states
                    .get(*candidate)
                    .is_some_and(|state| state.mapped && !state.minimized)
                    && parent_window(space, candidate).as_ref() == Some(parent)
            })
            .cloned()
            .collect()
    }

    /// The always-on-top windows and their dialogs, in the order to raise
    /// them: parents before their dialogs.
    pub(crate) fn topmost(&self, space: &Space<Window>) -> Vec<Window> {
        let mut topmost: Vec<_> = space
            .elements()
            .filter(|window| {
                let mut current = Some((*window).clone());
                let mut visited = Vec::new();
                while let Some(target) = current {
                    if visited.contains(&target) {
                        break;
                    }
                    if self
                        .states
                        .get(&target)
                        .is_some_and(|state| state.always_on_top)
                    {
                        return true;
                    }
                    current = parent_window(space, &target);
                    visited.push(target);
                }
                false
            })
            .cloned()
            .collect();
        topmost.sort_by_key(|window| {
            let mut depth = 0;
            let mut current = parent_window(space, window);
            let mut visited = vec![window.clone()];
            while let Some(parent) = current {
                if visited.contains(&parent) {
                    break;
                }
                depth += 1;
                current = parent_window(space, &parent);
                visited.push(parent);
            }
            depth
        });
        topmost
    }

    /// `window` and every visible dialog below it, to raise together when it
    /// is focused.
    pub(crate) fn family(&self, space: &Space<Window>, window: &Window) -> Vec<Window> {
        let mut family = vec![window.clone()];
        let mut index = 0;
        while index < family.len() {
            for child in self.dialog_children(space, &family[index]) {
                if !family.contains(&child) {
                    family.push(child);
                }
            }
            index += 1;
        }
        family
    }

    /// The window that takes focus when `window` is focused: its topmost
    /// modal dialog, followed down the chain of modal dialogs.
    pub(crate) fn modal_focus(&self, space: &Space<Window>, window: Window) -> Window {
        let mut window = window;
        let mut visited = Vec::new();
        while !visited.contains(&window) {
            visited.push(window.clone());
            let Some(next) = self
                .dialog_children(space, &window)
                .into_iter()
                .rev()
                .find(modal_dialog)
            else {
                break;
            };
            window = next;
        }
        window
    }
}

/// The mapped window `window` is a dialog or transient for.
pub(crate) fn parent_window(space: &Space<Window>, window: &Window) -> Option<Window> {
    if let Some(parent) = window
        .toplevel()
        .and_then(smithay::wayland::shell::xdg::ToplevelSurface::parent)
    {
        return space
            .elements()
            .find(|candidate| {
                candidate
                    .toplevel()
                    .is_some_and(|top| top.wl_surface() == &parent)
            })
            .cloned();
    }
    let parent = window.x11_surface()?.is_transient_for()?;
    space
        .elements()
        .find(|candidate| {
            candidate
                .x11_surface()
                .is_some_and(|surface| surface.window_id() == parent)
        })
        .cloned()
}

pub(crate) fn modal_dialog(window: &Window) -> bool {
    if let Some(top) = window.toplevel() {
        return with_states(top.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|data| data.lock().ok())
                .is_some_and(|data| data.dialog_hint == ToplevelDialogHint::Modal)
        });
    }
    window
        .x11_surface()
        .is_some_and(smithay::xwayland::X11Surface::is_modal)
}

pub(crate) fn is_override_redirect(window: &Window) -> bool {
    window
        .x11_surface()
        .is_some_and(smithay::xwayland::X11Surface::is_override_redirect)
}

/// Where a newly mapped dialog goes: centered on its parent, kept inside
/// `area`. `None` when it has no mapped parent.
pub(crate) fn dialog_location(
    space: &Space<Window>,
    window: &Window,
    area: Option<Rectangle<i32, Logical>>,
) -> Option<Point<i32, Logical>> {
    let parent = parent_window(space, window)?;
    let parent_location = space.element_location(&parent)?;
    let parent_geometry = parent.geometry();
    let child_geometry = window.geometry();
    let mut location = parent_location
        + Point::from((
            (parent_geometry.size.w - child_geometry.size.w) / 2,
            (parent_geometry.size.h - child_geometry.size.h) / 2,
        ));
    if let Some(area) = area {
        location.x = location.x.clamp(
            area.loc.x,
            area.loc.x + (area.size.w - child_geometry.size.w).max(0),
        );
        location.y = location.y.clamp(
            area.loc.y,
            area.loc.y + (area.size.h - child_geometry.size.h).max(0),
        );
    }
    Some(location)
}

/// Where a maximized window dragged by its title bar goes once restored:
/// under the pointer at the same fraction of its width it was pressed at.
pub(crate) fn restored_drag_location(
    pressed: Point<f64, Logical>,
    location: Point<f64, Logical>,
    maximized: Rectangle<i32, Logical>,
    restored: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let fraction = ((pressed.x - f64::from(maximized.loc.x)) / f64::from(maximized.size.w.max(1)))
        .clamp(0.0, 1.0);
    Point::<f64, Logical>::from((
        location.x - fraction * f64::from(restored.w),
        location.y + f64::from(maximized.loc.y) - pressed.y,
    ))
    .to_i32_round()
}

/// Ask a window to close. The titlebar's Close button and the
/// wlr-foreign-toplevel-management close request both use this path.
pub(crate) fn close_window(window: &Window) {
    if let Some(top) = window.toplevel() {
        top.send_close();
    }
    if let Some(x11) = window.x11_surface()
        && let Err(error) = x11.close()
    {
        tracing::warn!(%error, "close X11 window");
    }
}
