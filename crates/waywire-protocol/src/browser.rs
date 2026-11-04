//! Browser-to-gateway and gateway-to-browser protocol vocabulary.
//! It covers JSON control messages, binary control records, and binary video frames.

use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeStruct;
use thiserror::Error;

use crate::PROTOCOL_VERSION;
use crate::ProtocolError;
use crate::pipe;
use crate::pipe::ClipboardText;
use crate::pipe::Command;
use crate::pipe::CursorPosition;
use crate::pipe::CursorShape;
use crate::pipe::CursorVisibility;
use crate::pipe::Fps;
use crate::pipe::FrameMetadata;
use crate::pipe::InputSequence;
use crate::pipe::InputText;
use crate::pipe::Kbps;
use crate::pipe::ScalePercent;
pub use crate::pipe::TextAction;
use crate::wire::InvalidValue;
use crate::wire::Reader;
use crate::wire::Record;
use crate::wire::RecordKind;
use crate::wire::Wire;
use crate::wire::Writer;

/// Matches the gateway RTP assembler's largest accepted access unit.
pub const MAX_VIDEO_DATA_BYTES: usize = 16 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientEvent {
    #[serde(rename_all = "camelCase")]
    VideoConfig {
        version: u8,
        codec: String,
    },
    Cursor(CursorState),
    Clipboard {
        text: ClipboardText,
    },
    ClipboardSnapshot {
        text: ClipboardText,
    },
    ResizeApplied(pipe::ResizeApplied),
    ResetVideoRefused {
        reason: pipe::ResetVideoRefusal,
    },
    Quality(QualityLevels),
    ControlState {
        state: ControlState,
    },
    AgentInput(pipe::AgentInputState),
    /// Where one of this viewer's input events went.
    #[serde(rename_all = "camelCase")]
    InputReceipt {
        sequence: InputSequence,
        delivery: pipe::ReceiptDelivery,
        window_id: pipe::ReceiptText,
        app_id: pipe::ReceiptText,
        title: pipe::ReceiptText,
    },
    #[serde(rename_all = "camelCase")]
    Pong {
        id: u64,
        server_nanos: String,
    },
}

