//! The window list: every window with a foreign-toplevel identifier, its
//! bounds on screen in output pixels, its place in the stacking order and its
//! state, as `GetWindows` returns it and `WindowsChanged` reports it.
//!
//! The list is read from the space the compositor renders, so it matches the
//! screen: a move, a resize or a client's own placement request changes it
//! before the next frame shows the change.

use smithay::desktop::Window;
use smithay::utils::Logical;
use smithay::utils::Physical;
use smithay::utils::Rectangle;

use super::Modality;
use super::WindowRecord;
use crate::State;
use crate::focus::KeyboardFocus;

/// One listed window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowEntry {
    record: WindowRecord,
    /// The client's content, without the compositor's titlebar.
    geometry: Rectangle<i32, Physical>,
    /// The content and the compositor's titlebar, when it draws one.
    frame: Rectangle<i32, Physical>,
    /// Position in the stacking order; 0 is the topmost window.
    stack: u32,
    focus: Focus,
    visibility: waywire_decorations::Visibility,
    size: waywire_decorations::SizeMode,
    extent: Extent,
    /// The parent window's identifier, for dialogs; empty otherwise.
    parent: String,
    modality: Modality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Focused,
    Unfocused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Extent {
    Windowed,
    Fullscreen,
}

impl WindowEntry {
    pub(crate) fn record(&self) -> &WindowRecord {
        &self.record
    }
    pub(crate) fn geometry(&self) -> Rectangle<i32, Physical> {
        self.geometry
    }
    pub(crate) fn frame(&self) -> Rectangle<i32, Physical> {
        self.frame
    }
    pub(crate) fn stack(&self) -> u32 {
        self.stack
    }
    pub(crate) fn focused(&self) -> bool {
        self.focus == Focus::Focused
    }
    pub(crate) fn minimized(&self) -> bool {
        self.visibility == waywire_decorations::Visibility::Minimized
    }
    pub(crate) fn maximized(&self) -> bool {
        self.size == waywire_decorations::SizeMode::Maximized
    }
    pub(crate) fn fullscreen(&self) -> bool {
        self.extent == Extent::Fullscreen
    }
    pub(crate) fn parent(&self) -> &str {
        &self.parent
    }
    pub(crate) fn modal(&self) -> bool {
        self.modality == Modality::Modal
    }
}

/// The whole list and the output it is measured on.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowList {
    /// Output size in pixels.
    width: u32,
    height: u32,
    /// Output pixels per logical unit, the factor between these bounds and
    /// `RemoteDesktop` portal coordinates.
    scale: f64,
    /// The area windows are placed in: the output above the taskbar.
    work_area: Rectangle<i32, Physical>,
    /// Topmost first.
    windows: Vec<WindowEntry>,
}

impl WindowList {
    pub(crate) fn width(&self) -> u32 {
        self.width
    }
    pub(crate) fn height(&self) -> u32 {
        self.height
    }
    pub(crate) fn scale(&self) -> f64 {
        self.scale
    }
    pub(crate) fn work_area(&self) -> Rectangle<i32, Physical> {
        self.work_area
    }
    pub(crate) fn windows(&self) -> &[WindowEntry] {
        &self.windows
    }
}

/// A `WindowsChanged` signal.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowsChanged {
    serial: u64,
    time_usec: u64,
    list: WindowList,
}

impl WindowsChanged {
    pub(super) fn new(serial: u64, time_usec: u64, list: WindowList) -> Self {
        Self {
            serial,
            time_usec,
            list,
        }
    }
    pub(super) fn serial(&self) -> u64 {
        self.serial
    }
    pub(super) fn time_usec(&self) -> u64 {
        self.time_usec
    }
    pub(crate) fn list(&self) -> &WindowList {
        &self.list
    }
}

/// Logical bounds in output pixels. Both edges are rounded, so adjacent
/// rectangles stay adjacent at fractional scales.
pub(super) fn to_pixels(rect: Rectangle<i32, Logical>, scale: f64) -> Rectangle<i32, Physical> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "coordinates are bounded by the output size"
    )]
    let edge = |value: i32| (f64::from(value) * scale).round() as i32;
    let left = edge(rect.loc.x);
    let top = edge(rect.loc.y);
    Rectangle::new(
        (left, top).into(),
        (
            edge(rect.loc.x + rect.size.w) - left,
            edge(rect.loc.y + rect.size.h) - top,
        )
            .into(),
    )
}

/// A window's content and its frame, the content plus the titlebar the
/// compositor draws, when the window's content is at `location`.
pub(super) fn bounds(
    window: &Window,
    location: smithay::utils::Point<i32, Logical>,
) -> (Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
    let geometry = Rectangle::new(location, window.geometry().size);
    let mut frame = geometry;
    if waywire_decorations::decorated(window) {
        frame.loc.y -= waywire_decorations::BAR;
        frame.size.h += waywire_decorations::BAR;
    }
    (geometry, frame)
}

impl State {
    /// The current window list, topmost first.
    pub(crate) fn window_list(&self) -> WindowList {
        let scale = self.output.current_scale().fractional_scale();
        let focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        let windows = self
            .space
            .elements()
            .rev()
            .filter(|window| self.toplevel_list.identifier(window).is_some())
            .filter_map(|window| self.window_entry(window, focus.as_ref(), scale))
            .enumerate()
            .map(|(index, mut entry)| {
                entry.stack = u32::try_from(index).unwrap_or(u32::MAX);
                entry
            })
            .collect();
        WindowList {
            width: self.size.width(),
            height: self.size.height(),
            scale,
            work_area: self
                .work_area()
                .map(|area| to_pixels(area, scale))
                .unwrap_or_default(),
            windows,
        }
    }

    fn window_entry(
        &self,
        window: &Window,
        focus: Option<&KeyboardFocus>,
        scale: f64,
    ) -> Option<WindowEntry> {
        let (geometry, frame) = bounds(window, self.space.element_location(window)?);
        let size = waywire_decorations::window_flags(window).size();
        let fullscreen = window.toplevel().map_or_else(
            || {
                window
                    .x11_surface()
                    .is_some_and(smithay::xwayland::X11Surface::is_fullscreen)
            },
            |top| {
                top.with_committed_state(|state| {
                    state.is_some_and(|state| {
                        state.states.contains(
                            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen,
                        )
                    })
                })
            },
        );
        Some(WindowEntry {
            record: self.window_record(window),
            geometry: to_pixels(geometry, scale),
            frame: to_pixels(frame, scale),
            stack: 0,
            focus: if focus.is_some() && focus.cloned() == KeyboardFocus::window(window) {
                Focus::Focused
            } else {
                Focus::Unfocused
            },
            visibility: if self.windows.is_minimized(window) {
                waywire_decorations::Visibility::Minimized
            } else {
                waywire_decorations::Visibility::Shown
            },
            size,
            extent: if fullscreen {
                Extent::Fullscreen
            } else {
                Extent::Windowed
            },
            parent: crate::wm::parent_window(&self.space, window)
                .and_then(|parent| self.toplevel_list.identifier(&parent))
                .unwrap_or_default(),
            modality: if crate::wm::modal_dialog(window) {
                Modality::Modal
            } else {
                Modality::Modeless
            },
        })
    }

    /// Send `WindowsChanged` to every subscriber when the list differs from
    /// the one last sent. The loop calls this after each dispatch, and
    /// `GetWindows` before it replies, so a reply never precedes the signal
    /// for a change it already shows.
    pub(crate) fn publish_window_list(&mut self) {
        if !self.delivery.wants_window_list() {
            return;
        }
        let list = self.window_list();
        self.delivery.windows_listed(list);
    }
}
