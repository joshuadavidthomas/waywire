//! `ScreenCast` portal stub returning metadata only, with no `PipeWire` video.
//!
//! This lets `RemoteDesktop` clients select the output on the shared session.
//! A real `PipeWire` stream is a separate decision.
#![expect(
    clippy::used_underscore_binding,
    clippy::unused_self,
    reason = "portal method signatures are fixed by the specification"
)]

use std::collections::HashMap;

use calloop::channel::Sender;
use tokio::sync::oneshot;
use zbus::interface;
use zbus::zvariant::OwnedObjectPath;
use zbus::zvariant::OwnedValue;
use zbus::zvariant::Value;

use super::PortalRequest;

pub(super) const STREAM_ID: u32 = 1;

pub(crate) struct ScreenCast {
    requests: Sender<PortalRequest>,
}

impl ScreenCast {
    pub(crate) fn new(requests: Sender<PortalRequest>) -> Self {
        Self { requests }
    }
}

pub(super) async fn streams(requests: &Sender<PortalRequest>) -> zbus::fdo::Result<OwnedValue> {
    let (reply, wait) = oneshot::channel();
    requests
        .send(PortalRequest::OutputSize { reply })
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
    let size = wait
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
        .ok_or_else(|| zbus::fdo::Error::Failed("output has no geometry".into()))?;
    let convert = |value| {
        OwnedValue::try_from(value)
            .map_err(|e: zbus::zvariant::Error| zbus::fdo::Error::Failed(e.to_string()))
    };
    let properties = HashMap::from([
        ("position".to_owned(), convert(Value::from((0i32, 0i32)))?),
        ("size".to_owned(), convert(Value::from(size))?),
        ("source_type".to_owned(), OwnedValue::from(1u32)),
    ]);
    let streams = vec![(STREAM_ID, properties)];
    OwnedValue::try_from(Value::from(streams))
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
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
        _session_handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        Ok((
            0,
            HashMap::from([("streams".to_owned(), streams(&self.requests).await?)]),
        ))
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
