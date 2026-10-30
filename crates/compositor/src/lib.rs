//! Headless Wayland compositor streaming H.264 RTP.

mod capture;
mod clipboard;
mod decorations;
mod eis;
mod event_writer;
mod focus;
mod foreign_toplevel;
mod grabs;
mod pointer_warp;
mod portal;
mod screencopy;
mod session_bus;
mod shell;
mod toplevel_list;
mod video;
mod virtual_keyboard;
mod virtual_pointer;
mod xwayland;

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Stdio;
use std::rc::Rc;
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
use smithay::input::pointer::RelativeMotionEvent;
use smithay::output::Output;
use smithay::output::PhysicalProperties;
use smithay::output::Scale;
use smithay::output::Subpixel;
use smithay::reexports::pixman::Image;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::ListeningSocket;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Point;
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
use smithay::wayland::virtual_pointer::VirtualPointerDevice;
use waywire_protocol::Decoder;
use waywire_protocol::pipe::Command;
use waywire_protocol::pipe::Event;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameDimension;
use waywire_protocol::pipe::FrameMetadata;
use waywire_protocol::pipe::FrameSize;
use waywire_protocol::pipe::Generation;
use waywire_protocol::pipe::InputSequence;
use waywire_protocol::pipe::Kbps;
use waywire_protocol::pipe::Quality;
use waywire_protocol::pipe::{
    self,
};

use crate::event_writer::EventSink;
use crate::event_writer::EventWriter;
use crate::video::CapturedFrame;
use crate::video::EncoderConfig;
use crate::video::Notification;
use crate::video::VideoEncoder;
use crate::video::encoded_dimensions;

/// Startup settings for the compositor and its initial video stream.
#[derive(Debug)]
pub struct Config {
    pub ffmpeg: String,
    pub frame_rate: Fps,
    pub bitrate: Kbps,
    pub rtp_port: u16,
    pub xkb_layout: String,
    pub resolution: FrameSize,
    /// Application argv. When empty, `WAYWIRE_SESSION` names one executable.
    pub session: Vec<OsString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameRequest {
    Idle,
    Pending,
}

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
    /// Last X11 stacking order sent to the XWM; `None` forces the next sync,
    /// which also flushes requests such as `new_selection` that do not flush.
    pub(crate) x11_stacking: Option<Vec<u32>>,
    pub(crate) xdisplay: Option<u32>,
    pub(crate) xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    pub(crate) eis_seats: HashMap<KeyboardSource, smithay::backend::libei::EiInputSeat>,
    pub(crate) virtual_pointers: HashMap<VirtualPointerDevice, KeyboardSource>,
    pub(crate) virtual_keyboards: virtual_keyboard::VirtualKeyboards,
    capture: capture::Capture,
    screencopy: screencopy::Screencopy,
    pub(crate) toplevel_list: toplevel_list::ToplevelList,
    clipboard: clipboard::Clipboard,
    windows: HashMap<Window, grabs::WindowState>,
    decoration_press: Option<(KeyboardSource, Window)>,
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
    /// Every connected agent source: EIS devices, virtual pointers and
    /// virtual keyboards. `KeyboardSource::MAIN` is the browser and never
    /// belongs here.
    agent_sources: HashSet<KeyboardSource>,
    /// Sources already logged as dropped during the current browser lease.
    dropped_agent_sources: HashSet<KeyboardSource>,
    agent_input: pipe::AgentInput,
    browser_holds_input: bool,
    pub(crate) dirty: bool,
    pub(crate) frame_pending: FrameRequest,
}

/// Advertise the globals whose server state `State` never needs to reach.
fn install_globals(display_handle: &DisplayHandle) {
    smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<State>(display_handle);
    OutputManagerState::new_with_xdg_output::<State>(display_handle);
    ViewporterState::new::<State>(display_handle);
    FractionalScaleManagerState::new::<State>(display_handle);
    CursorShapeManagerState::new::<State>(display_handle);
    TextInputManagerState::new::<State>(display_handle);
    pointer_warp::install(display_handle);
    virtual_pointer::install(display_handle);
    virtual_keyboard::init(display_handle);
}

