//! `PipeWire` video for the `ScreenCast` portal.
//!
//! Like the private session bus, the `PipeWire` daemon belongs to the
//! compositor: it starts the distribution's `pipewire` binary with a minimal
//! configuration whose socket lives in its runtime directory, so a host
//! desktop's daemon is never touched. Without the binary, portal sessions
//! still start and report metadata-only streams.
//!
//! Each started portal session owns one video source node. The node
//! advertises the rendered output size in `BGRx` or `BGRA` with a variable frame
//! rate capped at the desktop frame rate. Frames are pushed only after the
//! renderer produced a damaged frame, by copying the rendered image into a
//! dequeued buffer and triggering one graph cycle; this stream is the graph
//! driver. When the consumer still holds every buffer, the frame waits; the
//! consumer returning a buffer wakes the compositor loop, which then sends the
//! latest image. Frames are skipped, never queued, so the compositor never
//! blocks on a consumer and the browser encoder is unaffected. An output
//! resize renegotiates the format, and frames pause until the consumer
//! accepts the new size. Closing the portal session destroys the node.
//!
//! The daemon runs without a session manager, so the compositor links each
//! video consumer whose `target.object` (or older `node.target`) names one of
//! its nodes, the way a session manager would for a portal client.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use nix::sys::signal::SigSet;
use nix::sys::signal::SigmaskHow;
use nix::sys::signal::pthread_sigmask;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use smithay::reexports::pixman::Image;
use spa::pod::ChoiceValue;
use spa::pod::Pod;
use spa::pod::Property;
use spa::pod::Value;
use spa::utils::Choice;
use spa::utils::ChoiceEnum;
use spa::utils::ChoiceFlags;
use tokio::sync::oneshot;

/// Socket name inside the runtime directory, the daemon's default name.
const SOCKET: &str = "pipewire-0";

/// Only what a video source and its linked consumers need: the native
/// protocol, client nodes and adapters, links, and access control that grants
/// ordinary clients full permissions and lets the portal frontend restrict the
/// connections it hands out.
const CONFIG: &str = "context.properties = {
    core.daemon = true
    core.name = pipewire-0
    support.dbus = false
}
context.spa-libs = {
    support.* = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
    { name = libpipewire-module-access }
]
";

/// A `pipewire` daemon child serving a socket in the runtime directory.
pub(crate) struct Daemon {
    child: Option<Child>,
    socket: PathBuf,
}

impl Daemon {
    /// Start `pipewire` with its socket under `runtime_path`. `ScreenCast`
    /// video is optional, so a daemon that cannot start only disables it.
    pub(crate) fn start(runtime_path: &Path) -> Option<Self> {
        Self::start_program("pipewire", runtime_path)
    }

    fn start_program(program: &str, runtime_path: &Path) -> Option<Self> {
        let config = runtime_path.join("pipewire.conf");
        if let Err(error) = std::fs::write(&config, CONFIG) {
            tracing::warn!(%error, "could not write PipeWire configuration; ScreenCast video disabled");
            return None;
        }
        let mut command = Command::new(program);
        command
            .arg("-c")
            .arg(&config)
            .env("PIPEWIRE_RUNTIME_DIR", runtime_path)
            .env_remove("PIPEWIRE_REMOTE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // SAFETY: pthread_sigmask is async-signal-safe, and the closure only
        // clears the mask inherited from the calloop signal source.
        unsafe {
            command.pre_exec(|| {
                pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
                    .map_err(std::io::Error::from)
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(%error, "could not start pipewire; ScreenCast video disabled");
                return None;
            }
        };
        let socket = runtime_path.join(SOCKET);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            match child.try_wait() {
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => {
                    tracing::warn!(
                        ?result,
                        "pipewire did not open its socket; ScreenCast video disabled"
                    );
                    reap(&mut child);
                    return None;
                }
            }
        }
        tracing::info!(socket = %socket.display(), "private PipeWire daemon ready");
        Some(Self {
            child: Some(child),
            socket,
        })
    }

    /// Absolute socket path, usable as `PIPEWIRE_REMOTE`.
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// The daemon's PID while it is running.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Log a daemon that exited on its own; the desktop keeps running.
    pub(crate) fn poll(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::warn!(%status, "pipewire exited; ScreenCast video unavailable");
                self.child = None;
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "could not inspect pipewire"),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            reap(&mut child);
        }
    }
}

