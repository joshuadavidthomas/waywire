use anyhow::{Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use std::io::Cursor;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorState {
    #[serde(rename = "type")]
    kind: &'static str,
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
            kind: "cursor",
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
    pub async fn with_image(
        &self,
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        bgra: Vec<u8>,
    ) -> Result<Self> {
        let image = tokio::task::spawn_blocking(move || encode(width, height, &bgra))
            .await
            .map_err(|_| anyhow!("cursor encoder panicked"))??;
        let mut next = self.clone();
        next.width = width;
        next.height = height;
        next.hotspot_x = hotspot_x;
        next.hotspot_y = hotspot_y;
        next.image = image;
        Ok(next)
    }
    pub fn with_visibility(&self, visible: bool) -> Self {
        let mut next = self.clone();
        next.visible = visible;
        next
    }
}
fn encode(width: u32, height: u32, bgra: &[u8]) -> Result<String> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for p in bgra.chunks_exact(4) {
        let a = p[3];
        let un = |v: u8| {
            if a == 0 {
                0
            } else {
                ((u16::from(v) * 255) / u16::from(a)).min(255) as u8
            }
        };
        rgba.extend_from_slice(&[un(p[2]), un(p[1]), un(p[0]), a]);
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut bytes), width, height);
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
    async fn converts_premultiplied_bgra() {
        let state = CursorState::default()
            .with_image(1, 1, -2, 3, vec![16, 32, 64, 128])
            .await
            .unwrap();
        assert!(state.image.starts_with("data:image/png;base64,"));
        assert_eq!(state.hotspot_x, -2);
    }
}