impl State {
    #[expect(clippy::too_many_lines, reason = "one initializer per State field")]
    fn new(
        display_handle: DisplayHandle,
        options: &Config,
        event_sink: EventSink,
        video: VideoEncoder,
    ) -> Result<Self> {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(&display_handle, [
            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Maximize,
            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Fullscreen,
        ]);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let xwayland_shell_state =
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<Self>(&display_handle);
        install_globals(&display_handle);
        let toplevel_list = toplevel_list::ToplevelList::new(&display_handle);
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
        let capture = capture::Capture::new(&display_handle);
        let screencopy = screencopy::Screencopy::new(&display_handle);
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
            x11_stacking: None,
            xdisplay: None,
            xwayland_shell_state,
            eis_seats: HashMap::new(),
            virtual_pointers: HashMap::new(),
            virtual_keyboards: virtual_keyboard::VirtualKeyboards::default(),
            capture,
            screencopy,
            toplevel_list,
            clipboard: clipboard::Clipboard::default(),
            windows: HashMap::new(),
            decoration_press: None,
            start: Instant::now(),
            running: true,
            failure: None,
            event_sink,
            video: Some(video),
            quality: Quality {
                fps: options.frame_rate,
                bitrate_kbps: options.bitrate,
                scale_percent: pipe::ScalePercent::new(100)?,
                crf: pipe::Crf::new(27)?,
                chroma: pipe::Chroma::Yuv420,
            },
            size: options.resolution,
            generation: Generation::new(1)?,
            acknowledged: None,
            sequence: 0,
            latest_input_sequence: None,
            buttons: HashMap::new(),
            agent_sources: HashSet::new(),
            dropped_agent_sources: HashSet::new(),
            agent_input: pipe::AgentInput::default(),
            browser_holds_input: false,
            dirty: true,
            frame_pending: FrameRequest::Pending,
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

    /// Gate every seat input entry point. Browser input always passes. Agent
    /// sources are registered on first use so a takeover can release them;
    /// while a browser holds the lease their events are dropped, with one
    /// debug line per source per lease rather than one per event.
    fn accepts_input(&mut self, source: KeyboardSource) -> bool {
        if source == KeyboardSource::MAIN {
            return true;
        }
        self.register_agent_source(source);
        if !self.browser_holds_input {
            return true;
        }
        if self.dropped_agent_sources.insert(source) {
            tracing::debug!(
                ?source,
                "dropped agent input while a browser holds input control"
            );
        }
        false
    }

    pub(crate) fn register_agent_source(&mut self, source: KeyboardSource) {
        if source == KeyboardSource::MAIN {
            return;
        }
        if self.agent_sources.insert(source) {
            self.publish_agent_input();
        }
    }

    /// An adapter's device is gone. Release its held input and stop counting
    /// it as connected.
    pub(crate) fn disconnect_input_source(&mut self, source: KeyboardSource) {
        self.release_input(source);
        self.dropped_agent_sources.remove(&source);
        if self.agent_sources.remove(&source) {
            self.publish_agent_input();
        }
    }

    fn publish_agent_input(&mut self) {
        let connected = !self.agent_sources.is_empty();
        let state = pipe::AgentInput {
            connected,
            dropped: self.browser_holds_input && (connected || self.agent_input.dropped),
        };
        if state != self.agent_input {
            self.agent_input = state;
            self.emit(Event::AgentInput(state));
        }
    }

    /// Apply a browser takeover or handback. On takeover, every agent source
    /// releases what it holds before its future input is dropped. On handback,
    /// the browser's own keys are released and agent input resumes.
    pub(crate) fn set_browser_lease(&mut self, held: bool) {
        if self.browser_holds_input == held {
            return;
        }
        self.browser_holds_input = held;
        if held {
            let sources: Vec<_> = self.agent_sources.iter().copied().collect();
            for source in sources {
                self.release_input(source);
            }
            self.release_input(KeyboardSource::MAIN);
            self.suspend_virtual_keymap();
        } else {
            self.release_input(KeyboardSource::MAIN);
            self.resume_virtual_keymap();
        }
        self.dropped_agent_sources.clear();
        self.toplevel_list.lease_changed();
        self.publish_agent_input();
    }

    pub(crate) fn keyboard_key(&mut self, source: KeyboardSource, key: u32, state: KeyState) {
        if !self.accepts_input(source) {
            return;
        }
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

    /// Apply a virtual keyboard's modifier state through the same gate as its
    /// keys, so a browser lease drops it too.
    pub(crate) fn keyboard_modifiers(
        &mut self,
        source: KeyboardSource,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) {
        if !self.accepts_input(source) {
            return;
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.with_xkb_state(self, |mut context| {
                context.set_modifier_mask(depressed, latched, locked, group);
            });
        }
    }

    pub(crate) fn pointer_motion(&mut self, source: KeyboardSource, location: Point<f64, Logical>) {
        self.pointer_motion_with_delta(source, location, None);
    }

    /// Relative input also reaches clients as relative motion, unclamped, so
    /// locked-pointer clients keep receiving movement at the output edge. It
    /// shares the absolute motion's frame: Xwayland's warp emulator then
    /// applies only the relative delta instead of moving twice.
    pub(crate) fn pointer_motion_with_delta(
        &mut self,
        source: KeyboardSource,
        location: Point<f64, Logical>,
        delta: Option<Point<f64, Logical>>,
    ) {
        if !self.accepts_input(source) {
            return;
        }
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
            if let Some(delta) = delta {
                pointer.relative_motion(
                    self,
                    self.surface_under(location),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: delta,
                        time: InputTime::now(),
                    },
                );
            }
            pointer.frame(self);
        }
        if self.decoration_press.as_ref().is_some_and(|(_, window)| {
            self.decoration_under(location)
                != Some((window.clone(), decorations::DecorationAction::Close))
        }) {
            self.decoration_press = None;
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
        if !self.accepts_input(source) {
            return;
        }
        self.pointer_button_inner(source, button, state);
    }

    pub(crate) fn pointer_axis(&mut self, source: KeyboardSource, frame: AxisFrame) {
        if !self.accepts_input(source) {
            return;
        }
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.axis(self, frame);
            pointer.frame(self);
        }
    }

