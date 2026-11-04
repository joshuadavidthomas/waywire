//! The calloop side of the xdg-desktop-portal backends that `waywire_portal`
//! serves on the private session bus: portal calls that need the seat, the
//! EIS server or the rendered frame arrive here as `PortalRequest`s.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::rc::Rc;

use anyhow::Result;
use calloop::LoopHandle;
use calloop::channel::Event;
use smithay::backend::input::Axis;
use smithay::backend::input::AxisSource;
use smithay::backend::input::KeyState;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::keyboard::Layout;
use smithay::input::keyboard::xkb;
use smithay::input::pointer::AxisFrame;
use smithay::reexports::pixman::Image;
use waywire_portal::PortalDirs;
use waywire_portal::PortalInput;
use waywire_portal::PortalRequest;
use waywire_portal::PortalService;
use waywire_portal::write_portal_files;

use crate::State;
use crate::capture::image_pixels;
use crate::delivery::SourceInfo;
use crate::delivery::SourceKind;

/// Serve the portal backends on `bus_address` for the lifetime of the
/// compositor. Returns the published directories, or `None` when the backends
/// could not be started; the desktop itself still runs.
pub(crate) fn start(
    handle: &LoopHandle<'static, State>,
    runtime_path: &Path,
    bus_address: &str,
    image: Rc<RefCell<Image<'static, 'static>>>,
    layout: String,
) -> Result<Option<PortalDirs>> {
    let dirs = write_portal_files(runtime_path)?;
    let screenshots_dir = runtime_path.join("screenshots");
    let (requests, incoming) = calloop::channel::channel::<PortalRequest>();
    let loop_handle = handle.clone();
    if let Err(error) = handle.insert_source(incoming, move |request, (), state| match request {
        Event::Msg(request) => match request {
            PortalRequest::RegisterSource { source, owner } => {
                state
                    .delivery
                    .describe(source, SourceInfo::new(SourceKind::Portal, owner));
                state.register_agent_source(source);
            }
            PortalRequest::DisconnectSource { source } => state.disconnect_input_source(source),
            PortalRequest::Input(input) => apply_portal_input(state, input),
            PortalRequest::Keysym {
                source,
                keysym,
                state: key_state,
                reply,
            } => {
                if reply
                    .send(apply_keysym(state, source, keysym, key_state))
                    .is_err()
                {
                    tracing::debug!("portal requester hung up");
                }
            }
            PortalRequest::OutputSize { reply } => {
                let size = state
                    .space
                    .output_geometry(&state.output)
                    .map(|rect| (rect.size.w, rect.size.h));
                if reply.send(size).is_err() {
                    tracing::debug!("portal requester hung up");
                }
            }
            PortalRequest::StartCast { session, reply } => {
                let size = (state.size.width(), state.size.height());
                let fps = state.stream.quality().fps.get();
                match state.screencast.as_mut() {
                    Some(streams) => streams.start(session, size, fps, reply),
                    None => {
                        if reply.send(None).is_err() {
                            tracing::debug!("portal requester hung up");
                        }
                    }
                }
            }
            PortalRequest::CloseCast { session } => {
                if let Some(streams) = state.screencast.as_mut() {
                    streams.close(&session);
                }
            }
            PortalRequest::ConnectToEis { reply, owner } => {
                if let Err(error) = reply.send(connect_to_eis(&loop_handle, &layout, owner)) {
                    tracing::debug!(?error, "portal requester hung up");
                }
            }
            PortalRequest::Screenshot { reply } => {
                let image = image.borrow();
                let screenshot = waywire_portal::write_screenshot(
                    image_pixels(&image),
                    image.width(),
                    image.height(),
                    image.stride(),
                    &screenshots_dir,
                );
                if let Err(error) = reply.send(screenshot) {
                    tracing::debug!(?error, "portal requester hung up");
                }
            }
        },
        Event::Closed => {
            // The portal thread is gone; nothing else can send on the channel.
        }
    }) {
        // Without the request channel the backends cannot answer the calls
        // that need the seat or the frame; the desktop itself still starts.
        tracing::error!(?error, "portal request channel failed to install");
        return Ok(None);
    }
    match waywire_portal::serve(requests, &dirs, bus_address)? {
        PortalService::Serving => Ok(Some(dirs)),
        PortalService::Unavailable => Ok(None),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the request channel hands over each input by value"
)]
fn apply_portal_input(state: &mut State, input: PortalInput) {
    match input {
        PortalInput::Motion { source, dx, dy } => {
            if let Some(pointer) = state.seat.get_pointer() {
                let delta = (dx, dy).into();
                state.pointer_motion_with_delta(
                    source,
                    pointer.current_location() + delta,
                    Some(delta),
                );
            }
        }
        PortalInput::Absolute { source, x, y } => {
            if let Some(rect) = state.space.output_geometry(&state.output) {
                let x = x.clamp(0.0, f64::from(rect.size.w.saturating_sub(1)));
                let y = y.clamp(0.0, f64::from(rect.size.h.saturating_sub(1)));
                state.pointer_motion(
                    source,
                    (x + f64::from(rect.loc.x), y + f64::from(rect.loc.y)).into(),
                );
            }
        }
        PortalInput::Button {
            source,
            button,
            state: button_state,
        } => state.pointer_button(source, button, button_state),
        PortalInput::Axis { source, dx, dy } => {
            let frame = AxisFrame::new(crate::InputTime::now())
                .source(AxisSource::Continuous)
                .value(Axis::Horizontal, dx)
                .value(Axis::Vertical, dy);
            state.pointer_axis(source, frame);
        }
        PortalInput::Discrete {
            source,
            axis,
            steps,
        } => {
            let frame = AxisFrame::new(crate::InputTime::now())
                .source(AxisSource::Wheel)
                .v120(axis, steps.saturating_mul(120))
                .value(axis, f64::from(steps) * 15.0);
            state.pointer_axis(source, frame);
        }
        PortalInput::Keycode {
            source,
            key,
            state: key_state,
        } => state.keyboard_key(source, key, key_state),
    }
}

