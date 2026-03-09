use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use crossfont::{BitmapBuffer, Metrics, RasterizedGlyph};
use image::imageops::{self, FilterType};
use image::{Rgba, RgbaImage};
use url::Url;

use crate::display::SizeInfo;
use crate::tab_panel_icons::tab_panel_icon_slot_layout;

const MAX_FAVICON_BYTES: usize = 512 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const FAVICON_SIZE_SCALE: f32 = 0.8;

#[derive(Clone, Debug)]
pub struct FaviconImage {
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

impl FaviconImage {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let image = image::load_from_memory(bytes).ok()?;
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        Some(Self { width, height, rgba: Arc::from(rgba.into_raw()) })
    }

    pub fn rasterized_glyph(
        &self,
        character: char,
        size_info: &SizeInfo,
        metrics: Metrics,
        text_offset_y: f32,
    ) -> RasterizedGlyph {
        let layout = tab_panel_icon_slot_layout(size_info, text_offset_y);
        let icon_size = layout.scaled_square_px(FAVICON_SIZE_SCALE).max(1) as u32;

        let mut image = self.to_image();
        if image.width() != icon_size || image.height() != icon_size {
            image = resize_to_square(&image, icon_size);
        }

        let mut buffer = image.into_raw();
        premultiply_rgba(&mut buffer);
        let (left, top) = layout.glyph_position(icon_size as i32, icon_size as i32, metrics);

        RasterizedGlyph {
            character,
            width: icon_size as i32,
            height: icon_size as i32,
            top,
            left,
            advance: (layout.advance_px(), 0),
            buffer: BitmapBuffer::Rgba(buffer),
        }
    }

    fn to_image(&self) -> RgbaImage {
        RgbaImage::from_raw(self.width, self.height, self.rgba.to_vec())
            .unwrap_or_else(|| RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0])))
    }
}

pub fn resolve_favicon_url(page_url: &str, icon_hint: &str) -> Option<String> {
    let hint = icon_hint.trim();
    let hint = hint.trim_matches('"');

    let icon_url = if !hint.is_empty() && hint != "null" && hint != "undefined" {
        if let Ok(url) = Url::parse(hint) {
            Some(url)
        } else if let Ok(base) = Url::parse(page_url) {
            base.join(hint).ok()
        } else {
            None
        }
    } else {
        None
    };

    if let Some(url) = icon_url {
        if url.scheme() != "data" {
            return Some(url.to_string());
        }
    }

    let base = Url::parse(page_url).ok()?;
    base.join("/favicon.ico").ok().map(|url| url.to_string())
}

pub fn fetch_favicon(url: &str) -> Option<FaviconImage> {
    if url.starts_with("data:") {
        return None;
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .timeout_write(WRITE_TIMEOUT)
        .build();

    let response = agent.get(url).call().ok()?;
    if response.status() >= 400 {
        return None;
    }

    let mut bytes = Vec::new();
    let mut reader = response.into_reader().take((MAX_FAVICON_BYTES + 1) as u64);
    reader.read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_FAVICON_BYTES {
        return None;
    }

    FaviconImage::from_bytes(&bytes)
}

fn resize_to_square(image: &RgbaImage, size: u32) -> RgbaImage {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 || size == 0 {
        return RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
    }

    let scale = (size as f32 / width as f32).min(size as f32 / height as f32);
    let new_width = (width as f32 * scale).round().max(1.0) as u32;
    let new_height = (height as f32 * scale).round().max(1.0) as u32;
    let resized = imageops::resize(image, new_width, new_height, FilterType::Triangle);

    if new_width == size && new_height == size {
        return resized;
    }

    let mut square = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 0]));
    let x = (size - new_width) / 2;
    let y = (size - new_height) / 2;
    imageops::overlay(&mut square, &resized, x.into(), y.into());
    square
}

fn premultiply_rgba(buffer: &mut [u8]) {
    for chunk in buffer.chunks_exact_mut(4) {
        let alpha = chunk[3] as u16;
        if alpha == 255 {
            continue;
        }
        let r = (u16::from(chunk[0]) * alpha + 127) / 255;
        let g = (u16::from(chunk[1]) * alpha + 127) / 255;
        let b = (u16::from(chunk[2]) * alpha + 127) / 255;
        chunk[0] = r as u8;
        chunk[1] = g as u8;
        chunk[2] = b as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_METRICS: Metrics = Metrics {
        average_advance: 10.0,
        line_height: 20.0,
        descent: 4.0,
        underline_position: 2.0,
        underline_thickness: 2.0,
        strikeout_position: 2.0,
        strikeout_thickness: 2.0,
    };

    #[test]
    fn rasterized_favicons_leave_padding_within_the_icon_slot() {
        let image = FaviconImage {
            width: 2,
            height: 2,
            rgba: Arc::from(vec![
                255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            ]),
        };
        let size_info = SizeInfo::new(200.0, 100.0, 10.0, 24.0, 0.0, 0.0, 0.0, false);
        let text_offset_y = -2.0;
        let layout = tab_panel_icon_slot_layout(&size_info, text_offset_y);
        let expected = layout.scaled_square_px(FAVICON_SIZE_SCALE);

        let glyph = image.rasterized_glyph('a', &size_info, TEST_METRICS, text_offset_y);

        assert_eq!(glyph.width, expected);
        assert_eq!(glyph.height, expected);
    }
}
