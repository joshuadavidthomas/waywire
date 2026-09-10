use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde::ser::SerializeMap;
use thiserror::Error;

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

pub const CONTROL_BYTES: usize = pipe::COMMAND_HEADER_BYTES;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientEvent {
    VideoConfig {
        version: u8,
        codec: String,
        #[serde(rename = "frameRate")]
        frame_rate: Fps,
    },
    Cursor(CursorState),
    Clipboard {
        text: ClipboardText,
    },
    ResizeApplied {
        request: pipe::RequestId,
        width: u32,
        height: u32,
        scale: pipe::ScaleV120,
        generation: pipe::Generation,
    },
    Quality(QualityLevels),
    ControlState {
        state: ControlState,
    },
    Pong {
        id: u64,
        #[serde(rename = "serverNanos")]
        server_nanos: String,
    },
}

impl ClientEvent {
    #[must_use]
    pub fn video_config(codec: String, frame_rate: Fps) -> Self {
        Self::VideoConfig {
            version: pipe::VERSION,
            codec,
            frame_rate,
        }
    }
}

impl Serialize for ClientEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::VideoConfig {
                version,
                codec,
                frame_rate,
            } => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("codec", codec)?;
                map.serialize_entry("frameRate", frame_rate)?;
                map.serialize_entry("type", "video-config")?;
                map.serialize_entry("version", version)?;
                map.end()
            }
            Self::Cursor(cursor) => {
                let mut map = serializer.serialize_map(Some(7))?;
                map.serialize_entry("type", "cursor")?;
                map.serialize_entry("visible", &cursor.visible)?;
                map.serialize_entry("width", &cursor.width)?;
                map.serialize_entry("height", &cursor.height)?;
                map.serialize_entry("hotspotX", &cursor.hotspot_x)?;
                map.serialize_entry("hotspotY", &cursor.hotspot_y)?;
                map.serialize_entry("image", &cursor.image)?;
                map.end()
            }
            Self::Clipboard { text } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("text", text.get())?;
                map.serialize_entry("type", "clipboard")?;
                map.end()
            }
            Self::ResizeApplied {
                request,
                width,
                height,
                scale,
                generation,
            } => {
                let mut map = serializer.serialize_map(Some(6))?;
                map.serialize_entry("generation", generation)?;
                map.serialize_entry("height", height)?;
                map.serialize_entry("request", request)?;
                map.serialize_entry("scale", scale)?;
                map.serialize_entry("type", "resize-applied")?;
                map.serialize_entry("width", width)?;
                map.end()
            }
            Self::Quality(levels) => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("bitrate", &levels.bitrate)?;
                map.serialize_entry("fps", &levels.fps)?;
                map.serialize_entry("scale", &levels.scale)?;
                map.serialize_entry("type", "quality")?;
                map.end()
            }
            Self::ControlState { state } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("state", state)?;
                map.serialize_entry("type", "control-state")?;
                map.end()
            }
            Self::Pong { id, server_nanos } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("serverNanos", server_nanos)?;
                map.serialize_entry("type", "pong")?;
                map.end()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlState {
    Active,
    Busy,
    Ready,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
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

impl CursorState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_visibility(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    #[must_use]
    pub fn with_image(
        mut self,
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        image: String,
    ) -> Self {
        self.width = width;
        self.height = height;
        self.hotspot_x = hotspot_x;
        self.hotspot_y = hotspot_y;
        self.image = image;
        self
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
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
                text: InputText::new(text)
                    .map_err(|error| BrowserError::InvalidText(error.to_string()))?,
                sequence: InputSequence::new(sequence)
                    .map_err(|error| BrowserError::InvalidText(error.to_string()))?,
            }),
            JsonInput::ClipboardWrite { text } => Ok(Self::ClipboardWrite {
                text: ClipboardText::new(text)
                    .map_err(|error| BrowserError::InvalidClipboard(error.to_string()))?,
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum JsonInput {
    #[serde(rename = "ping")]
    Ping { id: u64 },
    #[serde(rename = "feedback")]
    Feedback(FeedbackValues),
    #[serde(rename = "text")]
    Text {
        action: TextAction,
        text: String,
        sequence: u32,
    },
    #[serde(rename = "clipboard-write")]
    ClipboardWrite { text: String },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FeedbackValues {
    pub received: u32,
    pub presented: u32,
    #[serde(rename = "queuePeak")]
    pub queue_peak: u32,
    #[serde(rename = "queueBusyMs")]
    pub queue_busy_ms: f64,
    #[serde(rename = "sampleMs")]
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
    InvalidControlHeader,
    #[error("invalid browser control fields for kind {0}")]
    InvalidControlFields(u8),
    #[error("invalid JSON control: {0}")]
    InvalidJson(#[source] serde_json::Error),
    #[error("invalid feedback: {0}")]
    InvalidFeedback(&'static str),
    #[error("invalid text: {0}")]
    InvalidText(String),
    #[error("invalid clipboard: {0}")]
    InvalidClipboard(String),
}

pub fn parse_browser_record(bytes: &[u8]) -> Result<Command, BrowserError> {
    let header =
        CommandHeader::parse(bytes).map_err(|_decode_error| BrowserError::InvalidControlHeader)?;
    let kind = header.wire_kind();
    if !header.is_browser_input() {
        return Err(BrowserError::InvalidControlFields(kind));
    }
    Command::decode(header, &[]).map_err(|_decode_error| BrowserError::InvalidControlFields(kind))
}

#[derive(Clone, Debug)]
pub struct VideoSample {
    pub data: Arc<[u8]>,
    pub key: bool,
    pub discontinuity: bool,
    pub metadata: FrameMetadata,
}

#[must_use]
pub fn encode_video_frame(sample: &VideoSample) -> Vec<u8> {
    let metadata = &sample.metadata;
    let mut bytes = vec![0; 40 + sample.data.len()];
    bytes[0] = pipe::VERSION;
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
