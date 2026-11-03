//! Headless Wayland compositor streaming H.264 RTP.

mod activation;
mod capture;
mod clipboard;
mod cursor;
mod decorations;
mod delivery;
mod dnd;
mod eis;
mod event_writer;
mod focus;
mod foreign_toplevel;
mod grabs;
mod input;
mod pointer_warp;
mod portal;
mod screencopy;
mod shell;
mod text_keyboard;
mod toplevel_list;
mod virtual_keyboard;
mod virtual_pointer;
mod wm;
mod xwayland;

use std::cell::RefCell;
use std::collections::HashMap;
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
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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
use calloop::timer::TimeoutAction;
use calloop::timer::Timer;
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
use smithay::desktop::PopupGrab;
use smithay::desktop::PopupManager;
use smithay::desktop::PopupPointerGrab;
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
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Physical;
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
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::text_input::TextInputSeat;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_pointer::VirtualPointerDevice;
use smithay::wayland::xdg_activation::XdgActivationState;
use waywire_decorations::DecorationAction;
use waywire_decorations::TaskbarChip;
use waywire_decorations::WindowAction;
use waywire_encoder::latency;
use waywire_encoder::striped;
use waywire_encoder::striped::StripedEncoder;
use waywire_encoder::striped::StripedFrame;
use waywire_encoder::video;
use waywire_encoder::video::CapturedFrame;
use waywire_encoder::video::EncoderConfig;
use waywire_encoder::video::Notification;
use waywire_encoder::video::VideoEncoder;
use waywire_encoder::video::encoded_dimensions;
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
use crate::input::PointerMotionKind;

/// Startup settings for the compositor and its initial video stream.
#[derive(Debug)]
pub struct Config {
    pub ffmpeg: String,
    pub encoder: EncoderChoice,
    /// Horizontal stripes for the in-process encoder; 0 picks one a core.
    pub stripes: usize,
    pub frame_rate: Fps,
    pub bitrate: Kbps,
    pub rtp_port: u16,
    pub xkb_layout: String,
    pub resolution: FrameSize,
    pub pins: Vec<Vec<String>>,
    /// Application argv. When empty, `WAYWIRE_SESSION` names one executable.
    pub session: Vec<OsString>,
}

/// Which H.264 encoder produces the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderChoice {
    /// libx264 or libopenh264 in an `FFmpeg` child process, delivered over RTP.
    Ffmpeg,
    /// `OpenH264` in process, in horizontal stripes, delivered as encoded-frame events.
    OpenH264,
}

/// The running encoder.
pub(crate) enum Video {
    Ffmpeg(VideoEncoder),
    Striped(StripedEncoder),
}

impl Video {
    fn set_generation(&self, generation: Generation) -> Result<(), video::VideoError> {
        match self {
            Self::Ffmpeg(video) => video.set_generation(generation),
            Self::Striped(video) => video.set_generation(generation),
        }
    }

    fn try_notification(&self) -> Option<Notification> {
        match self {
            Self::Ffmpeg(video) => video.try_notification(),
            Self::Striped(_) => None,
        }
    }

    fn take_notification_source(&mut self) -> Option<calloop::ping::PingSource> {
        match self {
            Self::Ffmpeg(video) => video.take_notification_source(),
            Self::Striped(video) => video.take_notification_source(),
        }
    }

    fn stop(self) {
        match self {
            Self::Ffmpeg(video) => video.stop(),
            Self::Striped(video) => video.stop(),
        }
    }