impl ClientEvent {
    #[must_use]
    pub fn video_config(codec: String) -> Self {
        Self::VideoConfig {
            version: PROTOCOL_VERSION,
            codec,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlState {
    Active,
    Busy,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CursorState {
    #[serde(rename = "visible")]
    visibility: CursorVisibility,
    shape: CursorShape,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<CursorImage>,
    #[serde(flatten)]
    event: CursorEvent,
}

impl CursorState {
    #[must_use]
    pub fn new(
        visibility: CursorVisibility,
        shape: CursorShape,
        image: Option<CursorImage>,
        event: CursorEvent,
    ) -> Self {
        Self {
            visibility,
            shape,
            image,
            event,
        }
    }
    #[must_use]
    pub fn visibility(&self) -> CursorVisibility {
        self.visibility
    }
    #[must_use]
    pub fn shape(&self) -> CursorShape {
        self.shape
    }
    #[must_use]
    pub fn image(&self) -> &Option<CursorImage> {
        &self.image
    }

    pub fn set_shape(&mut self, shape: CursorShape) {
        self.shape = shape;
        self.image = None;
    }

    pub fn set_image(&mut self, image: CursorImage) {
        self.image = Some(image);
    }

    pub fn set_visibility(&mut self, visibility: CursorVisibility) {
        self.visibility = visibility;
    }

    pub fn set_position(&mut self, position: CursorPosition) {
        self.event = CursorEvent::State(Some(position));
    }

    #[must_use]
    pub fn clicked(mut self, position: CursorPosition) -> Self {
        self.event = CursorEvent::Click(position);
        self
    }
}

/// A click always has a position; ordinary state may precede the first position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorEvent {
    State(Option<CursorPosition>),
    Click(CursorPosition),
}

impl Serialize for CursorEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let position = match self {
            Self::State(position) => *position,
            Self::Click(position) => Some(*position),
        };
        let mut state =
            serializer.serialize_struct("CursorEvent", 1 + usize::from(position.is_some()))?;
        if let Some(position) = position {
            state.serialize_field("position", &position)?;
        }
        state.serialize_field("click", &matches!(self, Self::Click(_)))?;
        state.end()
    }
}

impl Serialize for pipe::AgentInputState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("AgentInput", 2)?;
        state.serialize_field("connected", &self.connected())?;
        state.serialize_field("dropped", &self.dropped())?;
        state.end()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorImage {
    url: String,
    width: u8,
    height: u8,
    hotspot_x: u8,
    hotspot_y: u8,
    output_width: u16,
}

impl CursorImage {
    #[must_use]
    pub fn new(
        url: String,
        width: u8,
        height: u8,
        hotspot_x: u8,
        hotspot_y: u8,
        output_width: u16,
    ) -> Self {
        Self {
            url,
            width,
            height,
            hotspot_x,
            hotspot_y,
            output_width,
        }
    }
    #[must_use]
    pub fn url(&self) -> &String {
        &self.url
    }
    #[must_use]
    pub fn width(&self) -> u8 {
        self.width
    }
    #[must_use]
    pub fn height(&self) -> u8 {
        self.height
    }
    #[must_use]
    pub fn hotspot_x(&self) -> u8 {
        self.hotspot_x
    }
    #[must_use]
    pub fn hotspot_y(&self) -> u8 {
        self.hotspot_y
    }
    #[must_use]
    pub fn output_width(&self) -> u16 {
        self.output_width
    }
}

impl Serialize for CursorVisibility {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bool(matches!(self, Self::Visible))
    }
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            visibility: CursorVisibility::Visible,
            shape: CursorShape::Default,
            image: None,
            event: CursorEvent::State(None),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct QualityLevels {
    bitrate: Kbps,
    fps: Fps,
    scale: ScalePercent,
}

impl QualityLevels {
    #[must_use]
    pub fn new(bitrate: Kbps, fps: Fps, scale: ScalePercent) -> Self {
        Self {
            bitrate,
            fps,
            scale,
        }
    }
    #[must_use]
    pub fn bitrate(&self) -> Kbps {
        self.bitrate
    }
    #[must_use]
    pub fn fps(&self) -> Fps {
        self.fps
    }
    #[must_use]
    pub fn scale(&self) -> ScalePercent {
        self.scale
    }

