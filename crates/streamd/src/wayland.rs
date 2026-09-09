mod capture;
mod clipboard;
mod cursor;
pub mod input;
mod output;

use std::fs::File;
use std::io;
use std::io::Read;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use calloop::EventLoop;
use calloop::Interest;
use calloop::LoopHandle;
use calloop::Mode;
use calloop::PostAction;
use calloop::channel::Event as ChannelEvent;
use calloop::channel::{
    self,
};
use calloop::generic::Generic;
use calloop::signals::Signal;
use calloop::signals::Signals;
use calloop_wayland_source::WaylandSource;
use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use wayland_client::Connection;
use wayland_client::Dispatch;
use wayland_client::QueueHandle;
use wayland_client::WEnum;
use wayland_client::backend::WaylandError;
use wayland_client::delegate_noop;
use wayland_client::event_created_child;
use wayland_client::protocol::wl_buffer;
use wayland_client::protocol::wl_output;
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_shm_pool;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_cursor_session_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_head_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_head_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_mode_v1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1;

use self::capture::Capture;
use self::clipboard::Clipboard;
use self::cursor::Cursor;
use self::input::Input;
use self::output::Head;
use self::output::OutputManager;
use self::output::OutputMode;
use self::output::ResizeRequest;
use crate::Options;
use crate::event_writer::EventSink;
use crate::event_writer::EventWriter;
use crate::protocol::Command;
use crate::protocol::CommandDecoder;
use crate::protocol::Event;
use crate::video::Notification;
use crate::video::VideoEncoder;

pub(super) enum ControlMessage {
    ClipboardReceived {
        generation: u64,
        result: std::result::Result<String, String>,
    },
}

pub struct State {
    qh: QueueHandle<Self>,
    running: bool,
    failure: Option<anyhow::Error>,
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    output_name: Option<String>,
    capture: Capture,
    input: Input,
    outputs: OutputManager,
    clipboard: Clipboard,
    cursor: Cursor,
    video: Option<VideoEncoder>,
    event_sink: EventSink,
    generation: u32,
    acknowledged_generation: u32,
    latest_input_sequence: u32,
    fps: u32,
    bitrate_kbps: u32,
    encoded_scale: u32,
}

pub fn run(options: Options) -> Result<()> {
    // Signals::new blocks these signals in this thread. Do this before any
    // native worker starts so every worker inherits the blocked mask.
    let signal_source =
        Signals::new(&[Signal::SIGINT, Signal::SIGTERM]).context("register process signals")?;
    let connection = Connection::connect_to_env().context("connect to Wayland compositor")?;
    let mut event_queue = connection.new_event_queue();
    let qh = event_queue.handle();
    let (event_writer, event_sink) = EventWriter::start().context("start stdout event writer")?;
    let mut video = VideoEncoder::start(options.ffmpeg, options.rtp_port)?;
    let notification_source = video
        .take_notification_source()
        .context("encoder notification source is already registered")?;
    let (internal_sender, internal_channel) = channel::sync_channel(8);
    let mut state = State {
        qh: qh.clone(),
        running: true,
        failure: None,
        shm: None,
        output: None,
        output_name: None,
        capture: Capture::new(),
        input: Input::new(options.xkb_layout),
        outputs: OutputManager::new(),
        clipboard: Clipboard::new(internal_sender),
        cursor: Cursor::new(event_sink.clone()),
        video: Some(video),
        event_sink,
        generation: 1,
        acknowledged_generation: 0,
        latest_input_sequence: 0,
        fps: options.frame_rate,
        bitrate_kbps: options.bitrate,
        encoded_scale: 100,
    };

    connection.display().get_registry(&qh, ());
    event_queue
        .roundtrip(&mut state)
        .context("discover Wayland globals")?;
    event_queue
        .roundtrip(&mut state)
        .context("receive initial Wayland global state")?;
    state.start_native()?;
    // Creating the virtual pointer changes fresh-seat capabilities required by cursor capture.
    event_queue
        .roundtrip(&mut state)
        .context("receive virtual input seat capabilities")?;
    state.start_cursor()?;

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(connection, event_queue).insert(handle.clone())?;
    handle
        .insert_source(internal_channel, |event, (), state| {
            if let ChannelEvent::Msg(message) = event {
                state.handle_internal(message);
            }
        })
        .map_err(|_err| anyhow!("register clipboard completion source"))?;

    register_stdin(&handle)?;
    handle
        .insert_source(notification_source, |(), (), state| state.drain_video())
        .map_err(|_err| anyhow!("register encoder notification source"))?;
    handle.insert_source(signal_source, |event, (), state| {
        if matches!(event.signal(), Signal::SIGINT | Signal::SIGTERM) {
            state.stop();
        }
    })?;

    state.request_capture()?;
    while state.running {
        if let Err(error) = event_loop.dispatch(None, &mut state) {
            state.fail(error.into());
        }
        if let Some(error) = event_writer.failure() {
            state.fail(error.into());
        }
    }
    drop(state.input.release_all());
    state.clipboard.cancel_transfers();
    if let Some(video) = state.video.take() {
        video.stop();
    }
    event_writer.stop();
    if let Some(error) = state.failure {
        Err(error)
    } else {
        Ok(())
    }
}

