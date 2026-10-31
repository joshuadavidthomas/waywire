//! Rootless software Xwayland and its small floating-window manager.
use std::cell::RefCell;
use std::fmt::Write;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use nix::sys::signal::SigSet;
use nix::sys::signal::SigmaskHow;
use nix::sys::signal::pthread_sigmask;
use smithay::desktop::Window;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::timer::TimeoutAction;
use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::x11rb;
use smithay::reexports::x11rb::connection::Connection;
use smithay::reexports::x11rb::protocol::xproto::AtomEnum;
use smithay::reexports::x11rb::protocol::xproto::ConnectionExt;
use smithay::reexports::x11rb::protocol::xproto::PropMode;
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
use smithay::xwayland::xwm::WmWindowProperty;
use smithay::xwayland::xwm::XwmId;

use crate::State;
use crate::clipboard::SelectionData;

// Keep X11 hit testing in the scene's stacking order. This also flushes XWM
// requests issued by Wayland callbacks (notably new_selection, which does not
// flush its SetSelectionOwner request in the pinned Smithay revision).
pub(crate) fn sync_stacking(state: &mut State) {
    let Some(xwm) = &mut state.xwm else {
        return;
    };
    // Smithay grabs and ungrabs the X server on every call, which woke
    // Xwayland on every loop iteration. Map, unmap and raise_window keep its
    // stacking list in step with the space, so only a reorder or an unflushed
    // request needs a sync.
    let order: Vec<u32> = state
        .space
        .elements()
        .filter_map(Window::x11_surface)
        .map(X11Surface::window_id)
        .collect();
    if state.x11_stacking.as_ref() == Some(&order) {
        return;
    }
    match xwm.update_stacking_order_upwards(state.space.elements().filter_map(Window::x11_surface))
    {
        Ok(()) => state.x11_stacking = Some(order),
        Err(error) => tracing::warn!(%error, "X11 stacking synchronization failed"),
    }
}