    fn pointer_button_inner(&mut self, source: KeyboardSource, button: u32, state: ButtonState) {
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
            // Another input source can keep the aggregate button held. It must
            // not inherit an armed Close after its original owner releases it.
            if button == 0x110
                && state == ButtonState::Released
                && self
                    .decoration_press
                    .as_ref()
                    .is_some_and(|(owner, _)| *owner == source)
            {
                self.decoration_press = None;
            }
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
                    self.decoration_press = Some((source, window));
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
        if state == ButtonState::Released
            && button == 0x110
            && let Some((owner, window)) = self.decoration_press.take()
            && owner == source
            && self.decoration_under(pointer.current_location())
                == Some((window.clone(), decorations::DecorationAction::Close))
        {
            Self::close_window(&window);
        }
    }

    /// Ask a window to close. The titlebar's Close button and the
    /// wlr-foreign-toplevel-management close request both use this path.
    pub(crate) fn close_window(window: &Window) {
        if let Some(top) = window.toplevel() {
            top.send_close();
        }
        if let Some(x11) = window.x11_surface()
            && let Err(error) = x11.close()
        {
            tracing::warn!(%error, "close X11 window");
        }
    }

    /// Activate a window from a foreign-toplevel request when the request's
    /// seat belongs to this compositor.
    pub(crate) fn activate_foreign_toplevel(&mut self, window: &Window, seat: &WlSeat) {
        if Seat::<Self>::from_resource(seat).as_ref() != Some(&self.seat) {
            return;
        }
        self.focus_window(Some(window.clone()));
    }

    /// Apply a maximize or fullscreen request to the window's native surface.
    /// Both paths already exist for client requests; this only selects the
    /// Wayland or X11 one.
    pub(crate) fn set_window_expanded(
        &mut self,
        window: &Window,
        mode: xdg_toplevel::State,
        enabled: bool,
    ) {
        if let Some(top) = window.toplevel() {
            self.set_expanded(top, mode, enabled);
        } else if let Some(surface) = window.x11_surface() {
            self.x11_zoom(surface, mode == xdg_toplevel::State::Fullscreen, enabled);
        }
    }

    /// Recompute every foreign-toplevel's state from the seat's current focus.
    pub(crate) fn refresh_toplevel_states(&mut self) {
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        self.toplevel_list.refresh_states(focused.as_ref());
    }

