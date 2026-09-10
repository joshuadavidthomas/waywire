use std::io::Cursor;

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sprite_desktop_protocol::browser::CursorState;
use sprite_desktop_protocol::pipe::CursorSize;

pub(crate) enum CursorUpdate {
    Visibility {
        visible: bool,
    },
    Image {
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        image: String,
    },
}

impl CursorUpdate {
    pub(crate) fn visibility(visible: bool) -> Self {
        Self::Visibility { visible }
    }

    pub(crate) async fn image(
        size: CursorSize,
        hotspot_x: i32,
        hotspot_y: i32,
        bgra: Vec<u8>,
    ) -> Result<Self> {
        let image = tokio::task::spawn_blocking(move || encode(size, &bgra))
            .await
            .context("cursor encoder task failed")??;
        Ok(Self::Image {
            width: size.width(),
            height: size.height(),
            hotspot_x,
            hotspot_y,
            image,
        })
    }

    pub(crate) fn apply(self, current: &mut CursorState) {
        match self {
            Self::Visibility { visible } => current.visible = visible,
            Self::Image {
                width,
                height,
                hotspot_x,
                hotspot_y,
                image,
            } => {
                current.width = width;
                current.height = height;
                current.hotspot_x = hotspot_x;
                current.hotspot_y = hotspot_y;
                current.image = image;
            }
        }
    }
}

fn encode(size: CursorSize, bgra: &[u8]) -> Result<String> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        let alpha = pixel[3];
        let unpremultiply = |value: u8| {
            if alpha == 0 {
                0
            } else {
                ((u16::from(value) * 255) / u16::from(alpha)).min(255) as u8
            }
        };
        rgba.extend_from_slice(&[
            unpremultiply(pixel[2]),
            unpremultiply(pixel[1]),
            unpremultiply(pixel[0]),
            alpha,
        ]);
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut bytes), size.width(), size.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgba)?;
    }
    Ok(format!("data:image/png;base64,{}", STANDARD.encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builds_cursor_state_with_unpremultiplied_png_pixels() {
        let size = CursorSize::new(1, 1).expect("test cursor size should be valid");
        let mut state = CursorState::new().with_visibility(false);
        CursorUpdate::image(size, -3, 4, vec![16, 32, 64, 128])
            .await
            .expect("test cursor image should encode")
            .apply(&mut state);

        assert!(!state.visible);
        assert_eq!(state.width, 1);
        assert_eq!(state.height, 1);
        assert_eq!(state.hotspot_x, -3);
        assert_eq!(state.hotspot_y, 4);

        let encoded = state
            .image
            .strip_prefix("data:image/png;base64,")
            .expect("cursor image should use a PNG data URL");
        let bytes = STANDARD
            .decode(encoded)
            .expect("cursor PNG should use valid base64");
        let decoder = png::Decoder::new(Cursor::new(bytes));
        let mut reader = decoder
            .read_info()
            .expect("cursor PNG header should decode");
        let mut pixels = vec![
            0;
            reader
                .output_buffer_size()
                .expect("cursor PNG output size should fit in memory")
        ];
        let info = reader
            .next_frame(&mut pixels)
            .expect("cursor PNG pixels should decode");

        assert_eq!(info.width, 1);
        assert_eq!(info.height, 1);
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        assert_eq!(&pixels[..info.buffer_size()], &[127, 63, 31, 128]);
    }
}
