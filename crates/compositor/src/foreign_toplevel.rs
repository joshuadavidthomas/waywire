//! The wlr-foreign-toplevel-management-unstable-v1 protocol.
//!
//! Every managed window publishes one handle per bound manager so standard
//! tools such as `wlrctl` can list windows and request window actions. The set
//! of windows matches the ext-foreign-toplevel-list handles in
//! `toplevel_list.rs`: Wayland toplevels and managed X11 windows, never
//! override-redirect popups.
//!
//! Title, app id and state events are sent only when their value changed,
//! followed by one `done`. A buffer commit with no window change sends nothing.
//! Requests go through the same browser input lease as agent input: while a
//! browser holds control they are dropped, with one debug line per lease.

use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_handle_v1::{
    self, ZwlrForeignToplevelHandleV1,
};
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::{
    self, ZwlrForeignToplevelManagerV1,
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, New, Resource,
};
use smithay::wayland::Dispatch2;
use smithay::wayland::GlobalDispatch2;
use smithay::xwayland::X11Surface;

use crate::State;
use crate::WindowAction;

/// The newest protocol version we serve. Version 2 adds fullscreen state and
/// requests; version 3 adds parent handles, which this compositor does not
/// publish.
const VERSION: u32 = 3;

struct GlobalData;

impl GlobalDispatch2<ZwlrForeignToplevelManagerV1, State> for GlobalData {
    fn bind(
        &self,
        state: &mut State,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        let manager = data_init.init(resource, ManagerData);
        state.toplevel_list.manager_bound(&manager);
    }
}

struct ManagerData;

impl Dispatch2<ZwlrForeignToplevelManagerV1, State> for ManagerData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            resource.finished();
            state.toplevel_list.manager_removed(resource);
        }
    }

    fn destroyed(
        &self,
        state: &mut State,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
    ) {
        state.toplevel_list.manager_removed(resource);
    }
}

/// Per-handle server data. The window identifies the managed toplevel.
struct HandleData {
    window: Window,
}

impl Dispatch2<ZwlrForeignToplevelHandleV1, State> for HandleData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;

        // A closed or otherwise retired handle is inert. Requests are ignored;
        // the client's destroy still reaches the `destroyed` callback.
        if !state.toplevel_list.holds_handle(&self.window, resource) {
            return;
        }

        // Destroy and the rectangle hint are not window actions and remain
        // valid while the browser holds input. Every other request is agent
        // input and follows the lease.
        if !matches!(request, Request::Destroy | Request::SetRectangle { .. })
            && !state
                .toplevel_list
                .accepts_request(state.browser_holds_input)
        {
            return;
        }
        match request {
            Request::Activate { seat } => {
                state.with_focus_cause(crate::delivery::FocusCause::WindowRequest, |state| {
                    state.activate_foreign_toplevel(&self.window, &seat);
                });
            }
            Request::Close => state.window_action(&self.window, WindowAction::Close),
            Request::SetMaximized => {
                state.window_action(&self.window, WindowAction::Maximize);
            }
            Request::UnsetMaximized => {
                state.set_window_expanded(&self.window, xdg_toplevel::State::Maximized, false);
            }
            Request::SetFullscreen { output: _ } => {
                state.set_window_expanded(&self.window, xdg_toplevel::State::Fullscreen, true);
            }
            Request::UnsetFullscreen => {
                state.set_window_expanded(&self.window, xdg_toplevel::State::Fullscreen, false);
            }
            Request::SetMinimized => state.window_action(&self.window, WindowAction::Minimize),
            Request::UnsetMinimized => {
                if state
                    .windows
                    .get(&self.window)
                    .is_some_and(|window| window.minimized)
                {
                    state.window_action(&self.window, WindowAction::Restore);
                }
            }
            // Destroy, the rectangle hint, and any future requests are
            // passive; the generated request enum is non-exhaustive.
            Request::SetRectangle { .. } | Request::Destroy | _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut State,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
    ) {
        state.toplevel_list.handle_destroyed(resource);
    }
}

