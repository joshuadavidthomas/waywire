//! The zwp-virtual-keyboard-v1 protocol.
//!
//! Virtual keyboards let standard tools such as `wtype` inject key events.
//! Smithay converts each request into an input-backend event; the handler below
//! sends key and modifier events to the same seat entry points as browser and
//! EI input.
//!
//! A client's key codes belong to the keymap it uploads, and they need not
//! be where a real keyboard has them. `wtype` builds a keymap per string and
//! gives its first distinct character evdev code 1, its second code 2, and so
//! on: the positions of Escape, the digit row, Backspace, Tab, Control and
//! F1 to F10. Clients that read the position as well as the keysym then act on
//! it. Chromium takes a key's DOM `code`, `keyCode` and editing commands from
//! the position, so a `:` on code 14 deletes the character before it as
//! Backspace, and a character on F5 reloads the page.
//!
//! The compositor therefore never activates a client's keymap. It decodes each
//! key with the client's keymap and the client's own modifiers, and presses the
//! key that has the same keysym in the seat's keymap, with the Shift or `AltGr`
//! that its level needs, as a real keyboard would. A keysym the seat's keymap
//! lacks goes on a spare keycode, as composed viewer text does. Clients keep
//! the seat's keymap, and a receipt follows each key the client sent. A client
//! that sends `no_keymap` already uses the seat's keycodes, so its keys pass
//! through.
//!
//! Key, modifier and keymap input from these devices shares the one ownership
//! model: a browser takeover releases held keys and drops later virtual-keyboard
//! input at the seat entry points in `lib.rs`.

use std::any::Any;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;

use smithay::backend::input::Event;
use smithay::backend::input::InputEvent;
use smithay::backend::input::KeyState;
use smithay::backend::input::KeyboardKeyEvent;
use smithay::input::keyboard::xkb;
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_v1;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_keyboard::KeymapFormat;
use smithay::wayland::virtual_keyboard::VirtualKeyboardBackend;
use smithay::wayland::virtual_keyboard::VirtualKeyboardDevice;
use smithay::wayland::virtual_keyboard::VirtualKeyboardHandler;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardSpecialEvent;

use crate::State;

/// Leave the NUL terminator out of a client keymap's size. `wtype` and other C
/// clients count it, as `wl_keyboard.keymap` does, and Smithay compiles the
/// buffer at that size, which libxkbcommon before 1.6 rejects. Runs on every
/// request before dispatch and changes only virtual-keyboard keymaps.
pub(crate) fn exclude_keymap_nul(request: &mut dyn Any) {
    let Some(zwp_virtual_keyboard_v1::Request::Keymap { format, fd, size }) =
        request.downcast_mut()
    else {
        return;
    };
    if *format != KeymapFormat::XkbV1 as u32 || *size == 0 {
        return;
    }
    let mut last = [0xff];
    let read = fd
        .try_clone()
        .map(File::from)
        .and_then(|file| file.read_exact_at(&mut last, u64::from(*size - 1)));
    if read.is_ok() && last == [0] {
        *size -= 1;
    }
}

/// Register the virtual keyboard manager global.
pub(crate) fn init(display_handle: &DisplayHandle) {
    // One trusted desktop session: local clients that can reach the socket, such
    // as `wtype`, may request a virtual keyboard without further screening.
    VirtualKeyboardManagerState::new::<State, _>(display_handle, |_client| true);
}

/// The key that types `keysym` in `keymap`: its evdev code, its layout, and
/// the modifiers its level needs, which are only ever Shift and `AltGr` (Mod5).
/// Keys in `layout` come first, then lower key codes. Only codes that X11
/// clients can receive are considered.
pub(crate) fn seat_key(
    keymap: &xkb::Keymap,
    keysym: xkb::Keysym,
    layout: u32,
) -> Option<(u32, u32, u32)> {
    let level_mods = [xkb::MOD_NAME_SHIFT, "Mod5"]
        .into_iter()
        .map(|name| 1u32.checked_shl(keymap.mod_get_index(name)).unwrap_or(0))
        .fold(0, |mask, bit| mask | bit);
    let layouts = keymap.num_layouts();
    (0..layouts)
        .map(|offset| (layout + offset) % layouts)
        .find_map(|group| {
            (8..=255).find_map(|code| {
                let key = xkb::Keycode::new(code);
                if group >= keymap.num_layouts_for_key(key) {
                    return None;
                }
                (0..keymap.num_levels_for_key(key, group)).find_map(|level| {
                    if !keymap
                        .key_get_syms_by_level(key, group, level)
                        .contains(&keysym)
                    {
                        return None;
                    }
                    let mut masks = [0; 32];
                    let count = keymap.key_get_mods_for_level(key, group, level, &mut masks);
                    masks[..count.min(masks.len())]
                        .iter()
                        .find(|mask| *mask & !level_mods == 0)
                        .map(|mask| (code - 8, group, *mask))
                })
            })
        })
}

