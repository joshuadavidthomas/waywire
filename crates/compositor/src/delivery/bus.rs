//! The `InputDelivery1` D-Bus interface on the compositor's private session
//! bus. Method calls travel into the calloop loop, which owns the seat; the
//! loop's receipts and events come back over a channel and leave as unicast
//! signals, addressed to each subscriber's unique name, so no other
//! connection on the bus sees them.

use std::collections::HashMap;
use std::sync::mpsc;

use anyhow::Result;
use calloop::LoopHandle;
use calloop::channel::Event;
use tokio::sync::oneshot;
use zbus::interface;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::names::UniqueName;
use zbus::zvariant::ObjectPath;
use zbus::zvariant::OwnedValue;
use zbus::zvariant::Value;

use super::InputKind;
use super::Message;
use super::Notice;
use super::Outgoing;
use super::Receipt;
use super::Signal;
use super::TextField;
use super::WindowRecord;
use crate::State;

/// Well-known name the compositor owns on its private bus.
pub(crate) const BUS_NAME: &str = "io.github.joshuadavidthomas.waywire.InputDelivery1";
/// Interface name, also used for the object path.
pub(crate) const INTERFACE: &str = "io.github.joshuadavidthomas.waywire.InputDelivery1";
pub(crate) const OBJECT_PATH: &str = "/io/github/joshuadavidthomas/waywire/InputDelivery1";

/// Requests the bus thread routes into the calloop loop.
pub(crate) enum Request {
    Subscribe {
        name: String,
        pid: Option<u32>,
        reply: oneshot::Sender<()>,
    },
    Unsubscribe {
        name: String,
    },
    Focus {
        reply: oneshot::Sender<HashMap<String, OwnedValue>>,
    },
}

/// Serve `InputDelivery1` on `bus_address` for the compositor's lifetime and
/// return the channel the loop sends signals through. The desktop runs
/// without it if the bus refuses.
pub(crate) fn start(
    handle: &LoopHandle<'static, State>,
    bus_address: &str,
) -> Result<mpsc::Sender<Message>> {
    let (requests, incoming) = calloop::channel::channel::<Request>();
    handle
        .insert_source(incoming, |event, (), state| {
            if let Event::Msg(request) = event {
                state.apply_delivery_request(request);
            }
        })
        .map_err(|error| anyhow::anyhow!("register input delivery requests: {error}"))?;
    let connection = zbus::blocking::connection::Builder::address(bus_address)?
        .name(BUS_NAME)?
        .serve_at(
            OBJECT_PATH,
            InputDelivery {
                requests: requests.clone(),
            },
        )?
        .build()?;
    let (outgoing, signals) = mpsc::channel::<Message>();
    let emitter = connection.clone();
    std::thread::Builder::new()
        .name("waywire-delivery".to_owned())
        .spawn(move || {
            for message in signals {
                match message {
                    Message::Send(message) => emit(&emitter, &message),
                    Message::Flush(after) => after(),
                }
            }
        })?;
    // A subscriber that disconnects without unsubscribing is dropped when
    // its unique name goes away.
    let watcher = zbus::blocking::fdo::DBusProxy::new(&connection)?;
    let changes = watcher.receive_name_owner_changed()?;
    std::thread::Builder::new()
        .name("waywire-delivery-names".to_owned())
        .spawn(move || {
            let _connection = connection;
            for change in changes {
                let Ok(args) = change.args() else {
                    continue;
                };
                if args.new_owner().is_none()
                    && matches!(args.name(), BusName::Unique(_))
                    && requests
                        .send(Request::Unsubscribe {
                            name: args.name().to_string(),
                        })
                        .is_err()
                {
                    break;
                }
            }
        })?;
    Ok(outgoing)
}

impl State {
    fn apply_delivery_request(&mut self, request: Request) {
        match request {
            Request::Subscribe { name, pid, reply } => {
                tracing::debug!(%name, ?pid, "input delivery subscriber");
                self.delivery.subscribe(name, pid);
                if reply.send(()).is_err() {
                    tracing::debug!("input delivery subscriber hung up");
                }
            }
            Request::Unsubscribe { name } => self.delivery.unsubscribe(&name),
            Request::Focus { reply } => {
                let (window, popup, text_field) = self.focus_report();
                let mut focus = HashMap::new();
                insert(&mut focus, "window", window_dict(window.as_ref()));
                insert(&mut focus, "popup", popup);
                insert_text_field(&mut focus, text_field);
                insert(&mut focus, "serial", self.delivery.last_serial());
                let focus = owned(focus);
                // Reply only after the signals already queued, so a driver
                // that calls this after its input has all of its receipts.
                self.delivery.flush(Box::new(move || {
                    if reply.send(focus).is_err() {
                        tracing::debug!("input delivery requester hung up");
                    }
                }));
            }
        }
    }
}

struct InputDelivery {
    requests: calloop::channel::Sender<Request>,
}

#[interface(name = "io.github.joshuadavidthomas.waywire.InputDelivery1")]
impl InputDelivery {
    /// Start receiving this connection's receipts and every focus event.
    async fn subscribe(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let sender: UniqueName<'_> = header
            .sender()
            .cloned()
            .ok_or_else(|| zbus::fdo::Error::Failed("message has no sender".into()))?;
        let pid = zbus::fdo::DBusProxy::new(connection)
            .await?
            .get_connection_unix_process_id(BusName::Unique(sender.clone()))
            .await
            .ok();
        let (reply, wait) = oneshot::channel();
        self.requests
            .send(Request::Subscribe {
                name: sender.to_string(),
                pid,
                reply,
            })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        wait.await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "zbus passes the header by value"
    )]
    fn unsubscribe(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        if let Some(sender) = header.sender() {
            self.requests
                .send(Request::Unsubscribe {
                    name: sender.to_string(),
                })
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        }
        Ok(())
    }

    /// The window that holds keyboard focus now and its text-field state.
    async fn get_focus(&self) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        let (reply, wait) = oneshot::channel();
        self.requests
            .send(Request::Focus { reply })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        wait.await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(property)]
    #[expect(clippy::unused_self, reason = "zbus properties are methods")]
    fn version(&self) -> u32 {
        1
    }
}

