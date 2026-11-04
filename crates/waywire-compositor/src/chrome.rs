//! Desktop chrome: the taskbar, its pinned applications and clock, the
//! window menu, and the title bar presses that become window actions.
//!
//! `Chrome` keeps this state and never touches the seat or the space. It
//! reads window state from [`Windows`] where it needs it, and returns the
//! window actions and redraws for the caller to apply.
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use anyhow::anyhow;
use smithay::desktop::Window;
use smithay::input::keyboard::KeyboardSource;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::Rectangle;
use smithay::utils::Size;
use waywire_decorations::Attention;
use waywire_decorations::ChipTarget;
use waywire_decorations::DecorationAction;
use waywire_decorations::PinProcess;
use waywire_decorations::TaskbarChip;
use waywire_decorations::Visibility;
use waywire_decorations::WindowAction;

use crate::State;
use crate::delivery;
use crate::focus;
use crate::toplevel_list;
use crate::wm::Windows;

/// A title bar press that a drag or a second press can still turn into an
/// action.
pub(crate) struct TitlebarPress {
    source: KeyboardSource,
    pub(crate) window: Window,
    pub(crate) location: Point<f64, Logical>,
    time: Instant,
}

impl TitlebarPress {
    fn moved(&self, source: KeyboardSource, location: Point<f64, Logical>) -> bool {
        self.source == source
            && (location - self.location)
                .x
                .hypot((location - self.location).y)
                > 4.0
    }
}

pub(crate) struct Pin {
    pub(crate) command: Vec<String>,
    pub(crate) pid: Option<u32>,
    pub(crate) child: Option<Child>,
}

impl Drop for Pin {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            drop(child.kill());
            drop(child.wait());
        }
    }
}

/// What applications launched on the desktop need to find it.
pub(crate) struct LaunchEnvironment {
    pub(crate) socket: std::path::PathBuf,
    pub(crate) runtime: std::path::PathBuf,
    pub(crate) eis: std::path::PathBuf,
    pub(crate) portal: Option<Vec<(String, String)>>,
}

/// A press while the window menu is open.
pub(crate) struct MenuPress {
    pub(crate) window: Window,
    /// The press closed the menu.
    pub(crate) closed: bool,
    /// The item a left press chose.
    pub(crate) action: Option<WindowAction>,
}

pub(crate) struct Chrome {
    taskbar: waywire_decorations::Taskbar,
    window_menu: Option<waywire_decorations::WindowMenu>,
    /// A press on a Close button, which closes the window if it is released
    /// there.
    decoration_press: Option<(KeyboardSource, Window)>,
    /// A press the compositor consumed, so its release is consumed too.
    menu_button: Option<(KeyboardSource, u32, delivery::Chrome)>,
    /// The source whose Escape closed the window menu.
    menu_escape: Option<KeyboardSource>,
    /// The source whose shortcut restored a minimized window.
    restore_key_source: Option<KeyboardSource>,
    last_titlebar_press: Option<TitlebarPress>,
    /// A press on a maximized window's title bar, which restores the window
    /// once the pointer drags it.
    pending_restore_drag: Option<TitlebarPress>,
    clock: String,
    pins: Vec<Pin>,
    launch_environment: Option<LaunchEnvironment>,
}

impl Chrome {
    pub(crate) fn new(pins: &[Vec<String>]) -> Result<Self> {
        Ok(Self {
            taskbar: waywire_decorations::Taskbar::default(),
            window_menu: None,
            decoration_press: None,
            menu_button: None,
            menu_escape: None,
            restore_key_source: None,
            last_titlebar_press: None,
            pending_restore_drag: None,
            clock: local_clock(SystemTime::now())?,
            pins: pins
                .iter()
                .map(|command| Pin {
                    command: command.clone(),
                    pid: None,
                    child: None,
                })
                .collect(),
            launch_environment: None,
        })
    }

