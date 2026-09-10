//! Gateway-to-streamd pipe wire format, version 2.
//! Values use little-endian byte order. Command records have a 16-byte header plus an optional
//! text payload, while event records have an 8-byte header plus a payload.

use std::num::NonZeroU16;
use std::num::NonZeroU32;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::PROTOCOL_VERSION;

pub const COMMAND_HEADER_BYTES: usize = 16;
pub const EVENT_HEADER_BYTES: usize = 8;
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 4_000;
pub const MAX_EVENT_BYTES: usize = MAX_CLIPBOARD_BYTES;
pub const MAX_RAW_PIXELS: u64 = 3840 * 2160;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct InvalidValue(&'static str);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("pipe stream ended halfway through a record")]
    Truncated,
    #[error("record header is invalid")]
    InvalidHeader,
    #[error("record fields are invalid for type {0}")]
    InvalidFields(u8),
    #[error("event type {0} has an invalid payload")]
    InvalidEvent(u8),
    #[error("record payload exceeds its limit")]
    PayloadTooLarge,
    #[error("record payload is not valid UTF-8")]
    NotUtf8,
}

macro_rules! nonzero_newtype {
    ($name:ident, $inner:ty, $nonzero:ty, $label:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name($nonzero);

        impl $name {
            pub fn new(value: $inner) -> Result<Self, InvalidValue> {
                <$nonzero>::new(value).map(Self).ok_or(InvalidValue($label))
            }

            #[must_use]
            pub const fn get(self) -> $inner {
                self.0.get()
            }
        }
    };
}

