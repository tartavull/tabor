use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use image::imageops::{self, FilterType};
use image::{ImageFormat, Rgba, RgbaImage};
use url::Url;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::TouchPhase;

const LOCAL_IMAGE_PROBE_BYTES: usize = 64;
const MAX_REMOTE_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const ZOOM_STEP: f64 = 1.15;
const MIN_MANUAL_ZOOM: f64 = 0.05;
const MAX_MANUAL_ZOOM: f64 = 64.0;
const ROTATION_SNAP_DEGREES: f32 = 90.0;
const ROTATION_END_THRESHOLD_DEGREES: f32 = 45.0;
const REMOTE_IMAGE_EXTENSIONS: &[&str] =
    &["png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageScaleMode {
    Fit,
    Fill,
    Actual,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageLoadState {
    Loading,
    Ready,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct LoadedImage {
    pub source: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub(crate) struct ImageViewState {
    pub source: String,
    pub title: String,
    pub load_state: ImageLoadState,
    pub scale_mode: ImageScaleMode,
    pub manual_zoom: f64,
    pub pan_x: f64,
    pub pan_y: f64,
    pub rotation_quarter_turns: u8,
    bitmap: Option<LoadedImage>,
    drag: Option<DragState>,
    gesture_state: GestureState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ImageRenderQuad {
    pub dest_x_px: f32,
    pub dest_y_px: f32,
    pub dest_width_px: f32,
    pub dest_height_px: f32,
    pub uv_top_left: (f32, f32),
    pub uv_top_right: (f32, f32),
    pub uv_bottom_left: (f32, f32),
    pub uv_bottom_right: (f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DragState {
    cursor: PhysicalPosition<f64>,
    pan_x: f64,
    pan_y: f64,
}

#[derive(Debug, Clone, Default)]
struct GestureState {
    accumulated_rotation_degrees: f32,
    last_pressure_stage: i64,
}

impl ImageViewState {
    pub(crate) fn new(source: String) -> Self {
        Self {
            title: image_title_for_source(&source),
            source,
            load_state: ImageLoadState::Loading,
            scale_mode: ImageScaleMode::Fit,
            manual_zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            rotation_quarter_turns: 0,
            bitmap: None,
            drag: None,
            gesture_state: GestureState::default(),
        }
    }

    pub(crate) fn reset_source(&mut self, source: String) {
        self.source = source;
        self.title = image_title_for_source(&self.source);
        self.load_state = ImageLoadState::Loading;
        self.bitmap = None;
        self.reset_view();
    }

    pub(crate) fn set_loaded(&mut self, image: LoadedImage, viewport: PhysicalSize<u32>) {
        self.source = image.source.clone();
        self.title = image.title.clone();
        self.bitmap = Some(image);
        self.load_state = ImageLoadState::Ready;
        self.drag = None;
        self.clear_gesture_state();
        self.clamp_pan(viewport);
    }

    pub(crate) fn set_error(&mut self, message: String) {
        self.load_state = ImageLoadState::Error(message);
        self.bitmap = None;
        self.drag = None;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.clear_gesture_state();
    }

    pub(crate) fn bitmap(&self) -> Option<&LoadedImage> {
        self.bitmap.as_ref()
    }

    pub(crate) fn reset_view(&mut self) {
        self.scale_mode = ImageScaleMode::Fit;
        self.manual_zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.rotation_quarter_turns = 0;
        self.drag = None;
        self.clear_gesture_state();
    }

    pub(crate) fn render_quad(&self, viewport: PhysicalSize<u32>) -> Option<ImageRenderQuad> {
        let bitmap = self.bitmap()?;
        let display_size = self.display_size(viewport);
        let origin_x = (f64::from(viewport.width) - display_size.0) / 2.0 + self.pan_x;
        let origin_y = (f64::from(viewport.height) - display_size.1) / 2.0 + self.pan_y;
        let uv = rotated_uv_corners(
            self.rotation_quarter_turns,
            bitmap.width as f32,
            bitmap.height as f32,
        );
        Some(ImageRenderQuad {
            dest_x_px: origin_x as f32,
            dest_y_px: origin_y as f32,
            dest_width_px: display_size.0 as f32,
            dest_height_px: display_size.1 as f32,
            uv_top_left: uv.0,
            uv_top_right: uv.1,
            uv_bottom_left: uv.2,
            uv_bottom_right: uv.3,
        })
    }

    pub(crate) fn debug_snapshot(
        &self,
        viewport: PhysicalSize<u32>,
        background: [u8; 4],
    ) -> Option<RgbaImage> {
        let bitmap = self.bitmap()?;
        let quad = self.render_quad(viewport)?;
        let source = RgbaImage::from_raw(bitmap.width, bitmap.height, bitmap.rgba.to_vec())?;
        let rotated = match self.rotation_quarter_turns % 4 {
            0 => source,
            1 => imageops::rotate90(&source),
            2 => imageops::rotate180(&source),
            _ => imageops::rotate270(&source),
        };
        let dest_width = quad.dest_width_px.round().max(1.0) as u32;
        let dest_height = quad.dest_height_px.round().max(1.0) as u32;
        let rendered = if rotated.width() == dest_width && rotated.height() == dest_height {
            rotated
        } else {
            imageops::resize(&rotated, dest_width, dest_height, FilterType::Triangle)
        };

        let mut canvas = RgbaImage::from_pixel(viewport.width, viewport.height, Rgba(background));
        imageops::overlay(
            &mut canvas,
            &rendered,
            quad.dest_x_px.round() as i64,
            quad.dest_y_px.round() as i64,
        );
        Some(canvas)
    }

    pub(crate) fn zoom_factor(&self, viewport: PhysicalSize<u32>) -> Option<f64> {
        let bitmap = self.bitmap()?;
        Some(match self.scale_mode {
            ImageScaleMode::Fit => fit_zoom(
                viewport,
                rotated_dimensions(bitmap.width, bitmap.height, self.rotation_quarter_turns),
            ),
            ImageScaleMode::Fill => fill_zoom(
                viewport,
                rotated_dimensions(bitmap.width, bitmap.height, self.rotation_quarter_turns),
            ),
            ImageScaleMode::Actual => 1.0,
            ImageScaleMode::Manual => self.manual_zoom,
        })
    }

    pub(crate) fn begin_pan(&mut self, cursor: PhysicalPosition<f64>) -> bool {
        if self.bitmap.is_none() {
            return false;
        }

        self.drag = Some(DragState { cursor, pan_x: self.pan_x, pan_y: self.pan_y });
        true
    }

    pub(crate) fn pan_to(&mut self, cursor: PhysicalPosition<f64>, viewport: PhysicalSize<u32>) {
        let Some(drag) = self.drag else {
            return;
        };

        self.pan_x = drag.pan_x + cursor.x - drag.cursor.x;
        self.pan_y = drag.pan_y + cursor.y - drag.cursor.y;
        self.clamp_pan(viewport);
    }

    pub(crate) fn pan_by(
        &mut self,
        delta_x: f64,
        delta_y: f64,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        if self.bitmap.is_none() || (!delta_x.is_finite() && !delta_y.is_finite()) {
            return false;
        }

        let delta_x = if delta_x.is_finite() { delta_x } else { 0.0 };
        let delta_y = if delta_y.is_finite() { delta_y } else { 0.0 };
        if delta_x.abs() <= f64::EPSILON && delta_y.abs() <= f64::EPSILON {
            return false;
        }

        self.pan_x += delta_x;
        self.pan_y += delta_y;
        self.clamp_pan(viewport);
        if let Some(drag) = self.drag.as_mut() {
            drag.cursor = cursor;
            drag.pan_x = self.pan_x;
            drag.pan_y = self.pan_y;
        }

        true
    }

    pub(crate) fn end_pan(&mut self) {
        self.drag = None;
    }

    pub(crate) fn is_panning(&self) -> bool {
        self.drag.is_some()
    }

    pub(crate) fn zoom_in(&mut self, viewport: PhysicalSize<u32>) {
        let center = viewport_center(viewport);
        self.zoom_by(ZOOM_STEP, center, viewport);
    }

    pub(crate) fn zoom_out(&mut self, viewport: PhysicalSize<u32>) {
        let center = viewport_center(viewport);
        self.zoom_by(1.0 / ZOOM_STEP, center, viewport);
    }

    pub(crate) fn zoom_fit(&mut self, viewport: PhysicalSize<u32>) {
        self.scale_mode = ImageScaleMode::Fit;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.drag = None;
        self.clamp_pan(viewport);
    }

    pub(crate) fn zoom_fill(&mut self, viewport: PhysicalSize<u32>) {
        self.scale_mode = ImageScaleMode::Fill;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.drag = None;
        self.clamp_pan(viewport);
    }

    pub(crate) fn zoom_actual(&mut self, viewport: PhysicalSize<u32>) {
        self.scale_mode = ImageScaleMode::Actual;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.drag = None;
        self.clamp_pan(viewport);
    }

    pub(crate) fn rotate_clockwise(&mut self, viewport: PhysicalSize<u32>) {
        self.rotate_quarter_turns(1, viewport);
    }

    pub(crate) fn rotate_quarter_turns(&mut self, turns: i32, viewport: PhysicalSize<u32>) {
        let turns = turns.rem_euclid(4);
        if turns == 0 {
            return;
        }

        self.rotation_quarter_turns =
            (i32::from(self.rotation_quarter_turns) + turns).rem_euclid(4) as u8;
        self.drag = None;
        self.clamp_pan(viewport);
    }

    pub(crate) fn smart_magnify(&mut self, viewport: PhysicalSize<u32>) -> bool {
        if self.bitmap.is_none() {
            return false;
        }

        match self.scale_mode {
            ImageScaleMode::Actual => self.zoom_fit(viewport),
            ImageScaleMode::Fit | ImageScaleMode::Fill | ImageScaleMode::Manual => {
                self.zoom_actual(viewport);
            },
        }

        true
    }

    pub(crate) fn pinch_gesture(
        &mut self,
        delta: f64,
        phase: TouchPhase,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        if phase == TouchPhase::Cancelled || !delta.is_finite() {
            return false;
        }

        let Some(old_zoom) = self.zoom_factor(viewport) else {
            return false;
        };

        let new_zoom = old_zoom + delta;
        if new_zoom <= 0.0 || (new_zoom - old_zoom).abs() <= f64::EPSILON {
            return false;
        }

        self.zoom_by(new_zoom / old_zoom, cursor, viewport);
        true
    }

    pub(crate) fn rotation_gesture(
        &mut self,
        delta: f32,
        phase: TouchPhase,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        match phase {
            TouchPhase::Started => {
                self.gesture_state.accumulated_rotation_degrees = 0.0;
            },
            TouchPhase::Moved | TouchPhase::Ended | TouchPhase::Cancelled => (),
        }

        if !delta.is_finite() {
            if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.gesture_state.accumulated_rotation_degrees = 0.0;
            }
            return false;
        }

        self.gesture_state.accumulated_rotation_degrees += delta;
        let mut quarter_turns = 0;

        while self.gesture_state.accumulated_rotation_degrees <= -ROTATION_SNAP_DEGREES {
            quarter_turns += 1;
            self.gesture_state.accumulated_rotation_degrees += ROTATION_SNAP_DEGREES;
        }
        while self.gesture_state.accumulated_rotation_degrees >= ROTATION_SNAP_DEGREES {
            quarter_turns -= 1;
            self.gesture_state.accumulated_rotation_degrees -= ROTATION_SNAP_DEGREES;
        }

        if phase == TouchPhase::Ended {
            if self.gesture_state.accumulated_rotation_degrees <= -ROTATION_END_THRESHOLD_DEGREES {
                quarter_turns += 1;
            } else if self.gesture_state.accumulated_rotation_degrees
                >= ROTATION_END_THRESHOLD_DEGREES
            {
                quarter_turns -= 1;
            }
            self.gesture_state.accumulated_rotation_degrees = 0.0;
        } else if phase == TouchPhase::Cancelled {
            self.gesture_state.accumulated_rotation_degrees = 0.0;
            return false;
        }

        if quarter_turns == 0 {
            return false;
        }

        self.rotate_quarter_turns(quarter_turns, viewport);
        true
    }

    pub(crate) fn touchpad_pressure(&mut self, stage: i64, cursor: PhysicalPosition<f64>) -> bool {
        if stage <= 0 {
            let was_active = self.gesture_state.last_pressure_stage > 0 && self.drag.is_some();
            self.gesture_state.last_pressure_stage = 0;
            self.end_pan();
            return was_active;
        }

        if self.bitmap.is_none() {
            return false;
        }

        let previous = self.gesture_state.last_pressure_stage;
        self.gesture_state.last_pressure_stage = stage;
        if previous > 0 {
            return false;
        }

        self.begin_pan(cursor)
    }

    pub(crate) fn zoom_by(
        &mut self,
        factor: f64,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) {
        let Some(old_zoom) = self.zoom_factor(viewport) else {
            return;
        };

        let new_zoom = (old_zoom * factor).clamp(MIN_MANUAL_ZOOM, MAX_MANUAL_ZOOM);
        let viewport_center = viewport_center(viewport);
        let dx = cursor.x - viewport_center.x;
        let dy = cursor.y - viewport_center.y;
        let scale = if old_zoom > 0.0 { new_zoom / old_zoom } else { 1.0 };

        self.scale_mode = ImageScaleMode::Manual;
        self.manual_zoom = new_zoom;
        self.pan_x = dx - (dx - self.pan_x) * scale;
        self.pan_y = dy - (dy - self.pan_y) * scale;
        self.drag = None;
        self.clamp_pan(viewport);
    }

    pub(crate) fn clamp_pan(&mut self, _viewport: PhysicalSize<u32>) {
        let Some(_) = self.bitmap() else {
            self.pan_x = 0.0;
            self.pan_y = 0.0;
            return;
        };
    }

    fn display_size(&self, viewport: PhysicalSize<u32>) -> (f64, f64) {
        let Some(bitmap) = self.bitmap() else {
            return (0.0, 0.0);
        };
        let (image_width, image_height) =
            rotated_dimensions(bitmap.width, bitmap.height, self.rotation_quarter_turns);
        let zoom = self.zoom_factor(viewport).unwrap_or(1.0);
        (f64::from(image_width) * zoom, f64::from(image_height) * zoom)
    }

    fn clear_gesture_state(&mut self) {
        self.gesture_state = GestureState::default();
    }
}

pub(crate) fn image_title_for_source(source: &str) -> String {
    if let Some(path) = local_image_path(source) {
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            return name.to_string();
        }
    }

    if let Ok(url) = Url::parse(source) {
        if let Some(segment) = url
            .path_segments()
            .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        {
            return segment.to_string();
        }
        if let Some(host) = url.host_str() {
            return host.to_string();
        }
    }

    source.to_string()
}

pub(crate) fn load_image_source(source: &str) -> Result<LoadedImage, String> {
    if let Some(path) = local_image_path(source) {
        return load_local_image(&path, source);
    }

    let response = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .timeout_write(WRITE_TIMEOUT)
        .build()
        .get(source)
        .call()
        .map_err(|err| format!("image request failed: {err}"))?;

    if response.status() >= 400 {
        return Err(format!("image request failed with HTTP {}", response.status()));
    }

    let final_source = response.get_url().to_string();
    let mut bytes = Vec::new();
    let mut reader = response.into_reader().take((MAX_REMOTE_IMAGE_BYTES + 1) as u64);
    reader.read_to_end(&mut bytes).map_err(|err| format!("image download failed: {err}"))?;
    if bytes.len() > MAX_REMOTE_IMAGE_BYTES {
        return Err(format!("image download exceeded {} bytes", MAX_REMOTE_IMAGE_BYTES));
    }

    decode_image_bytes(&bytes, source, &final_source)
}

pub(crate) fn local_image_path(url: &str) -> Option<PathBuf> {
    crate::macos::open_url::local_file_path(url)
}

fn is_local_image_url(url: &str) -> bool {
    let Some(path) = local_image_path(url) else {
        return false;
    };

    match probe_local_image_format(&path) {
        Ok(Some(format)) => supports_local_image_format(format),
        Ok(None) | Err(_) => false,
    }
}

fn is_remote_image_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }

    let path = parsed.path().trim_end_matches('/');
    let Some(extension) = path.rsplit('.').next() else {
        return false;
    };
    REMOTE_IMAGE_EXTENSIONS.iter().any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

pub(crate) fn is_image_source(url: &str) -> bool {
    is_remote_image_url(url) || is_local_image_url(url)
}

fn probe_local_image_format(path: &Path) -> Result<Option<ImageFormat>, String> {
    let mut file =
        File::open(path).map_err(|err| format!("unable to open {}: {err}", path.display()))?;
    let mut probe = [0_u8; LOCAL_IMAGE_PROBE_BYTES];
    let read =
        file.read(&mut probe).map_err(|err| format!("unable to read {}: {err}", path.display()))?;
    if read == 0 {
        return Ok(None);
    }

    image::guess_format(&probe[..read]).map(Some).or(Ok(None))
}

fn load_local_image(path: &Path, source: &str) -> Result<LoadedImage, String> {
    let bytes =
        std::fs::read(path).map_err(|err| format!("unable to read {}: {err}", path.display()))?;
    decode_image_bytes(&bytes, source, source)
}

fn decode_image_bytes(
    bytes: &[u8],
    source: &str,
    title_source: &str,
) -> Result<LoadedImage, String> {
    let image =
        image::load_from_memory(bytes).map_err(|err| format!("unable to decode image: {err}"))?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut rgba = rgba.into_raw();
    premultiply_rgba(&mut rgba);
    Ok(LoadedImage {
        source: source.to_string(),
        title: image_title_for_source(title_source),
        width,
        height,
        rgba: Arc::from(rgba),
    })
}

fn rotated_dimensions(width: u32, height: u32, quarter_turns: u8) -> (u32, u32) {
    if quarter_turns % 2 == 0 { (width, height) } else { (height, width) }
}

fn fit_zoom(viewport: PhysicalSize<u32>, image_size: (u32, u32)) -> f64 {
    if viewport.width == 0 || viewport.height == 0 || image_size.0 == 0 || image_size.1 == 0 {
        return 1.0;
    }

    let scale_x = f64::from(viewport.width) / f64::from(image_size.0);
    let scale_y = f64::from(viewport.height) / f64::from(image_size.1);
    scale_x.min(scale_y)
}

fn fill_zoom(viewport: PhysicalSize<u32>, image_size: (u32, u32)) -> f64 {
    if viewport.width == 0 || viewport.height == 0 || image_size.0 == 0 || image_size.1 == 0 {
        return 1.0;
    }

    let scale_x = f64::from(viewport.width) / f64::from(image_size.0);
    let scale_y = f64::from(viewport.height) / f64::from(image_size.1);
    scale_x.max(scale_y)
}

fn viewport_center(viewport: PhysicalSize<u32>) -> PhysicalPosition<f64> {
    PhysicalPosition::new(f64::from(viewport.width) / 2.0, f64::from(viewport.height) / 2.0)
}

type UvCorner = (f32, f32);
type RotatedUvCorners = (UvCorner, UvCorner, UvCorner, UvCorner);

fn rotated_uv_corners(quarter_turns: u8, width: f32, height: f32) -> RotatedUvCorners {
    match quarter_turns % 4 {
        0 => ((0.0, 0.0), (width, 0.0), (0.0, height), (width, height)),
        1 => ((0.0, height), (0.0, 0.0), (width, height), (width, 0.0)),
        2 => ((width, height), (0.0, height), (width, 0.0), (0.0, 0.0)),
        _ => ((width, 0.0), (width, height), (0.0, 0.0), (0.0, height)),
    }
}

fn supports_local_image_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png
            | ImageFormat::Jpeg
            | ImageFormat::Gif
            | ImageFormat::Bmp
            | ImageFormat::Tiff
            | ImageFormat::WebP
            | ImageFormat::Ico
    )
}

fn premultiply_rgba(buffer: &mut [u8]) {
    for chunk in buffer.chunks_exact_mut(4) {
        let alpha = u16::from(chunk[3]);
        if alpha == 255 {
            continue;
        }
        chunk[0] = ((u16::from(chunk[0]) * alpha + 127) / 255) as u8;
        chunk[1] = ((u16::from(chunk[1]) * alpha + 127) / 255) as u8;
        chunk[2] = ((u16::from(chunk[2]) * alpha + 127) / 255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos::open_url::{OpenUrlKind, classify_open_url};

    const ONE_BY_ONE_PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn remote_image_classifier_checks_suffix_only() {
        assert_eq!(
            classify_open_url("https://example.com/image.png?download=1"),
            OpenUrlKind::Image
        );
        assert_eq!(classify_open_url("https://example.com/index.html"), OpenUrlKind::Web);
    }

    #[test]
    fn local_image_classifier_detects_extensionless_png() {
        let dir = tempfile::tempdir().expect("tempdir");
        let image_path = dir.path().join("fixture");
        std::fs::write(&image_path, ONE_BY_ONE_PNG).expect("write png");
        let url = Url::from_file_path(&image_path).expect("file url").to_string();

        assert_eq!(classify_open_url(&url), OpenUrlKind::Image);
    }

    #[test]
    fn local_image_classifier_rejects_non_images() {
        let dir = tempfile::tempdir().expect("tempdir");
        let text_path = dir.path().join("fixture.txt");
        std::fs::write(&text_path, b"not an image").expect("write text");
        let url = Url::from_file_path(&text_path).expect("file url").to_string();

        assert_eq!(classify_open_url(&url), OpenUrlKind::Web);
    }

    #[test]
    fn fit_fill_and_actual_zoom_use_rotated_dimensions() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 200,
            height: 100,
            rgba: Arc::from(vec![255; 200 * 100 * 4]),
        });
        let viewport = PhysicalSize::new(300, 300);

        assert_eq!(state.zoom_factor(viewport), Some(1.5));

        state.scale_mode = ImageScaleMode::Fill;
        assert_eq!(state.zoom_factor(viewport), Some(3.0));

        state.scale_mode = ImageScaleMode::Actual;
        assert_eq!(state.zoom_factor(viewport), Some(1.0));

        state.scale_mode = ImageScaleMode::Fit;
        state.rotation_quarter_turns = 1;
        assert_eq!(state.zoom_factor(viewport), Some(1.5));
    }

    #[test]
    fn cursor_centered_zoom_preserves_focus_point() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 100,
            height: 100,
            rgba: Arc::from(vec![255; 100 * 100 * 4]),
        });
        state.scale_mode = ImageScaleMode::Actual;
        let viewport = PhysicalSize::new(150, 150);
        let cursor = PhysicalPosition::new(100.0, 75.0);

        state.zoom_by(2.0, cursor, viewport);

        assert_eq!(state.scale_mode, ImageScaleMode::Manual);
        assert!((state.manual_zoom - 2.0).abs() < f64::EPSILON);
        assert!((state.pan_x + 25.0).abs() < 0.001);
        assert!((state.pan_y - 0.0).abs() < 0.001);
    }

    #[test]
    fn clamp_pan_preserves_offsets_for_larger_images() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 100,
            height: 100,
            rgba: Arc::from(vec![255; 100 * 100 * 4]),
        });
        state.scale_mode = ImageScaleMode::Manual;
        state.manual_zoom = 4.0;
        let viewport = PhysicalSize::new(200, 200);

        state.pan_x = 500.0;
        state.pan_y = -500.0;
        state.clamp_pan(viewport);

        assert_eq!(state.pan_x, 500.0);
        assert_eq!(state.pan_y, -500.0);
    }

    #[test]
    fn clamp_pan_preserves_offsets_for_smaller_images() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 100,
            height: 100,
            rgba: Arc::from(vec![255; 100 * 100 * 4]),
        });
        state.scale_mode = ImageScaleMode::Actual;
        let viewport = PhysicalSize::new(200, 200);

        state.pan_x = 500.0;
        state.pan_y = -500.0;
        state.clamp_pan(viewport);

        assert_eq!(state.pan_x, 500.0);
        assert_eq!(state.pan_y, -500.0);
    }

    #[test]
    fn pan_by_moves_image_and_preserves_active_drag_anchor() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 100,
            height: 100,
            rgba: Arc::from(vec![255; 100 * 100 * 4]),
        });
        state.scale_mode = ImageScaleMode::Manual;
        state.manual_zoom = 4.0;
        let viewport = PhysicalSize::new(200, 200);
        let cursor = PhysicalPosition::new(100.0, 100.0);

        assert!(state.begin_pan(cursor));
        assert!(state.pan_by(30.0, -20.0, cursor, viewport));
        assert_eq!(state.pan_x, 30.0);
        assert_eq!(state.pan_y, -20.0);

        state.pan_to(PhysicalPosition::new(120.0, 110.0), viewport);
        assert_eq!(state.pan_x, 50.0);
        assert_eq!(state.pan_y, -10.0);
    }

    #[test]
    fn pinch_gesture_uses_additive_scale_deltas() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 100,
            height: 100,
            rgba: Arc::from(vec![255; 100 * 100 * 4]),
        });
        state.scale_mode = ImageScaleMode::Actual;
        let viewport = PhysicalSize::new(150, 150);
        let cursor = PhysicalPosition::new(75.0, 75.0);

        assert!(!state.pinch_gesture(0.0, TouchPhase::Started, cursor, viewport));
        assert!(state.pinch_gesture(0.2, TouchPhase::Moved, cursor, viewport));
        assert!(state.pinch_gesture(0.3, TouchPhase::Moved, cursor, viewport));
        assert!(!state.pinch_gesture(0.0, TouchPhase::Ended, cursor, viewport));

        assert_eq!(state.scale_mode, ImageScaleMode::Manual);
        assert!((state.manual_zoom - 1.5).abs() < 0.001);
    }

    #[test]
    fn smart_magnify_toggles_fit_and_actual() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 200,
            height: 100,
            rgba: Arc::from(vec![255; 200 * 100 * 4]),
        });
        let viewport = PhysicalSize::new(300, 300);

        assert!(state.smart_magnify(viewport));
        assert_eq!(state.scale_mode, ImageScaleMode::Actual);
        assert_eq!(state.zoom_factor(viewport), Some(1.0));

        assert!(state.smart_magnify(viewport));
        assert_eq!(state.scale_mode, ImageScaleMode::Fit);
        assert_eq!(state.zoom_factor(viewport), Some(1.5));
    }

    #[test]
    fn rotation_gesture_snaps_across_multiple_quarter_turns() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 100,
            height: 100,
            rgba: Arc::from(vec![255; 100 * 100 * 4]),
        });
        let viewport = PhysicalSize::new(200, 200);

        assert!(!state.rotation_gesture(0.0, TouchPhase::Started, viewport));
        assert!(state.rotation_gesture(-190.0, TouchPhase::Moved, viewport));
        assert_eq!(state.rotation_quarter_turns, 2);
        assert!((state.gesture_state.accumulated_rotation_degrees + 10.0).abs() < 0.001);
    }

    #[test]
    fn rotation_gesture_rounds_leftover_delta_on_end() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 100,
            height: 100,
            rgba: Arc::from(vec![255; 100 * 100 * 4]),
        });
        let viewport = PhysicalSize::new(200, 200);

        assert!(!state.rotation_gesture(0.0, TouchPhase::Started, viewport));
        assert!(state.rotation_gesture(50.0, TouchPhase::Ended, viewport));
        assert_eq!(state.rotation_quarter_turns, 3);
        assert_eq!(state.gesture_state.accumulated_rotation_degrees, 0.0);
    }

    #[test]
    fn touchpad_pressure_starts_and_ends_pan_once_per_press() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.bitmap = Some(LoadedImage {
            source: state.source.clone(),
            title: state.title.clone(),
            width: 100,
            height: 100,
            rgba: Arc::from(vec![255; 100 * 100 * 4]),
        });
        let cursor = PhysicalPosition::new(100.0, 100.0);

        state.scale_mode = ImageScaleMode::Manual;
        state.manual_zoom = 3.0;
        state.pan_x = 20.0;
        state.rotation_quarter_turns = 1;

        assert!(state.touchpad_pressure(1, cursor));
        assert!(state.is_panning());
        assert_eq!(state.scale_mode, ImageScaleMode::Manual);
        assert_eq!(state.rotation_quarter_turns, 1);

        state.rotation_quarter_turns = 1;
        assert!(!state.touchpad_pressure(2, cursor));
        assert_eq!(state.rotation_quarter_turns, 1);

        assert!(state.touchpad_pressure(0, cursor));
        assert!(!state.is_panning());
        assert!(state.touchpad_pressure(1, cursor));
        assert!(state.is_panning());
    }

    #[test]
    fn reset_view_clears_transient_gesture_state() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.gesture_state.accumulated_rotation_degrees = 30.0;
        state.gesture_state.last_pressure_stage = 2;

        state.reset_view();

        assert_eq!(state.gesture_state.accumulated_rotation_degrees, 0.0);
        assert_eq!(state.gesture_state.last_pressure_stage, 0);
    }

    #[test]
    fn source_and_error_reset_clear_transient_gesture_state() {
        let mut state = ImageViewState::new(String::from("file:///tmp/test.png"));
        state.gesture_state.accumulated_rotation_degrees = 30.0;
        state.gesture_state.last_pressure_stage = 2;

        state.reset_source(String::from("file:///tmp/next.png"));
        assert_eq!(state.gesture_state.accumulated_rotation_degrees, 0.0);
        assert_eq!(state.gesture_state.last_pressure_stage, 0);

        state.gesture_state.accumulated_rotation_degrees = -30.0;
        state.gesture_state.last_pressure_stage = 1;
        state.set_error(String::from("decode failed"));
        assert_eq!(state.gesture_state.accumulated_rotation_degrees, 0.0);
        assert_eq!(state.gesture_state.last_pressure_stage, 0);
    }
}
