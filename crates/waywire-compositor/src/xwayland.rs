//! Rootless software Xwayland and its small floating-window manager.
use std::cell::RefCell;
use std::fmt::Write;
use std::os::fd::AsFd;
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
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::Space;
use smithay::desktop::Window;
use smithay::reexports::calloop::Interest;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::Mode;
use smithay::reexports::calloop::PostAction;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::TimeoutAction;
use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::x11rb;
use smithay::reexports::x11rb::connection::Connection;
use smithay::reexports::x11rb::errors::ReplyOrIdError;
use smithay::reexports::x11rb::protocol::xproto::AtomEnum;
use smithay::reexports::x11rb::protocol::xproto::ChangeWindowAttributesAux;
use smithay::reexports::x11rb::protocol::xproto::ConnectionExt;
use smithay::reexports::x11rb::protocol::xproto::EventMask;
use smithay::reexports::x11rb::protocol::xproto::PropMode;
use smithay::utils::Logical;
use smithay::utils::Rectangle;
use smithay::wayland::compositor::CompositorHandler;
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::XWaylandShellHandler;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::X11Surface;
use smithay::xwayland::X11Wm;
use smithay::xwayland::XWaylandClientData;
use smithay::xwayland::XWaylandEvent;
use smithay::xwayland::XwmHandler;
use smithay::xwayland::xwm::Reorder;
use smithay::xwayland::xwm::ResizeEdge;
use smithay::xwayland::xwm::WmWindowProperty;
use smithay::xwayland::xwm::XwmId;
use waywire_decorations::WindowAction;

use crate::State;

/// The Xwayland server's Wayland client, and the X11 window manager running
/// on it once Xwayland is ready.
#[derive(Default)]
pub(crate) struct XWayland {
    /// Xwayland's own Wayland client, from spawn on.
    client: Option<Client>,
    ready: Option<Ready>,
}

/// What exists once Xwayland is ready, until its window manager disconnects.
struct Ready {
    wm: X11Wm,
    /// The X11 display number.
    display: u32,
    /// Last X11 stacking order sent to the XWM; `None` forces the next sync,
    /// which also flushes requests such as `new_selection` that do not flush.
    stacking: Option<Vec<u32>>,
}

#[derive(PartialEq, Eq)]
enum ScaleUpdate {
    NotReady,
    Unchanged,
    Changed,
}

impl XWayland {
    fn spawned(&mut self, client: Client) {
        self.client = Some(client);
    }

    fn ready(&mut self, wm: X11Wm, display: u32) {
        self.ready = Some(Ready {
            wm,
            display,
            stacking: None,
        });
    }

    fn disconnected(&mut self) {
        self.ready = None;
    }

    /// The X11 display number, once Xwayland is ready.
    pub(crate) fn display(&self) -> Option<u32> {
        self.ready.as_ref().map(|ready| ready.display)
    }

    /// The factor Xwayland's clients scale X11 coordinates by; 1 before
    /// Xwayland starts.
    pub(crate) fn client_scale(&self) -> f64 {
        self.client
            .as_ref()
            .and_then(|client| client.get_data::<XWaylandClientData>())
            .map_or(1.0, |data| data.compositor_state.client_scale())
    }

