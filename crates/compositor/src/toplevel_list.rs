//! The ext-foreign-toplevel-list-v1 protocol.
//!
//! Every managed window publishes one foreign toplevel handle so standard tools
//! such as `lswt` can observe open windows with their title and app id. Wayland
//! toplevels are announced when they map and X11 windows when the XWM maps
//! them; override-redirect popups are not toplevels and never get a handle.
//! Unmapping or destroying a window closes its handle.

use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::IsAlive;
use smithay::wayland::compositor::with_states;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelListHandler;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelListState;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::xwayland::X11Surface;

use crate::State;
use crate::foreign_toplevel::ForeignToplevels;

pub(crate) struct ToplevelList {
    state: ForeignToplevelListState,
    handles: HashMap<Window, ForeignToplevelHandle>,
    foreign: ForeignToplevels,
}

impl ToplevelList {
    pub(crate) fn new(display_handle: &DisplayHandle) -> Self {
        Self {
            state: ForeignToplevelListState::new::<State>(display_handle),
            handles: HashMap::new(),
            foreign: ForeignToplevels::new(display_handle),
        }
    }

    pub(crate) fn state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.state
    }

    /// Announce the window once. Shell commits call this on every frame, so an
    /// already announced window sends nothing; metadata changes go through
    /// [`Self::update`].
    pub(crate) fn publish(&mut self, window: &Window) {
        if self.handles.contains_key(window) {
            return;
        }
        let Some((title, app_id)) = metadata(window) else {
            return;
        };
        let handle = self
            .state
            .new_toplevel::<State>(title.clone(), app_id.clone());
        self.handles.insert(window.clone(), handle);
        let states = crate::foreign_toplevel::states(
            window,
            crate::foreign_toplevel::activated(window),
            false,
        );
        self.foreign.publish(window, title, app_id, states);
    }

    /// Refresh an announced window's title and app id, sending `done` only
    /// when one of them changed.
    pub(crate) fn update(&mut self, window: &Window) {
        let Some((title, app_id)) = metadata(window) else {
            return;
        };
        if let Some(handle) = self.handles.get(window)
            && (handle.title() != title || handle.app_id() != app_id)
        {
            handle.send_title(&title);
            handle.send_app_id(&app_id);
            handle.send_done();
        }
        self.foreign.update_metadata(window, &title, &app_id);
    }

    /// Refresh the foreign-toplevel states of every window after a focus
    /// change. Commits do not call this.
    pub(crate) fn refresh_states(
        &mut self,
        focused: Option<&crate::focus::KeyboardFocus>,
        minimized: &[Window],
    ) {
        self.foreign.refresh(focused, minimized);
    }

    /// Close the window's handle; the entry is removed when the window unmaps
    /// or is destroyed.
    pub(crate) fn unpublish(&mut self, window: &Window) {
        if let Some(handle) = self.handles.remove(window) {
            self.state.remove_toplevel(&handle);
        }
        self.foreign.unpublish(window);
    }

    /// Close handles of windows that were destroyed without an unmap, such as
    /// a client disconnecting between frames.
    pub(crate) fn cleanup(&mut self) {
        let dead: Vec<Window> = self
            .handles
            .keys()
            .filter(|window| !window.alive())
            .cloned()
            .collect();
        for window in dead {
            self.unpublish(&window);
        }
    }

    pub(crate) fn manager_bound(&mut self, manager: &ZwlrForeignToplevelManagerV1) {
        self.foreign.manager_bound(manager);
    }

    pub(crate) fn manager_removed(&mut self, manager: &ZwlrForeignToplevelManagerV1) {
        self.foreign.manager_removed(manager);
    }

    pub(crate) fn handle_destroyed(&mut self, handle: &ZwlrForeignToplevelHandleV1) {
        self.foreign.handle_destroyed(handle);
    }

    pub(crate) fn holds_handle(
        &self,
        window: &Window,
        handle: &ZwlrForeignToplevelHandleV1,
    ) -> bool {
        self.foreign.holds_handle(window, handle)
    }

    pub(crate) fn accepts_request(&mut self, browser_holds_input: bool) -> bool {
        self.foreign.accepts_request(browser_holds_input)
    }

    pub(crate) fn lease_changed(&mut self) {
        self.foreign.lease_changed();
    }
}

/// The window's observable metadata, or `None` for windows that must not be
/// listed: override-redirect popups and surfaces without a toplevel role.
pub(super) fn metadata(window: &Window) -> Option<(String, String)> {
    if window
        .x11_surface()
        .is_some_and(X11Surface::is_override_redirect)
    {
        return None;
    }
    if let Some(top) = window.toplevel() {
        return with_states(top.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|data| data.lock().ok())
                .map(|data| {
                    (
                        data.title.clone().unwrap_or_default(),
                        data.app_id.clone().unwrap_or_default(),
                    )
                })
        });
    }
    window
        .x11_surface()
        .map(|surface| (surface.title(), surface.class()))
}

impl ForeignToplevelListHandler for State {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        self.toplevel_list.state()
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::io::OwnedFd;

