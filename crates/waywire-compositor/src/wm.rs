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
use smithay::input::Seat;
use smithay::input::keyboard::KeyboardSource;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::Rectangle;
use smithay::utils::SERIAL_COUNTER;
use smithay::utils::Serial;
use smithay::utils::Size;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::shell::xdg::dialog::ToplevelDialogHint;
pub(crate) use waywire_decorations::Activation;
pub(crate) use waywire_decorations::Stacking;
use waywire_decorations::Visibility as MappedVisibility;
use waywire_decorations::WindowAction;

use crate::State;
use crate::delivery;
use crate::focus;

pub(crate) fn map_window(
    space: &mut Space<Window>,
    window: Window,
    location: impl Into<Point<i32, Logical>>,
    activation: Activation,
) {
    space.map_element(window, location, matches!(activation, Activation::Active));
}

pub(crate) fn raise_window(space: &mut Space<Window>, window: &Window, activation: Activation) {
    space.raise_element(window, matches!(activation, Activation::Active));
}

pub(crate) fn raise_window_above(
    space: &mut Space<Window>,
    window: &Window,
    reference: &Window,
    activation: Activation,
) {
    space.raise_element_above(window, reference, matches!(activation, Activation::Active));
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Visibility {
    #[default]
    Unmapped,
    Mapped(MappedVisibility),
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Urgency {
    #[default]
    Normal,
    Urgent,
}

#[derive(Clone, Copy)]
pub(crate) enum ExpandRequest {
    Maximize,
    Unmaximize,
    Fullscreen,
    Unfullscreen,
}

impl ExpandRequest {
    pub(crate) fn mode(self) -> xdg_toplevel::State {
        match self {
            Self::Maximize | Self::Unmaximize => xdg_toplevel::State::Maximized,
            Self::Fullscreen | Self::Unfullscreen => xdg_toplevel::State::Fullscreen,
        }
    }

    pub(crate) fn enabled(self) -> bool {
        matches!(self, Self::Maximize | Self::Fullscreen)
    }
}

pub(crate) struct WindowState {
    visibility: Visibility,
    always_on_top: Stacking,
    /// An activation request without a valid token asked for attention.
    urgency: Urgency,
    decoration: Option<waywire_decorations::Decoration>,
    decoration_mode: Option<smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode>,
    restore: Option<Rectangle<i32, Logical>>,
    resize: Option<(u32, Rectangle<i32, Logical>, Option<Serial>)>,
    grab_serial: Option<Serial>,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            visibility: Visibility::Unmapped,
            always_on_top: Stacking::Normal,
            urgency: Urgency::Normal,
            decoration: None,
            decoration_mode: None,
            restore: None,
            resize: None,
            grab_serial: None,
        }
    }
}