    #[expect(
        clippy::expect_used,
        reason = "XWM events are dispatched only after registration"
    )]
    fn wm(&mut self) -> &mut X11Wm {
        &mut self
            .ready
            .as_mut()
            .expect("XWM callback before WM registration")
            .wm
    }

    /// Make the next [`Self::sync_stacking`] talk to the XWM, which also
    /// flushes its pending requests.
    pub(crate) fn restack(&mut self) {
        if let Some(ready) = &mut self.ready {
            ready.stacking = None;
        }
    }

    pub(crate) fn raise(&mut self, surface: &X11Surface) {
        if let Some(ready) = &mut self.ready
            && let Err(error) = ready.wm.raise_window(surface)
        {
            tracing::warn!(%error, "raise X11 window");
        }
    }

    /// Give X11 clients `target` with `mime_types`, or clear it for them
    /// with `None`. The next stacking sync flushes the request.
    pub(crate) fn new_selection(
        &mut self,
        target: SelectionTarget,
        mime_types: Option<Vec<String>>,
    ) -> Result<(), ReplyOrIdError> {
        match &mut self.ready {
            Some(ready) => {
                ready.stacking = None;
                ready.wm.new_selection(target, mime_types)
            }
            None => Ok(()),
        }
    }

    /// Ask the X11 owner of `target` to write it into `fd` as `mime_type`.
    pub(crate) fn send_selection(
        &mut self,
        target: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) -> Result<()> {
        if let Some(ready) = &mut self.ready {
            ready.wm.send_selection(target, mime_type, fd)?;
        }
        Ok(())
    }

    // Keep X11 hit testing in the scene's stacking order. This also flushes XWM
    // requests issued by Wayland callbacks (notably new_selection, which does not
    // flush its SetSelectionOwner request in the pinned Smithay revision).
    pub(crate) fn sync_stacking(&mut self, space: &Space<Window>) {
        let Some(ready) = &mut self.ready else {
            return;
        };
        // Smithay grabs and ungrabs the X server on every call, which woke
        // Xwayland on every loop iteration. Map, unmap and raise_window keep its
        // stacking list in step with the space, so only a reorder or an unflushed
        // request needs a sync.
        let order: Vec<u32> = space
            .elements()
            .filter_map(Window::x11_surface)
            .map(X11Surface::window_id)
            .collect();
        if ready.stacking.as_ref() == Some(&order) {
            return;
        }
        match ready
            .wm
            .update_stacking_order_upwards(space.elements().filter_map(Window::x11_surface))
        {
            Ok(()) => ready.stacking = Some(order),
            Err(error) => tracing::warn!(%error, "X11 stacking synchronization failed"),
        }
    }

    /// Tell Xwayland and its toolkits the output's `scale`. Returns whether
    /// the client scale changed, so X11 windows need configuring again.
    fn set_scale(&mut self, scale: f64) -> Result<ScaleUpdate> {
        let (Some(client), Some(Ready { wm, display, .. })) = (&self.client, &mut self.ready)
        else {
            return Ok(ScaleUpdate::NotReady);
        };
        let display = *display;
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
        wm.set_xsettings(
            settings
                .into_iter()
                .map(|(name, value)| (name, value.into())),
        )?;
        update_resources(display, scale)?;
        let data = client
            .get_data::<XWaylandClientData>()
            .ok_or_else(|| anyhow!("missing Xwayland client data"))?;
        if data.compositor_state.client_scale().to_bits() == scale.to_bits() {
            return Ok(ScaleUpdate::Unchanged);
        }
        data.compositor_state.set_client_scale(scale);
        Ok(ScaleUpdate::Changed)
    }
}

/// What a `_NET_WM_STATE` client message asks to do with the states it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NetWmStateAction {
    Remove,
    Add,
    Toggle,
}

impl NetWmStateAction {
    /// The action in a message's first data word; `None` for an unknown one.
    const fn parse(action: u32) -> Option<Self> {
        match action {
            0 => Some(Self::Remove),
            1 => Some(Self::Add),
            2 => Some(Self::Toggle),
            _ => None,
        }
    }
}

