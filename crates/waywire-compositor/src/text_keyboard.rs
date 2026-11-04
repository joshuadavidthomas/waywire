use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt::Write;

use smithay::backend::input::InputTime;
use smithay::backend::input::KeyState;
use smithay::input::keyboard::FilterResult;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::keyboard::KeymapFileId;
use smithay::utils::SERIAL_COUNTER;

use crate::State;

/// The XKB codes of the RO and YEN keys: character keys of real keyboards
/// that layouts without them leave empty.
const CHARACTER_KEYS: [u32; 2] = [89 + 8, 124 + 8];

#[derive(Default)]
pub(crate) struct TextKeyboard {
    pending: VecDeque<String>,
    keymap: Keymap,
    slots: Vec<Slot>,
    clock: u64,
}

enum Keymap {
    Unread { previous: Option<KeymapFileId> },
    Base(String),
    Installed { base: String, id: KeymapFileId },
}

impl Default for Keymap {
    fn default() -> Self {
        Self::Unread { previous: None }
    }
}

impl Keymap {
    fn installed(&self) -> Option<KeymapFileId> {
        match self {
            Self::Unread { previous } => *previous,
            Self::Base(_) => None,
            Self::Installed { id, .. } => Some(*id),
        }
    }
}

enum SlotName {
    Existing(String),
    Added(String),
}

impl SlotName {
    fn as_str(&self) -> &str {
        match self {
            Self::Existing(name) | Self::Added(name) => name,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeymapEdit {
    Unchanged,
    Changed,
}

struct Slot {
    code: u32,
    name: SlotName,
    character: Option<char>,
    last_used: u64,
}

impl TextKeyboard {
    /// Queue composed text to type once no key is held.
    pub(crate) fn queue(&mut self, text: String) {
        self.pending.push_back(text);
    }

    /// Take the next queued text.
    fn next_pending(&mut self) -> Option<String> {
        self.pending.pop_front()
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The keymap with the slots' characters became the seat's keymap.
    fn installed(&mut self, id: KeymapFileId) {
        self.keymap = match std::mem::take(&mut self.keymap) {
            Keymap::Base(base) | Keymap::Installed { base, .. } => Keymap::Installed { base, id },
            Keymap::Unread { .. } => Keymap::Unread { previous: Some(id) },
        };
    }

    fn base(&mut self, base: String, slots: Vec<Slot>) {
        tracing::info!(
            x11 = slots.iter().filter(|slot| slot.code <= 255).count(),
            wayland = slots.iter().filter(|slot| slot.code > 255).count(),
            "text keyboard spare keycodes"
        );
        self.keymap = match self.keymap.installed() {
            Some(id) => Keymap::Installed { base, id },
            None => Keymap::Base(base),
        };
        self.slots = slots;
    }

    /// Re-read the seat's keymap on the next attempt after a failed edit.
    fn abandon(&mut self) {
        self.keymap = Keymap::Unread {
            previous: self.keymap.installed(),
        };
    }

    /// Start over from the seat's keymap when something else replaced the
    /// one this installed.
    fn sync(&mut self, active: KeymapFileId) {
        if self.keymap.installed().is_some_and(|id| active != id) {
            self.keymap = Keymap::default();
            self.slots.clear();
        }
    }

    /// Empty the spare keycode `code` when it holds a character of the keymap
    /// `active`. Returns whether the keymap must be installed again.
    fn reclaim(&mut self, active: KeymapFileId, code: u32) -> KeymapEdit {
        if self.keymap.installed() != Some(active) {
            return KeymapEdit::Unchanged;
        }
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.code == code && slot.character.is_some())
        else {
            return KeymapEdit::Unchanged;
        };
        slot.character = None;
        KeymapEdit::Changed
    }

    /// Whether a spare keycode is one of the real character keys.
    fn has_character_key(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| CHARACTER_KEYS.contains(&slot.code))
    }

    /// Spare keycodes, among those `usable` allows, that type `text`, and
    /// whether the keymap has to change for them.
    fn assign(&mut self, text: &str, usable: impl Fn(u32) -> bool) -> (Vec<u32>, KeymapEdit) {
        let eligible = |slot: &Slot| usable(slot.code);
        let capacity = self.slots.iter().filter(|slot| eligible(slot)).count();
        let mut characters = HashSet::new();
        for character in text.chars() {
            if characters.len() < capacity || characters.contains(&character) {
                characters.insert(character);
            }
        }
        let mut used: HashSet<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                eligible(slot) && slot.character.is_some_and(|c| characters.contains(&c))
            })
            .map(|(index, _)| index)
            .collect();
        let mut codes = Vec::new();
        let mut changed = KeymapEdit::Unchanged;
        for character in text.chars() {
            if !characters.contains(&character) {
                tracing::warn!(%character, "text keyboard spare keycodes exhausted; character dropped");
                continue;
            }
            let index = self
                .slots
                .iter()
                .position(|s| eligible(s) && s.character == Some(character))
                .or_else(|| {
                    self.slots
                        .iter()
                        .enumerate()
                        .filter(|(index, slot)| eligible(slot) && !used.contains(index))
                        .min_by_key(|(_, slot)| slot.last_used)
                        .map(|(index, _)| index)
                });
            let Some(index) = index else { continue };
            used.insert(index);
            self.clock += 1;
            let slot = &mut self.slots[index];
            if slot.character != Some(character) {
                slot.character = Some(character);
                changed = KeymapEdit::Changed;
            }
            slot.last_used = self.clock;
            codes.push(slot.code);
        }
        (codes, changed)
    }

