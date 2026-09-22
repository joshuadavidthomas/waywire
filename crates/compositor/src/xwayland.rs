//! Rootless software Xwayland and its small floating-window manager.
use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use smithay::desktop::Window;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::timer::TimeoutAction;
use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::Logical;
use smithay::utils::Rectangle;
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::selection::data_device::clear_data_device_selection;
use smithay::wayland::selection::data_device::current_data_device_selection_userdata;
use smithay::wayland::selection::data_device::set_data_device_selection;
use smithay::wayland::xwayland_shell::XWaylandShellHandler;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::X11Surface;
use smithay::xwayland::X11Wm;
use smithay::xwayland::XWayland;
use smithay::xwayland::XWaylandEvent;
use smithay::xwayland::XwmHandler;
use smithay::xwayland::xwm::Reorder;
use smithay::xwayland::xwm::ResizeEdge;
use smithay::xwayland::xwm::XwmId;

use crate::State;
use crate::clipboard::SelectionData;

// Keep X11 hit testing in the scene's stacking order. This also flushes XWM
// requests issued by Wayland callbacks (notably new_selection, which does not
// flush its SetSelectionOwner request in the pinned Smithay revision).
pub(crate) fn sync_stacking(state: &mut State) {
    if let Some(xwm) = &mut state.xwm
        && let Err(error) = xwm
            .update_stacking_order_upwards(state.space.elements().filter_map(Window::x11_surface))
    {
        tracing::warn!(%error, "X11 stacking synchronization failed");
    }
}

pub(crate) fn start(
    handle: &LoopHandle<'static, State>,
    state: &mut State,
    eis_path: &Path,
) -> Result<()> {
    let (xwayland, client) = XWayland::spawn(
        &state.display_handle,
        None,
        [("LIBEI_SOCKET", eis_path.as_os_str())],
        ["-nolisten", "tcp"],
        true,
        Stdio::null(),
        Stdio::inherit(),
        |_| {},
    )?;
    let ready_handle = handle.clone();
    let ready_client = client.clone();
    let token = handle.insert_source(xwayland, move |event, (), state| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => {
            match X11Wm::start_wm(
                ready_handle.clone(),
                &state.display_handle,
                x11_socket,
                ready_client.clone(),
            ) {
                Ok(wm) => {
                    state.xwm = Some(wm);
                    state.xdisplay = Some(display_number);
                    tracing::info!(display_number, "Xwayland ready");
                }
                Err(error) => state.fail(anyhow!("Xwayland WM startup: {error}")),
            }
        }
        XWaylandEvent::Error => state.fail(anyhow!("Xwayland startup failed")),
    })?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let monitor_handle = handle.clone();
    handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(100)),
            move |_, (), state| {
                // Smithay's readiness source does not treat display-fd EOF as failure.
                // Also retain the source after Ready: dropping it terminates Xwayland.
                if client.get_credentials(&state.display_handle).is_err() {
                    monitor_handle.remove(token);
                    state.fail(anyhow!("Xwayland disconnected"));
                    TimeoutAction::Drop
                } else if state.xdisplay.is_none() && Instant::now() >= deadline {
                    monitor_handle.remove(token);
                    state.fail(anyhow!("Xwayland did not become ready within 10 seconds"));
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(Duration::from_millis(100))
                }
            },
        )
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(())
}

#[derive(Default)]
struct RestoreGeometry(RefCell<Option<Rectangle<i32, Logical>>>);

impl State {
    fn x11_window(&self, surface: &X11Surface) -> Option<Window> {
        self.space
            .elements()
            .find(|window| window.x11_surface() == Some(surface))
            .cloned()
    }

    fn x11_geometry(&mut self, surface: &X11Surface, geometry: Rectangle<i32, Logical>) {
        if let Err(error) = surface.configure(geometry) {
            tracing::warn!(%error, "X11 configure failed");
            return;
        }
        if let Some(window) = self.x11_window(surface) {
            self.space.map_element(window, geometry.loc, false);
        }
        self.dirty = true;
    }

