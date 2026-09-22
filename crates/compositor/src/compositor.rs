use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use calloop::EventLoop;
use calloop::Interest;
use calloop::Mode;
use calloop::PostAction;
use calloop::generic::Generic;
use calloop::signals::Signal;
use calloop::signals::Signals;
use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use smithay::backend::allocator::Fourcc;
use smithay::backend::input::Axis;
use smithay::backend::input::AxisSource;
use smithay::backend::input::ButtonState;
use smithay::backend::input::InputTime;
use smithay::backend::input::KeyState;
use smithay::backend::renderer::Bind;
use smithay::backend::renderer::ExportMem;
use smithay::backend::renderer::Offscreen;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::PopupManager;
use smithay::desktop::Space;
use smithay::desktop::Window;
use smithay::desktop::WindowSurfaceType;
use smithay::input::Seat;
use smithay::input::SeatState;
use smithay::input::keyboard::FilterResult;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::keyboard::XkbConfig;
use smithay::input::pointer::AxisFrame;
use smithay::input::pointer::ButtonEvent;
use smithay::input::pointer::MotionEvent;
use smithay::output::Output;
use smithay::output::PhysicalProperties;
use smithay::output::Scale;
use smithay::output::Subpixel;
use smithay::reexports::pixman::Image;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::ListeningSocket;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::Rectangle;
use smithay::utils::SERIAL_COUNTER;
use smithay::utils::Transform;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::text_input::TextInputSeat;
use smithay::wayland::viewporter::ViewporterState;
use waywire_protocol::Decoder;
use waywire_protocol::pipe::Command;
use waywire_protocol::pipe::Event;
use waywire_protocol::pipe::FrameDimension;
use waywire_protocol::pipe::FrameMetadata;
use waywire_protocol::pipe::FrameSize;
use waywire_protocol::pipe::Generation;
use waywire_protocol::pipe::InputSequence;
use waywire_protocol::pipe::Quality;
use waywire_protocol::pipe::{
    self,
};

use crate::Options;
use crate::event_writer::EventSink;
use crate::event_writer::EventWriter;
use crate::video::CapturedFrame;
use crate::video::EncoderConfig;
use crate::video::Notification;
use crate::video::VideoEncoder;
use crate::video::encoded_dimensions;

#[path = "clipboard.rs"]
pub(crate) mod clipboard;
#[path = "decorations.rs"]
mod decorations;
#[path = "focus.rs"]
pub(crate) mod focus;
#[path = "grabs.rs"]
mod grabs;
#[path = "shell.rs"]
mod shell;

#[expect(
    clippy::struct_field_names,
    reason = "Smithay protocol state names match their handler methods"
)]
pub(crate) struct State {
    pub(crate) display_handle: DisplayHandle,
    pub(crate) space: Space<Window>,
    pub(crate) seat: Seat<Self>,
    pub(crate) output: Output,
    pub(crate) compositor_state: CompositorState,
    pub(crate) shm_state: ShmState,
    pub(crate) xdg_shell_state: XdgShellState,
    pub(crate) seat_state: SeatState<Self>,
    pub(crate) data_device_state: DataDeviceState,
    pub(crate) popups: PopupManager,
    pub(crate) xwm: Option<smithay::xwayland::X11Wm>,
    pub(crate) xdisplay: Option<u32>,
    pub(crate) xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    pub(crate) eis_seats: HashMap<KeyboardSource, smithay::backend::libei::EiInputSeat>,
    clipboard: clipboard::Clipboard,
    windows: HashMap<Window, grabs::WindowState>,
    start: Instant,
    running: bool,
    failure: Option<anyhow::Error>,
    event_sink: EventSink,
    video: Option<VideoEncoder>,
    quality: Quality,
    size: FrameSize,
    generation: Generation,
    acknowledged: Option<Generation>,
    sequence: u64,
    latest_input_sequence: Option<InputSequence>,
    buttons: HashMap<KeyboardSource, HashSet<u32>>,
    pub(crate) dirty: bool,
}