/// The states exposed by the protocol.
pub(crate) fn states(window: &Window, activated: bool, minimized: bool) -> Vec<u32> {
    let (maximized, fullscreen) = if let Some(top) = window.toplevel() {
        top.with_pending_state(|state| {
            (
                state.states.contains(xdg_toplevel::State::Maximized),
                state.states.contains(xdg_toplevel::State::Fullscreen),
            )
        })
    } else if let Some(surface) = window.x11_surface() {
        (surface.is_maximized(), surface.is_fullscreen())
    } else {
        (false, false)
    };
    let mut states = Vec::with_capacity(3);
    if maximized {
        states.push(zwlr_foreign_toplevel_handle_v1::State::Maximized as u32);
    }
    if fullscreen {
        states.push(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32);
    }
    if activated {
        states.push(zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
    }
    if minimized {
        states.push(zwlr_foreign_toplevel_handle_v1::State::Minimized as u32);
    }
    states
}

/// Whether the window itself currently carries the activated state. Initial
/// publication uses this; focus changes then call [`ForeignToplevels::refresh`].
pub(crate) fn activated(window: &Window) -> bool {
    if let Some(top) = window.toplevel() {
        top.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Activated))
    } else {
        window.x11_surface().is_some_and(X11Surface::is_activated)
    }
}

struct Manager {
    id: u64,
    resource: ZwlrForeignToplevelManagerV1,
}

struct Toplevel {
    title: String,
    app_id: String,
    states: Vec<u32>,
    handles: Vec<(ZwlrForeignToplevelHandleV1, u64)>,
}

pub(crate) struct ForeignToplevels {
    display: DisplayHandle,
    managers: Vec<Manager>,
    toplevels: HashMap<Window, Toplevel>,
    next_manager: u64,
    lease_drop_logged: bool,
}

impl ForeignToplevels {
    pub(crate) fn new(display_handle: &DisplayHandle) -> Self {
        display_handle.create_global::<State, ZwlrForeignToplevelManagerV1, _>(VERSION, GlobalData);
        Self {
            display: display_handle.clone(),
            managers: Vec::new(),
            toplevels: HashMap::new(),
            next_manager: 1,
            lease_drop_logged: false,
        }
    }

    /// Announce a window to every bound manager. The caller guarantees the
    /// window is not announced yet.
    pub(crate) fn publish(
        &mut self,
        window: &Window,
        title: String,
        app_id: String,
        states: Vec<u32>,
    ) {
        let mut handles = Vec::new();
        for manager in &self.managers {
            if let Some(handle) = self.create_handle(manager, window, &title, &app_id, &states) {
                handles.push((handle, manager.id));
            }
        }
        self.toplevels.insert(
            window.clone(),
            Toplevel {
                title,
                app_id,
                states,
                handles,
            },
        );
    }

    /// A new manager observes every window that is open now.
    pub(crate) fn manager_bound(&mut self, resource: &ZwlrForeignToplevelManagerV1) {
        let id = self.next_manager;
        self.next_manager += 1;
        let manager = Manager {
            id,
            resource: resource.clone(),
        };
        let windows: Vec<(Window, String, String, Vec<u32>)> = self
            .toplevels
            .iter()
            .map(|(window, toplevel)| {
                (
                    window.clone(),
                    toplevel.title.clone(),
                    toplevel.app_id.clone(),
                    toplevel.states.clone(),
                )
            })
            .collect();
        for (window, title, app_id, states) in windows {
            if let Some(handle) = self.create_handle(&manager, &window, &title, &app_id, &states)
                && let Some(toplevel) = self.toplevels.get_mut(&window)
            {
                toplevel.handles.push((handle, id));
            }
        }
        self.managers.push(manager);
    }

    pub(crate) fn manager_removed(&mut self, resource: &ZwlrForeignToplevelManagerV1) {
        let Some(index) = self
            .managers
            .iter()
            .position(|manager| manager.resource == *resource)
        else {
            return;
        };
        let manager = self.managers.remove(index);
        for toplevel in self.toplevels.values_mut() {
            toplevel.handles.retain(|(_, id)| *id != manager.id);
        }
    }

