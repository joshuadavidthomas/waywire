//! `RemoteDesktop` portal backend: Notify input and EIS connections share the
//! compositor's input arbitration.
//!
//! `CreateSession` registers an `org.freedesktop.impl.portal.Session` object at
//! the handle the frontend passed in, with the `Close` method the frontend
//! calls when the session ends. `ConnectToEIS` creates a Unix socket pair,
//! serves one end on the compositor's calloop loop as a new EI connection, and
//! returns the other end as the file descriptor; the connection then behaves
//! exactly like one accepted on the listening socket. `Start` approves without
//! a dialog: the desktop is headless and its callers are on the box. Browser
//! ownership drops agent input through the compositor's shared seat entry points.
//!
//! The xdg-desktop-portal frontend forwards Notify calls asynchronously and
//! may report success to clients even when the backend returns an error.
#![expect(
    clippy::needless_pass_by_value,
    clippy::used_underscore_binding,
    clippy::unused_self,
    reason = "portal method signatures are fixed by the specification; zbus's generated dispatchers use the parameters"
)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use calloop::channel::Sender;
use smithay::backend::input::Axis;
use smithay::backend::input::AxisSource;
use smithay::backend::input::ButtonState;
use smithay::backend::input::KeyState;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::keyboard::Layout;
use smithay::input::keyboard::xkb;
use smithay::input::pointer::AxisFrame;
use tokio::sync::oneshot;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::Fd;
use zbus::zvariant::OwnedObjectPath;
use zbus::zvariant::OwnedValue;

use crate::State;
use crate::portal::PortalRequest;

pub(crate) enum PortalInput {
    Motion {
        source: KeyboardSource,
        dx: f64,
        dy: f64,
    },
    Absolute {
        source: KeyboardSource,
        x: f64,
        y: f64,
    },
    Button {
        source: KeyboardSource,
        button: u32,
        state: ButtonState,
    },
    Axis {
        source: KeyboardSource,
        dx: f64,
        dy: f64,
    },
    Discrete {
        source: KeyboardSource,
        axis: Axis,
        steps: i32,
    },
    Keycode {
        source: KeyboardSource,
        key: u32,
        state: KeyState,
    },
    Keysym {
        source: KeyboardSource,
        keysym: u32,
        state: KeyState,
    },
}

