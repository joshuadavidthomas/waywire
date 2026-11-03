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

use super::Notice;
use super::Signal;
use super::WindowRecord;
use super::monotonic_usec;
use crate::State;
use crate::focus::KeyboardFocus;

/// One listed window.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each state is a separate key in the D-Bus record"
)]
pub(crate) struct WindowEntry {
    pub(crate) record: WindowRecord,
    /// The client's content, without the compositor's titlebar.
    pub(crate) geometry: Rectangle<i32, Physical>,
    /// The content and the compositor's titlebar, when it draws one.
    pub(crate) frame: Rectangle<i32, Physical>,
    /// Position in the stacking order; 0 is the topmost window.
    pub(crate) stack: u32,
    pub(crate) focused: bool,
    pub(crate) minimized: bool,
    pub(crate) maximized: bool,
    pub(crate) fullscreen: bool,
    /// The parent window's identifier, for dialogs; empty otherwise.
    pub(crate) parent: String,
    pub(crate) modal: bool,
}

/// The whole list and the output it is measured on.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowList {
    /// Output size in pixels.
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Output pixels per logical unit, the factor between these bounds and
    /// `RemoteDesktop` portal coordinates.
    pub(crate) scale: f64,
    /// The area windows are placed in: the output above the taskbar.
    pub(crate) work_area: Rectangle<i32, Physical>,
    /// Topmost first.
    pub(crate) windows: Vec<WindowEntry>,
}

/// A `WindowsChanged` signal.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowsChanged {
    pub(crate) serial: u64,
    pub(crate) time_usec: u64,
    pub(crate) list: WindowList,
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
        let (_, maximized) = waywire_decorations::window_flags(window);
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
            focused: focus.is_some() && focus.cloned() == KeyboardFocus::window(window),
            minimized: self
                .windows
                .get(window)
                .is_some_and(|state| state.minimized),
            maximized,
            fullscreen,
            parent: crate::wm::parent_window(&self.space, window)
                .and_then(|parent| self.toplevel_list.identifier(&parent))
                .unwrap_or_default(),
            modal: crate::wm::modal_dialog(window),
        })
    }

    /// Send `WindowsChanged` to every subscriber when the list differs from
    /// the one last sent. The loop calls this after each dispatch, and
    /// `GetWindows` before it replies, so a reply never precedes the signal
    /// for a change it already shows.
    pub(crate) fn publish_window_list(&mut self) {
        let everyone = self.delivery.everyone();
        if everyone.is_empty() {
            self.delivery.reported_windows = None;
            return;
        }
        let list = self.window_list();
        if self.delivery.reported_windows.as_ref() == Some(&list) {
            return;
        }
        self.delivery.reported_windows = Some(list.clone());
        let changed = WindowsChanged {
            serial: self.delivery.next_serial(),
            time_usec: monotonic_usec(),
            list,
        };
        self.delivery.send(
            everyone,
            Signal::Notice(Notice::WindowsChanged(Box::new(changed))),
        );
    }
}