fn insert<'a>(dict: &mut HashMap<String, Value<'a>>, key: &str, value: impl Into<Value<'a>>) {
    dict.insert(key.to_owned(), value.into());
}

fn owned(dict: HashMap<String, Value<'_>>) -> HashMap<String, OwnedValue> {
    dict.into_iter()
        .filter_map(|(key, value)| Some((key, OwnedValue::try_from(value).ok()?)))
        .collect()
}

/// A window record; an empty dictionary means no window.
fn window_dict(window: Option<&WindowRecord>) -> HashMap<String, Value<'static>> {
    let mut dict = HashMap::new();
    if let Some(window) = window {
        insert(&mut dict, "id", window.id.clone());
        insert(&mut dict, "app_id", window.app_id.clone());
        insert(&mut dict, "title", window.title.clone());
        insert(&mut dict, "pid", window.pid.unwrap_or(0));
        insert(&mut dict, "x11", window.x11);
    }
    dict
}

fn insert_text_field(dict: &mut HashMap<String, Value<'_>>, text_field: TextField) {
    insert(dict, "text_field", text_field.as_str());
    if let Some(purpose) = text_field.purpose() {
        insert(dict, "text_purpose", purpose);
    }
}

pub(crate) fn receipt_dict(receipt: &Receipt) -> HashMap<String, Value<'static>> {
    let mut dict = HashMap::new();
    insert(&mut dict, "serial", receipt.serial);
    insert(&mut dict, "time_usec", receipt.time_usec);
    insert(&mut dict, "source", receipt.source.as_str());
    insert(&mut dict, "source_pid", receipt.source_pid.unwrap_or(0));
    let (event, pressed, button) = match receipt.input {
        InputKind::Key { pressed } => ("key", Some(pressed), None),
        InputKind::Button { button, pressed } => ("button", Some(pressed), Some(button)),
        InputKind::Scroll => ("scroll", None, None),
        InputKind::Text => ("text", None, None),
    };
    insert(&mut dict, "event", event);
    if let Some(pressed) = pressed {
        insert(&mut dict, "pressed", pressed);
    }
    if let Some(button) = button {
        insert(&mut dict, "button", button);
    }
    insert(&mut dict, "delivery", receipt.delivery);
    insert(&mut dict, "window", window_dict(receipt.window.as_ref()));
    insert(&mut dict, "popup", receipt.popup);
    if let Some(chrome) = receipt.chrome {
        insert(&mut dict, "chrome", chrome.as_str());
    }
    if let Some(text_field) = receipt.text_field {
        insert_text_field(&mut dict, text_field);
    }
    dict
}

pub(crate) fn notice_dict(notice: &Notice) -> (&'static str, HashMap<String, Value<'static>>) {
    let mut dict = HashMap::new();
    match notice {
        Notice::FocusChanged {
            serial,
            time_usec,
            window,
            previous,
            popup,
            cause,
            source,
            receipt,
        } => {
            insert(&mut dict, "serial", *serial);
            insert(&mut dict, "time_usec", *time_usec);
            insert(&mut dict, "window", window_dict(window.as_ref()));
            insert(&mut dict, "previous", window_dict(previous.as_ref()));
            insert(&mut dict, "popup", *popup);
            insert(&mut dict, "cause", cause.as_str());
            if let Some(source) = source {
                insert(&mut dict, "source", source.as_str());
            }
            if let Some(receipt) = receipt {
                insert(&mut dict, "receipt", *receipt);
            }
            ("FocusChanged", dict)
        }
        Notice::WindowMapped {
            serial,
            time_usec,
            window,
            parent,
            modal,
        } => {
            insert(&mut dict, "serial", *serial);
            insert(&mut dict, "time_usec", *time_usec);
            insert(&mut dict, "window", window_dict(Some(window)));
            insert(&mut dict, "parent", window_dict(parent.as_ref()));
            insert(&mut dict, "modal", *modal);
            ("WindowMapped", dict)
        }
        Notice::TextFieldChanged {
            serial,
            time_usec,
            window,
            text_field,
        } => {
            insert(&mut dict, "serial", *serial);
            insert(&mut dict, "time_usec", *time_usec);
            insert(&mut dict, "window", window_dict(window.as_ref()));
            insert_text_field(&mut dict, *text_field);
            ("TextFieldChanged", dict)
        }
    }
}

fn emit(connection: &zbus::blocking::Connection, message: &Outgoing) {
    let (member, body) = match &message.signal {
        Signal::Receipt(receipt) => ("Receipt", receipt_dict(receipt)),
        Signal::Notice(notice) => notice_dict(notice),
    };
    let Ok(path) = ObjectPath::try_from(OBJECT_PATH) else {
        return;
    };
    for destination in &message.to {
        if let Err(error) = connection.emit_signal(
            Some(destination.as_str()),
            &path,
            INTERFACE,
            member,
            &(&body,),
        ) {
            tracing::debug!(%error, %destination, member, "input delivery signal");
        }
    }
}
