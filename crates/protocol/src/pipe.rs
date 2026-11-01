//! Gateway-to-compositor pipe vocabulary for protocol version 12.

use std::num::NonZeroU16;
use std::num::NonZeroU32;

use serde::Deserialize;
use serde::Serialize;

use crate::wire::InvalidValue;
use crate::wire::Reader;
use crate::wire::Record;
use crate::wire::RecordKind;
use crate::wire::Wire;
use crate::wire::Writer;

pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 4_000;
/// Longest window id, app id or title an input receipt carries; longer ones are cut.
pub const MAX_RECEIPT_TEXT_BYTES: usize = 512;
pub const MAX_RAW_PIXELS: u64 = parse_pixel_cap(include_str!("../max-display-pixels.json"));

const fn parse_pixel_cap(value: &str) -> u64 {
    let bytes = value.as_bytes();
    let mut result = 0_u64;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\n' && index + 1 == bytes.len() {
            break;
        }
        assert!(byte >= b'0' && byte <= b'9', "invalid display pixel cap");
        result = result * 10 + (byte - b'0') as u64;
        index += 1;
    }
    assert!(result > 0, "display pixel cap must be positive");
    result
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
            pub const fn new(value: $inner) -> Result<Self, InvalidValue> {
                let minimum: $inner = $minimum;
                let maximum: $inner = $maximum;
                if minimum <= value && value <= maximum {
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

macro_rules! ordered_ranged_newtype {
    ($name:ident, $inner:ty, $minimum:expr, $maximum:expr, $label:literal) => {
        ranged_newtype!($name, $inner, $minimum, $maximum, $label);

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.0.cmp(&other.0)
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
ordered_ranged_newtype!(
    Kbps,
    u32,
    300,
    50_000,
    "bitrate must be between 300 and 50000 Kbps"
);
ranged_newtype!(Crf, u8, 0, 51, "CRF must be between 0 and 51");
ordered_ranged_newtype!(
    Fps,
    u32,
    10,
    120,
    "frame rate must be between 10 and 120 FPS"
);
ordered_ranged_newtype!(
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

impl Kbps {
    pub const MINIMUM: Self = Self(300);
    pub const MAXIMUM: Self = Self(50_000);

    #[must_use]
    pub fn scaled(self, percent: u8) -> Self {
        let value = self
            .get()
            .saturating_mul(u32::from(percent))
            .saturating_div(100)
            .clamp(Self::MINIMUM.get(), Self::MAXIMUM.get());
        Self(value)
    }

    #[must_use]
    pub fn at_least(self, minimum: Self) -> Self {
        Self(self.get().max(minimum.get()))
    }

    #[must_use]
    pub fn at_most(self, maximum: Self) -> Self {
        Self(self.get().min(maximum.get()))
    }
}

impl Fps {
    pub const MINIMUM: Self = Self(10);
    pub const MAXIMUM: Self = Self(120);

    /// The encoder cadence: one keyframe every half second at the configured rate. This is
    /// never below 5 because [`Fps::MINIMUM`] is 10. Key frames dominate a typing stream's
    /// bytes, so a shorter cadence costs bandwidth on every keystroke.
    #[must_use]
    pub const fn keyframe_interval(self) -> u32 {
        self.get().div_ceil(2)
    }

    #[must_use]
    pub fn lowered_by(self, step: u32, floor: Self) -> Self {
        Self(self.get().saturating_sub(step).max(floor.get()))
    }

    #[must_use]
    pub fn raised_by(self, step: u32, ceiling: Self) -> Self {
        Self(
            self.get()
                .saturating_add(step)
                .min(ceiling.get())
                .min(Self::MAXIMUM.get()),
        )
    }
}

impl ScalePercent {
    pub const MINIMUM: Self = Self(50);
    pub const MAXIMUM: Self = Self(100);

    #[must_use]
    pub fn lowered_by(self, step: u32, floor: Self) -> Self {
        Self(self.get().saturating_sub(step).max(floor.get()))
    }

    #[must_use]
    pub fn raised_by(self, step: u32, ceiling: Self) -> Self {
        Self(
            self.get()
                .saturating_add(step)
                .min(ceiling.get())
                .min(Self::MAXIMUM.get()),
        )
    }
}

/// Encoded component representation chosen by the quality ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chroma {
    Yuv444,
    Yuv420,
    /// Full-range sRGB components with the H.264 identity (GBR) matrix.
    Rgb,
}

impl Chroma {
    #[must_use]
    pub const fn h264_profile(self) -> H264Profile {
        match self {
            Self::Yuv444 | Self::Rgb => H264Profile {
                profile_idc: 0xf4,
                constraints: 0,
                level_idc: 0x34,
                ffmpeg_profile: "high444",
            },
            Self::Yuv420 => H264Profile {
                profile_idc: 0x64,
                constraints: 0,
                level_idc: 0x34,
                ffmpeg_profile: "high",
            },
        }
    }

    #[must_use]
    pub const fn ffmpeg_pixel_format(self) -> &'static str {
        match self {
            Self::Yuv444 => "yuv444p",
            Self::Yuv420 => "yuv420p",
            Self::Rgb => "bgr0",
        }
    }
}

impl Wire for Chroma {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Yuv444 => 0_u8,
            Self::Yuv420 => 1,
            Self::Rgb => 2,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self::Yuv444),
            1 => Ok(Self::Yuv420),
            2 => Ok(Self::Rgb),
            _ => Err(InvalidValue("unknown chroma sampling")),
        }
    }
}

/// The stored `FFmpeg` profile name must encode `profile_idc`; streamd's SPS
/// tests check both choices against emitted encoder data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct H264Profile {
    profile_idc: u8,
    constraints: u8,
    level_idc: u8,
    ffmpeg_profile: &'static str,
}

impl H264Profile {
    #[must_use]
    pub const fn ffmpeg_profile(self) -> &'static str {
        self.ffmpeg_profile
    }

    #[must_use]
    pub fn ffmpeg_level(self) -> String {
        format!("{}.{}", self.level_idc / 10, self.level_idc % 10)
    }

    #[must_use]
    pub fn codec(self) -> String {
        format!(
            "avc1.{:02X}{:02X}{:02X}",
            self.profile_idc, self.constraints, self.level_idc
        )
    }
}

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