    pub fn set_bitrate(&mut self, value: Kbps) {
        self.bitrate = value;
    }
    pub fn set_fps(&mut self, value: Fps) {
        self.fps = value;
    }
    pub fn set_scale(&mut self, value: ScalePercent) {
        self.scale = value;
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QualityPreset {
    Automatic,
    High,
    Medium,
    Low,
}

#[derive(Debug, PartialEq)]
pub enum ClientMessage {
    AcquireControl,
    ReleaseControl,
    Ping {
        id: u64,
    },
    SetQuality {
        preset: QualityPreset,
    },
    Feedback(Feedback),
    Text {
        action: TextAction,
        text: InputText,
        sequence: InputSequence,
    },
    ClipboardWrite {
        text: ClipboardText,
    },
    ResetVideo,
    /// A viewer of a striped stream lost its decoder state and waits for a key frame.
    RequestKeyFrame,
}

impl ClientMessage {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, BrowserError> {
        let input: JsonInput = serde_json::from_slice(bytes).map_err(BrowserError::InvalidJson)?;
        match input {
            JsonInput::Acquire => Ok(Self::AcquireControl),
            JsonInput::Release => Ok(Self::ReleaseControl),
            JsonInput::Ping { id } => Ok(Self::Ping { id }),
            JsonInput::SetQuality { preset } => Ok(Self::SetQuality { preset }),
            JsonInput::Feedback(values) => Feedback::new(values).map(Self::Feedback),
            JsonInput::Text {
                action,
                text,
                sequence,
            } => Ok(Self::Text {
                action,
                text: InputText::new(text).map_err(BrowserError::InvalidText)?,
                sequence: InputSequence::new(sequence).map_err(BrowserError::InvalidSequence)?,
            }),
            JsonInput::ClipboardWrite { text } => Ok(Self::ClipboardWrite {
                text: ClipboardText::new(text).map_err(BrowserError::InvalidClipboard)?,
            }),
            JsonInput::ResetVideo => Ok(Self::ResetVideo),
            JsonInput::RequestKeyFrame => Ok(Self::RequestKeyFrame),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum JsonInput {
    Acquire,
    Release,
    Ping {
        id: u64,
    },
    SetQuality {
        preset: QualityPreset,
    },
    Feedback(FeedbackValues),
    Text {
        action: TextAction,
        text: String,
        sequence: u32,
    },
    ClipboardWrite {
        text: String,
    },
    ResetVideo,
    RequestKeyFrame,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeedbackValues {
    received: u32,
    presented: u32,
    queue_peak: u32,
    queue_busy_ms: f64,
    sample_ms: f64,
    /// Capacity/reset loss, not decoded frames superseded by a newer canvas draw.
    dropped: u32,
    rtt: f64,
    /// Stream payload bytes the viewer received during the sample.
    bytes: u32,
}

impl FeedbackValues {
    #[expect(
        clippy::too_many_arguments,
        reason = "constructor names every field of the wire record"
    )]
    #[must_use]
    pub fn new(
        received: u32,
        presented: u32,
        queue_peak: u32,
        queue_busy_ms: f64,
        sample_ms: f64,
        dropped: u32,
        rtt: f64,
        bytes: u32,
    ) -> Self {
        Self {
            received,
            presented,
            queue_peak,
            queue_busy_ms,
            sample_ms,
            dropped,
            rtt,
            bytes,
        }
    }
    #[must_use]
    pub fn received(&self) -> u32 {
        self.received
    }
    #[must_use]
    pub fn presented(&self) -> u32 {
        self.presented
    }
    #[must_use]
    pub fn queue_peak(&self) -> u32 {
        self.queue_peak
    }
    #[must_use]
    pub fn queue_busy_ms(&self) -> f64 {
        self.queue_busy_ms
    }
    #[must_use]
    pub fn sample_ms(&self) -> f64 {
        self.sample_ms
    }
    #[must_use]
    pub fn dropped(&self) -> u32 {
        self.dropped
    }
    #[must_use]
    pub fn rtt(&self) -> f64 {
        self.rtt
    }
    #[must_use]
    pub fn bytes(&self) -> u32 {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Feedback(FeedbackValues);

impl Feedback {
    pub fn new(values: FeedbackValues) -> Result<Self, BrowserError> {
        if values.received > 10_000 {
            return Err(BrowserError::InvalidFeedback("received out of range"));
        }
        if values.presented > 10_000 {
            return Err(BrowserError::InvalidFeedback("presented out of range"));
        }
        if values.queue_peak > 64 {
            return Err(BrowserError::InvalidFeedback("queuePeak out of range"));
        }
        if !values.sample_ms.is_finite()
            || !(0.0..=60_000.0).contains(&values.sample_ms)
            || values.sample_ms == 0.0
        {
            return Err(BrowserError::InvalidFeedback("sampleMs out of range"));
        }
        if !values.queue_busy_ms.is_finite()
            || values.queue_busy_ms < 0.0
            || values.queue_busy_ms > values.sample_ms
        {
            return Err(BrowserError::InvalidFeedback("queueBusyMs out of range"));
        }
        if values.dropped > 10_000 {
            return Err(BrowserError::InvalidFeedback("dropped out of range"));
        }
        if !values.rtt.is_finite() || !(0.0..=60_000.0).contains(&values.rtt) {
            return Err(BrowserError::InvalidFeedback("rtt out of range"));
        }
        Ok(Self(values))
    }

    #[must_use]
    pub const fn received(self) -> u32 {
        self.0.received
    }

    #[must_use]
    pub const fn presented(self) -> u32 {
        self.0.presented
    }

    #[must_use]
    pub const fn queue_peak(self) -> u32 {
        self.0.queue_peak
    }

    #[must_use]
    pub const fn queue_busy_ms(self) -> f64 {
        self.0.queue_busy_ms
    }

    #[must_use]
    pub const fn sample_ms(self) -> f64 {
        self.0.sample_ms
    }

    #[must_use]
    pub const fn dropped(self) -> u32 {
        self.0.dropped
    }

    #[must_use]
    pub const fn rtt(self) -> f64 {
        self.0.rtt
    }

    #[must_use]
    pub const fn bytes(self) -> u32 {
        self.0.bytes
    }
}

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("invalid browser control record")]
    InvalidControl(#[source] ProtocolError),
    #[error("browser may not send control kind {0}")]
    PrivateControlKind(u8),
    #[error("invalid JSON control")]
    InvalidJson(#[source] serde_json::Error),
    #[error("invalid feedback: {0}")]
    InvalidFeedback(&'static str),
    #[error("invalid text")]
    InvalidText(#[source] InvalidValue),
    #[error("invalid input sequence")]
    InvalidSequence(#[source] InvalidValue),
    #[error("invalid clipboard")]
    InvalidClipboard(#[source] InvalidValue),
}

pub fn parse_browser_record(bytes: &[u8]) -> Result<Command, BrowserError> {
    let command = Command::decode(bytes).map_err(BrowserError::InvalidControl)?;
    let kind = command.kind();
    if !kind.browser_input() {
        return Err(BrowserError::PrivateControlKind(RecordKind::wire(kind)));
    }
    Ok(command)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Delta,
    Key,
}

impl Wire for FrameKind {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Delta => 0_u8,
            Self::Key => 1,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self::Delta),
            1 => Ok(Self::Key),
            _ => Err(InvalidValue::new("frame kind must be zero or one")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Continuity {
    Continuous,
    AfterGap,
}

impl Wire for Continuity {
    fn write(&self, out: &mut Writer) {
        out.put(&match self {
            Self::Continuous => 0_u8,
            Self::AfterGap => 1,
        });
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match input.get()? {
            0_u8 => Ok(Self::Continuous),
            1 => Ok(Self::AfterGap),
            _ => Err(InvalidValue::new("continuity must be zero or one")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BrowserKind {
    Frame = 1,
    /// A frame whose data is the stripe container ([`push_stripe`]).
    StripedFrame = 2,
}

impl RecordKind for BrowserKind {
    fn wire(self) -> u8 {
        match self {
            Self::Frame => 1,
            Self::StripedFrame => 2,
        }
    }

    fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Frame),
            2 => Some(Self::StripedFrame),
            _ => None,
        }
    }

    fn max_payload(self) -> usize {
        match self {
            // Frame kind u8, continuity u8, 33-byte metadata, then the encoded access unit.
            Self::Frame | Self::StripedFrame => 35 + MAX_VIDEO_DATA_BYTES,
        }
    }
}

/// Bytes before each stripe's access unit in a video payload.
pub const STRIPE_HEADER_BYTES: usize = 9;

/// Append one stripe to a video payload. A payload carries one or more horizontal
/// stripes of the frame, each its own H.264 stream: the stripe's top row and height
/// (u16 LE each), flags (u8, bit 0 set for a key frame), the access unit's length
/// (u32 LE), then the Annex-B access unit.
pub fn push_stripe(out: &mut Vec<u8>, y: u16, height: u16, kind: FrameKind, unit: &[u8]) {
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.push(match kind {
        FrameKind::Delta => 0,
        FrameKind::Key => 1,
    });
    out.extend_from_slice(&u32::try_from(unit.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(unit);
}

/// How a sample's data carries the picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// One Annex-B access unit of the whole frame.
    Whole,
    /// The stripe container of the stripes that changed ([`push_stripe`]).
    Stripes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSample {
    data: Arc<[u8]>,
    layout: Layout,
    kind: FrameKind,
    continuity: Continuity,
    metadata: FrameMetadata,
}

impl VideoSample {
    #[must_use]
    pub fn new(
        data: Arc<[u8]>,
        layout: Layout,
        kind: FrameKind,
        continuity: Continuity,
        metadata: FrameMetadata,
    ) -> Self {
        Self {
            data,
            layout,
            kind,
            continuity,
            metadata,
        }
    }
    #[must_use]
    pub fn data(&self) -> &Arc<[u8]> {
        &self.data
    }
    #[must_use]
    pub fn layout(&self) -> Layout {
        self.layout
    }
    #[must_use]
    pub fn kind(&self) -> FrameKind {
        self.kind
    }
    #[must_use]
    pub fn continuity(&self) -> Continuity {
        self.continuity
    }
    #[must_use]
    pub fn metadata(&self) -> &FrameMetadata {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut FrameMetadata {
        &mut self.metadata
    }

    pub fn set_continuity(&mut self, value: Continuity) {
        self.continuity = value;
    }
}

impl Record for VideoSample {
    type Kind = BrowserKind;

    fn kind(&self) -> Self::Kind {
        match self.layout {
            Layout::Whole => BrowserKind::Frame,
            Layout::Stripes => BrowserKind::StripedFrame,
        }
    }

    fn write_payload(&self, out: &mut Writer) {
        out.put(&self.kind);
        out.put(&self.continuity);
        out.put(&self.metadata);
        out.bytes(&self.data);
    }

    fn read_payload(kind: Self::Kind, input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self {
            layout: match kind {
                BrowserKind::Frame => Layout::Whole,
                BrowserKind::StripedFrame => Layout::Stripes,
            },
            kind: input.get()?,
            continuity: input.get()?,
            metadata: input.get()?,
            data: Arc::from(input.rest()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<T>(result: Result<T, InvalidValue>) -> T {
        result.expect("test protocol value should be valid")
    }

    fn json(event: &ClientEvent) -> String {
        serde_json::to_string(event).expect("test client event should serialize")
    }

    fn record(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![13, kind, 0, 0];
        bytes.extend(
            u32::try_from(payload.len())
                .expect("test payload length should fit u32")
                .to_le_bytes(),
        );
        bytes.extend(payload);
        bytes
    }

    #[test]
    fn video_config_json_is_exact_for_each_chroma_profile() {
        for (chroma, codec) in [
            (pipe::Chroma::Yuv444, "avc1.F40034"),
            (pipe::Chroma::Yuv420, "avc1.640034"),
            (pipe::Chroma::Rgb, "avc1.F40034"),
        ] {
            let event = ClientEvent::video_config(chroma.h264_profile().codec());
            assert_eq!(
                json(&event),
                format!(r#"{{"type":"video-config","version":13,"codec":"{codec}"}}"#)
            );
        }
    }

    #[test]
    fn cursor_json_is_exact() {
        let cursor = CursorState {
            visibility: CursorVisibility::Visible,
            shape: CursorShape::Pointer,
            image: None,
            event: CursorEvent::State(Some(CursorPosition::new(
                value(pipe::PointerCoordinate::new(10)),
                value(pipe::PointerCoordinate::new(20)),
                pipe::CursorSource::Browser,
            ))),
        };
        assert_eq!(
            json(&ClientEvent::Cursor(cursor.clone())),
            r#"{"type":"cursor","visible":true,"shape":"pointer","position":{"x":10,"y":20,"source":"browser"},"click":false}"#
        );
        assert_eq!(
            json(&ClientEvent::Cursor(cursor.clicked(CursorPosition::new(
                value(pipe::PointerCoordinate::new(30)),
                value(pipe::PointerCoordinate::new(40)),
                pipe::CursorSource::Agent,
            )))),
            r#"{"type":"cursor","visible":true,"shape":"pointer","position":{"x":30,"y":40,"source":"agent"},"click":true}"#
        );
    }

    #[test]
    fn cursor_image_json_is_exact() {
        let cursor = CursorState {
            image: Some(CursorImage {
                url: "data:image/png;base64,iVBORw0KGgo=".into(),
                width: 16,
                height: 24,
                hotspot_x: 2,
                hotspot_y: 3,
                output_width: 1600,
            }),
            ..CursorState::default()
        };
        assert_eq!(
            json(&ClientEvent::Cursor(cursor)),
            r#"{"type":"cursor","visible":true,"shape":"default","image":{"url":"data:image/png;base64,iVBORw0KGgo=","width":16,"height":24,"hotspotX":2,"hotspotY":3,"outputWidth":1600},"click":false}"#
        );
    }

    #[test]
    fn cursor_without_a_position_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::Cursor(CursorState::default())),
            r#"{"type":"cursor","visible":true,"shape":"default","click":false}"#
        );
    }

    #[test]
    fn clipboard_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::Clipboard {
                text: value(ClipboardText::new("hello".into()))
            }),
            r#"{"type":"clipboard","text":"hello"}"#
        );
        assert_eq!(
            json(&ClientEvent::ClipboardSnapshot {
                text: value(ClipboardText::new("hello".into()))
            }),
            r#"{"type":"clipboard-snapshot","text":"hello"}"#
        );
    }

    #[test]
    fn resize_applied_json_is_exact() {
        let event = ClientEvent::ResizeApplied(pipe::ResizeApplied::new(
            value(pipe::RequestId::new(9)),
            value(pipe::FrameSize::new(1280, 720)),
            value(pipe::ScaleV120::new(180)),
            value(pipe::Generation::new(4)),
        ));
        assert_eq!(
            json(&event),
            r#"{"type":"resize-applied","request":9,"width":1280,"height":720,"scale":180,"generation":4}"#
        );
    }

    #[test]
    fn reset_video_refused_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::ResetVideoRefused {
                reason: pipe::ResetVideoRefusal::CurrentModeUnknown,
            }),
            r#"{"type":"reset-video-refused","reason":"current-mode-unknown"}"#
        );
    }

    #[test]
    fn quality_json_is_exact() {
        let event = ClientEvent::Quality(QualityLevels {
            bitrate: value(Kbps::new(8_000)),
            fps: value(Fps::new(60)),
            scale: value(ScalePercent::new(75)),
        });
        assert_eq!(
            json(&event),
            r#"{"type":"quality","bitrate":8000,"fps":60,"scale":75}"#
        );
    }

    #[test]
    fn control_state_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::ControlState {
                state: ControlState::Active
            }),
            r#"{"type":"control-state","state":"active"}"#
        );
    }

    #[test]
    fn agent_input_json_is_exact() {
        for (state, expected) in [
            (
                pipe::AgentInputState::None,
                r#"{"type":"agent-input","connected":false,"dropped":false}"#,
            ),
            (
                pipe::AgentInputState::Driving,
                r#"{"type":"agent-input","connected":true,"dropped":false}"#,
            ),
            (
                pipe::AgentInputState::Paused,
                r#"{"type":"agent-input","connected":true,"dropped":true}"#,
            ),
            (
                pipe::AgentInputState::PausedDisconnected,
                r#"{"type":"agent-input","connected":false,"dropped":true}"#,
            ),
        ] {
            assert_eq!(json(&ClientEvent::AgentInput(state)), expected);
        }
    }

    #[test]
    fn input_receipt_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::InputReceipt {
                sequence: InputSequence::new(41).expect("sequence"),
                delivery: pipe::ReceiptDelivery::Dropped,
                window_id: pipe::ReceiptText::truncated("a1"),
                app_id: pipe::ReceiptText::truncated("foot"),
                title: pipe::ReceiptText::truncated("Terminal"),
            }),
            r#"{"type":"input-receipt","sequence":41,"delivery":"dropped","windowId":"a1","appId":"foot","title":"Terminal"}"#
        );
    }

    #[test]
    fn pong_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::Pong {
                id: 3,
                server_nanos: "99".into()
            }),
            r#"{"type":"pong","id":3,"serverNanos":"99"}"#
        );
    }

    fn browser_record_cases() -> Vec<(&'static str, Vec<u8>, Vec<u8>)> {
        vec![
            (
                "pointer absolute",
                record(1, &[13, 0, 0, 0, 34, 0, 0, 0, 7, 0, 0, 0]),
                record(1, &[0, 0, 1, 0, 34, 0, 0, 0, 7, 0, 0, 0]),
            ),
            (
                "pointer button",
                record(2, &[16, 1, 0, 0, 1, 7, 0, 0, 0]),
                record(2, &[21, 1, 0, 0, 1, 7, 0, 0, 0]),
            ),
            (
                "pointer scroll",
                record(3, &[0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0]),
                record(3, &[0, 0, 192, 127, 0, 0, 0, 0, 7, 0, 0, 0]),
            ),
            (
                "keyboard key",
                record(4, &[30, 0, 0, 0, 2, 7, 0, 0, 0]),
                record(4, &[0, 1, 0, 0, 2, 7, 0, 0, 0]),
            ),
            ("release all", record(5, &[]), record(5, &[1])),
            (
                "resize",
                record(6, &[0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 9, 0]),
                record(6, &[1, 5, 0, 0, 208, 2, 0, 0, 180, 0, 9, 0]),
            ),
            (
                "pointer relative",
                record(8, &[0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0]),
                record(8, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            ),
            ("reset video", record(12, &[]), record(12, &[1])),
        ]
    }

    #[test]
    fn browser_records_accept_valid_input() {
        for (name, accepted, _) in browser_record_cases() {
            assert!(
                parse_browser_record(&accepted).is_ok(),
                "{name} should be accepted"
            );
        }
    }

    #[test]
    fn browser_records_reject_invalid_input() {
        for (name, _, rejected) in browser_record_cases() {
            assert!(
                parse_browser_record(&rejected).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn resize_records_in_the_viewer_layout_parse_across_its_envelope_and_scales() {
        // The viewer's resizeRecord layout: header, u32 width, u32 height,
        // u16 scale, u16 request. Its sizes are even, 320 to 6000 by 180 to
        // 6000 within the shared pixel cap, and its scales 120 to 480.
        let record = |width: u32, height: u32, scale: u16| {
            let mut bytes = vec![PROTOCOL_VERSION, 6, 0, 0, 12, 0, 0, 0];
            bytes.extend(width.to_le_bytes());
            bytes.extend(height.to_le_bytes());
            bytes.extend(scale.to_le_bytes());
            bytes.extend(7_u16.to_le_bytes());
            bytes
        };
        for (width, height) in [
            (320, 180),
            (6000, 180),
            (320, 6000),
            (6000, 1382),
            (4988, 1662),
            (2880, 2880),
            (3840, 2160),
        ] {
            for scale in 120..=480 {
                let Ok(Command::Resize(resize)) =
                    parse_browser_record(&record(width, height, scale))
                else {
                    panic!("{width}x{height}@{scale} should parse as a resize");
                };
                assert_eq!(
                    (
                        resize.size().width(),
                        resize.size().height(),
                        resize.scale_v120().get()
                    ),
                    (width, height, scale)
                );
                assert_eq!(resize.request_id().get(), 7);
            }
            for scale in [119, 481] {
                assert!(parse_browser_record(&record(width, height, scale)).is_err());
            }
        }
    }

    #[test]
    fn browser_records_reject_private_kinds() {
        let commands = [
            Command::Clipboard(value(ClipboardText::new(String::new()))),
            Command::Quality(pipe::Quality::new(
                value(Kbps::new(8_000)),
                value(Fps::new(60)),
                value(ScalePercent::new(100)),
                value(pipe::Crf::new(23)),
                pipe::Chroma::Yuv444,
            )),
            Command::Text(pipe::Text::new(
                TextAction::Commit,
                value(InputSequence::new(1)),
                value(InputText::new(String::new())),
            )),
            Command::KeyframeReadiness(pipe::KeyframeReadiness::new(
                value(pipe::Generation::new(1)),
                pipe::KeyframeState::Cached,
            )),
        ];
        for command in commands {
            let record = command.encode();
            let kind = record[1];
            assert!(
                matches!(
                    parse_browser_record(&record),
                    Err(BrowserError::PrivateControlKind(actual)) if actual == kind
                ),
                "private kind {kind} should be rejected"
            );
        }
    }

    #[test]
    fn feedback_is_parsed_and_validated_in_one_step() {
        let valid = br#"{"type":"feedback","received":40,"presented":42,"queuePeak":2,"queueBusyMs":25,"sampleMs":1000,"dropped":0,"rtt":20,"bytes":250000}"#;
        assert!(matches!(
            ClientMessage::parse_json(valid),
            Ok(ClientMessage::Feedback(_))
        ));

        let invalid = br#"{"type":"feedback","received":1,"presented":1,"queuePeak":2,"queueBusyMs":0,"sampleMs":0,"dropped":0,"rtt":20,"bytes":250000}"#;
        let error = ClientMessage::parse_json(invalid).map_err(|error| error.to_string());
        assert_eq!(error, Err("invalid feedback: sampleMs out of range".into()));
    }

    #[test]
    fn parses_control_ownership_json() {
        assert_eq!(
            ClientMessage::parse_json(br#"{"type":"acquire"}"#).expect("acquire should parse"),
            ClientMessage::AcquireControl
        );
        assert_eq!(
            ClientMessage::parse_json(br#"{"type":"release"}"#).expect("release should parse"),
            ClientMessage::ReleaseControl
        );
    }

    #[test]
    fn parses_reset_video_json() {
        assert_eq!(
            ClientMessage::parse_json(br#"{"type":"reset-video"}"#)
                .expect("reset video should parse"),
            ClientMessage::ResetVideo
        );
        assert_eq!(
            ClientMessage::parse_json(br#"{"type":"request-key-frame"}"#)
                .expect("key frame request should parse"),
            ClientMessage::RequestKeyFrame
        );
    }

    #[test]
    fn parses_ping_json() {
        assert_eq!(
            ClientMessage::parse_json(br#"{"type":"ping","id":3}"#).expect("ping should parse"),
            ClientMessage::Ping { id: 3 }
        );
    }

    #[test]
    fn parses_quality_presets() {
        let cases = [
            ("automatic", QualityPreset::Automatic),
            ("high", QualityPreset::High),
            ("medium", QualityPreset::Medium),
            ("low", QualityPreset::Low),
        ];

        for (name, preset) in cases {
            let message = format!(r#"{{"type":"set-quality","preset":"{name}"}}"#);
            assert_eq!(
                ClientMessage::parse_json(message.as_bytes()).expect("quality preset should parse"),
                ClientMessage::SetQuality { preset }
            );
        }
    }

    #[test]
    fn rejects_unknown_quality_preset() {
        assert!(ClientMessage::parse_json(br#"{"type":"set-quality","preset":"ultra"}"#).is_err());
    }

    #[test]
    fn parses_text_json() {
        assert_eq!(
            ClientMessage::parse_json(
                br#"{"type":"text","action":"commit","text":"hello","sequence":7}"#
            )
            .expect("text message should parse"),
            ClientMessage::Text {
                action: TextAction::Commit,
                sequence: value(InputSequence::new(7)),
                text: value(InputText::new("hello".into())),
            }
        );
    }

    #[test]
    fn parses_clipboard_write_json() {
        assert_eq!(
            ClientMessage::parse_json(br#"{"type":"clipboard-write","text":"hello"}"#)
                .expect("clipboard write should parse"),
            ClientMessage::ClipboardWrite {
                text: value(ClipboardText::new("hello".into())),
            }
        );
    }

    #[test]
    fn rejects_zero_text_sequence() {
        assert!(matches!(
            ClientMessage::parse_json(
                br#"{"type":"text","action":"commit","text":"hello","sequence":0}"#
            ),
            Err(BrowserError::InvalidSequence(_))
        ));
    }

    #[test]
    fn video_frame_header_is_exact() {
        let mut sample = VideoSample {
            data: vec![1, 2].into(),
            layout: Layout::Whole,
            kind: FrameKind::Key,
            continuity: Continuity::AfterGap,
            metadata: FrameMetadata::new(
                value(pipe::Generation::new(4)),
                value(pipe::FrameDimension::new(1280)),
                value(pipe::FrameDimension::new(720)),
                99_000,
                17,
                Some(value(InputSequence::new(8))),
                value(Fps::new(60)),
                pipe::Chroma::Yuv420,
            ),
        };
        let bytes = vec![
            13, 1, 0, 0, 37, 0, 0, 0, 1, 1, 4, 0, 0, 0, 0, 5, 208, 2, 184, 130, 1, 0, 0, 0, 0, 0,
            17, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 60, 0, 0, 0, 1, 1, 2,
        ];
        assert_eq!(sample.encode(), bytes);
        assert_eq!(VideoSample::decode(&bytes), Ok(sample.clone()));
        sample.layout = Layout::Stripes;
        let mut striped = bytes;
        striped[1] = 2;
        assert_eq!(sample.encode(), striped);
        assert_eq!(VideoSample::decode(&striped), Ok(sample));
    }

    #[test]
    fn stripe_header_is_exact() {
        let mut out = Vec::new();
        push_stripe(&mut out, 90, 180, FrameKind::Key, &[0, 0, 0, 1, 0x65]);
        assert_eq!(out, [90, 0, 180, 0, 1, 5, 0, 0, 0, 0, 0, 0, 1, 0x65]);
        assert_eq!(out.len() - 5, STRIPE_HEADER_BYTES);
    }
}