pub(super) fn apply_portal_input(state: &mut State, input: PortalInput) -> Result<(), String> {
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
        PortalInput::Keysym {
            source,
            keysym,
            state: key_state,
        } => return apply_keysym(state, source, keysym, key_state),
    }
    Ok(())
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
    let mut found = None;
    'search: for code in 8..=255 {
        let key = xkb::Keycode::new(code);
        for group in 0..keymap.num_layouts_for_key(key) {
            for level in 0..keymap.num_levels_for_key(key, group) {
                if !keymap
                    .key_get_syms_by_level(key, group, level)
                    .contains(&xkb::Keysym::new(keysym))
                {
                    continue;
                }
                let mut masks = [0; 32];
                let count = keymap.key_get_mods_for_level(key, group, level, &mut masks);
                if let Some(mask) = masks[..count.min(masks.len())]
                    .iter()
                    .copied()
                    .find(|mask| mask & !(shift_mask | altgr_mask) == 0)
                {
                    found = Some((code - 8, group, mask));
                    break 'search;
                }
            }
        }
    }
    let (key, group, mask) =
        found.ok_or_else(|| format!("keysym {keysym:#x} is not available in the keymap"))?;
    if state.browser_holds_input {
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

fn finite_pair(x: f64, y: f64) -> zbus::fdo::Result<()> {
    if x.is_finite() && y.is_finite() {
        Ok(())
    } else {
        Err(zbus::fdo::Error::InvalidArgs(
            "coordinates must be finite".into(),
        ))
    }
}

fn key_state(state: u32) -> zbus::fdo::Result<KeyState> {
    match state {
        0 => Ok(KeyState::Released),
        1 => Ok(KeyState::Pressed),
        _ => Err(zbus::fdo::Error::InvalidArgs("invalid key state".into())),
    }
}

/// The frontend checks this property to decide whether it may offer EIS
/// connections; the packaged `RemoteDesktop` interface is version 2.
const IMPL_VERSION: u32 = 2;

/// Approved.
const RESPONSE_OK: u32 = 0;

/// Keyboard and pointer bitmasks from the portal specification.
const KEYBOARD: u32 = 1;
const POINTER: u32 = 2;

/// Device types this seat exposes, and the bitmask echoed back to callers.
const DEVICE_TYPES: u32 = KEYBOARD | POINTER;

/// State of one remote desktop session, shared between the D-Bus interface
/// objects.
struct PortalSession {
    source: KeyboardSource,
    warned_keysyms: Mutex<HashSet<u32>>,
    /// Duplicate of the socket-pair end the EIS server holds. Shutting it
    /// down ends the EI connection even though the loop keeps its own
    /// descriptor, and the loop then releases the seat input it owns.
    eis_server: Mutex<Option<UnixStream>>,
    /// The stream `Start` reported, the only one absolute motion may name.
    stream: AtomicU32,
}

/// Serves `org.freedesktop.impl.portal.RemoteDesktop` on the session bus.
pub(crate) struct RemoteDesktop {
    requests: Sender<PortalRequest>,
    sessions: Arc<Mutex<HashMap<OwnedObjectPath, Arc<PortalSession>>>>,
}

impl RemoteDesktop {
    pub(crate) fn new(requests: Sender<PortalRequest>) -> Self {
        Self {
            requests,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn source(&self, path: &OwnedObjectPath) -> zbus::fdo::Result<KeyboardSource> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .map(|session| session.source)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown session".into()))
    }

    fn send_input(&self, input: PortalInput) -> zbus::fdo::Result<()> {
        self.requests
            .send(PortalRequest::Input(input))
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }
}

#[interface(name = "org.freedesktop.impl.portal.RemoteDesktop")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let session = Arc::new(PortalSession {
            source: KeyboardSource::new_auxiliary(),
            warned_keysyms: Mutex::new(HashSet::new()),
            eis_server: Mutex::new(None),
            stream: AtomicU32::new(super::screencast::STREAM_ID),
        });
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_handle.clone(), Arc::clone(&session));
        let object_server = emitter.connection().object_server();
        // Drop any object the frontend may have re-registered the handle for.
        if let Err(error) = object_server.remove::<Session, _>(&session_handle).await {
            tracing::debug!(%error, "replace portal session object");
        }
        let session = Session {
            state: session,
            sessions: Arc::clone(&self.sessions),
            path: session_handle.clone(),
            requests: self.requests.clone(),
        };
        if let Err(error) = object_server.at(&session_handle, session).await {
            tracing::warn!(%error, "serve portal session object");
        }
        (RESPONSE_OK, HashMap::new())
    }

    fn select_devices(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        tracing::debug!(%session_handle, %app_id, "select devices");
        // Intersect the request with what this seat exposes; the frontend
        // records it for its AvailableDevices property.
        let requested = match options
            .get("types")
            .and_then(|value| u32::try_from(value).ok())
        {
            Some(types) => types & DEVICE_TYPES,
            None => DEVICE_TYPES,
        };
        let mut results = HashMap::new();
        results.insert("device_types".to_owned(), OwnedValue::from(requested));
        (RESPONSE_OK, results)
    }

    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        tracing::debug!(%session_handle, %app_id, "start remote desktop session");
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_handle)
            .cloned()
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown session".into()))?;
        self.requests
            .send(PortalRequest::RegisterSource {
                source: session.source,
            })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        // See the module comment: approve without a dialog for now.
        let mut results = HashMap::new();
        results.insert("devices".to_owned(), OwnedValue::from(DEVICE_TYPES));
        // The frontend starts a shared RemoteDesktop/ScreenCast session through
        // RemoteDesktop.Start, not ScreenCast.Start.
        let (stream, streams) = super::screencast::streams(&self.requests, &session_handle).await?;
        session.stream.store(stream, Ordering::Relaxed);
        results.insert("streams".to_owned(), streams);
        Ok((RESPONSE_OK, results))
    }

    fn notify_pointer_motion(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        dx: f64,
        dy: f64,
    ) -> zbus::fdo::Result<()> {
        finite_pair(dx, dy)?;
        self.send_input(PortalInput::Motion {
            source: self.source(&session_handle)?,
            dx,
            dy,
        })
    }

    fn notify_pointer_motion_absolute(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        stream: u32,
        x: f64,
        y: f64,
    ) -> zbus::fdo::Result<()> {
        finite_pair(x, y)?;
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_handle)
            .cloned()
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown session".into()))?;
        if stream != session.stream.load(Ordering::Relaxed) {
            return Err(zbus::fdo::Error::InvalidArgs("unknown stream".into()));
        }
        self.send_input(PortalInput::Absolute {
            source: session.source,
            x,
            y,
        })
    }

    fn notify_pointer_button(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        button: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        let button = u32::try_from(button)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(format!("invalid button: {error}")))?;
        let state = match state {
            0 => ButtonState::Released,
            1 => ButtonState::Pressed,
            _ => return Err(zbus::fdo::Error::InvalidArgs("invalid button state".into())),
        };
        self.send_input(PortalInput::Button {
            source: self.source(&session_handle)?,
            button,
            state,
        })
    }

    fn notify_pointer_axis(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        dx: f64,
        dy: f64,
    ) -> zbus::fdo::Result<()> {
        finite_pair(dx, dy)?;
        self.send_input(PortalInput::Axis {
            source: self.source(&session_handle)?,
            dx,
            dy,
        })
    }

    fn notify_pointer_axis_discrete(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        axis: u32,
        steps: i32,
    ) -> zbus::fdo::Result<()> {
        let axis = match axis {
            0 => Axis::Vertical,
            1 => Axis::Horizontal,
            _ => return Err(zbus::fdo::Error::InvalidArgs("invalid axis".into())),
        };
        self.send_input(PortalInput::Discrete {
            source: self.source(&session_handle)?,
            axis,
            steps,
        })
    }

    fn notify_keyboard_keycode(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        keycode: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        let key = u32::try_from(keycode)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(format!("invalid keycode: {error}")))?;
        self.send_input(PortalInput::Keycode {
            source: self.source(&session_handle)?,
            key,
            state: key_state(state)?,
        })
    }

    async fn notify_keyboard_keysym(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        keysym: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        let keysym = u32::try_from(keysym)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(format!("invalid keysym: {error}")))?;
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_handle)
            .cloned()
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown session".into()))?;
        let (reply, wait) = oneshot::channel();
        self.requests
            .send(PortalRequest::Keysym {
                input: PortalInput::Keysym {
                    source: session.source,
                    keysym,
                    state: key_state(state)?,
                },
                reply,
            })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        match wait
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
        {
            Ok(()) => Ok(()),
            Err(error) => {
                if session
                    .warned_keysyms
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(keysym)
                {
                    tracing::warn!(%session_handle, keysym, %error, "portal keysym unavailable");
                }
                Err(zbus::fdo::Error::InvalidArgs(error))
            }
        }
    }

    /// Returns a file descriptor for an EI sender connection to this
    /// compositor's EIS server, as the `ConnectToEIS` method requires.
    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<Fd<'static>> {
        tracing::debug!(%session_handle, %app_id, "connect to EIS");
        let (reply, wait) = oneshot::channel();
        if self
            .requests
            .send(PortalRequest::ConnectToEis { reply })
            .is_err()
        {
            return Err(zbus::fdo::Error::Failed("compositor loop is gone".into()));
        }
        let (client, server) = wait
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        // Keep the server end's duplicate so this session's Close can end the
        // connection the loop serves.
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_handle)
            .cloned();
        if let Some(session) = session
            && let Some(previous) = session
                .eis_server
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .replace(server)
        {
            // A repeated ConnectToEIS replaces the session's connection.
            shut_down(&previous);
        }
        Ok(Fd::from(client))
    }

    #[zbus(property)]
    fn available_device_types(&self) -> u32 {
        DEVICE_TYPES
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        IMPL_VERSION
    }
}