    /// Frames a still screen is refreshed over: x264 refines a repeated frame a little
    /// each pass, and the striped encoder spreads its stripes' refreshes.
    const fn refresh_frames(&self) -> u8 {
        match self {
            Self::Ffmpeg(_) => REFRESH_FRAMES,
            #[expect(clippy::cast_possible_truncation, reason = "a small constant")]
            Self::Striped(_) => striped::REFRESH_FRAMES as u8,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameRequest {
    Idle,
    Pending,
}

struct TitlebarPress {
    source: KeyboardSource,
    window: Window,
    location: Point<f64, Logical>,
    time: Instant,
}

struct Pin {
    command: Vec<String>,
    pid: Option<u32>,
    child: Option<Child>,
}

impl Drop for Pin {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            drop(child.kill());
            drop(child.wait());
        }
    }
}

struct LaunchEnvironment {
    socket: std::path::PathBuf,
    runtime: std::path::PathBuf,
    eis: std::path::PathBuf,
    portal: Option<Vec<(String, String)>>,
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
    pub(crate) surfaces: Vec<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    pub(crate) cursor_surface: Option<WlSurface>,
    pub(crate) cursor_image: Option<pipe::CursorImage>,
    pub(crate) shm_state: ShmState,
    pub(crate) xdg_shell_state: XdgShellState,
    pub(crate) seat_state: SeatState<Self>,
    pub(crate) data_device_state: DataDeviceState,
    pub(crate) drag: Option<dnd::Drag>,
    pub(crate) activation: XdgActivationState,
    pub(crate) popups: PopupManager,
    /// The popup grab holding the pointer, kept to see when it ends.
    popup_grab: Option<PopupGrab<State>>,
    pub(crate) xwm: Option<smithay::xwayland::X11Wm>,
    /// Last X11 stacking order sent to the XWM; `None` forces the next sync,
    /// which also flushes requests such as `new_selection` that do not flush.
    pub(crate) x11_stacking: Option<Vec<u32>>,
    pub(crate) xdisplay: Option<u32>,
    pub(crate) xwayland_client: Option<smithay::reexports::wayland_server::Client>,
    pub(crate) xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    pub(crate) eis_seats: HashMap<KeyboardSource, smithay::backend::libei::EiInputSeat>,
    pub(crate) virtual_pointers: HashMap<VirtualPointerDevice, KeyboardSource>,
    pub(crate) virtual_keyboards: virtual_keyboard::VirtualKeyboards,
    text_keyboard: text_keyboard::TextKeyboard,
    capture: capture::Capture,
    screencopy: screencopy::Screencopy,
    /// `ScreenCast` portal video, present while the private `PipeWire`
    /// daemon is reachable.
    screencast: Option<waywire_pipewire::Streams>,
    pub(crate) toplevel_list: toplevel_list::ToplevelList,
    pub(crate) delivery: delivery::Delivery,
    clipboard: clipboard::Clipboard,
    windows: wm::Windows,
    pins: Vec<Pin>,
    launch_environment: Option<LaunchEnvironment>,
    loop_handle: Option<calloop::LoopHandle<'static, State>>,
    restore_key_source: Option<KeyboardSource>,
    decoration_press: Option<(KeyboardSource, Window)>,
    window_menu: Option<waywire_decorations::WindowMenu>,
    taskbar: waywire_decorations::Taskbar,
    clock: String,
    /// A press the compositor consumed, so its release is consumed too.
    menu_button: Option<(KeyboardSource, u32, delivery::Chrome)>,
    menu_escape: Option<KeyboardSource>,
    last_titlebar_press: Option<TitlebarPress>,
    pending_restore_drag: Option<TitlebarPress>,
    start: Instant,
    running: bool,
    failure: Option<anyhow::Error>,
    event_sink: EventSink,
    video: Option<Video>,
    quality: Quality,
    size: FrameSize,
    generation: Generation,
    acknowledged: Option<Generation>,
    sequence: u64,
    latest_input_sequence: Option<InputSequence>,
    input: input::Input,
    pub(crate) dirty: bool,
    pub(crate) frame_pending: FrameRequest,
    /// The content hash of the last submitted frame when that frame was a forced full
    /// redraw, which reports the whole output damaged whether or not a pixel changed.
    forced_frame_hash: Option<u64>,
    refresh: Refresh,
}

/// The seat's key repeat, in keys per second after a delay in milliseconds,
/// while no browser holds input. Clients repeat held keys on their own timer.
const KEY_REPEAT_RATE: i32 = 25;
const KEY_REPEAT_DELAY_MS: i32 = 200;
/// Once nothing has changed for `REFRESH_DELAY`, the last frame is encoded
/// `REFRESH_FRAMES` more times. x264's constant rate factor codes a still frame's
/// remaining residual finer on each pass: a scrolled page gains about 2.5 dB after
/// six. The delay keeps the refresh out of typing and pointer cadences.
const REFRESH_DELAY: Duration = Duration::from_millis(500);
const REFRESH_FRAMES: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refresh {
    Idle,
    Due { at: Instant, left: u8 },
}

/// Advertise the globals whose server state `State` never needs to reach.
fn install_globals(display_handle: &DisplayHandle) {
    smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<State>(display_handle);
    XdgDialogState::new::<State>(display_handle);
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
        video: Video,
    ) -> Result<Self> {
        let compositor_state = CompositorState::new_v6::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(&display_handle, [
            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Maximize,
            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Fullscreen,
        ]);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let clipboard = clipboard::Clipboard::new(&display_handle);
        let activation = XdgActivationState::new::<Self>(&display_handle);
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
            KEY_REPEAT_DELAY_MS,
            KEY_REPEAT_RATE,
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
            surfaces: Vec::new(),
            cursor_surface: None,
            cursor_image: None,
            shm_state,
            xdg_shell_state,
            seat_state,
            data_device_state,
            drag: None,
            activation,
            popups: PopupManager::default(),
            popup_grab: None,
            xwm: None,
            x11_stacking: None,
            xdisplay: None,
            xwayland_client: None,
            xwayland_shell_state,
            eis_seats: HashMap::new(),
            virtual_pointers: HashMap::new(),
            virtual_keyboards: virtual_keyboard::VirtualKeyboards::default(),
            text_keyboard: text_keyboard::TextKeyboard::default(),
            capture,
            screencopy,
            screencast: None,
            toplevel_list,
            delivery: delivery::Delivery::default(),
            clipboard,
            windows: wm::Windows::default(),
            pins: options
                .pins
                .iter()
                .map(|command| Pin {
                    command: command.clone(),
                    pid: None,
                    child: None,
                })
                .collect(),
            launch_environment: None,
            loop_handle: None,
            restore_key_source: None,
            decoration_press: None,
            window_menu: None,
            taskbar: waywire_decorations::Taskbar::default(),
            clock: local_clock(SystemTime::now())?,
            menu_button: None,
            menu_escape: None,
            last_titlebar_press: None,
            pending_restore_drag: None,
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
            input: input::Input::default(),
            dirty: true,
            frame_pending: FrameRequest::Pending,
            forced_frame_hash: None,
            refresh: Refresh::Idle,
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

    /// The gateway lost its key frame: the in-process encoder codes the next frame as one.
    /// The `FFmpeg` encoder's periodic key frames need no request.
    fn request_key(&self) {
        if let Some(Video::Striped(video)) = &self.video {
            video.request_key();
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
        self.under(pos)
            .map(|(_, surface, location)| (surface, location))
    }

    /// The pointer target at `pos`: X11 windows as themselves, so drags reach
    /// them through XDND, and Wayland surfaces as the surface hit.
    pub(crate) fn pointer_focus_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(focus::PointerFocus, Point<f64, Logical>)> {
        self.under(pos).map(|(window, surface, location)| {
            let focus = match window.x11_surface() {
                Some(x11) => focus::PointerFocus::X11(x11.clone()),
                None => focus::PointerFocus::Wayland(surface),
            };
            (focus, location)
        })
    }

    fn under(&self, pos: Point<f64, Logical>) -> Option<(&Window, WlSurface, Point<f64, Logical>)> {
        if self.window_menu.is_some() || self.decoration_under(pos).is_some() {
            return None;
        }
        self.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, offset)| (window, surface, (offset + location).to_f64()))
        })
    }

    fn element_under(&self, pos: Point<f64, Logical>) -> Option<(&Window, Point<i32, Logical>)> {
        let over_bar = self
            .work_area()
            .is_some_and(|area| pos.y >= f64::from(area.loc.y + area.size.h));
        for window in self.space.elements().rev() {
            if self.windows.is_minimized(window) {
                continue;
            }
            if over_bar && !waywire_decorations::above_taskbar(window) {
                break;
            }
            let location = self.space.element_location(window)? - window.geometry().loc;
            if window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .is_some()
            {
                return Some((window, location));
            }
        }
        None
    }

    fn taskbar_chips(&self) -> Vec<TaskbarChip> {
        let focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        let windows: Vec<_> = self
            .windows
            .mapped_order()
            .iter()
            .filter(|window| waywire_decorations::taskbar_window(window))
            .cloned()
            .collect();
        let mut matches = vec![None; self.pins.len()];
        let mut used = Vec::new();
        for (index, pin) in self.pins.iter().enumerate() {
            if let Some(pid) = pin.pid
                && let Some(window) = windows
                    .iter()
                    .find(|window| !used.contains(*window) && self.window_pid(window) == Some(pid))
            {
                used.push(window.clone());
                matches[index] = Some(window.clone());
            }
        }
        for (index, pin) in self.pins.iter().enumerate() {
            if matches[index].is_some() {
                continue;
            }
            let name = pin
                .command
                .first()
                .and_then(|command| std::path::Path::new(command).file_name())
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if let Some(window) = windows.iter().find(|window| {
                !used.contains(*window)
                    && toplevel_list::metadata(window)
                        .is_some_and(|metadata| metadata.1.eq_ignore_ascii_case(name))
            }) {
                used.push(window.clone());
                matches[index] = Some(window.clone());
            }
        }
        let make_chip = |window: Option<Window>, pin: Option<usize>, fallback: String| {
            let title = window
                .as_ref()
                .and_then(toplevel_list::metadata)
                .map_or(fallback, |metadata| metadata.0);
            TaskbarChip {
                running: window.is_some()
                    || pin.is_some_and(|index| self.pins[index].child.is_some()),
                minimized: window
                    .as_ref()
                    .and_then(|window| self.windows.get(window))
                    .is_some_and(|state| state.minimized),
                focused: window.as_ref().is_some_and(|window| {
                    focus.is_some() && focus == focus::KeyboardFocus::window(window)
                }),
                urgent: window
                    .as_ref()
                    .and_then(|window| self.windows.get(window))
                    .is_some_and(|state| state.urgent),
                window,
                pin,
                title,
            }
        };
        let mut chips: Vec<_> = self
            .pins
            .iter()
            .enumerate()
            .map(|(index, pin)| {
                make_chip(
                    matches[index].clone(),
                    Some(index),
                    pin.command
                        .first()
                        .and_then(|command| std::path::Path::new(command).file_name())
                        .and_then(|name| name.to_str())
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect();
        chips.extend(
            windows
                .into_iter()
                .filter(|window| !used.contains(window))
                .map(|window| make_chip(Some(window), None, String::new())),
        );
        chips
    }

    fn window_pid(&self, window: &Window) -> Option<u32> {
        if let Some(surface) = window.x11_surface() {
            return surface.pid();
        }
        let surface = window.toplevel()?.wl_surface();
        let credentials = surface
            .client()?
            .get_credentials(&self.display_handle)
            .ok()?;
        u32::try_from(credentials.pid).ok()
    }

    fn launch_pin(&mut self, index: usize) {
        let Some(pin) = self.pins.get(index) else {
            return;
        };
        if pin.child.is_some() {
            return;
        }
        let Some(program) = pin.command.first() else {
            return;
        };
        let mut command = match application_command(
            program,
            &pin.command[1..],
            self.launch_environment.as_ref(),
            self.xdisplay,
        ) {
            Ok(command) => command,
            Err(error) => {
                tracing::warn!(%error, "prepare pinned application");
                return;
            }
        };
        // The pin click is the user's request: a single-instance app that
        // forwards this launch to its running instance may raise it.
        command.env("XDG_ACTIVATION_TOKEN", self.launch_token());
        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                if let Some(handle) = &self.loop_handle
                    && let Err(error) = wake_on_exit(handle, pid)
                {
                    tracing::warn!(%error, "watch pinned application");
                }
                self.pins[index].pid = Some(pid);
                self.pins[index].child = Some(child);
                self.dirty = true;
            }
            Err(error) => tracing::warn!(%error, "start pinned application"),
        }
    }

    fn reap_pins(&mut self) {
        for pin in &mut self.pins {
            if let Some(child) = &mut pin.child {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        pin.child = None;
                        pin.pid = None;
                        self.dirty = true;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error, "wait for pinned application"),
                }
            }
        }
    }

    /// Gate every seat input entry point. Browser input always passes. Agent
    /// sources are registered on first use so a takeover can release them;
    /// while a browser holds the lease their events are dropped, with one
    /// debug line per source per lease rather than one per event.
    fn accepts_input(&mut self, source: KeyboardSource) -> bool {
        self.register_agent_source(source);
        self.input.accepts(source)
    }

    pub(crate) fn register_agent_source(&mut self, source: KeyboardSource) {
        if let Some(state) = self.input.register(source) {
            self.emit(Event::AgentInput(state));
        }
    }

    /// An adapter's device is gone. Release its held input and stop counting
    /// it as connected.
    pub(crate) fn disconnect_input_source(&mut self, source: KeyboardSource) {
        self.release_input(source);
        self.delivery.forget(source);
        if let Some(state) = self.input.disconnect(source) {
            self.emit(Event::AgentInput(state));
        }
    }

    fn control_text(&self) -> &'static str {
        if self.input.browser_holds_input() {
            "You're driving"
        } else {
            "Agent driving"
        }
    }

    fn update_clock(&mut self, time: SystemTime) -> Result<()> {
        let clock = local_clock(time)?;
        if self.clock != clock {
            self.clock = clock;
            self.dirty = true;
        }
        Ok(())
    }

    /// Apply a browser takeover or handback. On takeover, every agent source
    /// releases what it holds before its future input is dropped. On handback,
    /// the browser's own keys are released and agent input resumes.
    ///
    /// While a browser holds input, clients do not repeat keys themselves:
    /// the browser's own repeats are replayed instead (see
    /// [`Self::keyboard_repeat`]). A page whose main thread is busy sends a
    /// key's release late, and a client repeating on its own timer would type
    /// that key until the release arrived.
    pub(crate) fn set_browser_lease(&mut self, held: bool) {
        let Some(release) = self.input.set_browser_lease(held) else {
            return;
        };
        self.dirty = true;
        self.void_activation_tokens();
        for source in release {
            self.release_input(source);
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard
                .change_repeat_info(if held { 0 } else { KEY_REPEAT_RATE }, KEY_REPEAT_DELAY_MS);
        }
        self.toplevel_list.lease_changed();
        if let Some(state) = self.input.publish() {
            self.emit(Event::AgentInput(state));
        }
    }

    pub(crate) fn keyboard_key(&mut self, source: KeyboardSource, key: u32, state: KeyState) {
        self.keyboard_key_inner(source, key, state, true);
    }

    /// Replay one of the browser's repeats of a key it holds. While a browser
    /// holds input the seat tells clients not to repeat keys, so a held key
    /// repeats exactly as often as the browser does. Before `wl_keyboard`
    /// version 10 there is no repeat event, so a repeat reaches the client as a
    /// release and a press, as X11 autorepeat does. A key the compositor kept
    /// for itself, a key no longer held, and a key the keymap does not repeat,
    /// such as a modifier, are left alone.
    fn keyboard_repeat(&mut self, source: KeyboardSource, key: u32) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let code = (key + 8).into();
        let repeats = keyboard.with_xkb_state(self, |context| {
            let xkb = context
                .xkb()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: the reference does not outlive the lock.
            unsafe { xkb.keymap() }.key_repeats(code)
        });
        if !self.input.browser_holds_input() || !repeats || !keyboard.pressed_keys().contains(&code)
        {
            return;
        }
        let serial = self.begin_input(source, delivery::FocusCause::Key);
        let (target, text_field) = self.keyboard_target();
        for state in [KeyState::Released, KeyState::Pressed] {
            keyboard.input_from_source::<(), _>(
                source,
                self,
                code,
                state,
                SERIAL_COUNTER.next_serial(),
                InputTime::now(),
                |_, _, _| FilterResult::Forward,
            );
        }
        self.finish_input(
            source,
            serial,
            delivery::InputKind::Key { pressed: true },
            &target,
            Some(text_field),
        );
    }

    /// A key the compositor picked for its symbol, such as a spare keycode
    /// that holds the character a virtual keyboard typed: pressing it keeps
    /// that character.
    pub(crate) fn keyboard_symbol_key(
        &mut self,
        source: KeyboardSource,
        key: u32,
        state: KeyState,
    ) {
        self.keyboard_key_inner(source, key, state, false);
    }

    fn keyboard_key_inner(
        &mut self,
        source: KeyboardSource,
        key: u32,
        state: KeyState,
        reclaim: bool,
    ) {
        let serial = self.begin_input(source, delivery::FocusCause::Key);
        let (target, text_field) = self.dispatch_key(source, key, state, reclaim);
        let input = delivery::InputKind::Key {
            pressed: state == KeyState::Pressed,
        };
        self.finish_input(source, serial, input, &target, text_field);
    }

    /// Route one key and say where it went.
    fn dispatch_key(
        &mut self,
        source: KeyboardSource,
        key: u32,
        state: KeyState,
        reclaim: bool,
    ) -> (delivery::Target, Option<delivery::TextField>) {
        use delivery::Chrome;
        use delivery::Target;
        if !self.accepts_input(source) {
            return (Target::Dropped, None);
        }
        if key == 1
            && state == KeyState::Pressed
            && let Some(menu) = self.window_menu.take()
        {
            self.menu_escape = Some(source);
            self.dirty = true;
            self.refresh_pointer_focus();
            return (
                Target::Compositor {
                    window: Some(menu.window),
                    chrome: Chrome::WindowMenu,
                },
                None,
            );
        }
        if key == 1 && state == KeyState::Released && self.menu_escape == Some(source) {
            self.menu_escape = None;
            return (
                Target::Compositor {
                    window: None,
                    chrome: Chrome::WindowMenu,
                },
                None,
            );
        }
        if key == 50 && state == KeyState::Released && self.restore_key_source == Some(source) {
            self.restore_key_source = None;
            return (
                Target::Compositor {
                    window: None,
                    chrome: Chrome::Shortcut,
                },
                None,
            );
        }
        if key == 50
            && state == KeyState::Pressed
            && self.seat.get_keyboard().is_some_and(|keyboard| {
                let mods = keyboard.modifier_state();
                mods.logo && mods.shift && !mods.ctrl && !mods.alt
            })
        {
            self.restore_key_source = Some(source);
            let window = self.windows.minimized_order().last().cloned();
            if let Some(window) = &window {
                self.window_action(window, WindowAction::Restore);
            }
            return (
                Target::Compositor {
                    window,
                    chrome: Chrome::Shortcut,
                },
                None,
            );
        }
        if reclaim && state == KeyState::Pressed {
            self.reclaim_text_keycode(key + 8);
        }
        let (target, text_field) = if self.delivery.wanted(source) {
            self.keyboard_target()
        } else {
            (Target::Nowhere, delivery::TextField::Unknown)
        };
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
            if state == KeyState::Released && keyboard.pressed_keys().is_empty() {
                self.flush_text_keyboard();
            }
        }
        (target, Some(text_field))
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

    pub(crate) fn refresh_pointer_focus(&mut self) {
        self.release_ended_popup_grab();
        if let Some(pointer) = self.seat.get_pointer() {
            let window_grab = pointer
                .with_grab(|_, grab| grab.is::<grabs::WindowGrab>())
                .unwrap_or(false);
            if pointer.is_grabbed() && !window_grab {
                return;
            }
            if window_grab {
                self.input
                    .set_pointer_motion_kind(PointerMotionKind::RefreshGrabFocus);
            }
            self.input.begin_focus_refresh();
            self.pointer_motion(KeyboardSource::MAIN, pointer.current_location());
            self.input.end_focus_refresh();
            self.input.set_pointer_motion_kind(PointerMotionKind::Input);
        }
    }

    /// A popup grab ends once its popups are gone, but Smithay releases the
    /// pointer only when the grab sees the next motion, and drops that
    /// motion; a click right after a popup closed then lands at the old
    /// position, and focus refreshes skip the grabbed pointer until then.
    fn release_ended_popup_grab(&mut self) {
        if !self.popup_grab.as_ref().is_some_and(PopupGrab::has_ended) {
            return;
        }
        self.popup_grab = None;
        if let Some(pointer) = self.seat.get_pointer()
            && pointer
                .with_grab(|_, grab| grab.is::<PopupPointerGrab<Self>>())
                .unwrap_or(false)
        {
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::now());
        }
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
        self.release_ended_popup_grab();
        let scale = self.output.current_scale().fractional_scale();
        let mut location = Point::from((
            location
                .x
                .clamp(0.0, (f64::from(self.size.width()) - 1.0) / scale),
            location
                .y
                .clamp(0.0, (f64::from(self.size.height()) - 1.0) / scale),
        ));
        if let Some(pointer) = self.seat.get_pointer() {
            let mut locked = false;
            if self.input.pointer_motion_kind() == PointerMotionKind::Input {
                (location, locked) = self.constrain_pointer_motion(&pointer, location);
            }
            if let Some(menu) = self.window_menu.as_mut() {
                self.dirty |= menu.motion(location);
            }
            if !locked {
                pointer.motion(
                    self,
                    self.pointer_focus_under(location),
                    &MotionEvent {
                        location,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::now(),
                    },
                );
            }
            if let Some(delta) = delta {
                pointer.relative_motion(
                    self,
                    self.pointer_focus_under(location),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: delta,
                        time: InputTime::now(),
                    },
                );
            }
            self.activate_focused_constraint(&pointer);
            pointer.frame(self);
        }
        if self.input.pointer_motion_kind() == PointerMotionKind::Input {
            self.update_titlebar_drag(source, location);
        }
        if self.decoration_press.as_ref().is_some_and(|(_, window)| {
            self.decoration_under(location) != Some((window.clone(), DecorationAction::Close))
        }) {
            self.decoration_press = None;
        }
        if self.drag.is_some() {
            // The drag icon follows the pointer, and the drag keeps its cursor.
            self.frame_pending = FrameRequest::Pending;
        } else if let Some(shape) = self.decoration_cursor_under(location) {
            self.emit(Event::CursorShape(shape));
        } else if self.surface_under(location).is_none() {
            self.emit(Event::CursorShape(pipe::CursorShape::Default));
        }
        let cursor_source = self.input.move_cursor(source);
        self.emit(Event::CursorPosition(pipe::CursorPosition {
            x: normalized_coordinate(location.x * scale, self.size.width()),
            y: normalized_coordinate(location.y * scale, self.size.height()),
            source: cursor_source,
        }));
    }

    fn update_titlebar_drag(&mut self, source: KeyboardSource, location: Point<f64, Logical>) {
        if self.pending_restore_drag.as_ref().is_some_and(|press| {
            press.source == source
                && (location - press.location)
                    .x
                    .hypot((location - press.location).y)
                    > 4.0
        }) && let Some(press) = self.pending_restore_drag.take()
        {
            self.last_titlebar_press = None;
            if let Some(maximized) = self.window_output_geometry(&press.window) {
                let restored = self
                    .windows
                    .get(&press.window)
                    .and_then(|state| state.restore);
                self.window_action(&press.window, WindowAction::Restore);
                let restored_size = restored.map_or_else(
                    || {
                        press
                            .window
                            .x11_surface()
                            .map_or(press.window.geometry().size, |surface| {
                                surface.last_configure().size
                            })
                    },
                    |geometry| geometry.size,
                );
                let position =
                    wm::restored_drag_location(press.location, location, maximized, restored_size);
                self.space.map_element(press.window.clone(), position, true);
                self.enforce_topmost();
                if let Some(surface) = press.window.x11_surface()
                    && let Err(error) = surface.configure(Some(
                        smithay::utils::Rectangle::new(position, restored_size)
                            + surface.frame_extents(),
                    ))
                {
                    tracing::warn!(%error, "position restored X11 window");
                }
                self.window_action(&press.window, WindowAction::Move);
            }
        }
        if self.last_titlebar_press.as_ref().is_some_and(|press| {
            press.source == source
                && self
                    .input
                    .buttons(source)
                    .is_some_and(|buttons| buttons.contains(&0x110))
                && (location - press.location)
                    .x
                    .hypot((location - press.location).y)
                    > 4.0
        }) {
            self.last_titlebar_press = None;
        }
    }

    pub(crate) fn pointer_button(
        &mut self,
        source: KeyboardSource,
        button: u32,
        state: ButtonState,
    ) {
        let serial = self.begin_input(source, delivery::FocusCause::Click);
        let target = if self.accepts_input(source) {
            if source != KeyboardSource::MAIN
                && state == ButtonState::Pressed
                && let Some(pointer) = self.seat.get_pointer()
            {
                let location = pointer.current_location();
                let scale = self.output.current_scale().fractional_scale();
                self.emit(Event::CursorClick(pipe::CursorPosition {
                    x: normalized_coordinate(location.x * scale, self.size.width()),
                    y: normalized_coordinate(location.y * scale, self.size.height()),
                    source: pipe::CursorSource::Agent,
                }));
            }
            self.pointer_button_inner(source, button, state)
        } else {
            delivery::Target::Dropped
        };
        let input = delivery::InputKind::Button {
            button,
            pressed: state == ButtonState::Pressed,
        };
        self.finish_input(source, serial, input, &target, None);
    }

    pub(crate) fn pointer_axis(&mut self, source: KeyboardSource, frame: AxisFrame) {
        let serial = self.begin_input(source, delivery::FocusCause::Other);
        let target = if self.accepts_input(source) {
            let target = self.pointer_target();
            if let Some(pointer) = self.seat.get_pointer() {
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            target
        } else {
            delivery::Target::Dropped
        };
        self.finish_input(source, serial, delivery::InputKind::Scroll, &target, None);
    }

    /// Route one button event and say where it went.
    #[expect(
        clippy::too_many_lines,
        reason = "pointer dispatch keeps button ownership together"
    )]
    fn pointer_button_inner(
        &mut self,
        source: KeyboardSource,
        button: u32,
        state: ButtonState,
    ) -> delivery::Target {
        use delivery::Chrome;
        use delivery::Target;
        if button == 0x110
            && state == ButtonState::Released
            && self
                .pending_restore_drag
                .as_ref()
                .is_some_and(|press| press.source == source)
        {
            self.pending_restore_drag = None;
        }
        if !self.input.track_button(source, button, state) {
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
            // Another source holds the same button, so no event is sent.
            return Target::Nowhere;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return Target::Nowhere;
        };
        let serial = SERIAL_COUNTER.next_serial();
        if state == ButtonState::Released
            && let Some((owner, held, chrome)) = self.menu_button
            && owner == source
            && held == button
        {
            self.menu_button = None;
            return Target::Compositor {
                window: None,
                chrome,
            };
        }
        if let Some(menu) = self.window_menu.as_ref() {
            if state == ButtonState::Pressed {
                let position = pointer.current_location();
                let selected = menu.click(position);
                let outside = !menu.contains(position);
                let window = menu.window.clone();
                if outside || (button == 0x110 && selected.is_some()) {
                    self.window_menu = None;
                    self.dirty = true;
                }
                if button == 0x110
                    && let Some(action) = selected
                {
                    if matches!(action, WindowAction::Move | WindowAction::Resize(_)) {
                        pointer.button(
                            self,
                            &ButtonEvent {
                                button,
                                state,
                                serial,
                                time: InputTime::now(),
                            },
                        );
                        self.window_action(&window, action);
                        pointer.frame(self);
                        return Target::Compositor {
                            window: Some(window),
                            chrome: Chrome::WindowMenu,
                        };
                    }
                    self.window_action(&window, action);
                }
                self.menu_button = Some((source, button, Chrome::WindowMenu));
                self.refresh_pointer_focus();
                return Target::Compositor {
                    window: Some(window),
                    chrome: Chrome::WindowMenu,
                };
            }
            return Target::Compositor {
                window: None,
                chrome: Chrome::WindowMenu,
            };
        }
        if state == ButtonState::Pressed && button == 0x110 && !pointer.is_grabbed() {
            let position = pointer.current_location();
            let covered = self
                .element_under(position)
                .is_some_and(|(window, _)| waywire_decorations::above_taskbar(window));
            if !covered && let Some(output) = self.space.output_geometry(&self.output) {
                let chips = self.taskbar_chips();
                let relative = position
                    - (output.loc + Point::from((0, output.size.h - waywire_decorations::TASKBAR)))
                        .to_f64();
                if let Some(chip) = chips.iter().enumerate().find_map(|(index, chip)| {
                    let rect =
                        waywire_decorations::chip_rectangle(output.size.w, chips.len(), index)?;
                    rect.to_f64().contains(relative).then_some(chip)
                }) {
                    let action = if chip.minimized {
                        WindowAction::Restore
                    } else if chip.focused {
                        WindowAction::Minimize
                    } else {
                        WindowAction::Focus
                    };
                    let window = chip.window.clone();
                    if let Some(window) = &window {
                        self.window_action(window, action);
                    } else if let Some(pin) = chip.pin {
                        self.launch_pin(pin);
                    }
                    self.menu_button = Some((source, button, Chrome::Taskbar));
                    return Target::Compositor {
                        window,
                        chrome: Chrome::Taskbar,
                    };
                }
            }
        }
        if state == ButtonState::Pressed
            && button == 0x111
            && !pointer.is_grabbed()
            && let Some((window, DecorationAction::Move)) =
                self.decoration_under(pointer.current_location())
        {
            self.window_action(&window, WindowAction::ShowMenu);
            self.menu_button = Some((source, button, Chrome::Titlebar));
            return Target::Compositor {
                window: Some(window),
                chrome: Chrome::Titlebar,
            };
        }
        if state == ButtonState::Pressed
            && button == 0x110
            && !pointer.is_grabbed()
            && let Some((window, action)) = self.decoration_under(pointer.current_location())
        {
            self.window_action(&window, WindowAction::Focus);
            pointer.button(
                self,
                &ButtonEvent {
                    button,
                    state,
                    serial,
                    time: InputTime::now(),
                },
            );
            self.press_decoration(source, window.clone(), action);
            pointer.frame(self);
            return Target::Compositor {
                window: Some(window),
                chrome: Chrome::Titlebar,
            };
        }
        if state == ButtonState::Pressed
            && !pointer.is_grabbed()
            && let Some(window) = self
                .element_under(pointer.current_location())
                .map(|(window, _)| window.clone())
        {
            self.focus_window(Some(window));
        }
        if state == ButtonState::Released {
            self.refresh_drag_target();
        }
        let target = match &self.decoration_press {
            Some((owner, window)) if *owner == source && state == ButtonState::Released => {
                Target::Compositor {
                    window: Some(window.clone()),
                    chrome: Chrome::Titlebar,
                }
            }
            _ if self.delivery.wanted(source) => self.pointer_target(),
            _ => Target::Nowhere,
        };
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
        if state == ButtonState::Released {
            self.refresh_pointer_focus();
        }
        if state == ButtonState::Released
            && button == 0x110
            && let Some((owner, window)) = self.decoration_press.take()
            && owner == source
            && self.decoration_under(pointer.current_location())
                == Some((window.clone(), DecorationAction::Close))
        {
            self.window_action(&window, WindowAction::Close);
        }
        target
    }

    fn press_decoration(
        &mut self,
        source: KeyboardSource,
        window: Window,
        action: DecorationAction,
    ) {
        match action {
            DecorationAction::Move => {
                let now = Instant::now();
                let double = self.last_titlebar_press.as_ref().is_some_and(|press| {
                    press.source == source
                        && press.window == window
                        && now.duration_since(press.time) <= Duration::from_millis(400)
                });
                let maximized = window.toplevel().is_some_and(|top| {
                    top.with_pending_state(|state| {
                        state.states.contains(xdg_toplevel::State::Maximized)
                    })
                }) || window
                    .x11_surface()
                    .is_some_and(smithay::xwayland::X11Surface::is_maximized);
                if double {
                    self.last_titlebar_press = None;
                    self.pending_restore_drag = None;
                    self.window_action(
                        &window,
                        if maximized {
                            WindowAction::Restore
                        } else {
                            WindowAction::Maximize
                        },
                    );
                } else if let Some(pointer) = self.seat.get_pointer() {
                    let press = TitlebarPress {
                        source,
                        window: window.clone(),
                        location: pointer.current_location(),
                        time: now,
                    };
                    self.last_titlebar_press = Some(TitlebarPress {
                        source,
                        window: window.clone(),
                        location: press.location,
                        time: now,
                    });
                    if maximized {
                        self.pending_restore_drag = Some(press);
                    } else {
                        self.window_action(&window, WindowAction::Move);
                    }
                }
            }
            DecorationAction::Resize(edges) => {
                self.window_action(&window, WindowAction::Resize(edges));
            }
            DecorationAction::Minimize => {
                self.window_action(&window, WindowAction::Minimize);
            }
            DecorationAction::Maximize => {
                self.window_action(&window, WindowAction::Maximize);
            }
            DecorationAction::Restore => {
                self.window_action(&window, WindowAction::Restore);
            }
            DecorationAction::Close => self.decoration_press = Some((source, window)),
        }
    }

    pub(crate) fn window_action(&mut self, window: &Window, action: WindowAction) {
        match action {
            WindowAction::Focus => self.focus_window(Some(window.clone())),
            WindowAction::Move => {
                self.start_decoration_action(window.clone(), SERIAL_COUNTER.next_serial(), None);
            }
            WindowAction::Resize(edges) => {
                if edges != xdg_toplevel::ResizeEdge::None {
                    self.start_decoration_action(
                        window.clone(),
                        SERIAL_COUNTER.next_serial(),
                        Some(edges),
                    );
                }
            }
            WindowAction::Maximize => {
                self.set_window_expanded(window, xdg_toplevel::State::Maximized, true);
            }
            WindowAction::Minimize => {
                if self.windows.minimize(window) {
                    if let Some(surface) = window.x11_surface()
                        && let Err(error) = surface.set_hidden(true)
                    {
                        tracing::warn!(%error, "set X11 hidden state");
                    }
                    if window.x11_surface().is_some() {
                        self.x11_stacking = None;
                    }
                    let focused = self
                        .seat
                        .get_keyboard()
                        .and_then(|keyboard| keyboard.current_focus());
                    if focused == focus::KeyboardFocus::window(window) {
                        let next = self.windows.topmost_focusable(&self.space, Some(window));
                        self.with_focus_cause(delivery::FocusCause::Minimized, |state| {
                            state.focus_window(next);
                        });
                    }
                    self.refresh_toplevel_states();
                    self.refresh_pointer_focus();
                    self.dirty = true;
                }
            }
            WindowAction::Restore => {
                if self.windows.unminimize(window) {
                    if let Some(surface) = window.x11_surface()
                        && let Err(error) = surface.set_hidden(false)
                    {
                        tracing::warn!(%error, "clear X11 hidden state");
                    }
                    if window.x11_surface().is_some() {
                        self.x11_stacking = None;
                    }
                    self.focus_window(Some(window.clone()));
                } else {
                    self.set_window_expanded(window, xdg_toplevel::State::Maximized, false);
                }
            }
            WindowAction::Close => wm::close_window(window),
            WindowAction::ToggleAlwaysOnTop => {
                self.windows.toggle_always_on_top(window);
                self.enforce_topmost();
                self.dirty = true;
            }
            WindowAction::ShowMenu => {
                if let (Some(pointer), Some(output)) = (
                    self.seat.get_pointer(),
                    self.space.output_geometry(&self.output),
                ) {
                    self.window_menu = Some(waywire_decorations::WindowMenu::new(
                        window.clone(),
                        pointer.current_location(),
                        output.size,
                        self.windows.always_on_top(window),
                    ));
                    self.refresh_pointer_focus();
                    self.dirty = true;
                }
            }
        }
    }

    /// Activate a window from a foreign-toplevel request when the request's
    /// seat belongs to this compositor.
    pub(crate) fn activate_foreign_toplevel(&mut self, window: &Window, seat: &WlSeat) {
        if Seat::<Self>::from_resource(seat).as_ref() != Some(&self.seat) {
            return;
        }
        self.activate_window(window);
    }

    /// Restore a minimized window, or raise and focus it. Foreign-toplevel
    /// and xdg-activation requests both use this path.
    pub(crate) fn activate_window(&mut self, window: &Window) {
        if self.windows.is_minimized(window) {
            self.window_action(window, WindowAction::Restore);
        } else {
            self.window_action(window, WindowAction::Focus);
        }
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
        self.toplevel_list
            .refresh_states(focused.as_ref(), self.windows.minimized_order());
    }

    pub(crate) fn release_input(&mut self, source: KeyboardSource) {
        // Disconnect/ownership loss must release buttons without activating Close.
        if self.restore_key_source == Some(source) {
            self.restore_key_source = None;
        }
        if self.menu_escape == Some(source) {
            self.menu_escape = None;
        }
        if self
            .decoration_press
            .as_ref()
            .is_some_and(|(owner, _)| *owner == source)
        {
            self.decoration_press = None;
        }
        if self
            .last_titlebar_press
            .as_ref()
            .is_some_and(|press| press.source == source)
        {
            self.last_titlebar_press = None;
        }
        if self
            .pending_restore_drag
            .as_ref()
            .is_some_and(|press| press.source == source)
        {
            self.pending_restore_drag = None;
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.release_source(self, source);
            if keyboard.pressed_keys().is_empty() {
                self.flush_text_keyboard();
            }
        }
        let buttons = self.input.buttons(source).cloned().unwrap_or_default();
        if !buttons.is_empty() {
            // Losing a source is not a drop: its drag ends without one.
            self.cancel_drag();
        }
        for button in buttons {
            self.pointer_button_inner(source, button, ButtonState::Released);
        }
        self.input.forget_buttons(source);
    }

    /// Raise always-on-top windows and their dialogs above the rest.
    pub(crate) fn enforce_topmost(&mut self) {
        for window in self.windows.topmost(&self.space) {
            self.space.raise_element(&window, false);
        }
    }

    pub(crate) fn center_dialog(&mut self, window: &Window) {
        let Some(location) =
            wm::dialog_location(&self.space, window, self.window_output_geometry(window))
        else {
            return;
        };
        self.space.map_element(window.clone(), location, false);
        if let Some(surface) = window.x11_surface() {
            let mut geometry = surface.last_configure();
            geometry.loc = location - window.geometry().loc;
            if let Err(error) = surface.configure(geometry) {
                tracing::warn!(%error, "center X11 dialog");
            }
        }
    }

    pub(crate) fn focus_window(&mut self, window: Option<Window>) {
        if window
            .as_ref()
            .is_some_and(|window| self.windows.is_minimized(window))
        {
            return;
        }
        let mut window = window;
        if let Some(target) = &window {
            for member in &self.windows.family(&self.space, target) {
                self.space.raise_element(member, true);
                if let (Some(xwm), Some(surface)) = (&mut self.xwm, member.x11_surface())
                    && let Err(error) = xwm.raise_window(surface)
                {
                    tracing::warn!(%error, "raise X11 window");
                }
            }
            self.enforce_topmost();
            window = Some(self.windows.modal_focus(&self.space, target.clone()));
        }
        if self
            .decoration_press
            .as_ref()
            .is_some_and(|(_, target)| Some(target) != window.as_ref())
        {
            self.decoration_press = None;
        }
        if let Some(window) = &window {
            // X11 menus, dropdowns and tooltips draw above their app but expect
            // keyboard focus to stay with it; taking focus closes them.
            if wm::is_override_redirect(window) {
                self.refresh_pointer_focus();
                self.dirty = true;
                return;
            }
        }
        if let Some(window) = &window {
            self.windows.clear_urgent(window);
        }
        self.refresh_pointer_focus();
        for current in self.space.elements() {
            current.set_activated(window.as_ref() == Some(current));
            if let Some(top) = current.toplevel()
                && top.is_initial_configure_sent()
            {
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
            tracing::debug!(
                target: "waywire_latency",
                stage = "compositor_input",
                input = sequence.get(),
                at_ns = latency::monotonic_nanos(),
            );
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
                pipe::KeyState::Repeated => self.keyboard_repeat(source, p.key.get()),
            },
            Command::ReleaseAll(_) => self.release_input(source),
            Command::InputLease(p) => self.set_browser_lease(p.held),
            Command::PointerScroll(p) => {
                let frame = AxisFrame::new(InputTime::now())
                    .source(AxisSource::Finger)
                    .value(Axis::Horizontal, f64::from(p.dx.get()))
                    .value(Axis::Vertical, f64::from(p.dy.get()));
                self.pointer_axis(source, frame);
            }
            Command::PointerWheel(p) => {
                let mut frame = AxisFrame::new(InputTime::now()).source(AxisSource::Wheel);
                for (axis, value) in [(Axis::Horizontal, p.dx.get()), (Axis::Vertical, p.dy.get())]
                {
                    if value != 0.0 {
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "wheel v120 is bounded by PointerDelta"
                        )]
                        let v120 = value.round() as i32;
                        frame = frame.v120(axis, v120).value(axis, f64::from(v120) / 8.0);
                    }
                }
                self.pointer_axis(source, frame);
            }
            Command::PointerScrollStop(p) => {
                let mut frame = AxisFrame::new(InputTime::now()).source(AxisSource::Finger);
                if p.dx.get() != 0.0 {
                    frame = frame.stop(Axis::Horizontal);
                }
                if p.dy.get() != 0.0 {
                    frame = frame.stop(Axis::Vertical);
                }
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
                crate::xwayland::update_scale(self)?;
                self.output_resized();
                self.update_cursor_image();
                self.capture.output_resized();
                if let Some(screencast) = &self.screencast {
                    screencast.resize((p.size.width(), p.size.height()), self.quality.fps.get());
                }
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
                    if q.fps != self.quality.fps
                        && let Some(mut mode) = self.output.current_mode()
                    {
                        mode.refresh = i32::try_from(q.fps.get())? * 1000;
                        self.output.set_preferred(mode);
                        self.output
                            .change_current_state(Some(mode), None, None, None);
                    }
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
                            self.request_key();
                            None
                        }
                    };
                }
            }
            Command::Text(p) => {
                if matches!(p.action, pipe::TextAction::Commit) {
                    let serial = self.begin_input(source, delivery::FocusCause::Key);
                    let (target, text_field) = self.keyboard_target();
                    self.finish_input(
                        source,
                        serial,
                        delivery::InputKind::Text,
                        &target,
                        Some(text_field),
                    );
                }
                let cursor = i32::try_from(p.text.as_str().len())?;
                let text_input = self.seat.text_input();
                let mut active = false;
                text_input.with_active_text_input(|text, _| {
                    active = true;
                    match p.action {
                        pipe::TextAction::Commit => {
                            text.preedit_string(None, 0, 0);
                            text.commit_string(Some(p.text.as_str().into()));
                        }
                        pipe::TextAction::Preedit => {
                            text.preedit_string(Some(p.text.as_str().into()), cursor, cursor);
                        }
                    }
                });
                text_input.done(false);
                if !active && matches!(p.action, pipe::TextAction::Commit) {
                    self.text_keyboard
                        .pending
                        .push_back(p.text.as_str().to_owned());
                    self.flush_text_keyboard();
                }
            }
            Command::Clipboard(text) => self.clipboard_set(&text),
        }
        Ok(())
    }

    fn drain_video(&mut self) {
        while let Some(frame) = match &self.video {
            Some(Video::Striped(video)) => video.try_frame(),
            _ => None,
        } {
            match frame {
                // A full stdout queue drops this frame rather than the session. The
                // stripes it carried are now out of step, so code a key frame next.
                Ok(frame) => {
                    let sequence = frame.metadata.sequence;
                    match self.event_sink.send(&Event::EncodedFrame(frame)) {
                        Ok(()) => latency::trace_frame("emitted", sequence, 0),
                        Err(event_writer::EventWriterError::QueueFull) => {
                            self.request_key();
                            self.frame_pending = FrameRequest::Pending;
                        }
                        Err(error) => self.fail(error.into()),
                    }
                }
                Err(error) => self.fail(anyhow!(error)),
            }
        }
        while let Some(notification) = self.video.as_ref().and_then(Video::try_notification) {
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
        let render_started_ns = latency::monotonic_nanos();
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
        self.windows.retain_alive();
        self.toplevel_list.cleanup();
        self.popups.cleanup();
        if self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .is_some_and(|focus| !smithay::utils::IsAlive::alive(&focus))
        {
            let next = self.windows.topmost_focusable(&self.space, None);
            self.with_focus_cause(delivery::FocusCause::WindowClosed, |state| {
                state.focus_window(next);
            });
        }
        let elements = self.scene_elements(renderer);
        let forced = self.dirty || self.acknowledged != Some(self.generation);
        let clear = [0.08, 0.09, 0.12, 1.0];
        let (damage, changed) = {
            let mut target = renderer.bind(image)?;
            if forced {
                // A forced redraw repaints everything, but settle waits need
                // what changed since the last frame.
                let (changed, states) = tracker.damage_output(1, &elements)?;
                let changed = changed.cloned().unwrap_or_default();
                let damage = tracker
                    .render_output_with_states(renderer, &mut target, 0, &elements, clear, states)?
                    .damage
                    .cloned();
                (damage, changed)
            } else {
                let damage = tracker
                    .render_output(renderer, &mut target, 1, &elements, clear)?
                    .damage
                    .cloned();
                let changed = damage.clone().unwrap_or_default();
                (damage, changed)
            }
        };
        let drawn = (!forced).then(|| damage.as_deref().unwrap_or_default());
        self.frame_rendered(delivery::monotonic_usec(), &changed, image, drawn);
        if damage.is_some() || forced {
            let full = [Rectangle::<i32, Physical>::from_size(
                (
                    i32::try_from(self.size.width())?,
                    i32::try_from(self.size.height())?,
                )
                    .into(),
            )];
            let damage = if forced {
                &full[..]
            } else {
                damage.as_deref().unwrap_or(&full)
            };
            // A forced redraw reports the whole output damaged. Hash only those, so
            // a redraw that changed no pixel is not encoded again.
            let hash = forced.then(|| xxhash_rust::xxh3::xxh3_64(image_pixels(image)));
            let unchanged = hash.is_some()
                && hash == self.forced_frame_hash
                && self.acknowledged == Some(self.generation);
            self.forced_frame_hash = hash;
            if !unchanged {
                latency::trace_frame("render", self.sequence, render_started_ns);
                self.submit_video(image, damage, false)?;
                self.refresh = Refresh::Due {
                    at: Instant::now() + REFRESH_DELAY,
                    left: self.video.as_ref().map_or(0, Video::refresh_frames),
                };
                if let Some(screencast) = &self.screencast {
                    screencast.frame(&cast_frame(image));
                }
            }
        }
        capture::serve_pending(self, image);
        screencopy::serve_pending(self, image);
        self.dirty = false;
        self.frame_pending = FrameRequest::Idle;
        self.send_cursor_frame();
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

    /// Encode the unchanged output again once the screen has held still.
    fn refresh_video(&mut self, image: &Image<'static, 'static>) -> Result<()> {
        let Refresh::Due { at, left } = self.refresh else {
            return Ok(());
        };
        let now = Instant::now();
        if now < at {
            return Ok(());
        }
        self.submit_video(image, &[], true)?;
        self.refresh = match left.checked_sub(1) {
            Some(left @ 1..) => Refresh::Due {
                at: now + Duration::from_secs_f64(1.0 / f64::from(self.quality.fps.get())),
                left,
            },
            _ => Refresh::Idle,
        };
        Ok(())
    }

    fn submit_video(
        &mut self,
        image: &Image<'static, 'static>,
        damage: &[Rectangle<i32, Physical>],
        refresh: bool,
    ) -> Result<()> {
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
        let metadata = FrameMetadata {
            generation: self.generation,
            width: config.encoded_width,
            height: config.encoded_height,
            capture_nanos: latency::monotonic_nanos(),
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
        match &self.video {
            Some(Video::Ffmpeg(video)) => {
                video.submit(CapturedFrame::new(image_pixels(image), metadata, config))?;
            }
            Some(Video::Striped(video)) => {
                let damage: Vec<_> = damage
                    .iter()
                    .map(|rectangle| {
                        rectangle.loc.y..rectangle.loc.y.saturating_add(rectangle.size.h)
                    })
                    .collect();
                video.submit(&StripedFrame {
                    pixels: image_pixels(image),
                    width: image.width(),
                    height: image.height(),
                    damage: &damage,
                    metadata,
                    crf: self.quality.crf,
                    bitrate: self.quality.bitrate_kbps,
                    refresh,
                })?;
            }
            None => {}
        }
        Ok(())
    }
}

/// The rendered output's packed BGRA bytes.
pub(crate) fn image_pixels<'a>(image: &'a Image<'static, 'static>) -> &'a [u8] {
    debug_assert_eq!(image.stride(), image.width() * 4);
    // Pixman finishes synchronously. Borrow our target instead of allocating
    // and blitting a second full image through ExportMem::copy_framebuffer.
    // SAFETY: create_buffer owns and initializes the complete image storage;
    // the renderer's exclusive target borrow has ended.
    let data = unsafe { image.data() };
    // SAFETY: stride * height covers this live image's allocation. Nothing
    // mutates it while the returned borrow of the image lives.
    unsafe { std::slice::from_raw_parts(data.cast::<u8>(), image.stride() * image.height()) }
}

