//! `RemoteDesktop` portal backend: sessions whose input flows through the same
//! EIS server Xwayland's libei bridge uses.
//!
//! `CreateSession` registers an `org.freedesktop.impl.portal.Session` object at
//! the handle the frontend passed in, with the `Close` method the frontend
//! calls when the session ends. `ConnectToEIS` creates a Unix socket pair,
//! serves one end on the compositor's calloop loop as a new EI connection, and
//! returns the other end as the file descriptor; the connection then behaves
//! exactly like one accepted on the listening socket. `Start` approves without
//! a dialog: the desktop is headless and its callers are on the box. Ownership
//! arbitration (ROADMAP phase 2) will gate what this grants.
#![expect(
    clippy::needless_pass_by_value,
    clippy::used_underscore_binding,
    clippy::unused_self,
    reason = "portal method signatures are fixed by the specification; zbus's generated dispatchers use the parameters"
)]

use std::collections::HashMap;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;

use calloop::channel::Sender;
use tokio::sync::oneshot;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::Fd;
use zbus::zvariant::OwnedObjectPath;
use zbus::zvariant::OwnedValue;

use crate::portal::PortalRequest;

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
#[derive(Default)]
struct PortalSession {
    /// Duplicate of the socket-pair end the EIS server holds. Shutting it
    /// down ends the EI connection even though the loop keeps its own
    /// descriptor, and the loop then releases the seat input it owns.
    eis_server: Mutex<Option<UnixStream>>,
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
        let session = Arc::new(PortalSession::default());
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

    fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        tracing::debug!(%session_handle, %app_id, "start remote desktop session");
        // See the module comment: approve without a dialog for now.
        let mut results = HashMap::new();
        results.insert("devices".to_owned(), OwnedValue::from(DEVICE_TYPES));
        (RESPONSE_OK, results)
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
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl Session {
    async fn close(&self, #[zbus(object_server)] server: &zbus::ObjectServer) {
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