fn apply_keysym(
    state: &mut State,
    source: KeyboardSource,
    keysym: u32,
    key_state: KeyState,
) -> Result<(), String> {
    let keyboard = state.seat.get_keyboard().ok_or("seat has no keyboard")?;
    let (text, original_layout) = keyboard.with_xkb_state(state, |context| {
        let xkb = context
            .xkb()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (xkb.keymap_as_string(), xkb.active_layout())
    });
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_string(
        &context,
        text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or("could not load active keymap")?;
    let shift_index = keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
    let altgr_index = keymap.mod_get_index("Mod5");
    let shift_mask = 1u32.checked_shl(shift_index).unwrap_or(0);
    let altgr_mask = 1u32.checked_shl(altgr_index).unwrap_or(0);
    let found =
        crate::virtual_keyboard::seat_key(&keymap, xkb::Keysym::new(keysym), original_layout.0);
    let (key, group, mask) =
        found.ok_or_else(|| format!("keysym {keysym:#x} is not available in the keymap"))?;
    if state.input.browser_holds_input() {
        state.keyboard_key(source, key, key_state);
        return Ok(());
    }
    if group != original_layout.0 {
        keyboard.with_xkb_state(state, |mut context| context.set_layout(Layout(group)));
    }
    let modifiers = [(42, shift_mask), (100, altgr_mask)];
    if key_state == KeyState::Pressed {
        for (modifier, bit) in modifiers {
            if mask & bit != 0 {
                state.keyboard_key(source, modifier, key_state);
            }
        }
        state.keyboard_key(source, key, key_state);
    } else {
        state.keyboard_key(source, key, key_state);
        for (modifier, bit) in modifiers.into_iter().rev() {
            if mask & bit != 0 {
                state.keyboard_key(source, modifier, key_state);
            }
        }
    }
    if group != original_layout.0 {
        keyboard.with_xkb_state(state, |mut context| context.set_layout(original_layout));
    }
    Ok(())
}

/// Runs in the calloop loop, which owns the seat and the EIS server: creates
/// the socket pair `ConnectToEIS` hands out. One end joins the existing EIS
/// server as a new client; the caller gets the other end and a duplicate of
/// the server end for closing the session.
fn connect_to_eis(
    handle: &smithay::reexports::calloop::LoopHandle<'static, crate::State>,
    layout: &str,
    owner: Option<u32>,
) -> std::io::Result<(OwnedFd, UnixStream)> {
    use smithay::reexports::reis::eis;

    let (client, server) = UnixStream::pair()?;
    let server_handle = server.try_clone()?;
    let context = eis::Context::new(server)?;
    crate::eis::serve_connection(
        handle,
        context,
        layout.to_owned(),
        SourceInfo::new(SourceKind::PortalEis, owner),
    )
    .map_err(std::io::Error::other)?;
    Ok((client.into(), server_handle))
}

#[cfg(test)]
mod tests {
    use smithay::backend::input::ButtonState;

    use super::*;

    #[test]
    fn pointer_input_uses_output_coordinates_and_obeys_lease() {
        let (_display, mut state) = crate::tests::test_state();
        let source = KeyboardSource::new_auxiliary();
        state.register_agent_source(source);
        let pointer = state.seat.get_pointer().expect("pointer");
        apply_portal_input(
            &mut state,
            PortalInput::Absolute {
                source,
                x: 311.0,
                y: 97.0,
            },
        );
        assert_eq!(pointer.current_location(), (311.0, 97.0).into());
        apply_portal_input(
            &mut state,
            PortalInput::Absolute {
                source,
                x: 5000.0,
                y: -20.0,
            },
        );
        assert_eq!(pointer.current_location(), (1919.0, 0.0).into());
        apply_portal_input(
            &mut state,
            PortalInput::Button {
                source,
                button: 0x110,
                state: ButtonState::Pressed,
            },
        );
        assert!(
            state
                .input
                .buttons(source)
                .is_some_and(|buttons| buttons.contains(&0x110))
        );
        state.set_browser_lease(true);
        apply_portal_input(
            &mut state,
            PortalInput::Button {
                source,
                button: 0x110,
                state: ButtonState::Pressed,
            },
        );
        apply_portal_input(
            &mut state,
            PortalInput::Absolute {
                source,
                x: 311.0,
                y: 97.0,
            },
        );
        assert!(
            state
                .input
                .buttons(source)
                .is_none_or(std::collections::HashSet::is_empty)
        );
        assert_eq!(pointer.current_location(), (1919.0, 0.0).into());
    }

    /// Sends `keys` as presses from a keyboard on a real EI connection.
    fn ei_key_presses(stream: UnixStream, keys: &'static [u32]) -> std::io::Result<()> {
        use smithay::reexports::reis::PendingRequestResult;
        use smithay::reexports::reis::ei;
        use smithay::reexports::reis::event::DeviceCapability;
        use smithay::reexports::reis::event::EiEvent;
        use smithay::reexports::reis::event::EiEventConverter;
        use smithay::reexports::reis::handshake::ei_handshake_blocking;

        let context = ei::Context::new(stream)?;
        let handshake =
            ei_handshake_blocking(&context, "key test", ei::handshake::ContextType::Sender)
                .map_err(std::io::Error::other)?;
        let mut converter = EiEventConverter::new(&context, handshake);
        loop {
            // The socket is nonblocking: zero bytes means nothing to read yet.
            match context.read() {
                Ok(0) => std::thread::sleep(std::time::Duration::from_millis(1)),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error),
            }
            while let Some(PendingRequestResult::Request(event)) = context.pending_event() {
                converter
                    .handle_event(event)
                    .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
            }
            while let Some(event) = converter.next_event() {
                if let EiEvent::SeatAdded(added) = &event {
                    added
                        .seat
                        .bind_capabilities(DeviceCapability::Keyboard.into());
                } else if let EiEvent::DeviceResumed(resumed) = &event
                    && let Some(keyboard) = resumed.device.interface::<ei::Keyboard>()
                {
                    let device = resumed.device.device();
                    device.start_emulating(resumed.serial, 1);
                    for &key in keys {
                        keyboard.key(key, ei::keyboard::KeyState::Press);
                        device.frame(resumed.serial, 1);
                    }
                }
            }
            context.flush().map_err(std::io::Error::from)?;
        }
    }

    #[test]
    fn ei_keys_beyond_the_evdev_range_are_dropped() {
        let mut event_loop =
            smithay::reexports::calloop::EventLoop::<State>::try_new().expect("event loop");
        let (_display, mut state) = crate::tests::test_state();
        let (client, server) =
            connect_to_eis(&event_loop.handle(), "us", None).expect("connect to EIS");
        // u32::MAX is the largest code an EI sender can send; 30 is KEY_A.
        let sender = std::thread::spawn(move || {
            ei_key_presses(UnixStream::from(client), &[u32::MAX, 0x300, 30])
        });
        let keyboard = state.seat.get_keyboard().expect("keyboard");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !keyboard.pressed_keys().contains(&38u32.into()) {
            assert!(std::time::Instant::now() < deadline, "KEY_A never arrived");
            event_loop
                .dispatch(std::time::Duration::from_millis(10), &mut state)
                .expect("dispatch EIS");
        }
        assert_eq!(keyboard.pressed_keys(), [38u32.into()].into());
        server
            .shutdown(std::net::Shutdown::Both)
            .expect("shut down EI connection");
        sender.join().expect("EI sender").expect("EI connection");
    }

    #[test]
    fn portal_eis_receipts_go_only_to_the_session_owner() {
        let mut event_loop =
            smithay::reexports::calloop::EventLoop::<State>::try_new().expect("event loop");
        let (_display, mut state) = crate::tests::test_state();
        let (outgoing, received) = std::sync::mpsc::channel();
        state.delivery.set_outgoing(outgoing);
        state
            .delivery
            .subscribe("owner".into(), Some(std::process::id()));
        state.delivery.subscribe("stranger".into(), None);
        let (client, server) = connect_to_eis(&event_loop.handle(), "us", Some(std::process::id()))
            .expect("connect to EIS");
        let sender = std::thread::spawn(move || ei_key_presses(UnixStream::from(client), &[30]));
        let mut receipts = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while receipts.is_empty() {
            assert!(std::time::Instant::now() < deadline, "no receipt arrived");
            event_loop
                .dispatch(std::time::Duration::from_millis(10), &mut state)
                .expect("dispatch EIS");
            for message in received.try_iter() {
                if let crate::delivery::Message::Send(message) = message
                    && let crate::delivery::Signal::Receipt(receipt) = message.signal
                {
                    receipts.push((message.to, receipt));
                }
            }
        }
        let (to, receipt) = &receipts[0];
        assert_eq!(to, &["owner".to_owned()]);
        assert_eq!(receipt.source, SourceKind::PortalEis);
        assert_eq!(receipt.source_pid, Some(std::process::id()));
        assert_eq!(receipt.delivery.as_str(), "nowhere", "no window has focus");
        server
            .shutdown(std::net::Shutdown::Both)
            .expect("shut down EI connection");
        sender.join().expect("EI sender").expect("EI connection");
    }

    #[test]
    fn keyboard_input_maps_keysyms_and_obeys_lease() {
        let (_display, mut state) = crate::tests::test_state();
        let source = KeyboardSource::new_auxiliary();
        let keyboard = state.seat.get_keyboard().expect("keyboard");
        apply_portal_input(
            &mut state,
            PortalInput::Keycode {
                source,
                key: 30,
                state: KeyState::Pressed,
            },
        );
        assert!(keyboard.pressed_keys().contains(&38u32.into()));
        apply_portal_input(
            &mut state,
            PortalInput::Keycode {
                source,
                key: 30,
                state: KeyState::Released,
            },
        );
        apply_keysym(&mut state, source, 0x41, KeyState::Pressed).expect("keysym");
        assert!(keyboard.pressed_keys().contains(&50u32.into()));
        assert!(keyboard.pressed_keys().contains(&38u32.into()));
        apply_keysym(&mut state, source, 0x41, KeyState::Released).expect("keysym release");
        assert!(keyboard.pressed_keys().is_empty());
        assert!(apply_keysym(&mut state, source, 0x1fff_ffff, KeyState::Pressed).is_err());
        state.set_browser_lease(true);
        apply_portal_input(
            &mut state,
            PortalInput::Keycode {
                source,
                key: 30,
                state: KeyState::Pressed,
            },
        );
        apply_keysym(&mut state, source, 0x41, KeyState::Pressed).expect("dropped keysym");
        assert!(keyboard.pressed_keys().is_empty());
    }
}