fn register_stdin(handle: &LoopHandle<'_, State>) -> Result<()> {
    let stdin = File::open("/dev/stdin").context("open daemon command pipe")?;
    let flags = OFlag::from_bits_truncate(fcntl(&stdin, FcntlArg::F_GETFL)?);
    fcntl(&stdin, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    let mut decoder = Some(CommandDecoder::new());
    handle.insert_source(
        Generic::new(stdin, Interest::READ, Mode::Level),
        move |readiness, input, state| {
            if readiness.error {
                state.fail(anyhow!("command pipe reported an error"));
                return Ok(PostAction::Remove);
            }
            let mut part = vec![0_u8; 64 * 1024];
            loop {
                // SAFETY: reading keeps the registered File in place; it is neither replaced nor dropped.
                match unsafe { input.get_mut() }.read(&mut part) {
                    Ok(0) => {
                        if let Some(decoder) = decoder.take()
                            && let Err(error) = decoder.finish()
                        {
                            state.fail(error.into());
                        } else {
                            state.stop();
                        }
                        return Ok(PostAction::Remove);
                    }
                    Ok(count) => {
                        let Some(decoder) = decoder.as_mut() else {
                            state.stop();
                            return Ok(PostAction::Remove);
                        };
                        match decoder.push(&part[..count]) {
                            Ok(commands) => {
                                for command in commands {
                                    if let Err(error) = state.apply_command(&command) {
                                        state.fail(error);
                                        return Ok(PostAction::Remove);
                                    }
                                }
                                // Yield after one bounded read so a command flood cannot starve
                                // Wayland dispatch, encoder notifications, or shutdown signals.
                                break;
                            }
                            Err(error) => {
                                state.fail(error.into());
                                return Ok(PostAction::Remove);
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        state.fail(error.into());
                        return Ok(PostAction::Remove);
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    )?;
    Ok(())
}

impl State {
    fn start_native(&mut self) -> Result<()> {
        let output = self.output.clone().context("compositor has no wl_output")?;
        let seat = self
            .input
            .seat
            .clone()
            .context("compositor has no wl_seat")?;
        self.shm.as_ref().context("compositor has no wl_shm")?;
        self.capture
            .manager
            .as_ref()
            .context("compositor lacks wlroots screencopy")?;
        self.input.start(&output, &self.qh)?;
        self.clipboard.start(&seat, &self.qh)?;
        Ok(())
    }

    fn start_cursor(&mut self) -> Result<()> {
        let output = self.output.clone().context("compositor has no wl_output")?;
        let seat = self
            .input
            .seat
            .clone()
            .context("compositor has no wl_seat")?;
        self.cursor.start(&seat, &output, &self.qh)
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn fail(&mut self, error: anyhow::Error) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        self.running = false;
    }

    fn request_capture(&mut self) -> Result<()> {
        let output = self.output.as_ref().context("output disappeared")?;
        self.capture.request(output, &self.qh)?;
        Ok(())
    }

    fn replace_media_generation(&mut self) -> Result<()> {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.acknowledged_generation = 0;
        if let Some(video) = &self.video {
            video.set_generation(self.generation)?;
        }
        Ok(())
    }

    fn advance_generation(&mut self) -> Result<()> {
        self.replace_media_generation()?;
        self.capture.cancel();
        self.request_capture()
    }

    fn apply_command(&mut self, command: &Command) -> Result<()> {
        match command {
            Command::Resize {
                width,
                height,
                scale_v120,
                request_id,
            } => {
                self.capture.cancel();
                if let Err(error) = self.outputs.configure(
                    self.output_name.as_deref(),
                    OutputMode {
                        width: *width,
                        height: *height,
                        scale_v120: *scale_v120,
                    },
                    *request_id,
                    self.fps,
                    &self.qh,
                ) {
                    self.request_capture()?;
                    return Err(error);
                }
            }
            Command::Clipboard(text) => self.clipboard.set_text(text.clone(), &self.qh)?,
            Command::Quality {
                bitrate_kbps,
                fps,
                scale_percent,
            } => {
                if (*bitrate_kbps, *fps, *scale_percent)
                    != (self.bitrate_kbps, self.fps, self.encoded_scale)
                {
                    self.bitrate_kbps = *bitrate_kbps;
                    self.fps = *fps;
                    self.encoded_scale = *scale_percent;
                    self.advance_generation()?;
                }
            }
            Command::KeyframeReadiness { generation, ready } => {
                if *generation == self.generation {
                    if *ready {
                        self.acknowledged_generation = *generation;
                    } else if self.acknowledged_generation == *generation {
                        self.acknowledged_generation = 0;
                        self.capture.cancel();
                        self.request_capture()?;
                    }
                }
            }
            Command::PointerRelative { .. }
            | Command::PointerAbsolute { .. }
            | Command::PointerButton { .. }
            | Command::PointerScroll { .. }
            | Command::KeyboardKey { .. }
            | Command::ReleaseAll
            | Command::Text { .. } => {
                let overlay = matches!(command, Command::PointerRelative { .. });
                let disables_overlay = matches!(
                    command,
                    Command::PointerAbsolute { .. } | Command::ReleaseAll
                );
                if (overlay && !self.capture.cursor_overlay)
                    || (disables_overlay && self.capture.cursor_overlay)
                {
                    self.capture.cursor_overlay = overlay;
                    self.capture.cancel();
                    self.capture.can_wait_for_damage = false;
                    self.request_capture()?;
                }
                self.input.apply(command)?;
                if let Some(sequence) = command.input_sequence() {
                    self.latest_input_sequence = sequence;
                }
            }
        }
        Ok(())
    }

    fn handle_internal(&mut self, message: ControlMessage) {
        match message {
            ControlMessage::ClipboardReceived { generation, result } => {
                if self.clipboard.accepts_transfer(generation) {
                    match result {
                        Ok(text) => {
                            if let Err(error) = self.event_sink.send(&Event::Clipboard(text)) {
                                self.fail(error.into());
                            }
                        }
                        Err(error) => {
                            eprintln!("sprite-desktop-streamd: clipboard read failed: {error}");
                        }
                    }
                }
            }
        }
    }

    fn drain_video(&mut self) {
        loop {
            let notification = self.video.as_ref().and_then(VideoEncoder::try_notification);
            match notification {
                Some(Notification::Submitted(metadata)) => {
                    // A completed pipe write owns one correlation record even
                    // if the Wayland generation changed before this queue was
                    // drained. Its RTP access unit may already be queued at the
                    // gateway; dropping the record lets that old unit consume
                    // metadata from the replacement encoder.
                    if let Err(error) = self.event_sink.send(&Event::Frame(metadata)) {
                        self.fail(error.into());
                        return;
                    }
                }
                Some(Notification::RestartRequired { generation })
                    if generation == self.generation =>
                {
                    if let Err(error) = self.advance_generation() {
                        self.fail(error);
                        return;
                    }
                }
                Some(Notification::RestartRequired { .. }) => {}
                Some(Notification::Fatal(error)) => {
                    self.fail(anyhow!(error));
                    return;
                }
                None => return,
            }
        }
    }

    fn resize_succeeded(&mut self) -> Result<()> {
        let applied = self
            .outputs
            .take_succeeded()
            .context("unexpected resize success")?;
        if applied.dimensions_changed {
            self.replace_media_generation()?;
        }
        self.capture.can_wait_for_damage = false;
        self.event_sink.send(&Event::ResizeApplied {
            request_id: applied.request_id,
            width: applied.mode.width,
            height: applied.mode.height,
            scale_v120: applied.mode.scale_v120,
            generation: self.generation,
        })?;
        self.start_queued_resize_or_capture()
    }

    fn start_queued_resize_or_capture(&mut self) -> Result<()> {
        let Some(ResizeRequest { mode, request_id }) = self.outputs.take_ready_queued() else {
            return if self.outputs.has_queued() {
                Ok(())
            } else {
                self.request_capture()
            };
        };
        self.outputs.configure(
            self.output_name.as_deref(),
            mode,
            request_id,
            self.fps,
            &self.qh,
        )
    }
}

// Core Wayland registry, output, and seat protocols.
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_shm" if state.shm.is_none() => {
                state.shm = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_output" if state.output.is_none() => {
                state.output = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "wl_seat" if state.input.seat.is_none() => {
                state.input.seat = Some(registry.bind(name, version.min(9), qh, ()));
            }
            "zwlr_screencopy_manager_v1" if state.capture.manager.is_none() => {
                state.capture.manager = Some(registry.bind(name, version.min(3), qh, ()));
            }
            "zwlr_virtual_pointer_manager_v1" if state.input.pointer_manager.is_none() => {
                state.input.pointer_manager = Some(registry.bind(name, version.min(2), qh, ()));
            }
            "zwp_virtual_keyboard_manager_v1" if state.input.keyboard_manager.is_none() => {
                state.input.keyboard_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "zwp_input_method_manager_v2" if state.input.method_manager.is_none() => {
                state.input.method_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "zwlr_output_manager_v1" if state.outputs.manager.is_none() => {
                state.outputs.manager = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "ext_data_control_manager_v1" if state.clipboard.manager.is_none() => {
                state.clipboard.manager = Some(registry.bind(name, 1, qh, ()));
            }
            "ext_output_image_capture_source_manager_v1"
                if state.cursor.source_manager.is_none() =>
            {
                state.cursor.source_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "ext_image_copy_capture_manager_v1" if state.cursor.capture_manager.is_none() => {
                state.cursor.capture_manager = Some(registry.bind(name, 1, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.output_name = Some(name);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            state.cursor.seat_capabilities(capabilities);
        }
    }
}

fn capture_flush(result: std::result::Result<(), WaylandError>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error).context("flush next Wayland capture request"),
    }
}

// wlroots output screencopy protocol.
impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        (): &(),
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if state.capture.frame.as_ref() != Some(proxy) {
            return;
        }
        let result = match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => match (format, state.shm.clone()) {
                (WEnum::Value(format), Some(shm)) => state
                    .capture
                    .set_constraints(&shm, width, height, stride, format, qh),
                _ => Err(anyhow!("unsupported screencopy SHM format")),
            },
            zwlr_screencopy_frame_v1::Event::BufferDone => (|| -> Result<()> {
                let wait = state.capture.can_wait_for_damage
                    && state.acknowledged_generation == state.generation;
                state.capture.begin_copy(wait)?;
                capture_flush(connection.flush())
            })(),
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if flags == WEnum::Value(zwlr_screencopy_frame_v1::Flags::empty()) {
                    Ok(())
                } else {
                    Err(anyhow!("screencopy transform is unsupported"))
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                // Frame metadata uses local callback time, not the compositor's clock.
                let capture_nanos = monotonic_nanos().context("read monotonic capture time");
                capture_nanos.and_then(|capture_nanos| {
                    protocol_ready_nanos(tv_sec_hi, tv_sec_lo, tv_nsec)
                        .context("invalid screencopy ready timestamp")?;
                    let sequence = state.capture.sequence;
                    state.capture.cancel();
                    sequence
                        .checked_add(1)
                        .context("frame sequence exhausted")?;
                    state.request_capture()?;
                    capture_flush(connection.flush())?;
                    // capture_output only announces the next frame's constraints.
                    // Its BufferDone cannot dispatch until this callback returns, so
                    // the current single SHM mapping remains stable during submit.
                    let frame = state.capture.completed_frame(
                        capture_nanos,
                        state.generation,
                        state.latest_input_sequence,
                        state.fps,
                        state.bitrate_kbps,
                        state.encoded_scale,
                    )?;
                    state
                        .video
                        .as_ref()
                        .context("video encoder missing")?
                        .submit(frame)?;
                    Ok(())
                })
            }
            zwlr_screencopy_frame_v1::Event::Failed => Err(anyhow!("Wayland screencopy failed")),
            zwlr_screencopy_frame_v1::Event::Damage { .. }
            | zwlr_screencopy_frame_v1::Event::LinuxDmabuf { .. }
            | _ => Ok(()),
        };
        if let Err(error) = result {
            state.fail(error);
        }
    }
}

// Input method protocol.
impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_input_method_v2::ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.input.method_event(&event);
    }
}

// wlroots output management protocols.
impl Dispatch<zwlr_output_manager_v1::ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_output_manager_v1::ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.outputs.heads.push(Head {
                proxy: head,
                name: None,
                enabled: false,
                finished: false,
            }),
            zwlr_output_manager_v1::Event::Done { serial } => {
                if state.outputs.publish_serial(serial)
                    && let Err(error) = state.start_queued_resize_or_capture()
                {
                    state.fail(error);
                }
            }
            zwlr_output_manager_v1::Event::Finished => state.outputs.finished = true,
            _ => {}
        }
    }
    event_created_child!(State, zwlr_output_manager_v1::ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (zwlr_output_head_v1::ZwlrOutputHeadV1, ())
    ]);
}