fn reap(child: &mut Child) {
    if let Err(error) = child.kill() {
        tracing::warn!(pid = child.id(), %error, "could not stop pipewire");
    }
    if let Err(error) = child.wait() {
        tracing::warn!(pid = child.id(), %error, "could not reap pipewire");
    }
}

/// A consumer node the linker watches for a target naming one of our nodes.
/// Listeners come first: fields drop in order, and a listener must unhook
/// before its proxy is destroyed.
struct Consumer {
    _listener: pw::node::NodeListener,
    _node: pw::node::Node,
    link: Option<pw::link::Link>,
}

/// What the linker knows about the graph.
#[derive(Default)]
struct Graph {
    /// Node IDs of the live screen-cast streams.
    sources: HashSet<u32>,
    /// `object.serial` of every node, which newer clients target instead of
    /// the ID.
    serials: HashMap<u32, String>,
    consumers: HashMap<u32, Consumer>,
}

impl Graph {
    fn source_for(&self, target: &str) -> Option<u32> {
        self.sources.iter().copied().find(|id| {
            target == id.to_string() || self.serials.get(id).is_some_and(|serial| serial == target)
        })
    }
}

/// Stream state the `PipeWire` callbacks update.
struct CastState {
    reply: RefCell<Option<oneshot::Sender<Option<u32>>>>,
    negotiated: Cell<Option<(u32, u32)>>,
    pending: Cell<bool>,
    graph: Rc<RefCell<Graph>>,
}

struct Cast {
    _listener: pw::stream::StreamListener<Rc<CastState>>,
    stream: pw::stream::StreamRc,
    state: Rc<CastState>,
}

impl Cast {
    /// Copy `image` into a free buffer and push it, if the consumer accepted
    /// the image's size and holds a buffer free. Otherwise the frame stays
    /// pending.
    fn offer(&self, image: &Image<'static, 'static>) {
        if !self.state.pending.get() || self.stream.state() != pw::stream::StreamState::Streaming {
            return;
        }
        let (Ok(width), Ok(height)) = (u32::try_from(image.width()), u32::try_from(image.height()))
        else {
            return;
        };
        if self.state.negotiated.get() != Some((width, height)) {
            return;
        }
        let Some(mut buffer) = self.stream.dequeue_buffer() else {
            return;
        };
        let pixels = crate::capture::image_pixels(image);
        let Some(data) = buffer.datas_mut().first_mut() else {
            return;
        };
        let Some(target) = data
            .data()
            .and_then(|target| target.get_mut(..pixels.len()))
        else {
            tracing::warn!("PipeWire buffer smaller than the frame");
            return;
        };
        target.copy_from_slice(pixels);
        let (Ok(size), Ok(stride)) = (u32::try_from(pixels.len()), i32::try_from(image.stride()))
        else {
            return;
        };
        let chunk = data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = stride;
        *chunk.size_mut() = size;
        // Dropping the buffer queues it; the cycle hands it to consumers.
        drop(buffer);
        self.state.pending.set(false);
        if let Err(error) = self.stream.trigger_process() {
            tracing::debug!(%error, "trigger ScreenCast cycle");
        }
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        let id = node_id(&self.stream);
        if let Err(error) = self.stream.disconnect() {
            tracing::debug!(%error, "disconnect ScreenCast stream");
        }
        if let Some(id) = id {
            self.state.graph.borrow_mut().sources.remove(&id);
        }
    }
}

fn node_id(stream: &pw::stream::Stream) -> Option<u32> {
    Some(stream.node_id()).filter(|id| *id != spa::sys::SPA_ID_INVALID)
}