/// Serves `org.freedesktop.impl.portal.Session` at the session handle path.
struct Session {
    state: Arc<PortalSession>,
    sessions: Arc<Mutex<HashMap<OwnedObjectPath, Arc<PortalSession>>>>,
    path: OwnedObjectPath,
    requests: Sender<PortalRequest>,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl Session {
    async fn close(&self, #[zbus(object_server)] server: &zbus::ObjectServer) {
        if let Err(error) = self.requests.send(PortalRequest::DisconnectSource {
            source: self.state.source,
        }) {
            tracing::debug!(%error, "disconnect portal source");
        }
        super::screencast::close(&self.requests, &self.path);
        if let Some(stream) = self
            .state
            .eis_server
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            shut_down(&stream);
        }
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.path);
        if let Err(error) = server.remove::<Self, _>(&self.path).await {
            tracing::debug!(%error, "remove closed portal session object");
        }
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        IMPL_VERSION
    }
}

/// End an EI connection through any descriptor of its server end.
fn shut_down(stream: &UnixStream) {
    if let Err(error) = stream.shutdown(Shutdown::Both) {
        tracing::debug!(%error, "shut down portal EI connection");
    }
}

/// Runs in the calloop loop, which owns the seat and the EIS server: creates
/// the socket pair `ConnectToEIS` hands out. One end joins the existing EIS
/// server as a new client; the caller gets the other end and a duplicate of
/// the server end for closing the session.
pub(crate) fn connect_to_eis(
    handle: &smithay::reexports::calloop::LoopHandle<'static, crate::State>,
    layout: &str,
) -> std::io::Result<(OwnedFd, UnixStream)> {
    use smithay::reexports::reis::eis;

    let (client, server) = UnixStream::pair()?;
    let server_handle = server.try_clone()?;
    let context = eis::Context::new(server)?;
    crate::eis::serve_connection(handle, context, layout.to_owned())
        .map_err(std::io::Error::other)?;
    Ok((client.into(), server_handle))
}

