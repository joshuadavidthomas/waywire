//! Keyboard targets preserve X11's input-model focus negotiation.
use std::borrow::Cow;

use smithay::backend::input::InputTime;
use smithay::backend::input::KeyState;
use smithay::desktop::Window;
use smithay::input::Seat;
use smithay::input::keyboard::KeyboardTarget;
use smithay::input::keyboard::KeysymHandle;
use smithay::input::keyboard::ModifiersState;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::IsAlive;
use smithay::utils::Serial;
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use super::State;

#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "direct Smithay focus targets avoid allocation on every input focus transition"
)]
pub(crate) enum KeyboardFocus {
    Wayland(WlSurface),
    X11(X11Surface),
}
impl From<WlSurface> for KeyboardFocus {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}
impl From<smithay::desktop::PopupKind> for KeyboardFocus {
    fn from(popup: smithay::desktop::PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}
impl From<KeyboardFocus> for WlSurface {
    #[expect(
        clippy::expect_used,
        reason = "PopupManager only converts mapped popup targets with an associated wl_surface"
    )]
    fn from(focus: KeyboardFocus) -> Self {
        focus
            .wl_surface()
            .expect("popup grab target has a Wayland surface")
            .into_owned()
    }
}
impl KeyboardFocus {
    pub(crate) fn window(window: &Window) -> Option<Self> {
        if let Some(surface) = window.x11_surface() {
            Some(Self::X11(surface.clone()))
        } else {
            window.wl_surface().map(|s| Self::Wayland(s.into_owned()))
        }
    }
    fn target(&self) -> &dyn KeyboardTarget<State> {
        match self {
            Self::Wayland(s) => s,
            Self::X11(s) => s,
        }
    }
}
impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            Self::X11(s) => s.alive(),
        }
    }
}
impl WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(Cow::Borrowed(s)),
            Self::X11(s) => s.wl_surface().map(Cow::Owned),
        }
    }
}
impl KeyboardTarget<State> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.target().enter(seat, state, keys, serial);
    }
    fn leave(&self, seat: &Seat<State>, state: &mut State, serial: Serial) {
        self.target().leave(seat, state, serial);
    }
    fn key(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        key: KeysymHandle<'_>,
        key_state: KeyState,
        serial: Serial,
        time: InputTime,
    ) {
        self.target().key(seat, state, key, key_state, serial, time);
    }
    fn modifiers(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        self.target().modifiers(seat, state, modifiers, serial);
    }
}
