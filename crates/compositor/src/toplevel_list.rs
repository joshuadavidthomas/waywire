//! The ext-foreign-toplevel-list-v1 protocol.
//!
//! Every managed window publishes one foreign toplevel handle so standard tools
//! such as `lswt` can observe open windows with their title and app id. Wayland
//! toplevels are announced when they map and X11 windows when the XWM maps
//! them; override-redirect popups are not toplevels and never get a handle.
//! Unmapping or destroying a window closes its handle.

use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::IsAlive;
use smithay::wayland::compositor::with_states;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelListHandler;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelListState;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::xwayland::X11Surface;

use crate::State;

pub(crate) struct ToplevelList {
    state: ForeignToplevelListState,
    handles: HashMap<Window, ForeignToplevelHandle>,
}

impl ToplevelList {
    pub(crate) fn new(display_handle: &DisplayHandle) -> Self {
        Self {
            state: ForeignToplevelListState::new::<State>(display_handle),
            handles: HashMap::new(),
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
        let handle = self.state.new_toplevel::<State>(title, app_id);
        self.handles.insert(window.clone(), handle);
    }

    /// Refresh an announced window's title and app id, sending `done` only
    /// when one of them changed.
    pub(crate) fn update(&mut self, window: &Window) {
        let Some(handle) = self.handles.get(window) else {
            return;
        };
        let Some((title, app_id)) = metadata(window) else {
            return;
        };
        if handle.title() == title && handle.app_id() == app_id {
            return;
        }
        handle.send_title(&title);
        handle.send_app_id(&app_id);
        handle.send_done();
    }

    /// Close the window's handle; the entry is removed when the window unmaps
    /// or is destroyed.
    pub(crate) fn unpublish(&mut self, window: &Window) {
        if let Some(handle) = self.handles.remove(window) {
            self.state.remove_toplevel(&handle);
        }
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

    use wayland_client::Connection;
    use wayland_client::Dispatch;
    use wayland_client::EventQueue;
    use wayland_client::QueueHandle;
    use wayland_client::protocol::wl_compositor::WlCompositor;
    use wayland_client::protocol::wl_registry;
    use wayland_client::protocol::wl_shm::WlShm;
    use wayland_client::protocol::wl_shm_pool::WlShmPool;
    use wayland_client::protocol::wl_surface::WlSurface;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1;
    use wayland_protocols::xdg::shell::client::xdg_surface::XdgSurface;
    use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
    use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;

    /// A Wayland client that maps a minimal xdg toplevel and records the
    /// foreign toplevel events the compositor announces.
    #[derive(Default)]
    struct App {
        compositor: Option<WlCompositor>,
        shm: Option<WlShm>,
        wm_base: Option<XdgWmBase>,
        list: Option<ExtForeignToplevelListV1>,
        last_configure: Option<u32>,
        handles: Vec<ToplevelHandleEvents>,
    }

    #[derive(Default, Clone, PartialEq)]
    struct ToplevelHandleEvents {
        identifier: String,
        title: String,
        app_id: String,
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
            _: &mut Self,
            _: &XdgToplevel,
            _: wayland_protocols::xdg::shell::client::xdg_toplevel::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
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
