//! The zwp-virtual-keyboard-v1 protocol.
//!
//! Virtual keyboards let standard tools such as `wtype` inject key events. The
//! manager global only creates the client-side objects; Smithay routes every
//! key, modifier and keymap request through the seat's regular keyboard
//! handle, so virtual keyboard input reaches the focused window through the
//! same keyboard path as browser and EI input.
//!
//! Control-ownership arbitration for this input source is planned in
//! ROADMAP.md phase 2 and is intentionally not part of this change.

use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;

use crate::State;

/// Register the virtual keyboard manager global.
pub(crate) fn init(display_handle: &DisplayHandle) {
    // One trusted desktop session: local clients that can reach the socket, such
    // as `wtype`, may request a virtual keyboard without further screening.
    VirtualKeyboardManagerState::new::<State, _>(display_handle, |_client| true);
}