    pub(crate) fn release_input(&mut self, source: KeyboardSource) {
        // Disconnect/ownership loss must release buttons without activating Close.
        if self
            .decoration_press
            .as_ref()
            .is_some_and(|(owner, _)| *owner == source)
        {
            self.decoration_press = None;
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.release_source(self, source);
        }
        let buttons = self.buttons.get(&source).cloned().unwrap_or_default();
        for button in buttons {
            self.pointer_button_inner(source, button, ButtonState::Released);
        }
        self.buttons.remove(&source);
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "owned target avoids borrowing the mutable Space across focus changes"
    )]
    pub(crate) fn focus_window(&mut self, window: Option<Window>) {
        if self
            .decoration_press
            .as_ref()
            .is_some_and(|(_, target)| Some(target) != window.as_ref())
        {
            self.decoration_press = None;
        }
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
        self.refresh_toplevel_states();
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
                source,
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
                    let delta =
                        Point::from((f64::from(p.dx.get()) / scale, f64::from(p.dy.get()) / scale));
                    self.pointer_motion_with_delta(
                        source,
                        pointer.current_location() + delta,
                        Some(delta),
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
            Command::InputLease(p) => self.set_browser_lease(p.held),
            Command::PointerScroll(p) => {
                let frame = AxisFrame::new(InputTime::now())
                    .source(AxisSource::Continuous)
                    .value(Axis::Horizontal, f64::from(p.dx.get()))
                    .value(Axis::Vertical, f64::from(p.dy.get()));
                self.pointer_axis(source, frame);
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
                self.capture.output_resized();
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
                        pipe::KeyframeState::Missing => {
                            self.frame_pending = FrameRequest::Pending;
                            None
                        }
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
        self.toplevel_list.cleanup();
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
        let forced = self.dirty || self.acknowledged != Some(self.generation);
        let damaged = {
            let mut target = renderer.bind(image)?;
            tracker
                .render_output(
                    renderer,
                    &mut target,
                    usize::from(!forced),
                    &elements,
                    [0.08, 0.09, 0.12, 1.0],
                )?
                .damage
                .is_some()
        };
        if damaged || forced {
            // Pixman finishes synchronously. Borrow our target instead of allocating
            // and blitting a second full image through ExportMem::copy_framebuffer.
            // SAFETY: create_buffer owns and initializes the complete image storage;
            // the renderer's exclusive target borrow has ended.
            let data = unsafe { image.data() };
            // SAFETY: stride * height covers this live image's allocation. Nothing
            // mutates it before submit finishes copying into worker-owned storage.
            let pixels = unsafe {
                std::slice::from_raw_parts(data.cast::<u8>(), image.stride() * image.height())
            };
            debug_assert_eq!(image.stride(), image.width() * 4);
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
        capture::serve_pending(self, image);
        screencopy::serve_pending(self, image);
        self.dirty = false;
        self.frame_pending = FrameRequest::Idle;
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

/// Run the compositor until shutdown, reading pipe commands from stdin and writing events to stdout.
#[expect(
    clippy::too_many_lines,
    reason = "the single calloop owner registers sources and performs ordered shutdown here"
)]
pub fn run(mut options: Config) -> Result<()> {
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT])?;
    let runtime = tempfile::Builder::new().prefix("waywire-").tempdir()?;
    // Portals need a session bus, but it must be ours: an inherited bus may
    // belong to the desktop that launched us. Without a daemon, portals are off.
    let mut bus = session_bus::SessionBus::start(runtime.path());
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
    let mut bytes = vec![0; 65536];
    handle.insert_source(
        Generic::new(stdin, Interest::READ, Mode::Level),
        move |_, input, state| {
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
    // The portal backends read the most recently rendered frame from the
    // calloop loop, which is the only writer.
    let image = Rc::new(RefCell::new(
        renderer.create_buffer(
            Fourcc::Argb8888,
            (
                i32::try_from(state.size.width())?,
                i32::try_from(state.size.height())?,
            )
                .into(),
        )?,
    ));
    let portal_dirs = match &bus {
        Some(bus) => crate::portal::start(
            &handle,
            runtime.path(),
            bus.address(),
            Rc::clone(&image),
            options.xkb_layout.clone(),
        )?,
        None => None,
    };
    let session_environment = match (&portal_dirs, &bus) {
        (Some(dirs), Some(bus)) => Some(crate::portal::portal_environment(dirs, bus.address())),
        _ => None,
    };
    let mut tracker = OutputDamageTracker::from_output(&state.output);
    let mut next_frame = Instant::now();
    let mut next_housekeeping = Instant::now() + Duration::from_secs(2);
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
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .env_remove("WAYLAND_SOCKET");
            // The session program and its portal clients discover the desktop
            // and its private bus the same way the bus-activated frontend does.
            if let Some(environment) = &session_environment {
                for (key, value) in environment {
                    command.env(key, value);
                }
            }
            command
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
        let now = Instant::now();
        let mut deadline = next_housekeeping;
        if state.dirty || state.frame_pending == FrameRequest::Pending {
            deadline = deadline.min(next_frame);
        }
        if state.clipboard_active() {
            deadline = deadline.min(now + Duration::from_millis(20));
        }
        if let Err(error) = event_loop.dispatch(deadline.saturating_duration_since(now), &mut state)
        {
            state.fail(error.into());
        }
        state.space.refresh();
        crate::xwayland::sync_stacking(&mut state);
        if (state.dirty || state.frame_pending == FrameRequest::Pending)
            && Instant::now() >= next_frame
        {
            let started = Instant::now();
            if let Err(error) = state.render(&mut renderer, &mut image.borrow_mut(), &mut tracker) {
                state.fail(error);
            }
            next_frame = frame_deadline(
                next_frame.max(started),
                Instant::now(),
                Duration::from_secs_f64(1.0 / f64::from(state.quality.fps.get())),
            );
        }
        if Instant::now() >= next_housekeeping {
            next_housekeeping = Instant::now() + Duration::from_secs(2);
            if state.acknowledged != Some(state.generation) {
                state.frame_pending = FrameRequest::Pending;
            }
        }
        if let Err(error) = state.display_handle.flush_clients() {
            state.fail(error.into());
        }
        if let Some(error) = event_writer.failure() {
            state.fail(error.into());
        }
        if let Some(bus) = bus.as_mut() {
            bus.poll();
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
    drop(bus);
    state.release_input(KeyboardSource::MAIN);
    if let Some(video) = state.video.take() {
        video.stop();
    }
    event_writer.stop();
    state.failure.map_or(Ok(()), Err)
}

fn frame_deadline(previous: Instant, finished: Instant, interval: Duration) -> Instant {
    // Render time is part of the period, not added to it. On overrun, discard
    // the missed deadlines rather than accumulating a burst of catch-up frames.
    (previous + interval).max(finished)
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

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn test_state() -> (Display<State>, State) {
        let display = Display::new().expect("test display");
        let options = Config {
            ffmpeg: String::new(),
            frame_rate: Fps::new(60).expect("test frame rate"),
            bitrate: Kbps::new(16000).expect("test bitrate"),
            rtp_port: 0,
            xkb_layout: "us".into(),
            resolution: FrameSize::new(1920, 1080).expect("test resolution"),
            session: Vec::new(),
        };
        let video = VideoEncoder::idle_for_test().expect("idle encoder");
        let mut state = State::new(display.handle(), &options, EventSink::for_test(), video)
            .expect("test compositor");
        state.video.take().expect("new encoder").stop();
        (display, state)
    }

    fn pressed(state: &State, key: u32) -> bool {
        state
            .seat
            .get_keyboard()
            .expect("seat keyboard")
            .pressed_keys()
            .contains(&(key + 8).into())
    }

    fn lease(state: &mut State, held: bool) {
        state
            .apply_command(Command::InputLease(waywire_protocol::pipe::InputLease {
                held,
            }))
            .expect("lease command");
    }

    #[test]
    fn agent_input_reaches_the_seat_without_a_browser_lease() {
        let (_display, mut state) = test_state();
        let agent = KeyboardSource::new_auxiliary();

        state.keyboard_key(agent, 30, KeyState::Pressed);
        state.pointer_button(agent, 0x110, ButtonState::Pressed);

        assert!(pressed(&state, 30), "the agent's key must reach the seat");
        assert!(
            state
                .buttons
                .get(&agent)
                .is_some_and(|buttons| buttons.contains(&0x110)),
            "the agent's button must reach the seat"
        );
        assert_eq!(
            state.agent_input,
            pipe::AgentInput {
                connected: true,
                dropped: false,
            }
        );
    }

    #[test]
    fn disconnecting_an_agent_releases_its_button() {
        let (_display, mut state) = test_state();
        let source = KeyboardSource::new_auxiliary();
        state.register_agent_source(source);
        state.pointer_button(source, 0x110, ButtonState::Pressed);
        assert!(
            state
                .buttons
                .get(&source)
                .is_some_and(|buttons| buttons.contains(&0x110))
        );
        state.disconnect_input_source(source);
        assert!(state.buttons.get(&source).is_none_or(HashSet::is_empty));
    }

    #[test]
    fn transient_agent_remains_paused_until_handback() {
        let (_display, mut state) = test_state();
        let source = KeyboardSource::new_auxiliary();
        lease(&mut state, true);
        state.keyboard_key(source, 30, KeyState::Pressed);
        state.disconnect_input_source(source);
        assert!(
            state
                .event_sink
                .take_events()
                .contains(&Event::AgentInput(pipe::AgentInput {
                    connected: false,
                    dropped: true
                }))
        );

        lease(&mut state, false);
        assert!(
            state
                .event_sink
                .take_events()
                .contains(&Event::AgentInput(pipe::AgentInput {
                    connected: false,
                    dropped: false
                }))
        );
        lease(&mut state, true);
        assert!(state.event_sink.take_events().is_empty());
    }

    #[test]
    fn a_browser_takeover_releases_held_agent_input_and_drops_later_events() {
        let (_display, mut state) = test_state();
        let agent = KeyboardSource::new_auxiliary();
        state.keyboard_key(agent, 30, KeyState::Pressed);
        state.keyboard_key(agent, 42, KeyState::Pressed);
        state.pointer_button(agent, 0x110, ButtonState::Pressed);
        state.pointer_button(agent, 0x113, ButtonState::Pressed);
        assert!(pressed(&state, 30) && pressed(&state, 42));

        lease(&mut state, true);

        assert!(
            state
                .seat
                .get_keyboard()
                .expect("seat keyboard")
                .pressed_keys()
                .is_empty(),
            "a key held across the takeover must be released"
        );
        assert!(
            state.buttons.get(&agent).is_none_or(HashSet::is_empty),
            "a button held across the takeover must be released"
        );

        // Later agent events are dropped rather than queued or forwarded.
        state.event_sink.take_events();
        state.keyboard_key(agent, 31, KeyState::Pressed);
        state.pointer_button(agent, 0x111, ButtonState::Pressed);
        state.pointer_motion(agent, (12.0, 34.0).into());
        assert!(!pressed(&state, 31));
        assert!(state.buttons.get(&agent).is_none_or(HashSet::is_empty));
        assert!(
            state.event_sink.take_events().is_empty(),
            "dropped agent motion must not move the cursor or emit an event"
        );
        assert_eq!(
            state.agent_input,
            pipe::AgentInput {
                connected: true,
                dropped: true,
            }
        );

        // Browser input still reaches the seat during its lease.
        state.keyboard_key(KeyboardSource::MAIN, 32, KeyState::Pressed);
        assert!(pressed(&state, 32));
    }

    #[test]
    fn agent_input_resumes_after_the_browser_hands_back() {
        let (_display, mut state) = test_state();
        let agent = KeyboardSource::new_auxiliary();
        lease(&mut state, true);
        state.keyboard_key(agent, 31, KeyState::Pressed);
        state.pointer_button(agent, 0x111, ButtonState::Pressed);
        state.keyboard_key(KeyboardSource::MAIN, 32, KeyState::Pressed);
        assert!(!pressed(&state, 31));
        assert!(state.buttons.get(&agent).is_none_or(HashSet::is_empty));
        assert!(pressed(&state, 32));

        lease(&mut state, false);

        assert!(!pressed(&state, 32), "the browser's held key is released");
        assert!(
            !pressed(&state, 31),
            "input dropped during the lease must not appear on handback"
        );
        state.keyboard_key(agent, 31, KeyState::Pressed);
        state.pointer_button(agent, 0x111, ButtonState::Pressed);
        assert!(pressed(&state, 31), "agent keys resume after handback");
        assert!(
            state
                .buttons
                .get(&agent)
                .is_some_and(|buttons| buttons.contains(&0x111)),
            "agent buttons resume after handback"
        );
        assert_eq!(
            state.agent_input,
            pipe::AgentInput {
                connected: true,
                dropped: false,
            }
        );
    }

    #[test]
    fn frame_work_does_not_lengthen_the_period() {
        let start = Instant::now();
        let interval = Duration::from_millis(20);
        let next = frame_deadline(start, start + Duration::from_millis(7), interval);
        assert_eq!(next, start + interval);
        assert_eq!(
            frame_deadline(next, next + Duration::from_millis(13), interval),
            start + interval * 2,
        );
        // A new FPS takes effect without preserving the old period.
        assert_eq!(
            frame_deadline(
                next,
                next + Duration::from_millis(2),
                Duration::from_millis(10)
            ),
            start + Duration::from_millis(30),
        );
    }

    #[test]
    fn frame_overrun_discards_missed_deadlines() {
        let start = Instant::now();
        let interval = Duration::from_millis(20);
        for elapsed in [20, 21, 137] {
            let finished = start + Duration::from_millis(elapsed);
            let next = frame_deadline(start, finished, interval);
            assert_eq!(next, finished);
            assert_eq!(
                frame_deadline(next, next + Duration::from_millis(3), interval),
                finished + interval,
            );
        }
    }
}