/// The compositor's `PipeWire` connection and its screen-cast streams, one
/// per started portal session.
pub(crate) struct Streams {
    casts: HashMap<String, Cast>,
    graph: Rc<RefCell<Graph>>,
    _registry_listener: pw::registry::Listener,
    _registry: pw::registry::RegistryRc,
    _core_listener: pw::core::Listener,
    core: pw::core::CoreRc,
    _context: pw::context::ContextRc,
    main_loop: pw::main_loop::MainLoopRc,
}

impl Streams {
    /// Connect to the daemon at `socket`.
    pub(crate) fn connect(socket: &Path) -> Result<Self> {
        pw::init();
        let main_loop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&main_loop, None)?;
        let core = context.connect_rc(Some(properties! {
            *pw::keys::REMOTE_NAME => socket.to_string_lossy().into_owned(),
        }))?;
        let core_listener = core
            .add_listener_local()
            .error(|id, seq, res, message| {
                tracing::warn!(id, seq, res, message, "PipeWire error");
            })
            .register();
        let registry = core.get_registry_rc()?;
        let graph = Rc::new(RefCell::new(Graph::default()));
        let registry_listener = registry
            .add_listener_local()
            .global({
                let graph = Rc::clone(&graph);
                let registry = registry.clone();
                let core = core.clone();
                move |global| watch_node(&graph, &registry, &core, global)
            })
            .global_remove({
                let graph = Rc::clone(&graph);
                move |id| {
                    let mut graph = graph.borrow_mut();
                    graph.serials.remove(&id);
                    graph.consumers.remove(&id);
                }
            })
            .register();
        Ok(Self {
            casts: HashMap::new(),
            graph,
            _registry_listener: registry_listener,
            _registry: registry,
            _core_listener: core_listener,
            core,
            _context: context,
            main_loop,
        })
    }

    /// Descriptor that becomes readable when `PipeWire` has work for
    /// [`Self::dispatch`].
    pub(crate) fn fd(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        self.main_loop.loop_().fd().try_clone_to_owned()
    }

    /// Handle pending `PipeWire` events without blocking.
    pub(crate) fn dispatch(&self) {
        self.main_loop.loop_().iterate(pw::loop_::Timeout::None);
    }

    /// Create the stream for portal `session`; `reply` receives its node ID
    /// once the daemon assigned one, or `None` on failure. A repeated start
    /// replaces the session's stream.
    pub(crate) fn start(
        &mut self,
        session: String,
        size: (u32, u32),
        fps: u32,
        reply: oneshot::Sender<Option<u32>>,
    ) {
        self.casts.remove(&session);
        match self.create(size, fps, reply) {
            Ok(cast) => {
                self.casts.insert(session, cast);
            }
            Err(error) => tracing::warn!(%error, "ScreenCast stream failed"),
        }
    }

    fn create(
        &self,
        size: (u32, u32),
        fps: u32,
        reply: oneshot::Sender<Option<u32>>,
    ) -> Result<Cast> {
        let stream = pw::stream::StreamRc::new(
            self.core.clone(),
            "waywire-screencast",
            properties! {
                *pw::keys::MEDIA_CLASS => "Video/Source",
                *pw::keys::MEDIA_ROLE => "Screen",
                *pw::keys::NODE_NAME => "waywire-screencast",
                *pw::keys::NODE_DESCRIPTION => "waywire output",
            },
        )?;
        let state = Rc::new(CastState {
            reply: RefCell::new(Some(reply)),
            negotiated: Cell::new(None),
            pending: Cell::new(true),
            graph: Rc::clone(&self.graph),
        });
        let listener = stream
            .add_local_listener_with_user_data(Rc::clone(&state))
            .state_changed(|stream, state, _, new| {
                if let pw::stream::StreamState::Error(error) = &new {
                    tracing::warn!(error, "ScreenCast stream error");
                }
                let id = node_id(stream);
                let ready = matches!(
                    new,
                    pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming
                );
                if ready && let Some(id) = id {
                    state.graph.borrow_mut().sources.insert(id);
                }
                if (ready || matches!(new, pw::stream::StreamState::Error(_)))
                    && let Some(reply) = state.reply.borrow_mut().take()
                    && reply.send(id).is_err()
                {
                    tracing::debug!("ScreenCast requester hung up");
                }
                // A consumer that starts or resumes needs the current image.
                state.pending.set(true);
            })
            .param_changed(|stream, state, id, param| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    state.negotiated.set(None);
                    return;
                };
                let mut info = spa::param::video::VideoInfoRaw::new();
                if let Err(error) = info.parse(param) {
                    tracing::warn!(%error, "unparsable ScreenCast format");
                    return;
                }
                let size = info.size();
                state.negotiated.set(Some((size.width, size.height)));
                state.pending.set(true);
                let buffers = match buffers_param(size.width, size.height) {
                    Ok(buffers) => buffers,
                    Err(error) => {
                        tracing::warn!(%error, "ScreenCast buffer parameters");
                        return;
                    }
                };
                let Some(pod) = Pod::from_bytes(&buffers) else {
                    return;
                };
                if let Err(error) = stream.update_params(&mut [pod]) {
                    tracing::warn!(%error, "ScreenCast buffer parameters");
                }
            })
            .register()?;
        let format = format_param(size, fps)?;
        let pod = Pod::from_bytes(&format).ok_or_else(|| anyhow!("invalid format pod"))?;
        stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )?;
        Ok(Cast {
            _listener: listener,
            stream,
            state,
        })
    }

    /// Destroy the stream for portal `session`, ending its consumers.
    pub(crate) fn close(&mut self, session: &str) {
        self.casts.remove(session);
    }

    /// Advertise the new output size; frames pause until the consumer
    /// renegotiates.
    pub(crate) fn resize(&self, size: (u32, u32), fps: u32) {
        let format = match format_param(size, fps) {
            Ok(format) => format,
            Err(error) => {
                tracing::warn!(%error, "ScreenCast format");
                return;
            }
        };
        let Some(pod) = Pod::from_bytes(&format) else {
            return;
        };
        for cast in self.casts.values() {
            cast.state.negotiated.set(None);
            if let Err(error) = cast.stream.update_params(&mut [pod]) {
                tracing::warn!(%error, "ScreenCast renegotiation");
            }
        }
    }

    /// Offer a newly rendered frame to every stream.
    pub(crate) fn frame(&self, image: &Image<'static, 'static>) {
        for cast in self.casts.values() {
            cast.state.pending.set(true);
            cast.offer(image);
        }
    }

    /// Retry frames that found no free buffer, or that a consumer needs
    /// after starting or renegotiating.
    pub(crate) fn offer_pending(&self, image: &Image<'static, 'static>) {
        for cast in self.casts.values() {
            cast.offer(image);
        }
    }
}

