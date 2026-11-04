//! Desktop audio for the browser viewer.
//!
//! Applications play to the private `PipeWire` daemon, most of them through
//! its `PulseAudio` protocol. This module owns the one sink they can reach: an
//! `Audio/Sink` stream node named `waywire`, served from its own thread so the
//! compositor loop and video never wait on audio. The daemon has no session
//! manager, so the thread links each application's playback ports to the
//! sink's ports of the same channel, a mono stream to both. The graph runs
//! only while a stream is linked, clocked by the daemon's dummy driver.
//!
//! Every 20 ms of the mix becomes one Opus packet from the system's libopus,
//! loaded at run time, sent as RTP payload type 111 to the gateway's loopback
//! RTP port beside the video. Without libopus or the daemon the desktop is
//! silent and everything else runs as before.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::c_int;
use std::ffi::c_void;
use std::net::Ipv4Addr;
use std::net::UdpSocket;
use std::path::Path;
use std::ptr::NonNull;
use std::rc::Rc;
use std::thread::JoinHandle;

use anyhow::Result;
use anyhow::anyhow;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;

/// RTP payload type of Opus packets on the gateway's RTP port. Video uses 96.
pub(crate) const PAYLOAD_TYPE: u8 = 111;
const SSRC: u32 = 0x5757_4155;
const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
/// Samples per channel in one 20 ms Opus frame.
const FRAME: u32 = 960;
/// Interleaved samples in one frame.
const FRAME_SAMPLES: usize = (FRAME * CHANNELS) as usize;
const BITRATE: i32 = 64_000;
/// The largest Opus packet for one frame is 1275 bytes.
const MAX_PACKET: usize = 1500;

/// The audio thread; dropping it stops the sink.
pub struct Audio {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Audio {
    /// Serve the desktop sink on the daemon at `socket` and send its Opus
    /// packets to the loopback `rtp_port`.
    pub fn start(socket: &Path, rtp_port: u16) -> Option<Self> {
        let socket = socket.to_path_buf();
        let (quit, receiver) = pw::channel::channel();
        let thread = std::thread::Builder::new()
            .name("audio".to_owned())
            .spawn(move || {
                if let Err(error) = run(&socket, rtp_port, receiver) {
                    tracing::warn!(%error, "desktop audio disabled");
                }
            });
        match thread {
            Ok(thread) => Some(Self {
                quit,
                thread: Some(thread),
            }),
            Err(error) => {
                tracing::warn!(%error, "could not start the audio thread; desktop audio disabled");
                None
            }
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        if self.quit.send(()).is_err() {
            tracing::debug!("audio thread already ended");
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("audio thread panicked");
        }
    }
}

fn run(socket: &Path, rtp_port: u16, quit: pw::channel::Receiver<()>) -> Result<()> {
    let encoder = Encoder::new()?;
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    udp.connect((Ipv4Addr::LOCALHOST, rtp_port))?;
    udp.set_nonblocking(true)?;
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(Some(properties! {
        *pw::keys::REMOTE_NAME => socket.to_string_lossy().into_owned(),
    }))?;
    let _quit = quit.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |()| main_loop.quit()
    });
    let graph = Rc::new(RefCell::new(Graph::default()));
    let registry = core.get_registry_rc()?;
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let graph = Rc::clone(&graph);
            let core = core.clone();
            move |global| {
                let mut graph = graph.borrow_mut();
                graph.add(global);
                graph.link(&core);
            }
        })
        .global_remove({
            let graph = Rc::clone(&graph);
            move |id| graph.borrow_mut().remove(id)
        })
        .register();
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "waywire-audio",
        properties! {
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => "waywire",
            *pw::keys::NODE_DESCRIPTION => "waywire desktop audio",
            "adapter.auto-port-config" =>
                "{ mode = dsp monitor = false control = false position = preserve }",
        },
    )?;
    let sender = Sender {
        encoder,
        udp,
        samples: Vec::with_capacity(FRAME_SAMPLES * 2),
        sequence: 0,
        timestamp: 0,
    };
    let _listener = stream
        .add_local_listener_with_user_data(sender)
        .state_changed({
            let graph = Rc::clone(&graph);
            let core = core.clone();
            move |stream, sender, _, new| {
                if let pw::stream::StreamState::Error(error) = &new {
                    tracing::warn!(error, "audio sink error");
                }
                // A paused graph leaves a partial frame that would click
                // against the next stream's first samples.
                if matches!(new, pw::stream::StreamState::Paused) {
                    sender.samples.clear();
                }
                let id = Some(stream.node_id()).filter(|id| *id != spa::sys::SPA_ID_INVALID);
                let mut graph = graph.borrow_mut();
                if graph.sink != id {
                    graph.sink = id;
                    graph.link(&core);
                }
            }
        })
        .process(|stream, sender| sender.process(stream))
        .register()?;
    let format = format_param()?;
    let pod = Pod::from_bytes(&format).ok_or_else(|| anyhow!("invalid format pod"))?;
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [pod],
    )?;
    tracing::info!("desktop audio sink ready");
    main_loop.run();
    if let Err(error) = stream.disconnect() {
        tracing::debug!(%error, "disconnect audio sink");
    }
    Ok(())
}

