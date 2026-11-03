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

#[derive(Default)]
pub(crate) struct Input {
    buttons: HashMap<KeyboardSource, HashSet<u32>>,
    /// Every connected agent source: EIS devices, virtual pointers and
    /// virtual keyboards. `KeyboardSource::MAIN` is the browser and never
    /// belongs here.
    agent_sources: HashSet<KeyboardSource>,
    /// Sources already logged as dropped during the current browser lease.
    dropped_agent_sources: HashSet<KeyboardSource>,
    /// The agent input state last published.
    published: pipe::AgentInput,
    browser_lease: bool,
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
    pub(crate) fn register(&mut self, source: KeyboardSource) -> Option<pipe::AgentInput> {
        if source == KeyboardSource::MAIN || !self.agent_sources.insert(source) {
            return None;
        }
        self.publish()
    }

    /// Stop counting a source as connected, once its held input is released.
    /// Returns the agent input state to publish when it changed.
    #[must_use]
    pub(crate) fn disconnect(&mut self, source: KeyboardSource) -> Option<pipe::AgentInput> {
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
    pub(crate) fn publish(&mut self) -> Option<pipe::AgentInput> {
        let connected = !self.agent_sources.is_empty();
        let state = pipe::AgentInput {
            connected,
            dropped: self.browser_lease && (connected || self.published.dropped),
        };
        (state != self.published).then(|| {
            self.published = state;
            state
        })
    }

    #[cfg(test)]
    pub(crate) fn agent_input(&self) -> pipe::AgentInput {
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
}
