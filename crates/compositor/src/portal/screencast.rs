//! `ScreenCast` portal backend: one monitor stream of the output, delivered
//! as a `PipeWire` video node.
//!
//! `Start` (and `RemoteDesktop.Start` for shared sessions) asks the calloop
//! loop to create the session's stream and answers with the stream's
//! `PipeWire` node ID, which is also the stream ID callers pass to
//! `NotifyPointerMotionAbsolute`. The frontend's `OpenPipeWireRemote` then
//! hands the caller a connection to the compositor's `PipeWire` daemon that
//! can see only that node. Without a daemon the stream is metadata only and
//! keeps the fixed [`STREAM_ID`], so `RemoteDesktop` clients that only read
//! the output geometry still work. Closing the session destroys the node.
//!
//! The rendered frames never contain a cursor, so the only cursor mode is
//! Hidden.
#![expect(
    clippy::used_underscore_binding,
    clippy::unused_self,
    reason = "portal method signatures are fixed by the specification"
)]

use std::collections::HashMap;

use calloop::channel::Sender;
use tokio::sync::oneshot;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;
use zbus::zvariant::OwnedValue;
use zbus::zvariant::Value;

use super::PortalRequest;

/// Stream ID of a metadata-only stream, reported when no `PipeWire` daemon
/// is available.
pub(super) const STREAM_ID: u32 = 1;

pub(crate) struct ScreenCast {
    requests: Sender<PortalRequest>,
}

impl ScreenCast {
    pub(crate) fn new(requests: Sender<PortalRequest>) -> Self {
        Self { requests }
    }
}

/// Start `session`'s stream and describe it in the `streams` result form,
/// returning the stream ID alongside.
pub(super) async fn streams(
    requests: &Sender<PortalRequest>,
    session: &OwnedObjectPath,
) -> zbus::fdo::Result<(u32, OwnedValue)> {
    let failed = |error: &dyn std::fmt::Display| zbus::fdo::Error::Failed(error.to_string());
    let (reply, wait) = oneshot::channel();
    requests
        .send(PortalRequest::OutputSize { reply })
        .map_err(|error| failed(&error))?;
    let size = wait
        .await
        .map_err(|error| failed(&error))?
        .ok_or_else(|| zbus::fdo::Error::Failed("output has no geometry".into()))?;
    let (reply, wait) = oneshot::channel();
    requests
        .send(PortalRequest::StartCast {
            session: session.to_string(),
            reply,
        })
        .map_err(|error| failed(&error))?;
    // A stream that failed, or a loop that dropped the request, leaves the
    // session with metadata only.
    let stream = wait.await.ok().flatten().unwrap_or(STREAM_ID);
    let convert = |value| OwnedValue::try_from(value).map_err(|e| failed(&e));
    let properties = HashMap::from([
        ("position".to_owned(), convert(Value::from((0i32, 0i32)))?),
        ("size".to_owned(), convert(Value::from(size))?),
        ("source_type".to_owned(), OwnedValue::from(1u32)),
    ]);
    let streams = vec![(stream, properties)];
    Ok((stream, convert(Value::from(streams))?))
}

/// End `session`'s stream, if it has one.
pub(super) fn close(requests: &Sender<PortalRequest>, session: &OwnedObjectPath) {
    if let Err(error) = requests.send(PortalRequest::CloseCast {
        session: session.to_string(),
    }) {
        tracing::debug!(%error, "close ScreenCast stream");
    }
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let session = Session {
            path: session_handle.clone(),
            requests: self.requests.clone(),
        };
        if let Err(error) = emitter
            .connection()
            .object_server()
            .at(&session_handle, session)
            .await
        {
            tracing::warn!(%error, "serve ScreenCast session object");
        }
        (0, HashMap::new())
    }

    fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        _session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        (0, HashMap::new())
    }

    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let (_, streams) = streams(&self.requests, &session_handle).await?;
        Ok((0, HashMap::from([("streams".to_owned(), streams)])))
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        1 // Monitor.
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        1 // Hidden.
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

/// Serves `org.freedesktop.impl.portal.Session` for a `ScreenCast`-only
/// session.
struct Session {
    path: OwnedObjectPath,
    requests: Sender<PortalRequest>,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl Session {
    async fn close(&self, #[zbus(object_server)] server: &zbus::ObjectServer) {
        close(&self.requests, &self.path);
        if let Err(error) = server.remove::<Self, _>(&self.path).await {
            tracing::debug!(%error, "remove closed ScreenCast session object");
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}
