use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::PopupKind;
use smithay::desktop::Window;
use smithay::desktop::find_popup_root_surface;
use smithay::desktop::get_popup_toplevel_coords;
use smithay::input::Seat;
use smithay::input::SeatHandler;
use smithay::input::SeatState;
use smithay::input::dnd::DndGrabHandler;
use smithay::input::dnd::GrabType;
use smithay::input::dnd::Source;
use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::backend::DisconnectReason;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::CompositorClientState;
use smithay::wayland::compositor::CompositorHandler;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::compositor::get_parent;
use smithay::wayland::compositor::is_sync_subsurface;
use smithay::wayland::fractional_scale::FractionalScaleHandler;
use smithay::wayland::fractional_scale::with_fractional_scale;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::DataDeviceHandler;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::data_device::WaylandDndGrabHandler;
use smithay::wayland::selection::data_device::set_data_device_focus;
use smithay::wayland::shell::xdg::PopupSurface;
use smithay::wayland::shell::xdg::PositionerState;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::XdgShellHandler;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmHandler;
use smithay::wayland::shm::ShmState;
use waywire_protocol::pipe::CursorShape;
use waywire_protocol::pipe::CursorVisibility;
use waywire_protocol::pipe::Event;

use super::State;

#[derive(Default)]
pub(crate) struct ClientState {
    pub(crate) compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    #[expect(
        clippy::expect_used,
        reason = "every accepted client has either ClientState or Smithay XWaylandClientData"
    )]
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(data) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &data.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("registered compositor client")
            .compositor_state
    }
    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = self
                .space
                .elements()
                .find(|w| w.wl_surface().is_some_and(|s| *s == root))
                .cloned();
            if let Some(window) = window {
                let old_size = window.bbox().size;
                let resizing = self
                    .windows
                    .get(&window)
                    .is_some_and(|state| state.resize.is_some());
                window.on_commit();
                self.resize_committed(&window);
                let mapped = smithay::backend::renderer::utils::with_renderer_surface_state(
                    &root,
                    |state| state.buffer().is_some(),
                )
                .unwrap_or(false);
                let previous = self.windows.entry(window.clone()).or_default().mapped;
                self.windows.entry(window.clone()).or_default().mapped = mapped;
                if mapped && (!previous || old_size != window.bbox().size) && !resizing {
                    // Clients can grow after their first commit. Keep their
                    // geometry on output without disturbing resize-grab anchors.
                    self.constrain_window(&window);
                }
                if mapped && !previous {
                    self.focus_window(Some(window.clone()));
                } else if previous && !mapped {
                    self.window_unmapped(&window);
                }
                if let Some(top) = window.toplevel()
                    && !top.is_initial_configure_sent()
                {
                    top.send_configure();
                }
            }
        }
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
            && let Err(error) = popup.send_configure()
        {
            tracing::warn!(%error, "configure popup");
        }
    }
}
impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _: &wl_buffer::WlBuffer) {}
}
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
impl OutputHandler for State {}
impl PointerConstraintsHandler for State {}
impl smithay::input::tablet::TabletSeatHandler for State {
    type ToolFocus = WlSurface;
}
impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        smithay::wayland::compositor::with_states(&surface, |states| {
            with_fractional_scale(states, |scale| {
                scale.set_preferred_scale(self.output.current_scale().fractional_scale());
            });
        });
    }
}
impl SeatHandler for State {
    type KeyboardFocus = super::focus::KeyboardFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focus: Option<&Self::KeyboardFocus>) {
        set_data_device_focus(
            &self.display_handle,
            seat,
            focus
                .and_then(WaylandFocus::wl_surface)
                .and_then(|s| s.client()),
        );
    }
    fn cursor_image(&mut self, _: &Seat<Self>, image: CursorImageStatus) {
        match image {
            CursorImageStatus::Hidden => {
                self.emit(Event::CursorVisibility(CursorVisibility::Hidden));
            }
            CursorImageStatus::Named(icon) => {
                let shape = CursorShape::ALL
                    .into_iter()
                    .find(|s| s.css_name() == icon.name())
                    .unwrap_or(CursorShape::Default);
                self.emit(Event::CursorShape(shape));
                self.emit(Event::CursorVisibility(CursorVisibility::Visible));
            }
            CursorImageStatus::Surface(_) => {
                self.emit(Event::CursorShape(CursorShape::Default));
                self.emit(Event::CursorVisibility(CursorVisibility::Visible));
            }
        }
    }
}
impl SelectionHandler for State {
    type SelectionUserData = super::clipboard::SelectionData;
    fn new_selection(
        &mut self,
        target: smithay::wayland::selection::SelectionTarget,
        source: Option<smithay::wayland::selection::SelectionSource>,
        _: Seat<Self>,
    ) {
        if target == smithay::wayland::selection::SelectionTarget::Clipboard {
            if let Some(xwm) = &mut self.xwm
                && let Err(error) = xwm.new_selection(
                    target,
                    source
                        .as_ref()
                        .map(smithay::wayland::selection::SelectionSource::mime_types),
                )
            {
                tracing::warn!(%error, "forward clipboard ownership to X11");
            }
            self.clipboard_changed(source);
        }
    }
    fn send_selection(
        &mut self,
        target: smithay::wayland::selection::SelectionTarget,
        mime: String,
        fd: std::os::fd::OwnedFd,
        _: Seat<Self>,
        text: &Self::SelectionUserData,
    ) {
        if target == smithay::wayland::selection::SelectionTarget::Clipboard {
            match text {
                super::clipboard::SelectionData::Text(text) => {
                    self.clipboard_send(&mime, fd, std::sync::Arc::clone(text));
                }
                super::clipboard::SelectionData::X11 => {
                    if let Some(xwm) = &mut self.xwm
                        && let Err(error) = xwm.send_selection(target, mime, fd)
                    {
                        tracing::warn!(%error, "send X11 clipboard");
                    }
                }
            }
        }
    }
}
impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}
impl DndGrabHandler for State {}
impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _: Option<WlSurface>,
        _: Seat<Self>,
        _: Serial,
        _: GrabType,
    ) {
        source.cancel();
    }
}
impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let offset = [0, 48, 96, 144, 192, 240, 288, 336][self.space.elements().count() % 8];
        if let Some(output) = self.space.output_geometry(&self.output) {
            surface.with_pending_state(|state| state.bounds = Some(output.size));
        }
        let window = Window::new_wayland_window(surface);
        self.space
            .map_element(window, (32 + offset, 32 + offset), false);
    }
    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        if Seat::<Self>::from_resource(&seat).as_ref() != Some(&self.seat) {
            return;
        }
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel() == Some(&surface))
            .cloned();
        if let Some(window) = window {
            self.start_move(window, serial);
        }
    }
    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        if Seat::<Self>::from_resource(&seat).as_ref() != Some(&self.seat) {
            return;
        }
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel() == Some(&surface))
            .cloned();
        if let Some(window) = window {
            self.start_resize(window, serial, edges);
        }
    }
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.set_expanded(&surface, xdg_toplevel::State::Maximized, true);
    }
    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        self.set_expanded(&surface, xdg_toplevel::State::Fullscreen, true);
    }
    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.set_expanded(&surface, xdg_toplevel::State::Maximized, false);
    }
    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.set_expanded(&surface, xdg_toplevel::State::Fullscreen, false);
    }
    fn new_popup(&mut self, surface: PopupSurface, _: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(error) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!(%error, "track popup");
        }
    }
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        use smithay::desktop::PopupKeyboardGrab;
        use smithay::desktop::PopupPointerGrab;
        use smithay::desktop::PopupUngrabStrategy;
        use smithay::input::pointer::Focus;
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Ok(mut grab) =
            self.popups
                .grab_popup(super::focus::KeyboardFocus::from(root), kind, &seat, serial)
        else {
            return;
        };
        let previous = grab.previous_serial().unwrap_or(serial);
        if let Some(pointer) = seat.get_pointer()
            && pointer.is_grabbed()
            && !(pointer.has_grab(serial) || pointer.has_grab(previous))
        {
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed() && !(keyboard.has_grab(serial) || keyboard.has_grab(previous))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
}
impl State {
    pub(crate) fn window_unmapped(&mut self, window: &Window) {
        if let Some(state) = self.windows.get_mut(window) {
            state.mapped = false;
            state.resize = None;
        }
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        if focused.is_some() && focused == super::focus::KeyboardFocus::window(window) {
            let next = self
                .space
                .elements()
                .rev()
                .find(|candidate| {
                    *candidate != window
                        && self
                            .windows
                            .get(*candidate)
                            .is_some_and(|state| state.mapped)
                        && !candidate
                            .x11_surface()
                            .is_some_and(smithay::xwayland::X11Surface::is_override_redirect)
                })
                .cloned();
            self.focus_window(next);
        }
    }

