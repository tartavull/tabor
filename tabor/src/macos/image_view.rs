use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use image::ImageFormat;
use url::Url;
use winit::dpi::{PhysicalPosition, PhysicalSize};

const LOCAL_IMAGE_PROBE_BYTES: usize = 64;
const MAX_REMOTE_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const ZOOM_STEP: f64 = 1.15;
const MIN_MANUAL_ZOOM: f64 = 0.05;
const MAX_MANUAL_ZOOM: f64 = 64.0;
const REMOTE_IMAGE_EXTENSIONS: &[&str] =
    &["png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenUrlKind {
    Web,
    Image,
}

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
        self.clamp_pan(viewport);
    }

    pub(crate) fn set_error(&mut self, message: String) {
        self.load_state = ImageLoadState::Error(message);
        self.bitmap = None;
        self.drag = None;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
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
        self.rotation_quarter_turns = (self.rotation_quarter_turns + 1) % 4;
        self.drag = None;
        self.clamp_pan(viewport);
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

    pub(crate) fn clamp_pan(&mut self, viewport: PhysicalSize<u32>) {
        let Some(_) = self.bitmap() else {
            self.pan_x = 0.0;
            self.pan_y = 0.0;
            return;
        };

        let display_size = self.display_size(viewport);
        let max_pan_x = ((display_size.0 - f64::from(viewport.width)) / 2.0).max(0.0);
        let max_pan_y = ((display_size.1 - f64::from(viewport.height)) / 2.0).max(0.0);
        self.pan_x = self.pan_x.clamp(-max_pan_x, max_pan_x);
        self.pan_y = self.pan_y.clamp(-max_pan_y, max_pan_y);
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
}

pub(crate) fn classify_open_url(url: &str) -> OpenUrlKind {
    if is_remote_image_url(url) || is_local_image_url(url) {
        OpenUrlKind::Image
    } else {
        OpenUrlKind::Web
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
    let parsed = Url::parse(url).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }
    parsed.to_file_path().ok()
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
    fn panning_clamps_to_visible_bounds() {
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

        assert_eq!(state.pan_x, 100.0);
        assert_eq!(state.pan_y, -100.0);
    }
}
