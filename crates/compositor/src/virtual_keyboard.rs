//! The zwp-virtual-keyboard-v1 protocol.
//!
//! Virtual keyboards let standard tools such as `wtype` inject key events.
//! Smithay converts each request into an input-backend event; the handler below
//! sends key and modifier events to the same seat entry points as browser and
//! EI input.
//!
//! Smithay also hands the compositor the client's keymap. Activating it on the
//! seat before routing keys is required, because the keycodes belong to that
//! keymap. With several virtual keyboards the newest one's keymap is active,
//! and the seat's own keymap returns when the last one goes away.
//!
//! Control-ownership arbitration for this input source is planned in
//! ROADMAP.md phase 2 and is intentionally not part of this change.

use smithay::backend::input::Event;
use smithay::backend::input::InputEvent;
use smithay::backend::input::KeyboardKeyEvent;
use smithay::input::keyboard::KeymapFile;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::virtual_keyboard::VirtualKeyboardBackend;
use smithay::wayland::virtual_keyboard::VirtualKeyboardDevice;
use smithay::wayland::virtual_keyboard::VirtualKeyboardHandler;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardSpecialEvent;

use crate::State;

/// Register the virtual keyboard manager global.
pub(crate) fn init(display_handle: &DisplayHandle) {
    // One trusted desktop session: local clients that can reach the socket, such
    // as `wtype`, may request a virtual keyboard without further screening.
    VirtualKeyboardManagerState::new::<State, _>(display_handle, |_client| true);
}

/// Virtual keyboards connected now, and the seat keymap to restore after the
/// last one leaves. The most recently connected or re-keymapped device's
/// keymap is the active one, because its keycodes are the ones arriving.
#[derive(Default)]
pub(crate) struct VirtualKeyboards {
    devices: Vec<VirtualKeyboardDevice>,
    seat_keymap: Option<KeymapFile>,
}

impl State {
    /// Make `device` the newest virtual keyboard and activate its keymap.
    fn activate_virtual_keymap(&mut self, device: VirtualKeyboardDevice) {
        let (Some(keymap), Some(keyboard)) = (device.keymap(), self.seat.get_keyboard()) else {
            return;
        };
        let keyboards = &mut self.virtual_keyboards;
        keyboards.devices.retain(|known| *known != device);
        keyboards.devices.push(device);
        keyboards
            .seat_keymap
            .get_or_insert_with(|| keyboard.keymap_file());
        if let Err(error) = keyboard.set_keymap_from_keymap_file(self, keymap) {
            tracing::warn!(%error, "virtual keyboard keymap activation failed");
        }
    }

    /// Drop `device` and activate the keymap that applies without it: the
    /// next newest virtual keyboard's, or the seat's own after the last.
    fn remove_virtual_keyboard(&mut self, device: &VirtualKeyboardDevice) {
        let keyboards = &mut self.virtual_keyboards;
        let Some(index) = keyboards.devices.iter().position(|known| known == device) else {
            return;
        };
        let was_active = index + 1 == keyboards.devices.len();
        keyboards.devices.remove(index);
        if !was_active {
            return;
        }
        let keymap = match keyboards.devices.last() {
            Some(next) => next.keymap(),
            None => keyboards.seat_keymap.take(),
        };
        if let (Some(keymap), Some(keyboard)) = (keymap, self.seat.get_keyboard())
            && let Err(error) = keyboard.set_keymap_from_keymap_file(self, keymap)
        {
            tracing::warn!(%error, "virtual keyboard keymap restore failed");
        }
    }
}

impl VirtualKeyboardHandler for State {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        match event {
            InputEvent::DeviceAdded { device }
            | InputEvent::Special(VirtualKeyboardSpecialEvent::KeymapChanged { device }) => {
                self.activate_virtual_keymap(device);
            }
            InputEvent::DeviceRemoved { device } => {
                // Release held keys while the device's keymap is still active.
                self.release_input(device.source());
                self.remove_virtual_keyboard(&device);
            }
            InputEvent::Keyboard { event } => {
                // The event's key code is the protocol key plus the XKB offset;
                // the shared entry point expects the evdev key. Codes below the
                // offset are not keys, as in the EI adapter.
                let Some(key) = u32::from(event.key_code()).checked_sub(8) else {
                    tracing::debug!(
                        key_code = u32::from(event.key_code()),
                        "dropped virtual key code below the evdev key range"
                    );
                    return;
                };
                self.keyboard_key(event.device().source(), key, event.state());
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            }) => {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.with_xkb_state(self, |mut context| {
                        context.set_modifier_mask(mods_depressed, mods_latched, mods_locked, group);
                    });
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

    /// Create a virtual keyboard that uploads its own keymap.
    fn keyboard_with_keymap(
        app: &App,
        qh: &QueueHandle<App>,
        layout: &str,
    ) -> ZwpVirtualKeyboardV1 {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(&context, "", "", layout, "", None, 0)
            .expect("test keymap compiles");
        let text = keymap
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
            .into_bytes();
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

    #[test]
    fn the_seat_keymap_returns_after_overlapping_virtual_keyboards_leave() {
        let (mut display, mut state) = crate::tests::test_state();
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
        let qh = queue.handle();
        let mut app = App::default();
        connection.display().get_registry(&qh, ());
        round_trip(&mut display, &mut state, &connection, &mut queue, &mut app);

        let keymap_id = |state: &State| {
            state
                .seat
                .get_keyboard()
                .expect("seat keyboard")
                .keymap_file()
                .id()
        };
        let seat_keymap = keymap_id(&state);

        let first = keyboard_with_keymap(&app, &qh, "de");
        round_trip(&mut display, &mut state, &connection, &mut queue, &mut app);
        let second = keyboard_with_keymap(&app, &qh, "fr");
        round_trip(&mut display, &mut state, &connection, &mut queue, &mut app);
        let second_keymap = keymap_id(&state);
        assert_ne!(
            second_keymap, seat_keymap,
            "the newest keyboard's keymap is active"
        );

        // The older keyboard leaving must not disturb the one still typing.
        first.destroy();
        round_trip(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(keymap_id(&state), second_keymap);

        second.destroy();
        round_trip(&mut display, &mut state, &connection, &mut queue, &mut app);
        assert_eq!(
            keymap_id(&state),
            seat_keymap,
            "the seat keymap is restored"
        );
    }
}