    fn constrain_window(&mut self, window: &Window) {
        if window
            .x11_surface()
            .is_some_and(smithay::xwayland::X11Surface::is_override_redirect)
        {
            return;
        }
        let (Some(location), Some(output)) = (
            self.space.element_location(window),
            self.window_output_geometry(window),
        ) else {
            return;
        };
        let size = window.geometry().size;
        let location = (
            location
                .x
                .clamp(output.loc.x, output.loc.x + (output.size.w - size.w).max(0)),
            location
                .y
                .clamp(output.loc.y, output.loc.y + (output.size.h - size.h).max(0)),
        );
        self.space.map_element(window.clone(), location, false);
        if let Some(x11) = window.x11_surface()
            && let Err(error) = x11.configure(smithay::utils::Rectangle::new(location.into(), size))
        {
            tracing::warn!(%error, "place X11 window");
        }
    }

    pub(crate) fn window_output_geometry(
        &self,
        window: &Window,
    ) -> Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>> {
        let mut geometry = self.space.output_geometry(&self.output)?;
        if super::decorations::decorated(window) {
            geometry.loc.y += super::decorations::BAR;
            geometry.size.h =
                (geometry.size.h - super::decorations::BAR - super::decorations::STRIP).max(1);
        }
        Some(geometry)
    }