    pub(crate) fn handle_destroyed(&mut self, handle: &ZwlrForeignToplevelHandleV1) {
        for toplevel in self.toplevels.values_mut() {
            toplevel.handles.retain(|(known, _)| known != handle);
        }
    }

    pub(crate) fn holds_handle(
        &self,
        window: &Window,
        handle: &ZwlrForeignToplevelHandleV1,
    ) -> bool {
        self.toplevels
            .get(window)
            .is_some_and(|toplevel| toplevel.handles.iter().any(|(known, _)| known == handle))
    }

    pub(crate) fn update_metadata(&mut self, window: &Window, title: &str, app_id: &str) {
        let Some(toplevel) = self.toplevels.get_mut(window) else {
            return;
        };
        let title_changed = toplevel.title != title;
        let app_id_changed = toplevel.app_id != app_id;
        if !title_changed && !app_id_changed {
            return;
        }
        if title_changed {
            title.clone_into(&mut toplevel.title);
        }
        if app_id_changed {
            app_id.clone_into(&mut toplevel.app_id);
        }
        for (handle, _) in &toplevel.handles {
            if title_changed {
                handle.title(title.to_owned());
            }
            if app_id_changed {
                handle.app_id(app_id.to_owned());
            }
            handle.done();
        }
    }

    pub(crate) fn update_states(&mut self, window: &Window, states: Vec<u32>) {
        let Some(toplevel) = self.toplevels.get_mut(window) else {
            return;
        };
        if toplevel.states == states {
            return;
        }
        let encoded: Vec<u8> = states
            .iter()
            .flat_map(|state| state.to_ne_bytes())
            .collect();
        toplevel.states = states;
        for (handle, _) in &toplevel.handles {
            handle.state(encoded.clone());
            handle.done();
        }
    }

    /// Refresh the states of every published window after a focus change.
    pub(crate) fn refresh(
        &mut self,
        focused: Option<&crate::focus::KeyboardFocus>,
        minimized: &[Window],
    ) {
        let published: Vec<Window> = self.toplevels.keys().cloned().collect();
        for window in published {
            let activated = crate::focus::KeyboardFocus::window(&window).as_ref() == focused;
            self.update_states(
                &window,
                states(&window, activated, minimized.contains(&window)),
            );
        }
    }

    pub(crate) fn unpublish(&mut self, window: &Window) {
        if let Some(toplevel) = self.toplevels.remove(window) {
            for (handle, _) in toplevel.handles {
                handle.closed();
            }
        }
    }

    /// True while requests may act on windows. A browser lease drops them and
    /// logs once, not once per request.
    pub(crate) fn accepts_request(&mut self, browser_holds_input: bool) -> bool {
        if !browser_holds_input {
            return true;
        }
        if !self.lease_drop_logged {
            self.lease_drop_logged = true;
            tracing::debug!("dropped foreign toplevel request while a browser holds input control");
        }
        false
    }

    /// Reset the one-log-per-lease guard whenever control changes hands.
    pub(crate) fn lease_changed(&mut self) {
        self.lease_drop_logged = false;
    }

    fn create_handle(
        &self,
        manager: &Manager,
        window: &Window,
        title: &str,
        app_id: &str,
        states: &[u32],
    ) -> Option<ZwlrForeignToplevelHandleV1> {
        let client = manager.resource.client()?;
        let handle = client
            .create_resource::<ZwlrForeignToplevelHandleV1, _, State>(
                &self.display,
                manager.resource.version(),
                HandleData {
                    window: window.clone(),
                },
            )
            .ok()?;
        manager.resource.toplevel(&handle);
        handle.title(title.to_owned());
        handle.app_id(app_id.to_owned());
        handle.state(
            states
                .iter()
                .flat_map(|state| state.to_ne_bytes())
                .collect(),
        );
        handle.done();
        Some(handle)
    }
}
