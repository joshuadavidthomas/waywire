//! xdg-activation: one application hands focus to another, or a launcher
//! hands it to the application it starts. The policy is recorded in the
//! ROADMAP's Decisions section.
//!
//! A token may raise its window when the compositor issued it for a launch,
//! or when a client created it with a serial from input it received while
//! it held keyboard focus. Browser, EIS, virtual-pointer and
//! virtual-keyboard input all deliver seat serials, so agent input grants
//! tokens the same way human input does. Any other activation request marks
//! the window urgent instead of stealing focus.
use std::time::Duration;

use smithay::desktop::Window;
use smithay::input::Seat;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::xdg_activation::XdgActivationHandler;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xdg_activation::XdgActivationToken;
use smithay::wayland::xdg_activation::XdgActivationTokenData;

use crate::State;

/// Long enough for a cold application start between a click and its window.
const TOKEN_LIFETIME: Duration = Duration::from_secs(30);

/// Marks a token that may raise its window.
struct Granted;

/// The xdg-activation global and the tokens it issued.
pub(crate) struct Activation(XdgActivationState);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenAction {
    Raise,
    MarkUrgent,
}

impl Activation {
    pub(crate) fn new(display_handle: &DisplayHandle) -> Self {
        Self(XdgActivationState::new::<State>(display_handle))
    }

    pub(crate) fn state(&mut self) -> &mut XdgActivationState {
        &mut self.0
    }

    /// A client created a token. Expired tokens go; `granted` marks this one
    /// as able to raise its window. Every token is kept, so a request with an
    /// ungranted token still arrives and can mark its window urgent.
    fn created(&mut self, data: &XdgActivationTokenData, action: TokenAction) {
        self.0
            .retain_tokens(|_, data| data.timestamp.elapsed() < TOKEN_LIFETIME);
        if action == TokenAction::Raise {
            data.user_data.insert_if_missing(|| Granted);
        }
    }

    /// Use up a token and say whether it may raise its window.
    fn redeem(&mut self, token: &XdgActivationToken, data: &XdgActivationTokenData) -> TokenAction {
        self.0.remove_token(token);
        if data.timestamp.elapsed() < TOKEN_LIFETIME && data.user_data.get::<Granted>().is_some() {
            TokenAction::Raise
        } else {
            TokenAction::MarkUrgent
        }
    }

    /// A token for an application the compositor launches.
    pub(crate) fn launch_token(&mut self) -> String {
        let (token, data) = self.0.create_external_token(None);
        data.user_data.insert_if_missing(|| Granted);
        token.to_string()
    }

    /// A takeover or handback changes who drives the desktop; activations
    /// requested by the previous driver must not move focus afterwards.
    pub(crate) fn void_tokens(&mut self) {
        self.0.retain_tokens(|_, _| false);
    }
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        self.activation.state()
    }

    fn token_created(&mut self, _: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        let action = if self.granted_by_focus(&data) {
            TokenAction::Raise
        } else {
            TokenAction::MarkUrgent
        };
        self.activation.created(&data, action);
        true
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let action = self.activation.redeem(&token, &data);
        let Some(window) = self.toplevel_window(&surface) else {
            return;
        };
        tracing::debug!(?action, "xdg-activation request");
        if action == TokenAction::Raise {
            self.with_focus_cause(crate::delivery::FocusCause::Activation, |state| {
                state.activate_window(&window);
            });
        } else if !self.window_focused(&window) {
            self.windows.entry(window).or_default().mark_urgent();
            self.frame_request.request(crate::FrameRequest::FullRedraw);
        }
    }
}

impl State {
    /// The token carries a serial of this seat no older than the current
    /// keyboard focus, and the client that holds that focus created it.
    fn granted_by_focus(&self, data: &XdgActivationTokenData) -> bool {
        let Some((serial, seat)) = &data.serial else {
            return false;
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        let focused_client = keyboard
            .current_focus()
            .and_then(|focus| focus.wl_surface().and_then(|surface| surface.client()))
            .map(|client| client.id());
        Seat::<Self>::from_resource(seat).as_ref() == Some(&self.seat)
            && focused_client.is_some()
            && focused_client == data.client_id
            && keyboard
                .last_enter()
                .is_some_and(|enter| serial.is_no_older_than(&enter))
    }

    fn toplevel_window(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|top| top.wl_surface() == surface)
            })
            .cloned()
    }

    fn window_focused(&self, window: &Window) -> bool {
        let focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        focus.is_some() && focus == crate::focus::KeyboardFocus::window(window)
    }
}