impl Dispatch<zwlr_output_head_v1::ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &zwlr_output_head_v1::ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(head) = state
            .outputs
            .heads
            .iter_mut()
            .find(|head| head.proxy == *proxy)
        else {
            return;
        };
        match event {
            zwlr_output_head_v1::Event::Name { name } => head.name = Some(name),
            zwlr_output_head_v1::Event::Enabled { enabled } => head.enabled = enabled != 0,
            zwlr_output_head_v1::Event::Finished => head.finished = true,
            zwlr_output_head_v1::Event::Description { .. }
            | zwlr_output_head_v1::Event::PhysicalSize { .. }
            | zwlr_output_head_v1::Event::Mode { .. }
            | zwlr_output_head_v1::Event::CurrentMode { .. }
            | zwlr_output_head_v1::Event::Position { .. }
            | zwlr_output_head_v1::Event::Transform { .. }
            | zwlr_output_head_v1::Event::Scale { .. }
            | zwlr_output_head_v1::Event::Make { .. }
            | zwlr_output_head_v1::Event::Model { .. }
            | zwlr_output_head_v1::Event::SerialNumber { .. }
            | zwlr_output_head_v1::Event::AdaptiveSync { .. }
            | _ => {}
        }
    }
    event_created_child!(State, zwlr_output_head_v1::ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (zwlr_output_mode_v1::ZwlrOutputModeV1, ())
    ]);
}