    /// Forget the presses `source` holds, without acting on them: a source
    /// that disconnects or loses input does not click Close.
    pub(crate) fn release(&mut self, source: KeyboardSource) {
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
    }

    /// Forget chrome that belongs to an unmapped window. Returns whether the
    /// window menu closed, which needs a redraw.
    pub(crate) fn window_unmapped(&mut self, window: &Window) -> bool {
        if self
            .decoration_press
            .as_ref()
            .is_some_and(|(_, target)| target == window)
        {
            self.decoration_press = None;
        }
        if self
            .window_menu
            .as_ref()
            .is_some_and(|menu| menu.window() == window)
        {
            self.window_menu = None;
            return true;
        }
        false
    }

    pub(crate) fn window_menu(&self) -> Option<&waywire_decorations::WindowMenu> {
        self.window_menu.as_ref()
    }

    pub(crate) fn window_menu_mut(&mut self) -> Option<&mut waywire_decorations::WindowMenu> {
        self.window_menu.as_mut()
    }

    pub(crate) fn open_menu(
        &mut self,
        window: Window,
        pointer: Point<f64, Logical>,
        output: Size<i32, Logical>,
        always_on_top: bool,
    ) {
        self.window_menu = Some(waywire_decorations::WindowMenu::new(
            window,
            pointer,
            output,
            if always_on_top {
                waywire_decorations::Stacking::AlwaysOnTop
            } else {
                waywire_decorations::Stacking::Normal
            },
        ));
    }

    #[cfg(test)]
    pub(crate) fn close_menu(&mut self) {
        self.window_menu = None;
    }

    /// Track the pointer over the open menu. Returns whether it needs a redraw.
    pub(crate) fn menu_motion(&mut self, location: Point<f64, Logical>) -> bool {
        self.window_menu
            .as_mut()
            .is_some_and(|menu| menu.motion(location))
    }

    /// A button press while the window menu is open. A press outside the
    /// menu closes it, and a left press on an item chooses that item.
    /// `None` when no menu is open.
    pub(crate) fn press_menu(
        &mut self,
        position: Point<f64, Logical>,
        button: u32,
    ) -> Option<MenuPress> {
        let menu = self.window_menu.as_ref()?;
        let selected = menu.click(position);
        let outside = !menu.contains(position);
        let window = menu.window().clone();
        let closed = outside || (button == 0x110 && selected.is_some());
        if closed {
            self.window_menu = None;
        }
        Some(MenuPress {
            window,
            closed,
            action: selected.filter(|_| button == 0x110),
        })
    }

    /// Escape closes the window menu. Returns the menu's window when it was
    /// open; the matching release is then consumed too.
    pub(crate) fn escape_menu(&mut self, source: KeyboardSource) -> Option<Window> {
        let menu = self.window_menu.take()?;
        self.menu_escape = Some(source);
        Some(menu.window().clone())
    }

    /// Whether this Escape release belongs to a press that closed the menu.
    pub(crate) fn release_escape(&mut self, source: KeyboardSource) -> bool {
        if self.menu_escape != Some(source) {
            return false;
        }
        self.menu_escape = None;
        true
    }

    #[cfg(test)]
    pub(crate) fn menu_escape(&self) -> Option<KeyboardSource> {
        self.menu_escape
    }

    /// Record the restore shortcut's press, so its release is consumed too.
    pub(crate) fn press_restore_key(&mut self, source: KeyboardSource) {
        self.restore_key_source = Some(source);
    }

    /// Whether this release belongs to a restore shortcut press.
    pub(crate) fn release_restore_key(&mut self, source: KeyboardSource) -> bool {
        if self.restore_key_source != Some(source) {
            return false;
        }
        self.restore_key_source = None;
        true
    }

    /// Consume `button`'s release from `source`, as its press was.
    pub(crate) fn consume_button(
        &mut self,
        source: KeyboardSource,
        button: u32,
        chrome: delivery::Chrome,
    ) {
        self.menu_button = Some((source, button, chrome));
    }