macro_rules! ranged_newtype {
    ($name:ident, $inner:ty, $minimum:expr, $maximum:expr, $label:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            pub fn new(value: $inner) -> Result<Self, InvalidValue> {
                if ($minimum..=$maximum).contains(&value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidValue($label))
                }
            }

            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

nonzero_newtype!(Generation, u32, NonZeroU32, "generation must be nonzero");
nonzero_newtype!(RequestId, u16, NonZeroU16, "request ID must be nonzero");
nonzero_newtype!(
    InputSequence,
    u32,
    NonZeroU32,
    "input sequence must be nonzero"
);
ranged_newtype!(
    ScaleV120,
    u16,
    120,
    480,
    "scale must be between 120 and 480"
);
ranged_newtype!(
    Kbps,
    u32,
    300,
    50_000,
    "bitrate must be between 300 and 50000 Kbps"
);
ranged_newtype!(
    Fps,
    u32,
    10,
    120,
    "frame rate must be between 10 and 120 FPS"
);
ranged_newtype!(
    ScalePercent,
    u32,
    50,
    100,
    "scale must be between 50 and 100 percent"
);
ranged_newtype!(KeyCode, u32, 1, 255, "key code must be between 1 and 255");
ranged_newtype!(
    PointerCoordinate,
    u32,
    0,
    65_535,
    "pointer coordinate must be between 0 and 65535"
);
ranged_newtype!(
    FrameDimension,
    u16,
    1,
    u16::MAX,
    "frame dimension must be nonzero"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FrameSize {
    width: u32,
    height: u32,
}

impl FrameSize {
    pub fn new(width: u32, height: u32) -> Result<Self, InvalidValue> {
        let pixels = u64::from(width) * u64::from(height);
        if (320..=6_000).contains(&width)
            && (180..=6_000).contains(&height)
            && width.is_multiple_of(2)
            && height.is_multiple_of(2)
            && pixels <= MAX_RAW_PIXELS
        {
            Ok(Self { width, height })
        } else {
            Err(InvalidValue("frame size is outside the supported range"))
        }
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorSize {
    width: u32,
    height: u32,
}

impl CursorSize {
    pub fn new(width: u32, height: u32) -> Result<Self, InvalidValue> {
        if (1..=256).contains(&width) && (1..=256).contains(&height) {
            Ok(Self { width, height })
        } else {
            Err(InvalidValue("cursor dimensions must be between 1 and 256"))
        }
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn byte_count(self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClipboardText(String);

impl ClipboardText {
    pub fn new(text: String) -> Result<Self, InvalidValue> {
        if text.len() <= MAX_CLIPBOARD_BYTES {
            Ok(Self(text))
        } else {
            Err(InvalidValue("clipboard text must be at most 1 MiB"))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputText(String);

impl InputText {
    pub fn new(text: String) -> Result<Self, InvalidValue> {
        if text.len() > MAX_TEXT_BYTES {
            Err(InvalidValue("input text must be at most 4000 bytes"))
        } else if text.contains('\0') {
            Err(InvalidValue("input text must not contain NUL"))
        } else {
            Ok(Self(text))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerDelta(f32);

impl PointerDelta {
    pub fn new(value: f32) -> Result<Self, InvalidValue> {
        if value.is_finite() && (-4096.0..=4096.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidValue(
                "pointer delta must be finite and between -4096 and 4096",
            ))
        }
    }

    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Side,
    Extra,
}

impl PointerButton {
    const fn from_wire(value: u32) -> Option<Self> {
        match value {
            0x110 => Some(Self::Left),
            0x111 => Some(Self::Right),
            0x112 => Some(Self::Middle),
            0x113 => Some(Self::Side),
            0x114 => Some(Self::Extra),
            _ => None,
        }
    }

    #[must_use]
    pub const fn wire(self) -> u32 {
        match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
            Self::Side => 0x113,
            Self::Extra => 0x114,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    Released,
    Pressed,
    Repeated,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TextAction {
    Preedit,
    Commit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    PointerAbsolute {
        x: PointerCoordinate,
        y: PointerCoordinate,
        sequence: InputSequence,
    },
    PointerButton {
        button: PointerButton,
        pressed: bool,
        sequence: InputSequence,
    },
    PointerScroll {
        dx: PointerDelta,
        dy: PointerDelta,
        sequence: InputSequence,
    },
    KeyboardKey {
        key: KeyCode,
        state: KeyState,
        sequence: InputSequence,
    },
    ReleaseAll,
    Resize {
        size: FrameSize,
        scale_v120: ScaleV120,
        request_id: RequestId,
    },
    Clipboard(ClipboardText),
    PointerRelative {
        dx: PointerDelta,
        dy: PointerDelta,
        sequence: InputSequence,
    },
    Quality {
        bitrate_kbps: Kbps,
        fps: Fps,
        scale_percent: ScalePercent,
    },
    Text {
        action: TextAction,
        text: InputText,
        sequence: InputSequence,
    },
    KeyframeReadiness {
        generation: Generation,
        ready: bool,
    },
}

impl Command {
    #[must_use]
    pub fn input_sequence(&self) -> Option<InputSequence> {
        match self {
            Self::PointerAbsolute { sequence, .. }
            | Self::PointerButton { sequence, .. }
            | Self::PointerScroll { sequence, .. }
            | Self::KeyboardKey { sequence, .. }
            | Self::PointerRelative { sequence, .. }
            | Self::Text { sequence, .. } => Some(*sequence),
            Self::ReleaseAll
            | Self::Resize { .. }
            | Self::Clipboard(_)
            | Self::Quality { .. }
            | Self::KeyframeReadiness { .. } => None,
        }
    }

    #[must_use]
    pub fn encoded_len(&self) -> usize {
        COMMAND_HEADER_BYTES
            + match self {
                Self::Clipboard(text) => text.as_str().len(),
                Self::Text { text, .. } => text.as_str().len(),
                Self::PointerAbsolute { .. }
                | Self::PointerButton { .. }
                | Self::PointerScroll { .. }
                | Self::KeyboardKey { .. }
                | Self::ReleaseAll
                | Self::Resize { .. }
                | Self::PointerRelative { .. }
                | Self::Quality { .. }
                | Self::KeyframeReadiness { .. } => 0,
            }
    }

    // Keeping the exhaustive wire mapping together makes kind drift visible in review.
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::PointerAbsolute { x, y, sequence } => command(
                CommandKind::PointerAbsolute,
                0,
                x.get(),
                y.get(),
                sequence.get(),
            ),
            Self::PointerButton {
                button,
                pressed,
                sequence,
            } => command(
                CommandKind::PointerButton,
                u8::from(*pressed),
                button.wire(),
                0,
                sequence.get(),
            ),
            Self::PointerScroll { dx, dy, sequence } => command(
                CommandKind::PointerScroll,
                0,
                dx.get().to_bits(),
                dy.get().to_bits(),
                sequence.get(),
            ),
            Self::KeyboardKey {
                key,
                state,
                sequence,
            } => command(
                CommandKind::KeyboardKey,
                match state {
                    KeyState::Released => 0,
                    KeyState::Pressed => 1,
                    KeyState::Repeated => 2,
                },
                key.get(),
                0,
                sequence.get(),
            ),
            Self::ReleaseAll => command(CommandKind::ReleaseAll, 0, 0, 0, 0),
            Self::Resize {
                size,
                scale_v120,
                request_id,
            } => {
                let packed = u32::from(scale_v120.get()) | (u32::from(request_id.get()) << 16);
                command(CommandKind::Resize, 0, size.width(), size.height(), packed)
            }
            Self::Clipboard(text) => {
                let length = u32::try_from(text.as_str().len()).unwrap_or(u32::MAX);
                let mut bytes = command(CommandKind::Clipboard, 0, length, 0, 0);
                bytes.extend_from_slice(text.as_str().as_bytes());
                bytes
            }
            Self::PointerRelative { dx, dy, sequence } => command(
                CommandKind::PointerRelative,
                0,
                dx.get().to_bits(),
                dy.get().to_bits(),
                sequence.get(),
            ),
            Self::Quality {
                bitrate_kbps,
                fps,
                scale_percent,
            } => command(
                CommandKind::Quality,
                0,
                bitrate_kbps.get(),
                fps.get(),
                scale_percent.get(),
            ),
            Self::Text {
                action,
                text,
                sequence,
            } => {
                let length = u32::try_from(text.as_str().len()).unwrap_or(u32::MAX);
                let mut bytes = command(
                    CommandKind::Text,
                    match action {
                        TextAction::Preedit => 1,
                        TextAction::Commit => 0,
                    },
                    length,
                    sequence.get(),
                    0,
                );
                bytes.extend_from_slice(text.as_str().as_bytes());
                bytes
            }
            Self::KeyframeReadiness { generation, ready } => command(
                CommandKind::KeyframeReadiness,
                u8::from(*ready),
                generation.get(),
                0,
                0,
            ),
        }
    }

    // One exhaustive decoder keeps every command invariant beside its wire fields.
    #[allow(clippy::too_many_lines)]
    pub fn decode(header: CommandHeader, text: &[u8]) -> Result<Self, ProtocolError> {
        if text.len() != header.text_payload_len().unwrap_or(0) {
            return Err(ProtocolError::Truncated);
        }
        let invalid = || ProtocolError::InvalidFields(header.kind.wire());
        match header.kind {
            CommandKind::PointerAbsolute => {
                if header.state != 0 || !text.is_empty() {
                    return Err(invalid());
                }
                Ok(Self::PointerAbsolute {
                    x: field(PointerCoordinate::new(header.a), invalid())?,
                    y: field(PointerCoordinate::new(header.b), invalid())?,
                    sequence: field(InputSequence::new(header.c), invalid())?,
                })
            }
            CommandKind::PointerButton => {
                if header.state > 1 || header.b != 0 || !text.is_empty() {
                    return Err(invalid());
                }
                Ok(Self::PointerButton {
                    button: PointerButton::from_wire(header.a).ok_or(invalid())?,
                    pressed: header.state == 1,
                    sequence: field(InputSequence::new(header.c), invalid())?,
                })
            }
            CommandKind::PointerScroll => {
                if header.state != 0 || !text.is_empty() {
                    return Err(invalid());
                }
                Ok(Self::PointerScroll {
                    dx: field(PointerDelta::new(f32::from_bits(header.a)), invalid())?,
                    dy: field(PointerDelta::new(f32::from_bits(header.b)), invalid())?,
                    sequence: field(InputSequence::new(header.c), invalid())?,
                })
            }
            CommandKind::KeyboardKey => {
                if header.state > 2 || header.b != 0 || !text.is_empty() {
                    return Err(invalid());
                }
                Ok(Self::KeyboardKey {
                    key: field(KeyCode::new(header.a), invalid())?,
                    state: match header.state {
                        0 => KeyState::Released,
                        1 => KeyState::Pressed,
                        2 => KeyState::Repeated,
                        _ => return Err(invalid()),
                    },
                    sequence: field(InputSequence::new(header.c), invalid())?,
                })
            }
            CommandKind::ReleaseAll => {
                if header.state != 0
                    || header.a != 0
                    || header.b != 0
                    || header.c != 0
                    || !text.is_empty()
                {
                    return Err(invalid());
                }
                Ok(Self::ReleaseAll)
            }
            CommandKind::Resize => {
                if header.state != 0 || !text.is_empty() {
                    return Err(invalid());
                }
                Ok(Self::Resize {
                    size: field(FrameSize::new(header.a, header.b), invalid())?,
                    scale_v120: field(ScaleV120::new((header.c & 0xffff) as u16), invalid())?,
                    request_id: field(RequestId::new((header.c >> 16) as u16), invalid())?,
                })
            }
            CommandKind::Clipboard => {
                if header.state != 0 || header.b != 0 || header.c != 0 {
                    return Err(invalid());
                }
                let text = String::from_utf8(text.to_vec())
                    .map_err(|_utf8_error| ProtocolError::NotUtf8)?;
                Ok(Self::Clipboard(field(ClipboardText::new(text), invalid())?))
            }
            CommandKind::PointerRelative => {
                if header.state != 0 || !text.is_empty() {
                    return Err(invalid());
                }
                Ok(Self::PointerRelative {
                    dx: field(PointerDelta::new(f32::from_bits(header.a)), invalid())?,
                    dy: field(PointerDelta::new(f32::from_bits(header.b)), invalid())?,
                    sequence: field(InputSequence::new(header.c), invalid())?,
                })
            }
            CommandKind::Quality => {
                if header.state != 0 || !text.is_empty() {
                    return Err(invalid());
                }
                Ok(Self::Quality {
                    bitrate_kbps: field(Kbps::new(header.a), invalid())?,
                    fps: field(Fps::new(header.b), invalid())?,
                    scale_percent: field(ScalePercent::new(header.c), invalid())?,
                })
            }
            CommandKind::Text => {
                if header.state > 1 || header.c != 0 {
                    return Err(invalid());
                }
                let text = String::from_utf8(text.to_vec())
                    .map_err(|_utf8_error| ProtocolError::NotUtf8)?;
                Ok(Self::Text {
                    action: if header.state == 1 {
                        TextAction::Preedit
                    } else {
                        TextAction::Commit
                    },
                    text: field(InputText::new(text), invalid())?,
                    sequence: field(InputSequence::new(header.b), invalid())?,
                })
            }
            CommandKind::KeyframeReadiness => {
                if header.state > 1 || header.b != 0 || header.c != 0 || !text.is_empty() {
                    return Err(invalid());
                }
                Ok(Self::KeyframeReadiness {
                    generation: field(Generation::new(header.a), invalid())?,
                    ready: header.state == 1,
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum CommandKind {
    PointerAbsolute = 1,
    PointerButton = 2,
    PointerScroll = 3,
    KeyboardKey = 4,
    ReleaseAll = 5,
    Resize = 6,
    Clipboard = 7,
    PointerRelative = 8,
    Quality = 9,
    Text = 10,
    KeyframeReadiness = 11,
}

impl CommandKind {
    const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::PointerAbsolute),
            2 => Some(Self::PointerButton),
            3 => Some(Self::PointerScroll),
            4 => Some(Self::KeyboardKey),
            5 => Some(Self::ReleaseAll),
            6 => Some(Self::Resize),
            7 => Some(Self::Clipboard),
            8 => Some(Self::PointerRelative),
            9 => Some(Self::Quality),
            10 => Some(Self::Text),
            11 => Some(Self::KeyframeReadiness),
            _ => None,
        }
    }

    const fn wire(self) -> u8 {
        self as u8
    }

    const fn payload_limit(self) -> Option<usize> {
        match self {
            Self::Clipboard => Some(MAX_CLIPBOARD_BYTES),
            Self::Text => Some(MAX_TEXT_BYTES),
            Self::PointerAbsolute
            | Self::PointerButton
            | Self::PointerScroll
            | Self::KeyboardKey
            | Self::ReleaseAll
            | Self::Resize
            | Self::PointerRelative
            | Self::Quality
            | Self::KeyframeReadiness => None,
        }
    }

    const fn browser_input(self) -> bool {
        matches!(
            self,
            Self::PointerAbsolute
                | Self::PointerButton
                | Self::PointerScroll
                | Self::KeyboardKey
                | Self::ReleaseAll
                | Self::Resize
                | Self::PointerRelative
        )
    }
}

/// A 16-byte header: version at 0, kind at 1, state at 2, zero at 3, `a` at 4, `b` at 8, and `c` at 12.
///
/// What `state`, `a`, `b`, and `c` hold for each kind:
///
/// | kind               | state     | a              | b        | c                               |
/// |--------------------|-----------|----------------|----------|---------------------------------|
/// | pointer absolute   | 0         | x              | y        | sequence                        |
/// | pointer button     | pressed   | button         | 0        | sequence                        |
/// | pointer scroll     | 0         | dx bits        | dy bits  | sequence                        |
/// | keyboard key       | key state | key            | 0        | sequence                        |
/// | release all        | 0         | 0              | 0        | 0                               |
/// | resize             | 0         | width          | height   | scale (low 16), request (high 16) |
/// | clipboard          | 0         | payload length | 0        | 0                               |
/// | pointer relative   | 0         | dx bits        | dy bits  | sequence                        |
/// | quality            | 0         | bitrate        | fps      | scale percent                   |
/// | text               | action    | payload length | sequence | 0                               |
/// | keyframe readiness | ready     | generation     | 0        | 0                               |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandHeader {
    kind: CommandKind,
    state: u8,
    a: u32,
    b: u32,
    c: u32,
    text_payload_len: Option<usize>,
}

impl CommandHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() != COMMAND_HEADER_BYTES || bytes[0] != PROTOCOL_VERSION || bytes[3] != 0 {
            return Err(ProtocolError::InvalidHeader);
        }
        let kind =
            CommandKind::from_wire(bytes[1]).ok_or(ProtocolError::InvalidFields(bytes[1]))?;
        let a = le32(&bytes[4..8]);
        let text_payload_len = kind
            .payload_limit()
            .map(|limit| checked_payload_len(a, limit))
            .transpose()?;
        Ok(Self {
            kind,
            state: bytes[2],
            a,
            b: le32(&bytes[8..12]),
            c: le32(&bytes[12..16]),
            text_payload_len,
        })
    }

    #[must_use]
    pub(crate) const fn wire_kind(self) -> u8 {
        self.kind.wire()
    }

    #[must_use]
    pub(crate) const fn is_browser_input(self) -> bool {
        self.kind.browser_input()
    }

    #[must_use]
    pub const fn text_payload_len(self) -> Option<usize> {
        self.text_payload_len
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameMetadata {
    pub generation: Generation,
    pub width: FrameDimension,
    pub height: FrameDimension,
    pub capture_nanos: u64,
    pub sequence: u64,
    pub input_sequence: Option<InputSequence>,
    pub fps: Fps,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    size: CursorSize,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    bgra: Vec<u8>,
}

impl CursorImage {
    pub fn new(
        size: CursorSize,
        hotspot_x: i32,
        hotspot_y: i32,
        bgra: Vec<u8>,
    ) -> Result<Self, InvalidValue> {
        if bgra.len() != size.byte_count() {
            return Err(InvalidValue("cursor image bytes must match its size"));
        }
        Ok(Self {
            size,
            hotspot_x,
            hotspot_y,
            bgra,
        })
    }

    #[must_use]
    pub const fn size(&self) -> CursorSize {
        self.size
    }

    #[must_use]
    pub fn bgra(&self) -> &[u8] {
        &self.bgra
    }

    #[must_use]
    pub fn into_bgra(self) -> Vec<u8> {
        self.bgra
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// UTF-8 clipboard bytes starting at payload offset 0.
    Clipboard(ClipboardText),
    /// A 32-byte payload: generation at 0, width at 4, height at 6, capture nanoseconds at 8,
    /// sequence at 16, input sequence at 24, and FPS at 28.
    Frame(FrameMetadata),
    /// A 20-byte payload: request ID at 0, zero at 2, width at 4, height at 8, scale at 12,
    /// zero at 14, and generation at 16.
    ResizeApplied {
        request_id: RequestId,
        size: FrameSize,
        scale_v120: ScaleV120,
        generation: Generation,
    },
    /// A payload with width at 0, height at 4, hotspot x at 8, hotspot y at 12, and BGRA bytes at 16.
    CursorImage(CursorImage),
    /// A one-byte payload at offset 0: 0 means hidden and 1 means visible.
    CursorVisibility(bool),
}

impl Event {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let (kind, payload) = match self {
            Self::Clipboard(text) => (EventKind::Clipboard, text.as_str().as_bytes().to_vec()),
            Self::Frame(frame) => {
                let mut payload = vec![0; 32];
                payload[0..4].copy_from_slice(&frame.generation.get().to_le_bytes());
                payload[4..6].copy_from_slice(&frame.width.get().to_le_bytes());
                payload[6..8].copy_from_slice(&frame.height.get().to_le_bytes());
                payload[8..16].copy_from_slice(&frame.capture_nanos.to_le_bytes());
                payload[16..24].copy_from_slice(&frame.sequence.to_le_bytes());
                payload[24..28].copy_from_slice(
                    &frame
                        .input_sequence
                        .map_or(0, InputSequence::get)
                        .to_le_bytes(),
                );
                payload[28..32].copy_from_slice(&frame.fps.get().to_le_bytes());
                (EventKind::Frame, payload)
            }
            Self::ResizeApplied {
                request_id,
                size,
                scale_v120,
                generation,
            } => {
                let mut payload = vec![0; 20];
                payload[0..2].copy_from_slice(&request_id.get().to_le_bytes());
                payload[4..8].copy_from_slice(&size.width().to_le_bytes());
                payload[8..12].copy_from_slice(&size.height().to_le_bytes());
                payload[12..14].copy_from_slice(&scale_v120.get().to_le_bytes());
                payload[16..20].copy_from_slice(&generation.get().to_le_bytes());
                (EventKind::ResizeApplied, payload)
            }
            Self::CursorImage(image) => {
                let size = image.size();
                let mut payload = Vec::with_capacity(16 + image.bgra().len());
                payload.extend_from_slice(&size.width().to_le_bytes());
                payload.extend_from_slice(&size.height().to_le_bytes());
                payload.extend_from_slice(&image.hotspot_x.to_le_bytes());
                payload.extend_from_slice(&image.hotspot_y.to_le_bytes());
                payload.extend_from_slice(image.bgra());
                (EventKind::CursorImage, payload)
            }
            Self::CursorVisibility(visible) => {
                (EventKind::CursorVisibility, vec![u8::from(*visible)])
            }
        };
        let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        let mut bytes = Vec::with_capacity(EVENT_HEADER_BYTES + payload.len());
        bytes.extend_from_slice(&[PROTOCOL_VERSION, kind.wire(), 0, 0]);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    pub fn decode(header: EventHeader, payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() != header.payload_len {
            return Err(ProtocolError::Truncated);
        }
        let invalid = || ProtocolError::InvalidEvent(header.kind.wire());
        match header.kind {
            EventKind::Clipboard => {
                let text = String::from_utf8(payload.to_vec())
                    .map_err(|_utf8_error| ProtocolError::NotUtf8)?;
                Ok(Self::Clipboard(field(ClipboardText::new(text), invalid())?))
            }
            EventKind::Frame => {
                if payload.len() != 32 {
                    return Err(invalid());
                }
                Ok(Self::Frame(FrameMetadata {
                    generation: field(Generation::new(le32(&payload[0..4])), invalid())?,
                    width: field(FrameDimension::new(le16(&payload[4..6])), invalid())?,
                    height: field(FrameDimension::new(le16(&payload[6..8])), invalid())?,
                    capture_nanos: le64(&payload[8..16]),
                    sequence: le64(&payload[16..24]),
                    input_sequence: match le32(&payload[24..28]) {
                        0 => None,
                        value => Some(field(InputSequence::new(value), invalid())?),
                    },
                    fps: field(Fps::new(le32(&payload[28..32])), invalid())?,
                }))
            }
            EventKind::ResizeApplied => {
                if payload.len() != 20 || payload[2..4] != [0, 0] || payload[14..16] != [0, 0] {
                    return Err(invalid());
                }
                Ok(Self::ResizeApplied {
                    request_id: field(RequestId::new(le16(&payload[0..2])), invalid())?,
                    size: field(
                        FrameSize::new(le32(&payload[4..8]), le32(&payload[8..12])),
                        invalid(),
                    )?,
                    scale_v120: field(ScaleV120::new(le16(&payload[12..14])), invalid())?,
                    generation: field(Generation::new(le32(&payload[16..20])), invalid())?,
                })
            }
            EventKind::CursorImage => {
                if payload.len() < 16 {
                    return Err(invalid());
                }
                let size = field(
                    CursorSize::new(le32(&payload[0..4]), le32(&payload[4..8])),
                    invalid(),
                )?;
                let image = CursorImage::new(
                    size,
                    le32(&payload[8..12]).cast_signed(),
                    le32(&payload[12..16]).cast_signed(),
                    payload[16..].to_vec(),
                );
                Ok(Self::CursorImage(field(image, invalid())?))
            }
            EventKind::CursorVisibility => {
                if payload != [0] && payload != [1] {
                    return Err(invalid());
                }
                Ok(Self::CursorVisibility(payload[0] == 1))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum EventKind {
    Clipboard = 1,
    Frame = 2,
    ResizeApplied = 3,
    CursorImage = 4,
    CursorVisibility = 5,
}

impl EventKind {
    const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Clipboard),
            2 => Some(Self::Frame),
            3 => Some(Self::ResizeApplied),
            4 => Some(Self::CursorImage),
            5 => Some(Self::CursorVisibility),
            _ => None,
        }
    }

    const fn wire(self) -> u8 {
        self as u8
    }
}

/// An 8-byte header: version at 0, kind at 1, zero at 2..4, and payload length at 4..8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventHeader {
    kind: EventKind,
    payload_len: usize,
}

impl EventHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() != EVENT_HEADER_BYTES
            || bytes[0] != PROTOCOL_VERSION
            || bytes[2] != 0
            || bytes[3] != 0
        {
            return Err(ProtocolError::InvalidHeader);
        }
        Ok(Self {
            kind: EventKind::from_wire(bytes[1]).ok_or(ProtocolError::InvalidEvent(bytes[1]))?,
            payload_len: checked_payload_len(le32(&bytes[4..8]), MAX_EVENT_BYTES)?,
        })
    }

    #[must_use]
    pub const fn payload_len(self) -> usize {
        self.payload_len
    }
}

fn checked_payload_len(length: u32, limit: usize) -> Result<usize, ProtocolError> {
    let length = usize::try_from(length).map_err(|_overflow| ProtocolError::PayloadTooLarge)?;
    if length > limit {
        Err(ProtocolError::PayloadTooLarge)
    } else {
        Ok(length)
    }
}

fn field<T>(result: Result<T, InvalidValue>, invalid: ProtocolError) -> Result<T, ProtocolError> {
    result.map_err(|_invalid_value| invalid)
}

fn command(kind: CommandKind, state: u8, a: u32, b: u32, c: u32) -> Vec<u8> {
    let mut bytes = vec![PROTOCOL_VERSION, kind.wire(), state, 0];
    bytes.extend(a.to_le_bytes());
    bytes.extend(b.to_le_bytes());
    bytes.extend(c.to_le_bytes());
    bytes
}

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

#[cfg(test)]
mod tests;