/// What a `_NET_WM_STATE` `action` on HIDDEN asks of a window that is
/// `minimized` or not.
const fn hidden_request(
    action: NetWmStateAction,
    visibility: crate::wm::Visibility,
) -> Option<WindowAction> {
    use waywire_decorations::Visibility::Minimized;
    use waywire_decorations::Visibility::Shown;

    use crate::wm::Visibility;
    match (action, visibility) {
        (NetWmStateAction::Remove | NetWmStateAction::Toggle, Visibility::Mapped(Minimized)) => {
            Some(WindowAction::Restore)
        }
        (
            NetWmStateAction::Add | NetWmStateAction::Toggle,
            Visibility::Unmapped | Visibility::Mapped(Shown),
        ) => Some(WindowAction::Minimize),
        (NetWmStateAction::Remove, Visibility::Unmapped | Visibility::Mapped(Shown))
        | (NetWmStateAction::Add, Visibility::Mapped(Minimized)) => None,
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
    let open_abstract_socket = true;
    let spawned = smithay::xwayland::XWayland::spawn(
        &state.display_handle,
        None,
        [("LIBEI_SOCKET", eis_path.as_os_str())],
        ["-nolisten", "tcp"],
        open_abstract_socket,
        Stdio::null(),
        Stdio::inherit(),
        |_| {},
    );
    if let Err(error) = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous_mask), None) {
        tracing::warn!(%error, "could not restore signals after Xwayland spawn");
    }
    let (xwayland, client) = spawned?;
    state.xwayland.spawned(client.clone());
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
                    state.xwayland.ready(wm, display_number);
                    if let Err(error) = watch_hidden_requests(&ready_handle, display_number) {
                        state.fail(error.context("watch X11 hidden requests"));
                    }
                    if let Err(error) = state.update_xwayland_scale() {
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
                } else if state.xwayland.display().is_some() {
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

// Smithay handles WM_CHANGE_STATE, but its XWM does not dispatch HIDDEN
// client messages. EWMH sends them to the root with SUBSTRUCTURE_NOTIFY, so a
// second X11 connection can observe them without claiming window management.
fn watch_hidden_requests(handle: &LoopHandle<'static, State>, display: u32) -> Result<()> {
    let (connection, screen) = x11rb::connect(Some(&format!(":{display}")))?;
    let root = connection.setup().roots[screen].root;
    let only_if_exists = false;
    let net_state = connection
        .intern_atom(only_if_exists, b"_NET_WM_STATE")?
        .reply()?
        .atom;
    let hidden = connection
        .intern_atom(only_if_exists, b"_NET_WM_STATE_HIDDEN")?
        .reply()?
        .atom;
    connection
        .change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()?;
    connection.flush()?;
    let fd = connection.stream().as_fd().try_clone_to_owned()?;
    handle
        .insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |_, _, state| {
                while let Some(event) =
                    connection.poll_for_event().map_err(std::io::Error::other)?
                {
                    if let x11rb::protocol::Event::ClientMessage(event) = event
                        && event.type_ == net_state
                        && event.format == 32
                    {
                        let data = event.data.as_data32();
                        let window = state
                            .space
                            .elements()
                            .find(|window| {
                                window
                                    .x11_surface()
                                    .is_some_and(|surface| surface.window_id() == event.window)
                            })
                            .cloned();
                        if (data[1] == hidden || data[2] == hidden)
                            && let Some(window) = window
                        {
                            let visibility = state.windows.get(&window).map_or(
                                crate::wm::Visibility::Unmapped,
                                crate::wm::WindowState::visibility,
                            );
                            if let Some(action) = NetWmStateAction::parse(data[0])
                                .and_then(|action| hidden_request(action, visibility))
                            {
                                state.window_action(&window, action);
                            }
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| anyhow!("register X11 hidden request listener: {error}"))?;
    Ok(())
}

fn update_resources(display: u32, scale: f64) -> Result<()> {
    let (connection, screen) = x11rb::connect(Some(&format!(":{display}")))?;
    let root = connection.setup().roots[screen].root;
    let property = AtomEnum::RESOURCE_MANAGER;
    let delete = false;
    let reply = connection
        .get_property(delete, root, property, AtomEnum::STRING, 0, u32::MAX)?
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

#[derive(Default)]
struct RestoreGeometry(RefCell<Option<Rectangle<i32, Logical>>>);

impl State {
    // Keep X11 hit testing in the scene's stacking order; see
    // [`XWayland::sync_stacking`].
    pub(crate) fn sync_x11_stacking(&mut self) {
        self.xwayland.sync_stacking(&self.space);
    }

    pub(crate) fn update_xwayland_scale(&mut self) -> Result<()> {
        // Match cosmic-comp's fractional descaling: retain 1.5 rather than rounding
        // Xwayland to 2, so X11 coordinates and toolkit DPI use the output scale.
        let scale = self.output.current_scale().fractional_scale().max(1.0);
        if self.xwayland.set_scale(scale)? != ScaleUpdate::Changed {
            return Ok(());
        }
        self.output.change_current_state(None, None, None, None);
        for surface in self.space.elements().filter_map(Window::x11_surface) {
            if !surface.is_override_redirect() {
                surface.configure(surface.last_configure())?;
            }
        }
        self.xwayland.restack();
        Ok(())
    }

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
            crate::wm::map_window(
                &mut self.space,
                window,
                location,
                crate::wm::Activation::Inactive,
            );
        }
        self.refresh_pointer_focus();
        self.frame_request.request(crate::FrameRequest::FullRedraw);
    }

    #[expect(
        clippy::expect_used,
        reason = "user data is inserted immediately before access"
    )]
    pub(crate) fn x11_zoom(&mut self, surface: &X11Surface, request: crate::wm::ExpandRequest) {
        let fullscreen = request.mode() == smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen;
        let enabled = request.enabled();
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
            *state.resize_mut() = None;
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

    // Xwayland before 23.1 names a window's surface over X11, which can arrive
    // after the surface committed its first buffer. That commit found no window,
    // so replay it now that the surface belongs to one.
    fn surface_associated(&mut self, _: XwmId, surface: WlSurface, _: X11Surface) {
        if with_renderer_surface_state(&surface, |state| state.buffer().is_some()) == Some(true) {
            CompositorHandler::commit(self, &surface);
        }
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _: XwmId) -> &mut X11Wm {
        self.xwayland.wm()
    }
    fn new_window(&mut self, _: XwmId, _: X11Surface) {}
    fn new_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}

    fn map_window_request(&mut self, _: XwmId, surface: X11Surface) {
        let mut geometry = surface.last_configure();
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // Modulo 8.
        let offset = (self.space.elements().count() % 8) as i32 * 48;
        let window = Window::new_x11_window(surface.clone());
        let hint_position = surface.size_hints().and_then(|hints| hints.position);
        if geometry.loc == (0, 0).into()
            && let Some((_, x, y)) = hint_position
        {
            geometry.loc = smithay::utils::Point::<i32, smithay::utils::Physical>::from((x, y))
                .to_f64()
                .to_logical(self.output.current_scale().fractional_scale().max(1.0))
                .to_i32_round();
        }
        let location = if hint_position.is_some() || geometry.loc != (0, 0).into() {
            geometry.loc + window.geometry().loc
        } else {
            smithay::utils::Point::<i32, Logical>::from((32 + offset, 32 + offset))
        };
        geometry.loc = location - window.geometry().loc;
        crate::wm::map_window(
            &mut self.space,
            window.clone(),
            location,
            crate::wm::Activation::Active,
        );
        self.windows.add_mapped(&window);
        self.enforce_topmost();
        self.toplevel_list.publish(&window);
        self.x11_geometry(&surface, geometry);
        if surface.is_maximized() || surface.is_fullscreen() {
            surface
                .user_data()
                .insert_if_missing(RestoreGeometry::default);
            if let Some(saved) = surface.user_data().get::<RestoreGeometry>() {
                *saved.0.borrow_mut() = Some(geometry);
            }
            if let Some(output) = self.window_output_geometry(&window) {
                self.x11_geometry(&surface, output + surface.frame_extents());
            }
        } else {
            self.center_dialog(&window);
            self.constrain_window(&window);
        }
        // Map only once placed, so the window never becomes viewable at the
        // geometry its client requested before the window manager placed it.
        if let Err(error) = surface.set_mapped(true) {
            tracing::warn!(%error, "X11 map failed");
            self.window_unmapped(&window);
            self.space.unmap_elem(&window);
        }
    }

    fn mapped_override_redirect_window(&mut self, _: XwmId, surface: X11Surface) {
        let location = surface.last_configure().loc + surface.geometry().loc;
        crate::wm::map_window(
            &mut self.space,
            Window::new_x11_window(surface),
            location,
            crate::wm::Activation::Inactive,
        );
        self.refresh_pointer_focus();
        self.frame_request.request(crate::FrameRequest::FullRedraw);
    }

    fn unmapped_window(&mut self, _: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.window_unmapped(&window);
            self.space.unmap_elem(&window);
        }
        if !surface.is_override_redirect()
            && let Err(error) = surface.set_mapped(false)
        {
            tracing::debug!(%error, "X11 window disappeared during unmap");
        }
        self.frame_request.request(crate::FrameRequest::FullRedraw);
    }

    fn destroyed_window(&mut self, xwm: XwmId, surface: X11Surface) {
        self.unmapped_window(xwm, surface);
        self.drag_surface_destroyed(None);
    }

    fn configure_request(
        &mut self,
        _: XwmId,
        surface: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        let mut geometry = surface.last_configure();
        if !surface.is_fullscreen() && !surface.is_maximized() {
            if let Some(x) = x {
                geometry.loc.x = x;
            }
            if let Some(y) = y {
                geometry.loc.y = y;
            }
            if let Some(width) = width {
                geometry.size.w = width.clamp(1, u16::MAX.into()).cast_signed();
            }
            if let Some(height) = height {
                geometry.size.h = height.clamp(1, u16::MAX.into()).cast_signed();
            }
        }
        self.x11_geometry(&surface, geometry);
        if (x.is_some() || y.is_some())
            && let Some(window) = self.x11_window(&surface)
        {
            self.constrain_window(&window);
        }
        if reorder == Some(Reorder::Top)
            && let Some(window) = self.x11_window(&surface)
        {
            crate::wm::raise_window(&mut self.space, &window, crate::wm::Activation::Inactive);
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
            crate::wm::map_window(
                &mut self.space,
                window.clone(),
                location,
                crate::wm::Activation::Inactive,
            );
            if let Some(reference) = above.and_then(|id| {
                self.space
                    .elements()
                    .find(|w| w.x11_surface().is_some_and(|s| s.window_id() == id))
                    .cloned()
            }) {
                crate::wm::raise_window_above(
                    &mut self.space,
                    &window,
                    &reference,
                    crate::wm::Activation::Inactive,
                );
                self.enforce_topmost();
            }
        }
        self.refresh_pointer_focus();
        self.frame_request.request(crate::FrameRequest::FullRedraw);
    }

    fn property_notify(&mut self, _: XwmId, surface: X11Surface, property: WmWindowProperty) {
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class)
            && let Some(window) = self.x11_window(&surface)
        {
            if self.toplevel_list.update(&window) {
                self.window_changed(&window);
            }
            self.frame_request.request(crate::FrameRequest::FullRedraw);
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

    fn minimize_request(&mut self, _: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.window_action(&window, WindowAction::Minimize);
        }
    }

    fn unminimize_request(&mut self, _: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface)
            && self
                .windows
                .get(&window)
                .is_some_and(super::wm::WindowState::is_minimized)
        {
            self.window_action(&window, WindowAction::Restore);
        }
    }

    fn maximize_request(&mut self, _: XwmId, surface: X11Surface) {
        self.x11_zoom(&surface, crate::wm::ExpandRequest::Maximize);
    }
    fn unmaximize_request(&mut self, _: XwmId, surface: X11Surface) {
        self.x11_zoom(&surface, crate::wm::ExpandRequest::Unmaximize);
    }
    fn fullscreen_request(&mut self, _: XwmId, surface: X11Surface) {
        self.x11_zoom(&surface, crate::wm::ExpandRequest::Fullscreen);
    }
    fn unfullscreen_request(&mut self, _: XwmId, surface: X11Surface) {
        self.x11_zoom(&surface, crate::wm::ExpandRequest::Unfullscreen);
    }

    fn active_window_request(
        &mut self,
        _: XwmId,
        surface: X11Surface,
        _: u32,
        _: Option<X11Surface>,
    ) {
        if let Some(window) = self.x11_window(&surface) {
            self.with_focus_cause(crate::delivery::FocusCause::WindowRequest, |state| {
                state.focus_window(Some(window));
            });
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

    fn allow_selection_access(&mut self, _: XwmId, _: SelectionTarget) -> bool {
        // One trusted desktop session; clipboard clients such as xclip need no window.
        true
    }

    fn send_selection(
        &mut self,
        _: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        self.serve_selection(selection, mime_type, fd);
    }

    fn new_selection(&mut self, _: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        self.x11_selection(selection, mime_types);
    }

    fn cleared_selection(&mut self, _: XwmId, selection: SelectionTarget) {
        self.x11_selection_cleared(selection);
    }

    fn disconnected(&mut self, _: XwmId) {
        self.xwayland.disconnected();
        self.fail(anyhow!("Xwayland WM disconnected"));
    }
}