    /// The chrome a consumed press went to, when this is its release.
    pub(crate) fn consumed_release(
        &mut self,
        source: KeyboardSource,
        button: u32,
    ) -> Option<delivery::Chrome> {
        let (owner, held, chrome) = self.menu_button?;
        if owner != source || held != button {
            return None;
        }
        self.menu_button = None;
        Some(chrome)
    }

    /// The Close button press waiting for its release.
    pub(crate) fn armed_close(&self) -> Option<&(KeyboardSource, Window)> {
        self.decoration_press.as_ref()
    }

    pub(crate) fn take_armed_close(&mut self) -> Option<(KeyboardSource, Window)> {
        self.decoration_press.take()
    }

    pub(crate) fn disarm_close(&mut self) {
        self.decoration_press = None;
    }

    /// A left press on a window's decoration at `pointer`. Returns the window
    /// action to start now. A Close press only arms Close for its release,
    /// and a press on a maximized title bar waits to see if it drags.
    pub(crate) fn press_decoration(
        &mut self,
        source: KeyboardSource,
        window: Window,
        action: DecorationAction,
        pointer: Point<f64, Logical>,
    ) -> Option<WindowAction> {
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
                    return Some(if maximized {
                        WindowAction::Restore
                    } else {
                        WindowAction::Maximize
                    });
                }
                self.last_titlebar_press = Some(TitlebarPress {
                    source,
                    window: window.clone(),
                    location: pointer,
                    time: now,
                });
                if maximized {
                    self.pending_restore_drag = Some(TitlebarPress {
                        source,
                        window,
                        location: pointer,
                        time: now,
                    });
                    None
                } else {
                    Some(WindowAction::Move)
                }
            }
            DecorationAction::Resize(edges) => Some(WindowAction::Resize(edges)),
            DecorationAction::Minimize => Some(WindowAction::Minimize),
            DecorationAction::Maximize => Some(WindowAction::Maximize),
            DecorationAction::Restore => Some(WindowAction::Restore),
            DecorationAction::Close => {
                self.decoration_press = Some((source, window));
                None
            }
        }
    }

    /// A left button release from `source` ends its wait for a restoring drag.
    pub(crate) fn cancel_restore_drag(&mut self, source: KeyboardSource) {
        if self
            .pending_restore_drag
            .as_ref()
            .is_some_and(|press| press.source == source)
        {
            self.pending_restore_drag = None;
        }
    }

    /// The pointer moved. Returns the press on a maximized title bar that
    /// this motion drags far enough to restore the window.
    pub(crate) fn start_restore_drag(
        &mut self,
        source: KeyboardSource,
        location: Point<f64, Logical>,
    ) -> Option<TitlebarPress> {
        if !self
            .pending_restore_drag
            .as_ref()
            .is_some_and(|press| press.moved(source, location))
        {
            return None;
        }
        self.last_titlebar_press = None;
        self.pending_restore_drag.take()
    }

    /// The pointer moved with the left button `held`. A drag is not the
    /// first press of a double click.
    pub(crate) fn titlebar_motion(
        &mut self,
        source: KeyboardSource,
        location: Point<f64, Logical>,
        held: bool,
    ) {
        if held
            && self
                .last_titlebar_press
                .as_ref()
                .is_some_and(|press| press.moved(source, location))
        {
            self.last_titlebar_press = None;
        }
    }

    pub(crate) fn taskbar_mut(&mut self) -> &mut waywire_decorations::Taskbar {
        &mut self.taskbar
    }

    /// The taskbar's chips: each pin, matched to a window by process or by
    /// name, then the windows no pin matched, in map order.
    pub(crate) fn taskbar_chips(
        &self,
        windows: &Windows,
        focused: impl Fn(&Window) -> bool,
        pid: impl Fn(&Window) -> Option<u32>,
    ) -> Vec<TaskbarChip> {
        let listed: Vec<_> = windows
            .mapped_order()
            .iter()
            .filter(|window| waywire_decorations::taskbar_window(window))
            .cloned()
            .collect();
        let mut matches = vec![None; self.pins.len()];
        let mut used = Vec::new();
        for (index, pin) in self.pins.iter().enumerate() {
            if let Some(pin_pid) = pin.pid
                && let Some(window) = listed
                    .iter()
                    .find(|window| !used.contains(*window) && pid(window) == Some(pin_pid))
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
            if let Some(window) = listed.iter().find(|window| {
                !used.contains(*window)
                    && toplevel_list::metadata(window)
                        .is_some_and(|metadata| metadata.1.eq_ignore_ascii_case(name))
            }) {
                used.push(window.clone());
                matches[index] = Some(window.clone());
            }
        }
        let window_chip = |window: Window, pin: Option<usize>, fallback: String| {
            let title = toplevel_list::metadata(&window).map_or(fallback, |metadata| metadata.0);
            let state = windows.get(&window);
            TaskbarChip::new(
                title,
                ChipTarget::Window {
                    visibility: if state.is_some_and(|state| state.minimized) {
                        Visibility::Minimized
                    } else {
                        Visibility::Shown
                    },
                    attention: if focused(&window) {
                        Attention::Focused
                    } else if state.is_some_and(|state| state.urgent) {
                        Attention::Urgent
                    } else {
                        Attention::Normal
                    },
                    window,
                    pin,
                },
            )
        };
        let mut chips: Vec<_> = self
            .pins
            .iter()
            .enumerate()
            .map(|(index, pin)| {
                let name = pin
                    .command
                    .first()
                    .and_then(|command| std::path::Path::new(command).file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned();
                match matches[index].clone() {
                    Some(window) => window_chip(window, Some(index), name),
                    None => TaskbarChip::new(
                        name,
                        ChipTarget::Pin {
                            index,
                            process: if pin.child.is_some() {
                                PinProcess::Running
                            } else {
                                PinProcess::Stopped
                            },
                        },
                    ),
                }
            })
            .collect();
        chips.extend(
            listed
                .into_iter()
                .filter(|window| !used.contains(window))
                .map(|window| window_chip(window, None, String::new())),
        );
        chips
    }

    /// The command that starts pin `index`, or `None` when it is running or
    /// cannot start.
    pub(crate) fn pin_command(
        &self,
        index: usize,
        display: Option<u32>,
    ) -> Option<std::process::Command> {
        let pin = self.pins.get(index)?;
        if pin.child.is_some() {
            return None;
        }
        let program = pin.command.first()?;
        match application_command(
            program,
            &pin.command[1..],
            self.launch_environment.as_ref(),
            display,
        ) {
            Ok(command) => Some(command),
            Err(error) => {
                tracing::warn!(%error, "prepare pinned application");
                None
            }
        }
    }

    /// Start pin `index` with `command`. Returns the process ID once it runs.
    pub(crate) fn start_pin(
        &mut self,
        index: usize,
        mut command: std::process::Command,
    ) -> Option<u32> {
        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                self.pins[index].pid = Some(pid);
                self.pins[index].child = Some(child);
                Some(pid)
            }
            Err(error) => {
                tracing::warn!(%error, "start pinned application");
                None
            }
        }
    }

    /// Forget pinned applications that exited. Returns whether any did,
    /// which needs a redraw.
    pub(crate) fn reap_pins(&mut self) -> bool {
        let mut reaped = false;
        for pin in &mut self.pins {
            if let Some(child) = &mut pin.child {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        pin.child = None;
                        pin.pid = None;
                        reaped = true;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error, "wait for pinned application"),
                }
            }
        }
        reaped
    }

    #[cfg(test)]
    pub(crate) fn pins_mut(&mut self) -> &mut Vec<Pin> {
        &mut self.pins
    }

    pub(crate) fn launch_environment(&self) -> Option<&LaunchEnvironment> {
        self.launch_environment.as_ref()
    }

    pub(crate) fn set_launch_environment(&mut self, environment: LaunchEnvironment) {
        self.launch_environment = Some(environment);
    }

    pub(crate) fn clock(&self) -> &str {
        &self.clock
    }

    /// Show the local time at `time`. Returns whether the displayed minute
    /// changed, which needs a redraw.
    pub(crate) fn update_clock(&mut self, time: SystemTime) -> Result<bool> {
        let clock = local_clock(time)?;
        if self.clock == clock {
            return Ok(false);
        }
        self.clock = clock;
        Ok(true)
    }
}