/// Bind video consumer nodes and link those that target one of our streams.
fn watch_node(
    graph: &Rc<RefCell<Graph>>,
    registry: &pw::registry::RegistryRc,
    core: &pw::core::CoreRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
) {
    if global.type_ != pw::types::ObjectType::Node {
        return;
    }
    let Some(props) = global.props else {
        return;
    };
    if let Some(serial) = props.get("object.serial") {
        graph
            .borrow_mut()
            .serials
            .insert(global.id, serial.to_owned());
    }
    if props.get(*pw::keys::MEDIA_CLASS) != Some("Stream/Input/Video") {
        return;
    }
    let node: pw::node::Node = match registry.bind(global) {
        Ok(node) => node,
        Err(error) => {
            tracing::debug!(%error, "bind PipeWire consumer node");
            return;
        }
    };
    let consumer = global.id;
    let listener = node
        .add_listener_local()
        .info({
            let graph = Rc::downgrade(graph);
            let core = core.clone();
            move |info| {
                let Some(graph) = graph.upgrade() else {
                    return;
                };
                let Some(props) = info.props() else {
                    return;
                };
                let Some(target) = props
                    .get("target.object")
                    .or_else(|| props.get("node.target"))
                else {
                    return;
                };
                let mut graph = graph.borrow_mut();
                let Some(source) = graph.source_for(target) else {
                    return;
                };
                let Some(entry) = graph.consumers.get_mut(&consumer) else {
                    return;
                };
                if entry.link.is_some() {
                    return;
                }
                match core.create_object::<pw::link::Link>(
                    "link-factory",
                    &properties! {
                        "link.output.node" => source.to_string(),
                        "link.input.node" => consumer.to_string(),
                        "object.linger" => "false",
                    },
                ) {
                    Ok(link) => entry.link = Some(link),
                    Err(error) => tracing::warn!(%error, "link ScreenCast consumer"),
                }
            }
        })
        .register();
    graph.borrow_mut().consumers.insert(
        consumer,
        Consumer {
            _listener: listener,
            _node: node,
            link: None,
        },
    );
}

