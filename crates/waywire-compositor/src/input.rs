//! Input arbitration: who may drive the desktop.
//!
//! The browser (`KeyboardSource::MAIN`) and agent sources (EIS devices,
//! virtual pointers and keyboards, portal sessions) share one seat. While a
//! browser holds the input lease, agent input is dropped. `Input` keeps the
//! bookkeeping behind that rule and never touches the seat itself: methods
//! return what the caller must apply, such as sources to release or an
//! `AgentInput` change to publish.
use std::collections::HashMap;
use std::collections::HashSet;

use smithay::backend::input::ButtonState;
use smithay::input::keyboard::KeyboardSource;
use waywire_protocol::pipe;

/// Why the pointer is moving.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerMotionKind {
    /// Input from the browser or an agent.
    Input,
    /// A focus refresh replaying the position under a window grab, which the
    /// grab ignores.
    RefreshGrabFocus,
    /// A client's pointer warp hint. Like a focus refresh, it skips pointer
    /// constraints and titlebar drags.
    WarpHint,
}

pub(crate) struct Input {
    buttons: HashMap<KeyboardSource, HashSet<u32>>,
    /// Every connected agent source: EIS devices, virtual pointers and
    /// virtual keyboards. `KeyboardSource::MAIN` is the browser and never
    /// belongs here.
    agent_sources: HashSet<KeyboardSource>,
    /// Sources already logged as dropped during the current browser lease.
    dropped_agent_sources: HashSet<KeyboardSource>,
    /// The agent input state last published.
    published: pipe::AgentInputState,
    browser_lease: bool,
    pointer_motion_kind: PointerMotionKind,
    /// The source the cursor position was last reported for.
    cursor_source: pipe::CursorSource,
    /// Set during a focus refresh, which moves the pointer on behalf of
    /// whoever moved it last.
    focus_refresh_source: Option<pipe::CursorSource>,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            buttons: HashMap::new(),
            agent_sources: HashSet::new(),
            dropped_agent_sources: HashSet::new(),
            published: pipe::AgentInputState::default(),
            browser_lease: false,
            pointer_motion_kind: PointerMotionKind::Input,
            cursor_source: pipe::CursorSource::Browser,
            focus_refresh_source: None,
        }
    }
}

impl Input {
    pub(crate) fn browser_holds_input(&self) -> bool {
        self.browser_lease
    }

    /// Whether `source` may reach the seat. Browser input always passes.
    /// While a browser holds the lease agent events are dropped, with one
    /// debug line per source per lease rather than one per event. Register
    /// the source first (see [`Self::register`]) so a takeover can release it.
    pub(crate) fn accepts(&mut self, source: KeyboardSource) -> bool {
        if source == KeyboardSource::MAIN || !self.browser_lease {
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

    /// Count an agent source as connected. Returns the agent input state to
    /// publish when it changed.
    #[must_use]
    pub(crate) fn register(&mut self, source: KeyboardSource) -> Option<pipe::AgentInputState> {
        if source == KeyboardSource::MAIN || !self.agent_sources.insert(source) {
            return None;
        }
        self.publish()
    }

    /// Stop counting a source as connected, once its held input is released.
    /// Returns the agent input state to publish when it changed.
    #[must_use]
    pub(crate) fn disconnect(&mut self, source: KeyboardSource) -> Option<pipe::AgentInputState> {
        self.dropped_agent_sources.remove(&source);
        if self.agent_sources.remove(&source) {
            self.publish()
        } else {
            None
        }
    }

    /// Record a browser takeover or handback. Returns `None` when the lease
    /// did not change, otherwise the sources whose held input the caller must
    /// release, in order: on takeover every agent source, then the browser.
    /// Publish the agent input state once they are released.
    #[must_use]
    pub(crate) fn set_browser_lease(&mut self, held: bool) -> Option<Vec<KeyboardSource>> {
        if self.browser_lease == held {
            return None;
        }
        self.browser_lease = held;
        self.dropped_agent_sources.clear();
        let mut release: Vec<_> = if held {
            self.agent_sources.iter().copied().collect()
        } else {
            Vec::new()
        };
        release.push(KeyboardSource::MAIN);
        Some(release)
    }

    /// The agent input state, when it changed since it was last published.
    #[must_use]
    pub(crate) fn publish(&mut self) -> Option<pipe::AgentInputState> {
        let connected = !self.agent_sources.is_empty();
        let dropped = self.browser_lease && (connected || self.published.dropped());
        let state = match (connected, dropped) {
            (false, false) => pipe::AgentInputState::None,
            (true, false) => pipe::AgentInputState::Driving,
            (true, true) => pipe::AgentInputState::Paused,
            (false, true) => pipe::AgentInputState::PausedDisconnected,
        };
        (state != self.published).then(|| {
            self.published = state;
            state
        })
    }

    #[cfg(test)]
    pub(crate) fn agent_input(&self) -> pipe::AgentInputState {
        self.published
    }

    /// Record one source's button press or release. Returns whether the
    /// aggregate over all sources changed, which is when the seat sees it:
    /// while another source holds the same button, no event is sent.
    pub(crate) fn track_button(
        &mut self,
        source: KeyboardSource,
        button: u32,
        state: ButtonState,
    ) -> bool {
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
        before != after
    }

    /// The buttons `source` holds.
    pub(crate) fn buttons(&self, source: KeyboardSource) -> Option<&HashSet<u32>> {
        self.buttons.get(&source)
    }

    /// Forget a source's buttons once each one has been released.
    pub(crate) fn forget_buttons(&mut self, source: KeyboardSource) {
        self.buttons.remove(&source);
    }

    pub(crate) fn pointer_motion_kind(&self) -> PointerMotionKind {
        self.pointer_motion_kind
    }

    pub(crate) fn set_pointer_motion_kind(&mut self, kind: PointerMotionKind) {
        self.pointer_motion_kind = kind;
    }

    /// Attribute pointer motion until [`Self::end_focus_refresh`] to the
    /// source that last moved the cursor.
    pub(crate) fn begin_focus_refresh(&mut self) {
        self.focus_refresh_source = Some(self.cursor_source);
    }

    pub(crate) fn end_focus_refresh(&mut self) {
        self.focus_refresh_source = None;
    }

    /// Record that `source` moved the pointer, and return the cursor source
    /// to report for that motion.
    pub(crate) fn move_cursor(&mut self, source: KeyboardSource) -> pipe::CursorSource {
        self.cursor_source =
            self.focus_refresh_source
                .unwrap_or(if source == KeyboardSource::MAIN {
                    pipe::CursorSource::Browser
                } else {
                    pipe::CursorSource::Agent
                });
        self.cursor_source
    }
}