/// The taskbar's input control label.
pub(crate) fn control_text(browser_holds_input: bool) -> &'static str {
    if browser_holds_input {
        "You're driving"
    } else {
        "Agent driving"
    }
}

/// The taskbar chip under `position` and the action clicking it takes:
/// restore a minimized window, minimize the focused one, focus any other.
pub(crate) fn taskbar_click(
    chips: Vec<TaskbarChip>,
    output: Rectangle<i32, Logical>,
    position: Point<f64, Logical>,
) -> Option<(TaskbarChip, WindowAction)> {
    let relative = position
        - (output.loc + Point::from((0, output.size.h - waywire_decorations::TASKBAR))).to_f64();
    let count = chips.len();
    let chip = chips.into_iter().enumerate().find_map(|(index, chip)| {
        let rect = waywire_decorations::chip_rectangle(output.size.w, count, index)?;
        rect.to_f64().contains(relative).then_some(chip)
    })?;
    let action = match chip.target() {
        ChipTarget::Window {
            visibility: Visibility::Minimized,
            ..
        } => WindowAction::Restore,
        ChipTarget::Window {
            attention: Attention::Focused,
            ..
        } => WindowAction::Minimize,
        ChipTarget::Window { .. } | ChipTarget::Pin { .. } => WindowAction::Focus,
    };
    Some((chip, action))
}