/// `mask`, a set of `from`'s modifiers, as the same-named modifiers of `to`.
fn translate_mods(mask: u32, from: &xkb::Keymap, to: &xkb::Keymap) -> u32 {
    (0..from.num_mods().min(32))
        .filter(|index| mask & (1 << index) != 0)
        .filter_map(|index| 1u32.checked_shl(to.mod_get_index(from.mod_get_name(index))))
        .fold(0, |mask, bit| mask | bit)
}

/// Compile the keymap a client uploaded, or `None` when it uses the seat's.
fn client_state(device: &VirtualKeyboardDevice) -> Option<xkb::State> {
    let keymap = device.keymap()?;
    let mut text = Vec::new();
    let read = keymap.with_fd(false, |fd, size| {
        text.resize(size, 0);
        if let Err(error) = fd
            .try_clone_to_owned()
            .map(File::from)
            .and_then(|file| file.read_exact_at(&mut text, 0))
        {
            tracing::warn!(%error, "virtual keyboard keymap unreadable");
            text.clear();
        }
    });
    if let Err(error) = read {
        tracing::warn!(%error, "virtual keyboard keymap unreadable");
        return None;
    }
    while text.last() == Some(&0) {
        text.pop();
    }
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_string(
        &context,
        String::from_utf8_lossy(&text).into_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    Some(xkb::State::new(&keymap))
}

/// Virtual keyboards connected now.
#[derive(Default)]
pub(crate) struct VirtualKeyboards {
    devices: Vec<VirtualKeyboard>,
}

impl VirtualKeyboards {
    fn get_mut(&mut self, device: &VirtualKeyboardDevice) -> Option<&mut VirtualKeyboard> {
        self.devices
            .iter_mut()
            .find(|known| known.device == *device)
    }

    /// Track a new device. Its keys are dropped when it uploaded a keymap
    /// that does not compile.
    fn add(&mut self, device: VirtualKeyboardDevice) {
        let client = client_state(&device);
        if device.keymap().is_some() && client.is_none() {
            tracing::warn!("virtual keyboard keymap does not compile; its keys are dropped");
        }
        self.devices.push(VirtualKeyboard {
            device,
            client,
            held: HashMap::new(),
            modifiers: [0; 3],
        });
    }

    /// A new keymap from a connected client. Keys it holds stay held on the
    /// seat keys they pressed.
    fn rekey(&mut self, device: &VirtualKeyboardDevice) {
        let client = client_state(device);
        if let Some(keyboard) = self.get_mut(device) {
            keyboard.client = client;
        }
    }

    /// Stop tracking the device. Returns whether it had set seat modifiers,
    /// which no key release will clear.
    fn remove(&mut self, device: &VirtualKeyboardDevice) -> bool {
        let Some(index) = self
            .devices
            .iter()
            .position(|known| known.device == *device)
        else {
            return false;
        };
        self.devices.remove(index).modifiers != [0; 3]
    }
}

struct VirtualKeyboard {
    device: VirtualKeyboardDevice,
    /// The client's keymap, in the state its keys and `modifiers` requests
    /// have put it in. `None` when the client sends seat key codes.
    client: Option<xkb::State>,
    /// Seat keys this device holds down, by the client key that pressed them.
    held: HashMap<u32, u32>,
    /// The seat modifiers this device last set: depressed, latched, locked.
    modifiers: [u32; 3],
}

impl State {
    fn virtual_keyboard(&mut self, device: &VirtualKeyboardDevice) -> Option<&mut VirtualKeyboard> {
        self.virtual_keyboards.get_mut(device)
    }

    /// The device is gone: release what it holds and clear the modifiers it
    /// set, which no key release will clear.
    fn remove_virtual_keyboard(&mut self, device: &VirtualKeyboardDevice) {
        self.disconnect_input_source(device.source());
        if self.virtual_keyboards.remove(device) && !self.input.browser_holds_input() {
            self.set_seat_modifiers([0; 3]);
        }
    }

    /// Set the seat's modifier masks, keeping its layout, and tell the
    /// focused client.
    fn set_seat_modifiers(&mut self, [depressed, latched, locked]: [u32; 3]) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.with_xkb_state(self, |mut context| {
                let layout = context
                    .xkb()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .active_layout();
                context.set_modifier_mask(depressed, latched, locked, layout.0);
            });
        }
    }

    /// Give the seat the modifiers the client holds, under the seat's names,
    /// when they changed since this device last set them.
    fn follow_client_modifiers(&mut self, device: &VirtualKeyboardDevice) {
        if self.input.browser_holds_input() {
            return;
        }
        let Some(entry) = self.virtual_keyboard(device) else {
            return;
        };
        let Some(client) = &entry.client else {
            return;
        };
        let client_keymap = client.get_keymap();
        let client_mods = [
            xkb::STATE_MODS_DEPRESSED,
            xkb::STATE_MODS_LATCHED,
            xkb::STATE_MODS_LOCKED,
        ]
        .map(|component| client.serialize_mods(component));
        let previous = entry.modifiers;
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let modifiers = keyboard.with_xkb_state(self, |context| {
            let xkb = context
                .xkb()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: the reference does not outlive the lock.
            let keymap = unsafe { xkb.keymap() };
            client_mods.map(|mask| translate_mods(mask, &client_keymap, keymap))
        });
        if modifiers == previous {
            return;
        }
        if let Some(entry) = self.virtual_keyboard(device) {
            entry.modifiers = modifiers;
        }
        self.set_seat_modifiers(modifiers);
    }

    /// Press or release the seat key with the keysym that the client's key
    /// has, under the client's modifiers. A press sets the modifiers that
    /// key needs only for itself, then puts the seat's back.
    fn virtual_key(&mut self, device: &VirtualKeyboardDevice, key: u32, key_state: KeyState) {
        let source = device.source();
        let Some(entry) = self.virtual_keyboard(device) else {
            return;
        };
        let Some(client) = &mut entry.client else {
            if device.keymap().is_none() {
                self.keyboard_key(source, key, key_state);
            }
            return;
        };
        let code = xkb::Keycode::new(key + 8);
        if key_state == KeyState::Released {
            client.update_key(code, xkb::KeyDirection::Up);
            let keysym = client.key_get_one_sym(code);
            if let Some(seat_key) = entry.held.remove(&key) {
                self.keyboard_symbol_key(source, seat_key, KeyState::Released);
            }
            if keysym.is_modifier_key() {
                self.follow_client_modifiers(device);
            }
            return;
        }
        if let Some(&seat_key) = entry.held.get(&key) {
            // A repeated press of a held key.
            self.keyboard_symbol_key(source, seat_key, KeyState::Pressed);
            return;
        }
        self.virtual_key_press(device, key);
    }

    fn virtual_key_press(&mut self, device: &VirtualKeyboardDevice, key: u32) {
        let source = device.source();
        let Some(client) = self
            .virtual_keyboard(device)
            .and_then(|entry| entry.client.as_mut())
        else {
            return;
        };
        let code = xkb::Keycode::new(key + 8);
        let keysym = client.key_get_one_sym(code);
        let consumed = client.key_get_consumed_mods(code);
        let client_mods = [
            xkb::STATE_MODS_DEPRESSED,
            xkb::STATE_MODS_LATCHED,
            xkb::STATE_MODS_LOCKED,
        ]
        .map(|component| client.serialize_mods(component) & !consumed);
        let client_keymap = client.get_keymap();
        client.update_key(code, xkb::KeyDirection::Down);
        if keysym == xkb::Keysym::NoSymbol {
            tracing::debug!(key, "virtual key has no symbol in its keymap");
            return;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let (found, seat_mods, layout, translated) = keyboard.with_xkb_state(self, |context| {
            let xkb = context
                .xkb()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: the reference does not outlive the lock.
            let keymap = unsafe { xkb.keymap() };
            let layout = xkb.active_layout().0;
            // SAFETY: as above.
            let state = unsafe { xkb.state() };
            let seat_mods = [
                xkb::STATE_MODS_DEPRESSED,
                xkb::STATE_MODS_LATCHED,
                xkb::STATE_MODS_LOCKED,
            ]
            .map(|component| state.serialize_mods(component));
            let translated = client_mods.map(|mask| translate_mods(mask, &client_keymap, keymap));
            (
                seat_key(keymap, keysym, layout),
                seat_mods,
                layout,
                translated,
            )
        });
        let found = found.or_else(|| {
            let character = keysym.key_char().filter(|c| !c.is_control())?;
            let code = self.text_keycode(character)?;
            Some((code - 8, layout, 0))
        });
        let Some((seat_key, group, level)) = found else {
            tracing::debug!(keysym = keysym.raw(), "virtual keysym has no seat key");
            return;
        };
        if let Some(entry) = self.virtual_keyboard(device) {
            entry.held.insert(key, seat_key);
        }
        if keysym.is_modifier_key() {
            self.keyboard_symbol_key(source, seat_key, KeyState::Pressed);
            self.follow_client_modifiers(device);
            return;
        }
        let [depressed, latched, locked] = translated;
        let wanted = [depressed | level, latched, locked];
        let change = !self.input.browser_holds_input() && (wanted != seat_mods || group != layout);
        if change && let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.with_xkb_state(self, |mut context| {
                context.set_modifier_mask(wanted[0], wanted[1], wanted[2], group);
            });
        }
        self.keyboard_symbol_key(source, seat_key, KeyState::Pressed);
        if change && let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.with_xkb_state(self, |mut context| {
                context.set_modifier_mask(seat_mods[0], seat_mods[1], seat_mods[2], layout);
            });
        }
    }
}