    #[expect(
        clippy::expect_used,
        reason = "user data is inserted immediately before access"
    )]
    fn x11_zoom(&mut self, surface: &X11Surface, fullscreen: bool, enabled: bool) {
        surface
            .user_data()
            .insert_if_missing(RestoreGeometry::default);
        let saved = surface
            .user_data()
            .get::<RestoreGeometry>()
            .expect("inserted restore geometry");
        let was_expanded = surface.is_fullscreen() || surface.is_maximized();
        let was_enabled = if fullscreen {
            surface.is_fullscreen()
        } else {
            surface.is_maximized()
        };
        if enabled == was_enabled {
            return;
        }
        let result = if fullscreen {
            surface.set_fullscreen(enabled)
        } else {
            surface.set_maximized(enabled)
        };
        if let Err(error) = result {
            tracing::warn!(%error, "X11 state update failed");
            return;
        }
        if enabled && !was_expanded {
            // geometry() is the last committed buffer's local bbox; the WM
            // needs the most recent global configure even before repaint.
            *saved.0.borrow_mut() = Some(surface.last_configure());
        }
        if surface.is_fullscreen() || surface.is_maximized() {
            if let Some(geometry) = self
                .x11_window(surface)
                .and_then(|window| self.window_output_geometry(&window))
            {
                self.x11_geometry(surface, geometry);
            }
        } else if let Some(geometry) = saved.0.borrow_mut().take() {
            self.x11_geometry(surface, geometry);
        }
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl XwmHandler for State {
    #[expect(
        clippy::expect_used,
        reason = "XWM events are dispatched only after registration"
    )]
    fn xwm_state(&mut self, _: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("XWM callback before WM registration")
    }
    fn new_window(&mut self, _: XwmId, _: X11Surface) {}
    fn new_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}

    fn map_window_request(&mut self, _: XwmId, surface: X11Surface) {
        if let Err(error) = surface.set_mapped(true) {
            tracing::warn!(%error, "X11 map failed");
            return;
        }
        let mut geometry = surface.last_configure();
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // Modulo 8.
        let offset = (self.space.elements().count() % 8) as i32 * 48;
        geometry.loc = (32 + offset, 32 + offset).into();
        self.space
            .map_element(Window::new_x11_window(surface.clone()), geometry.loc, true);
        self.x11_geometry(&surface, geometry);
    }

    fn mapped_override_redirect_window(&mut self, _: XwmId, surface: X11Surface) {
        let location = surface.last_configure().loc;
        self.space
            .map_element(Window::new_x11_window(surface), location, false);
        self.dirty = true;
    }

    fn unmapped_window(&mut self, _: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.space.unmap_elem(&window);
        }
        if !surface.is_override_redirect()
            && let Err(error) = surface.set_mapped(false)
        {
            tracing::debug!(%error, "X11 window disappeared during unmap");
        }
        self.dirty = true;
    }

    fn destroyed_window(&mut self, xwm: XwmId, surface: X11Surface) {
        self.unmapped_window(xwm, surface);
    }

    fn configure_request(
        &mut self,
        _: XwmId,
        surface: X11Surface,
        _: Option<i32>,
        _: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        let mut geometry = surface.last_configure();
        if !surface.is_fullscreen() && !surface.is_maximized() {
            if let Some(width) = width {
                geometry.size.w = width.clamp(1, u16::MAX.into()).cast_signed();
            }
            if let Some(height) = height {
                geometry.size.h = height.clamp(1, u16::MAX.into()).cast_signed();
            }
        }
        self.x11_geometry(&surface, geometry);
        if reorder == Some(Reorder::Top)
            && let Some(window) = self.x11_window(&surface)
        {
            self.space.raise_element(&window, false);
        }
    }

    fn configure_notify(
        &mut self,
        _: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<u32>,
    ) {
        if let Some(window) = self.x11_window(&surface) {
            self.space.map_element(window.clone(), geometry.loc, false);
            if let Some(reference) = above.and_then(|id| {
                self.space
                    .elements()
                    .find(|w| w.x11_surface().is_some_and(|s| s.window_id() == id))
                    .cloned()
            }) {
                self.space.raise_element_above(&window, &reference, false);
            }
        }
        self.dirty = true;
    }

    fn maximize_request(&mut self, _: XwmId, surface: X11Surface) {
        self.x11_zoom(&surface, false, true);
    }
    fn unmaximize_request(&mut self, _: XwmId, surface: X11Surface) {
        self.x11_zoom(&surface, false, false);
    }
    fn fullscreen_request(&mut self, _: XwmId, surface: X11Surface) {
        self.x11_zoom(&surface, true, true);
    }
    fn unfullscreen_request(&mut self, _: XwmId, surface: X11Surface) {
        self.x11_zoom(&surface, true, false);
    }

    fn active_window_request(
        &mut self,
        _: XwmId,
        surface: X11Surface,
        _: u32,
        _: Option<X11Surface>,
    ) {
        if let Some(window) = self.x11_window(&surface) {
            self.focus_window(Some(window));
        }
    }

    fn move_request(&mut self, _: XwmId, surface: X11Surface, _: u32) {
        if let (Some(window), Some(serial)) = (
            self.x11_window(&surface),
            self.seat
                .get_pointer()
                .and_then(|p| p.with_grab(|serial, _| serial)),
        ) {
            self.start_move(window, serial);
        }
    }

    fn resize_request(&mut self, _: XwmId, surface: X11Surface, _: u32, edge: ResizeEdge) {
        let edge = match edge {
            ResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
            ResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
            ResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
            ResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
            ResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
            ResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
            ResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
            ResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
        };
        if let (Some(window), Some(serial)) = (
            self.x11_window(&surface),
            self.seat
                .get_pointer()
                .and_then(|p| p.with_grab(|serial, _| serial)),
        ) {
            self.start_resize(window, serial, edge);
        }
    }

    fn allow_selection_access(&mut self, _: XwmId, selection: SelectionTarget) -> bool {
        // One trusted desktop session; clipboard clients such as xclip need no window.
        selection == SelectionTarget::Clipboard
    }

    fn send_selection(
        &mut self,
        _: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        if selection == SelectionTarget::Clipboard {
            self.send_clipboard(mime_type, fd);
        }
    }

    fn new_selection(&mut self, _: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        if selection == SelectionTarget::Clipboard {
            set_data_device_selection(
                &self.display_handle,
                &self.seat,
                mime_types.clone(),
                SelectionData::X11,
            );
            self.clipboard_offer(mime_types);
        }
    }

    fn cleared_selection(&mut self, _: XwmId, selection: SelectionTarget) {
        if selection == SelectionTarget::Clipboard
            && current_data_device_selection_userdata(&self.seat)
                .is_some_and(|data| matches!(*data, SelectionData::X11))
        {
            clear_data_device_selection(&self.display_handle, &self.seat);
            self.clipboard_offer(Vec::new());
        }
    }

    fn disconnected(&mut self, _: XwmId) {
        self.xwm = None;
        self.xdisplay = None;
        self.fail(anyhow!("Xwayland WM disconnected"));
    }
}