impl State {
    fn new(
        display_handle: DisplayHandle,
        options: &Options,
        event_sink: EventSink,
        video: VideoEncoder,
    ) -> Result<Self> {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(&display_handle, [
            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Maximize,
            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Fullscreen,
        ]);
        smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let xwayland_shell_state =
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<Self>(&display_handle);
        OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        ViewporterState::new::<Self>(&display_handle);
        FractionalScaleManagerState::new::<Self>(&display_handle);
        CursorShapeManagerState::new::<Self>(&display_handle);
        TextInputManagerState::new::<Self>(&display_handle);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "waywire");
        seat.add_keyboard(
            XkbConfig {
                layout: &options.xkb_layout,
                ..Default::default()
            },
            200,
            25,
        )?;
        seat.add_pointer();
        seat.text_input().set_compositor_input_method(true);
        let output = Output::new(
            "waywire".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Waywire".into(),
                model: "Headless".into(),
                serial_number: "1".into(),
            },
        );
        output.create_global::<Self>(&display_handle);
        let mode = smithay::output::Mode {
            size: (
                i32::try_from(options.resolution.width())?,
                i32::try_from(options.resolution.height())?,
            )
                .into(),
            refresh: i32::try_from(options.frame_rate.get())? * 1000,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        let mut space = Space::default();
        space.map_output(&output, (0, 0));
        Ok(Self {
            display_handle,
            space,
            seat,
            output,
            compositor_state,
            shm_state,
            xdg_shell_state,
            seat_state,
            data_device_state,
            popups: PopupManager::default(),
            xwm: None,
            xdisplay: None,
            xwayland_shell_state,
            eis_seats: HashMap::new(),
            clipboard: clipboard::Clipboard::default(),
            windows: HashMap::new(),
            start: Instant::now(),
            running: true,
            failure: None,
            event_sink,
            video: Some(video),
            quality: Quality {
                fps: options.frame_rate,
                bitrate_kbps: options.bitrate,
                scale_percent: pipe::ScalePercent::new(100)?,
                crf: pipe::Crf::new(23)?,
                chroma: pipe::Chroma::Yuv444,
            },
            size: options.resolution,
            generation: Generation::new(1)?,
            acknowledged: None,
            sequence: 0,
            latest_input_sequence: None,
            buttons: HashMap::new(),
            dirty: true,
        })
    }

    pub(crate) fn fail(&mut self, error: anyhow::Error) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        self.running = false;
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "events are ephemeral messages consumed at emission"
    )]
    fn emit(&mut self, event: Event) {
        if let Err(error) = self.event_sink.send(&event) {
            self.fail(error.into());
        }
    }

    fn advance_generation(&mut self) -> Result<()> {
        self.generation = Generation::new(
            self.generation
                .get()
                .checked_add(1)
                .context("media generation exhausted")?,
        )?;
        self.acknowledged = None;
        self.dirty = true;
        if let Some(video) = &self.video {
            video.set_generation(self.generation)?;
        }
        Ok(())
    }

    pub(crate) fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.decoration_under(pos).is_some() {
            return None;
        }
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, offset)| (surface, (offset + location).to_f64()))
            })
    }

    pub(crate) fn keyboard_key(&mut self, source: KeyboardSource, key: u32, state: KeyState) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.input_from_source::<(), _>(
                source,
                self,
                (key + 8).into(),
                state,
                SERIAL_COUNTER.next_serial(),
                InputTime::now(),
                |_, _, _| FilterResult::Forward,
            );
        }
    }

    pub(crate) fn pointer_motion(&mut self, location: Point<f64, Logical>) {
        let scale = self.output.current_scale().fractional_scale();
        let location = Point::from((
            location
                .x
                .clamp(0.0, (f64::from(self.size.width()) - 1.0) / scale),
            location
                .y
                .clamp(0.0, (f64::from(self.size.height()) - 1.0) / scale),
        ));
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.motion(
                self,
                self.surface_under(location),
                &MotionEvent {
                    location,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: InputTime::now(),
                },
            );
            pointer.frame(self);
        }
        self.emit(Event::CursorPosition(pipe::CursorPosition {
            x: normalized_coordinate(location.x * scale, self.size.width()),
            y: normalized_coordinate(location.y * scale, self.size.height()),
        }));
    }

    pub(crate) fn pointer_button(
        &mut self,
        source: KeyboardSource,
        button: u32,
        state: ButtonState,
    ) {
        let before = self.buttons.values().any(|keys| keys.contains(&button));
        let keys = self.buttons.entry(source).or_default();
        match state {
            ButtonState::Pressed => {
                keys.insert(button);
            }
            ButtonState::Released => {
                keys.remove(&button);
            }
        }
        let after = self.buttons.values().any(|keys| keys.contains(&button));
        if before == after {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        if state == ButtonState::Pressed
            && button == 0x110
            && !pointer.is_grabbed()
            && let Some((window, action)) = self.decoration_under(pointer.current_location())
        {
            self.focus_window(Some(window.clone()));
            pointer.button(
                self,
                &ButtonEvent {
                    button,
                    state,
                    serial,
                    time: InputTime::now(),
                },
            );
            match action {
                decorations::DecorationAction::Move => {
                    self.start_decoration_grab(window, serial, false);
                }
                decorations::DecorationAction::Resize => {
                    self.start_decoration_grab(window, serial, true);
                }
                decorations::DecorationAction::Close => {
                    if let Some(top) = window.toplevel() {
                        top.send_close();
                    }
                    if let Some(x11) = window.x11_surface()
                        && let Err(error) = x11.close()
                    {
                        tracing::warn!(%error, "close X11 window");
                    }
                }
            }
            pointer.frame(self);
            return;
        }
        if state == ButtonState::Pressed && !pointer.is_grabbed() {
            let window = self
                .space
                .element_under(pointer.current_location())
                .map(|(window, _)| window.clone());
            self.focus_window(window);
        }
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial,
                time: InputTime::now(),
            },
        );
        pointer.frame(self);
    }

    pub(crate) fn release_input(&mut self, source: KeyboardSource) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.release_source(self, source);
        }
        let buttons = self.buttons.get(&source).cloned().unwrap_or_default();
        for button in buttons {
            self.pointer_button(source, button, ButtonState::Released);
        }
        self.buttons.remove(&source);
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "owned target avoids borrowing the mutable Space across focus changes"
    )]
    pub(crate) fn focus_window(&mut self, window: Option<Window>) {
        if let Some(window) = &window {
            self.space.raise_element(window, true);
            if let (Some(xwm), Some(surface)) = (&mut self.xwm, window.x11_surface())
                && let Err(error) = xwm.raise_window(surface)
            {
                tracing::warn!(%error, "raise X11 window");
            }
        }
        for current in self.space.elements() {
            current.set_activated(window.as_ref() == Some(current));
            if let Some(top) = current.toplevel() {
                top.send_pending_configure();
            }
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                window.as_ref().and_then(focus::KeyboardFocus::window),
                SERIAL_COUNTER.next_serial(),
            );
        }
        self.dirty = true;
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one direct match owns the pipe command vocabulary"
    )]
    fn apply_command(&mut self, command: Command) -> Result<()> {
        if let Some(sequence) = command.input_sequence() {
            self.latest_input_sequence = Some(sequence);
        }
        let source = KeyboardSource::MAIN;
        match command {
            Command::PointerAbsolute(p) => self.pointer_motion(
                (
                    f64::from(p.x.get()) / 65535.0 * f64::from(self.size.width())
                        / self.output.current_scale().fractional_scale(),
                    f64::from(p.y.get()) / 65535.0 * f64::from(self.size.height())
                        / self.output.current_scale().fractional_scale(),
                )
                    .into(),
            ),
            Command::PointerRelative(p) => {
                if let Some(pointer) = self.seat.get_pointer() {
                    let scale = self.output.current_scale().fractional_scale();
                    self.pointer_motion(
                        pointer.current_location()
                            + Point::from((
                                f64::from(p.dx.get()) / scale,
                                f64::from(p.dy.get()) / scale,
                            )),
                    );
                }
            }
            Command::PointerButton(p) => self.pointer_button(
                source,
                p.button.evdev_code(),
                match p.state {
                    pipe::ButtonState::Pressed => ButtonState::Pressed,
                    pipe::ButtonState::Released => ButtonState::Released,
                },
            ),
            Command::KeyboardKey(p) => match p.state {
                pipe::KeyState::Pressed => {
                    self.keyboard_key(source, p.key.get(), KeyState::Pressed);
                }
                pipe::KeyState::Released => {
                    self.keyboard_key(source, p.key.get(), KeyState::Released);
                }
                pipe::KeyState::Repeated => {}
            },
            Command::ReleaseAll(_) => self.release_input(source),
            Command::PointerScroll(p) => {
                if let Some(pointer) = self.seat.get_pointer() {
                    let frame = AxisFrame::new(InputTime::now())
                        .source(AxisSource::Continuous)
                        .value(Axis::Horizontal, f64::from(p.dx.get()))
                        .value(Axis::Vertical, f64::from(p.dy.get()));
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
            }
            Command::Resize(p) => {
                self.size = p.size;
                let mode = smithay::output::Mode {
                    size: (
                        i32::try_from(p.size.width())?,
                        i32::try_from(p.size.height())?,
                    )
                        .into(),
                    refresh: i32::try_from(self.quality.fps.get())? * 1000,
                };
                self.output.change_current_state(
                    Some(mode),
                    None,
                    Some(Scale::Fractional(f64::from(p.scale_v120.get()) / 120.0)),
                    None,
                );
                self.output.set_preferred(mode);
                self.space.map_output(&self.output, (0, 0));
                self.output_resized();
                crate::eis::refresh_regions(self);
                self.advance_generation()?;
                self.emit(Event::ResizeApplied(pipe::ResizeApplied {
                    request_id: p.request_id,
                    size: p.size,
                    scale_v120: p.scale_v120,
                    generation: self.generation,
                }));
            }
            Command::Quality(q) => {
                if q != self.quality {
                    self.quality = q;
                    self.advance_generation()?;
                }
            }
            Command::ResetVideo(_) => self.advance_generation()?,
            Command::KeyframeReadiness(p) => {
                if p.generation == self.generation {
                    self.acknowledged = match p.state {
                        pipe::KeyframeState::Cached => Some(p.generation),
                        pipe::KeyframeState::Missing => None,
                    };
                }
            }
            Command::Text(p) => {
                let cursor = i32::try_from(p.text.as_str().len())?;
                let text_input = self.seat.text_input();
                text_input.with_active_text_input(|text, _| match p.action {
                    pipe::TextAction::Commit => {
                        text.preedit_string(None, 0, 0);
                        text.commit_string(Some(p.text.as_str().into()));
                    }
                    pipe::TextAction::Preedit => {
                        text.preedit_string(Some(p.text.as_str().into()), cursor, cursor);
                    }
                });
                text_input.done(false);
            }
            Command::Clipboard(text) => self.clipboard_set(&text),
        }
        Ok(())
    }

    fn drain_video(&mut self) {
        while let Some(notification) = self.video.as_ref().and_then(VideoEncoder::try_notification)
        {
            match notification {
                Notification::Submitted(metadata) => self.emit(Event::Frame(metadata)),
                Notification::RestartRequired { generation } if generation == self.generation => {
                    if let Err(error) = self.advance_generation() {
                        self.fail(error);
                    }
                }
                Notification::RestartRequired { .. } => {}
                Notification::Fatal(error) => self.fail(anyhow!(error)),
            }
        }
    }

    fn render(
        &mut self,
        renderer: &mut PixmanRenderer,
        image: &mut Image<'static, 'static>,
        tracker: &mut OutputDamageTracker,
    ) -> Result<()> {
        let size = (
            i32::try_from(self.size.width())?,
            i32::try_from(self.size.height())?,
        )
            .into();
        if image.width() != self.size.width() as usize
            || image.height() != self.size.height() as usize
        {
            *image = renderer.create_buffer(Fourcc::Argb8888, size)?;
            *tracker = OutputDamageTracker::from_output(&self.output);
        }
        self.space.refresh();
        self.windows
            .retain(|window, _| smithay::utils::IsAlive::alive(window));
        self.popups.cleanup();
        if self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .is_some_and(|focus| !smithay::utils::IsAlive::alive(&focus))
        {
            let next = self
                .space
                .elements()
                .rev()
                .find(|window| self.windows.get(*window).is_some_and(|state| state.mapped))
                .cloned();
            self.focus_window(next);
        }
        let elements = self.scene_elements(renderer);
        let mut target = renderer.bind(image)?;
        let forced = self.dirty || self.acknowledged != Some(self.generation);
        let result = tracker.render_output(
            renderer,
            &mut target,
            usize::from(!forced),
            &elements,
            [0.08, 0.09, 0.12, 1.0],
        )?;
        if result.damage.is_some() || forced {
            let mapping =
                renderer.copy_framebuffer(&target, Rectangle::from_size(size), Fourcc::Argb8888)?;
            let pixels = renderer.map_texture(&mapping)?;
            let (width, height) = encoded_dimensions(
                self.size.width(),
                self.size.height(),
                self.quality.scale_percent,
                self.quality.fps,
            );
            let config = EncoderConfig {
                raw_width: FrameDimension::new(self.size.width().try_into()?)?,
                raw_height: FrameDimension::new(self.size.height().try_into()?)?,
                encoded_width: FrameDimension::new(width)?,
                encoded_height: FrameDimension::new(height)?,
                fps: self.quality.fps,
                bitrate_kbps: self.quality.bitrate_kbps,
                crf: self.quality.crf,
                chroma: self.quality.chroma,
            };
            let timestamp = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)?;
            let metadata = FrameMetadata {
                generation: self.generation,
                width: config.encoded_width,
                height: config.encoded_height,
                capture_nanos: u64::try_from(timestamp.tv_sec())? * 1_000_000_000
                    + u64::try_from(timestamp.tv_nsec())?,
                sequence: self.sequence,
                input_sequence: self.latest_input_sequence,
                fps: self.quality.fps,
                chroma: self.quality.chroma,
            };
            self.sequence = self
                .sequence
                .checked_add(1)
                .context("frame sequence exhausted")?;
            // ARGB8888 on little-endian hosts is packed BGRA; four-byte pixels have no row padding.
            if let Some(video) = &self.video {
                video.submit(CapturedFrame::new(pixels, metadata, config))?;
            }
        }
        self.dirty = false;
        for window in self.space.elements() {
            window.send_frame(
                &self.output,
                self.start.elapsed(),
                Some(Duration::ZERO),
                |_, _| Some(self.output.clone()),
            );
        }
        Ok(())
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the single calloop owner registers sources and performs ordered shutdown here"
)]
pub(crate) fn run(mut options: Options) -> Result<()> {
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT])?;
    let runtime = tempfile::Builder::new().prefix("waywire-").tempdir()?;
    let socket_path = runtime.path().join("wayland-0");
    let socket = ListeningSocket::bind_absolute(socket_path.clone())?;
    let mut event_loop = EventLoop::<State>::try_new()?;
    let handle = event_loop.handle();
    let display = Display::<State>::new()?;
    let (event_writer, sink) = EventWriter::start()?;
    let mut video = VideoEncoder::start(options.ffmpeg.clone(), options.rtp_port)?;
    let notifications = video
        .take_notification_source()
        .context("encoder notification source missing")?;
    let mut state = State::new(display.handle(), &options, sink, video)?;
    let eis_path = runtime.path().join("eis");
    crate::eis::listen(&handle, &eis_path, options.xkb_layout.clone())?;
    crate::xwayland::start(&handle, &mut state, &eis_path)?;
    handle.insert_source(
        Generic::new(socket, Interest::READ, Mode::Level),
        |_, socket, state| {
            while let Some(stream) = socket.accept()? {
                state
                    .display_handle
                    .insert_client(stream, Arc::new(shell::ClientState::default()))?;
            }
            Ok(PostAction::Continue)
        },
    )?;
    handle.insert_source(
        Generic::new(display, Interest::READ, Mode::Level),
        |_, display, state| {
            // SAFETY: the registered display remains in place for the whole callback.
            unsafe {
                display.get_mut().dispatch_clients(state)?;
            }
            Ok(PostAction::Continue)
        },
    )?;
    handle.insert_source(signals, |_, (), state| state.running = false)?;
    handle
        .insert_source(notifications, |(), (), state| state.drain_video())
        .map_err(|error| anyhow!("register video source: {error}"))?;
    let stdin = File::open("/dev/stdin")?;
    let flags = OFlag::from_bits_truncate(fcntl(&stdin, FcntlArg::F_GETFL)?);
    fcntl(&stdin, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    let mut decoder = Some(Decoder::<Command>::new());
    handle.insert_source(
        Generic::new(stdin, Interest::READ, Mode::Level),
        move |_, input, state| {
            let mut bytes = vec![0; 65536];
            // SAFETY: reading does not move or drop the registered File.
            match unsafe { input.get_mut() }.read(&mut bytes) {
                Ok(0) => {
                    if let Some(decoder) = decoder.take()
                        && let Err(error) = decoder.finish()
                    {
                        state.fail(error.into());
                    }
                    state.running = false;
                    return Ok(PostAction::Remove);
                }
                Ok(count) => {
                    if let Some(decoder) = decoder.as_mut() {
                        match decoder.push(&bytes[..count]) {
                            Ok(commands) => {
                                for command in commands {
                                    if let Err(error) = state.apply_command(command) {
                                        state.fail(error);
                                        break;
                                    }
                                }
                            }
                            Err(error) => state.fail(error.into()),
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => state.fail(error.into()),
            }
            Ok(PostAction::Continue)
        },
    )?;
    if options.session.is_empty() {
        options
            .session
            .extend(std::env::var_os("WAYWIRE_SESSION").filter(|v| !v.is_empty()));
    }
    let mut child = Session(None);
    let mut renderer = PixmanRenderer::new()?;
    let mut image = renderer.create_buffer(
        Fourcc::Argb8888,
        (
            i32::try_from(state.size.width())?,
            i32::try_from(state.size.height())?,
        )
            .into(),
    )?;
    let mut tracker = OutputDamageTracker::from_output(&state.output);
    let mut next_frame = Instant::now();
    tracing::info!(socket = %socket_path.display(), "Wayland compositor ready");
    while state.running {
        if child.0.is_none()
            && let Some(display) = state.xdisplay
            && let Some(program) = options.session.first()
        {
            let mut command = std::process::Command::new(program);
            command
                .args(&options.session[1..])
                .env("WAYLAND_DISPLAY", &socket_path)
                .env("XDG_RUNTIME_DIR", runtime.path())
                .env("DISPLAY", format!(":{display}"))
                .env("LIBEI_SOCKET", &eis_path)
                .env_remove("WAYLAND_SOCKET")
                .stdin(Stdio::null())
                .stdout(Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?))
                .stderr(Stdio::inherit());
            // SAFETY: this hook only invokes pthread_sigmask before exec, without allocation.
            unsafe {
                command.pre_exec(|| {
                    nix::sys::signal::pthread_sigmask(
                        nix::sys::signal::SigmaskHow::SIG_SETMASK,
                        Some(&nix::sys::signal::SigSet::empty()),
                        None,
                    )
                    .map_err(std::io::Error::from)
                });
            }
            match command.spawn() {
                Ok(process) => child.0 = Some(process),
                Err(error) => state.fail(error.into()),
            }
        }
        state.clipboard_tick();
        if let Err(error) = event_loop.dispatch(
            next_frame.saturating_duration_since(Instant::now()),
            &mut state,
        ) {
            state.fail(error.into());
        }
        state.space.refresh();
        crate::xwayland::sync_stacking(&mut state);
        if Instant::now() >= next_frame {
            if let Err(error) = state.render(&mut renderer, &mut image, &mut tracker) {
                state.fail(error);
            }
            next_frame =
                Instant::now() + Duration::from_secs_f64(1.0 / f64::from(state.quality.fps.get()));
        }
        if let Err(error) = state.display_handle.flush_clients() {
            state.fail(error.into());
        }
        if let Some(error) = event_writer.failure() {
            state.fail(error.into());
        }
        if let Some(child) = child.0.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => state.running = false,
                Ok(None) => {}
                Err(error) => state.fail(error.into()),
            }
        }
    }
    drop(child);
    state.release_input(KeyboardSource::MAIN);
    if let Some(video) = state.video.take() {
        video.stop();
    }
    event_writer.stop();
    state.failure.map_or(Ok(()), Err)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::expect_used,
    reason = "rounded and clamped to the protocol's unsigned 16-bit coordinate range"
)]
fn normalized_coordinate(pixel: f64, extent: u32) -> pipe::PointerCoordinate {
    pipe::PointerCoordinate::new(
        (pixel / f64::from(extent) * 65535.0)
            .round()
            .clamp(0.0, 65535.0) as u32,
    )
    .expect("clamped cursor coordinate")
}

struct Session(Option<Child>);
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            // Descendants remain in the gateway-owned process group, including
            // when this compositor crashes. Child::kill avoids PID reuse after wait.
            // An already exited child is normal; cleanup errors cannot be returned from Drop.
            drop(child.kill());
            drop(child.wait());
        }
    }
}