impl Dispatch<zwlr_output_configuration_v1::ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state
            .outputs
            .pending
            .as_ref()
            .is_none_or(|pending| pending.configuration != *proxy)
        {
            return;
        }
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => {
                if let Err(error) = state.resize_succeeded() {
                    state.fail(error);
                }
            }
            zwlr_output_configuration_v1::Event::Failed => {
                state.outputs.reject_pending();
                if let Err(error) = state.start_queued_resize_or_capture() {
                    state.fail(error);
                }
            }
            zwlr_output_configuration_v1::Event::Cancelled => {
                state.outputs.retry_cancelled();
                if let Err(error) = state.start_queued_resize_or_capture() {
                    state.fail(error);
                }
            }
            _ => {}
        }
    }
}

// External data control protocols.
impl Dispatch<ext_data_control_device_v1::ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_data_control_device_v1::ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let result = match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.clipboard.pending_offer = Some(id);
                Ok(())
            }
            ext_data_control_device_v1::Event::Selection { id } => state.clipboard.select(id),
            ext_data_control_device_v1::Event::Finished => Err(anyhow!("clipboard device stopped")),
            ext_data_control_device_v1::Event::PrimarySelection { .. } | _ => Ok(()),
        };
        if let Err(error) = result {
            eprintln!("sprite-desktop-streamd: clipboard event failed: {error}");
        }
    }
    event_created_child!(State, ext_data_control_device_v1::ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ext_data_control_offer_v1::ExtDataControlOfferV1, ())
    ]);
}