    use smithay::backend::input::ButtonState;
    use smithay::desktop::Window;
    use smithay::input::keyboard::KeyboardSource;
    use smithay::wayland::seat::WaylandFocus;
    use wayland_client::Connection;
    use wayland_client::Dispatch;
    use wayland_client::EventQueue;
    use wayland_client::QueueHandle;
    use wayland_client::protocol::wl_compositor::WlCompositor;
    use wayland_client::protocol::wl_output;
    use wayland_client::protocol::wl_registry;
    use wayland_client::protocol::wl_seat::WlSeat;
    use wayland_client::protocol::wl_shm::WlShm;
    use wayland_client::protocol::wl_shm_pool::WlShmPool;
    use wayland_client::protocol::wl_surface::WlSurface;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1;
    use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
    use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::{
        self as confined,
    };
    use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
    use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::{
        self as locked,
    };
    use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1;
    use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
        self as constraints,
    };
    use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
    use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;
    use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::{
        self as relative,
    };
    use wayland_protocols::xdg::shell::client::xdg_popup::XdgPopup;
    use wayland_protocols::xdg::shell::client::xdg_popup::{
        self,
    };
    use wayland_protocols::xdg::shell::client::xdg_positioner::XdgPositioner;
    use wayland_protocols::xdg::shell::client::xdg_positioner::{
        self,
    };
    use wayland_protocols::xdg::shell::client::xdg_surface::XdgSurface;
    use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
    use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::State as WlrState;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
        self as wlr_handle,
    };
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
        self as wlr_manager,
    };

    use crate::focus::KeyboardFocus;

    /// A Wayland client that maps a minimal xdg toplevel and records the
    /// foreign toplevel events the compositor announces.
    #[derive(Default)]
    struct App {
        compositor: Option<WlCompositor>,
        shm: Option<WlShm>,
        wm_base: Option<XdgWmBase>,
        list: Option<ExtForeignToplevelListV1>,
        seat: Option<WlSeat>,
        manager: Option<ZwlrForeignToplevelManagerV1>,
        last_configure: Option<u32>,
        last_surface: Option<WlSurface>,
        last_xdg: Option<XdgSurface>,
        handles: Vec<ToplevelHandleEvents>,
        managed: Vec<ManagedToplevelEvents>,
        close_requests: usize,
        keys: Vec<u32>,
        pointer_focus: Option<WlSurface>,
        pointer_serial: Option<u32>,
        button_targets: Vec<WlSurface>,
        pointer_motions: usize,
        constraints: Option<ZwpPointerConstraintsV1>,
        relative_manager: Option<ZwpRelativePointerManagerV1>,
        locked: bool,
        confined: bool,
        relative: Vec<(f64, f64)>,
        preferred_scales: Vec<i32>,
        refresh_rates: Vec<i32>,
        popup_geometry: Option<(i32, i32, i32, i32)>,
    }

    #[derive(Default, Clone, PartialEq)]
    struct ToplevelHandleEvents {
        identifier: String,
        title: String,
        app_id: String,
        closed: bool,
        done: usize,
    }

    #[derive(Default)]
    struct ManagedToplevelEvents {
        handle: Option<ZwlrForeignToplevelHandleV1>,
        title: String,
        app_id: String,
        states: Vec<u32>,
        closed: bool,
        done: usize,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for App {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            (): &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                match interface.as_str() {
                    "wl_compositor" => {
                        // Bind the advertised version so wl_surface supports
                        // damage_buffer (since version 4).
                        state.compositor = Some(registry.bind(name, version, qh, ()));
                    }
                    "wl_shm" => {
                        state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                    }
                    "wl_output" => {
                        let _: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    }
                    "xdg_wm_base" => {
                        state.wm_base = Some(registry.bind(name, version.min(1), qh, ()));
                    }
                    "ext_foreign_toplevel_list_v1" => {
                        state.list = Some(registry.bind(name, version.min(1), qh, ()));
                    }
                    "wl_seat" => {
                        state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                    }
                    "zwp_pointer_constraints_v1" => {
                        state.constraints = Some(registry.bind(name, 1, qh, ()));
                    }
                    "zwp_relative_pointer_manager_v1" => {
                        state.relative_manager = Some(registry.bind(name, 1, qh, ()));
                    }
                    "zwlr_foreign_toplevel_manager_v1" => {
                        state.manager = Some(registry.bind(name, version.min(3), qh, ()));
                    }
                    _ => {}
                }
            }
        }
    }

    impl Dispatch<WlCompositor, ()> for App {
        fn event(
            _: &mut Self,
            _: &WlCompositor,
            _: wayland_client::protocol::wl_compositor::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_output::WlOutput, ()> for App {
        fn event(
            state: &mut Self,
            _: &wl_output::WlOutput,
            event: wl_output::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_output::Event::Mode { refresh, .. } = event {
                state.refresh_rates.push(refresh);
            }
        }
    }

    impl Dispatch<WlSurface, ()> for App {
        fn event(
            state: &mut Self,
            _: &WlSurface,
            event: wayland_client::protocol::wl_surface::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wayland_client::protocol::wl_surface::Event::PreferredBufferScale { factor } =
                event
            {
                state.preferred_scales.push(factor);
            }
        }
    }

    impl Dispatch<WlShm, ()> for App {
        fn event(
            _: &mut Self,
            _: &WlShm,
            _: wayland_client::protocol::wl_shm::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<WlShmPool, ()> for App {
        fn event(
            _: &mut Self,
            _: &WlShmPool,
            _: wayland_client::protocol::wl_shm_pool::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wayland_client::protocol::wl_buffer::WlBuffer, ()> for App {
        fn event(
            _: &mut Self,
            _: &wayland_client::protocol::wl_buffer::WlBuffer,
            _: wayland_client::protocol::wl_buffer::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    wayland_client::delegate_noop!(App: ignore WlSeat);
    wayland_client::delegate_noop!(App: ignore XdgPositioner);
    wayland_client::delegate_noop!(App: ignore ZwpPointerConstraintsV1);
    wayland_client::delegate_noop!(App: ignore ZwpRelativePointerManagerV1);
    wayland_client::delegate_noop!(App: ignore wayland_client::protocol::wl_region::WlRegion);

    impl Dispatch<XdgPopup, ()> for App {
        fn event(
            state: &mut Self,
            _: &XdgPopup,
            event: xdg_popup::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } = event
            {
                state.popup_geometry = Some((x, y, width, height));
            }
        }
    }

    impl Dispatch<ZwpLockedPointerV1, ()> for App {
        fn event(
            state: &mut Self,
            _: &ZwpLockedPointerV1,
            event: locked::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                locked::Event::Locked => state.locked = true,
                locked::Event::Unlocked => state.locked = false,
                _ => {}
            }
        }
    }
    impl Dispatch<ZwpConfinedPointerV1, ()> for App {
        fn event(
            state: &mut Self,
            _: &ZwpConfinedPointerV1,
            event: confined::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                confined::Event::Confined => state.confined = true,
                confined::Event::Unconfined => state.confined = false,
                _ => {}
            }
        }
    }
    impl Dispatch<ZwpRelativePointerV1, ()> for App {
        fn event(
            state: &mut Self,
            _: &ZwpRelativePointerV1,
            event: relative::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let relative::Event::RelativeMotion { dx, dy, .. } = event {
                state.relative.push((dx, dy));
            }
        }
    }

    impl Dispatch<wayland_client::protocol::wl_keyboard::WlKeyboard, ()> for App {
        fn event(
            state: &mut Self,
            _: &wayland_client::protocol::wl_keyboard::WlKeyboard,
            event: wayland_client::protocol::wl_keyboard::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wayland_client::protocol::wl_keyboard::Event::Key { key, .. } = event {
                state.keys.push(key);
            }
        }
    }

    impl Dispatch<wayland_client::protocol::wl_pointer::WlPointer, ()> for App {
        fn event(
            state: &mut Self,
            _: &wayland_client::protocol::wl_pointer::WlPointer,
            event: wayland_client::protocol::wl_pointer::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                wayland_client::protocol::wl_pointer::Event::Enter {
                    surface, serial, ..
                } => {
                    state.pointer_focus = Some(surface);
                    state.pointer_serial = Some(serial);
                }
                wayland_client::protocol::wl_pointer::Event::Leave { .. } => {
                    state.pointer_focus = None;
                }
                wayland_client::protocol::wl_pointer::Event::Button { .. } => {
                    if let Some(surface) = &state.pointer_focus {
                        state.button_targets.push(surface.clone());
                    }
                }
                wayland_client::protocol::wl_pointer::Event::Motion { .. } => {
                    state.pointer_motions += 1;
                }
                wayland_client::protocol::wl_pointer::Event::Axis { .. }
                | wayland_client::protocol::wl_pointer::Event::Frame
                | wayland_client::protocol::wl_pointer::Event::AxisSource { .. }
                | wayland_client::protocol::wl_pointer::Event::AxisStop { .. }
                | wayland_client::protocol::wl_pointer::Event::AxisDiscrete { .. }
                | wayland_client::protocol::wl_pointer::Event::AxisValue120 { .. }
                | wayland_client::protocol::wl_pointer::Event::AxisRelativeDirection { .. }
                | wayland_client::protocol::wl_pointer::Event::Warp { .. }
                | _ => {}
            }
        }
    }

    impl Dispatch<XdgWmBase, ()> for App {
        fn event(
            _: &mut Self,
            _: &XdgWmBase,
            _: wayland_protocols::xdg::shell::client::xdg_wm_base::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<XdgSurface, ()> for App {
        fn event(
            state: &mut Self,
            _: &XdgSurface,
            event: wayland_protocols::xdg::shell::client::xdg_surface::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wayland_protocols::xdg::shell::client::xdg_surface::Event::Configure { serial } =
                event
            {
                state.last_configure = Some(serial);
            }
        }
    }

    impl Dispatch<XdgToplevel, ()> for App {
        fn event(
            state: &mut Self,
            _: &XdgToplevel,
            event: wayland_protocols::xdg::shell::client::xdg_toplevel::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wayland_protocols::xdg::shell::client::xdg_toplevel::Event::Close = event {
                state.close_requests += 1;
            }
        }
    }

    impl Dispatch<ExtForeignToplevelListV1, ()> for App {
        fn event(
            state: &mut Self,
            _: &ExtForeignToplevelListV1,
            event: wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::Event::Toplevel { .. } =
                event
            {
                state.handles.push(ToplevelHandleEvents::default());
            }
        }

        wayland_client::event_created_child!(App, ExtForeignToplevelListV1, [
            wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE =>
                (ExtForeignToplevelHandleV1, ()),
        ]);
    }

    impl Dispatch<ExtForeignToplevelHandleV1, ()> for App {
        fn event(
            state: &mut Self,
            _: &ExtForeignToplevelHandleV1,
            event: wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let events = state.handles.last_mut().expect("handle announced first");
            match event {
                wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                    events.identifier = identifier;
                }
                wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                    events.title = title;
                }
                wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                    events.app_id = app_id;
                }
                wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event::Closed => {
                    events.closed = true;
                }
                wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event::Done => {
                    events.done += 1;
                }
                _ => {}
            }
        }
    }

    impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for App {
        fn event(
            state: &mut Self,
            _: &ZwlrForeignToplevelManagerV1,
            event: wlr_manager::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wlr_manager::Event::Toplevel { toplevel } = event {
                state.managed.push(ManagedToplevelEvents {
                    handle: Some(toplevel),
                    ..Default::default()
                });
            }
        }

        wayland_client::event_created_child!(App, ZwlrForeignToplevelManagerV1, [
            wlr_manager::EVT_TOPLEVEL_OPCODE =>
                (ZwlrForeignToplevelHandleV1, ()),
        ]);
    }

    impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for App {
        fn event(
            state: &mut Self,
            handle: &ZwlrForeignToplevelHandleV1,
            event: wlr_handle::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let Some(events) = state
                .managed
                .iter_mut()
                .find(|events| events.handle.as_ref() == Some(handle))
            else {
                return;
            };
            match event {
                wlr_handle::Event::Title { title } => events.title = title,
                wlr_handle::Event::AppId { app_id } => events.app_id = app_id,
                wlr_handle::Event::State { state } => {
                    events.states = state
                        .chunks_exact(4)
                        .map(|entry| u32::from_ne_bytes([entry[0], entry[1], entry[2], entry[3]]))
                        .collect();
                }
                wlr_handle::Event::Done => events.done += 1,
                wlr_handle::Event::Closed => events.closed = true,
                wlr_handle::Event::OutputEnter { .. }
                | wlr_handle::Event::OutputLeave { .. }
                | wlr_handle::Event::Parent { .. }
                | _ => {}
            }
        }
    }

    /// Process requests and events on both sides once.
    fn sync(
        display: &mut smithay::reexports::wayland_server::Display<crate::State>,
        state: &mut crate::State,
        connection: &Connection,
        queue: &mut EventQueue<App>,
        app: &mut App,
    ) {
        // Flush requests queued by the client during the previous sync, then
        // let the server dispatch them.
        connection.flush().expect("client flush");
        display.dispatch_clients(state).expect("server dispatch");
        display.handle().flush_clients().expect("server flush");
        // The client socket is nonblocking; poll the read guard first so an
        // idle server is not an error.
        if let Some(guard) = connection.prepare_read() {
            let mut poll = [nix::poll::PollFd::new(
                guard.connection_fd(),
                nix::poll::PollFlags::POLLIN,
            )];
            let ready = nix::poll::poll(&mut poll, nix::poll::PollTimeout::from(0u8)).unwrap_or(-1);
            if ready > 0 {
                guard.read().expect("client read");
            }
        }
        queue.dispatch_pending(app).expect("client dispatch");
        // Requests queued by this dispatch reach the server at the next sync.
        connection.flush().expect("client flush");
    }

    fn connect_client(
        display: &mut smithay::reexports::wayland_server::DisplayHandle,
    ) -> (Connection, EventQueue<App>, App) {
        let (client_socket, server_socket) = nix::sys::socket::socketpair(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            None,
            nix::sys::socket::SockFlag::empty(),
        )
        .expect("socketpair");
        let server_socket: std::os::unix::net::UnixStream = server_socket.into();
        display
            .insert_client(
                server_socket,
                std::sync::Arc::new(crate::shell::ClientState::default()),
            )
            .expect("insert client");
        let client_socket: std::os::unix::net::UnixStream = client_socket.into();
        let connection = Connection::from_socket(client_socket).expect("client connection");
        let queue: EventQueue<App> = connection.new_event_queue();
        let app = App::default();
        connection.display().get_registry(&queue.handle(), ());
        (connection, queue, app)
    }

    fn attach_buffer(surface: &WlSurface, app: &App, qh: &QueueHandle<App>, size: i32) {
        let shm = app.shm.as_ref().expect("wl_shm bound");
        let memfd = nix::sys::memfd::memfd_create(
            "toplevel-test-buffer",
            nix::sys::memfd::MFdFlags::empty(),
        )
        .expect("memfd");
        let fd: OwnedFd = memfd;
        nix::unistd::ftruncate(&fd, 4 * i64::from(size)).expect("ftruncate");
        let pool = shm.create_pool(fd.as_fd(), 4 * size, qh, ());
        let buffer = pool.create_buffer(
            0,
            size,
            1,
            4 * size,
            wayland_client::protocol::wl_shm::Format::Argb8888,
            qh,
            (),
        );
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, size, 1);
    }

    /// Map an xdg toplevel with the given metadata and return its objects.
    fn map_toplevel(
        display: &mut smithay::reexports::wayland_server::Display<crate::State>,
        state: &mut crate::State,
        connection: &Connection,
        queue: &mut EventQueue<App>,
        app: &mut App,
        title: &str,
        app_id: &str,
    ) -> (WlSurface, XdgSurface, XdgToplevel) {
        let qh = queue.handle();
        let compositor = app.compositor.as_ref().expect("wl_compositor bound");
        let wm_base = app.wm_base.as_ref().expect("xdg_wm_base bound");
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title(title.to_owned());
        toplevel.set_app_id(app_id.to_owned());
        // An empty commit requests the initial configure.
        surface.commit();
        sync(display, state, connection, queue, app);
        let serial = app.last_configure.expect("initial configure received");
        xdg_surface.ack_configure(serial);
        // The initial configure must be acknowledged before the first buffer.
        attach_buffer(&surface, app, &qh, 1);
        surface.commit();
        sync(display, state, connection, queue, app);
        app.last_surface = Some(surface.clone());
        app.last_xdg = Some(xdg_surface.clone());
        (surface, xdg_surface, toplevel)
    }

    fn commit_configure(
        display: &mut smithay::reexports::wayland_server::Display<crate::State>,
        state: &mut crate::State,
        connection: &Connection,
        queue: &mut EventQueue<App>,
        app: &mut App,
    ) {
        sync(display, state, connection, queue, app);
        app.last_xdg
            .as_ref()
            .expect("xdg surface")
            .ack_configure(app.last_configure.expect("configure"));
        app.last_surface.as_ref().expect("surface").commit();
        sync(display, state, connection, queue, app);
    }

    fn commit_server_decoration(
        display: &mut smithay::reexports::wayland_server::Display<crate::State>,
        state: &mut crate::State,
        connection: &Connection,
        queue: &mut EventQueue<App>,
        app: &mut App,
        window: &Window,
    ) {
        let top = window.toplevel().expect("toplevel");
        top.with_pending_state(|pending| pending.decoration_mode = Some(smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode::ServerSide));
        top.send_configure();
        commit_configure(display, state, connection, queue, app);
    }

    fn top_window(state: &crate::State) -> Window {
        state
            .space
            .elements()
            .last()
            .cloned()
            .expect("mapped window")
    }

    #[test]
    fn uncommitted_toplevel_receives_no_configure() {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as TopState;
        use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

        #[derive(Debug)]
        enum Path {
            Decoration,
            Focus,
            Resize,
            Expand,
        }
        let mut premature = Vec::new();
        for path in [Path::Decoration, Path::Focus, Path::Resize, Path::Expand] {
            let (mut display, mut state) = crate::tests::test_state();
            let (connection, mut queue, mut app) = connect_client(&mut display.handle());
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            let qh = queue.handle();
            let surface = app
                .compositor
                .as_ref()
                .expect("compositor")
                .create_surface(&qh, ());
            let xdg = app
                .wm_base
                .as_ref()
                .expect("xdg shell")
                .get_xdg_surface(&surface, &qh, ());
            let _top = xdg.get_toplevel(&qh, ());
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            let window = top_window(&state);
            let top = window.toplevel().expect("toplevel").clone();
            match path {
                Path::Decoration => state.request_mode(top, Mode::ServerSide),
                Path::Focus => state.focus_window(Some(window)),
                Path::Resize => state.output_resized(),
                Path::Expand => state.set_expanded(&top, TopState::Maximized, true),
            }
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            if app.last_configure.is_some() {
                premature.push(path);
            }
        }
        assert!(
            premature.is_empty(),
            "premature configure from {premature:?}"
        );
    }

    #[test]
    fn remapped_toplevel_retains_decoration_and_bounds() {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (surface, xdg, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Window",
            "window",
        );
        let window = top_window(&state);
        let top = window.toplevel().expect("toplevel").clone();
        state.request_mode(top.clone(), Mode::ServerSide);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        xdg.ack_configure(app.last_configure.expect("decoration configure"));
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        app.last_configure = None;
        surface.attach(None, 0, 0);
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(app.last_configure.is_none(), "no configure on unmap");
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        top.with_pending_state(|pending| {
            assert_eq!(pending.decoration_mode, Some(Mode::ServerSide));
            assert_eq!(pending.bounds, Some((1920, 1048).into()));
        });
        xdg.ack_configure(app.last_configure.expect("remap configure"));
        attach_buffer(&surface, &app, &queue.handle(), 1);
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(state.windows.get(&window).expect("window state").mapped);
    }

    #[test]
    fn repeated_maximize_still_sends_configure() {
        let (mut display, mut state, connection, mut queue, mut app, window) = action_fixture();
        let top = window.toplevel().expect("toplevel").clone();
        state.set_expanded(&top, smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized, true);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let first = app.last_configure.expect("first maximize configure");
        state.set_expanded(&top, smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized, true);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_ne!(
            app.last_configure,
            Some(first),
            "repeated maximize configure"
        );
    }

    #[test]
    fn destroying_a_window_ends_its_move_and_resize_grabs() {
        for resize in [false, true] {
            let (mut display, mut state) = crate::tests::test_state();
            let (connection, mut queue, mut app) = connect_client(&mut display.handle());
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            let (_, _, toplevel) = map_toplevel(
                &mut display,
                &mut state,
                &connection,
                &mut queue,
                &mut app,
                "Window",
                "window",
            );
            let window = top_window(&state);
            let (_, _, other) = map_toplevel(
                &mut display,
                &mut state,
                &connection,
                &mut queue,
                &mut app,
                "Other",
                "other",
            );
            let pointer = state.seat.get_pointer().expect("pointer");
            state.start_decoration_grab(
                window,
                smithay::utils::SERIAL_COUNTER.next_serial(),
                resize,
            );
            assert!(pointer.is_grabbed());
            other.destroy();
            connection.flush().expect("client flush");
            display
                .dispatch_clients(&mut state)
                .expect("server dispatch");
            assert!(pointer.is_grabbed(), "another window must not end the grab");
            toplevel.destroy();
            connection.flush().expect("client flush");
            display
                .dispatch_clients(&mut state)
                .expect("server dispatch");
            assert!(!pointer.is_grabbed(), "grab survives window destruction");
        }
    }

    #[test]
    fn window_chrome_follows_client_committed_state() {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as TopState;
        use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (surface, xdg, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Window",
            "window",
        );
        let window = top_window(&state);
        let top = window.toplevel().expect("toplevel").clone();
        state.request_mode(top.clone(), Mode::ServerSide);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        xdg.ack_configure(app.last_configure.expect("decoration configure"));
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(crate::decorations::decorated(&window));
        top.with_pending_state(|pending| {
            pending.states.set(TopState::Maximized);
            pending.states.set(TopState::Fullscreen);
        });
        top.send_configure();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(
            crate::decorations::decorated(&window),
            "pending fullscreen must not hide chrome"
        );
        assert!(
            !crate::decorations::window_flags(&window).1,
            "pending maximize must not change chrome"
        );
        assert!(
            !crate::decorations::above_taskbar(&window),
            "pending fullscreen must not change stacking"
        );
        xdg.ack_configure(app.last_configure.expect("fullscreen configure"));
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(!crate::decorations::decorated(&window));
        assert!(crate::decorations::window_flags(&window).1);
        assert!(crate::decorations::above_taskbar(&window));
    }

    fn action_fixture() -> (
        smithay::reexports::wayland_server::Display<crate::State>,
        crate::State,
        Connection,
        EventQueue<App>,
        App,
        Window,
    ) {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Window",
            "window",
        );
        let window = top_window(&state);
        (display, state, connection, queue, app, window)
    }

    #[test]
    fn taskbar_chips_follow_mapping_title_focus_and_minimization() {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (first_surface, _, first_top) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "First",
            "first",
        );
        let first = top_window(&state);
        first
            .toplevel()
            .expect("top")
            .with_pending_state(|pending| pending.decoration_mode = Some(Mode::ServerSide));
        let (_, _, second_top) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Second",
            "second",
        );
        let second = top_window(&state);
        second
            .toplevel()
            .expect("top")
            .with_pending_state(|pending| pending.decoration_mode = Some(Mode::ServerSide));
        assert_eq!(
            state
                .taskbar_chips()
                .iter()
                .map(|chip| chip.title.as_str())
                .collect::<Vec<_>>(),
            ["First", "Second"]
        );
        assert_eq!(
            state
                .taskbar_chips()
                .iter()
                .map(|chip| chip.focused)
                .collect::<Vec<_>>(),
            [false, true]
        );
        state.window_action(&first, crate::WindowAction::Focus);
        assert_eq!(
            state
                .taskbar_chips()
                .iter()
                .map(|chip| chip.focused)
                .collect::<Vec<_>>(),
            [true, false]
        );
        state.window_action(&first, crate::WindowAction::Minimize);
        assert!(state.taskbar_chips()[0].minimized);
        let top = second.toplevel().expect("top").clone();
        state.set_expanded(&top, smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen, true);
        assert_eq!(
            state.taskbar_chips().len(),
            2,
            "fullscreen retains its chip"
        );
        state.set_expanded(&top, smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen, false);
        second_top.set_title("Renamed".into());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(state.taskbar_chips()[1].title, "Renamed");
        second_top.set_parent(Some(&first_top));
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(state.taskbar_chips().len(), 1);
        second_top.set_parent(None);
        first_surface.attach(None, 0, 0);
        first_surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let chips = state.taskbar_chips();
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].window, Some(second));
        assert_eq!(chips[0].title, "Renamed");
    }

    #[test]
    fn chip_click_focuses_unfocused_window() {
        let (mut display, mut state, connection, mut queue, mut app, first) = action_fixture();
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Second",
            "second",
        );
        state.pointer_motion(KeyboardSource::MAIN, (40.0, 1060.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        assert_eq!(
            state.seat.get_keyboard().expect("keyboard").current_focus(),
            KeyboardFocus::window(&first)
        );
    }

    #[test]
    fn pinned_chips_prioritize_pid_then_app_id_without_duplicates() {
        let (mut display, mut state, connection, mut queue, mut app, first) = action_fixture();
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Beta",
            "beta",
        );
        let second = top_window(&state);
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Other",
            "other",
        );
        let third = top_window(&state);
        state.pins = vec![
            crate::Pin {
                command: vec!["/usr/bin/beta".into()],
                pid: None,
                child: None,
            },
            crate::Pin {
                command: vec!["different-name".into()],
                pid: state.window_pid(&first),
                child: None,
            },
            crate::Pin {
                command: vec!["absent".into()],
                pid: None,
                child: None,
            },
        ];
        let chips = state.taskbar_chips();
        assert_eq!(chips.len(), 4);
        assert_eq!(chips[0].pin, Some(0));
        assert_eq!(chips[0].window, Some(second));
        assert_eq!(chips[1].pin, Some(1));
        assert_eq!(chips[1].window, Some(first));
        assert_eq!(chips[2].pin, Some(2));
        assert_eq!(chips[2].window, None);
        assert_eq!(chips[3].pin, None);
        assert_eq!(chips[3].window, Some(third));
    }

    #[test]
    fn chip_click_minimizes_focused_window() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.pointer_motion(KeyboardSource::MAIN, (40.0, 1060.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        assert!(state.windows.get(&window).expect("window").minimized);
    }

    #[test]
    fn clicking_unmatched_pin_spawns_and_reaps_process() {
        let (_, mut state, _, _, _, _) = action_fixture();
        state.pins.push(crate::Pin {
            command: vec!["sleep".into(), "5".into()],
            pid: None,
            child: None,
        });
        state.pointer_motion(KeyboardSource::MAIN, (40.0, 1060.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        let pin = &mut state.pins[0];
        let child = pin.child.as_mut().expect("spawned process");
        assert_eq!(pin.pid, Some(child.id()));
        child.kill().expect("stop child");
        child.wait().expect("wait child");
        state.reap_pins();
        assert!(state.pins[0].pid.is_none());
        assert!(!state.taskbar_chips()[0].running);
    }

    #[test]
    fn dropping_state_reaps_pinned_child() {
        let (_, mut state, _, _, _, _) = action_fixture();
        state.pins.push(crate::Pin {
            command: vec!["sleep".into(), "30".into()],
            pid: None,
            child: None,
        });
        state.launch_pin(0);
        let pid = nix::unistd::Pid::from_raw(
            i32::try_from(state.pins[0].pid.expect("spawned process")).expect("valid pid"),
        );
        drop(state);
        let result = nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
        if matches!(result, Ok(nix::sys::wait::WaitStatus::StillAlive)) {
            nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).expect("kill child");
            nix::sys::wait::waitpid(pid, None).expect("reap child");
        }
        assert!(
            matches!(result, Err(nix::errno::Errno::ECHILD)),
            "child was not reaped: {result:?}"
        );
    }

    #[test]
    fn clicking_running_pin_minimizes_its_window() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.pins.push(crate::Pin {
            command: vec!["window".into()],
            pid: None,
            child: None,
        });
        assert_eq!(state.taskbar_chips()[0].window, Some(window.clone()));
        state.pointer_motion(KeyboardSource::MAIN, (40.0, 1060.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        assert!(state.windows.get(&window).expect("window").minimized);
    }

    #[test]
    fn chip_click_restores_minimized_window() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.window_action(&window, crate::WindowAction::Minimize);
        state.pointer_motion(KeyboardSource::MAIN, (40.0, 1060.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        assert!(!state.windows.get(&window).expect("window").minimized);
        assert_eq!(
            state.seat.get_keyboard().expect("keyboard").current_focus(),
            KeyboardFocus::window(&window)
        );
    }

    #[test]
    fn action_focus_focuses_window() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.focus_window(None);
        state.window_action(&window, crate::WindowAction::Focus);
        assert_eq!(
            state.seat.get_keyboard().expect("keyboard").current_focus(),
            KeyboardFocus::window(&window)
        );
    }

    #[test]
    fn minimize_keeps_window_mapped_and_moves_focus() {
        let (mut display, mut state, connection, mut queue, mut app, first) = action_fixture();
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Other",
            "other",
        );
        let second = top_window(&state);
        state.window_action(&second, crate::WindowAction::Minimize);
        assert!(state.windows.get(&second).expect("window").minimized);
        assert!(state.space.element_location(&second).is_some());
        assert_eq!(state.topmost_focusable(None), Some(first.clone()));
        assert_eq!(
            state.seat.get_keyboard().expect("keyboard").current_focus(),
            KeyboardFocus::window(&first)
        );
        state.focus_window(Some(second.clone()));
        assert_eq!(
            state.seat.get_keyboard().expect("keyboard").current_focus(),
            KeyboardFocus::window(&first)
        );
    }

    #[test]
    fn restore_shortcut_recovers_most_recent_minimized_window() {
        use smithay::backend::input::KeyState;
        let (mut display, mut state, connection, mut queue, mut app, first) = action_fixture();
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Other",
            "other",
        );
        let second = top_window(&state);
        state.window_action(&first, crate::WindowAction::Minimize);
        state.window_action(&second, crate::WindowAction::Minimize);
        for key in [125, 42, 50] {
            state.keyboard_key(KeyboardSource::MAIN, key, KeyState::Pressed);
        }
        assert!(!state.windows.get(&second).expect("window").minimized);
        assert!(state.windows.get(&first).expect("window").minimized);
        assert_eq!(
            state.seat.get_keyboard().expect("keyboard").current_focus(),
            KeyboardFocus::window(&second)
        );
        for key in [50, 42, 125] {
            state.keyboard_key(KeyboardSource::MAIN, key, KeyState::Released);
        }
    }

    #[test]
    fn action_move_starts_a_grab() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.window_action(&window, crate::WindowAction::Move);
        assert!(state.seat.get_pointer().expect("pointer").is_grabbed());
    }

    #[test]
    fn moving_window_keeps_titlebar_above_taskbar() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.pointer_motion(KeyboardSource::MAIN, (100.0, 40.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.window_action(&window, crate::WindowAction::Move);
        state.pointer_motion(KeyboardSource::MAIN, (100.0, 1079.0).into());
        assert_eq!(
            state.space.element_location(&window).expect("location").y,
            1048
        );
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
    }

    #[test]
    fn action_resize_starts_the_requested_edge_grab() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.window_action(&window, crate::WindowAction::Resize(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge::TopLeft));
        assert!(state.seat.get_pointer().expect("pointer").is_grabbed());
        assert_eq!(
            state
                .windows
                .get(&window)
                .expect("window")
                .resize
                .as_ref()
                .map(|resize| resize.0),
            Some(5)
        );
    }

    #[test]
    fn action_maximize_sets_maximized_state() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.window_action(&window, crate::WindowAction::Maximize);
        assert!(window.toplevel().expect("toplevel").with_pending_state(|state| state.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)));
    }

    #[test]
    fn action_restore_clears_maximized_state() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.window_action(&window, crate::WindowAction::Maximize);
        state.window_action(&window, crate::WindowAction::Restore);
        assert!(!window.toplevel().expect("toplevel").with_pending_state(|state| state.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)));
    }

    #[test]
    fn maximized_decorated_window_fills_output_including_titlebar() {
        let (_display, mut state, _connection, _queue, _app, window) = action_fixture();
        let top = window.toplevel().expect("toplevel");
        top.with_pending_state(|pending| {
            pending.decoration_mode = Some(smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode::ServerSide);
        });
        state.window_action(&window, crate::WindowAction::Maximize);
        let output = state.space.output_geometry(&state.output).expect("output");
        let location = state.space.element_location(&window).expect("location");
        let content = top.with_pending_state(|pending| pending.size.expect("maximized size"));
        assert_eq!(location.y - crate::decorations::BAR, output.loc.y);
        assert_eq!(
            content.h,
            output.size.h - crate::decorations::BAR - crate::decorations::TASKBAR
        );
        assert_eq!(content.w, output.size.w);
    }

    #[test]
    fn work_area_follows_output_size_and_scale() {
        let (_, mut state) = crate::tests::test_state();
        assert_eq!(
            state.work_area().expect("work area"),
            smithay::utils::Rectangle::new((0, 0).into(), (1920, 1048).into())
        );
        state.output.change_current_state(
            Some(smithay::output::Mode {
                size: (800, 600).into(),
                refresh: 60_000,
            }),
            None,
            Some(smithay::output::Scale::Integer(1)),
            None,
        );
        state.output_resized();
        assert_eq!(
            state.work_area().expect("work area"),
            smithay::utils::Rectangle::new((0, 0).into(), (800, 568).into())
        );
        state.output.change_current_state(
            None,
            None,
            Some(smithay::output::Scale::Integer(2)),
            None,
        );
        state.output_resized();
        assert_eq!(
            state.work_area().expect("work area"),
            smithay::utils::Rectangle::new((0, 0).into(), (400, 268).into())
        );
    }

    #[test]
    fn placement_and_output_resize_constrain_window_to_work_area() {
        let (_, mut state, _, _, _, window) = action_fixture();
        assert_eq!(state.space.element_location(&window), Some((32, 32).into()));
        state.space.map_element(window.clone(), (1900, 1070), false);
        state.output_resized();
        assert_eq!(
            state.space.element_location(&window),
            Some((1900, 1047).into())
        );
    }

    #[test]
    fn popup_is_constrained_above_taskbar() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (_, parent, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "parent",
            "parent",
        );
        let window = top_window(&state);
        state.space.map_element(window, (100, 1000), true);
        let qh = queue.handle();
        let positioner = app
            .wm_base
            .as_ref()
            .expect("wm base")
            .create_positioner(&qh, ());
        positioner.set_size(100, 100);
        positioner.set_anchor_rect(0, 40, 1, 1);
        positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideY);
        let surface = app
            .compositor
            .as_ref()
            .expect("compositor")
            .create_surface(&qh, ());
        let xdg = app
            .wm_base
            .as_ref()
            .expect("wm base")
            .get_xdg_surface(&surface, &qh, ());
        let _popup = xdg.get_popup(Some(&parent), &positioner, &qh, ());
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (_, y, _, height) = app.popup_geometry.expect("popup configure");
        assert_eq!(1000 + y + height, 1048);
    }

    #[test]
    fn fullscreen_and_menu_render_above_taskbar() {
        use smithay::backend::renderer::pixman::PixmanRenderer;
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

        use crate::decorations::SceneElement;

        let (mut display, mut state, connection, mut queue, mut app, window) = action_fixture();
        let mut renderer = PixmanRenderer::new().expect("renderer");
        let normal = state.scene_elements(&mut renderer);
        assert!(matches!(normal.first(), Some(SceneElement::Decoration(_))));
        assert!(
            normal
                .iter()
                .skip(1)
                .any(|element| matches!(element, SceneElement::Surface(_)))
        );

        state.window_action(&window, crate::WindowAction::Maximize);
        let maximized = state.scene_elements(&mut renderer);
        assert!(matches!(
            maximized.first(),
            Some(SceneElement::Decoration(_))
        ));
        assert!(
            maximized
                .iter()
                .skip(1)
                .any(|element| matches!(element, SceneElement::Surface(_)))
        );

        let top = window.toplevel().expect("toplevel").clone();
        state.set_expanded(&top, xdg_toplevel::State::Fullscreen, true);
        commit_configure(&mut display, &mut state, &connection, &mut queue, &mut app);
        let fullscreen = state.scene_elements(&mut renderer);
        assert!(matches!(fullscreen.first(), Some(SceneElement::Surface(_))));
        assert!(
            fullscreen
                .iter()
                .skip(1)
                .any(|element| matches!(element, SceneElement::Decoration(_)))
        );

        state.pointer_motion(KeyboardSource::MAIN, (100.0, 100.0).into());
        state.window_action(&window, crate::WindowAction::ShowMenu);
        let menu = state.scene_elements(&mut renderer);
        assert!(matches!(menu.first(), Some(SceneElement::Decoration(_))));
        assert!(matches!(menu.get(1), Some(SceneElement::Surface(_))));
        assert!(
            menu.iter()
                .skip(2)
                .any(|element| matches!(element, SceneElement::Decoration(_)))
        );
    }

    #[test]
    fn normal_window_above_fullscreen_stays_above_it_in_scene() {
        use smithay::backend::renderer::pixman::PixmanRenderer;
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

        use crate::decorations::SceneElement;

        let (mut display, mut state, connection, mut queue, mut app, fullscreen) = action_fixture();
        let top = fullscreen.toplevel().expect("toplevel").clone();
        state.set_expanded(&top, xdg_toplevel::State::Fullscreen, true);
        let _normal = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "normal",
            "normal",
        );
        let mut renderer = PixmanRenderer::new().expect("renderer");
        let scene = state.scene_elements(&mut renderer);
        assert!(
            matches!(scene.first(), Some(SceneElement::Decoration(_))),
            "the taskbar precedes the normal and fullscreen windows"
        );
    }

    #[test]
    fn empty_taskbar_click_keeps_focus_and_does_not_reach_window() {
        let (mut display, mut state, connection, mut queue, mut app, window) = action_fixture();
        let _pointer = app
            .seat
            .as_ref()
            .expect("seat")
            .get_pointer(&queue.handle(), ());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        state.space.map_element(window.clone(), (300, 1050), true);
        let keyboard = state.seat.get_keyboard().expect("keyboard");
        let focus = keyboard.current_focus().expect("focused window");
        state.pointer_motion(KeyboardSource::MAIN, (300.0, 1050.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(keyboard.current_focus(), Some(focus));
        assert!(app.button_targets.is_empty());
    }

    #[test]
    fn titlebar_double_click_toggles_maximize_and_restore() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (surface, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Window",
            "window",
        );
        attach_buffer(&surface, &app, &queue.handle(), 400);
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let window = top_window(&state);
        commit_server_decoration(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            &window,
        );
        state.space.map_element(window.clone(), (100, 100), true);
        state.pointer_motion(KeyboardSource::MAIN, (120.0, 80.0).into());
        for _ in 0..2 {
            state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
            state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        }
        assert!(window.toplevel().expect("top").with_pending_state(|state| state.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)));
        commit_configure(&mut display, &mut state, &connection, &mut queue, &mut app);
        state.pointer_motion(KeyboardSource::MAIN, (20.0, 10.0).into());
        for _ in 0..2 {
            state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
            state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        }
        assert!(!window.toplevel().expect("top").with_pending_state(|state| state.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)));
    }

    #[test]
    fn dragging_maximized_titlebar_restores_at_relative_pointer_x() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (surface, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Window",
            "window",
        );
        attach_buffer(&surface, &app, &queue.handle(), 400);
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let window = top_window(&state);
        commit_server_decoration(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            &window,
        );
        state.space.map_element(window.clone(), (100, 100), true);
        state.window_action(&window, crate::WindowAction::Maximize);
        commit_configure(&mut display, &mut state, &connection, &mut queue, &mut app);
        attach_buffer(&surface, &app, &queue.handle(), 1920);
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        state.pointer_motion(KeyboardSource::MAIN, (480.0, 10.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.pointer_motion(KeyboardSource::MAIN, (490.0, 20.0).into());
        assert!(!window.toplevel().expect("top").with_pending_state(|state| state.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)));
        assert_eq!(
            state.space.element_location(&window).expect("location"),
            (390, 38).into()
        );
        assert!(state.seat.get_pointer().expect("pointer").is_grabbed());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
    }

    #[test]
    fn action_close_notifies_client() {
        let (mut display, mut state, connection, mut queue, mut app, window) = action_fixture();
        state.window_action(&window, crate::WindowAction::Close);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.close_requests, 1);
    }

    #[test]
    fn action_show_menu_opens_and_dismisses() {
        let (_, mut state, _, _, _, window) = action_fixture();
        let before = state.space.element_location(&window);
        state.window_action(&window, crate::WindowAction::ShowMenu);
        assert!(state.window_menu.is_some());
        assert_eq!(state.space.element_location(&window), before);
        assert!(!state.seat.get_pointer().expect("pointer").is_grabbed());
        state.keyboard_key(
            KeyboardSource::MAIN,
            1,
            smithay::backend::input::KeyState::Pressed,
        );
        assert!(state.window_menu.is_none());
        state.keyboard_key(
            KeyboardSource::MAIN,
            1,
            smithay::backend::input::KeyState::Released,
        );
        state.window_action(&window, crate::WindowAction::ShowMenu);
        state.window_unmapped(&window);
        assert!(state.window_menu.is_none());
    }

    #[test]
    fn takeover_clears_pending_menu_escape() {
        let (_, mut state, _, _, _, window) = action_fixture();
        let agent = KeyboardSource::new_auxiliary();
        state.keyboard_key(agent, 30, smithay::backend::input::KeyState::Pressed);
        state.window_action(&window, crate::WindowAction::ShowMenu);
        state.keyboard_key(agent, 1, smithay::backend::input::KeyState::Pressed);
        assert_eq!(state.menu_escape, Some(agent));
        state.set_browser_lease(true);
        assert_eq!(state.menu_escape, None);
    }

    #[test]
    fn outside_click_dismisses_window_menu() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.pointer_motion(KeyboardSource::MAIN, (100.0, 100.0).into());
        state.window_action(&window, crate::WindowAction::ShowMenu);
        state.pointer_motion(KeyboardSource::MAIN, (400.0, 400.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        assert!(state.window_menu.is_none());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
    }

    fn click_menu_item(state: &mut crate::State, window: &Window, row: i32) {
        state.pointer_motion(KeyboardSource::MAIN, (100.0, 100.0).into());
        state.window_action(window, crate::WindowAction::ShowMenu);
        state.pointer_motion(
            KeyboardSource::MAIN,
            (110.0, f64::from(113 + row * 26)).into(),
        );
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
    }

    #[test]
    fn dialog_menu_cannot_minimize() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (_, _, parent_top) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "parent",
            "parent",
        );
        let (_, _, dialog_top) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "dialog",
            "dialog",
        );
        let dialog = top_window(&state);
        dialog_top.set_parent(Some(&parent_top));
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        click_menu_item(&mut state, &dialog, 0);
        assert!(!state.windows.get(&dialog).expect("dialog state").minimized);
    }

    #[test]
    fn menu_minimize_hides_window() {
        let (_, mut state, _, _, _, window) = action_fixture();
        click_menu_item(&mut state, &window, 0);
        assert!(state.window_menu.is_none());
        assert!(state.windows.get(&window).expect("window").minimized);
    }

    #[test]
    fn menu_maximize_and_restore() {
        let (mut display, mut state, connection, mut queue, mut app, window) = action_fixture();
        click_menu_item(&mut state, &window, 1);
        assert!(state.window_menu.is_none());
        assert!(window.toplevel().expect("toplevel").with_pending_state(|pending| pending.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)));
        commit_configure(&mut display, &mut state, &connection, &mut queue, &mut app);
        click_menu_item(&mut state, &window, 1);
        assert!(!window.toplevel().expect("toplevel").with_pending_state(|pending| pending.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)));
    }

    #[test]
    fn menu_move_starts_grab() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.pointer_motion(KeyboardSource::MAIN, (100.0, 100.0).into());
        state.window_action(&window, crate::WindowAction::ShowMenu);
        state.pointer_motion(KeyboardSource::MAIN, (110.0, 191.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        assert!(state.seat.get_pointer().expect("pointer").is_grabbed());
        assert!(state.window_menu.is_none());
    }

    #[test]
    fn menu_resize_starts_bottom_right_grab() {
        let (_, mut state, _, _, _, window) = action_fixture();
        state.pointer_motion(KeyboardSource::MAIN, (100.0, 100.0).into());
        state.window_action(&window, crate::WindowAction::ShowMenu);
        state.pointer_motion(KeyboardSource::MAIN, (110.0, 217.0).into());
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
        assert_eq!(
            state
                .windows
                .get(&window)
                .expect("window")
                .resize
                .as_ref()
                .map(|resize| resize.0),
            Some(10)
        );
        assert!(state.window_menu.is_none());
    }

    #[test]
    fn menu_close_notifies_client() {
        let (mut display, mut state, connection, mut queue, mut app, window) = action_fixture();
        click_menu_item(&mut state, &window, 5);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.close_requests, 1);
        assert!(state.window_menu.is_none());
    }

    #[test]
    fn disabled_menu_items_do_nothing() {
        let (mut display, mut state, connection, mut queue, mut app, window) = action_fixture();
        state.window_action(&window, crate::WindowAction::Maximize);
        commit_configure(&mut display, &mut state, &connection, &mut queue, &mut app);
        for row in [3, 4] {
            click_menu_item(&mut state, &window, row);
            assert!(state.window_menu.is_some());
            assert!(!state.seat.get_pointer().expect("pointer").is_grabbed());
            state.window_menu = None;
        }
    }

    #[test]
    fn menu_always_on_top_preserves_stacking_when_focusing_other_window() {
        let (mut display, mut state, connection, mut queue, mut app, pinned) = action_fixture();
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Other",
            "other",
        );
        let other = top_window(&state);
        click_menu_item(&mut state, &pinned, 2);
        assert!(state.windows.get(&pinned).expect("window").always_on_top);
        state.focus_window(Some(other.clone()));
        assert_eq!(top_window(&state), pinned);
        assert_eq!(
            state.seat.get_keyboard().expect("keyboard").current_focus(),
            KeyboardFocus::window(&other)
        );
        click_menu_item(&mut state, &pinned, 2);
        assert!(!state.windows.get(&pinned).expect("window").always_on_top);
        state.focus_window(Some(other.clone()));
        assert_eq!(top_window(&state), other);
    }

    #[test]
    fn resize_edges_start_matching_grabs_and_cursor_shapes() {
        use waywire_protocol::pipe::CursorShape;
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (surface, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Window",
            "window",
        );
        attach_constraint_buffer(&surface, &app, &queue.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let window = top_window(&state);
        commit_server_decoration(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            &window,
        );
        state.space.map_element(window.clone(), (100, 100), true);
        for (point, edge, cursor) in [
            ((150.0, 72.0), 1, CursorShape::NResize),
            ((150.0, 200.0), 2, CursorShape::SResize),
            ((100.0, 150.0), 4, CursorShape::WResize),
            ((200.0, 150.0), 8, CursorShape::EResize),
            ((100.0, 72.0), 5, CursorShape::NwResize),
            ((200.0, 72.0), 9, CursorShape::NeResize),
            ((100.0, 200.0), 6, CursorShape::SwResize),
            ((200.0, 200.0), 10, CursorShape::SeResize),
        ] {
            state.pointer_motion(KeyboardSource::MAIN, point.into());
            assert_eq!(state.decoration_cursor_under(point.into()), Some(cursor));
            state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
            assert_eq!(
                state
                    .windows
                    .get(&window)
                    .and_then(|state| state.resize)
                    .map(|resize| resize.0),
                Some(edge)
            );
            assert!(state.seat.get_pointer().expect("pointer").is_grabbed());
            state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        }
    }

    #[test]
    fn top_and_left_resize_keep_opposite_edges_fixed() {
        for (point, delta, expected_size, expected_location) in [
            ((100.0, 150.0), (10.0, 0.0), (90, 100), (110, 100)),
            ((150.0, 72.0), (0.0, 20.0), (100, 80), (100, 120)),
            ((100.0, 72.0), (10.0, 20.0), (90, 80), (110, 120)),
        ] {
            let (mut display, mut state) = crate::tests::test_state();
            let (connection, mut queue, mut app) = connect_client(&mut display.handle());
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            let (surface, xdg, _) = map_toplevel(
                &mut display,
                &mut state,
                &connection,
                &mut queue,
                &mut app,
                "Window",
                "window",
            );
            attach_constraint_buffer(&surface, &app, &queue.handle());
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            let window = top_window(&state);
            let top = window.toplevel().expect("top");
            commit_server_decoration(
                &mut display,
                &mut state,
                &connection,
                &mut queue,
                &mut app,
                &window,
            );
            state.space.map_element(window.clone(), (100, 100), true);
            state.pointer_motion(KeyboardSource::MAIN, point.into());
            state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Pressed);
            state.pointer_motion(
                KeyboardSource::MAIN,
                (point.0 + delta.0, point.1 + delta.1).into(),
            );
            assert_eq!(
                top.with_pending_state(|pending| pending.size),
                Some(expected_size.into())
            );
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            xdg.ack_configure(app.last_configure.expect("resize configure"));
            attach_rectangle_buffer(
                &surface,
                &app,
                &queue.handle(),
                expected_size.0,
                expected_size.1,
            );
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            assert_eq!(window.geometry().size, expected_size.into());
            assert_eq!(
                state.space.element_location(&window),
                Some(expected_location.into())
            );
            state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        }
    }

    #[test]
    fn cursor_returns_to_default_after_leaving_resize_band() {
        use waywire_protocol::pipe::CursorShape;
        use waywire_protocol::pipe::Event;
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (surface, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "window",
            "window",
        );
        attach_constraint_buffer(&surface, &app, &queue.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let window = top_window(&state);
        commit_server_decoration(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            &window,
        );
        state.space.map_element(window, (100, 100), true);
        state.pointer_motion(KeyboardSource::MAIN, (100.0, 72.0).into());
        assert!(
            state
                .event_sink
                .take_events()
                .iter()
                .any(|event| matches!(event, Event::CursorShape(CursorShape::NwResize)))
        );
        state.pointer_motion(KeyboardSource::MAIN, (500.0, 500.0).into());
        assert!(
            state
                .event_sink
                .take_events()
                .iter()
                .any(|event| matches!(event, Event::CursorShape(CursorShape::Default)))
        );
    }

    fn managed_handle(app: &App, title: &str) -> ZwlrForeignToplevelHandleV1 {
        app.managed
            .iter()
            .find(|events| events.title == title)
            .and_then(|events| events.handle.clone())
            .expect("managed toplevel handle")
    }

    fn managed_states<'a>(app: &'a App, title: &str) -> &'a [u32] {
        &app.managed
            .iter()
            .find(|events| events.title == title)
            .expect("managed toplevel")
            .states
    }

    fn attach_constraint_buffer(surface: &WlSurface, app: &App, qh: &QueueHandle<App>) {
        attach_rectangle_buffer(surface, app, qh, 100, 100);
    }

    fn attach_rectangle_buffer(
        surface: &WlSurface,
        app: &App,
        qh: &QueueHandle<App>,
        width: i32,
        height: i32,
    ) {
        let fd: OwnedFd =
            nix::sys::memfd::memfd_create("constraint-test", nix::sys::memfd::MFdFlags::empty())
                .expect("memfd");
        let stride = 4 * width;
        let bytes = stride * height;
        nix::unistd::ftruncate(&fd, i64::from(bytes)).expect("ftruncate");
        let pool = app
            .shm
            .as_ref()
            .expect("shm")
            .create_pool(fd.as_fd(), bytes, qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            stride,
            wayland_client::protocol::wl_shm::Format::Argb8888,
            qh,
            (),
        );
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the protocol scenario keeps setup and assertions together"
    )]
    fn constraint_client(mode: &str) {
        use smithay::input::keyboard::KeyboardSource;
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let qh = queue.handle();
        let pointer_client = app.seat.as_ref().expect("seat").get_pointer(&qh, ());
        let relative_manager = app
            .relative_manager
            .as_ref()
            .expect("relative pointer manager");
        let _relative = relative_manager.get_relative_pointer(&pointer_client, &qh, ());
        state.pointer_motion(KeyboardSource::MAIN, (1700.0, 900.0).into());
        let (surface, _xdg, _top) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Constraint",
            "constraint",
        );
        attach_constraint_buffer(&surface, &app, &qh);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let window = top_window(&state);
        state.space.map_element(window, (100, 100), true);
        state.pointer_motion(KeyboardSource::MAIN, (1700.0, 900.0).into());
        if mode != "pending" {
            state.pointer_motion(KeyboardSource::MAIN, (120.0, 120.0).into());
        }
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let manager = app.constraints.as_ref().expect("constraints manager");
        let region = app
            .compositor
            .as_ref()
            .expect("compositor")
            .create_region(&qh, ());
        region.add(10, 10, 30, 30);
        if mode == "confine" {
            region.subtract(25, 10, 5, 30);
            let _constraint = manager.confine_pointer(
                &surface,
                &pointer_client,
                Some(&region),
                constraints::Lifetime::Persistent,
                &qh,
                (),
            );
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            assert!(app.confined, "confine did not activate");
            state.pointer_motion_with_delta(
                KeyboardSource::MAIN,
                (190.0, 120.0).into(),
                Some((70.0, 0.0).into()),
            );
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            let pos = state
                .seat
                .get_pointer()
                .expect("pointer")
                .current_location();
            assert!(
                pos.x < 125.0 && pos.x >= 120.0,
                "confine crossed a hole: {pos:?}"
            );
        } else {
            let constraint = manager.lock_pointer(
                &surface,
                &pointer_client,
                Some(&region),
                constraints::Lifetime::Persistent,
                &qh,
                (),
            );
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            if mode == "pending" {
                assert!(!app.locked, "lock activated outside surface");
                state.pointer_motion(KeyboardSource::MAIN, (120.0, 120.0).into());
                sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            }
            assert!(app.locked, "lock did not activate");
            let pointer = state.seat.get_pointer().expect("pointer");
            let before = pointer.current_location();
            constraint.set_cursor_position_hint(80.0, 80.0);
            surface.commit();
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            assert_eq!(
                pointer.current_location(),
                before,
                "Wayland hint moved a locked pointer"
            );
            let motions = app.pointer_motions;
            state.pointer_motion_with_delta(
                KeyboardSource::MAIN,
                (before.x + 70.0, before.y).into(),
                Some((70.0, 0.0).into()),
            );
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            assert_eq!(pointer.current_location(), before, "locked pointer moved");
            assert_eq!(
                app.pointer_motions, motions,
                "locked pointer sent absolute motion"
            );
            assert_eq!(
                app.relative.last(),
                Some(&(70.0, 0.0)),
                "relative motion lost"
            );
            surface.attach(None, 0, 0);
            surface.commit();
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            state.refresh_pointer_focus();
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            assert!(!app.locked, "lock remains active after focus leaves");
        }
    }

    #[test]
    fn pointer_lock_keeps_position_and_sends_relative_motion() {
        constraint_client("lock");
    }
    #[test]
    fn pointer_constraint_created_before_focus_activates_on_entry() {
        constraint_client("pending");
    }
    #[test]
    fn pointer_confine_keeps_position_in_region() {
        constraint_client("confine");
    }

    #[test]
    fn background_click_preserves_keyboard_focus() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let _client_keyboard = app
            .seat
            .as_ref()
            .expect("seat")
            .get_keyboard(&queue.handle(), ());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Focused",
            "focused",
        );
        let keyboard = state.seat.get_keyboard().expect("keyboard");
        let focused = keyboard.current_focus().expect("window focused on map");
        state.pointer_motion(
            smithay::input::keyboard::KeyboardSource::MAIN,
            (1800.0, 1000.0).into(),
        );
        state.pointer_button(
            smithay::input::keyboard::KeyboardSource::MAIN,
            0x110,
            smithay::backend::input::ButtonState::Pressed,
        );
        assert_eq!(keyboard.current_focus(), Some(focused));
        state.keyboard_key(
            smithay::input::keyboard::KeyboardSource::MAIN,
            30,
            smithay::backend::input::KeyState::Pressed,
        );
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.keys, [30], "the focused client receives typing");
    }

    #[test]
    fn expanded_windows_refuse_decoration_resize_grabs() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Window",
            "window",
        );
        let window = top_window(&state);
        let pointer = state.seat.get_pointer().expect("pointer");
        for mode in [
            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized,
            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen,
        ] {
            state.set_window_expanded(&window, mode, true);
            state.start_decoration_grab(window.clone(), smithay::utils::SERIAL_COUNTER.next_serial(), true);
            assert!(!pointer.is_grabbed(), "expanded window must not begin resizing");
            state.set_window_expanded(&window, mode, false);
        }
        state.start_decoration_grab(window, smithay::utils::SERIAL_COUNTER.next_serial(), true);
        assert!(pointer.is_grabbed(), "normal window can begin resizing");
    }

    #[test]
    fn stationary_pointer_tracks_newly_mapped_window() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let _client_pointer = app
            .seat
            .as_ref()
            .expect("seat")
            .get_pointer(&queue.handle(), ());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "First",
            "first",
        );
        let first = top_window(&state);
        state.space.map_element(first.clone(), (80, 80), true);
        state.pointer_motion(
            smithay::input::keyboard::KeyboardSource::MAIN,
            (80.0, 80.0).into(),
        );
        let pointer = state.seat.get_pointer().expect("pointer");
        assert_eq!(
            pointer.current_focus(),
            first.wl_surface().map(std::borrow::Cow::into_owned)
        );
        state.start_decoration_grab(
            first.clone(),
            smithay::utils::SERIAL_COUNTER.next_serial(),
            false,
        );
        assert!(pointer.is_grabbed());
        let (second_surface, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Second",
            "second",
        );
        let second = top_window(&state);
        assert_ne!(first, second);
        pointer.unset_grab(
            &mut state,
            smithay::utils::SERIAL_COUNTER.next_serial(),
            smithay::backend::input::InputTime::now(),
        );
        assert_eq!(
            pointer.current_focus(),
            second.wl_surface().map(std::borrow::Cow::into_owned)
        );
        state.pointer_button(
            smithay::input::keyboard::KeyboardSource::MAIN,
            0x110,
            smithay::backend::input::ButtonState::Pressed,
        );
        state.pointer_button(
            smithay::input::keyboard::KeyboardSource::MAIN,
            0x110,
            smithay::backend::input::ButtonState::Released,
        );
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(
            app.button_targets,
            [second_surface.clone(), second_surface.clone()]
        );
        state.start_decoration_grab(
            second.clone(),
            smithay::utils::SERIAL_COUNTER.next_serial(),
            true,
        );
        state.focus_window(Some(first.clone()));
        pointer.unset_grab(
            &mut state,
            smithay::utils::SERIAL_COUNTER.next_serial(),
            smithay::backend::input::InputTime::now(),
        );
        assert_eq!(
            pointer.current_focus(),
            first.wl_surface().map(std::borrow::Cow::into_owned)
        );
        state.focus_window(Some(second.clone()));
        assert_eq!(
            pointer.current_focus(),
            second.wl_surface().map(std::borrow::Cow::into_owned)
        );
        second_surface.attach(None, 0, 0);
        second_surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(
            pointer.current_focus(),
            first.wl_surface().map(std::borrow::Cow::into_owned)
        );
    }

    #[test]
    fn wayland_dialog_is_centered_and_stays_above_its_parent() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (parent_surface, _, parent_top) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Parent",
            "parent",
        );
        attach_buffer(&parent_surface, &app, &queue.handle(), 50);
        parent_surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let parent = top_window(&state);
        let qh = queue.handle();
        let surface = app
            .compositor
            .as_ref()
            .expect("compositor")
            .create_surface(&qh, ());
        let xdg = app
            .wm_base
            .as_ref()
            .expect("shell")
            .get_xdg_surface(&surface, &qh, ());
        let dialog = xdg.get_toplevel(&qh, ());
        dialog.set_parent(Some(&parent_top));
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        xdg.ack_configure(app.last_configure.expect("configure"));
        attach_buffer(&surface, &app, &qh, 1);
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let child = top_window(&state);
        let parent_location = state
            .space
            .element_location(&parent)
            .expect("parent location");
        let child_location = state
            .space
            .element_location(&child)
            .expect("child location");
        assert_eq!(
            child_location.x,
            parent_location.x + 24,
            "dialog centered over parent"
        );
        state.pointer_motion(
            smithay::input::keyboard::KeyboardSource::MAIN,
            (f64::from(parent_location.x), f64::from(parent_location.y)).into(),
        );
        state.pointer_button(
            smithay::input::keyboard::KeyboardSource::MAIN,
            0x110,
            smithay::backend::input::ButtonState::Pressed,
        );
        assert_eq!(top_window(&state), child, "dialog stays above parent");
        assert_eq!(
            state.seat.get_keyboard().expect("keyboard").current_focus(),
            KeyboardFocus::window(&child)
        );
        state.pointer_button(KeyboardSource::MAIN, 0x110, ButtonState::Released);
        state.window_action(&parent, crate::WindowAction::ToggleAlwaysOnTop);
        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Other",
            "other",
        );
        let other = state
            .space
            .elements()
            .find(|window| *window != &parent && *window != &child)
            .cloned()
            .expect("other window");
        state.focus_window(Some(other));
        assert_eq!(
            top_window(&state),
            child,
            "dialog stays above always-on-top parent and normal windows"
        );
        state.window_action(&parent, crate::WindowAction::Maximize);
        assert_eq!(
            top_window(&state),
            child,
            "maximizing the parent keeps its dialog above it"
        );
    }

    #[test]
    fn wlr_client_sees_one_handle_with_title_app_id_and_state() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(
            app.manager.is_some(),
            "zwlr_foreign_toplevel_manager_v1 global advertised"
        );

        map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Foot",
            "foot",
        );
        assert_eq!(app.managed.len(), 1, "one handle for the mapped window");
        let toplevel = &app.managed[0];
        assert_eq!(toplevel.title, "Foot");
        assert_eq!(toplevel.app_id, "foot");
        assert_eq!(toplevel.states, vec![WlrState::Activated as u32]);
        assert!(!toplevel.closed);

        let handle = toplevel.handle.clone().expect("handle announced");
        let done = toplevel.done;
        handle.set_maximized();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(
            managed_states(&app, "Foot"),
            [WlrState::Maximized as u32, WlrState::Activated as u32]
        );
        assert_eq!(app.managed[0].done, done + 1, "one done per state change");

        handle.unset_maximized();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(managed_states(&app, "Foot"), [WlrState::Activated as u32]);
    }

    #[test]
    fn unchanged_commits_do_not_resend_wlr_done() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);

        let (surface, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Foot",
            "foot",
        );
        let done = app.managed[0].done;
        surface.commit();
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(
            app.managed[0].done, done,
            "a commit without a window change sends no done"
        );
    }

    #[test]
    fn close_request_closes_the_window_through_the_shared_path() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);

        let (surface, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Foot",
            "foot",
        );
        managed_handle(&app, "Foot").close();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.close_requests, 1, "the client receives xdg close");
        assert!(!app.managed[0].closed, "the handle closes only on unmap");

        surface.attach(None, 0, 0);
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(app.managed[0].closed, "unmapping closes the wlr handle");
        assert_eq!(state.toplevel_list.handles.len(), 0);
    }

    #[test]
    fn activate_raises_and_focuses_a_window_behind_another() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);

        let (_surface_a, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "A",
            "a",
        );
        let window_a = top_window(&state);
        let (_surface_b, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "B",
            "b",
        );
        let window_b = top_window(&state);
        assert_eq!(
            state
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus()),
            KeyboardFocus::window(&window_b),
            "the newer window starts focused"
        );
        assert_eq!(
            state.space.elements().last(),
            Some(&window_b),
            "the newer window starts on top"
        );
        assert_eq!(managed_states(&app, "B"), [WlrState::Activated as u32]);

        state.window_action(&window_a, crate::WindowAction::Minimize);
        managed_handle(&app, "A").activate(app.seat.as_ref().expect("seat bound"));
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(!state.windows.get(&window_a).expect("window").minimized);

        assert_eq!(
            state
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus()),
            KeyboardFocus::window(&window_a)
        );
        assert_eq!(
            state.space.elements().last(),
            Some(&window_a),
            "activate raises the window"
        );
        assert_eq!(managed_states(&app, "A"), [WlrState::Activated as u32]);
        assert!(
            managed_states(&app, "B").is_empty(),
            "the previously focused window is deactivated"
        );
    }

    #[test]
    fn foreign_minimize_reports_state_and_unmaximize_keeps_it_hidden() {
        let (mut display, mut state, connection, mut queue, mut app, window) = action_fixture();
        let handle = managed_handle(&app, "Window");
        handle.set_maximized();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        handle.set_minimized();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(state.windows.get(&window).expect("window").minimized);
        assert!(managed_states(&app, "Window").contains(&(WlrState::Minimized as u32)));
        handle.unset_maximized();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(state.windows.get(&window).expect("window").minimized);
        assert!(!managed_states(&app, "Window").contains(&(WlrState::Maximized as u32)));
        handle.unset_minimized();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(!state.windows.get(&window).expect("window").minimized);
        assert!(!managed_states(&app, "Window").contains(&(WlrState::Minimized as u32)));
    }

    #[test]
    fn foreign_toplevel_requests_are_dropped_during_a_browser_lease() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);

        let (_surface_a, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "A",
            "a",
        );
        let window_a = top_window(&state);
        let (_surface_b, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "B",
            "b",
        );
        let window_b = top_window(&state);
        let handle_a = managed_handle(&app, "A");
        let done = app
            .managed
            .iter()
            .find(|events| events.title == "A")
            .expect("managed A")
            .done;

        state.set_browser_lease(true);
        handle_a.activate(app.seat.as_ref().expect("seat bound"));
        handle_a.close();
        handle_a.set_maximized();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);

        assert_eq!(
            state
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus()),
            KeyboardFocus::window(&window_b),
            "a dropped activate leaves focus alone"
        );
        assert_eq!(app.close_requests, 0, "a dropped close does nothing");
        assert!(
            !managed_states(&app, "A").contains(&(WlrState::Maximized as u32)),
            "a dropped maximize changes no state"
        );
        assert_eq!(
            app.managed
                .iter()
                .find(|events| events.title == "A")
                .expect("managed A")
                .done,
            done,
            "dropping a request sends no done"
        );

        state.set_browser_lease(false);
        assert_eq!(
            state
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus()),
            KeyboardFocus::window(&window_b),
            "handback leaves focus where it was"
        );
        handle_a.activate(app.seat.as_ref().expect("seat bound"));
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(
            state
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus()),
            KeyboardFocus::window(&window_a)
        );
        assert_eq!(
            managed_states(&app, "A"),
            [WlrState::Activated as u32],
            "requests resume after handback"
        );
    }

    #[test]
    fn surface_cursor_sends_scaled_png_and_hotspot_once() {
        use smithay::input::keyboard::KeyboardSource;
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let qh = queue.handle();
        let pointer = app.seat.as_ref().expect("seat").get_pointer(&qh, ());
        let (surface, _, _) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Cursor",
            "cursor",
        );
        attach_constraint_buffer(&surface, &app, &qh);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let window = top_window(&state);
        state.space.map_element(window, (100, 100), true);
        state.pointer_motion(KeyboardSource::MAIN, (120.0, 120.0).into());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let cursor = app
            .compositor
            .as_ref()
            .expect("compositor")
            .create_surface(&qh, ());
        let fd: OwnedFd =
            nix::sys::memfd::memfd_create("cursor-test", nix::sys::memfd::MFdFlags::empty())
                .expect("memfd");
        nix::unistd::ftruncate(&fd, 256).expect("size");
        let pixels = [0_u8, 0, 255, 255].repeat(64);
        assert_eq!(nix::unistd::write(&fd, &pixels).expect("write pixels"), 256);
        let pool = app
            .shm
            .as_ref()
            .expect("shm")
            .create_pool(fd.as_fd(), 256, &qh, ());
        let buffer = pool.create_buffer(
            0,
            8,
            8,
            32,
            wayland_client::protocol::wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        cursor.set_buffer_scale(2);
        cursor.attach(Some(&buffer), 0, 0);
        pointer.set_cursor(
            app.pointer_serial.expect("pointer enter"),
            Some(&cursor),
            1,
            2,
        );
        cursor.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let events = state.event_sink.take_events();
        let image = events
            .iter()
            .find_map(|event| {
                if let waywire_protocol::pipe::Event::CursorImage(image) = event {
                    Some(image)
                } else {
                    None
                }
            })
            .expect("cursor image event");
        assert_eq!(
            (image.width, image.height, image.hotspot_x, image.hotspot_y),
            (4, 4, 1, 2)
        );
        assert!(image.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let mut decoded =
            cairo::ImageSurface::create_from_png(&mut image.png.as_slice()).expect("decode PNG");
        assert_eq!(&decoded.data().expect("PNG pixels")[..4], &[0, 0, 255, 255]);
        cursor.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(
            !state
                .event_sink
                .take_events()
                .iter()
                .any(|event| matches!(event, waywire_protocol::pipe::Event::CursorImage(_)))
        );
    }

    #[test]
    fn output_reports_refresh_after_quality_change() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.refresh_rates.last(), Some(&60_000));
        let mut quality = state.quality;
        quality.fps = waywire_protocol::pipe::Fps::new(30).expect("valid fps");
        state
            .apply_command(waywire_protocol::pipe::Command::Quality(quality))
            .expect("quality change");
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.refresh_rates.last(), Some(&30_000));
    }

    #[test]
    fn surface_receives_preferred_buffer_scale_on_map_and_resize() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        let (_surface, _xdg, _top) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Scale",
            "scale",
        );
        state.space.refresh();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.preferred_scales, [1]);
        for (scale, expected) in [(1.5, 2), (2.0, 2), (1.0, 1)] {
            state.output.change_current_state(
                None,
                None,
                Some(smithay::output::Scale::Fractional(scale)),
                None,
            );
            state.output_resized();
            sync(&mut display, &mut state, &connection, &mut queue, &mut app);
            assert_eq!(app.preferred_scales.last(), Some(&expected));
        }
    }

    #[test]
    fn mapped_toplevel_is_announced_and_unmapping_closes_it() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        // Round one flushes the registry request, round two binds the globals.
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(
            app.list.is_some(),
            "ext_foreign_toplevel_list global advertised"
        );

        let (surface, _xdg_surface, _toplevel) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Foot",
            "foot",
        );
        assert_eq!(app.handles.len(), 1, "one handle for the mapped window");
        assert_eq!(app.handles[0].title, "Foot");
        assert_eq!(app.handles[0].app_id, "foot");
        assert!(!app.handles[0].closed);
        assert!(!app.handles[0].identifier.is_empty());

        // Unmap by removing the buffer; the compositor must close the handle.
        surface.attach(None, 0, 0);
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(app.handles[0].closed, "unmapping closes the handle");
        assert_eq!(
            state.toplevel_list.handles.len(),
            0,
            "no handle remains after unmap"
        );
    }

    #[test]
    fn title_change_updates_the_handle_without_reannouncing() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect_client(&mut display.handle());
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);

        let (surface, _, toplevel) = map_toplevel(
            &mut display,
            &mut state,
            &connection,
            &mut queue,
            &mut app,
            "Old title",
            "foot",
        );
        assert_eq!(app.handles.len(), 1);

        // Commits without a metadata change send no events at all.
        let announced_done = app.handles[0].done;
        surface.commit();
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.handles[0].done, announced_done);

        toplevel.set_title("New title".to_owned());
        surface.commit();
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(app.handles.len(), 1, "no second handle");
        assert_eq!(app.handles[0].done, announced_done + 1);
        assert_eq!(app.handles[0].title, "New title");
        assert_eq!(app.handles[0].app_id, "foot");
        assert!(!app.handles[0].closed);
    }
}