impl Wire for ClipboardText {
    fn write(&self, out: &mut Writer) {
        out.bytes(self.as_str().as_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Self::new(input.rest_utf8()?.to_owned())
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

impl Wire for InputText {
    fn write(&self, out: &mut Writer) {
        out.bytes(self.as_str().as_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Self::new(input.rest_utf8()?.to_owned())
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
pub enum Button {
    Left,
    Right,
    Middle,
    Side,
    Extra,
}

impl Button {
    /// The Linux input event code, which is also the protocol value.
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

impl Wire for Button {
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

/// Named cursor shapes drawn from CSS `cursor` and `wp_cursor_shape_v1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(into = "&'static str")]
pub enum CursorShape {
    Default,
    ContextMenu,
    Help,
    Pointer,
    Progress,
    Wait,
    Cell,
    Crosshair,
    Text,
    VerticalText,
    Alias,
    Copy,
    Move,
    NoDrop,
    NotAllowed,
    Grab,
    Grabbing,
    EResize,
    NResize,
    NeResize,
    NwResize,
    SResize,
    SeResize,
    SwResize,
    WResize,
    EwResize,
    NsResize,
    NeswResize,
    NwseResize,
    ColResize,
    RowResize,
    AllScroll,
    ZoomIn,
    ZoomOut,
    DndAsk,
    AllResize,
}

impl CursorShape {
    pub const ALL: [Self; 36] = [
        Self::Default,
        Self::ContextMenu,
        Self::Help,
        Self::Pointer,
        Self::Progress,
        Self::Wait,
        Self::Cell,
        Self::Crosshair,
        Self::Text,
        Self::VerticalText,
        Self::Alias,
        Self::Copy,
        Self::Move,
        Self::NoDrop,
        Self::NotAllowed,
        Self::Grab,
        Self::Grabbing,
        Self::EResize,
        Self::NResize,
        Self::NeResize,
        Self::NwResize,
        Self::SResize,
        Self::SeResize,
        Self::SwResize,
        Self::WResize,
        Self::EwResize,
        Self::NsResize,
        Self::NeswResize,
        Self::NwseResize,
        Self::ColResize,
        Self::RowResize,
        Self::AllScroll,
        Self::ZoomIn,
        Self::ZoomOut,
        Self::DndAsk,
        Self::AllResize,
    ];

    #[must_use]
    pub const fn css_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ContextMenu => "context-menu",
            Self::Help => "help",
            Self::Pointer => "pointer",
            Self::Progress => "progress",
            Self::Wait => "wait",
            Self::Cell => "cell",
            Self::Crosshair => "crosshair",
            Self::Text => "text",
            Self::VerticalText => "vertical-text",
            Self::Alias => "alias",
            Self::Copy => "copy",
            Self::Move => "move",
            Self::NoDrop => "no-drop",
            Self::NotAllowed => "not-allowed",
            Self::Grab => "grab",
            Self::Grabbing => "grabbing",
            Self::EResize => "e-resize",
            Self::NResize => "n-resize",
            Self::NeResize => "ne-resize",
            Self::NwResize => "nw-resize",
            Self::SResize => "s-resize",
            Self::SeResize => "se-resize",
            Self::SwResize => "sw-resize",
            Self::WResize => "w-resize",
            Self::EwResize => "ew-resize",
            Self::NsResize => "ns-resize",
            Self::NeswResize => "nesw-resize",
            Self::NwseResize => "nwse-resize",
            Self::ColResize => "col-resize",
            Self::RowResize => "row-resize",
            Self::AllScroll => "all-scroll",
            Self::ZoomIn => "zoom-in",
            Self::ZoomOut => "zoom-out",
            Self::DndAsk => "dnd-ask",
            Self::AllResize => "all-resize",
        }
    }
}

impl From<CursorShape> for &'static str {
    fn from(shape: CursorShape) -> Self {
        shape.css_name()
    }
}

impl Wire for CursorShape {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Default => 1_u8,
            Self::ContextMenu => 2,
            Self::Help => 3,
            Self::Pointer => 4,
            Self::Progress => 5,
            Self::Wait => 6,
            Self::Cell => 7,
            Self::Crosshair => 8,
            Self::Text => 9,
            Self::VerticalText => 10,
            Self::Alias => 11,
            Self::Copy => 12,
            Self::Move => 13,
            Self::NoDrop => 14,
            Self::NotAllowed => 15,
            Self::Grab => 16,
            Self::Grabbing => 17,
            Self::EResize => 18,
            Self::NResize => 19,
            Self::NeResize => 20,
            Self::NwResize => 21,
            Self::SResize => 22,
            Self::SeResize => 23,
            Self::SwResize => 24,
            Self::WResize => 25,
            Self::EwResize => 26,
            Self::NsResize => 27,
            Self::NeswResize => 28,
            Self::NwseResize => 29,
            Self::ColResize => 30,
            Self::RowResize => 31,
            Self::AllScroll => 32,
            Self::ZoomIn => 33,
            Self::ZoomOut => 34,
            Self::DndAsk => 35,
            Self::AllResize => 36,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            1_u8 => Ok(Self::Default),
            2 => Ok(Self::ContextMenu),
            3 => Ok(Self::Help),
            4 => Ok(Self::Pointer),
            5 => Ok(Self::Progress),
            6 => Ok(Self::Wait),
            7 => Ok(Self::Cell),
            8 => Ok(Self::Crosshair),
            9 => Ok(Self::Text),
            10 => Ok(Self::VerticalText),
            11 => Ok(Self::Alias),
            12 => Ok(Self::Copy),
            13 => Ok(Self::Move),
            14 => Ok(Self::NoDrop),
            15 => Ok(Self::NotAllowed),
            16 => Ok(Self::Grab),
            17 => Ok(Self::Grabbing),
            18 => Ok(Self::EResize),
            19 => Ok(Self::NResize),
            20 => Ok(Self::NeResize),
            21 => Ok(Self::NwResize),
            22 => Ok(Self::SResize),
            23 => Ok(Self::SeResize),
            24 => Ok(Self::SwResize),
            25 => Ok(Self::WResize),
            26 => Ok(Self::EwResize),
            27 => Ok(Self::NsResize),
            28 => Ok(Self::NeswResize),
            29 => Ok(Self::NwseResize),
            30 => Ok(Self::ColResize),
            31 => Ok(Self::RowResize),
            32 => Ok(Self::AllScroll),
            33 => Ok(Self::ZoomIn),
            34 => Ok(Self::ZoomOut),
            35 => Ok(Self::DndAsk),
            36 => Ok(Self::AllResize),
            _ => Err(InvalidValue("unknown cursor shape")),
        }
    }
}

/// Cursor position normalized to 0..=65535 over the output, as with absolute input.
/// Independent of output scale and encoded video dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CursorPosition {
    pub x: PointerCoordinate,
    pub y: PointerCoordinate,
    pub source: CursorSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorSource {
    Browser,
    Agent,
}

impl Wire for CursorSource {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Browser => 0_u8,
            Self::Agent => 1,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get::<u8>()? {
            0 => Ok(Self::Browser),
            1 => Ok(Self::Agent),
            _ => Err(InvalidValue("unknown cursor source")),
        }
    }
}

impl Wire for CursorPosition {
    fn write(&self, out: &mut Writer) {
        out.put(&self.x);
        out.put(&self.y);
        out.put(&self.source);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            x: input.get()?,
            y: input.get()?,
            source: input.get()?,
        })
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerAbsolute {
    pub x: PointerCoordinate,
    pub y: PointerCoordinate,
    pub sequence: InputSequence,
}

impl Wire for PointerAbsolute {
    fn write(&self, out: &mut Writer) {
        out.put(&self.x);
        out.put(&self.y);
        out.put(&self.sequence);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            x: input.get()?,
            y: input.get()?,
            sequence: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerButton {
    pub button: Button,
    pub state: ButtonState,
    pub sequence: InputSequence,
}

impl Wire for PointerButton {
    fn write(&self, out: &mut Writer) {
        out.put(&self.button);
        out.put(&self.state);
        out.put(&self.sequence);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            button: input.get()?,
            state: input.get()?,
            sequence: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerScroll {
    pub dx: PointerDelta,
    pub dy: PointerDelta,
    pub sequence: InputSequence,
}

impl Wire for PointerScroll {
    fn write(&self, out: &mut Writer) {
        out.put(&self.dx);
        out.put(&self.dy);
        out.put(&self.sequence);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            dx: input.get()?,
            dy: input.get()?,
            sequence: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyboardKey {
    pub key: KeyCode,
    pub state: KeyState,
    pub sequence: InputSequence,
}

impl Wire for KeyboardKey {
    fn write(&self, out: &mut Writer) {
        out.put(&self.key);
        out.put(&self.state);
        out.put(&self.sequence);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            key: input.get()?,
            state: input.get()?,
            sequence: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseAll;

impl Wire for ReleaseAll {
    fn write(&self, _out: &mut Writer) {}

    fn read(_input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self)
    }
}

/// Whether a browser currently holds the seat's input lease. While it does,
/// the compositor releases and then drops every agent source's input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputLease {
    pub held: bool,
}

impl Wire for InputLease {
    fn write(&self, out: &mut Writer) {
        out.put(&u8::from(self.held));
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self { held: false }),
            1 => Ok(Self { held: true }),
            _ => Err(InvalidValue("lease state must be zero or one")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetVideo;

impl Wire for ResetVideo {
    fn write(&self, _out: &mut Writer) {}

    fn read(_input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResetVideoRefusal {
    CurrentModeUnknown,
    OutputTooSmall,
    CompositorRejected,
    CompositorCancelled,
    CompositorTimedOut,
    OutputUnavailable,
}

impl Wire for ResetVideoRefusal {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::CurrentModeUnknown => 1_u8,
            Self::OutputTooSmall => 2,
            Self::CompositorRejected => 3,
            Self::CompositorCancelled => 4,
            Self::CompositorTimedOut => 5,
            Self::OutputUnavailable => 6,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            1_u8 => Ok(Self::CurrentModeUnknown),
            2 => Ok(Self::OutputTooSmall),
            3 => Ok(Self::CompositorRejected),
            4 => Ok(Self::CompositorCancelled),
            5 => Ok(Self::CompositorTimedOut),
            6 => Ok(Self::OutputUnavailable),
            _ => Err(InvalidValue("unknown video reset refusal reason")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resize {
    pub size: FrameSize,
    pub scale_v120: ScaleV120,
    pub request_id: RequestId,
}

impl Wire for Resize {
    fn write(&self, out: &mut Writer) {
        out.put(&self.size);
        out.put(&self.scale_v120);
        out.put(&self.request_id);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            size: input.get()?,
            scale_v120: input.get()?,
            request_id: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerRelative {
    pub dx: PointerDelta,
    pub dy: PointerDelta,
    pub sequence: InputSequence,
}

impl Wire for PointerRelative {
    fn write(&self, out: &mut Writer) {
        out.put(&self.dx);
        out.put(&self.dy);
        out.put(&self.sequence);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            dx: input.get()?,
            dy: input.get()?,
            sequence: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality {
    pub bitrate_kbps: Kbps,
    pub fps: Fps,
    pub scale_percent: ScalePercent,
    pub crf: Crf,
    pub chroma: Chroma,
}

impl Wire for Quality {
    fn write(&self, out: &mut Writer) {
        out.put(&self.bitrate_kbps);
        out.put(&self.fps);
        out.put(&self.scale_percent);
        out.put(&self.crf);
        out.put(&self.chroma);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            bitrate_kbps: input.get()?,
            fps: input.get()?,
            scale_percent: input.get()?,
            crf: input.get()?,
            chroma: input.get()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    pub action: TextAction,
    pub sequence: InputSequence,
    pub text: InputText,
}

impl Wire for Text {
    fn write(&self, out: &mut Writer) {
        out.put(&self.action);
        out.put(&self.sequence);
        out.put(&self.text);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            action: input.get()?,
            sequence: input.get()?,
            text: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyframeReadiness {
    pub generation: Generation,
    pub state: KeyframeState,
}

impl Wire for KeyframeReadiness {
    fn write(&self, out: &mut Writer) {
        out.put(&self.generation);
        out.put(&self.state);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            generation: input.get()?,
            state: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CommandKind {
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
    ResetVideo = 12,
    InputLease = 13,
    PointerWheel = 14,
    PointerScrollStop = 15,
}

impl CommandKind {
    pub(crate) const fn browser_input(self) -> bool {
        matches!(
            self,
            Self::PointerAbsolute
                | Self::PointerButton
                | Self::PointerScroll
                | Self::PointerWheel
                | Self::PointerScrollStop
                | Self::KeyboardKey
                | Self::ReleaseAll
                | Self::Resize
                | Self::ResetVideo
                | Self::PointerRelative
        )
    }
}

impl RecordKind for CommandKind {
    fn wire(self) -> u8 {
        match self {
            Self::PointerAbsolute => 1,
            Self::PointerButton => 2,
            Self::PointerScroll => 3,
            Self::KeyboardKey => 4,
            Self::ReleaseAll => 5,
            Self::Resize => 6,
            Self::Clipboard => 7,
            Self::PointerRelative => 8,
            Self::Quality => 9,
            Self::Text => 10,
            Self::KeyframeReadiness => 11,
            Self::ResetVideo => 12,
            Self::InputLease => 13,
            Self::PointerWheel => 14,
            Self::PointerScrollStop => 15,
        }
    }

    fn from_wire(byte: u8) -> Option<Self> {
        match byte {
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
            12 => Some(Self::ResetVideo),
            13 => Some(Self::InputLease),
            14 => Some(Self::PointerWheel),
            15 => Some(Self::PointerScrollStop),
            _ => None,
        }
    }

    fn max_payload(self) -> usize {
        match self {
            Self::PointerAbsolute
            | Self::PointerScroll
            | Self::PointerWheel
            | Self::PointerScrollStop
            | Self::Resize
            | Self::PointerRelative => 12,
            Self::Quality => 14,
            Self::PointerButton | Self::KeyboardKey => 9,
            Self::ReleaseAll | Self::ResetVideo => 0,
            Self::InputLease => 1,
            Self::Clipboard => MAX_CLIPBOARD_BYTES,
            Self::Text => MAX_TEXT_BYTES + 5,
            Self::KeyframeReadiness => 5,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    PointerAbsolute(PointerAbsolute),
    PointerButton(PointerButton),
    PointerScroll(PointerScroll),
    PointerWheel(PointerScroll),
    PointerScrollStop(PointerScroll),
    KeyboardKey(KeyboardKey),
    ReleaseAll(ReleaseAll),
    Resize(Resize),
    Clipboard(ClipboardText),
    PointerRelative(PointerRelative),
    Quality(Quality),
    Text(Text),
    KeyframeReadiness(KeyframeReadiness),
    ResetVideo(ResetVideo),
    InputLease(InputLease),
}

impl Command {
    #[must_use]
    pub fn input_sequence(&self) -> Option<InputSequence> {
        match self {
            Self::PointerAbsolute(payload) => Some(payload.sequence),
            Self::PointerButton(payload) => Some(payload.sequence),
            Self::PointerScroll(payload) => Some(payload.sequence),
            Self::PointerWheel(payload) | Self::PointerScrollStop(payload) => {
                Some(payload.sequence)
            }
            Self::KeyboardKey(payload) => Some(payload.sequence),
            Self::PointerRelative(payload) => Some(payload.sequence),
            Self::Text(payload) => Some(payload.sequence),
            Self::ReleaseAll(_)
            | Self::Resize(_)
            | Self::Clipboard(_)
            | Self::Quality(_)
            | Self::KeyframeReadiness(_)
            | Self::ResetVideo(_)
            | Self::InputLease(_) => None,
        }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> CommandKind {
        match self {
            Self::PointerAbsolute(_) => CommandKind::PointerAbsolute,
            Self::PointerButton(_) => CommandKind::PointerButton,
            Self::PointerScroll(_) => CommandKind::PointerScroll,
            Self::PointerWheel(_) => CommandKind::PointerWheel,
            Self::PointerScrollStop(_) => CommandKind::PointerScrollStop,
            Self::KeyboardKey(_) => CommandKind::KeyboardKey,
            Self::ReleaseAll(_) => CommandKind::ReleaseAll,
            Self::Resize(_) => CommandKind::Resize,
            Self::Clipboard(_) => CommandKind::Clipboard,
            Self::PointerRelative(_) => CommandKind::PointerRelative,
            Self::Quality(_) => CommandKind::Quality,
            Self::Text(_) => CommandKind::Text,
            Self::KeyframeReadiness(_) => CommandKind::KeyframeReadiness,
            Self::ResetVideo(_) => CommandKind::ResetVideo,
            Self::InputLease(_) => CommandKind::InputLease,
        }
    }
}

impl Record for Command {
    type Kind = CommandKind;

    fn kind(&self) -> Self::Kind {
        self.kind()
    }

    fn write_payload(&self, out: &mut Writer) {
        match self {
            Self::PointerAbsolute(payload) => out.put(payload),
            Self::PointerButton(payload) => out.put(payload),
            Self::PointerScroll(payload)
            | Self::PointerWheel(payload)
            | Self::PointerScrollStop(payload) => out.put(payload),
            Self::KeyboardKey(payload) => out.put(payload),
            Self::ReleaseAll(payload) => out.put(payload),
            Self::Resize(payload) => out.put(payload),
            Self::Clipboard(payload) => out.put(payload),
            Self::PointerRelative(payload) => out.put(payload),
            Self::Quality(payload) => out.put(payload),
            Self::Text(payload) => out.put(payload),
            Self::KeyframeReadiness(payload) => out.put(payload),
            Self::ResetVideo(payload) => out.put(payload),
            Self::InputLease(payload) => out.put(payload),
        }
    }

    fn read_payload(kind: Self::Kind, input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match kind {
            CommandKind::PointerAbsolute => Ok(Self::PointerAbsolute(input.get()?)),
            CommandKind::PointerButton => Ok(Self::PointerButton(input.get()?)),
            CommandKind::PointerScroll => Ok(Self::PointerScroll(input.get()?)),
            CommandKind::PointerWheel => Ok(Self::PointerWheel(input.get()?)),
            CommandKind::PointerScrollStop => Ok(Self::PointerScrollStop(input.get()?)),
            CommandKind::KeyboardKey => Ok(Self::KeyboardKey(input.get()?)),
            CommandKind::ReleaseAll => Ok(Self::ReleaseAll(input.get()?)),
            CommandKind::Resize => Ok(Self::Resize(input.get()?)),
            CommandKind::Clipboard => Ok(Self::Clipboard(input.get()?)),
            CommandKind::PointerRelative => Ok(Self::PointerRelative(input.get()?)),
            CommandKind::Quality => Ok(Self::Quality(input.get()?)),
            CommandKind::Text => Ok(Self::Text(input.get()?)),
            CommandKind::KeyframeReadiness => Ok(Self::KeyframeReadiness(input.get()?)),
            CommandKind::ResetVideo => Ok(Self::ResetVideo(input.get()?)),
            CommandKind::InputLease => Ok(Self::InputLease(input.get()?)),
        }
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
    pub chroma: Chroma,
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
        out.put(&self.chroma);
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
            chroma: input.get()?,
        })
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
        out.put(&self.size);
        out.put(&self.scale_v120);
        out.put(&self.generation);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            request_id: input.get()?,
            size: input.get()?,
            scale_v120: input.get()?,
            generation: input.get()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentInput {
    pub connected: bool,
    pub dropped: bool,
}

impl Wire for AgentInput {
    fn write(&self, out: &mut Writer) {
        out.put(&u8::from(self.connected));
        out.put(&u8::from(self.dropped));
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        let connected = match input.get()? {
            0_u8 => false,
            1 => true,
            _ => return Err(InvalidValue("agent connection state must be zero or one")),
        };
        let dropped = match input.get()? {
            0_u8 => false,
            1 => true,
            _ => return Err(InvalidValue("agent drop state must be zero or one")),
        };
        Ok(Self { connected, dropped })
    }
}

/// Where one browser input event went, reported back to the viewer that sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptDelivery {
    /// A client window received it.
    Window,
    /// The compositor's own titlebars, taskbar or menus consumed it.
    Compositor,
    /// No window had focus or was under the pointer.
    Nowhere,
    /// Input control was not held, so the compositor dropped it.
    Dropped,
}

impl Wire for ReceiptDelivery {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Window => 1_u8,
            Self::Compositor => 2,
            Self::Nowhere => 3,
            Self::Dropped => 4,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            1_u8 => Ok(Self::Window),
            2 => Ok(Self::Compositor),
            3 => Ok(Self::Nowhere),
            4 => Ok(Self::Dropped),
            _ => Err(InvalidValue("receipt delivery is unknown")),
        }
    }
}

/// A window id, app id or title in a receipt, at most
/// [`MAX_RECEIPT_TEXT_BYTES`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ReceiptText(String);

impl ReceiptText {
    pub fn new(text: String) -> Result<Self, InvalidValue> {
        if text.len() <= MAX_RECEIPT_TEXT_BYTES {
            Ok(Self(text))
        } else {
            Err(InvalidValue("receipt text must be at most 512 bytes"))
        }
    }

    /// `text`, cut at the last character boundary that fits.
    #[must_use]
    pub fn truncated(text: &str) -> Self {
        let mut end = text.len().min(MAX_RECEIPT_TEXT_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self(text[..end].to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Wire for ReceiptText {
    fn write(&self, out: &mut Writer) {
        // At most MAX_RECEIPT_TEXT_BYTES, which fits a u16.
        out.put(&u16::try_from(self.0.len()).unwrap_or(u16::MAX));
        out.bytes(self.0.as_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        let length: u16 = input.get()?;
        let Ok(text) = std::str::from_utf8(input.slice(usize::from(length))?) else {
            return Err(InvalidValue("receipt text is not UTF-8"));
        };
        Self::new(text.to_owned())
    }
}

/// The receipt for one browser input event: its input sequence, and the window
/// it reached, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputReceipt {
    pub sequence: InputSequence,
    pub delivery: ReceiptDelivery,
    /// The window's ext-foreign-toplevel-list identifier, or empty.
    pub window_id: ReceiptText,
    pub app_id: ReceiptText,
    pub title: ReceiptText,
}

impl Wire for InputReceipt {
    fn write(&self, out: &mut Writer) {
        out.put(&self.sequence);
        out.put(&self.delivery);
        out.put(&self.window_id);
        out.put(&self.app_id);
        out.put(&self.title);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            sequence: input.get()?,
            delivery: input.get()?,
            window_id: input.get()?,
            app_id: input.get()?,
            title: input.get()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u8,
    pub height: u8,
    pub hotspot_x: u8,
    pub hotspot_y: u8,
    pub output_width: u16,
    pub png: Vec<u8>,
}

impl Wire for CursorImage {
    fn write(&self, out: &mut Writer) {
        out.put(&self.width);
        out.put(&self.height);
        out.put(&self.hotspot_x);
        out.put(&self.hotspot_y);
        out.put(&self.output_width);
        out.bytes(&self.png);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        let width = input.get()?;
        let height = input.get()?;
        let hotspot_x = input.get()?;
        let hotspot_y = input.get()?;
        let output_width = input.get()?;
        let png = input.rest();
        if width == 0
            || width > 128
            || height == 0
            || height > 128
            || hotspot_x >= width
            || hotspot_y >= height
            || output_width == 0
            || !png.starts_with(b"\x89PNG\r\n\x1a\n")
        {
            return Err(InvalidValue("invalid cursor image"));
        }
        Ok(Self {
            width,
            height,
            hotspot_x,
            hotspot_y,
            output_width,
            png: png.to_vec(),
        })
    }
}

/// A frame the compositor encoded in process: the browser stripe container
/// ([`crate::browser::push_stripe`]) of the stripes that changed, and whether every
/// stripe of the frame is a key frame, so a viewer can start from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    pub key: bool,
    pub metadata: FrameMetadata,
    pub data: Vec<u8>,
}

impl Wire for EncodedFrame {
    fn write(&self, out: &mut Writer) {
        out.put(&u8::from(self.key));
        out.put(&self.metadata);
        out.bytes(&self.data);
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        let key = match input.get()? {
            0_u8 => false,
            1 => true,
            _ => return Err(InvalidValue("encoded frame key flag must be zero or one")),
        };
        Ok(Self {
            key,
            metadata: input.get()?,
            data: input.rest().to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Clipboard(ClipboardText),
    Frame(FrameMetadata),
    EncodedFrame(EncodedFrame),
    ResizeApplied(ResizeApplied),
    CursorShape(CursorShape),
    CursorImage(CursorImage),
    CursorVisibility(CursorVisibility),
    CursorPosition(CursorPosition),
    CursorClick(CursorPosition),
    ResetVideoRefused(ResetVideoRefusal),
    AgentInput(AgentInput),
    InputReceipt(InputReceipt),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    Clipboard = 1,
    Frame = 2,
    ResizeApplied = 3,
    CursorShape = 4,
    CursorVisibility = 5,
    CursorPosition = 6,
    ResetVideoRefused = 7,
    AgentInput = 8,
    CursorImage = 9,
    CursorClick = 10,
    EncodedFrame = 11,
    InputReceipt = 12,
}

impl RecordKind for EventKind {
    fn wire(self) -> u8 {
        match self {
            Self::Clipboard => 1,
            Self::Frame => 2,
            Self::ResizeApplied => 3,
            Self::CursorShape => 4,
            Self::CursorVisibility => 5,
            Self::CursorPosition => 6,
            Self::ResetVideoRefused => 7,
            Self::AgentInput => 8,
            Self::CursorImage => 9,
            Self::CursorClick => 10,
            Self::EncodedFrame => 11,
            Self::InputReceipt => 12,
        }
    }

    fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Clipboard),
            2 => Some(Self::Frame),
            3 => Some(Self::ResizeApplied),
            4 => Some(Self::CursorShape),
            5 => Some(Self::CursorVisibility),
            6 => Some(Self::CursorPosition),
            7 => Some(Self::ResetVideoRefused),
            8 => Some(Self::AgentInput),
            9 => Some(Self::CursorImage),
            10 => Some(Self::CursorClick),
            11 => Some(Self::EncodedFrame),
            12 => Some(Self::InputReceipt),
            _ => None,
        }
    }

    fn max_payload(self) -> usize {
        match self {
            Self::Clipboard => MAX_CLIPBOARD_BYTES,
            Self::Frame => 33,
            Self::ResizeApplied => 16,
            Self::CursorShape | Self::CursorVisibility | Self::ResetVideoRefused => 1,
            Self::CursorPosition | Self::CursorClick => 9,
            Self::AgentInput => 2,
            Self::CursorImage => 128 * 128 * 8 + 6,
            // Key flag u8, 33-byte metadata, then the stripe container.
            Self::EncodedFrame => 34 + crate::browser::MAX_VIDEO_DATA_BYTES,
            // Sequence, delivery, then three length-prefixed strings.
            Self::InputReceipt => 5 + 3 * (2 + MAX_RECEIPT_TEXT_BYTES),
        }
    }
}

impl Event {
    const fn kind(&self) -> EventKind {
        match self {
            Self::Clipboard(_) => EventKind::Clipboard,
            Self::Frame(_) => EventKind::Frame,
            Self::EncodedFrame(_) => EventKind::EncodedFrame,
            Self::ResizeApplied(_) => EventKind::ResizeApplied,
            Self::CursorShape(_) => EventKind::CursorShape,
            Self::CursorImage(_) => EventKind::CursorImage,
            Self::CursorVisibility(_) => EventKind::CursorVisibility,
            Self::CursorPosition(_) => EventKind::CursorPosition,
            Self::CursorClick(_) => EventKind::CursorClick,
            Self::ResetVideoRefused(_) => EventKind::ResetVideoRefused,
            Self::AgentInput(_) => EventKind::AgentInput,
            Self::InputReceipt(_) => EventKind::InputReceipt,
        }
    }
}

impl Record for Event {
    type Kind = EventKind;

    fn kind(&self) -> Self::Kind {
        self.kind()
    }

    fn write_payload(&self, out: &mut Writer) {
        match self {
            Self::Clipboard(payload) => out.put(payload),
            Self::Frame(payload) => out.put(payload),
            Self::EncodedFrame(payload) => out.put(payload),
            Self::ResizeApplied(payload) => out.put(payload),
            Self::CursorShape(payload) => out.put(payload),
            Self::CursorImage(payload) => out.put(payload),
            Self::CursorVisibility(payload) => out.put(payload),
            Self::CursorPosition(payload) | Self::CursorClick(payload) => out.put(payload),
            Self::ResetVideoRefused(payload) => out.put(payload),
            Self::AgentInput(payload) => out.put(payload),
            Self::InputReceipt(payload) => out.put(payload),
        }
    }

    fn read_payload(kind: Self::Kind, input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match kind {
            EventKind::Clipboard => Ok(Self::Clipboard(input.get()?)),
            EventKind::Frame => Ok(Self::Frame(input.get()?)),
            EventKind::EncodedFrame => Ok(Self::EncodedFrame(input.get()?)),
            EventKind::ResizeApplied => Ok(Self::ResizeApplied(input.get()?)),
            EventKind::CursorShape => Ok(Self::CursorShape(input.get()?)),
            EventKind::CursorImage => Ok(Self::CursorImage(input.get()?)),
            EventKind::CursorVisibility => Ok(Self::CursorVisibility(input.get()?)),
            EventKind::CursorPosition => Ok(Self::CursorPosition(input.get()?)),
            EventKind::CursorClick => Ok(Self::CursorClick(input.get()?)),
            EventKind::ResetVideoRefused => Ok(Self::ResetVideoRefused(input.get()?)),
            EventKind::AgentInput => Ok(Self::AgentInput(input.get()?)),
            EventKind::InputReceipt => Ok(Self::InputReceipt(input.get()?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Decoder;
    use crate::wire::HEADER_BYTES;
    use crate::wire::ProtocolError;

    fn value<T>(result: Result<T, InvalidValue>) -> T {
        result.expect("test protocol value should be valid")
    }

    fn first_command_cases() -> Vec<(&'static str, Command, Vec<u8>)> {
        let sequence = value(InputSequence::new(7));
        vec![
            (
                "pointer absolute",
                Command::PointerAbsolute(PointerAbsolute {
                    x: value(PointerCoordinate::new(12)),
                    y: value(PointerCoordinate::new(34)),
                    sequence,
                }),
                vec![
                    12, 1, 0, 0, 12, 0, 0, 0, 12, 0, 0, 0, 34, 0, 0, 0, 7, 0, 0, 0,
                ],
            ),
            (
                "pointer button",
                Command::PointerButton(PointerButton {
                    button: Button::Left,
                    state: ButtonState::Pressed,
                    sequence,
                }),
                vec![12, 2, 0, 0, 9, 0, 0, 0, 16, 1, 0, 0, 1, 7, 0, 0, 0],
            ),
            (
                "pointer scroll",
                Command::PointerScroll(PointerScroll {
                    dx: value(PointerDelta::new(1.5)),
                    dy: value(PointerDelta::new(-2.25)),
                    sequence,
                }),
                vec![
                    12, 3, 0, 0, 12, 0, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0,
                ],
            ),
            (
                "pointer wheel",
                Command::PointerWheel(PointerScroll {
                    dx: value(PointerDelta::new(0.0)),
                    dy: value(PointerDelta::new(120.0)),
                    sequence,
                }),
                vec![
                    12, 14, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 240, 66, 7, 0, 0, 0,
                ],
            ),
            (
                "pointer scroll stop",
                Command::PointerScrollStop(PointerScroll {
                    dx: value(PointerDelta::new(0.0)),
                    dy: value(PointerDelta::new(1.0)),
                    sequence,
                }),
                vec![
                    12, 15, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 63, 7, 0, 0, 0,
                ],
            ),
            (
                "keyboard key",
                Command::KeyboardKey(KeyboardKey {
                    key: value(KeyCode::new(30)),
                    state: KeyState::Repeated,
                    sequence,
                }),
                vec![12, 4, 0, 0, 9, 0, 0, 0, 30, 0, 0, 0, 2, 7, 0, 0, 0],
            ),
            (
                "release all",
                Command::ReleaseAll(ReleaseAll),
                vec![12, 5, 0, 0, 0, 0, 0, 0],
            ),
            (
                "resize",
                Command::Resize(Resize {
                    size: value(FrameSize::new(1280, 720)),
                    scale_v120: value(ScaleV120::new(180)),
                    request_id: value(RequestId::new(9)),
                }),
                vec![
                    12, 6, 0, 0, 12, 0, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 9, 0,
                ],
            ),
        ]
    }

    fn remaining_command_cases() -> Vec<(&'static str, Command, Vec<u8>)> {
        let sequence = value(InputSequence::new(7));
        vec![
            (
                "clipboard",
                Command::Clipboard(value(ClipboardText::new("clip".into()))),
                vec![12, 7, 0, 0, 4, 0, 0, 0, 99, 108, 105, 112],
            ),
            (
                "pointer relative",
                Command::PointerRelative(PointerRelative {
                    dx: value(PointerDelta::new(1.5)),
                    dy: value(PointerDelta::new(-2.25)),
                    sequence,
                }),
                vec![
                    12, 8, 0, 0, 12, 0, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0,
                ],
            ),
            (
                "quality",
                Command::Quality(Quality {
                    bitrate_kbps: value(Kbps::new(8_000)),
                    fps: value(Fps::new(60)),
                    scale_percent: value(ScalePercent::new(75)),
                    crf: value(Crf::new(23)),
                    chroma: Chroma::Yuv420,
                }),
                vec![
                    12, 9, 0, 0, 14, 0, 0, 0, 64, 31, 0, 0, 60, 0, 0, 0, 75, 0, 0, 0, 23, 1,
                ],
            ),
            (
                "text",
                Command::Text(Text {
                    action: TextAction::Preedit,
                    sequence,
                    text: value(InputText::new("hey".into())),
                }),
                vec![12, 10, 0, 0, 8, 0, 0, 0, 1, 7, 0, 0, 0, 104, 101, 121],
            ),
            (
                "keyframe readiness",
                Command::KeyframeReadiness(KeyframeReadiness {
                    generation: value(Generation::new(4)),
                    state: KeyframeState::Cached,
                }),
                vec![12, 11, 0, 0, 5, 0, 0, 0, 4, 0, 0, 0, 1],
            ),
            (
                "reset video",
                Command::ResetVideo(ResetVideo),
                vec![12, 12, 0, 0, 0, 0, 0, 0],
            ),
            (
                "browser input lease",
                Command::InputLease(InputLease { held: true }),
                vec![12, 13, 0, 0, 1, 0, 0, 0, 1],
            ),
            (
                "browser input handback",
                Command::InputLease(InputLease { held: false }),
                vec![12, 13, 0, 0, 1, 0, 0, 0, 0],
            ),
        ]
    }

    fn command_cases() -> Vec<(&'static str, Command, Vec<u8>)> {
        let mut cases = first_command_cases();
        cases.extend(remaining_command_cases());
        cases
    }

    fn event_cases() -> Vec<(&'static str, Event, Vec<u8>)> {
        let mut cases = first_event_cases();
        cases.extend(remaining_event_cases());
        cases
    }

    fn first_event_cases() -> Vec<(&'static str, Event, Vec<u8>)> {
        vec![
            (
                "clipboard",
                Event::Clipboard(value(ClipboardText::new("clip".into()))),
                vec![12, 1, 0, 0, 4, 0, 0, 0, 99, 108, 105, 112],
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
                    chroma: Chroma::Yuv420,
                }),
                vec![
                    12, 2, 0, 0, 33, 0, 0, 0, 1, 0, 0, 0, 0, 5, 208, 2, 2, 0, 0, 0, 0, 0, 0, 0, 3,
                    0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 60, 0, 0, 0, 1,
                ],
            ),
            (
                "encoded frame",
                Event::EncodedFrame(EncodedFrame {
                    key: true,
                    metadata: FrameMetadata {
                        generation: value(Generation::new(1)),
                        width: value(FrameDimension::new(1280)),
                        height: value(FrameDimension::new(720)),
                        capture_nanos: 2,
                        sequence: 3,
                        input_sequence: Some(value(InputSequence::new(4))),
                        fps: value(Fps::new(60)),
                        chroma: Chroma::Yuv420,
                    },
                    data: vec![0xaa, 0xbb],
                }),
                vec![
                    12, 11, 0, 0, 36, 0, 0, 0, 1, 1, 0, 0, 0, 0, 5, 208, 2, 2, 0, 0, 0, 0, 0, 0, 0,
                    3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 60, 0, 0, 0, 1, 0xaa, 0xbb,
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
                    12, 3, 0, 0, 16, 0, 0, 0, 9, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 4, 0, 0, 0,
                ],
            ),
            (
                "cursor shape",
                Event::CursorShape(CursorShape::Pointer),
                vec![12, 4, 0, 0, 1, 0, 0, 0, 4],
            ),
            (
                "cursor visibility",
                Event::CursorVisibility(CursorVisibility::Hidden),
                vec![12, 5, 0, 0, 1, 0, 0, 0, 0],
            ),
            (
                "cursor position",
                Event::CursorPosition(CursorPosition {
                    x: value(PointerCoordinate::new(10)),
                    y: value(PointerCoordinate::new(20)),
                    source: CursorSource::Agent,
                }),
                vec![12, 6, 0, 0, 9, 0, 0, 0, 10, 0, 0, 0, 20, 0, 0, 0, 1],
            ),
            (
                "cursor click",
                Event::CursorClick(CursorPosition {
                    x: value(PointerCoordinate::new(10)),
                    y: value(PointerCoordinate::new(20)),
                    source: CursorSource::Agent,
                }),
                vec![12, 10, 0, 0, 9, 0, 0, 0, 10, 0, 0, 0, 20, 0, 0, 0, 1],
            ),
            (
                "video reset refused",
                Event::ResetVideoRefused(ResetVideoRefusal::CurrentModeUnknown),
                vec![12, 7, 0, 0, 1, 0, 0, 0, 1],
            ),
        ]
    }

    fn remaining_event_cases() -> Vec<(&'static str, Event, Vec<u8>)> {
        vec![
            (
                "agent input",
                Event::AgentInput(AgentInput {
                    connected: true,
                    dropped: true,
                }),
                vec![12, 8, 0, 0, 2, 0, 0, 0, 1, 1],
            ),
            (
                "input receipt",
                Event::InputReceipt(InputReceipt {
                    sequence: value(InputSequence::new(7)),
                    delivery: ReceiptDelivery::Window,
                    window_id: value(ReceiptText::new("a1".into())),
                    app_id: value(ReceiptText::new("foot".into())),
                    title: value(ReceiptText::new("T".into())),
                }),
                vec![
                    12, 12, 0, 0, 18, 0, 0, 0, 7, 0, 0, 0, 1, 2, 0, 97, 49, 4, 0, 102, 111, 111,
                    116, 1, 0, 84,
                ],
            ),
        ]
    }

    #[test]
    fn receipt_text_is_cut_at_a_character_boundary() {
        // 511 ASCII bytes and a two-byte character straddle the limit.
        let title = format!("{}é", "a".repeat(MAX_RECEIPT_TEXT_BYTES - 1));
        assert_eq!(
            ReceiptText::truncated(&title).as_str(),
            "a".repeat(MAX_RECEIPT_TEXT_BYTES - 1)
        );
        assert!(ReceiptText::new(title).is_err());
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
    fn agent_input_round_trips_every_flag_combination() {
        for connected in [false, true] {
            for dropped in [false, true] {
                let event = Event::AgentInput(AgentInput { connected, dropped });
                assert_eq!(Event::decode(&event.encode()), Ok(event));
            }
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
    fn crf_accepts_the_encoder_range_and_rejects_values_above_it() {
        assert_eq!(Crf::new(0).map(Crf::get), Ok(0));
        assert_eq!(Crf::new(51).map(Crf::get), Ok(51));
        assert!(Crf::new(52).is_err());
    }

    #[test]
    fn quality_wire_rejects_an_invalid_crf() {
        let record = [
            12, 9, 0, 0, 14, 0, 0, 0, 64, 31, 0, 0, 60, 0, 0, 0, 75, 0, 0, 0, 52, 0,
        ];
        assert!(matches!(
            Command::decode(&record),
            Err(ProtocolError::InvalidPayload { kind: 9, .. })
        ));
    }

    #[test]
    fn quality_wire_pins_all_component_representations() {
        for (byte, chroma) in [(0, Chroma::Yuv444), (1, Chroma::Yuv420), (2, Chroma::Rgb)] {
            let record = [
                12, 9, 0, 0, 14, 0, 0, 0, 64, 31, 0, 0, 60, 0, 0, 0, 75, 0, 0, 0, 23, byte,
            ];
            let command = Command::Quality(Quality {
                bitrate_kbps: value(Kbps::new(8_000)),
                fps: value(Fps::new(60)),
                scale_percent: value(ScalePercent::new(75)),
                crf: value(Crf::new(23)),
                chroma,
            });
            assert_eq!(Command::decode(&record), Ok(command.clone()));
            assert_eq!(command.encode(), record);
        }
    }

    #[test]
    fn quality_wire_rejects_unknown_chroma() {
        let record = [
            12, 9, 0, 0, 14, 0, 0, 0, 64, 31, 0, 0, 60, 0, 0, 0, 75, 0, 0, 0, 23, 3,
        ];
        assert!(matches!(
            Command::decode(&record),
            Err(ProtocolError::InvalidPayload { kind: 9, .. })
        ));
    }

    #[test]
    fn record_length_uses_the_common_header() {
        let clipboard = [12, 7, 0, 0, 0, 0, 0, 0];
        let release_all = [12, 5, 0, 0, 0, 0, 0, 0];

        assert_eq!(Command::record_len(&clipboard), Ok(HEADER_BYTES));
        assert_eq!(Command::record_len(&release_all), Ok(HEADER_BYTES));
    }

    #[test]
    fn wrong_version_is_an_invalid_header() {
        let header = [8, 5, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            Command::record_len(&header),
            Err(ProtocolError::InvalidHeader)
        );
    }

    #[test]
    fn cursor_image_record_round_trips_and_rejects_invalid_hotspot() {
        let event = Event::CursorImage(CursorImage {
            width: 16,
            height: 24,
            hotspot_x: 2,
            hotspot_y: 3,
            output_width: 1600,
            png: b"\x89PNG\r\n\x1a\nexample".to_vec(),
        });
        let bytes = event.encode();
        assert_eq!(Event::decode(&bytes), Ok(event));
        let mut invalid = bytes;
        invalid[10] = 16;
        assert!(matches!(
            Event::decode(&invalid),
            Err(ProtocolError::InvalidPayload { kind: 9, .. })
        ));
    }

    #[test]
    fn cursor_position_rejects_out_of_range_coordinates() {
        let record = [12, 6, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0];
        assert_eq!(
            Event::decode(&record),
            Ok(Event::CursorPosition(CursorPosition {
                x: value(PointerCoordinate::new(0)),
                y: value(PointerCoordinate::new(65_535)),
                source: CursorSource::Browser,
            }))
        );
        for offset in [8, 12] {
            let mut invalid = record;
            invalid[offset..offset + 4].copy_from_slice(&65_536_u32.to_le_bytes());
            assert!(matches!(
                Event::decode(&invalid),
                Err(ProtocolError::InvalidPayload { kind: 6, .. })
            ));
        }
    }

    #[test]
    fn nonzero_reserved_byte_is_an_invalid_header() {
        let header = [11, 5, 1, 0, 0, 0, 0, 0];
        assert_eq!(
            Command::record_len(&header),
            Err(ProtocolError::InvalidHeader)
        );
    }

    #[test]
    fn unknown_command_kind_is_invalid_kind() {
        let header = [12, 99, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            Command::record_len(&header),
            Err(ProtocolError::InvalidKind { kind: 99 })
        );
    }

    #[test]
    fn unknown_event_kind_is_invalid_kind() {
        let header = [12, 99, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            Event::record_len(&header),
            Err(ProtocolError::InvalidKind { kind: 99 })
        );
    }

    #[test]
    fn command_text_payload_over_limit_is_too_large() {
        let mut header = [12, 10, 0, 0, 0, 0, 0, 0];
        header[4..8].copy_from_slice(
            &(u32::try_from(MAX_TEXT_BYTES + 6).expect("limit fits u32")).to_le_bytes(),
        );
        assert_eq!(
            Command::record_len(&header),
            Err(ProtocolError::PayloadTooLarge)
        );
    }

    #[test]
    fn event_payload_over_limit_is_too_large() {
        let mut header = [12, 1, 0, 0, 0, 0, 0, 0];
        header[4..8].copy_from_slice(
            &(u32::try_from(MAX_CLIPBOARD_BYTES + 1).expect("limit fits u32")).to_le_bytes(),
        );
        assert_eq!(
            Event::record_len(&header),
            Err(ProtocolError::PayloadTooLarge)
        );
    }

    #[test]
    fn clipboard_payload_must_be_utf8() {
        let record = [12, 1, 0, 0, 1, 0, 0, 0, 0xff];
        assert!(matches!(
            Event::decode(&record),
            Err(ProtocolError::InvalidPayload { kind: 1, .. })
        ));
    }

    #[test]
    fn text_payload_must_not_contain_nul() {
        let record = [12, 10, 0, 0, 6, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        assert!(matches!(
            Command::decode(&record),
            Err(ProtocolError::InvalidPayload { kind: 10, .. })
        ));
    }

    #[test]
    fn short_frame_payload_is_invalid() {
        let mut record = vec![12, 2, 0, 0, 32, 0, 0, 0];
        let mut payload = [0; 32];
        payload[0] = 1;
        payload[4] = 1;
        payload[6] = 1;
        payload[28] = 10;
        record.extend_from_slice(&payload);
        assert!(matches!(
            Event::decode(&record),
            Err(ProtocolError::InvalidPayload { kind: 2, .. })
        ));
    }

    #[test]
    fn frame_size_accepts_the_viewer_normalized_resizes() {
        // Sizes the viewer's normalizeResizeDimensions produces, including the
        // proportional scaling that fits the pixel cap.
        for (width, height) in [
            (2880, 2880),
            (4988, 1662),
            (6000, 1382),
            (1920, 1080),
            (3840, 2160),
            (320, 180),
        ] {
            assert!(
                FrameSize::new(width, height).is_ok(),
                "{width}x{height} should be accepted"
            );
        }
        for (width, height) in [
            (5000, 5000),
            (6000, 2000),
            (3840, 2162),
            (3842, 2160),
            (318, 1080),
            (1920, 178),
            (1000, 1001),
        ] {
            assert!(
                FrameSize::new(width, height).is_err(),
                "{width}x{height} should be rejected"
            );
        }
    }

    #[test]
    fn frame_size_accepts_the_entire_viewer_resize_envelope() {
        for width in (320..=6000).step_by(2) {
            for height in (180..=6000).step_by(2) {
                if u64::from(width) * u64::from(height) <= MAX_RAW_PIXELS {
                    assert!(FrameSize::new(width, height).is_ok(), "{width}x{height}");
                }
            }
        }
    }

    #[test]
    fn record_with_trailing_byte_is_an_invalid_header() {
        let mut record = event_cases()[1].2.clone();
        record.push(0);
        assert_eq!(Event::decode(&record), Err(ProtocolError::InvalidHeader));
    }

    #[test]
    fn command_payload_length_must_match_its_header() {
        let record = [12, 7, 0, 0, 1, 0, 0, 0];
        assert_eq!(Command::decode(&record), Err(ProtocolError::Truncated));
    }

    #[test]
    fn empty_text_payload_command_does_not_consume_the_next_header() {
        let clipboard = Command::Clipboard(
            ClipboardText::new(String::new()).expect("empty clipboard text should be valid"),
        );
        let release = Command::ReleaseAll(ReleaseAll);
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
    fn every_cursor_shape_has_its_pinned_wire_byte() {
        for (index, shape) in CursorShape::ALL.into_iter().enumerate() {
            let wire = u8::try_from(index + 1).expect("36 cursor shapes fit in a byte");
            let bytes = vec![12, 4, 0, 0, 1, 0, 0, 0, wire];
            assert_eq!(Event::CursorShape(shape).encode(), bytes);
            assert_eq!(Event::decode(&bytes), Ok(Event::CursorShape(shape)));
        }
    }

    #[test]
    fn unknown_cursor_shape_wire_byte_is_invalid() {
        let record = [12, 4, 0, 0, 1, 0, 0, 0, 37];
        assert!(matches!(
            Event::decode(&record),
            Err(ProtocolError::InvalidPayload { kind: 4, .. })
        ));
    }

    #[test]
    fn cursor_shape_json_names_are_exact() {
        assert_eq!(
            serde_json::to_string(CursorShape::ALL.as_slice())
                .expect("cursor shape names should serialize"),
            r#"["default","context-menu","help","pointer","progress","wait","cell","crosshair","text","vertical-text","alias","copy","move","no-drop","not-allowed","grab","grabbing","e-resize","n-resize","ne-resize","nw-resize","s-resize","se-resize","sw-resize","w-resize","ew-resize","ns-resize","nesw-resize","nwse-resize","col-resize","row-resize","all-scroll","zoom-in","zoom-out","dnd-ask","all-resize"]"#
        );
    }

    // Property tests over the pinned records above, which cover every kind.
    // `PROPTEST_CASES` raises the case count for longer runs (`just fuzz`).

    /// The largest payload each command kind may declare, from the field
    /// layouts: fixed kinds carry exactly their fields, text carries up to 4000
    /// bytes after its one-byte action and four-byte sequence, and a clipboard
    /// up to 1 MiB.
    const COMMAND_LIMITS: [(u8, u32); 15] = [
        (1, 12),      // pointer absolute: x, y, sequence
        (2, 9),       // pointer button: button, state, sequence
        (3, 12),      // pointer scroll: dx, dy, sequence
        (4, 9),       // keyboard key: key, state, sequence
        (5, 0),       // release all
        (6, 12),      // resize: width, height, scale, request
        (7, 1 << 20), // clipboard
        (8, 12),      // pointer relative: dx, dy, sequence
        (9, 14),      // quality: bitrate, fps, scale, CRF, chroma
        (10, 4_005),  // text
        (11, 5),      // keyframe readiness: generation, state
        (12, 0),      // reset video
        (13, 1),      // input lease
        (14, 12),     // pointer wheel
        (15, 12),     // pointer scroll stop
    ];

    /// The same for events. A cursor image is a six-byte header and a PNG of
    /// at most 8 bytes per pixel of a 128x128 cursor.
    const EVENT_LIMITS: [(u8, u32); 12] = [
        (1, 1 << 20),           // clipboard
        (2, 33),                // frame metadata
        (11, 34 + (16 << 20)),  // encoded frame: key flag, metadata, stripes
        (3, 16),                // resize applied: request, size, scale, generation
        (4, 1),                 // cursor shape
        (5, 1),                 // cursor visibility
        (6, 9),                 // cursor position: x, y, source
        (7, 1),                 // reset video refused
        (8, 2),                 // agent input
        (9, 6 + 128 * 128 * 8), // cursor image
        (10, 9),                // cursor click
        (12, 5 + 3 * 514),      // input receipt: sequence, delivery, three strings
    ];

    fn limit(table: &[(u8, u32)], kind: u8) -> Option<u32> {
        table
            .iter()
            .find(|(candidate, _)| *candidate == kind)
            .map(|(_, limit)| *limit)
    }

    fn pinned_bytes<R>(cases: Vec<(&'static str, R, Vec<u8>)>) -> Vec<Vec<u8>> {
        cases.into_iter().map(|(_, _, bytes)| bytes).collect()
    }

    /// A decoder must not accept two encodings of one value: any bytes it
    /// accepts must be exactly what the value encodes back to.
    fn assert_canonical<R: Record + PartialEq + std::fmt::Debug>(
        record: &[u8],
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        if let Ok(value) = R::decode(record) {
            proptest::prop_assert_eq!(value.encode(), record, "decoded {:?}", value);
        }
        Ok(())
    }

    /// Feed `stream` to a decoder in pieces split at `cuts` and check every
    /// record arrives once, in order, with only the unfinished one buffered.
    fn assert_chunked_stream<R: Record + PartialEq + std::fmt::Debug>(
        records: &[(R, Vec<u8>)],
        cuts: &[proptest::sample::Index],
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        let stream: Vec<u8> = records
            .iter()
            .flat_map(|(_, bytes)| bytes.clone())
            .collect();
        let mut boundaries = vec![0];
        for (_, bytes) in records {
            boundaries.push(boundaries.last().copied().unwrap_or(0) + bytes.len());
        }
        let mut cuts: Vec<usize> = cuts
            .iter()
            .map(|cut| cut.index(stream.len() + 1))
            .chain([stream.len()])
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut decoder = Decoder::<R>::new();
        let mut received = Vec::new();
        let mut start = 0;
        for cut in cuts {
            received.extend(
                decoder.push(&stream[start..cut]).map_err(|error| {
                    proptest::test_runner::TestCaseError::fail(error.to_string())
                })?,
            );
            start = cut;
            let complete = boundaries
                .iter()
                .rev()
                .find(|boundary| **boundary <= cut)
                .copied()
                .unwrap_or(0);
            proptest::prop_assert_eq!(decoder.pending(), cut - complete);
            proptest::prop_assert_eq!(
                received.len(),
                boundaries
                    .iter()
                    .filter(|boundary| (1..=cut).contains(*boundary))
                    .count()
            );
        }
        let expected: Vec<&R> = records.iter().map(|(record, _)| record).collect();
        proptest::prop_assert_eq!(received.iter().collect::<Vec<_>>(), expected);
        proptest::prop_assert!(decoder.finish().is_ok());
        Ok(())
    }

    #[test]
    fn every_strict_prefix_of_a_record_is_truncated() {
        fn check<R: Record + std::fmt::Debug>(records: &[Vec<u8>]) {
            for record in records {
                for length in 0..record.len() {
                    let prefix = &record[..length];
                    assert!(
                        matches!(R::decode(prefix), Err(ProtocolError::Truncated)),
                        "{prefix:?} should be truncated"
                    );
                    let mut decoder = Decoder::<R>::new();
                    assert!(
                        decoder.push(prefix).is_ok_and(|records| records.is_empty()),
                        "{prefix:?} should wait for the rest of the record"
                    );
                    assert_eq!(decoder.pending(), length);
                    if length > 0 {
                        assert_eq!(decoder.finish(), Err(ProtocolError::Truncated));
                    }
                }
            }
        }
        check::<Command>(&pinned_bytes(command_cases()));
        check::<Event>(&pinned_bytes(event_cases()));
    }

    proptest::proptest! {
        #[test]
        fn mutated_records_decode_only_to_their_own_encoding(
            command in proptest::sample::select(pinned_bytes(command_cases())),
            event in proptest::sample::select(pinned_bytes(event_cases())),
            mutations in proptest::collection::vec(
                (proptest::prelude::any::<proptest::sample::Index>(), proptest::prelude::any::<u8>()),
                1..4,
            ),
        ) {
            for mut record in [command.clone(), event.clone()] {
                // Keep the header so the mutations reach the payload decoders.
                let payload = record.len() - HEADER_BYTES;
                if payload == 0 {
                    continue;
                }
                for (index, byte) in &mutations {
                    record[HEADER_BYTES + index.index(payload)] = *byte;
                }
                if record[..HEADER_BYTES] == command[..HEADER_BYTES] {
                    assert_canonical::<Command>(&record)?;
                } else {
                    assert_canonical::<Event>(&record)?;
                }
            }
        }

        #[test]
        fn command_streams_decode_the_same_however_they_are_split(
            picks in proptest::collection::vec(proptest::sample::select(command_cases()), 0..12),
            cuts in proptest::collection::vec(proptest::prelude::any::<proptest::sample::Index>(), 0..24),
        ) {
            let records: Vec<_> = picks.into_iter().map(|(_, record, bytes)| (record, bytes)).collect();
            assert_chunked_stream(&records, &cuts)?;
        }

        #[test]
        fn event_streams_decode_the_same_however_they_are_split(
            picks in proptest::collection::vec(proptest::sample::select(event_cases()), 0..12),
            cuts in proptest::collection::vec(proptest::prelude::any::<proptest::sample::Index>(), 0..24),
        ) {
            let records: Vec<_> = picks.into_iter().map(|(_, record, bytes)| (record, bytes)).collect();
            assert_chunked_stream(&records, &cuts)?;
        }

        #[test]
        fn headers_with_another_version_or_nonzero_reserved_bytes_are_invalid(
            // Bias each checked byte toward its valid value so every
            // combination of one wrong byte with two right ones is common.
            version in proptest::prop_oneof![proptest::strategy::Just(12_u8), proptest::prelude::any::<u8>()],
            reserved in proptest::array::uniform2(
                proptest::prop_oneof![proptest::strategy::Just(0_u8), proptest::prelude::any::<u8>()],
            ),
            kind in 1..=10_u8,
            length in 0..=1_u32,
        ) {
            let mut reserved = reserved;
            if version == 12 && reserved == [0, 0] {
                // Make one reserved byte wrong rather than discard the case.
                reserved[usize::from(kind % 2)] = kind;
            }
            let mut header = [version, kind, reserved[0], reserved[1], 0, 0, 0, 0];
            header[4..].copy_from_slice(&length.to_le_bytes());
            proptest::prop_assert_eq!(Command::record_len(&header), Err(ProtocolError::InvalidHeader));
            proptest::prop_assert_eq!(Event::record_len(&header), Err(ProtocolError::InvalidHeader));
            proptest::prop_assert_eq!(
                Decoder::<Command>::new().push(&header),
                Err(ProtocolError::InvalidHeader)
            );
        }

        #[test]
        fn declared_lengths_are_checked_against_each_kind_before_buffering(
            kind in proptest::prelude::any::<u8>(),
            length in proptest::prelude::any::<u32>(),
            // One byte over the limit is the likeliest mistake; bias toward it.
            excess in proptest::prop_oneof![1..=4_u32, 1..=u32::MAX],
        ) {
            for (limit, record_len, push) in [
                (
                    limit(&COMMAND_LIMITS, kind),
                    Command::record_len as fn(&[u8]) -> Result<usize, ProtocolError>,
                    (|header: &[u8]| Decoder::<Command>::new().push(header).map(|records| records.len()))
                        as fn(&[u8]) -> Result<usize, ProtocolError>,
                ),
                (
                    limit(&EVENT_LIMITS, kind),
                    Event::record_len,
                    |header: &[u8]| Decoder::<Event>::new().push(header).map(|records| records.len()),
                ),
            ] {
                let header = |length: u32| {
                    let mut header = [12, kind, 0, 0, 0, 0, 0, 0];
                    header[4..].copy_from_slice(&length.to_le_bytes());
                    header
                };
                let Some(limit) = limit else {
                    proptest::prop_assert_eq!(record_len(&header(length)), Err(ProtocolError::InvalidKind { kind }));
                    proptest::prop_assert_eq!(push(&header(length)), Err(ProtocolError::InvalidKind { kind }));
                    continue;
                };
                let within = length % (limit + 1);
                proptest::prop_assert_eq!(
                    record_len(&header(within)),
                    Ok(HEADER_BYTES + usize::try_from(within).expect("u32 fits usize"))
                );
                if limit > 0 {
                    // A nonempty declared payload within the limit is awaited.
                    let awaited = 1 + length % limit;
                    proptest::prop_assert_eq!(push(&header(awaited)), Ok(0));
                }
                let beyond = limit.saturating_add(excess);
                if beyond > limit {
                    proptest::prop_assert_eq!(record_len(&header(beyond)), Err(ProtocolError::PayloadTooLarge));
                    proptest::prop_assert_eq!(push(&header(beyond)), Err(ProtocolError::PayloadTooLarge));
                }
            }
        }
    }
}