fn local_clock(time: SystemTime) -> Result<String> {
    let seconds = time.duration_since(UNIX_EPOCH)?.as_secs().try_into()?;
    let mut local = std::mem::MaybeUninit::<nix::libc::tm>::uninit();
    // SAFETY: both pointers reference valid storage, and localtime_r initializes it on success.
    let result = unsafe { nix::libc::localtime_r(&raw const seconds, local.as_mut_ptr()) };
    if result.is_null() {
        return Err(anyhow!("local time conversion failed"));
    }
    // SAFETY: localtime_r returned a non-null pointer to the initialized storage.
    let local = unsafe { local.assume_init() };
    Ok(format!("{:02}:{:02}", local.tm_hour, local.tm_min))
}

fn next_minute_delay(time: SystemTime) -> Duration {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let remainder = Duration::new(elapsed.as_secs() % 60, elapsed.subsec_nanos());
    Duration::from_mins(1).saturating_sub(remainder)
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
    let mut bus = waywire_portal::session_bus::SessionBus::start(runtime.path());
    // ScreenCast video likewise needs our own PipeWire daemon, and only
    // matters when portals run.
    let mut pipewire_daemon = bus
        .as_ref()
        .and_then(|_| waywire_pipewire::Daemon::start(runtime.path()));
    let audio = pipewire_daemon.as_ref().and_then(|daemon| {
        waywire_pipewire::audio::Audio::start(daemon.socket(), options.rtp_port)
    });
    let socket_path = runtime.path().join("wayland-0");
    let socket = ListeningSocket::bind_absolute(socket_path.clone())?;
    let mut event_loop = EventLoop::<State>::try_new()?;
    let handle = event_loop.handle();
    let display = Display::<State>::new()?;
    let (event_writer, sink) = EventWriter::start()?;
    let mut video = match options.encoder {
        EncoderChoice::Ffmpeg => Video::Ffmpeg(VideoEncoder::start(
            options.ffmpeg.clone(),
            options.rtp_port,
        )?),
        EncoderChoice::OpenH264 => {
            Video::Striped(StripedEncoder::start(options.stripes, Generation::new(1)?)?)
        }
    };
    let notifications = video
        .take_notification_source()
        .context("encoder notification source missing")?;
    let mut state = State::new(display.handle(), &options, sink, video)?;
    handle
        .insert_source(
            Timer::from_duration(next_minute_delay(SystemTime::now())),
            |_, (), state| {
                let now = SystemTime::now();
                if let Err(error) = state.update_clock(now) {
                    state.fail(error);
                    return TimeoutAction::Drop;
                }
                TimeoutAction::ToDuration(next_minute_delay(now))
            },
        )
        .map_err(|error| anyhow!("register clock timer: {error}"))?;
    let eis_path = runtime.path().join("eis");
    crate::eis::listen(&handle, &eis_path, options.xkb_layout.clone())?;
    crate::xwayland::start(&handle, &mut state, &eis_path)?;
    handle.insert_source(
        Generic::new(socket, Interest::READ, Mode::Level),
        |_, socket, state| {
            while let Some(stream) = socket.accept()? {
                let client = Arc::new(shell::ClientState::new(state));
                state.display_handle.insert_client(stream, client)?;
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
    if let Some(pid) = bus
        .as_ref()
        .and_then(waywire_portal::session_bus::SessionBus::pid)
    {
        wake_on_exit(&handle, pid)?;
    }
    if let Some(pid) = pipewire_daemon
        .as_ref()
        .and_then(waywire_pipewire::Daemon::pid)
    {
        wake_on_exit(&handle, pid)?;
    }
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
    if let Some(bus) = &bus {
        match crate::delivery::start(&handle, bus.address()) {
            Ok(outgoing) => state.delivery.set_outgoing(outgoing),
            Err(error) => tracing::error!(%error, "input delivery D-Bus service failed to start"),
        }
    }
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
        (Some(dirs), Some(bus)) => Some(waywire_portal::portal_environment(dirs, bus.address())),
        _ => None,
    };
    state.launch_environment = Some(LaunchEnvironment {
        socket: socket_path.clone(),
        runtime: runtime.path().to_path_buf(),
        eis: eis_path.clone(),
        portal: session_environment,
    });
    state.loop_handle = Some(handle.clone());
    if let Some(daemon) = &pipewire_daemon {
        state.screencast = start_screencast(&handle, daemon, Rc::clone(&image));
    }
    let mut tracker = OutputDamageTracker::from_output(&state.output);
    let mut next_frame = Instant::now();
    let mut next_housekeeping = Instant::now() + Duration::from_secs(2);
    tracing::info!(socket = %socket_path.display(), "Wayland compositor ready");
    while state.running {
        if child.0.is_none()
            && let Some(display) = state.xdisplay
            && let Some(program) = options.session.first()
        {
            let mut command = application_command(
                program,
                &options.session[1..],
                state.launch_environment.as_ref(),
                Some(display),
            )?;
            match command.spawn() {
                Ok(process) => {
                    if let Err(error) = wake_on_exit(&handle, process.id()) {
                        state.fail(error);
                    }
                    child.0 = Some(process);
                }
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
        if let Refresh::Due { at, .. } = state.refresh {
            deadline = deadline.min(at);
        }
        if let Some(at) = state.settle_wake() {
            let wait = at.saturating_sub(delivery::monotonic_usec());
            deadline = deadline.min(now + Duration::from_micros(wait));
        }
        if let Err(error) = event_loop.dispatch(deadline.saturating_duration_since(now), &mut state)
        {
            state.fail(error.into());
        }
        state.reap_pins();
        state.space.refresh();
        crate::xwayland::sync_stacking(&mut state);
        state.publish_window_list();
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
        state.poll_settled(delivery::monotonic_usec());
        if let Err(error) = state.refresh_video(&image.borrow()) {
            state.fail(error);
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
        if let Some(daemon) = pipewire_daemon.as_mut() {
            daemon.poll();
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
    // Streams first: their proxies talk to the daemon.
    state.screencast = None;
    drop(audio);
    drop(pipewire_daemon);
    drop(bus);
    state.release_input(KeyboardSource::MAIN);
    if let Some(video) = state.video.take() {
        video.stop();
    }
    event_writer.stop();
    state.failure.map_or(Ok(()), Err)
}

/// Connect to the private `PipeWire` daemon and dispatch its events on the
/// calloop loop. A consumer that frees a buffer wakes the loop, which retries
/// the latest rendered frame. Returns `None`, leaving `ScreenCast` metadata
/// only, when the connection fails.
fn start_screencast(
    handle: &calloop::LoopHandle<'static, State>,
    daemon: &waywire_pipewire::Daemon,
    image: Rc<RefCell<Image<'static, 'static>>>,
) -> Option<waywire_pipewire::Streams> {
    let streams = match waywire_pipewire::Streams::connect(daemon.socket()) {
        Ok(streams) => streams,
        Err(error) => {
            tracing::warn!(%error, "PipeWire connection failed; ScreenCast video disabled");
            return None;
        }
    };
    let fd = match streams.fd() {
        Ok(fd) => fd,
        Err(error) => {
            tracing::warn!(%error, "PipeWire loop descriptor; ScreenCast video disabled");
            return None;
        }
    };
    if let Err(error) = handle.insert_source(
        Generic::new(fd, Interest::READ, Mode::Level),
        move |_, _, state| {
            if let Some(streams) = &state.screencast {
                streams.dispatch();
                streams.offer_pending(&cast_frame(&image.borrow()));
            }
            Ok(PostAction::Continue)
        },
    ) {
        tracing::warn!(%error, "PipeWire event source; ScreenCast video disabled");
        return None;
    }
    Some(streams)
}

fn cast_frame<'a>(image: &'a Image<'static, 'static>) -> waywire_pipewire::Frame<'a> {
    waywire_pipewire::Frame {
        pixels: image_pixels(image),
        width: image.width(),
        height: image.height(),
        stride: image.stride(),
    }
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

/// Open a pidfd, which becomes readable when the process exits.
pub(crate) fn pidfd(pid: u32) -> std::io::Result<std::os::fd::OwnedFd> {
    // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor.
    let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = i32::try_from(fd).map_err(std::io::Error::other)?;
    // SAFETY: the successful syscall returned a new descriptor we now own.
    Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) })
}

fn application_command(
    program: impl AsRef<std::ffi::OsStr>,
    args: &[impl AsRef<std::ffi::OsStr>],
    environment: Option<&LaunchEnvironment>,
    display: Option<u32>,
) -> Result<std::process::Command> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID");
    if let Some(environment) = environment {
        command
            .env("WAYLAND_DISPLAY", &environment.socket)
            .env("XDG_RUNTIME_DIR", &environment.runtime)
            .env("LIBEI_SOCKET", &environment.eis)
            // PulseAudio clients then find the private daemon's socket in
            // XDG_RUNTIME_DIR, not a host server.
            .env_remove("PULSE_SERVER")
            .env_remove("PULSE_RUNTIME_PATH");
        if let Some(display) = display {
            command.env("DISPLAY", format!(":{display}"));
        } else {
            command.env_remove("DISPLAY");
        }
        if let Some(portal) = &environment.portal {
            for (key, value) in portal {
                command.env(key, value);
            }
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
    Ok(command)
}

/// Wake the event loop once `pid` exits so the checks after dispatch see it
/// immediately instead of at the next housekeeping deadline.
fn wake_on_exit(handle: &calloop::LoopHandle<'static, State>, pid: u32) -> Result<()> {
    handle
        .insert_source(
            Generic::new(pidfd(pid)?, Interest::READ, Mode::Level),
            |_, _, _| Ok(PostAction::Remove),
        )
        .map_err(|error| anyhow!("watch child exit: {error}"))?;
    Ok(())
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
    use std::collections::HashSet;

    use super::*;

    pub(super) fn test_state() -> (Display<State>, State) {
        let display = Display::new().expect("test display");
        let options = Config {
            ffmpeg: String::new(),
            encoder: EncoderChoice::Ffmpeg,
            stripes: 0,
            frame_rate: Fps::new(60).expect("test frame rate"),
            bitrate: Kbps::new(16000).expect("test bitrate"),
            rtp_port: 0,
            xkb_layout: "us".into(),
            resolution: FrameSize::new(1920, 1080).expect("test resolution"),
            pins: Vec::new(),
            session: Vec::new(),
        };
        let video = Video::Ffmpeg(VideoEncoder::idle_for_test().expect("idle encoder"));
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

    /// The keysyms the seat's active keymap gives XKB keycode `code`.
    fn keysyms(state: &mut State, code: u32) -> Vec<u32> {
        use smithay::input::keyboard::xkb;
        let keyboard = state.seat.get_keyboard().expect("seat keyboard");
        let text = keyboard.with_xkb_state(state, |context| {
            context.xkb().lock().expect("xkb lock").keymap_as_string()
        });
        let keymap = xkb::Keymap::new_from_string(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("active keymap");
        keymap
            .key_get_syms_by_level(code.into(), 0, 0)
            .iter()
            .map(|keysym| keysym.raw())
            .collect()
    }

    #[test]
    fn japanese_keys_on_spare_keycodes_do_not_retype_composed_text() {
        let (_display, mut state) = test_state();
        // Without Wayland focus, composed text uses the X11 spare keycodes:
        // the US layout gives Zenkaku/Hankaku (85), Ro (89) and Yen (124) no
        // symbols, so they are among them.
        let text: String = ('\u{3b1}'..='\u{3c9}').collect();
        state
            .apply_command(Command::Text(pipe::Text {
                action: pipe::TextAction::Commit,
                sequence: InputSequence::new(1).expect("sequence"),
                text: pipe::InputText::new(text).expect("text"),
            }))
            .expect("commit text");
        for key in [85, 89, 124] {
            let composed = keysyms(&mut state, key + 8);
            assert!(
                composed.len() == 1 && composed[0] >= 0x0100_03b1,
                "{key} holds no composed character: {composed:x?}"
            );
            state.keyboard_key(KeyboardSource::MAIN, key, KeyState::Pressed);
            assert_eq!(keysyms(&mut state, key + 8), Vec::<u32>::new());
            state.keyboard_key(KeyboardSource::MAIN, key, KeyState::Released);
        }
    }

    #[test]
    fn control_text_follows_gateway_lease() {
        let (_display, mut state) = test_state();
        assert_eq!(state.control_text(), "Agent driving");
        state.dirty = false;
        lease(&mut state, true);
        assert_eq!(state.control_text(), "You're driving");
        assert!(state.dirty);
        state.dirty = false;
        lease(&mut state, true);
        assert!(!state.dirty);
        lease(&mut state, false);
        assert_eq!(state.control_text(), "Agent driving");
    }

    #[test]
    fn clock_updates_only_when_the_displayed_minute_changes() {
        let (_display, mut state) = test_state();
        let time = UNIX_EPOCH + Duration::from_mins(28_333_334);
        state.update_clock(time).expect("local time");
        assert_eq!(state.clock, local_clock(time).expect("local time"));
        state.dirty = false;
        state
            .update_clock(time + Duration::from_secs(30))
            .expect("local time");
        assert!(!state.dirty);
        state
            .update_clock(time + Duration::from_mins(1))
            .expect("local time");
        assert!(state.dirty);
        assert_ne!(state.clock, local_clock(time).expect("local time"));
    }

    #[test]
    fn clock_timer_aligns_to_next_minute() {
        assert_eq!(next_minute_delay(UNIX_EPOCH), Duration::from_mins(1));
        assert_eq!(
            next_minute_delay(UNIX_EPOCH + Duration::from_millis(75_250)),
            Duration::from_millis(44_750)
        );
    }

    #[test]
    fn pointer_events_report_their_input_source() {
        let (_display, mut state) = test_state();
        let eis = KeyboardSource::new_auxiliary();
        let virtual_pointer = KeyboardSource::new_auxiliary();
        for (source, expected) in [
            (KeyboardSource::MAIN, pipe::CursorSource::Browser),
            (eis, pipe::CursorSource::Agent),
            (virtual_pointer, pipe::CursorSource::Agent),
        ] {
            state.event_sink.take_events();
            state.pointer_motion(source, (100.0, 100.0).into());
            let positions: Vec<_> = state
                .event_sink
                .take_events()
                .into_iter()
                .filter_map(|event| {
                    if let Event::CursorPosition(position) = event {
                        Some(position.source)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(positions, vec![expected]);
        }
        state.pointer_button(virtual_pointer, 0x110, ButtonState::Pressed);
        assert!(state.event_sink.take_events().iter().any(|event| matches!(event, Event::CursorClick(position) if position.source == pipe::CursorSource::Agent)));
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
                .input
                .buttons(agent)
                .is_some_and(|buttons| buttons.contains(&0x110)),
            "the agent's button must reach the seat"
        );
        assert_eq!(
            state.input.agent_input(),
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
                .input
                .buttons(source)
                .is_some_and(|buttons| buttons.contains(&0x110))
        );
        state.disconnect_input_source(source);
        assert!(state.input.buttons(source).is_none_or(HashSet::is_empty));
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
            state.input.buttons(agent).is_none_or(HashSet::is_empty),
            "a button held across the takeover must be released"
        );

        // Later agent events are dropped rather than queued or forwarded.
        state.event_sink.take_events();
        state.keyboard_key(agent, 31, KeyState::Pressed);
        state.pointer_button(agent, 0x111, ButtonState::Pressed);
        state.pointer_motion(agent, (12.0, 34.0).into());
        assert!(!pressed(&state, 31));
        assert!(state.input.buttons(agent).is_none_or(HashSet::is_empty));
        assert!(
            state.event_sink.take_events().is_empty(),
            "dropped agent motion must not move the cursor or emit an event"
        );
        assert_eq!(
            state.input.agent_input(),
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
        assert!(state.input.buttons(agent).is_none_or(HashSet::is_empty));
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
                .input
                .buttons(agent)
                .is_some_and(|buttons| buttons.contains(&0x111)),
            "agent buttons resume after handback"
        );
        assert_eq!(
            state.input.agent_input(),
            pipe::AgentInput {
                connected: true,
                dropped: false,
            }
        );
    }

    fn resize(state: &mut State, width: u32, height: u32, scale_v120: u16) {
        state
            .apply_command(Command::Resize(pipe::Resize {
                size: FrameSize::new(width, height).expect("test size"),
                scale_v120: pipe::ScaleV120::new(scale_v120).expect("test scale"),
                request_id: pipe::RequestId::new(1).expect("test request"),
            }))
            .expect("resize command");
    }

    fn browser_absolute(state: &mut State, x: u32, y: u32) {
        state
            .apply_command(Command::PointerAbsolute(pipe::PointerAbsolute {
                x: pipe::PointerCoordinate::new(x).expect("x"),
                y: pipe::PointerCoordinate::new(y).expect("y"),
                sequence: InputSequence::new(1).expect("sequence"),
            }))
            .expect("pointer command");
    }

    /// The normalized positions reported to the viewer since the last call.
    fn reported_positions(state: &State) -> Vec<(u32, u32)> {
        state
            .event_sink
            .take_events()
            .into_iter()
            .filter_map(|event| {
                if let Event::CursorPosition(position) = event {
                    Some((position.x.get(), position.y.get()))
                } else {
                    None
                }
            })
            .collect()
    }

    fn assert_location(state: &State, expected: (f64, f64)) {
        let location = state
            .seat
            .get_pointer()
            .expect("pointer")
            .current_location();
        assert!(
            (location.x - expected.0).abs() < 1e-9 && (location.y - expected.1).abs() < 1e-9,
            "pointer at {location:?}, expected {expected:?}"
        );
    }

    #[test]
    fn browser_pointer_maps_onto_the_logical_output_at_fractional_scales() {
        let (_display, mut state) = test_state();
        // 21845 is a third of 65535: pixel 640 of 1920 and row 360 of 1080,
        // which each scale divides into logical coordinates.
        for (scale_v120, expected) in [
            (120, (640.0, 360.0)),
            (150, (512.0, 288.0)),
            (180, (640.0 / 1.5, 240.0)),
            (480, (160.0, 90.0)),
        ] {
            resize(&mut state, 1920, 1080, scale_v120);
            state.event_sink.take_events();
            browser_absolute(&mut state, 21845, 21845);
            assert_location(&state, expected);
            assert_eq!(
                reported_positions(&state),
                [(21845, 21845)],
                "the viewer's cursor returns to the point it sent at scale {scale_v120}"
            );
        }
    }

    #[test]
    fn browser_pointer_at_the_output_edges_stays_on_the_outermost_pixels() {
        let (_display, mut state) = test_state();
        for (scale_v120, scale) in [(120, 1.0), (150, 1.25), (180, 1.5)] {
            resize(&mut state, 1920, 1080, scale_v120);
            browser_absolute(&mut state, 0, 0);
            assert_location(&state, (0.0, 0.0));
            state.event_sink.take_events();
            // The far edge is pixel 1919 and row 1079, not one past them:
            // 1919 / 1920 * 65535 rounds to 65501 and 1079 / 1080 * 65535 to
            // 65474 at every scale.
            browser_absolute(&mut state, 65535, 65535);
            assert_location(&state, (1919.0 / scale, 1079.0 / scale));
            assert_eq!(reported_positions(&state), [(65501, 65474)]);
        }
        // Shrinking the output pulls a pointer on the old edge onto the new one.
        resize(&mut state, 800, 600, 120);
        assert_location(&state, (799.0, 599.0));
    }

    #[test]
    fn browser_relative_motion_divides_pixels_by_the_scale_and_stops_at_the_edge() {
        let (_display, mut state) = test_state();
        resize(&mut state, 1920, 1080, 180);
        browser_absolute(&mut state, 21845, 21845);
        let relative = |state: &mut State, dx: f32, dy: f32| {
            state
                .apply_command(Command::PointerRelative(pipe::PointerRelative {
                    dx: pipe::PointerDelta::new(dx).expect("dx"),
                    dy: pipe::PointerDelta::new(dy).expect("dy"),
                    sequence: InputSequence::new(2).expect("sequence"),
                }))
                .expect("relative command");
        };
        // Three pixels right and six up at 1.5 are two and four logical units.
        relative(&mut state, 3.0, -6.0);
        assert_location(&state, (640.0 / 1.5 + 2.0, 236.0));
        relative(&mut state, 4000.0, -4000.0);
        assert_location(&state, (1919.0 / 1.5, 0.0));
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
