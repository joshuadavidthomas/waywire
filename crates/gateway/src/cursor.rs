use std::io::Cursor;

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sprite_desktop_protocol::browser::CursorBitmap;
use sprite_desktop_protocol::browser::CursorState;
use sprite_desktop_protocol::pipe::CursorImage;
use sprite_desktop_protocol::pipe::CursorSize;
use sprite_desktop_protocol::pipe::CursorVisibility;

pub(crate) enum CursorUpdate {
    Visibility(CursorVisibility),
    Bitmap(CursorBitmap),
}

impl CursorUpdate {
    pub(crate) async fn image(cursor: CursorImage) -> Result<Self> {
        let size = cursor.size();
        let hotspot = cursor.hotspot;
        let pixels = cursor.into_pixels();
        let image = tokio::task::spawn_blocking(move || encode(size, &pixels))
            .await
            .context("cursor encoder task failed")??;
        Ok(Self::Bitmap(CursorBitmap {
            size,
            hotspot,
            image,
        }))
    }

    pub(crate) fn apply(self, current: &mut CursorState) {
        match self {
            Self::Visibility(visibility) => current.visibility = visibility,
            Self::Bitmap(bitmap) => current.bitmap = Some(bitmap),
        }
    }
}

fn encode(size: CursorSize, pixels: &[u8]) -> Result<String> {
    let mut rgba = Vec::with_capacity(pixels.len());
    for pixel in pixels.chunks_exact(4) {
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
    use sprite_desktop_protocol::pipe::Hotspot;

    use super::*;

    #[tokio::test]
    async fn builds_cursor_state_with_unpremultiplied_png_pixels() {
        let size = CursorSize::new(1, 1).expect("test cursor size should be valid");
        let mut state = CursorState {
            visibility: CursorVisibility::Hidden,
            bitmap: None,
        };
        let cursor = CursorImage::new(size, Hotspot { x: -3, y: 4 }, vec![16, 32, 64, 128])
            .expect("test cursor image should be valid");
        CursorUpdate::image(cursor)
            .await
            .expect("test cursor image should encode")
            .apply(&mut state);

        assert_eq!(state.visibility, CursorVisibility::Hidden);
        let bitmap = state.bitmap.expect("cursor image should set the bitmap");
        assert_eq!(bitmap.size, size);
        assert_eq!(bitmap.hotspot, Hotspot { x: -3, y: 4 });

        let encoded = bitmap
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
