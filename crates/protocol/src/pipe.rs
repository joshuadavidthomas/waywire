use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

pub const VERSION: u8 = 2;
pub const COMMAND_HEADER_BYTES: usize = 16;
pub const EVENT_HEADER_BYTES: usize = 8;
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 4_000;
pub const MAX_EVENT_BYTES: usize = MAX_CLIPBOARD_BYTES;
pub const MAX_RAW_PIXELS: u64 = 3840 * 2160;

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
    #[error("record exceeds the byte limit")]
    TooLarge,
    #[error("record payload is not valid text")]
    InvalidText,
    #[error("record payload is not valid UTF-8")]
    Utf8,
    #[error("invalid protocol value: {0}")]
    InvalidValue(&'static str),
}

macro_rules! ranged_newtype {
    ($name:ident, $inner:ty, $minimum:expr, $maximum:expr, $label:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name($inner);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = <$inner>::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }

        impl $name {
            pub fn new(value: $inner) -> Result<Self, ProtocolError> {
                if ($minimum..=$maximum).contains(&value) {
                    Ok(Self(value))
                } else {
                    Err(ProtocolError::InvalidValue($label))
                }
            }

            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

ranged_newtype!(Generation, u32, 1, u32::MAX, "generation must be nonzero");
ranged_newtype!(RequestId, u16, 1, u16::MAX, "request ID must be nonzero");
ranged_newtype!(
    InputSequence,
    u32,
    1,
    u32::MAX,
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
    PointerButton,
    u32,
    0x110,
    0x114,
    "pointer button must be between 0x110 and 0x114"
);
ranged_newtype!(
    FrameDimension,
    u16,
    1,
    u16::MAX,
    "frame dimension must be nonzero"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSize {
    width: u32,
    height: u32,
}

impl FrameSize {
    pub fn new(width: u32, height: u32) -> Result<Self, ProtocolError> {
        let pixels = u64::from(width) * u64::from(height);
        if (320..=6_000).contains(&width)
            && (180..=6_000).contains(&height)
            && width.is_multiple_of(2)
            && height.is_multiple_of(2)
            && pixels <= MAX_RAW_PIXELS
        {
            Ok(Self { width, height })
        } else {
            Err(ProtocolError::InvalidValue(
                "frame size is outside the supported range",
            ))
        }
    }

    #[must_use]
    pub const fn get(self) -> (u32, u32) {
        (self.width, self.height)
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
    pub fn new(width: u32, height: u32) -> Result<Self, ProtocolError> {
        if (1..=256).contains(&width) && (1..=256).contains(&height) {
            Ok(Self { width, height })
        } else {
            Err(ProtocolError::InvalidValue(
                "cursor dimensions must be between 1 and 256",
            ))
        }
    }

    #[must_use]
    pub const fn get(self) -> (u32, u32) {
        (self.width, self.height)
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardText(String);

impl<'de> Deserialize<'de> for ClipboardText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        Self::new(text).map_err(serde::de::Error::custom)
    }
}

impl ClipboardText {
    pub fn new(text: String) -> Result<Self, ProtocolError> {
        if text.len() <= MAX_CLIPBOARD_BYTES {
            Ok(Self(text))
        } else {
            Err(ProtocolError::PayloadTooLarge)
        }
    }

    #[must_use]
    pub fn get(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputText(String);

impl InputText {
    pub fn new(text: String) -> Result<Self, ProtocolError> {
        if text.len() > MAX_TEXT_BYTES {
            Err(ProtocolError::PayloadTooLarge)
        } else if text.contains('\0') {
            Err(ProtocolError::InvalidText)
        } else {
            Ok(Self(text))
        }
    }

    #[must_use]
    pub fn get(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerDelta(f32);

impl Eq for PointerDelta {}

impl PointerDelta {
    pub fn new(value: f32) -> Result<Self, ProtocolError> {
        if value.is_finite() && (-4096.0..=4096.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ProtocolError::InvalidValue(
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
                Self::Clipboard(text) => text.get().len(),
                Self::Text { text, .. } => text.get().len(),
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
                button.get(),
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
                let (width, height) = size.get();
                let packed = u32::from(scale_v120.get()) | (u32::from(request_id.get()) << 16);
                command(CommandKind::Resize, 0, width, height, packed)
            }
            Self::Clipboard(text) => {
                let length = u32::try_from(text.get().len()).unwrap_or(u32::MAX);
                let mut bytes = command(CommandKind::Clipboard, 0, length, 0, 0);
                bytes.extend_from_slice(text.get().as_bytes());
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
                let length = u32::try_from(text.get().len()).unwrap_or(u32::MAX);
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
                bytes.extend_from_slice(text.get().as_bytes());
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
        let sequence = |value| InputSequence::new(value).map_err(|_value_error| invalid());
        match header.kind {
            CommandKind::PointerAbsolute if header.state == 0 && text.is_empty() => {
                Ok(Self::PointerAbsolute {
                    x: PointerCoordinate::new(header.a).map_err(|_value_error| invalid())?,
                    y: PointerCoordinate::new(header.b).map_err(|_value_error| invalid())?,
                    sequence: sequence(header.c)?,
                })
            }
            CommandKind::PointerButton if header.state <= 1 && header.b == 0 && text.is_empty() => {
                Ok(Self::PointerButton {
                    button: PointerButton::new(header.a).map_err(|_value_error| invalid())?,
                    pressed: header.state == 1,
                    sequence: sequence(header.c)?,
                })
            }
            CommandKind::PointerScroll if header.state == 0 && text.is_empty() => {
                Ok(Self::PointerScroll {
                    dx: PointerDelta::new(f32::from_bits(header.a))
                        .map_err(|_value_error| invalid())?,
                    dy: PointerDelta::new(f32::from_bits(header.b))
                        .map_err(|_value_error| invalid())?,
                    sequence: sequence(header.c)?,
                })
            }
            CommandKind::KeyboardKey if header.state <= 2 && header.b == 0 && text.is_empty() => {
                Ok(Self::KeyboardKey {
                    key: KeyCode::new(header.a).map_err(|_value_error| invalid())?,
                    state: match header.state {
                        0 => KeyState::Released,
                        1 => KeyState::Pressed,
                        2 => KeyState::Repeated,
                        _ => return Err(invalid()),
                    },
                    sequence: sequence(header.c)?,
                })
            }
            CommandKind::ReleaseAll
                if header.state == 0
                    && header.a == 0
                    && header.b == 0
                    && header.c == 0
                    && text.is_empty() =>
            {
                Ok(Self::ReleaseAll)
            }
            CommandKind::Resize if header.state == 0 && text.is_empty() => Ok(Self::Resize {
                size: FrameSize::new(header.a, header.b).map_err(|_value_error| invalid())?,
                scale_v120: ScaleV120::new((header.c & 0xffff) as u16)
                    .map_err(|_value_error| invalid())?,
                request_id: RequestId::new((header.c >> 16) as u16)
                    .map_err(|_value_error| invalid())?,
            }),
            CommandKind::Clipboard if header.state == 0 && header.b == 0 && header.c == 0 => {
                Ok(Self::Clipboard(ClipboardText::new(
                    String::from_utf8(text.to_vec())
                        .map_err(|_utf8_error| ProtocolError::InvalidText)?,
                )?))
            }
            CommandKind::PointerRelative if header.state == 0 && text.is_empty() => {
                Ok(Self::PointerRelative {
                    dx: PointerDelta::new(f32::from_bits(header.a))
                        .map_err(|_value_error| invalid())?,
                    dy: PointerDelta::new(f32::from_bits(header.b))
                        .map_err(|_value_error| invalid())?,
                    sequence: sequence(header.c)?,
                })
            }
            CommandKind::Quality if header.state == 0 && text.is_empty() => Ok(Self::Quality {
                bitrate_kbps: Kbps::new(header.a).map_err(|_value_error| invalid())?,
                fps: Fps::new(header.b).map_err(|_value_error| invalid())?,
                scale_percent: ScalePercent::new(header.c).map_err(|_value_error| invalid())?,
            }),
            CommandKind::Text if header.state <= 1 && header.c == 0 => Ok(Self::Text {
                action: if header.state == 1 {
                    TextAction::Preedit
                } else {
                    TextAction::Commit
                },
                text: InputText::new(
                    String::from_utf8(text.to_vec())
                        .map_err(|_utf8_error| ProtocolError::InvalidText)?,
                )?,
                sequence: sequence(header.b)?,
            }),
            CommandKind::KeyframeReadiness
                if header.state <= 1 && header.b == 0 && header.c == 0 && text.is_empty() =>
            {
                Ok(Self::KeyframeReadiness {
                    generation: Generation::new(header.a).map_err(|_value_error| invalid())?,
                    ready: header.state == 1,
                })
            }
            CommandKind::PointerAbsolute
            | CommandKind::PointerButton
            | CommandKind::PointerScroll
            | CommandKind::KeyboardKey
            | CommandKind::ReleaseAll
            | CommandKind::Resize
            | CommandKind::Clipboard
            | CommandKind::PointerRelative
            | CommandKind::Quality
            | CommandKind::Text
            | CommandKind::KeyframeReadiness => Err(invalid()),
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
    const ALL: [Self; 11] = [
        Self::PointerAbsolute,
        Self::PointerButton,
        Self::PointerScroll,
        Self::KeyboardKey,
        Self::ReleaseAll,
        Self::Resize,
        Self::Clipboard,
        Self::PointerRelative,
        Self::Quality,
        Self::Text,
        Self::KeyframeReadiness,
    ];

    fn from_wire(value: u8) -> Option<Self> {
        let index = usize::from(value.checked_sub(1)?);
        Self::ALL
            .get(index)
            .copied()
            .filter(|kind| kind.wire() == value)
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
        if bytes.len() != COMMAND_HEADER_BYTES || bytes[0] != VERSION || bytes[3] != 0 {
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
    pub const fn wire_kind(self) -> u8 {
        self.kind.wire()
    }

    #[must_use]
    pub const fn is_browser_input(self) -> bool {
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
pub enum Event {
    Clipboard(ClipboardText),
    Frame(FrameMetadata),
    ResizeApplied {
        request_id: RequestId,
        size: FrameSize,
        scale_v120: ScaleV120,
        generation: Generation,
    },
    CursorImage {
        size: CursorSize,
        hotspot_x: i32,
        hotspot_y: i32,
        bgra: Vec<u8>,
    },
    CursorVisibility(bool),
}

impl Event {
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let (kind, payload) = match self {
            Self::Clipboard(text) => (EventKind::Clipboard, text.get().as_bytes().to_vec()),
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
            Self::CursorImage {
                size,
                hotspot_x,
                hotspot_y,
                bgra,
            } => {
                if bgra.len() != size.byte_count() {
                    return Err(ProtocolError::InvalidEvent(EventKind::CursorImage.wire()));
                }
                let mut payload = Vec::with_capacity(16 + bgra.len());
                payload.extend_from_slice(&size.width().to_le_bytes());
                payload.extend_from_slice(&size.height().to_le_bytes());
                payload.extend_from_slice(&hotspot_x.to_le_bytes());
                payload.extend_from_slice(&hotspot_y.to_le_bytes());
                payload.extend_from_slice(bgra);
                (EventKind::CursorImage, payload)
            }
            Self::CursorVisibility(visible) => {
                (EventKind::CursorVisibility, vec![u8::from(*visible)])
            }
        };
        let length =
            u32::try_from(payload.len()).map_err(|_overflow| ProtocolError::PayloadTooLarge)?;
        let mut bytes = Vec::with_capacity(EVENT_HEADER_BYTES + payload.len());
        bytes.extend_from_slice(&[VERSION, kind.wire(), 0, 0]);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    pub fn decode(header: EventHeader, payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() != header.payload_len {
            return Err(ProtocolError::Truncated);
        }
        let invalid = || ProtocolError::InvalidEvent(header.kind.wire());
        match header.kind {
            EventKind::Clipboard => Ok(Self::Clipboard(ClipboardText::new(
                String::from_utf8(payload.to_vec()).map_err(|_utf8_error| ProtocolError::Utf8)?,
            )?)),
            EventKind::Frame if payload.len() == 32 => Ok(Self::Frame(FrameMetadata {
                generation: Generation::new(le32(&payload[0..4]))
                    .map_err(|_value_error| invalid())?,
                width: FrameDimension::new(le16(&payload[4..6]))
                    .map_err(|_value_error| invalid())?,
                height: FrameDimension::new(le16(&payload[6..8]))
                    .map_err(|_value_error| invalid())?,
                capture_nanos: le64(&payload[8..16]),
                sequence: le64(&payload[16..24]),
                input_sequence: match le32(&payload[24..28]) {
                    0 => None,
                    value => Some(InputSequence::new(value).map_err(|_value_error| invalid())?),
                },
                fps: Fps::new(le32(&payload[28..32])).map_err(|_value_error| invalid())?,
            })),
            EventKind::ResizeApplied
                if payload.len() == 20 && payload[2..4] == [0, 0] && payload[14..16] == [0, 0] =>
            {
                Ok(Self::ResizeApplied {
                    request_id: RequestId::new(le16(&payload[0..2]))
                        .map_err(|_value_error| invalid())?,
                    size: FrameSize::new(le32(&payload[4..8]), le32(&payload[8..12]))
                        .map_err(|_value_error| invalid())?,
                    scale_v120: ScaleV120::new(le16(&payload[12..14]))
                        .map_err(|_value_error| invalid())?,
                    generation: Generation::new(le32(&payload[16..20]))
                        .map_err(|_value_error| invalid())?,
                })
            }
            EventKind::CursorImage if payload.len() >= 16 => {
                let size = CursorSize::new(le32(&payload[0..4]), le32(&payload[4..8]))
                    .map_err(|_value_error| invalid())?;
                if payload.len() != 16 + size.byte_count() {
                    return Err(invalid());
                }
                Ok(Self::CursorImage {
                    size,
                    hotspot_x: le32(&payload[8..12]).cast_signed(),
                    hotspot_y: le32(&payload[12..16]).cast_signed(),
                    bgra: payload[16..].to_vec(),
                })
            }
            EventKind::CursorVisibility if payload == [0] || payload == [1] => {
                Ok(Self::CursorVisibility(payload[0] == 1))
            }
            EventKind::Frame
            | EventKind::ResizeApplied
            | EventKind::CursorImage
            | EventKind::CursorVisibility => Err(invalid()),
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
    const ALL: [Self; 5] = [
        Self::Clipboard,
        Self::Frame,
        Self::ResizeApplied,
        Self::CursorImage,
        Self::CursorVisibility,
    ];

    fn from_wire(value: u8) -> Option<Self> {
        let index = usize::from(value.checked_sub(1)?);
        Self::ALL
            .get(index)
            .copied()
            .filter(|kind| kind.wire() == value)
    }

    const fn wire(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventHeader {
    kind: EventKind,
    payload_len: usize,
}

impl EventHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() != EVENT_HEADER_BYTES
            || bytes[0] != VERSION
            || bytes[2] != 0
            || bytes[3] != 0
        {
            return Err(ProtocolError::InvalidHeader);
        }
        Ok(Self {
            kind: EventKind::from_wire(bytes[1]).ok_or(ProtocolError::InvalidEvent(bytes[1]))?,
            payload_len: checked_event_payload_len(le32(&bytes[4..8]))?,
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

fn checked_event_payload_len(length: u32) -> Result<usize, ProtocolError> {
    let length = usize::try_from(length).map_err(|_overflow| ProtocolError::TooLarge)?;
    if length > MAX_EVENT_BYTES {
        Err(ProtocolError::TooLarge)
    } else {
        Ok(length)
    }
}

fn command(kind: CommandKind, state: u8, a: u32, b: u32, c: u32) -> Vec<u8> {
    let mut bytes = vec![VERSION, kind.wire(), state, 0];
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
