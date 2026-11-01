//! Keyboard targets preserve X11's input-model focus negotiation; pointer
//! targets keep X11 windows distinct for drag-and-drop.
use std::borrow::Cow;

use smithay::backend::input::InputTime;
use smithay::backend::input::KeyState;
use smithay::desktop::Window;
use smithay::input::Seat;
use smithay::input::keyboard::KeyboardTarget;
use smithay::input::keyboard::KeysymHandle;
use smithay::input::keyboard::ModifiersState;
use smithay::input::pointer::AxisFrame;
use smithay::input::pointer::ButtonEvent;
use smithay::input::pointer::GestureHoldBeginEvent;
use smithay::input::pointer::GestureHoldEndEvent;
use smithay::input::pointer::GesturePinchBeginEvent;
use smithay::input::pointer::GesturePinchEndEvent;
use smithay::input::pointer::GesturePinchUpdateEvent;
use smithay::input::pointer::GestureSwipeBeginEvent;
use smithay::input::pointer::GestureSwipeEndEvent;
use smithay::input::pointer::GestureSwipeUpdateEvent;
use smithay::input::pointer::MotionEvent;
use smithay::input::pointer::PointerTarget;
use smithay::input::pointer::RelativeMotionEvent;
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
/// Pointer targets keep X11 windows distinct from their Wayland surfaces, so
/// a drag-and-drop offer to an X11 window goes through the XWM's XDND bridge
/// instead of `wl_data_device`, which Xwayland does not read.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "direct Smithay focus targets avoid allocation on every pointer focus transition"
)]
pub(crate) enum PointerFocus {
    Wayland(WlSurface),
    X11(X11Surface),
}
impl From<KeyboardFocus> for PointerFocus {
    fn from(focus: KeyboardFocus) -> Self {
        match focus {
            KeyboardFocus::Wayland(surface) => Self::Wayland(surface),
            KeyboardFocus::X11(surface) => Self::X11(surface),
        }
    }
}
impl PointerFocus {
    fn target(&self) -> &dyn PointerTarget<State> {
        match self {
            Self::Wayland(s) => s,
            Self::X11(s) => s,
        }
    }
    /// Whether this target delivers its input to `surface`.
    pub(crate) fn is(&self, surface: &WlSurface) -> bool {
        self.wl_surface().is_some_and(|own| *own == *surface)
    }
}
impl IsAlive for PointerFocus {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            Self::X11(s) => s.alive(),
        }
    }
}
impl WaylandFocus for PointerFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(Cow::Borrowed(s)),
            Self::X11(s) => s.wl_surface().map(Cow::Owned),
        }
    }
}
impl PointerTarget<State> for PointerFocus {
    fn enter(&self, seat: &Seat<State>, state: &mut State, event: &MotionEvent) {
        self.target().enter(seat, state, event);
    }
    fn motion(&self, seat: &Seat<State>, state: &mut State, event: &MotionEvent) {
        self.target().motion(seat, state, event);
    }
    fn relative_motion(&self, seat: &Seat<State>, state: &mut State, event: &RelativeMotionEvent) {
        self.target().relative_motion(seat, state, event);
    }
    fn button(&self, seat: &Seat<State>, state: &mut State, event: &ButtonEvent) {
        self.target().button(seat, state, event);
    }
    fn axis(&self, seat: &Seat<State>, state: &mut State, frame: AxisFrame) {
        self.target().axis(seat, state, frame);
    }
    fn frame(&self, seat: &Seat<State>, state: &mut State) {
        self.target().frame(seat, state);
    }
    fn gesture_swipe_begin(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        event: &GestureSwipeBeginEvent,
    ) {
        self.target().gesture_swipe_begin(seat, state, event);
    }
    fn gesture_swipe_update(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        event: &GestureSwipeUpdateEvent,
    ) {
        self.target().gesture_swipe_update(seat, state, event);
    }
    fn gesture_swipe_end(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        event: &GestureSwipeEndEvent,
    ) {
        self.target().gesture_swipe_end(seat, state, event);
    }
    fn gesture_pinch_begin(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        event: &GesturePinchBeginEvent,
    ) {
        self.target().gesture_pinch_begin(seat, state, event);
    }
    fn gesture_pinch_update(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        event: &GesturePinchUpdateEvent,
    ) {
        self.target().gesture_pinch_update(seat, state, event);
    }
    fn gesture_pinch_end(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        event: &GesturePinchEndEvent,
    ) {
        self.target().gesture_pinch_end(seat, state, event);
    }
    fn gesture_hold_begin(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        event: &GestureHoldBeginEvent,
    ) {
        self.target().gesture_hold_begin(seat, state, event);
    }
    fn gesture_hold_end(&self, seat: &Seat<State>, state: &mut State, event: &GestureHoldEndEvent) {
        self.target().gesture_hold_end(seat, state, event);
    }
    fn leave(&self, seat: &Seat<State>, state: &mut State, serial: Serial, time: InputTime) {
        self.target().leave(seat, state, serial, time);
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
