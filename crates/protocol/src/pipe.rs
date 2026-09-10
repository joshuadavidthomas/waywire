//! Gateway-to-streamd pipe wire format, version 2.
//! Values use little-endian byte order. Command records have a 16-byte header plus an optional
//! text payload, while event records have an 8-byte header plus a payload.

use std::marker::PhantomData;
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

pub(crate) trait Wire: Sized {
    fn write(&self, out: &mut Writer);
    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue>;
}

pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], InvalidValue> {
        let (head, tail) = self
            .0
            .split_first_chunk::<N>()
            .ok_or(InvalidValue("payload is too short"))?;
        self.0 = tail;
        Ok(*head)
    }

    fn get<T: Wire>(&mut self) -> Result<T, InvalidValue> {
        T::read(self)
    }

    fn reserved<const N: usize>(&mut self) -> Result<(), InvalidValue> {
        if self.take::<N>()?.iter().all(|byte| *byte == 0) {
            Ok(())
        } else {
            Err(InvalidValue("reserved bytes must be zero"))
        }
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = self.0;
        self.0 = &[];
        rest
    }

    fn rest_utf8(&mut self) -> Result<&'a str, InvalidValue> {
        let Ok(text) = std::str::from_utf8(self.rest()) else {
            return Err(InvalidValue("text is not UTF-8"));
        };
        Ok(text)
    }

    fn finish(&self) -> Result<(), InvalidValue> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(InvalidValue("payload has trailing bytes"))
        }
    }
}

pub(crate) struct Writer(Vec<u8>);

impl Writer {
    #[must_use]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    pub(crate) fn put<T: Wire>(&mut self, value: &T) {
        value.write(self);
    }

    pub(crate) fn reserved<const N: usize>(&mut self) {
        self.0.extend_from_slice(&[0; N]);
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    /// Writes a payload length. Payloads are bounded where their values are constructed
    /// (`MAX_CLIPBOARD_BYTES`, `MAX_TEXT_BYTES`, `CursorSize`), so every length fits the slot.
    pub(crate) fn length(&mut self, length: usize) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "payload lengths are bounded at construction"
        )]
        let length = length as u32;
        self.put(&length);
    }

    #[must_use]
    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

impl Wire for u8 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

impl Wire for u16 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

impl Wire for u32 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

impl Wire for i32 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

impl Wire for u64 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("pipe stream ended halfway through a record")]
    Truncated,
    #[error("record header is invalid")]
    InvalidHeader,
    #[error("command {kind} is invalid: {reason}")]
    InvalidFields { kind: u8, reason: InvalidValue },
    #[error("event {kind} is invalid: {reason}")]
    InvalidEvent { kind: u8, reason: InvalidValue },
    #[error("record payload exceeds its limit")]
    PayloadTooLarge,
}

pub trait Record: Sized {
    const HEADER_BYTES: usize;

    /// Total record length, given exactly `HEADER_BYTES` bytes.
    fn record_len(header: &[u8]) -> Result<usize, ProtocolError>;

    fn decode(record: &[u8]) -> Result<Self, ProtocolError>;

    fn encode(&self) -> Vec<u8>;
}

pub struct Decoder<R: Record> {
    buffer: Vec<u8>,
    record: PhantomData<R>,
}

impl<R: Record> Decoder<R> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            record: PhantomData,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<R>, ProtocolError> {
        self.buffer.extend_from_slice(bytes);
        let mut records = Vec::new();
        loop {
            if self.buffer.len() < R::HEADER_BYTES {
                break;
            }
            let record_len = R::record_len(&self.buffer[..R::HEADER_BYTES])?;
            if self.buffer.len() < record_len {
                break;
            }
            records.push(R::decode(&self.buffer[..record_len])?);
            drop(self.buffer.drain(..record_len));
        }
        Ok(records)
    }

    /// Bytes of the record still being received, zero when none.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }

    /// Errors with `ProtocolError::Truncated` if a record is half received.
    pub fn finish(self) -> Result<(), ProtocolError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::Truncated)
        }
    }
}

impl<R: Record> Default for Decoder<R> {
    fn default() -> Self {
        Self::new()
    }
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

        impl Wire for $name {
            fn write(&self, out: &mut Writer) {
                out.put(&self.get());
            }

            fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
                Self::new(input.get()?)
            }
        }

        impl Wire for Option<$name> {
            fn write(&self, out: &mut Writer) {
                out.put(&self.map_or(0, $name::get));
            }

            fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
                match input.get()? {
                    0 => Ok(None),
                    value => $name::new(value).map(Some),
                }
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

        impl Wire for $name {
            fn write(&self, out: &mut Writer) {
                out.put(&self.get());
            }

            fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
                Self::new(input.get()?)
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

impl Wire for FrameSize {
    fn write(&self, out: &mut Writer) {
        out.put(&self.width);
        out.put(&self.height);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Self::new(input.get()?, input.get()?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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

impl Wire for CursorSize {
    fn write(&self, out: &mut Writer) {
        out.put(&self.width);
        out.put(&self.height);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Self::new(input.get()?, input.get()?)
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

impl Wire for PointerDelta {
    fn write(&self, out: &mut Writer) {
        out.put(&self.0.to_bits());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Self::new(f32::from_bits(input.get()?))
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
    /// The Linux input event code, which is also the wire value.
    #[must_use]
    pub const fn evdev_code(self) -> u32 {
        match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
            Self::Side => 0x113,
            Self::Extra => 0x114,
        }
    }
}

impl Wire for PointerButton {
    fn write(&self, out: &mut Writer) {
        out.put(&self.evdev_code());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0x110_u32 => Ok(Self::Left),
            0x111 => Ok(Self::Right),
            0x112 => Ok(Self::Middle),
            0x113 => Ok(Self::Side),
            0x114 => Ok(Self::Extra),
            _ => Err(InvalidValue("unknown pointer button")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    Released,
    Pressed,
    Repeated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonState {
    Released,
    Pressed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyframeState {
    Missing,
    Cached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorVisibility {
    Hidden,
    Visible,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TextAction {
    Preedit,
    Commit,
}

impl Wire for ButtonState {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Released => 0_u8,
            Self::Pressed => 1,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self::Released),
            1 => Ok(Self::Pressed),
            _ => Err(InvalidValue("state must be zero or one")),
        }
    }
}

impl Wire for KeyState {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Released => 0_u8,
            Self::Pressed => 1,
            Self::Repeated => 2,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self::Released),
            1 => Ok(Self::Pressed),
            2 => Ok(Self::Repeated),
            _ => Err(InvalidValue("state must be zero, one, or two")),
        }
    }
}

impl Wire for KeyframeState {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Missing => 0_u8,
            Self::Cached => 1,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self::Missing),
            1 => Ok(Self::Cached),
            _ => Err(InvalidValue("state must be zero or one")),
        }
    }
}

impl Wire for CursorVisibility {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Hidden => 0_u8,
            Self::Visible => 1,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self::Hidden),
            1 => Ok(Self::Visible),
            _ => Err(InvalidValue("visibility must be zero or one")),
        }
    }
}

impl Wire for TextAction {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Commit => 0_u8,
            Self::Preedit => 1,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self::Commit),
            1 => Ok(Self::Preedit),
            _ => Err(InvalidValue("action must be zero or one")),
        }
    }
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
        state: ButtonState,
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
        state: KeyframeState,
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

    #[must_use]
    pub(crate) fn kind(&self) -> CommandKind {
        match self {
            Self::PointerAbsolute { .. } => CommandKind::PointerAbsolute,
            Self::PointerButton { .. } => CommandKind::PointerButton,
            Self::PointerScroll { .. } => CommandKind::PointerScroll,
            Self::KeyboardKey { .. } => CommandKind::KeyboardKey,
            Self::ReleaseAll => CommandKind::ReleaseAll,
            Self::Resize { .. } => CommandKind::Resize,
            Self::Clipboard(_) => CommandKind::Clipboard,
            Self::PointerRelative { .. } => CommandKind::PointerRelative,
            Self::Quality { .. } => CommandKind::Quality,
            Self::Text { .. } => CommandKind::Text,
            Self::KeyframeReadiness { .. } => CommandKind::KeyframeReadiness,
        }
    }

    /// Reads the raw kind byte, the kind, and the whole record length from a header.
    fn header(bytes: &[u8]) -> Result<(u8, CommandKind, usize), ProtocolError> {
        let &[PROTOCOL_VERSION, kind, _, 0, a0, a1, a2, a3, ..] = bytes else {
            return Err(ProtocolError::InvalidHeader);
        };
        let command_kind = CommandKind::from_wire(kind).ok_or(ProtocolError::InvalidFields {
            kind,
            reason: InvalidValue("unknown command kind"),
        })?;
        let text_len = match command_kind.payload_limit() {
            Some(limit) => usize::try_from(u32::from_le_bytes([a0, a1, a2, a3]))
                .ok()
                .filter(|length| *length <= limit)
                .ok_or(ProtocolError::PayloadTooLarge)?,
            None => 0,
        };
        Ok((kind, command_kind, COMMAND_HEADER_BYTES + text_len))
    }

    /// Reads everything after the version and kind bytes. `header` already matched the length
    /// slot of a text kind against the record, so the rest of the record is the text.
    fn decode_fields(kind: CommandKind, input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        let command = match kind {
            CommandKind::PointerAbsolute => {
                input.reserved::<2>()?;
                Self::PointerAbsolute {
                    x: input.get()?,
                    y: input.get()?,
                    sequence: input.get()?,
                }
            }
            CommandKind::PointerButton => {
                let state = input.get()?;
                input.reserved::<1>()?;
                let button = input.get()?;
                input.reserved::<4>()?;
                Self::PointerButton {
                    button,
                    state,
                    sequence: input.get()?,
                }
            }
            CommandKind::PointerScroll => {
                input.reserved::<2>()?;
                Self::PointerScroll {
                    dx: input.get()?,
                    dy: input.get()?,
                    sequence: input.get()?,
                }
            }
            CommandKind::KeyboardKey => {
                let state = input.get()?;
                input.reserved::<1>()?;
                let key = input.get()?;
                input.reserved::<4>()?;
                Self::KeyboardKey {
                    key,
                    state,
                    sequence: input.get()?,
                }
            }
            CommandKind::ReleaseAll => {
                input.reserved::<14>()?;
                Self::ReleaseAll
            }
            CommandKind::Resize => {
                input.reserved::<2>()?;
                Self::Resize {
                    size: input.get()?,
                    scale_v120: input.get()?,
                    request_id: input.get()?,
                }
            }
            CommandKind::Clipboard => {
                input.reserved::<2>()?;
                let _length: u32 = input.get()?;
                input.reserved::<8>()?;
                Self::Clipboard(ClipboardText::new(input.rest_utf8()?.to_owned())?)
            }
            CommandKind::PointerRelative => {
                input.reserved::<2>()?;
                Self::PointerRelative {
                    dx: input.get()?,
                    dy: input.get()?,
                    sequence: input.get()?,
                }
            }
            CommandKind::Quality => {
                input.reserved::<2>()?;
                Self::Quality {
                    bitrate_kbps: input.get()?,
                    fps: input.get()?,
                    scale_percent: input.get()?,
                }
            }
            CommandKind::Text => {
                let action = input.get()?;
                input.reserved::<1>()?;
                let _length: u32 = input.get()?;
                let sequence = input.get()?;
                input.reserved::<4>()?;
                Self::Text {
                    action,
                    text: InputText::new(input.rest_utf8()?.to_owned())?,
                    sequence,
                }
            }
            CommandKind::KeyframeReadiness => {
                let state = input.get()?;
                input.reserved::<1>()?;
                let generation = input.get()?;
                input.reserved::<8>()?;
                Self::KeyframeReadiness { generation, state }
            }
        };
        input.finish()?;
        Ok(command)
    }
}