impl VirtualKeyboardHandler for State {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        match event {
            InputEvent::DeviceAdded { device } => {
                self.describe_client_source(
                    device.source(),
                    crate::delivery::SourceKind::VirtualKeyboard,
                    device.client(),
                );
                self.register_agent_source(device.source());
                self.virtual_keyboards.add(device);
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::KeymapChanged { device }) => {
                self.virtual_keyboards.rekey(&device);
            }
            InputEvent::DeviceRemoved { device } => {
                self.remove_virtual_keyboard(&device);
            }
            InputEvent::Keyboard { event } => {
                // The event's key code is the protocol key plus the XKB offset.
                // Codes below the offset are not keys, as in the EI adapter.
                let Some(key) = u32::from(event.key_code()).checked_sub(8) else {
                    tracing::debug!(
                        key_code = u32::from(event.key_code()),
                        "dropped virtual key code below the evdev key range"
                    );
                    return;
                };
                self.virtual_key(&event.device(), key, event.state());
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::Modifiers {
                device,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            }) => {
                let Some(entry) = self.virtual_keyboard(&device) else {
                    return;
                };
                match &mut entry.client {
                    Some(client) => {
                        client.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                        self.follow_client_modifiers(&device);
                    }
                    None => self.keyboard_modifiers(
                        device.source(),
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        group,
                    ),
                }
            }
            // This backend only produces keyboard events.
            InputEvent::PointerMotion { .. }
            | InputEvent::PointerMotionAbsolute { .. }
            | InputEvent::PointerButton { .. }
            | InputEvent::PointerAxis { .. }
            | InputEvent::GestureSwipeBegin { .. }
            | InputEvent::GestureSwipeUpdate { .. }
            | InputEvent::GestureSwipeEnd { .. }
            | InputEvent::GesturePinchBegin { .. }
            | InputEvent::GesturePinchUpdate { .. }
            | InputEvent::GesturePinchEnd { .. }
            | InputEvent::GestureHoldBegin { .. }
            | InputEvent::GestureHoldEnd { .. }
            | InputEvent::TouchDown { .. }
            | InputEvent::TouchMotion { .. }
            | InputEvent::TouchUp { .. }
            | InputEvent::TouchCancel { .. }
            | InputEvent::TouchFrame { .. }
            | InputEvent::TabletToolAxis { .. }
            | InputEvent::TabletToolProximity { .. }
            | InputEvent::TabletToolTip { .. }
            | InputEvent::TabletToolButton { .. }
            | InputEvent::SwitchToggle { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::AsFd;

    use smithay::input::keyboard::xkb;
    use smithay::reexports::wayland_server::Display;
    use wayland_client::Connection;
    use wayland_client::Dispatch;
    use wayland_client::EventQueue;
    use wayland_client::QueueHandle;
    use wayland_client::protocol::wl_registry;
    use wayland_client::protocol::wl_seat::WlSeat;
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

    use crate::State;

    #[derive(Default)]
    struct App {
        seat: Option<WlSeat>,
        manager: Option<ZwpVirtualKeyboardManagerV1>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for App {
        fn event(
            app: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            (): &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name, interface, ..
            } = event
            {
                match interface.as_str() {
                    "wl_seat" => app.seat = Some(registry.bind(name, 1, qh, ())),
                    "zwp_virtual_keyboard_manager_v1" => {
                        app.manager = Some(registry.bind(name, 1, qh, ()));
                    }
                    _ => {}
                }
            }
        }
    }

    wayland_client::delegate_noop!(App: ignore WlSeat);
    wayland_client::delegate_noop!(App: ZwpVirtualKeyboardManagerV1);
    wayland_client::delegate_noop!(App: ZwpVirtualKeyboardV1);

    fn round_trip(
        display: &mut Display<State>,
        state: &mut State,
        connection: &Connection,
        queue: &mut EventQueue<App>,
        app: &mut App,
    ) {
        for _ in 0..2 {
            connection.flush().expect("client flush");
            display.dispatch_clients(state).expect("server dispatch");
            display.handle().flush_clients().expect("server flush");
            if let Some(guard) = connection.prepare_read() {
                let mut poll = [nix::poll::PollFd::new(
                    guard.connection_fd(),
                    nix::poll::PollFlags::POLLIN,
                )];
                if nix::poll::poll(&mut poll, nix::poll::PollTimeout::from(0u8)).unwrap_or(-1) > 0 {
                    match guard.read() {
                        Ok(_) => {}
                        Err(wayland_client::backend::WaylandError::Io(error))
                            if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(error) => panic!("client read: {error}"),
                    }
                }
            }
            queue.dispatch_pending(app).expect("client dispatch");
        }
    }

    /// Create a virtual keyboard that uploads its own keymap. With
    /// `trailing_nul` the size counts a NUL terminator, as `wtype` sends it.
    fn keyboard_with_keymap(
        app: &App,
        qh: &QueueHandle<App>,
        layout: &str,
        trailing_nul: bool,
    ) -> ZwpVirtualKeyboardV1 {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(&context, "", "", layout, "", None, 0)
            .expect("test keymap compiles");
        let mut text = keymap
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
            .into_bytes();
        if trailing_nul {
            text.push(0);
        }
        let mut file = tempfile::tempfile().expect("keymap file");
        file.write_all(&text).expect("keymap written");
        let keyboard = app
            .manager
            .as_ref()
            .expect("virtual keyboard manager bound")
            .create_virtual_keyboard(app.seat.as_ref().expect("seat bound"), qh, ());
        keyboard.keymap(
            1,
            file.as_fd(),
            u32::try_from(text.len()).expect("keymap length fits"),
        );
        keyboard
    }

    fn connect(
        display: &mut Display<State>,
        state: &mut State,
    ) -> (Connection, EventQueue<App>, App) {
        let (client_socket, server_socket) =
            std::os::unix::net::UnixStream::pair().expect("socket pair");
        display
            .handle()
            .insert_client(
                server_socket,
                std::sync::Arc::new(crate::shell::ClientState::default()),
            )
            .expect("insert client");
        let connection = Connection::from_socket(client_socket).expect("client connection");
        let mut queue: EventQueue<App> = connection.new_event_queue();
        let mut app = App::default();
        connection.display().get_registry(&queue.handle(), ());
        round_trip(display, state, &connection, &mut queue, &mut app);
        (connection, queue, app)
    }

    /// `wtype` counts the keymap's NUL terminator in its size, which
    /// libxkbcommon before 1.6 refuses to compile from a buffer. The keymap
    /// still decodes the keys, and they arrive as the seat's keys: the key a
    /// German keymap has `z` on presses the seat's Z key.
    #[test]
    fn a_keymap_size_counting_its_nul_terminator_types_on_the_seat_keymap() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect(&mut display, &mut state);
        let qh = queue.handle();
        let seat_keymap = state
            .seat
            .get_keyboard()
            .expect("keyboard")
            .keymap_file()
            .id();
        let keyboard = keyboard_with_keymap(&app, &qh, "de", true);
        keyboard.key(0, 21, 1);
        round_trip(&mut display, &mut state, &connection, &mut queue, &mut app);
        let seat = state.seat.get_keyboard().expect("keyboard");
        assert_eq!(
            seat.keymap_file().id(),
            seat_keymap,
            "the seat keeps its keymap"
        );
        assert!(
            seat.pressed_keys().contains(&(44u32 + 8).into()),
            "Z is down"
        );
        assert!(
            !seat.pressed_keys().contains(&(21u32 + 8).into()),
            "Y is not"
        );
    }

    /// Modifiers a client sets with the `modifiers` request reach the seat
    /// under the seat's names, and end when the keyboard goes away.
    #[test]
    fn client_modifiers_reach_the_seat_and_end_with_the_keyboard() {
        let (mut display, mut state) = crate::tests::test_state();
        let (connection, mut queue, mut app) = connect(&mut display, &mut state);
        let qh = queue.handle();
        let keyboard = keyboard_with_keymap(&app, &qh, "us", false);
        // Control, as `wtype -M ctrl` sends it.
        keyboard.modifiers(4, 0, 0, 0);
        round_trip(&mut display, &mut state, &connection, &mut queue, &mut app);
        let seat = state.seat.get_keyboard().expect("keyboard");
        assert!(seat.modifier_state().ctrl);
        keyboard.destroy();
        round_trip(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert!(!seat.modifier_state().ctrl);
    }
}
