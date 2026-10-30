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
        let states =
            crate::foreign_toplevel::states(window, crate::foreign_toplevel::activated(window));
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
    pub(crate) fn refresh_states(&mut self, focused: Option<&crate::focus::KeyboardFocus>) {
        self.foreign.refresh(focused);
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
fn metadata(window: &Window) -> Option<(String, String)> {
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

    use smithay::desktop::Window;
    use smithay::wayland::seat::WaylandFocus;
    use wayland_client::Connection;
    use wayland_client::Dispatch;
    use wayland_client::EventQueue;
    use wayland_client::QueueHandle;
    use wayland_client::protocol::wl_compositor::WlCompositor;
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
        handles: Vec<ToplevelHandleEvents>,
        managed: Vec<ManagedToplevelEvents>,
        close_requests: usize,
        keys: Vec<u32>,
        pointer_focus: Option<WlSurface>,
        button_targets: Vec<WlSurface>,
        pointer_motions: usize,
        constraints: Option<ZwpPointerConstraintsV1>,
        relative_manager: Option<ZwpRelativePointerManagerV1>,
        locked: bool,
        confined: bool,
        relative: Vec<(f64, f64)>,
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

    impl Dispatch<WlSurface, ()> for App {
        fn event(
            _: &mut Self,
            _: &WlSurface,
            _: wayland_client::protocol::wl_surface::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
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
    wayland_client::delegate_noop!(App: ignore ZwpPointerConstraintsV1);
    wayland_client::delegate_noop!(App: ignore ZwpRelativePointerManagerV1);
    wayland_client::delegate_noop!(App: ignore wayland_client::protocol::wl_region::WlRegion);

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
                wayland_client::protocol::wl_pointer::Event::Enter { surface, .. } => {
                    state.pointer_focus = Some(surface);
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
        (surface, xdg_surface, toplevel)
    }

    fn top_window(state: &crate::State) -> Window {
        state
            .space
            .elements()
            .last()
            .cloned()
            .expect("mapped window")
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
        let fd: OwnedFd =
            nix::sys::memfd::memfd_create("constraint-test", nix::sys::memfd::MFdFlags::empty())
                .expect("memfd");
        nix::unistd::ftruncate(&fd, 40000).expect("ftruncate");
        let pool = app
            .shm
            .as_ref()
            .expect("shm")
            .create_pool(fd.as_fd(), 40000, qh, ());
        let buffer = pool.create_buffer(
            0,
            100,
            100,
            400,
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

        managed_handle(&app, "A").activate(app.seat.as_ref().expect("seat bound"));
        sync(&mut display, &mut state, &connection, &mut queue, &mut app);

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