impl Dispatch<ext_data_control_offer_v1::ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ext_data_control_offer_v1::ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.clipboard.offer_mime(proxy, mime_type);
        }
    }
}

impl Dispatch<ext_data_control_source_v1::ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ext_data_control_source_v1::ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { fd, .. } => {
                if let Err(error) = state.clipboard.send_requested(proxy, fd) {
                    eprintln!("sprite-desktop-streamd: clipboard send failed: {error}");
                }
            }
            ext_data_control_source_v1::Event::Cancelled => state.clipboard.cancel_source(proxy),
            _ => {}
        }
    }
}

// External image-copy cursor capture protocols.
impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if state.cursor.session.as_ref() != Some(proxy) {
            return;
        }
        let result = match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.cursor.begin_constraints();
                state.cursor.batch_width = Some(width);
                state.cursor.batch_height = Some(height);
                Ok(())
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                state.cursor.begin_constraints();
                if format == WEnum::Value(wl_shm::Format::Argb8888) {
                    state.cursor.accept_argb();
                }
                Ok(())
            }
            ext_image_copy_capture_session_v1::Event::DmabufDevice { .. }
            | ext_image_copy_capture_session_v1::Event::DmabufFormat { .. } => {
                state.cursor.begin_constraints();
                Ok(())
            }
            ext_image_copy_capture_session_v1::Event::Done => match state.shm.clone() {
                Some(shm) => state.cursor.finish_constraints(&shm, qh),
                None => Err(anyhow!("wl_shm disappeared")),
            },
            ext_image_copy_capture_session_v1::Event::Stopped => {
                Err(anyhow!("cursor capture stopped"))
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            state.fail(error);
        }
    }
}