    pub(super) fn output_resized(&mut self) {
        let scale = self.output.current_scale().fractional_scale();
        for window in self.space.elements().cloned().collect::<Vec<_>>() {
            let Some(geometry) = self.window_output_geometry(&window) else {
                continue;
            };
            window.with_surfaces(|_, states| {
                with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale));
            });
            let expanded = if let Some(top) = window.toplevel() {
                let expanded = top.with_pending_state(|state| {
                    state.bounds = Some(geometry.size);
                    let expanded = state.states.contains(xdg_toplevel::State::Maximized)
                        || state.states.contains(xdg_toplevel::State::Fullscreen);
                    if expanded {
                        state.size = Some(geometry.size);
                    }
                    expanded
                });
                top.send_pending_configure();
                expanded
            } else if let Some(x11) = window.x11_surface() {
                let expanded = x11.is_maximized() || x11.is_fullscreen();
                if expanded && let Err(error) = x11.configure(geometry) {
                    tracing::warn!(%error, "resize expanded X11 window");
                }
                expanded
            } else {
                false
            };
            if expanded {
                self.space.map_element(window, geometry.loc, false);
            } else {
                self.constrain_window(&window);
            }
        }
        if let Some(pointer) = self.seat.get_pointer() {
            self.pointer_motion(pointer.current_location());
        }
    }

    fn set_expanded(
        &mut self,
        surface: &ToplevelSurface,
        mode: xdg_toplevel::State,
        enabled: bool,
    ) {
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel() == Some(surface))
            .cloned()
        else {
            return;
        };
        let Some(location) = self.space.element_location(&window) else {
            return;
        };
        let expanded = surface.with_pending_state(|state| {
            if enabled {
                state.states.set(mode);
            } else {
                state.states.unset(mode);
            }
            state.states.contains(xdg_toplevel::State::Maximized)
                || state.states.contains(xdg_toplevel::State::Fullscreen)
        });
        let Some(output) = self.window_output_geometry(&window) else {
            return;
        };
        let window_state = self.windows.entry(window.clone()).or_default();
        window_state.resize = None;
        let geometry = if expanded {
            window_state
                .restore
                .get_or_insert(smithay::utils::Rectangle::new(
                    location,
                    window.geometry().size,
                ));
            output
        } else {
            window_state
                .restore
                .take()
                .unwrap_or(smithay::utils::Rectangle::new(
                    location,
                    window.geometry().size,
                ))
        };
        surface.with_pending_state(|state| {
            state.size = Some(geometry.size);
        });
        surface.send_pending_configure();
        self.space.map_element(window, geometry.loc, true);
        self.dirty = true;
    }
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|top| top.wl_surface() == &root))
        else {
            return;
        };
        let (Some(mut target), Some(window_geo)) = (
            self.space.output_geometry(&self.output),
            self.space.element_geometry(window),
        ) else {
            return;
        };
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
smithay::delegate_dispatch2!(State);