    fn keymap(&self) -> Option<String> {
        let mut keymap = match &self.keymap {
            Keymap::Unread { .. } => return None,
            Keymap::Base(base) | Keymap::Installed { base, .. } => base.clone(),
        };
        let symbols_end = keymap.rfind("};\n\n};")?;
        let mut symbols = String::new();
        let mut names = String::new();
        for slot in &self.slots {
            if let Some(c) = slot.character {
                writeln!(
                    symbols,
                    "\tkey <{}> {{ [ U{:04X} ] }};",
                    slot.name.as_str(),
                    u32::from(c)
                )
                .ok()?;
                if let SlotName::Added(name) = &slot.name {
                    writeln!(names, "\t<{}> = {};", name, slot.code).ok()?;
                }
            }
        }
        keymap.insert_str(symbols_end, &symbols);
        let keycodes_end = keymap.find("};\n\nxkb_types")?;
        keymap.insert_str(keycodes_end, &names);
        Some(keymap)
    }
}

impl State {
    fn init_text_keyboard(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let result = keyboard.with_xkb_state(self, |context| {
            let Ok(guard) = context.xkb().lock() else {
                return None;
            };
            // SAFETY: The keymap reference does not outlive the lock or acquire a ref-count.
            let keymap = unsafe { guard.keymap() };
            let mut slots = Vec::new();
            for code in 8..=511 {
                let key = code.into();
                let used = (0..keymap.num_layouts_for_key(key)).any(|layout| {
                    (0..keymap.num_levels_for_key(key, layout))
                        .any(|level| !keymap.key_get_syms_by_level(key, layout, level).is_empty())
                });
                if !used {
                    let name = match keymap.key_get_name(key) {
                        Some(name) => SlotName::Existing(name.to_owned()),
                        None => SlotName::Added(format!("W{code:03}")),
                    };
                    slots.push(Slot {
                        code,
                        name,
                        character: None,
                        last_used: 0,
                    });
                }
            }
            Some((guard.keymap_as_string(), slots))
        });
        if let Some((base, slots)) = result {
            self.text_keyboard.base(base, slots);
        }
    }

    /// Installs the base keymap with the slots' current characters.
    fn install_text_keymap(&mut self) -> bool {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        let Some(keymap) = self.text_keyboard.keymap() else {
            tracing::warn!("unexpected XKB keymap format");
            self.text_keyboard.abandon();
            return false;
        };
        let modifiers = keyboard.modifier_state();
        if let Err(error) = keyboard.set_keymap_from_string(self, keymap) {
            tracing::warn!(%error, "text keyboard keymap failed");
            self.text_keyboard.abandon();
            return false;
        }
        self.text_keyboard.installed(keyboard.keymap_file().id());
        keyboard.set_modifier_state(modifiers);
        keyboard.advertise_modifier_state(self);
        true
    }

    /// A key pressed on a spare keycode that holds a composed character, such
    /// as Zenkaku/Hankaku under a US layout, gets its own empty symbols back
    /// before the press, so it never retypes that character.
    pub(crate) fn reclaim_text_keycode(&mut self, code: u32) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if self
            .text_keyboard
            .reclaim(keyboard.keymap_file().id(), code)
            == KeymapEdit::Changed
        {
            self.install_text_keymap();
        }
    }

    /// A seat keycode whose first level types `character`, holding it in a
    /// spare keycode. Installs the keymap when the character is new to it.
    ///
    /// Chromium drops key codes that no real keyboard has, such as the
    /// Wayland spare keycodes above 255, so Wayland clients get the RO and
    /// YEN keys of Japanese keyboards when the seat's layout leaves them
    /// empty, as US and most European layouts do. X11 clients keep the larger
    /// pool, because they can decode a key with a mapping that replaced it.
    pub(crate) fn text_keycode(&mut self, character: char) -> Option<u32> {
        let keyboard = self.seat.get_keyboard()?;
        self.prepare_text_keyboard(keyboard.keymap_file().id());
        let wayland = matches!(
            keyboard.current_focus(),
            Some(crate::focus::KeyboardFocus::Wayland(_))
        );
        let real_key = |code| CHARACTER_KEYS.contains(&code);
        let usable: &dyn Fn(u32) -> bool = if wayland && self.text_keyboard.has_character_key() {
            &real_key
        } else {
            &|code| (code > 255) == wayland
        };
        let (codes, changed) = self.text_keyboard.assign(&character.to_string(), usable);
        if changed == KeymapEdit::Changed && !self.install_text_keymap() {
            return None;
        }
        codes.first().copied()
    }

    /// Start over from the seat's keymap when something else replaced the
    /// one this installed.
    fn prepare_text_keyboard(&mut self, active: KeymapFileId) {
        self.text_keyboard.sync(active);
        if matches!(self.text_keyboard.keymap, Keymap::Unread { .. }) {
            self.init_text_keyboard();
        }
    }

    pub(crate) fn flush_text_keyboard(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if !keyboard.pressed_keys().is_empty() || !self.text_keyboard.has_pending() {
            return;
        }
        self.prepare_text_keyboard(keyboard.keymap_file().id());
        let wayland = matches!(
            keyboard.current_focus(),
            Some(crate::focus::KeyboardFocus::Wayland(_))
        );
        while let Some(text) = self.text_keyboard.next_pending() {
            let (codes, changed) = self
                .text_keyboard
                .assign(&text, |code| (code > 255) == wayland);
            if changed == KeymapEdit::Changed && !self.install_text_keymap() {
                return;
            }
            for code in codes {
                for state in [KeyState::Pressed, KeyState::Released] {
                    keyboard.input_from_source::<(), _>(
                        KeyboardSource::MAIN,
                        self,
                        code.into(),
                        state,
                        SERIAL_COUNTER.next_serial(),
                        InputTime::now(),
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
        }
    }
}