impl Dispatch<ext_image_copy_capture_cursor_session_v1::ExtImageCopyCaptureCursorSessionV1, ()>
    for State
{
    fn event(
        state: &mut Self,
        proxy: &ext_image_copy_capture_cursor_session_v1::ExtImageCopyCaptureCursorSessionV1,
        event: ext_image_copy_capture_cursor_session_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.cursor.pointer_session.as_ref() != Some(proxy) {
            return;
        }
        let result = match event {
            ext_image_copy_capture_cursor_session_v1::Event::Enter => state.cursor.visibility(true),
            ext_image_copy_capture_cursor_session_v1::Event::Leave => {
                state.cursor.visibility(false)
            }
            ext_image_copy_capture_cursor_session_v1::Event::Hotspot { x, y } => {
                state.cursor.hotspot(x, y);
                Ok(())
            }
            ext_image_copy_capture_cursor_session_v1::Event::Position { .. } | _ => Ok(()),
        };
        if let Err(error) = result {
            state.fail(error);
        }
    }
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if state.cursor.frame.as_ref() != Some(proxy) {
            return;
        }
        let result = match event {
            ext_image_copy_capture_frame_v1::Event::Transform { transform } => {
                state.cursor.transform(transform)
            }
            ext_image_copy_capture_frame_v1::Event::Ready => state.cursor.frame_ready(qh),
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                let changed = reason
                    == WEnum::Value(
                        ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints,
                    );
                match state.shm.clone() {
                    Some(shm) => state.cursor.frame_failed(changed, &shm, qh),
                    None => Err(anyhow!("wl_shm disappeared")),
                }
            }
            ext_image_copy_capture_frame_v1::Event::Damage { .. }
            | ext_image_copy_capture_frame_v1::Event::PresentationTime { .. }
            | _ => Ok(()),
        };
        if let Err(error) = result {
            state.fail(error);
        }
    }
}

