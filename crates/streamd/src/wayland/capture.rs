use std::fs::File;
use std::os::fd::AsFd;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use memmap2::MmapMut;
use memmap2::MmapOptions;
use nix::sys::memfd::MFdFlags;
use nix::sys::memfd::memfd_create;
use nix::unistd::ftruncate;
use wayland_client::Proxy;
use wayland_client::QueueHandle;
use wayland_client::protocol::wl_buffer;
use wayland_client::protocol::wl_output;
use wayland_client::protocol::wl_shm;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1;

use super::State;
use crate::protocol::FrameMetadata;
use crate::protocol::MAX_RAW_PIXELS;
use crate::video::CapturedFrame;
use crate::video::EncoderConfig;
use crate::video::encoded_dimensions;

pub struct Capture {
    pub manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    pub frame: Option<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1>,
    pub buffer: Option<wl_buffer::WlBuffer>,
    mapping: Option<MmapMut>,
    pub width: u32,
    pub height: u32,
    stride: u32,
    format: Option<wl_shm::Format>,
    constraints: bool,
    pub can_wait_for_damage: bool,
    pub cursor_overlay: bool,
    pub sequence: u64,
}

impl Capture {
    pub fn new() -> Self {
        Self {
            manager: None,
            frame: None,
            buffer: None,
            mapping: None,
            width: 0,
            height: 0,
            stride: 0,
            format: None,
            constraints: false,
            can_wait_for_damage: false,
            cursor_overlay: false,
            sequence: 0,
        }
    }

    pub fn request(&mut self, output: &wl_output::WlOutput, qh: &QueueHandle<State>) -> Result<()> {
        if self.frame.is_some() {
            bail!("capture frame is already pending");
        }
        let manager = self
            .manager
            .as_ref()
            .context("missing zwlr_screencopy_manager_v1")?;
        self.frame = Some(manager.capture_output(i32::from(self.cursor_overlay), output, qh, ()));
        self.constraints = false;
        Ok(())
    }

    pub fn cancel(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.constraints = false;
    }

    pub fn set_constraints(
        &mut self,
        shm: &wl_shm::WlShm,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let pixels = u64::from(width) * u64::from(height);
        let expected_stride = width.checked_mul(4).context("capture stride overflow")?;
        if width == 0
            || height == 0
            || pixels > MAX_RAW_PIXELS
            || stride != expected_stride
            || !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888)
        {
            bail!("unsupported or oversized capture buffer {width}x{height} stride {stride}");
        }
        if self.buffer.is_some()
            && self.width == width
            && self.height == height
            && self.stride == stride
            && self.format == Some(format)
        {
            self.constraints = true;
            return Ok(());
        }

        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.mapping = None;
        let length = usize::try_from(u64::from(stride) * u64::from(height))
            .context("capture allocation exceeds address space")?;
        let fd = memfd_create(c"sprite-desktop-capture", MFdFlags::MFD_CLOEXEC)?;
        ftruncate(&fd, i64::try_from(length)?)?;
        let file = File::from(fd);
        // SAFETY: the map owns a duplicate of the valid memfd and uses its current length.
        let mapping = unsafe { MmapOptions::new().len(length).map_mut(&file)? };
        let pool = shm.create_pool(file.as_fd(), i32::try_from(length)?, qh, ());
        let buffer = pool.create_buffer(
            0,
            i32::try_from(width)?,
            i32::try_from(height)?,
            i32::try_from(stride)?,
            format,
            qh,
            (),
        );
        pool.destroy();
        self.buffer = Some(buffer);
        self.mapping = Some(mapping);
        self.width = width;
        self.height = height;
        self.stride = stride;
        self.format = Some(format);
        self.constraints = true;
        Ok(())
    }

    pub fn begin_copy(&self, wait_for_damage: bool) -> Result<()> {
        if !self.constraints {
            bail!("screencopy frame omitted SHM constraints");
        }
        let frame = self.frame.as_ref().context("missing capture frame")?;
        let buffer = self.buffer.as_ref().context("missing capture buffer")?;
        if wait_for_damage && frame.version() >= 2 {
            frame.copy_with_damage(buffer);
        } else {
            frame.copy(buffer);
        }
        Ok(())
    }

    pub fn completed_frame(
        &mut self,
        capture_nanos: u64,
        generation: u32,
        input_sequence: u32,
        fps: u32,
        bitrate_kbps: u32,
        scale_percent: u32,
    ) -> Result<CapturedFrame<'_>> {
        let mapping = self.mapping.as_ref().context("missing capture mapping")?;
        let expected = usize::try_from(u64::from(self.width) * u64::from(self.height) * 4)?;
        if mapping.len() != expected {
            bail!("capture mapping length changed");
        }
        let (encoded_width, encoded_height) =
            encoded_dimensions(self.width, self.height, scale_percent, fps);
        let metadata = FrameMetadata {
            generation,
            width: encoded_width,
            height: encoded_height,
            capture_nanos,
            sequence: self.sequence,
            input_sequence,
            fps,
        };
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("frame sequence exhausted")?;
        self.can_wait_for_damage = true;
        Ok(CapturedFrame::new(
            mapping,
            metadata,
            EncoderConfig {
                raw_width: self.width,
                raw_height: self.height,
                encoded_width,
                encoded_height,
                fps,
                bitrate_kbps,
            },
        ))
    }
}
