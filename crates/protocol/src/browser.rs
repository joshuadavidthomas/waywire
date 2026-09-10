//! Browser-to-gateway and gateway-to-browser protocol vocabulary.
//! It covers JSON control messages, 16-byte binary control records that reuse the pipe command
//! header, and binary video frames.

use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::PROTOCOL_VERSION;
use crate::pipe;
use crate::pipe::ClipboardText;
use crate::pipe::Command;
use crate::pipe::CommandHeader;
use crate::pipe::Fps;
use crate::pipe::FrameMetadata;
use crate::pipe::InputSequence;
use crate::pipe::InputText;
use crate::pipe::Kbps;
use crate::pipe::ScalePercent;
pub use crate::pipe::TextAction;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientEvent {
    #[serde(rename_all = "camelCase")]
    VideoConfig {
        version: u8,
        codec: String,
        frame_rate: Fps,
    },
    Cursor(CursorState),
    Clipboard {
        text: ClipboardText,
    },
    ResizeApplied(ResizeApplied),
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResizeApplied {
    pub request: pipe::RequestId,
    #[serde(flatten)]
    pub size: pipe::FrameSize,
    pub scale: pipe::ScaleV120,
    pub generation: pipe::Generation,
}

impl ClientEvent {
    #[must_use]
    pub fn video_config(codec: String, frame_rate: Fps) -> Self {
        Self::VideoConfig {
            version: PROTOCOL_VERSION,
            codec,
            frame_rate,
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
#[serde(rename_all = "camelCase")]
pub struct CursorState {
    pub visible: bool,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub image: String,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            visible: true,
            width: 0,
            height: 0,
            hotspot_x: 0,
            hotspot_y: 0,
            image: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct QualityLevels {
    pub bitrate: Kbps,
    pub fps: Fps,
    pub scale: ScalePercent,
}

#[derive(Debug, PartialEq)]
pub enum ClientMessage {
    Ping {
        id: u64,
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
}

impl ClientMessage {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, BrowserError> {
        let input: JsonInput = serde_json::from_slice(bytes).map_err(BrowserError::InvalidJson)?;
        match input {
            JsonInput::Ping { id } => Ok(Self::Ping { id }),
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
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum JsonInput {
    Ping {
        id: u64,
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
    #[error("invalid browser control header")]
    InvalidControlHeader(#[source] pipe::ProtocolError),
    #[error("browser may not send control kind {0}")]
    PrivateControlKind(u8),
    #[error("invalid browser control fields for kind {kind}")]
    InvalidControlFields {
        kind: u8,
        #[source]
        source: pipe::ProtocolError,
    },
    #[error("invalid JSON control")]
    InvalidJson(#[source] serde_json::Error),
    #[error("invalid feedback: {0}")]
    InvalidFeedback(&'static str),
    #[error("invalid text")]
    InvalidText(#[source] pipe::InvalidValue),
    #[error("invalid input sequence")]
    InvalidSequence(#[source] pipe::InvalidValue),
    #[error("invalid clipboard")]
    InvalidClipboard(#[source] pipe::InvalidValue),
}

pub fn parse_browser_record(bytes: &[u8]) -> Result<Command, BrowserError> {
    let header = CommandHeader::parse(bytes).map_err(BrowserError::InvalidControlHeader)?;
    let kind = header.wire_kind();
    if !header.is_browser_input() {
        return Err(BrowserError::PrivateControlKind(kind));
    }
    Command::decode(header, &[])
        .map_err(|source| BrowserError::InvalidControlFields { kind, source })
}

#[derive(Clone, Debug)]
pub struct VideoSample {
    pub data: Arc<[u8]>,
    pub key: bool,
    pub discontinuity: bool,
    pub metadata: FrameMetadata,
}

/// Encodes a 40-byte header: version at 0, record type at 1 (1 = video), flags at 2, zero at 3,
/// sequence at 4, capture time in microseconds at 12, generation at 20, width at 24, height at 26,
/// capture time in nanoseconds at 28, and input sequence at 36.
#[must_use]
pub fn encode_video_frame(sample: &VideoSample) -> Vec<u8> {
    let metadata = &sample.metadata;
    let mut bytes = vec![0; 40 + sample.data.len()];
    bytes[0] = PROTOCOL_VERSION;
    bytes[1] = 1;
    bytes[2] = u8::from(sample.key) | (u8::from(sample.discontinuity) << 1);
    bytes[4..12].copy_from_slice(&metadata.sequence.to_le_bytes());
    bytes[12..20].copy_from_slice(&(metadata.capture_nanos / 1_000).to_le_bytes());
    bytes[20..24].copy_from_slice(&metadata.generation.get().to_le_bytes());
    bytes[24..26].copy_from_slice(&metadata.width.get().to_le_bytes());
    bytes[26..28].copy_from_slice(&metadata.height.get().to_le_bytes());
    bytes[28..36].copy_from_slice(&metadata.capture_nanos.to_le_bytes());
    bytes[36..40].copy_from_slice(
        &metadata
            .input_sequence
            .map_or(0, InputSequence::get)
            .to_le_bytes(),
    );
    bytes[40..].copy_from_slice(&sample.data);
    bytes
}

#[cfg(test)]
mod tests;
