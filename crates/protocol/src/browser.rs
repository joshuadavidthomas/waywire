//! Browser-to-gateway and gateway-to-browser protocol vocabulary.
//! It covers JSON control messages, binary control records, and binary video frames.

use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
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
    ResizeApplied(pipe::ResizeApplied),
    ResetVideoRefused {
        reason: pipe::ResetVideoRefusal,
    },
    Quality(QualityLevels),
    ControlState {
        state: ControlState,
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
    pub visibility: CursorVisibility,
    pub shape: CursorShape,
    /// `None` until the compositor has reported a position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<CursorPosition>,
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
            position: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct QualityLevels {
    pub bitrate: Kbps,
    pub fps: Fps,
    pub scale: ScalePercent,
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
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeedbackValues {
    pub received: u32,
    pub presented: u32,
    pub queue_peak: u32,
    pub queue_busy_ms: f64,
    pub sample_ms: f64,
    pub dropped: u32,
    pub rtt: f64,
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
            _ => Err(InvalidValue("frame kind must be zero or one")),
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
            _ => Err(InvalidValue("continuity must be zero or one")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BrowserKind {
    Frame = 1,
}

impl RecordKind for BrowserKind {
    fn wire(self) -> u8 {
        match self {
            Self::Frame => 1,
        }
    }

    fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Frame),
            _ => None,
        }
    }

    fn max_payload(self) -> usize {
        match self {
            // Frame kind u8, continuity u8, 33-byte metadata, then the encoded access unit.
            Self::Frame => 35 + MAX_VIDEO_DATA_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSample {
    pub data: Arc<[u8]>,
    pub kind: FrameKind,
    pub continuity: Continuity,
    pub metadata: FrameMetadata,
}

impl Record for VideoSample {
    type Kind = BrowserKind;

    fn kind(&self) -> Self::Kind {
        BrowserKind::Frame
    }

    fn write_payload(&self, out: &mut Writer) {
        out.put(&self.kind);
        out.put(&self.continuity);
        out.put(&self.metadata);
        out.bytes(&self.data);
    }

    fn read_payload(kind: Self::Kind, input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        match kind {
            BrowserKind::Frame => Ok(Self {
                kind: input.get()?,
                continuity: input.get()?,
                metadata: input.get()?,
                data: Arc::from(input.rest()),
            }),
        }
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
        let mut bytes = vec![9, kind, 0, 0];
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
                format!(r#"{{"type":"video-config","version":9,"codec":"{codec}"}}"#)
            );
        }
    }

    #[test]
    fn cursor_json_is_exact() {
        let cursor = CursorState {
            visibility: CursorVisibility::Visible,
            shape: CursorShape::Pointer,
            position: Some(CursorPosition {
                x: value(pipe::PointerCoordinate::new(10)),
                y: value(pipe::PointerCoordinate::new(20)),
            }),
        };
        assert_eq!(
            json(&ClientEvent::Cursor(cursor)),
            r#"{"type":"cursor","visible":true,"shape":"pointer","position":{"x":10,"y":20}}"#
        );
    }

    #[test]
    fn cursor_without_a_position_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::Cursor(CursorState::default())),
            r#"{"type":"cursor","visible":true,"shape":"default"}"#
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
    }

    #[test]
    fn resize_applied_json_is_exact() {
        let event = ClientEvent::ResizeApplied(pipe::ResizeApplied {
            request_id: value(pipe::RequestId::new(9)),
            size: value(pipe::FrameSize::new(1280, 720)),
            scale_v120: value(pipe::ScaleV120::new(180)),
            generation: value(pipe::Generation::new(4)),
        });
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
                record(1, &[12, 0, 0, 0, 34, 0, 0, 0, 7, 0, 0, 0]),
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
    fn browser_records_reject_private_kinds() {
        let commands = [
            Command::Clipboard(value(ClipboardText::new(String::new()))),
            Command::Quality(pipe::Quality {
                bitrate_kbps: value(Kbps::new(8_000)),
                fps: value(Fps::new(60)),
                scale_percent: value(ScalePercent::new(100)),
                crf: value(pipe::Crf::new(23)),
                chroma: pipe::Chroma::Yuv444,
            }),
            Command::Text(pipe::Text {
                action: TextAction::Commit,
                sequence: value(InputSequence::new(1)),
                text: value(InputText::new(String::new())),
            }),
            Command::KeyframeReadiness(pipe::KeyframeReadiness {
                generation: value(pipe::Generation::new(1)),
                state: pipe::KeyframeState::Cached,
            }),
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
        let valid = br#"{"type":"feedback","received":40,"presented":42,"queuePeak":2,"queueBusyMs":25,"sampleMs":1000,"dropped":0,"rtt":20}"#;
        assert!(matches!(
            ClientMessage::parse_json(valid),
            Ok(ClientMessage::Feedback(_))
        ));

        let invalid = br#"{"type":"feedback","received":1,"presented":1,"queuePeak":2,"queueBusyMs":0,"sampleMs":0,"dropped":0,"rtt":20}"#;
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
                text: value(InputText::new("hello".into())),
                sequence: value(InputSequence::new(7)),
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
        let sample = VideoSample {
            data: vec![1, 2].into(),
            kind: FrameKind::Key,
            continuity: Continuity::AfterGap,
            metadata: FrameMetadata {
                generation: value(pipe::Generation::new(4)),
                width: value(pipe::FrameDimension::new(1280)),
                height: value(pipe::FrameDimension::new(720)),
                capture_nanos: 99_000,
                sequence: 17,
                input_sequence: Some(value(InputSequence::new(8))),
                fps: value(Fps::new(60)),
                chroma: pipe::Chroma::Yuv420,
            },
        };
        let bytes = vec![
            9, 1, 0, 0, 37, 0, 0, 0, 1, 1, 4, 0, 0, 0, 0, 5, 208, 2, 184, 130, 1, 0, 0, 0, 0, 0,
            17, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 60, 0, 0, 0, 1, 1, 2,
        ];
        assert_eq!(sample.encode(), bytes);
        assert_eq!(VideoSample::decode(&bytes), Ok(sample));
    }
}