pub(crate) fn local_clock(time: SystemTime) -> Result<String> {
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

pub(crate) fn next_minute_delay(time: SystemTime) -> Duration {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let remainder = Duration::new(elapsed.as_secs() % 60, elapsed.subsec_nanos());
    Duration::from_mins(1).saturating_sub(remainder)
}

pub(crate) fn application_command(
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

/// The compositor applies chrome's decisions: it gathers seat focus and
/// client processes for the taskbar, issues activation tokens, watches
/// launched pins, and marks redraws.
impl State {
    pub(crate) fn taskbar_chips(&self) -> Vec<TaskbarChip> {
        let focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        self.chrome.taskbar_chips(
            &self.windows,
            |window| focus.is_some() && focus == focus::KeyboardFocus::window(window),
            |window| self.window_pid(window),
        )
    }

    pub(crate) fn launch_pin(&mut self, index: usize) {
        let Some(mut command) = self.chrome.pin_command(index, self.xwayland.display()) else {
            return;
        };
        // The pin click is the user's request: a single-instance app that
        // forwards this launch to its running instance may raise it.
        command.env("XDG_ACTIVATION_TOKEN", self.activation.launch_token());
        if let Some(pid) = self.chrome.start_pin(index, command) {
            if let Some(handle) = &self.loop_handle
                && let Err(error) = crate::wake_on_exit(handle, pid)
            {
                tracing::warn!(%error, "watch pinned application");
            }
            self.dirty = true;
        }
    }

    pub(crate) fn reap_pins(&mut self) {
        if self.chrome.reap_pins() {
            self.dirty = true;
        }
    }

    pub(crate) fn update_clock(&mut self, time: SystemTime) -> Result<()> {
        if self.chrome.update_clock(time)? {
            self.dirty = true;
        }
        Ok(())
    }
}