/// Interleaved 48 kHz stereo floats, the encoder's input.
fn format_param() -> Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )?
    .0
    .into_inner())
}

/// A port of an audio node, from its registry properties.
struct Port {
    node: u32,
    direction: spa::utils::Direction,
    channel: String,
}

/// What the linker knows about the audio graph.
#[derive(Default)]
struct Graph {
    sink: Option<u32>,
    /// Application playback streams.
    playback: HashSet<u32>,
    ports: HashMap<u32, Port>,
    /// Links this thread made, by output and input port.
    links: HashMap<(u32, u32), pw::link::Link>,
}

impl Graph {
    fn add(&mut self, global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) {
        let Some(props) = global.props else {
            return;
        };
        if global.type_ == pw::types::ObjectType::Node {
            if props.get(*pw::keys::MEDIA_CLASS) == Some("Stream/Output/Audio") {
                self.playback.insert(global.id);
            }
        } else if global.type_ == pw::types::ObjectType::Port
            && let (Some(node), Some(direction), Some(channel)) = (
                props.get(*pw::keys::NODE_ID).and_then(|id| id.parse().ok()),
                props.get(*pw::keys::PORT_DIRECTION),
                props.get(*pw::keys::AUDIO_CHANNEL),
            )
        {
            self.ports.insert(
                global.id,
                Port {
                    node,
                    // PipeWire sets port.direction to "in" or "out".
                    direction: if direction == "out" {
                        spa::utils::Direction::Output
                    } else {
                        spa::utils::Direction::Input
                    },
                    channel: channel.to_owned(),
                },
            );
        }
    }

    fn remove(&mut self, id: u32) {
        self.playback.remove(&id);
        self.ports.remove(&id);
        self.links
            .retain(|(output, input), _| *output != id && *input != id);
    }

    /// Link every playback port to the sink's port of its channel.
    fn link(&mut self, core: &pw::core::CoreRc) {
        let Some(sink) = self.sink else {
            return;
        };
        let inputs: Vec<_> = self
            .ports
            .iter()
            .filter(|(_, port)| port.node == sink && port.direction == spa::utils::Direction::Input)
            .collect();
        let mut wanted = Vec::new();
        for (&output, port) in &self.ports {
            if port.direction != spa::utils::Direction::Output
                || !self.playback.contains(&port.node)
            {
                continue;
            }
            for &(&input, sink_port) in &inputs {
                if (port.channel == sink_port.channel || port.channel == "MONO")
                    && !self.links.contains_key(&(output, input))
                {
                    wanted.push((port.node, output, input));
                }
            }
        }
        for (node, output, input) in wanted {
            match core.create_object::<pw::link::Link>(
                "link-factory",
                &properties! {
                    "link.output.node" => node.to_string(),
                    "link.output.port" => output.to_string(),
                    "link.input.node" => sink.to_string(),
                    "link.input.port" => input.to_string(),
                    "object.linger" => "false",
                },
            ) {
                Ok(link) => {
                    self.links.insert((output, input), link);
                }
                Err(error) => tracing::warn!(%error, "link audio stream"),
            }
        }
    }
}