fn monotonic_nanos() -> Option<u64> {
    let timestamp = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).ok()?;
    let seconds = u64::try_from(timestamp.tv_sec()).ok()?;
    let nanos = u64::try_from(timestamp.tv_nsec()).ok()?;
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}

fn protocol_ready_nanos(tv_sec_hi: u32, tv_sec_lo: u32, tv_nsec: u32) -> Option<u64> {
    if tv_nsec >= 1_000_000_000 {
        return None;
    }
    let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
    seconds
        .checked_mul(1_000_000_000)?
        .checked_add(u64::from(tv_nsec))
}

#[cfg(test)]
mod tests {
    use super::WaylandError;
    use super::capture_flush;
    use super::io;
    use super::protocol_ready_nanos;

    #[test]
    fn capture_flush_accepts_backpressure_but_rejects_broken_connections() {
        assert!(capture_flush(Ok(())).is_ok());
        assert!(capture_flush(Err(WaylandError::Io(io::ErrorKind::WouldBlock.into()))).is_ok());
        assert!(capture_flush(Err(WaylandError::Io(io::ErrorKind::BrokenPipe.into()))).is_err());
    }

    #[test]
    fn ready_timestamp_combines_nonzero_high_word() {
        assert_eq!(
            protocol_ready_nanos(1, 2, 345),
            Some(((1_u64 << 32) | 2) * 1_000_000_000 + 345)
        );
    }

    #[test]
    fn ready_timestamp_rejects_nsec_out_of_bounds_and_overflow() {
        assert_eq!(protocol_ready_nanos(0, 0, 1_000_000_000), None);
        assert_eq!(protocol_ready_nanos(u32::MAX, u32::MAX, 0), None);
    }
}

// Protocol objects whose events streamd does not use.
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_pointer::WlPointer);
delegate_noop!(State: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
delegate_noop!(State: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
delegate_noop!(State: ignore zwp_input_method_manager_v2::ZwpInputMethodManagerV2);
delegate_noop!(State: ignore zwlr_output_mode_v1::ZwlrOutputModeV1);
delegate_noop!(State: ignore zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1);
delegate_noop!(State: ignore ext_data_control_manager_v1::ExtDataControlManagerV1);
delegate_noop!(State: ignore ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);
delegate_noop!(State: ignore ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
