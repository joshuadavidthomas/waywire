//! H.264 encoders for the compositor's rendered frames: `video` drives an
//! `FFmpeg` child process that sends RTP, and `striped` encodes horizontal
//! stripes in process with `OpenH264`.

pub mod latency;
pub mod striped;
pub mod video;