/// Collects the mix into 20 ms frames and sends each as an RTP packet.
struct Sender {
    encoder: Encoder,
    udp: UdpSocket,
    samples: Vec<f32>,
    sequence: u16,
    timestamp: u32,
}

impl Sender {
    /// Take the sink's next buffer of the mix.
    fn process(&mut self, stream: &pw::stream::Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else {
            return;
        };
        let Some(data) = buffer.datas_mut().first_mut() else {
            return;
        };
        let chunk = data.chunk();
        let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
        if let Some(bytes) = data
            .data()
            .and_then(|bytes| bytes.get(offset..offset + size))
        {
            self.push(bytes);
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.samples.extend(
            bytes
                .chunks_exact(4)
                .map(|sample| f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]])),
        );
        let mut packet = [0; 12 + MAX_PACKET];
        while self.samples.len() >= FRAME_SAMPLES {
            packet[0] = 0x80;
            packet[1] = PAYLOAD_TYPE;
            packet[2..4].copy_from_slice(&self.sequence.to_be_bytes());
            packet[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
            packet[8..12].copy_from_slice(&SSRC.to_be_bytes());
            match self
                .encoder
                .encode(&self.samples[..FRAME_SAMPLES], &mut packet[12..])
            {
                Ok(length) => {
                    // The gateway may be busy or gone; audio never waits.
                    if let Err(error) = self.udp.send(&packet[..12 + length]) {
                        tracing::debug!(%error, "send audio packet");
                    }
                }
                Err(error) => tracing::warn!(%error, "Opus encode"),
            }
            self.sequence = self.sequence.wrapping_add(1);
            self.timestamp = self.timestamp.wrapping_add(FRAME);
            self.samples.drain(..FRAME_SAMPLES);
        }
    }
}

type EncoderCreate = unsafe extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut c_void;
type EncodeFloat = unsafe extern "C" fn(*mut c_void, *const f32, c_int, *mut u8, i32) -> i32;
type EncoderCtl = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type EncoderDestroy = unsafe extern "C" fn(*mut c_void);

const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;

/// A stereo 48 kHz Opus encoder from the system's libopus.
struct Encoder {
    state: NonNull<c_void>,
    encode: EncodeFloat,
    destroy: EncoderDestroy,
    _library: libloading::Library,
}

impl Encoder {
    fn new() -> Result<Self> {
        // SAFETY: libopus runs no initialisation code with preconditions.
        let library = unsafe { libloading::Library::new("libopus.so.0") }
            .map_err(|error| anyhow!("libopus not available: {error}"))?;
        // SAFETY: the types match the declarations in opus.h.
        let create = unsafe { *library.get::<EncoderCreate>(b"opus_encoder_create\0")? };
        // SAFETY: as above.
        let encode = unsafe { *library.get::<EncodeFloat>(b"opus_encode_float\0")? };
        // SAFETY: as above.
        let ctl = unsafe { *library.get::<EncoderCtl>(b"opus_encoder_ctl\0")? };
        // SAFETY: as above.
        let destroy = unsafe { *library.get::<EncoderDestroy>(b"opus_encoder_destroy\0")? };
        let mut error = 0;
        // SAFETY: a supported rate, channel count and application, and a valid
        // error pointer.
        let state = unsafe {
            create(
                RATE.cast_signed(),
                CHANNELS.cast_signed(),
                OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &raw mut error,
            )
        };
        let state =
            NonNull::new(state).ok_or_else(|| anyhow!("opus_encoder_create failed: {error}"))?;
        let encoder = Self {
            state,
            encode,
            destroy,
            _library: library,
        };
        // SAFETY: the request takes one opus_int32 argument.
        let result = unsafe { ctl(encoder.state.as_ptr(), OPUS_SET_BITRATE_REQUEST, BITRATE) };
        if result != 0 {
            return Err(anyhow!("OPUS_SET_BITRATE failed: {result}"));
        }
        Ok(encoder)
    }