#[cfg(test)]
mod tests {
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
        )
        .expect("motion");
        assert_eq!(pointer.current_location(), (311.0, 97.0).into());
        apply_portal_input(
            &mut state,
            PortalInput::Absolute {
                source,
                x: 5000.0,
                y: -20.0,
            },
        )
        .expect("motion");
        assert_eq!(pointer.current_location(), (1919.0, 0.0).into());
        apply_portal_input(
            &mut state,
            PortalInput::Button {
                source,
                button: 0x110,
                state: ButtonState::Pressed,
            },
        )
        .expect("button");
        assert!(
            state
                .buttons
                .get(&source)
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
        )
        .expect("button");
        apply_portal_input(
            &mut state,
            PortalInput::Absolute {
                source,
                x: 311.0,
                y: 97.0,
            },
        )
        .expect("motion");
        assert!(
            state
                .buttons
                .get(&source)
                .is_none_or(std::collections::HashSet::is_empty)
        );
        assert_eq!(pointer.current_location(), (1919.0, 0.0).into());
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
        )
        .expect("keycode");
        assert!(keyboard.pressed_keys().contains(&38u32.into()));
        apply_portal_input(
            &mut state,
            PortalInput::Keycode {
                source,
                key: 30,
                state: KeyState::Released,
            },
        )
        .expect("keycode release");
        apply_portal_input(
            &mut state,
            PortalInput::Keysym {
                source,
                keysym: 0x41,
                state: KeyState::Pressed,
            },
        )
        .expect("keysym");
        assert!(keyboard.pressed_keys().contains(&50u32.into()));
        assert!(keyboard.pressed_keys().contains(&38u32.into()));
        apply_portal_input(
            &mut state,
            PortalInput::Keysym {
                source,
                keysym: 0x41,
                state: KeyState::Released,
            },
        )
        .expect("keysym release");
        assert!(keyboard.pressed_keys().is_empty());
        assert!(
            apply_portal_input(
                &mut state,
                PortalInput::Keysym {
                    source,
                    keysym: 0x1fff_ffff,
                    state: KeyState::Pressed
                }
            )
            .is_err()
        );
        state.set_browser_lease(true);
        apply_portal_input(
            &mut state,
            PortalInput::Keycode {
                source,
                key: 30,
                state: KeyState::Pressed,
            },
        )
        .expect("dropped keycode");
        apply_portal_input(
            &mut state,
            PortalInput::Keysym {
                source,
                keysym: 0x41,
                state: KeyState::Pressed,
            },
        )
        .expect("dropped keysym");
        assert!(keyboard.pressed_keys().is_empty());
    }
}