fn serialize(value: &Value) -> Result<Vec<u8>> {
    Ok(
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)?
            .0
            .into_inner(),
    )
}

/// The one format the rendered image has: its size in `BGRx` or `BGRA`, with a
/// variable frame rate up to the desktop's.
fn format_param((width, height): (u32, u32), fps: u32) -> Result<Vec<u8>> {
    use spa::param::format::FormatProperties;
    use spa::param::format::MediaSubtype;
    use spa::param::format::MediaType;
    use spa::param::video::VideoFormat;
    use spa::utils::Fraction;
    use spa::utils::Rectangle;

    serialize(&Value::Object(spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
        spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 1, denom: 1 },
            Fraction { num: fps, denom: 1 }
        ),
    )))
}

/// Single-plane shared-memory buffers holding one tightly packed frame.
fn buffers_param(width: u32, height: u32) -> Result<Vec<u8>> {
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("frame too wide"))?;
    let size = stride
        .checked_mul(height)
        .ok_or_else(|| anyhow!("frame too large"))?;
    let int = |value: u32| -> Result<Value> { Ok(Value::Int(i32::try_from(value)?)) };
    serialize(&Value::Object(spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 3,
                        min: 2,
                        max: 8,
                    },
                ))),
            ),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_size, int(size)?),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, int(stride)?),
        ],
    }))
}

#[cfg(test)]
mod tests {
    use smithay::reexports::pixman;

    use super::*;

    /// What the test consumer has seen.
    #[derive(Default)]
    struct Seen {
        size: Option<(u32, u32)>,
        frame: Option<((u32, u32), Vec<u8>)>,
    }

    fn solid(width: usize, height: usize, argb: u32) -> Image<'static, 'static> {
        let mut image =
            Image::new(pixman::FormatCode::A8R8G8B8, width, height, true).expect("test image");
        image
            .fill_boxes(
                pixman::Operation::Src,
                argb,
                &[pixman::Box32 {
                    x1: 0,
                    y1: 0,
                    x2: i32::try_from(width).expect("width"),
                    y2: i32::try_from(height).expect("height"),
                }],
            )
            .expect("fill test image");
        image
    }