pub(crate) fn start(
    handle: &LoopHandle<'static, State>,
    state: &mut State,
    eis_path: &Path,
) -> Result<()> {
    // `Signals::new` in run() blocked SIGTERM and SIGINT before this spawn, and
    // the fork inherits the blocked mask. Smithay's XWayland::spawn offers no
    // pre-exec hook for the signal mask, so clear it around the spawn call and
    // restore the previous mask afterwards.
    let mut previous_mask = SigSet::empty();
    pthread_sigmask(
        SigmaskHow::SIG_SETMASK,
        Some(&SigSet::empty()),
        Some(&mut previous_mask),
    )
    .map_err(|error| anyhow!("could not unblock signals before Xwayland spawn: {error}"))?;
    let spawned = XWayland::spawn(
        &state.display_handle,
        None,
        [("LIBEI_SOCKET", eis_path.as_os_str())],
        ["-nolisten", "tcp"],
        true,
        Stdio::null(),
        Stdio::inherit(),
        |_| {},
    );
    if let Err(error) = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous_mask), None) {
        tracing::warn!(%error, "could not restore signals after Xwayland spawn");
    }
    let (xwayland, client) = spawned?;
    state.xwayland_client = Some(client.clone());
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
                    if let Err(error) = update_scale(state) {
                        state.fail(error.context("configure Xwayland scale"));
                    }
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
                } else if state.xdisplay.is_some() {
                    TimeoutAction::Drop
                } else if Instant::now() >= deadline {
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

fn update_resources(display: u32, scale: f64) -> Result<()> {
    let (connection, screen) = x11rb::connect(Some(&format!(":{display}")))?;
    let root = connection.setup().roots[screen].root;
    let property = AtomEnum::RESOURCE_MANAGER;
    let reply = connection
        .get_property(false, root, property, AtomEnum::STRING, 0, u32::MAX)?
        .reply()?;
    let old = String::from_utf8_lossy(&reply.value);
    let mut resources = old
        .lines()
        .filter(|line| !line.trim_start().starts_with("Xft.dpi:"))
        .collect::<Vec<_>>()
        .join("\n");
    if !resources.is_empty() {
        resources.push('\n');
    }
    writeln!(resources, "Xft.dpi:\t{}", 96.0 * scale)?;
    connection
        .change_property(
            PropMode::REPLACE,
            root,
            property,
            AtomEnum::STRING,
            8,
            resources.len().try_into()?,
            resources.as_bytes(),
        )?
        .check()?;
    connection.flush()?;
    Ok(())
}

pub(crate) fn update_scale(state: &mut State) -> Result<()> {
    let (Some(client), Some(display), Some(xwm)) =
        (&state.xwayland_client, state.xdisplay, &mut state.xwm)
    else {
        return Ok(());
    };
    // Match cosmic-comp's fractional descaling: retain 1.5 rather than rounding
    // Xwayland to 2, so X11 coordinates and toolkit DPI use the output scale.
    let scale = state.output.current_scale().fractional_scale().max(1.0);
    let geometries = state
        .space
        .elements()
        .filter_map(Window::x11_surface)
        .map(|surface| (surface.clone(), surface.last_configure()))
        .collect::<Vec<_>>();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "output scale and DPI are bounded"
    )]
    let settings = [
        ("Xft/DPI".into(), (scale * 96.0 * 1024.0).round() as i32),
        ("Gdk/WindowScalingFactor".into(), scale.floor() as i32),
        (
            "Gdk/UnscaledDPI".into(),
            (96.0 * 1024.0 * (1.0 + scale.fract() / scale.floor())).round() as i32,
        ),
    ];
    xwm.set_xsettings(
        settings
            .into_iter()
            .map(|(name, value)| (name, value.into())),
    )?;
    update_resources(display, scale)?;
    let data = client
        .get_data::<smithay::xwayland::XWaylandClientData>()
        .ok_or_else(|| anyhow!("missing Xwayland client data"))?;
    if data.compositor_state.client_scale().to_bits() == scale.to_bits() {
        return Ok(());
    }
    data.compositor_state.set_client_scale(scale);
    state.output.change_current_state(None, None, None, None);
    for (surface, geometry) in geometries {
        if !surface.is_override_redirect() {
            surface.configure(geometry)?;
        }
    }
    state.x11_stacking = None;
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
            let location = geometry.loc + window.geometry().loc;
            self.space.map_element(window, location, false);
        }
        self.refresh_pointer_focus();
        self.dirty = true;
    }

    #[expect(
        clippy::expect_used,
        reason = "user data is inserted immediately before access"
    )]
    pub(crate) fn x11_zoom(&mut self, surface: &X11Surface, fullscreen: bool, enabled: bool) {
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
        if let Some(window) = self.x11_window(surface)
            && let Some(state) = self.windows.get_mut(&window)
        {
            state.resize = None;
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
                self.x11_geometry(surface, geometry + surface.frame_extents());
            }
        } else if let Some(geometry) = saved.0.borrow_mut().take() {
            self.x11_geometry(surface, geometry);
        }
        self.refresh_toplevel_states();
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
        let window = Window::new_x11_window(surface.clone());
        let location = smithay::utils::Point::<i32, Logical>::from((32 + offset, 32 + offset));
        geometry.loc = location - window.geometry().loc;
        self.space.map_element(window.clone(), location, true);
        self.enforce_topmost();
        self.toplevel_list.publish(&window);
        self.x11_geometry(&surface, geometry);
        self.center_dialog(&window);
    }

    fn mapped_override_redirect_window(&mut self, _: XwmId, surface: X11Surface) {
        let location = surface.last_configure().loc + surface.geometry().loc;
        self.space
            .map_element(Window::new_x11_window(surface), location, false);
        self.refresh_pointer_focus();
        self.dirty = true;
    }

    fn unmapped_window(&mut self, _: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.space.unmap_elem(&window);
            self.window_unmapped(&window);
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
            self.enforce_topmost();
            self.refresh_pointer_focus();
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
            let location = geometry.loc + window.geometry().loc;
            self.space.map_element(window.clone(), location, false);
            if let Some(reference) = above.and_then(|id| {
                self.space
                    .elements()
                    .find(|w| w.x11_surface().is_some_and(|s| s.window_id() == id))
                    .cloned()
            }) {
                self.space.raise_element_above(&window, &reference, false);
                self.enforce_topmost();
            }
        }
        self.refresh_pointer_focus();
        self.dirty = true;
    }

    fn property_notify(&mut self, _: XwmId, surface: X11Surface, property: WmWindowProperty) {
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class)
            && let Some(window) = self.x11_window(&surface)
        {
            self.toplevel_list.update(&window);
        }
    }

    fn modal_request(&mut self, _: XwmId, surface: X11Surface) {
        if let Err(error) = surface.set_modal(true) {
            tracing::warn!(%error, "set X11 modal state");
        }
    }

    fn unmodal_request(&mut self, _: XwmId, surface: X11Surface) {
        if let Err(error) = surface.set_modal(false) {
            tracing::warn!(%error, "clear X11 modal state");
        }
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