impl WindowState {
    pub(crate) fn visibility(&self) -> Visibility {
        self.visibility
    }
    pub(crate) fn set_visibility(&mut self, visibility: Visibility) {
        self.visibility = visibility;
    }
    pub(crate) fn is_mapped(&self) -> bool {
        matches!(self.visibility, Visibility::Mapped(_))
    }
    pub(crate) fn is_minimized(&self) -> bool {
        self.visibility == Visibility::Mapped(MappedVisibility::Minimized)
    }
    pub(crate) fn is_urgent(&self) -> bool {
        self.urgency == Urgency::Urgent
    }
    pub(crate) fn mark_urgent(&mut self) {
        self.urgency = Urgency::Urgent;
    }
    pub(crate) fn decoration_mut(&mut self) -> &mut Option<waywire_decorations::Decoration> {
        &mut self.decoration
    }
    pub(crate) fn decoration_mode(&self) -> Option<smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode>{
        self.decoration_mode
    }
    pub(crate) fn set_decoration_mode(
        &mut self,
        mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    ) {
        self.decoration_mode = Some(mode);
    }
    pub(crate) fn restore_mut(&mut self) -> &mut Option<Rectangle<i32, Logical>> {
        &mut self.restore
    }
    pub(crate) fn resize(&self) -> Option<(u32, Rectangle<i32, Logical>, Option<Serial>)> {
        self.resize
    }
    pub(crate) fn resize_mut(
        &mut self,
    ) -> &mut Option<(u32, Rectangle<i32, Logical>, Option<Serial>)> {
        &mut self.resize
    }
    pub(crate) fn grab_serial(&self) -> Option<Serial> {
        self.grab_serial
    }
    pub(crate) fn set_grab_serial(&mut self, serial: Option<Serial>) {
        self.grab_serial = serial;
    }
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
        self.states
            .get(window)
            .is_some_and(WindowState::is_minimized)
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
            state.visibility = Visibility::Unmapped;
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
        if state.is_minimized() {
            return false;
        }
        state.visibility = Visibility::Mapped(MappedVisibility::Minimized);
        self.minimized_order.push(window.clone());
        true
    }

    /// Clear a window's minimized state. Returns whether it was minimized.
    pub(crate) fn unminimize(&mut self, window: &Window) -> bool {
        let Some(state) = self
            .states
            .get_mut(window)
            .filter(|state| state.is_minimized())
        else {
            return false;
        };
        state.visibility = Visibility::Mapped(MappedVisibility::Shown);
        self.minimized_order.retain(|item| item != window);
        true
    }

    pub(crate) fn toggle_always_on_top(&mut self, window: &Window) {
        let state = self.states.entry(window.clone()).or_default();
        state.always_on_top = match state.always_on_top {
            Stacking::Normal => Stacking::AlwaysOnTop,
            Stacking::AlwaysOnTop => Stacking::Normal,
        };
    }

    pub(crate) fn stacking(&self, window: &Window) -> Stacking {
        self.states
            .get(window)
            .map_or(Stacking::Normal, |state| state.always_on_top)
    }

    pub(crate) fn clear_urgent(&mut self, window: &Window) {
        if let Some(state) = self.states.get_mut(window) {
            state.urgency = Urgency::Normal;
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
                    && self.states.get(*window).is_some_and(|state| {
                        state.visibility == Visibility::Mapped(MappedVisibility::Shown)
                    })
            })
            .cloned()
    }

    /// The visible dialogs whose parent is `parent`, bottom to top.
    fn dialog_children(&self, space: &Space<Window>, parent: &Window) -> Vec<Window> {
        space
            .elements()
            .filter(|candidate| {
                self.states.get(*candidate).is_some_and(|state| {
                    state.visibility == Visibility::Mapped(MappedVisibility::Shown)
                }) && parent_window(space, candidate).as_ref() == Some(parent)
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
                        .is_some_and(|state| state.always_on_top == Stacking::AlwaysOnTop)
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

/// The compositor applies window management's decisions to the space, the
/// seat, the XWM and the toplevel list.
impl State {
    pub(crate) fn window_pid(&self, window: &Window) -> Option<u32> {
        if let Some(surface) = window.x11_surface() {
            return surface.pid();
        }
        let surface = window.toplevel()?.wl_surface();
        let credentials = surface
            .client()?
            .get_credentials(&self.display_handle)
            .ok()?;
        u32::try_from(credentials.pid).ok()
    }

    pub(crate) fn update_titlebar_drag(
        &mut self,
        source: KeyboardSource,
        location: Point<f64, Logical>,
    ) {
        if let Some(press) = self.chrome.start_restore_drag(source, location)
            && let Some(maximized) = self.window_output_geometry(press.window())
        {
            let restored = self
                .windows
                .get(press.window())
                .and_then(|state| state.restore);
            self.window_action(press.window(), WindowAction::Restore);
            let restored_size = restored.map_or_else(
                || {
                    press
                        .window()
                        .x11_surface()
                        .map_or(press.window().geometry().size, |surface| {
                            surface.last_configure().size
                        })
                },
                |geometry| geometry.size,
            );
            let position =
                restored_drag_location(press.location(), location, maximized, restored_size);
            crate::wm::map_window(
                &mut self.space,
                press.window().clone(),
                position,
                crate::wm::Activation::Active,
            );
            self.enforce_topmost();
            if let Some(surface) = press.window().x11_surface()
                && let Err(error) = surface.configure(Some(
                    smithay::utils::Rectangle::new(position, restored_size)
                        + surface.frame_extents(),
                ))
            {
                tracing::warn!(%error, "position restored X11 window");
            }
            self.window_action(press.window(), WindowAction::Move);
        }
        let held = self
            .input
            .buttons(source)
            .is_some_and(|buttons| buttons.contains(&0x110));
        if held {
            self.chrome.titlebar_motion(source, location);
        }
    }

    pub(crate) fn window_action(&mut self, window: &Window, action: WindowAction) {
        match action {
            WindowAction::Focus => self.focus_window(Some(window.clone())),
            WindowAction::Move => {
                self.start_decoration_action(window.clone(), SERIAL_COUNTER.next_serial(), None);
            }
            WindowAction::Resize(edges) => {
                if edges != xdg_toplevel::ResizeEdge::None {
                    self.start_decoration_action(
                        window.clone(),
                        SERIAL_COUNTER.next_serial(),
                        Some(edges),
                    );
                }
            }
            WindowAction::Maximize => {
                self.set_window_expanded(window, crate::wm::ExpandRequest::Maximize);
            }
            WindowAction::Minimize => {
                if self.windows.minimize(window) {
                    if let Some(surface) = window.x11_surface()
                        && let Err(error) = surface.set_hidden(true)
                    {
                        tracing::warn!(%error, "set X11 hidden state");
                    }
                    if window.x11_surface().is_some() {
                        self.xwayland.restack();
                    }
                    let focused = self
                        .seat
                        .get_keyboard()
                        .and_then(|keyboard| keyboard.current_focus());
                    if focused == focus::KeyboardFocus::window(window) {
                        let next = self.windows.topmost_focusable(&self.space, Some(window));
                        self.with_focus_cause(delivery::FocusCause::Minimized, |state| {
                            state.focus_window(next);
                        });
                    }
                    self.refresh_toplevel_states();
                    self.refresh_pointer_focus();
                    self.frame_request.request(crate::FrameRequest::FullRedraw);
                }
            }
            WindowAction::Restore => {
                if self.windows.unminimize(window) {
                    if let Some(surface) = window.x11_surface()
                        && let Err(error) = surface.set_hidden(false)
                    {
                        tracing::warn!(%error, "clear X11 hidden state");
                    }
                    if window.x11_surface().is_some() {
                        self.xwayland.restack();
                    }
                    self.focus_window(Some(window.clone()));
                } else {
                    self.set_window_expanded(window, crate::wm::ExpandRequest::Unmaximize);
                }
            }
            WindowAction::Close => close_window(window),
            WindowAction::ToggleAlwaysOnTop => {
                self.windows.toggle_always_on_top(window);
                self.enforce_topmost();
                self.frame_request.request(crate::FrameRequest::FullRedraw);
            }
            WindowAction::ShowMenu => {
                if let (Some(pointer), Some(output)) = (
                    self.seat.get_pointer(),
                    self.space.output_geometry(&self.output),
                ) {
                    self.chrome.open_menu(
                        window.clone(),
                        pointer.current_location(),
                        output.size,
                        self.windows.stacking(window),
                    );
                    self.refresh_pointer_focus();
                    self.frame_request.request(crate::FrameRequest::FullRedraw);
                }
            }
        }
    }

    /// Activate a window from a foreign-toplevel request when the request's
    /// seat belongs to this compositor.
    pub(crate) fn activate_foreign_toplevel(&mut self, window: &Window, seat: &WlSeat) {
        if Seat::<Self>::from_resource(seat).as_ref() != Some(&self.seat) {
            return;
        }
        self.activate_window(window);
    }

    /// Restore a minimized window, or raise and focus it. Foreign-toplevel
    /// and xdg-activation requests both use this path.
    pub(crate) fn activate_window(&mut self, window: &Window) {
        if self.windows.is_minimized(window) {
            self.window_action(window, WindowAction::Restore);
        } else {
            self.window_action(window, WindowAction::Focus);
        }
    }

    /// Apply a maximize or fullscreen request to the window's native surface.
    /// Both paths already exist for client requests; this only selects the
    /// Wayland or X11 one.
    pub(crate) fn set_window_expanded(&mut self, window: &Window, request: ExpandRequest) {
        if let Some(top) = window.toplevel() {
            self.set_expanded(top, request);
        } else if let Some(surface) = window.x11_surface() {
            self.x11_zoom(surface, request);
        }
    }

    /// Recompute every foreign-toplevel's state from the seat's current focus.
    pub(crate) fn refresh_toplevel_states(&mut self) {
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        self.toplevel_list
            .refresh_states(focused.as_ref(), self.windows.minimized_order());
    }

    /// Raise always-on-top windows and their dialogs above the rest.
    pub(crate) fn enforce_topmost(&mut self) {
        for window in self.windows.topmost(&self.space) {
            crate::wm::raise_window(&mut self.space, &window, crate::wm::Activation::Inactive);
        }
    }

    pub(crate) fn center_dialog(&mut self, window: &Window) {
        let Some(location) =
            dialog_location(&self.space, window, self.window_output_geometry(window))
        else {
            return;
        };
        crate::wm::map_window(
            &mut self.space,
            window.clone(),
            location,
            crate::wm::Activation::Inactive,
        );
        if let Some(surface) = window.x11_surface() {
            let mut geometry = surface.last_configure();
            geometry.loc = location - window.geometry().loc;
            if let Err(error) = surface.configure(geometry) {
                tracing::warn!(%error, "center X11 dialog");
            }
        }
    }

    pub(crate) fn focus_window(&mut self, window: Option<Window>) {
        if window
            .as_ref()
            .is_some_and(|window| self.windows.is_minimized(window))
        {
            return;
        }
        let mut window = window;
        if let Some(target) = &window {
            for member in &self.windows.family(&self.space, target) {
                crate::wm::raise_window(&mut self.space, member, crate::wm::Activation::Active);
                if let Some(surface) = member.x11_surface() {
                    self.xwayland.raise(surface);
                }
            }
            self.enforce_topmost();
            window = Some(self.windows.modal_focus(&self.space, target.clone()));
        }
        if self
            .chrome
            .armed_close()
            .is_some_and(|(_, target)| Some(target) != window.as_ref())
        {
            self.chrome.disarm_close();
        }
        if let Some(window) = &window {
            // X11 menus, dropdowns and tooltips draw above their app but expect
            // keyboard focus to stay with it; taking focus closes them.
            if is_override_redirect(window) {
                self.refresh_pointer_focus();
                self.frame_request.request(crate::FrameRequest::FullRedraw);
                return;
            }
        }
        if let Some(window) = &window {
            self.windows.clear_urgent(window);
        }
        self.refresh_pointer_focus();
        for current in self.space.elements() {
            current.set_activated(window.as_ref() == Some(current));
            if let Some(top) = current.toplevel()
                && top.is_initial_configure_sent()
            {
                top.send_pending_configure();
            }
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                window.as_ref().and_then(focus::KeyboardFocus::window),
                SERIAL_COUNTER.next_serial(),
            );
        }
        self.refresh_toplevel_states();
        self.frame_request.request(crate::FrameRequest::FullRedraw);
    }
}
