//! Headless Wayland compositor streaming H.264 RTP.

mod capture;
mod clipboard;
mod cursor;
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
mod text_keyboard;
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
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::SERIAL_COUNTER;
use smithay::utils::Transform;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::compositor::with_states;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::shell::xdg::dialog::ToplevelDialogHint;
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
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
    pub pins: Vec<Vec<String>>,
    /// Application argv. When empty, `WAYWIRE_SESSION` names one executable.
    pub session: Vec<OsString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameRequest {
    Idle,
    Pending,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PointerMotionKind {
    Input,
    RefreshGrabFocus,
    WarpHint,
}

struct TitlebarPress {
    source: KeyboardSource,
    window: Window,
    location: Point<f64, Logical>,
    time: Instant,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum WindowAction {
    Focus,
    Move,
    Resize(xdg_toplevel::ResizeEdge),
    Minimize,
    Maximize,
    Restore,
    Close,
    ShowMenu,
    ToggleAlwaysOnTop,
}

#[derive(Clone, Debug, PartialEq)]
struct TaskbarChip {
    window: Option<Window>,
    pin: Option<usize>,
    running: bool,
    title: String,
    minimized: bool,
    focused: bool,
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
    pub(crate) popups: PopupManager,
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
    pub(crate) toplevel_list: toplevel_list::ToplevelList,
    clipboard: clipboard::Clipboard,
    windows: HashMap<Window, grabs::WindowState>,
    pins: Vec<Pin>,
    launch_environment: Option<LaunchEnvironment>,
    loop_handle: Option<calloop::LoopHandle<'static, State>>,
    mapped_order: Vec<Window>,
    minimized_order: Vec<Window>,
    restore_key_source: Option<KeyboardSource>,
    decoration_press: Option<(KeyboardSource, Window)>,
    window_menu: Option<decorations::WindowMenu>,
    taskbar: decorations::Taskbar,
    clock: String,
    menu_button: Option<(KeyboardSource, u32)>,
    menu_escape: Option<KeyboardSource>,
    last_titlebar_press: Option<TitlebarPress>,
    pending_restore_drag: Option<TitlebarPress>,
    pointer_motion_kind: PointerMotionKind,
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
        video: VideoEncoder,
    ) -> Result<Self> {
        let compositor_state = CompositorState::new_v6::<Self>(&display_handle);
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
            surfaces: Vec::new(),
            cursor_surface: None,
            cursor_image: None,
            shm_state,
            xdg_shell_state,
            seat_state,
            data_device_state,
            popups: PopupManager::default(),
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
            toplevel_list,
            clipboard: clipboard::Clipboard::default(),
            windows: HashMap::new(),
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
            mapped_order: Vec::new(),
            minimized_order: Vec::new(),
            restore_key_source: None,
            decoration_press: None,
            window_menu: None,
            taskbar: decorations::Taskbar::default(),
            clock: local_clock(SystemTime::now())?,
            menu_button: None,
            menu_escape: None,
            last_titlebar_press: None,
            pending_restore_drag: None,
            pointer_motion_kind: PointerMotionKind::Input,
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
        if self.window_menu.is_some() || self.decoration_under(pos).is_some() {
            return None;
        }
        self.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, offset)| (surface, (offset + location).to_f64()))
        })
    }

    fn element_under(&self, pos: Point<f64, Logical>) -> Option<(&Window, Point<i32, Logical>)> {
        let over_bar = self
            .work_area()
            .is_some_and(|area| pos.y >= f64::from(area.loc.y + area.size.h));
        for window in self.space.elements().rev() {
            if self
                .windows
                .get(window)
                .is_some_and(|state| state.minimized)
            {
                continue;
            }
            if over_bar && !decorations::above_taskbar(window) {
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
            .mapped_order
            .iter()
            .filter(|window| decorations::taskbar_window(window))
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

    fn control_text(&self) -> &'static str {
        if self.browser_holds_input {
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
    pub(crate) fn set_browser_lease(&mut self, held: bool) {
        if self.browser_holds_input == held {
            return;
        }
        self.browser_holds_input = held;
        self.dirty = true;
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
        if key == 1 && state == KeyState::Pressed && self.window_menu.take().is_some() {
            self.menu_escape = Some(source);
            self.dirty = true;
            self.refresh_pointer_focus();
            return;
        }
        if key == 1 && state == KeyState::Released && self.menu_escape == Some(source) {
            self.menu_escape = None;
            return;
        }
        if key == 50 && state == KeyState::Released && self.restore_key_source == Some(source) {
            self.restore_key_source = None;
            return;
        }
        if key == 50
            && state == KeyState::Pressed
            && self.seat.get_keyboard().is_some_and(|keyboard| {
                let mods = keyboard.modifier_state();
                mods.logo && mods.shift && !mods.ctrl && !mods.alt
            })
        {
            self.restore_key_source = Some(source);
            if let Some(window) = self.minimized_order.last().cloned() {
                self.window_action(&window, WindowAction::Restore);
            }
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
            if state == KeyState::Released && keyboard.pressed_keys().is_empty() {
                self.flush_text_keyboard();
            }
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

    pub(crate) fn refresh_pointer_focus(&mut self) {
        if let Some(pointer) = self.seat.get_pointer() {
            let window_grab = pointer
                .with_grab(|_, grab| grab.is::<grabs::WindowGrab>())
                .unwrap_or(false);
            if pointer.is_grabbed() && !window_grab {
                return;
            }
            if window_grab {
                self.pointer_motion_kind = PointerMotionKind::RefreshGrabFocus;
            }
            self.pointer_motion(KeyboardSource::MAIN, pointer.current_location());
            self.pointer_motion_kind = PointerMotionKind::Input;
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
            if self.pointer_motion_kind == PointerMotionKind::Input {
                (location, locked) = self.constrain_pointer_motion(&pointer, location);
            }
            if let Some(menu) = self.window_menu.as_mut() {
                self.dirty |= menu.motion(location);
            }
            if !locked {
                pointer.motion(
                    self,
                    self.surface_under(location),
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
                    self.surface_under(location),
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
        if self.pointer_motion_kind == PointerMotionKind::Input {
            self.update_titlebar_drag(source, location);
        }
        if self.decoration_press.as_ref().is_some_and(|(_, window)| {
            self.decoration_under(location)
                != Some((window.clone(), decorations::DecorationAction::Close))
        }) {
            self.decoration_press = None;
        }
        if let Some(shape) = self.decoration_cursor_under(location) {
            self.emit(Event::CursorShape(shape));
        } else if self.surface_under(location).is_none() {
            self.emit(Event::CursorShape(pipe::CursorShape::Default));
        }
        self.emit(Event::CursorPosition(pipe::CursorPosition {
            x: normalized_coordinate(location.x * scale, self.size.width()),
            y: normalized_coordinate(location.y * scale, self.size.height()),
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
                let fraction = ((press.location.x - f64::from(maximized.loc.x))
                    / f64::from(maximized.size.w.max(1)))
                .clamp(0.0, 1.0);
                let position = Point::<f64, Logical>::from((
                    location.x - fraction * f64::from(restored_size.w),
                    location.y + f64::from(maximized.loc.y) - press.location.y,
                ))
                .to_i32_round();
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
                    .buttons
                    .get(&source)
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

    #[expect(
        clippy::too_many_lines,
        reason = "pointer dispatch keeps button ownership together"
    )]
    fn pointer_button_inner(&mut self, source: KeyboardSource, button: u32, state: ButtonState) {
        if button == 0x110
            && state == ButtonState::Released
            && self
                .pending_restore_drag
                .as_ref()
                .is_some_and(|press| press.source == source)
        {
            self.pending_restore_drag = None;
        }
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
        if state == ButtonState::Released && self.menu_button == Some((source, button)) {
            self.menu_button = None;
            return;
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
                        return;
                    }
                    self.window_action(&window, action);
                }
                self.menu_button = Some((source, button));
                self.refresh_pointer_focus();
            }
            return;
        }
        if state == ButtonState::Pressed && button == 0x110 && !pointer.is_grabbed() {
            let position = pointer.current_location();
            let covered = self
                .element_under(position)
                .is_some_and(|(window, _)| decorations::above_taskbar(window));
            if !covered && let Some(output) = self.space.output_geometry(&self.output) {
                let chips = self.taskbar_chips();
                let relative = position
                    - (output.loc + Point::from((0, output.size.h - decorations::TASKBAR)))
                        .to_f64();
                if let Some(chip) = chips.iter().enumerate().find_map(|(index, chip)| {
                    let rect = decorations::chip_rectangle(output.size.w, chips.len(), index)?;
                    rect.to_f64().contains(relative).then_some(chip)
                }) {
                    let action = if chip.minimized {
                        WindowAction::Restore
                    } else if chip.focused {
                        WindowAction::Minimize
                    } else {
                        WindowAction::Focus
                    };
                    if let Some(window) = &chip.window {
                        self.window_action(window, action);
                    } else if let Some(pin) = chip.pin {
                        self.launch_pin(pin);
                    }
                    self.menu_button = Some((source, button));
                    return;
                }
            }
        }
        if state == ButtonState::Pressed
            && button == 0x111
            && !pointer.is_grabbed()
            && let Some((window, decorations::DecorationAction::Move)) =
                self.decoration_under(pointer.current_location())
        {
            self.window_action(&window, WindowAction::ShowMenu);
            self.menu_button = Some((source, button));
            return;
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
            self.press_decoration(source, window, action);
            pointer.frame(self);
            return;
        }
        if state == ButtonState::Pressed
            && !pointer.is_grabbed()
            && let Some(window) = self
                .element_under(pointer.current_location())
                .map(|(window, _)| window.clone())
        {
            self.focus_window(Some(window));
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
        if state == ButtonState::Released {
            self.refresh_pointer_focus();
        }
        if state == ButtonState::Released
            && button == 0x110
            && let Some((owner, window)) = self.decoration_press.take()
            && owner == source
            && self.decoration_under(pointer.current_location())
                == Some((window.clone(), decorations::DecorationAction::Close))
        {
            self.window_action(&window, WindowAction::Close);
        }
    }

    fn press_decoration(
        &mut self,
        source: KeyboardSource,
        window: Window,
        action: decorations::DecorationAction,
    ) {
        match action {
            decorations::DecorationAction::Move => {
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
            decorations::DecorationAction::Resize(edges) => {
                self.window_action(&window, WindowAction::Resize(edges));
            }
            decorations::DecorationAction::Minimize => {
                self.window_action(&window, WindowAction::Minimize);
            }
            decorations::DecorationAction::Maximize => {
                self.window_action(&window, WindowAction::Maximize);
            }
            decorations::DecorationAction::Restore => {
                self.window_action(&window, WindowAction::Restore);
            }
            decorations::DecorationAction::Close => self.decoration_press = Some((source, window)),
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
                let state = self.windows.entry(window.clone()).or_default();
                if !state.minimized {
                    state.minimized = true;
                    self.minimized_order.push(window.clone());
                    let focused = self
                        .seat
                        .get_keyboard()
                        .and_then(|keyboard| keyboard.current_focus());
                    if focused == focus::KeyboardFocus::window(window) {
                        self.focus_window(self.topmost_focusable(Some(window)));
                    }
                    self.refresh_toplevel_states();
                    self.refresh_pointer_focus();
                    self.dirty = true;
                }
            }
            WindowAction::Restore => {
                if self
                    .windows
                    .get(window)
                    .is_some_and(|state| state.minimized)
                {
                    if let Some(state) = self.windows.get_mut(window) {
                        state.minimized = false;
                    }
                    self.minimized_order.retain(|item| item != window);
                    self.focus_window(Some(window.clone()));
                } else {
                    self.set_window_expanded(window, xdg_toplevel::State::Maximized, false);
                }
            }
            WindowAction::Close => Self::close_window(window),
            WindowAction::ToggleAlwaysOnTop => {
                let state = self.windows.entry(window.clone()).or_default();
                state.always_on_top = !state.always_on_top;
                self.enforce_topmost();
                self.dirty = true;
            }
            WindowAction::ShowMenu => {
                if let (Some(pointer), Some(output)) = (
                    self.seat.get_pointer(),
                    self.space.output_geometry(&self.output),
                ) {
                    self.window_menu = Some(decorations::WindowMenu::new(
                        window.clone(),
                        pointer.current_location(),
                        output.size,
                        self.windows
                            .get(window)
                            .is_some_and(|state| state.always_on_top),
                    ));
                    self.refresh_pointer_focus();
                    self.dirty = true;
                }
            }
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
        if self
            .windows
            .get(window)
            .is_some_and(|state| state.minimized)
        {
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
            .refresh_states(focused.as_ref(), &self.minimized_order);
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
        let buttons = self.buttons.get(&source).cloned().unwrap_or_default();
        for button in buttons {
            self.pointer_button_inner(source, button, ButtonState::Released);
        }
        self.buttons.remove(&source);
    }

    /// The topmost mapped window that can take keyboard focus, skipping `except`.
    pub(crate) fn topmost_focusable(&self, except: Option<&Window>) -> Option<Window> {
        self.space
            .elements()
            .rev()
            .find(|window| {
                Some(*window) != except
                    && !is_override_redirect(window)
                    && self
                        .windows
                        .get(*window)
                        .is_some_and(|state| state.mapped && !state.minimized)
            })
            .cloned()
    }

    fn parent_window(&self, window: &Window) -> Option<Window> {
        if let Some(parent) = window
            .toplevel()
            .and_then(smithay::wayland::shell::xdg::ToplevelSurface::parent)
        {
            return self
                .space
                .elements()
                .find(|candidate| {
                    candidate
                        .toplevel()
                        .is_some_and(|top| top.wl_surface() == &parent)
                })
                .cloned();
        }
        let parent = window.x11_surface()?.is_transient_for()?;
        self.space
            .elements()
            .find(|candidate| {
                candidate
                    .x11_surface()
                    .is_some_and(|surface| surface.window_id() == parent)
            })
            .cloned()
    }

    fn modal_dialog(window: &Window) -> bool {
        if let Some(top) = window.toplevel() {
            return with_states(top.wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .and_then(|data| data.lock().ok())
                    .is_none_or(|data| data.dialog_hint != ToplevelDialogHint::Dialog)
            });
        }
        window
            .x11_surface()
            .is_some_and(smithay::xwayland::X11Surface::is_modal)
    }

    fn dialog_children(&self, parent: &Window) -> Vec<Window> {
        self.space
            .elements()
            .filter(|candidate| {
                self.windows
                    .get(*candidate)
                    .is_some_and(|state| state.mapped && !state.minimized)
                    && self.parent_window(candidate).as_ref() == Some(parent)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn enforce_topmost(&mut self) {
        let mut topmost: Vec<_> = self
            .space
            .elements()
            .filter(|window| {
                let mut current = Some((*window).clone());
                let mut visited = Vec::new();
                while let Some(target) = current {
                    if visited.contains(&target) {
                        break;
                    }
                    if self
                        .windows
                        .get(&target)
                        .is_some_and(|state| state.always_on_top)
                    {
                        return true;
                    }
                    current = self.parent_window(&target);
                    visited.push(target);
                }
                false
            })
            .cloned()
            .collect();
        topmost.sort_by_key(|window| {
            let mut depth = 0;
            let mut current = self.parent_window(window);
            let mut visited = vec![window.clone()];
            while let Some(parent) = current {
                if visited.contains(&parent) {
                    break;
                }
                depth += 1;
                current = self.parent_window(&parent);
                visited.push(parent);
            }
            depth
        });
        for window in topmost {
            self.space.raise_element(&window, false);
        }
    }

    pub(crate) fn center_dialog(&mut self, window: &Window) {
        let Some(parent) = self.parent_window(window) else {
            return;
        };
        let Some(parent_location) = self.space.element_location(&parent) else {
            return;
        };
        let parent_geometry = parent.geometry();
        let child_geometry = window.geometry();
        let mut location = parent_location
            + Point::from((
                (parent_geometry.size.w - child_geometry.size.w) / 2,
                (parent_geometry.size.h - child_geometry.size.h) / 2,
            ));
        if let Some(area) = self.window_output_geometry(window) {
            location.x = location.x.clamp(
                area.loc.x,
                area.loc.x + (area.size.w - child_geometry.size.w).max(0),
            );
            location.y = location.y.clamp(
                area.loc.y,
                area.loc.y + (area.size.h - child_geometry.size.h).max(0),
            );
        }
        self.space.map_element(window.clone(), location, false);
        if let Some(surface) = window.x11_surface() {
            let mut geometry = surface.last_configure();
            geometry.loc = location - child_geometry.loc;
            if let Err(error) = surface.configure(geometry) {
                tracing::warn!(%error, "center X11 dialog");
            }
        }
    }

    pub(crate) fn focus_window(&mut self, window: Option<Window>) {
        if window.as_ref().is_some_and(|window| {
            self.windows
                .get(window)
                .is_some_and(|state| state.minimized)
        }) {
            return;
        }
        let mut window = window;
        if let Some(target) = &window {
            let mut family = vec![target.clone()];
            let mut index = 0;
            while index < family.len() {
                for child in self.dialog_children(&family[index]) {
                    if !family.contains(&child) {
                        family.push(child);
                    }
                }
                index += 1;
            }
            for member in &family {
                self.space.raise_element(member, true);
                if let (Some(xwm), Some(surface)) = (&mut self.xwm, member.x11_surface())
                    && let Err(error) = xwm.raise_window(surface)
                {
                    tracing::warn!(%error, "raise X11 window");
                }
            }
            self.enforce_topmost();
            let mut visited = Vec::new();
            while let Some(target) = &window {
                if visited.contains(target) {
                    break;
                }
                visited.push(target.clone());
                let next = self
                    .dialog_children(target)
                    .into_iter()
                    .rev()
                    .find(Self::modal_dialog);
                if next.is_none() {
                    break;
                }
                window = next;
            }
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
            if is_override_redirect(window) {
                self.refresh_pointer_focus();
                self.dirty = true;
                return;
            }
        }
        self.refresh_pointer_focus();
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
                            None
                        }
                    };
                }
            }
            Command::Text(p) => {
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
        self.minimized_order.retain(smithay::utils::IsAlive::alive);
        self.toplevel_list.cleanup();
        self.popups.cleanup();
        if self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .is_some_and(|focus| !smithay::utils::IsAlive::alive(&focus))
        {
            self.focus_window(self.topmost_focusable(None));
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
    if let Some(pid) = bus.as_ref().and_then(session_bus::SessionBus::pid) {
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
    state.launch_environment = Some(LaunchEnvironment {
        socket: socket_path.clone(),
        runtime: runtime.path().to_path_buf(),
        eis: eis_path.clone(),
        portal: session_environment,
    });
    state.loop_handle = Some(handle.clone());
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
        if let Err(error) = event_loop.dispatch(deadline.saturating_duration_since(now), &mut state)
        {
            state.fail(error.into());
        }
        state.reap_pins();
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

fn is_override_redirect(window: &Window) -> bool {
    window
        .x11_surface()
        .is_some_and(smithay::xwayland::X11Surface::is_override_redirect)
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
        .env_remove("WAYLAND_SOCKET");
    if let Some(environment) = environment {
        command
            .env("WAYLAND_DISPLAY", &environment.socket)
            .env("XDG_RUNTIME_DIR", &environment.runtime)
            .env("LIBEI_SOCKET", &environment.eis);
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
            pins: Vec::new(),
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