    /// Encode one frame of interleaved samples into `out`, returning the
    /// packet length.
    fn encode(&self, frame: &[f32], out: &mut [u8]) -> Result<usize> {
        debug_assert_eq!(frame.len(), FRAME_SAMPLES);
        // SAFETY: `frame` holds one frame for both channels and `out` is
        // writable for the length given.
        let length = unsafe {
            (self.encode)(
                self.state.as_ptr(),
                frame.as_ptr(),
                FRAME.cast_signed(),
                out.as_mut_ptr(),
                i32::try_from(out.len())?,
            )
        };
        usize::try_from(length).map_err(|_negative| anyhow!("opus_encode_float failed: {length}"))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the state came from opus_encoder_create and is not used again.
        unsafe { (self.destroy)(self.state.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;
    use crate::Daemon;

    const TONE_HZ: f32 = 1000.0;

    type DecoderCreate = unsafe extern "C" fn(i32, c_int, *mut c_int) -> *mut c_void;
    type DecodeFloat =
        unsafe extern "C" fn(*mut c_void, *const u8, i32, *mut f32, c_int, c_int) -> c_int;

    /// Decode `packets` to interleaved stereo samples with libopus.
    fn decode(library: &libloading::Library, packets: &[Vec<u8>]) -> Vec<f32> {
        // SAFETY: the types match the declarations in opus.h.
        let create = unsafe {
            *library
                .get::<DecoderCreate>(b"opus_decoder_create\0")
                .expect("create")
        };
        // SAFETY: as above.
        let decode = unsafe {
            *library
                .get::<DecodeFloat>(b"opus_decode_float\0")
                .expect("decode")
        };
        let mut error = 0;
        // SAFETY: a supported rate and channel count, and a valid error pointer.
        let state = unsafe { create(48_000, 2, &raw mut error) };
        assert!(!state.is_null(), "opus_decoder_create failed: {error}");
        let mut samples = Vec::new();
        let mut frame = vec![0.0; FRAME_SAMPLES];
        for packet in packets {
            // SAFETY: `packet` and `frame` are valid for the lengths given,
            // and `frame` holds one 20 ms stereo frame. The decoder is leaked
            // with the test process.
            let length = unsafe {
                decode(
                    state,
                    packet.as_ptr(),
                    i32::try_from(packet.len()).expect("packet length"),
                    frame.as_mut_ptr(),
                    FRAME.cast_signed(),
                    0,
                )
            };
            let length = usize::try_from(length).expect("decoded frame") * 2;
            samples.extend_from_slice(&frame[..length]);
        }
        samples
    }

    /// Power of `samples` at `hz`, by the Goertzel algorithm.
    fn power(samples: &[f32], hz: f32) -> f32 {
        let coefficient = 2.0 * (TAU * hz / 48_000.0).cos();
        let (mut previous, mut before) = (0.0, 0.0);
        for sample in samples {
            let current = sample + coefficient * previous - before;
            before = previous;
            previous = current;
        }
        previous * previous + before * before - coefficient * previous * before
    }

    /// A stream an application plays reaches the gateway's RTP port as Opus
    /// packets that decode to what it played.
    #[test]
    fn playback_reaches_the_rtp_port_as_opus() {
        let runtime = tempfile::tempdir().expect("temporary runtime directory");
        let Some(daemon) = Daemon::start(runtime.path()) else {
            // Without the daemon the desktop is silent; so is the test.
            return;
        };
        // SAFETY: loading libopus runs no initialisation with preconditions.
        let Ok(opus) = (unsafe { libloading::Library::new("libopus.so.0") }) else {
            return;
        };
        let gateway = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("RTP socket");
        gateway
            .set_read_timeout(Some(Duration::from_millis(5)))
            .expect("read timeout");
        let port = gateway.local_addr().expect("RTP address").port();
        let _audio = Audio::start(daemon.socket(), port).expect("audio thread");

        let main_loop = pw::main_loop::MainLoopRc::new(None).expect("player loop");
        let context = pw::context::ContextRc::new(&main_loop, None).expect("context");
        let core = context
            .connect_rc(Some(properties! {
                *pw::keys::REMOTE_NAME => daemon.socket().to_string_lossy().into_owned(),
            }))
            .expect("player connection");
        let player = pw::stream::StreamRc::new(
            core,
            "player",
            properties! {
                *pw::keys::MEDIA_CLASS => "Stream/Output/Audio",
                "adapter.auto-port-config" =>
                    "{ mode = dsp monitor = false control = false position = preserve }",
            },
        )
        .expect("player stream");
        let _listener = player
            .add_local_listener_with_user_data(0_u32)
            .process(|stream, position| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let data = &mut buffer.datas_mut()[0];
                let Some(bytes) = data.data() else {
                    return;
                };
                let frames = (bytes.len() / 8).min(FRAME as usize);
                for frame in bytes.chunks_exact_mut(8).take(frames) {
                    #[expect(clippy::cast_precision_loss, reason = "a short test tone")]
                    let sample = 0.5 * (TAU * TONE_HZ * *position as f32 / 48_000.0).sin();
                    frame[..4].copy_from_slice(&sample.to_le_bytes());
                    frame[4..].copy_from_slice(&sample.to_le_bytes());
                    *position += 1;
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = 8;
                *chunk.size_mut() = u32::try_from(frames * 8).expect("chunk size");
            })
            .register()
            .expect("player listener");
        let format = format_param().expect("format");
        player
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::MAP_BUFFERS,
                &mut [Pod::from_bytes(&format).expect("format pod")],
            )
            .expect("connect player");

        // Half a second of audio, after the encoder's first frames settle.
        let mut packets = Vec::new();
        let mut packet = [0; 2048];
        let deadline = Instant::now() + Duration::from_secs(10);
        while packets.len() < 30 && Instant::now() < deadline {
            main_loop
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(1)));
            if let Ok(length) = gateway.recv(&mut packet) {
                packets.push(packet[..length].to_vec());
            }
        }
        assert_eq!(packets.len(), 30, "the sink sends a packet per 20 ms");
        for (index, packet) in packets.iter().enumerate() {
            assert_eq!(packet[..2], [0x80, PAYLOAD_TYPE]);
            let sequence = u16::from_be_bytes([packet[2], packet[3]]);
            let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
            assert_eq!(sequence, u16::try_from(index).expect("sequence"));
            assert_eq!(timestamp, FRAME * u32::try_from(index).expect("timestamp"));
        }
        let payloads: Vec<_> = packets.iter().map(|packet| packet[12..].to_vec()).collect();
        let samples = decode(&opus, &payloads);
        let left: Vec<f32> = samples[samples.len() / 2..]
            .iter()
            .step_by(2)
            .copied()
            .collect();
        let tone = power(&left, TONE_HZ);
        for other in [500.0, 1500.0, 3000.0] {
            assert!(
                tone > 1000.0 * power(&left, other),
                "{TONE_HZ} Hz is not dominant: {tone} against {} at {other} Hz",
                power(&left, other)
            );
        }
    }
}