impl Record for Command {
    const HEADER_BYTES: usize = COMMAND_HEADER_BYTES;

    fn record_len(header: &[u8]) -> Result<usize, ProtocolError> {
        if header.len() != Self::HEADER_BYTES {
            return Err(ProtocolError::InvalidHeader);
        }
        Self::header(header).map(|(_, _, record_len)| record_len)
    }

    fn decode(record: &[u8]) -> Result<Self, ProtocolError> {
        let Some(header) = record.get(..Self::HEADER_BYTES) else {
            return Err(ProtocolError::Truncated);
        };
        let (kind, command_kind, record_len) = Self::header(header)?;
        if record.len() != record_len {
            return Err(ProtocolError::Truncated);
        }
        Self::decode_fields(command_kind, &mut Reader(&record[2..]))
            .map_err(|reason| ProtocolError::InvalidFields { kind, reason })
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Writer::with_capacity(self.encoded_len());
        out.put(&PROTOCOL_VERSION);
        out.put(&self.kind().wire());
        match self {
            Self::PointerAbsolute { x, y, sequence } => {
                out.reserved::<2>();
                out.put(x);
                out.put(y);
                out.put(sequence);
            }
            Self::PointerButton {
                button,
                state,
                sequence,
            } => {
                out.put(state);
                out.reserved::<1>();
                out.put(button);
                out.reserved::<4>();
                out.put(sequence);
            }
            Self::PointerScroll { dx, dy, sequence }
            | Self::PointerRelative { dx, dy, sequence } => {
                out.reserved::<2>();
                out.put(dx);
                out.put(dy);
                out.put(sequence);
            }
            Self::KeyboardKey {
                key,
                state,
                sequence,
            } => {
                out.put(state);
                out.reserved::<1>();
                out.put(key);
                out.reserved::<4>();
                out.put(sequence);
            }
            Self::ReleaseAll => out.reserved::<14>(),
            Self::Resize {
                size,
                scale_v120,
                request_id,
            } => {
                out.reserved::<2>();
                out.put(size);
                out.put(scale_v120);
                out.put(request_id);
            }
            Self::Clipboard(text) => {
                out.reserved::<2>();
                out.length(text.as_str().len());
                out.reserved::<8>();
                out.bytes(text.as_str().as_bytes());
            }
            Self::Quality {
                bitrate_kbps,
                fps,
                scale_percent,
            } => {
                out.reserved::<2>();
                out.put(bitrate_kbps);
                out.put(fps);
                out.put(scale_percent);
            }
            Self::Text {
                action,
                text,
                sequence,
            } => {
                out.put(action);
                out.reserved::<1>();
                out.length(text.as_str().len());
                out.put(sequence);
                out.reserved::<4>();
                out.bytes(text.as_str().as_bytes());
            }
            Self::KeyframeReadiness { generation, state } => {
                out.put(state);
                out.reserved::<1>();
                out.put(generation);
                out.reserved::<8>();
            }
        }
        out.into_inner()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CommandKind {
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

    pub(crate) const fn wire(self) -> u8 {
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

    pub(crate) const fn browser_input(self) -> bool {
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

impl Wire for FrameMetadata {
    fn write(&self, out: &mut Writer) {
        out.put(&self.generation);
        out.put(&self.width);
        out.put(&self.height);
        out.put(&self.capture_nanos);
        out.put(&self.sequence);
        out.put(&self.input_sequence);
        out.put(&self.fps);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            generation: input.get()?,
            width: input.get()?,
            height: input.get()?,
            capture_nanos: input.get()?,
            sequence: input.get()?,
            input_sequence: input.get()?,
            fps: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Hotspot {
    pub x: i32,
    pub y: i32,
}

impl Wire for Hotspot {
    fn write(&self, out: &mut Writer) {
        out.put(&self.x);
        out.put(&self.y);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            x: input.get()?,
            y: input.get()?,
        })
    }
}

/// A cursor bitmap as Wayland shared memory provides it: one byte each of blue, green, red,
/// and alpha per pixel, row-major, with color premultiplied by alpha. `pixels` holds exactly
/// `size.byte_count()` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    size: CursorSize,
    pub hotspot: Hotspot,
    pixels: Vec<u8>,
}

impl CursorImage {
    pub fn new(size: CursorSize, hotspot: Hotspot, pixels: Vec<u8>) -> Result<Self, InvalidValue> {
        if pixels.len() != size.byte_count() {
            return Err(InvalidValue("cursor pixels must match its size"));
        }
        Ok(Self {
            size,
            hotspot,
            pixels,
        })
    }

    #[must_use]
    pub const fn size(&self) -> CursorSize {
        self.size
    }

    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    #[must_use]
    pub fn into_pixels(self) -> Vec<u8> {
        self.pixels
    }
}

impl Wire for CursorImage {
    fn write(&self, out: &mut Writer) {
        out.put(&self.size);
        out.put(&self.hotspot);
        out.bytes(&self.pixels);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        let size = input.get()?;
        let hotspot = input.get()?;
        let pixels = input.rest().to_vec();
        Self::new(size, hotspot, pixels)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ResizeApplied {
    #[serde(rename = "request")]
    pub request_id: RequestId,
    #[serde(flatten)]
    pub size: FrameSize,
    #[serde(rename = "scale")]
    pub scale_v120: ScaleV120,
    pub generation: Generation,
}

impl Wire for ResizeApplied {
    fn write(&self, out: &mut Writer) {
        out.put(&self.request_id);
        out.reserved::<2>();
        out.put(&self.size);
        out.put(&self.scale_v120);
        out.reserved::<2>();
        out.put(&self.generation);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        let request_id = input.get()?;
        input.reserved::<2>()?;
        let size = input.get()?;
        let scale_v120 = input.get()?;
        input.reserved::<2>()?;
        let generation = input.get()?;
        Ok(Self {
            request_id,
            size,
            scale_v120,
            generation,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// UTF-8 clipboard text.
    Clipboard(ClipboardText),
    /// Metadata for one captured frame.
    Frame(FrameMetadata),
    /// The output settings applied for a resize request.
    ResizeApplied(ResizeApplied),
    /// A cursor bitmap and hotspot.
    CursorImage(CursorImage),
    /// Whether the cursor is hidden or visible.
    CursorVisibility(CursorVisibility),
}

impl Event {
    const fn kind(&self) -> EventKind {
        match self {
            Self::Clipboard(_) => EventKind::Clipboard,
            Self::Frame(_) => EventKind::Frame,
            Self::ResizeApplied(_) => EventKind::ResizeApplied,
            Self::CursorImage(_) => EventKind::CursorImage,
            Self::CursorVisibility(_) => EventKind::CursorVisibility,
        }
    }

    /// Reads the raw kind byte, the kind, and the whole record length from a header.
    fn header(bytes: &[u8]) -> Result<(u8, EventKind, usize), ProtocolError> {
        let &[PROTOCOL_VERSION, kind, 0, 0, l0, l1, l2, l3] = bytes else {
            return Err(ProtocolError::InvalidHeader);
        };
        let event_kind = EventKind::from_wire(kind).ok_or(ProtocolError::InvalidEvent {
            kind,
            reason: InvalidValue("unknown event kind"),
        })?;
        let payload_len = usize::try_from(u32::from_le_bytes([l0, l1, l2, l3]))
            .ok()
            .filter(|length| *length <= MAX_EVENT_BYTES)
            .ok_or(ProtocolError::PayloadTooLarge)?;
        Ok((kind, event_kind, EVENT_HEADER_BYTES + payload_len))
    }

    fn decode_payload(kind: EventKind, input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        let event = match kind {
            EventKind::Clipboard => {
                Self::Clipboard(ClipboardText::new(input.rest_utf8()?.to_owned())?)
            }
            EventKind::Frame => Self::Frame(input.get()?),
            EventKind::ResizeApplied => Self::ResizeApplied(input.get()?),
            EventKind::CursorImage => Self::CursorImage(input.get()?),
            EventKind::CursorVisibility => Self::CursorVisibility(input.get()?),
        };
        input.finish()?;
        Ok(event)
    }
}

impl Record for Event {
    const HEADER_BYTES: usize = EVENT_HEADER_BYTES;

    fn record_len(header: &[u8]) -> Result<usize, ProtocolError> {
        Self::header(header).map(|(_, _, record_len)| record_len)
    }

    fn decode(record: &[u8]) -> Result<Self, ProtocolError> {
        let Some(header) = record.get(..Self::HEADER_BYTES) else {
            return Err(ProtocolError::Truncated);
        };
        let (kind, event_kind, record_len) = Self::header(header)?;
        if record.len() != record_len {
            return Err(ProtocolError::Truncated);
        }
        Self::decode_payload(event_kind, &mut Reader(&record[Self::HEADER_BYTES..]))
            .map_err(|reason| ProtocolError::InvalidEvent { kind, reason })
    }

    fn encode(&self) -> Vec<u8> {
        let capacity = match self {
            Self::Clipboard(text) => text.as_str().len(),
            Self::Frame(_) => 32,
            Self::ResizeApplied(_) => 20,
            Self::CursorImage(image) => 16 + image.pixels().len(),
            Self::CursorVisibility(_) => 1,
        };
        let mut payload = Writer::with_capacity(capacity);
        match self {
            Self::Clipboard(text) => payload.bytes(text.as_str().as_bytes()),
            Self::Frame(frame) => payload.put(frame),
            Self::ResizeApplied(applied) => payload.put(applied),
            Self::CursorImage(image) => payload.put(image),
            Self::CursorVisibility(visibility) => payload.put(visibility),
        }
        let payload = payload.into_inner();
        let mut out = Writer::with_capacity(Self::HEADER_BYTES + payload.len());
        out.put(&PROTOCOL_VERSION);
        out.put(&self.kind().wire());
        out.reserved::<2>();
        out.length(payload.len());
        out.bytes(&payload);
        out.into_inner()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn value<T>(result: Result<T, InvalidValue>) -> T {
        result.expect("test protocol value should be valid")
    }

    fn command_cases() -> Vec<(&'static str, Command, Vec<u8>)> {
        let sequence = value(InputSequence::new(7));
        vec![
            (
                "pointer absolute",
                Command::PointerAbsolute {
                    x: value(PointerCoordinate::new(12)),
                    y: value(PointerCoordinate::new(34)),
                    sequence,
                },
                vec![2, 1, 0, 0, 12, 0, 0, 0, 34, 0, 0, 0, 7, 0, 0, 0],
            ),
            (
                "pointer button",
                Command::PointerButton {
                    button: PointerButton::Left,
                    state: ButtonState::Pressed,
                    sequence,
                },
                vec![2, 2, 1, 0, 16, 1, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0],
            ),
            (
                "pointer scroll",
                Command::PointerScroll {
                    dx: value(PointerDelta::new(1.5)),
                    dy: value(PointerDelta::new(-2.25)),
                    sequence,
                },
                vec![2, 3, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
            ),
            (
                "keyboard key",
                Command::KeyboardKey {
                    key: value(KeyCode::new(30)),
                    state: KeyState::Repeated,
                    sequence,
                },
                vec![2, 4, 2, 0, 30, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0],
            ),
            (
                "release all",
                Command::ReleaseAll,
                vec![2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                "resize",
                Command::Resize {
                    size: value(FrameSize::new(1280, 720)),
                    scale_v120: value(ScaleV120::new(180)),
                    request_id: value(RequestId::new(9)),
                },
                vec![2, 6, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 9, 0],
            ),
            (
                "clipboard",
                Command::Clipboard(value(ClipboardText::new("clip".into()))),
                vec![
                    2, 7, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 99, 108, 105, 112,
                ],
            ),
            (
                "pointer relative",
                Command::PointerRelative {
                    dx: value(PointerDelta::new(1.5)),
                    dy: value(PointerDelta::new(-2.25)),
                    sequence,
                },
                vec![2, 8, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
            ),
            (
                "quality",
                Command::Quality {
                    bitrate_kbps: value(Kbps::new(8_000)),
                    fps: value(Fps::new(60)),
                    scale_percent: value(ScalePercent::new(75)),
                },
                vec![2, 9, 0, 0, 64, 31, 0, 0, 60, 0, 0, 0, 75, 0, 0, 0],
            ),
            (
                "text",
                Command::Text {
                    action: TextAction::Preedit,
                    text: value(InputText::new("hey".into())),
                    sequence,
                },
                vec![
                    2, 10, 1, 0, 3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 104, 101, 121,
                ],
            ),
            (
                "keyframe readiness",
                Command::KeyframeReadiness {
                    generation: value(Generation::new(4)),
                    state: KeyframeState::Cached,
                },
                vec![2, 11, 1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
        ]
    }

    fn event_cases() -> Vec<(&'static str, Event, Vec<u8>)> {
        vec![
            (
                "clipboard",
                Event::Clipboard(value(ClipboardText::new("clip".into()))),
                vec![2, 1, 0, 0, 4, 0, 0, 0, 99, 108, 105, 112],
            ),
            (
                "frame",
                Event::Frame(FrameMetadata {
                    generation: value(Generation::new(1)),
                    width: value(FrameDimension::new(1280)),
                    height: value(FrameDimension::new(720)),
                    capture_nanos: 2,
                    sequence: 3,
                    input_sequence: Some(value(InputSequence::new(4))),
                    fps: value(Fps::new(60)),
                }),
                vec![
                    2, 2, 0, 0, 32, 0, 0, 0, 1, 0, 0, 0, 0, 5, 208, 2, 2, 0, 0, 0, 0, 0, 0, 0, 3,
                    0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 60, 0, 0, 0,
                ],
            ),
            (
                "resize applied",
                Event::ResizeApplied(ResizeApplied {
                    request_id: value(RequestId::new(9)),
                    size: value(FrameSize::new(1280, 720)),
                    scale_v120: value(ScaleV120::new(180)),
                    generation: value(Generation::new(4)),
                }),
                vec![
                    2, 3, 0, 0, 20, 0, 0, 0, 9, 0, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 0, 0, 4,
                    0, 0, 0,
                ],
            ),
            (
                "cursor image",
                Event::CursorImage(value(CursorImage::new(
                    value(CursorSize::new(1, 1)),
                    Hotspot { x: -1, y: 2 },
                    vec![1, 2, 3, 4],
                ))),
                vec![
                    2, 4, 0, 0, 20, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 255, 255, 255, 255, 2, 0, 0,
                    0, 1, 2, 3, 4,
                ],
            ),
            (
                "cursor visibility",
                Event::CursorVisibility(CursorVisibility::Visible),
                vec![2, 5, 0, 0, 1, 0, 0, 0, 1],
            ),
        ]
    }

    #[test]
    fn every_command_encodes_to_its_pinned_bytes() {
        for (name, command, bytes) in command_cases() {
            assert_eq!(command.encode(), bytes, "{name}");
        }
    }

    #[test]
    fn every_pinned_command_record_decodes() {
        for (name, command, bytes) in command_cases() {
            assert_eq!(Command::decode(&bytes), Ok(command), "{name}");
        }
    }

    #[test]
    fn every_event_encodes_to_its_pinned_bytes() {
        for (name, event, bytes) in event_cases() {
            assert_eq!(event.encode(), bytes, "{name}");
        }
    }

    #[test]
    fn every_pinned_event_record_decodes() {
        for (name, event, bytes) in event_cases() {
            assert_eq!(Event::decode(&bytes), Ok(event), "{name}");
        }
    }

    #[test]
    fn pointer_invariants_reject_values_the_decoder_cannot_accept() {
        assert!(PointerCoordinate::new(65_535).is_ok());
        assert!(PointerCoordinate::new(65_536).is_err());
        assert!(PointerDelta::new(-4096.0).is_ok());
        assert!(PointerDelta::new(4096.0).is_ok());
        assert!(PointerDelta::new(-4096.5).is_err());
        assert!(PointerDelta::new(4096.5).is_err());
        assert!(PointerDelta::new(f32::NAN).is_err());
        assert!(PointerDelta::new(f32::INFINITY).is_err());
    }

    #[test]
    fn command_header_names_text_payload_presence_even_when_empty() {
        let empty_clipboard = [2, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let release_all = [2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

        assert_eq!(
            Command::record_len(&empty_clipboard),
            Ok(COMMAND_HEADER_BYTES)
        );
        assert_eq!(Command::record_len(&release_all), Ok(COMMAND_HEADER_BYTES));
    }

    #[test]
    fn wrong_version_is_an_invalid_header() {
        let mut header = [0; COMMAND_HEADER_BYTES];
        header[0] = PROTOCOL_VERSION - 1;
        header[1] = 5;
        assert_eq!(
            Command::record_len(&header),
            Err(ProtocolError::InvalidHeader)
        );
    }

    #[test]
    fn unknown_command_kind_is_invalid_fields() {
        let mut header = [0; COMMAND_HEADER_BYTES];
        header[0] = PROTOCOL_VERSION;
        header[1] = 99;
        assert!(matches!(
            Command::record_len(&header),
            Err(ProtocolError::InvalidFields { kind: 99, .. })
        ));
    }

    #[test]
    fn unknown_event_kind_is_invalid_event() {
        let header = [PROTOCOL_VERSION, 99, 0, 0, 0, 0, 0, 0];
        assert!(matches!(
            Event::record_len(&header),
            Err(ProtocolError::InvalidEvent { kind: 99, .. })
        ));
    }

    #[test]
    fn command_text_payload_over_limit_is_too_large() {
        let mut header = [0; COMMAND_HEADER_BYTES];
        header[0] = PROTOCOL_VERSION;
        header[1] = 10;
        header[4..8].copy_from_slice(
            &(u32::try_from(MAX_TEXT_BYTES).expect("limit fits u32") + 1).to_le_bytes(),
        );
        assert_eq!(
            Command::record_len(&header),
            Err(ProtocolError::PayloadTooLarge)
        );
    }

    #[test]
    fn event_payload_over_limit_is_too_large() {
        let mut header = [PROTOCOL_VERSION, 1, 0, 0, 0, 0, 0, 0];
        header[4..8].copy_from_slice(
            &(u32::try_from(MAX_EVENT_BYTES).expect("limit fits u32") + 1).to_le_bytes(),
        );
        assert_eq!(
            Event::record_len(&header),
            Err(ProtocolError::PayloadTooLarge)
        );
    }

    #[test]
    fn clipboard_payload_must_be_utf8() {
        let record = [PROTOCOL_VERSION, 1, 0, 0, 1, 0, 0, 0, 0xff];
        assert!(matches!(
            Event::decode(&record),
            Err(ProtocolError::InvalidEvent { kind: 1, .. })
        ));
    }

    #[test]
    fn short_frame_payload_is_an_invalid_event() {
        let mut record = vec![PROTOCOL_VERSION, 2, 0, 0, 31, 0, 0, 0];
        let mut payload = [0; 31];
        payload[0] = 1;
        payload[4] = 1;
        payload[6] = 1;
        payload[28] = 10;
        record.extend_from_slice(&payload);
        assert!(matches!(
            Event::decode(&record),
            Err(ProtocolError::InvalidEvent { kind: 2, .. })
        ));
    }

    #[test]
    fn frame_payload_with_trailing_byte_is_an_invalid_event() {
        let mut record = vec![PROTOCOL_VERSION, 2, 0, 0, 33, 0, 0, 0];
        let mut payload = [0; 33];
        payload[0] = 1;
        payload[4] = 1;
        payload[6] = 1;
        payload[28] = 10;
        record.extend_from_slice(&payload);
        assert!(matches!(
            Event::decode(&record),
            Err(ProtocolError::InvalidEvent { kind: 2, .. })
        ));
    }

    #[test]
    fn cursor_image_payload_length_must_match_its_size() {
        let mut record = vec![PROTOCOL_VERSION, 4, 0, 0, 16, 0, 0, 0];
        let mut payload = [0; 16];
        payload[0..4].copy_from_slice(&1_u32.to_le_bytes());
        payload[4..8].copy_from_slice(&1_u32.to_le_bytes());
        record.extend_from_slice(&payload);
        assert!(matches!(
            Event::decode(&record),
            Err(ProtocolError::InvalidEvent { kind: 4, .. })
        ));
    }

    #[test]
    fn command_payload_length_must_match_its_header() {
        let mut clipboard_with_one_byte = [0; COMMAND_HEADER_BYTES];
        clipboard_with_one_byte[0] = PROTOCOL_VERSION;
        clipboard_with_one_byte[1] = 7;
        clipboard_with_one_byte[4] = 1;
        assert_eq!(
            Command::decode(&clipboard_with_one_byte),
            Err(ProtocolError::Truncated)
        );
    }

    #[test]
    fn empty_text_payload_command_does_not_consume_the_next_header() {
        let clipboard = Command::Clipboard(
            ClipboardText::new(String::new()).expect("empty clipboard text should be valid"),
        );
        let release = Command::ReleaseAll;
        let bytes = [clipboard.encode(), release.encode()].concat();
        let mut decoder = Decoder::<Command>::new();

        assert_eq!(
            decoder
                .push(&bytes)
                .expect("two valid commands should parse"),
            vec![clipboard, release]
        );
        assert!(decoder.finish().is_ok());
    }

    #[test]
    fn event_decoder_returns_two_records_split_across_pushes() {
        let clipboard = Event::Clipboard(value(ClipboardText::new("clip".into())));
        let visible = Event::CursorVisibility(CursorVisibility::Visible);
        let first = clipboard.encode();
        let split = first.len() + 3;
        let mut bytes = first;
        bytes.extend(visible.encode());
        let mut decoder = Decoder::<Event>::new();
        let mut events = decoder
            .push(&bytes[..split])
            .expect("first event and partial second event should parse");
        events.extend(
            decoder
                .push(&bytes[split..])
                .expect("second event should finish"),
        );

        assert_eq!(events, vec![clipboard, visible]);
        assert!(decoder.finish().is_ok());
    }

    #[test]
    fn input_text_rejects_nul() {
        assert!(InputText::new("a\0b".into()).is_err());
    }

    #[test]
    fn clipboard_text_rejects_oversize_input() {
        assert!(ClipboardText::new("x".repeat(MAX_CLIPBOARD_BYTES + 1)).is_err());
    }

    #[test]
    fn cursor_image_rejects_wrong_length_buffer() {
        let size = value(CursorSize::new(1, 1));
        assert!(CursorImage::new(size, Hotspot { x: 0, y: 0 }, Vec::new()).is_err());
    }
}