    /// Iterate both loops until `done` holds or five seconds pass.
    fn pump(
        streams: &Streams,
        consumer: &pw::main_loop::MainLoopRc,
        image: &Image<'static, 'static>,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            streams.dispatch();
            streams.offer_pending(image);
            consumer
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(5)));
            if done() {
                return true;
            }
        }
        false
    }

    /// A portal client connected through the stream's node ID receives the
    /// rendered pixels at the output size, follows a resize, and loses the
    /// stream when the session closes.
    #[expect(clippy::too_many_lines, reason = "one end-to-end stream lifetime")]
    #[test]
    fn stream_serves_the_output_and_follows_its_size() {
        let runtime = tempfile::tempdir().expect("temporary runtime directory");
        // A missing daemon disables video instead of failing the desktop.
        assert!(Daemon::start_program("/nonexistent/pipewire", runtime.path()).is_none());
        let Some(daemon) = Daemon::start(runtime.path()) else {
            // The compositor runs without ScreenCast video; so does the test.
            return;
        };
        let mut streams = Streams::connect(daemon.socket()).expect("connect");
        let (reply, mut node) = oneshot::channel();
        streams.start("/session/1".to_owned(), (64, 48), 30, reply);
        let image = solid(64, 48, 0xff33_6699);
        let consumer_loop = pw::main_loop::MainLoopRc::new(None).expect("consumer loop");
        let mut id = None;
        assert!(
            pump(&streams, &consumer_loop, &image, || {
                id = node.try_recv().ok().flatten();
                id.is_some()
            }),
            "Start returns a node"
        );
        let id = id.expect("node ID");

        let context = pw::context::ContextRc::new(&consumer_loop, None).expect("context");
        let core = context
            .connect_rc(Some(properties! {
                *pw::keys::REMOTE_NAME => daemon.socket().to_string_lossy().into_owned(),
            }))
            .expect("consumer connection");
        let consumer = pw::stream::StreamRc::new(
            core,
            "consumer",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                "target.object" => id.to_string(),
            },
        )
        .expect("consumer stream");
        let seen = Rc::new(RefCell::new(Seen::default()));
        let _listener = consumer
            .add_local_listener_with_user_data(Rc::clone(&seen))
            .param_changed(|_, seen, param_id, param| {
                if param_id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    return;
                };
                let mut info = spa::param::video::VideoInfoRaw::new();
                info.parse(param).expect("consumer format");
                let size = info.size();
                seen.borrow_mut().size = Some((size.width, size.height));
            })
            .process(|stream, seen| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(size) = seen.borrow().size else {
                    return;
                };
                let data = &mut buffer.datas_mut()[0];
                let length = usize::try_from(data.chunk().size()).expect("chunk size");
                let bytes = data.data().expect("mapped buffer")[..length].to_vec();
                seen.borrow_mut().frame = Some((size, bytes));
            })
            .register()
            .expect("consumer listener");
        let format = format_param((1, 1), 30).expect("format");
        // Accept any size: the consumer must take what the source offers.
        let mut accept = spa::pod::deserialize::PodDeserializer::deserialize_any_from(&format)
            .expect("parse format")
            .1;
        if let Value::Object(object) = &mut accept {
            for property in &mut object.properties {
                if property.key == spa::sys::SPA_FORMAT_VIDEO_size {
                    property.value = Value::Choice(ChoiceValue::Rectangle(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Range {
                            default: spa::utils::Rectangle {
                                width: 1,
                                height: 1,
                            },
                            min: spa::utils::Rectangle {
                                width: 1,
                                height: 1,
                            },
                            max: spa::utils::Rectangle {
                                width: 8192,
                                height: 8192,
                            },
                        },
                    )));
                }
            }
        }
        let accept = serialize(&accept).expect("consumer format");
        consumer
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut [Pod::from_bytes(&accept).expect("format pod")],
            )
            .expect("connect consumer");

        let frame = |seen: &Rc<RefCell<Seen>>| seen.borrow_mut().frame.take();
        let mut received = None;
        assert!(
            pump(&streams, &consumer_loop, &image, || {
                received = frame(&seen);
                received.is_some()
            }),
            "consumer receives a frame"
        );
        let (size, bytes) = received.take().expect("first frame");
        assert_eq!(size, (64, 48));
        assert_eq!(bytes.len(), 64 * 48 * 4);
        // Little-endian ARGB8888 is BGRA in memory.
        assert!(
            bytes
                .chunks(4)
                .all(|pixel| pixel == [0x99, 0x66, 0x33, 0xff])
        );

        streams.resize((32, 24), 30);
        let image = solid(32, 24, 0xff11_2233);
        streams.frame(&image);
        assert!(
            pump(&streams, &consumer_loop, &image, || {
                received = frame(&seen);
                received.as_ref().is_some_and(|(size, _)| *size == (32, 24))
            }),
            "consumer follows the resize"
        );
        let (_, bytes) = received.expect("resized frame");
        assert_eq!(bytes.len(), 32 * 24 * 4);
        assert!(
            bytes
                .chunks(4)
                .all(|pixel| pixel == [0x33, 0x22, 0x11, 0xff])
        );

        streams.close("/session/1");
        assert!(
            pump(&streams, &consumer_loop, &image, || {
                consumer.state() != pw::stream::StreamState::Streaming
            }),
            "closing the session ends the consumer's stream"
        );
        assert!(streams.graph.borrow().sources.is_empty());
    }
}
