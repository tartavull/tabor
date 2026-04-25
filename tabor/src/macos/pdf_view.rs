use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hayro::{InterpreterSettings, Pdf, RenderSettings};
use image::imageops;
use image::{Rgba, RgbaImage};
use kurbo::Rect;
use pdf_extract::{
    Document as PdfExtractDocument, MediaBox as PdfExtractMediaBox, OutputDev, OutputError,
    Transform as PdfExtractTransform, output_doc_page,
};
use url::Url;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::TouchPhase;

use crate::macos::image_view::ImageRenderQuad;
use crate::renderer::BitmapCacheKey;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REMOTE_PDF_BYTES: usize = 128 * 1024 * 1024;
const PAGE_GAP: f64 = 24.0;
const ZOOM_STEP: f64 = 1.15;
const MIN_MANUAL_ZOOM: f64 = 0.25;
const MAX_MANUAL_ZOOM: f64 = 8.0;
const MAX_PAGE_CACHE_BYTES: usize = 256 * 1024 * 1024;
const MAX_PENDING_RENDER_REQUESTS: usize = 8;
const MAX_PREFETCH_RENDER_REQUESTS_PER_PASS: usize = 4;
const RENDER_SCALE_BUCKET: f64 = 0.125;
const AUTO_INVERT_MAX_CHECKED_PAGES: usize = 3;
const AUTO_INVERT_MAX_SAMPLED_PIXELS: usize = 16_384;
const AUTO_INVERT_WHITE_LUMINANCE_THRESHOLD: f64 = 0.92;
const AUTO_INVERT_DARK_LUMINANCE_THRESHOLD: f64 = 0.25;
const AUTO_INVERT_MIN_DARK_RATIO: f64 = 0.005;
const AUTO_INVERT_MAX_DARK_RATIO: f64 = 0.35;
const AUTO_INVERT_MIN_WHITE_RATIO: f64 = 0.70;
const AUTO_INVERT_MIN_AVERAGE_LUMINANCE: f64 = 0.75;

static NEXT_PDF_RENDER_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PdfZoomMode {
    FitWidth,
    FitPage,
    Actual,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PdfLoadState {
    Loading,
    Ready,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PdfDarkModeOverride {
    Auto,
    Forced(bool),
}

#[derive(Debug, Clone)]
pub struct LoadedPdfSource {
    pub source: String,
    pub title: String,
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PdfViewportRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PdfPageMetrics {
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PageRenderKey {
    page_index: usize,
    width_px: u16,
    height_px: u16,
}

#[derive(Debug, Clone)]
pub struct CachedPageBitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PageDrawInfo {
    page_index: usize,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PdfPagePoint {
    page_index: usize,
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SelectionState {
    anchor: PdfPagePoint,
    focus: PdfPagePoint,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PanDragState {
    cursor: PhysicalPosition<f64>,
    scroll_x: f64,
    scroll_y: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct PdfTextChunk {
    text: String,
    rect: Rect,
}

#[derive(Debug, Clone, PartialEq)]
struct PdfTextPage {
    chunks: Vec<PdfTextChunk>,
    normalized_text: String,
    normalized_chunk_indices: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PdfSearchMatch {
    page_index: usize,
    start: usize,
    end: usize,
}

struct PdfDocument {
    pdf: Arc<Pdf>,
    text_document: PdfExtractDocument,
    page_metrics: Vec<PdfPageMetrics>,
}

#[derive(Debug, Clone, Default)]
struct GestureState {
    last_pressure_stage: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PdfAutoInvertMetrics {
    average_luminance: f64,
    white_ratio: f64,
    dark_ratio: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PdfAutoInvertDecision {
    Invert,
    KeepNormal,
    Inconclusive,
}

#[derive(Clone)]
pub(crate) struct PdfRenderRequest {
    pub revision: u64,
    pub pdf: Arc<Pdf>,
    pub page_index: usize,
    pub width_px: u16,
    pub height_px: u16,
}

#[derive(Debug, Clone)]
pub struct PdfRasterizedPage {
    pub revision: u64,
    pub page_index: usize,
    pub width_px: u16,
    pub height_px: u16,
    pub bitmap: Option<CachedPageBitmap>,
}

pub(crate) struct PdfViewState {
    pub source: String,
    pub title: String,
    pub load_state: PdfLoadState,
    pub zoom_mode: PdfZoomMode,
    pub manual_zoom: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
    dark_mode_enabled: bool,
    dark_mode_override: PdfDarkModeOverride,
    auto_invert_decision: Option<bool>,
    checked_auto_invert_pages: Vec<usize>,
    render_revision: u64,
    fit_page_anchor_index: usize,
    document: Option<PdfDocument>,
    cached_pages: HashMap<PageRenderKey, CachedPageBitmap>,
    cache_order: VecDeque<PageRenderKey>,
    cached_page_bytes: usize,
    pending_renders: HashSet<PageRenderKey>,
    text_pages: Vec<Option<PdfTextPage>>,
    selection: Option<SelectionState>,
    pending_selection_anchor: Option<PdfPagePoint>,
    pan_drag: Option<PanDragState>,
    gesture_state: GestureState,
    last_search_query: Option<String>,
    search_matches: Vec<PdfSearchMatch>,
    active_search_match: Option<usize>,
}

impl PdfViewState {
    pub(crate) fn new(source: String) -> Self {
        Self {
            title: pdf_title_for_source(&source),
            source,
            load_state: PdfLoadState::Loading,
            zoom_mode: PdfZoomMode::FitWidth,
            manual_zoom: 1.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            dark_mode_enabled: false,
            dark_mode_override: PdfDarkModeOverride::Auto,
            auto_invert_decision: None,
            checked_auto_invert_pages: Vec::new(),
            render_revision: next_pdf_render_revision(),
            fit_page_anchor_index: 0,
            document: None,
            cached_pages: HashMap::new(),
            cache_order: VecDeque::new(),
            cached_page_bytes: 0,
            pending_renders: HashSet::new(),
            text_pages: Vec::new(),
            selection: None,
            pending_selection_anchor: None,
            pan_drag: None,
            gesture_state: GestureState::default(),
            last_search_query: None,
            search_matches: Vec::new(),
            active_search_match: None,
        }
    }

    pub(crate) fn reset_source(&mut self, source: String) {
        self.source = source;
        self.title = pdf_title_for_source(&self.source);
        self.load_state = PdfLoadState::Loading;
        self.document = None;
        self.reset_auto_invert_state();
        self.invalidate_render_cache();
        self.text_pages.clear();
        self.reset_view();
        self.clear_search();
    }

    pub(crate) fn set_loaded(&mut self, loaded: LoadedPdfSource) -> Result<(), String> {
        let pdf = Arc::new(
            Pdf::new(Arc::new(loaded.bytes.clone()))
                .map_err(|err| format!("pdf parse failed: {err:?}"))?,
        );
        let text_document = PdfExtractDocument::load_mem(loaded.bytes.as_ref())
            .map_err(|err| format!("pdf text parse failed: {err}"))?;
        let page_metrics = pdf
            .pages()
            .iter()
            .map(|page| {
                let (width, height) = page.render_dimensions();
                PdfPageMetrics { width: f64::from(width), height: f64::from(height) }
            })
            .collect::<Vec<_>>();
        if page_metrics.is_empty() {
            return Err(String::from("pdf contains no pages"));
        }

        self.source = loaded.source;
        self.title = loaded.title;
        self.document = Some(PdfDocument { pdf, text_document, page_metrics });
        self.text_pages = vec![None; self.page_count()];
        self.load_state = PdfLoadState::Ready;
        self.reset_auto_invert_state();
        self.invalidate_render_cache();
        self.reset_view();
        self.clear_search();
        Ok(())
    }

    pub(crate) fn set_error(&mut self, message: String) {
        self.load_state = PdfLoadState::Error(message);
        self.document = None;
        self.invalidate_render_cache();
        self.text_pages.clear();
        self.selection = None;
        self.pending_selection_anchor = None;
        self.pan_drag = None;
        self.clear_search();
    }

    pub(crate) fn page_count(&self) -> usize {
        self.document.as_ref().map_or(0, |document| document.page_metrics.len())
    }

    fn clear_render_cache(&mut self) {
        self.cached_pages.clear();
        self.cache_order.clear();
        self.cached_page_bytes = 0;
        self.pending_renders.clear();
    }

    fn invalidate_render_cache(&mut self) {
        self.render_revision = next_pdf_render_revision();
        self.clear_render_cache();
    }

    fn reset_auto_invert_state(&mut self) {
        self.dark_mode_override = PdfDarkModeOverride::Auto;
        self.auto_invert_decision = None;
        self.checked_auto_invert_pages.clear();
    }

    fn requested_dark_inversion(&self) -> bool {
        match self.dark_mode_override {
            PdfDarkModeOverride::Auto => self.auto_invert_decision.unwrap_or(false),
            PdfDarkModeOverride::Forced(value) => value,
        }
    }

    fn effective_dark_inversion(&self) -> bool {
        self.dark_mode_enabled && self.requested_dark_inversion()
    }

    pub(crate) fn footer_invert_status(&self) -> &'static str {
        match self.dark_mode_override {
            PdfDarkModeOverride::Auto if self.auto_invert_decision.unwrap_or(false) => "auto-on",
            PdfDarkModeOverride::Auto => "auto-off",
            PdfDarkModeOverride::Forced(true) => "on",
            PdfDarkModeOverride::Forced(false) => "off",
        }
    }

    pub(crate) fn set_dark_mode_enabled(&mut self, dark_mode_enabled: bool) -> bool {
        let effective_before = self.effective_dark_inversion();
        self.dark_mode_enabled = dark_mode_enabled;
        if self.effective_dark_inversion() == effective_before {
            return false;
        }

        self.invalidate_render_cache();
        true
    }

    pub(crate) fn toggle_dark_mode_override(&mut self) -> bool {
        let effective_before = self.effective_dark_inversion();
        self.dark_mode_override = match self.dark_mode_override {
            PdfDarkModeOverride::Auto => {
                PdfDarkModeOverride::Forced(!self.requested_dark_inversion())
            },
            PdfDarkModeOverride::Forced(_) => PdfDarkModeOverride::Auto,
        };
        if self.effective_dark_inversion() != effective_before {
            self.invalidate_render_cache();
        }
        true
    }

    pub(crate) fn current_page(&self, viewport: PhysicalSize<u32>) -> usize {
        let center_y = f64::from(viewport.height) / 2.0;
        self.page_draws(viewport)
            .into_iter()
            .min_by(|left, right| {
                let left_mid = left.y + left.height / 2.0;
                let right_mid = right.y + right.height / 2.0;
                (left_mid - center_y)
                    .abs()
                    .partial_cmp(&(right_mid - center_y).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map_or(1, |draw| draw.page_index + 1)
    }

    #[allow(dead_code)]
    pub(crate) fn page_dimensions(&self, page_index: usize) -> Option<(u32, u32)> {
        let metrics = self.document.as_ref()?.page_metrics.get(page_index)?;
        Some((metrics.width.round() as u32, metrics.height.round() as u32))
    }

    pub(crate) fn reset_view(&mut self) {
        self.zoom_mode = PdfZoomMode::FitWidth;
        self.manual_zoom = 1.0;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.fit_page_anchor_index = 0;
        self.selection = None;
        self.pending_selection_anchor = None;
        self.pan_drag = None;
        self.gesture_state = GestureState::default();
    }

    pub(crate) fn zoom_factor(&self, viewport: PhysicalSize<u32>) -> Option<f64> {
        let document = self.document.as_ref()?;
        let max_width = document
            .page_metrics
            .iter()
            .fold(0.0f64, |max_width, metrics| max_width.max(metrics.width));
        let fit_page_anchor = document
            .page_metrics
            .get(self.fit_page_anchor_index)
            .copied()
            .unwrap_or(document.page_metrics[0]);
        let zoom = match self.zoom_mode {
            PdfZoomMode::FitWidth => {
                if max_width <= 0.0 {
                    1.0
                } else {
                    f64::from(viewport.width).max(1.0) / max_width
                }
            },
            PdfZoomMode::FitPage => {
                let width_zoom =
                    f64::from(viewport.width).max(1.0) / fit_page_anchor.width.max(1.0);
                let height_zoom =
                    f64::from(viewport.height).max(1.0) / fit_page_anchor.height.max(1.0);
                width_zoom.min(height_zoom)
            },
            PdfZoomMode::Actual => 1.0,
            PdfZoomMode::Manual => self.manual_zoom,
        };

        Some(zoom.clamp(MIN_MANUAL_ZOOM, MAX_MANUAL_ZOOM))
    }

    fn horizontal_pan_limits(&self, viewport: PhysicalSize<u32>) -> Option<(f64, f64)> {
        let document = self.document.as_ref()?;
        let scale = self.zoom_factor(viewport)?;
        let content_width = document
            .page_metrics
            .iter()
            .fold(0.0f64, |max_width, metrics| max_width.max(metrics.width * scale));
        let half_overflow = (content_width - f64::from(viewport.width)).max(0.0) / 2.0;
        Some((-half_overflow, half_overflow))
    }

    fn clamped_scroll_x(&self, viewport: PhysicalSize<u32>) -> f64 {
        let Some((min_scroll_x, max_scroll_x)) = self.horizontal_pan_limits(viewport) else {
            return 0.0;
        };
        self.scroll_x.clamp(min_scroll_x, max_scroll_x)
    }

    fn clamp_horizontal_scroll(&mut self, viewport: PhysicalSize<u32>) {
        self.scroll_x = self.clamped_scroll_x(viewport);
    }

    fn page_draws(&self, viewport: PhysicalSize<u32>) -> Vec<PageDrawInfo> {
        let Some(document) = self.document.as_ref() else {
            return Vec::new();
        };
        let Some(scale) = self.zoom_factor(viewport) else {
            return Vec::new();
        };

        let scroll_x = self.clamped_scroll_x(viewport);
        let mut y = self.scroll_y;
        document
            .page_metrics
            .iter()
            .enumerate()
            .map(|(page_index, metrics)| {
                let width = metrics.width * scale;
                let height = metrics.height * scale;
                let x = (f64::from(viewport.width) - width) / 2.0 + scroll_x;
                let draw = PageDrawInfo { page_index, x, y, width, height, scale };
                y += height + PAGE_GAP * scale;
                draw
            })
            .collect()
    }

    fn refresh_fit_page_anchor(&mut self, viewport: PhysicalSize<u32>) {
        if self.zoom_mode != PdfZoomMode::FitPage {
            return;
        }

        let page_count = self.page_count();
        if page_count == 0 {
            self.fit_page_anchor_index = 0;
            return;
        }

        self.fit_page_anchor_index =
            self.current_page(viewport).saturating_sub(1).min(page_count - 1);
    }

    pub(crate) fn begin_selection(
        &mut self,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        let Some(point) = self.viewport_point_to_page_point(cursor, viewport) else {
            self.selection = None;
            self.pending_selection_anchor = None;
            return false;
        };

        self.pending_selection_anchor = Some(point);
        self.selection = Some(SelectionState { anchor: point, focus: point });
        true
    }

    pub(crate) fn update_selection(
        &mut self,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        let Some(anchor) = self.pending_selection_anchor else {
            return false;
        };
        let Some(point) = self.viewport_point_to_page_point(cursor, viewport) else {
            return false;
        };
        if point.page_index != anchor.page_index {
            return false;
        }
        self.selection = Some(SelectionState { anchor, focus: point });
        true
    }

    pub(crate) fn end_selection(&mut self) {
        self.pending_selection_anchor = None;
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
        self.pending_selection_anchor = None;
    }

    pub(crate) fn selection_text(&mut self) -> Option<String> {
        let selection = self.selection?;
        let page = self.ensure_text_page(selection.anchor.page_index)?;
        let selection_rect = selection_page_rect(selection);
        let mut text = String::new();
        for chunk in &page.chunks {
            if chunk.rect.intersect(selection_rect).area() > 0.0 {
                text.push_str(&chunk.text);
            }
        }

        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    pub(crate) fn begin_pan(&mut self, cursor: PhysicalPosition<f64>) -> bool {
        if self.document.is_none() {
            return false;
        }

        self.pan_drag =
            Some(PanDragState { cursor, scroll_x: self.scroll_x, scroll_y: self.scroll_y });
        true
    }

    pub(crate) fn pan_to(&mut self, cursor: PhysicalPosition<f64>, viewport: PhysicalSize<u32>) {
        let Some(drag) = self.pan_drag else {
            return;
        };

        self.scroll_x = drag.scroll_x + cursor.x - drag.cursor.x;
        self.scroll_y = drag.scroll_y + cursor.y - drag.cursor.y;
        self.clamp_horizontal_scroll(viewport);
    }

    pub(crate) fn end_pan(&mut self) {
        self.pan_drag = None;
    }

    pub(crate) fn is_panning(&self) -> bool {
        self.pan_drag.is_some()
    }

    pub(crate) fn pan_by(
        &mut self,
        delta_x: f64,
        delta_y: f64,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        if self.document.is_none() || (!delta_x.is_finite() && !delta_y.is_finite()) {
            return false;
        }

        let delta_x = if delta_x.is_finite() { delta_x } else { 0.0 };
        let delta_y = if delta_y.is_finite() { delta_y } else { 0.0 };
        if delta_x.abs() <= f64::EPSILON && delta_y.abs() <= f64::EPSILON {
            return false;
        }

        self.scroll_x += delta_x;
        self.scroll_y += delta_y;
        self.clamp_horizontal_scroll(viewport);
        true
    }

    pub(crate) fn touchpad_pressure(&mut self, stage: i64, cursor: PhysicalPosition<f64>) -> bool {
        if stage <= 0 {
            let was_active = self.gesture_state.last_pressure_stage > 0 && self.pan_drag.is_some();
            self.gesture_state.last_pressure_stage = 0;
            self.end_pan();
            return was_active;
        }

        let previous = self.gesture_state.last_pressure_stage;
        self.gesture_state.last_pressure_stage = stage;
        if previous > 0 {
            return false;
        }

        self.begin_pan(cursor)
    }

    pub(crate) fn zoom_in(
        &mut self,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        self.zoom_by(ZOOM_STEP, cursor, viewport)
    }

    pub(crate) fn zoom_out(
        &mut self,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        self.zoom_by(1.0 / ZOOM_STEP, cursor, viewport)
    }

    pub(crate) fn zoom_fit_width(&mut self, viewport: PhysicalSize<u32>) -> bool {
        if self.document.is_none() {
            return false;
        }
        self.zoom_mode = PdfZoomMode::FitWidth;
        self.clamp_horizontal_scroll(viewport);
        true
    }

    pub(crate) fn zoom_fit_page(&mut self, viewport: PhysicalSize<u32>) -> bool {
        if self.document.is_none() {
            return false;
        }
        self.zoom_mode = PdfZoomMode::FitPage;
        self.clamp_horizontal_scroll(viewport);
        true
    }

    pub(crate) fn zoom_actual(&mut self, viewport: PhysicalSize<u32>) -> bool {
        if self.document.is_none() {
            return false;
        }
        self.zoom_mode = PdfZoomMode::Actual;
        self.manual_zoom = 1.0;
        self.clamp_horizontal_scroll(viewport);
        true
    }

    pub(crate) fn smart_magnify(&mut self, viewport: PhysicalSize<u32>) -> bool {
        if self.document.is_none() {
            return false;
        }

        match self.zoom_mode {
            PdfZoomMode::Actual => self.zoom_fit_width(viewport),
            PdfZoomMode::FitWidth | PdfZoomMode::FitPage | PdfZoomMode::Manual => {
                self.zoom_actual(viewport)
            },
        }
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

        self.zoom_by(new_zoom / old_zoom, cursor, viewport)
    }

    pub(crate) fn find(
        &mut self,
        query: &str,
        backwards: bool,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        if self.document.is_none() {
            return false;
        }
        let query = normalize_for_search(query);
        if query.is_empty() {
            return false;
        }

        if self.last_search_query.as_deref() != Some(query.as_str()) {
            self.last_search_query = Some(query.clone());
            self.active_search_match = None;
            self.search_matches = self.collect_search_matches(&query);
        }

        if self.search_matches.is_empty() {
            self.active_search_match = None;
            return false;
        }

        self.active_search_match = Some(match self.active_search_match {
            Some(active) if backwards => {
                active.checked_sub(1).unwrap_or(self.search_matches.len() - 1)
            },
            Some(active) => (active + 1) % self.search_matches.len(),
            None if backwards => self.search_matches.len() - 1,
            None => 0,
        });

        if let Some(search_match) =
            self.active_search_match.and_then(|index| self.search_matches.get(index).copied())
        {
            self.scroll_match_into_view(search_match, viewport);
            return true;
        }

        false
    }

    pub(crate) fn active_search_match_count(&self) -> usize {
        self.search_matches.len()
    }

    pub(crate) fn search_query(&self) -> Option<&str> {
        self.last_search_query.as_deref()
    }

    pub(crate) fn take_visible_render_requests(
        &mut self,
        viewport: PhysicalSize<u32>,
    ) -> Vec<PdfRenderRequest> {
        if !matches!(self.load_state, PdfLoadState::Ready) {
            return Vec::new();
        }

        self.refresh_fit_page_anchor(viewport);
        let Some(pdf) = self.document.as_ref().map(|document| document.pdf.clone()) else {
            return Vec::new();
        };

        let center_y = f64::from(viewport.height) / 2.0;
        let mut draws = self.visible_page_draws(viewport);
        draws.sort_by(|left, right| {
            let left_mid = left.y + left.height / 2.0;
            let right_mid = right.y + right.height / 2.0;
            (left_mid - center_y)
                .abs()
                .partial_cmp(&(right_mid - center_y).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut requests = Vec::new();
        for draw in draws {
            if self.pending_renders.len() >= MAX_PENDING_RENDER_REQUESTS
                || requests.len() >= MAX_PREFETCH_RENDER_REQUESTS_PER_PASS
            {
                break;
            }

            let Some(key) = self.page_render_key(draw.page_index, draw.scale) else {
                continue;
            };
            if self.cached_pages.contains_key(&key) || self.pending_renders.contains(&key) {
                continue;
            }

            self.pending_renders.insert(key);
            requests.push(PdfRenderRequest {
                revision: self.render_revision,
                pdf: pdf.clone(),
                page_index: key.page_index,
                width_px: key.width_px,
                height_px: key.height_px,
            });
        }

        requests
    }

    pub(crate) fn apply_rasterized_page(&mut self, raster: PdfRasterizedPage) -> bool {
        if raster.revision != self.render_revision {
            return false;
        }

        let key = raster.key();
        self.pending_renders.remove(&key);
        if let Some(bitmap) = raster.bitmap {
            let effective_before = self.effective_dark_inversion();
            self.observe_auto_invert_page(raster.page_index, &bitmap);
            let effective_after = self.effective_dark_inversion();
            if effective_after != effective_before {
                self.invalidate_render_cache();
            }

            let bitmap = if effective_after { invert_pdf_bitmap(bitmap) } else { bitmap };
            self.insert_cached_page(key, bitmap);
            return true;
        }

        false
    }

    pub(crate) fn visible_page_images(
        &self,
        viewport: PhysicalSize<u32>,
    ) -> Vec<(BitmapCacheKey, &CachedPageBitmap, ImageRenderQuad)> {
        self.visible_page_draws(viewport)
            .into_iter()
            .filter_map(|draw| {
                let desired_key = self.page_render_key(draw.page_index, draw.scale)?;
                let (cache_key, bitmap) = self.cached_page_for_draw(desired_key)?;
                Some((
                    self.renderer_cache_key(cache_key),
                    bitmap,
                    page_quad(draw, bitmap.width as f32, bitmap.height as f32),
                ))
            })
            .collect()
    }

    pub(crate) fn overlay_rects(&mut self, viewport: PhysicalSize<u32>) -> Vec<PdfViewportRect> {
        let mut rects = Vec::new();

        if let Some(selection) = self.selection {
            if let Some(chunks) =
                self.ensure_text_page(selection.anchor.page_index).map(|page| page.chunks.clone())
            {
                let selection_rect = selection_page_rect(selection);
                let page_index = selection.anchor.page_index;
                for chunk in &chunks {
                    if chunk.rect.intersect(selection_rect).area() > 0.0 {
                        if let Some(rect) =
                            self.chunk_viewport_rect(page_index, &chunk.rect, viewport)
                        {
                            rects.push(rect);
                        }
                    }
                }
            }
        }

        if let Some(active) =
            self.active_search_match.and_then(|index| self.search_matches.get(index).copied())
        {
            if let Some(page) = self.ensure_text_page(active.page_index).cloned() {
                let mut seen = HashSet::new();
                for &chunk_index in &page.normalized_chunk_indices[active.start..active.end] {
                    if !seen.insert(chunk_index) {
                        continue;
                    }
                    if let Some(chunk) = page.chunks.get(chunk_index) {
                        if let Some(rect) =
                            self.chunk_viewport_rect(active.page_index, &chunk.rect, viewport)
                        {
                            rects.push(rect);
                        }
                    }
                }
            }
        }

        rects
    }

    #[allow(dead_code)]
    pub(crate) fn debug_snapshot(
        &mut self,
        viewport: PhysicalSize<u32>,
        background: [u8; 4],
    ) -> Option<RgbaImage> {
        self.ensure_visible_page_bitmaps_for_snapshot(viewport);
        let mut canvas = RgbaImage::from_pixel(viewport.width, viewport.height, Rgba(background));
        for (_, bitmap, quad) in self.visible_page_images(viewport) {
            let rendered = RgbaImage::from_raw(bitmap.width, bitmap.height, bitmap.rgba.to_vec())?;
            imageops::overlay(
                &mut canvas,
                &rendered,
                quad.dest_x_px.round() as i64,
                quad.dest_y_px.round() as i64,
            );
        }
        for rect in self.overlay_rects(viewport) {
            fill_rect(&mut canvas, rect, Rgba([255, 208, 64, 110]));
        }
        Some(canvas)
    }

    fn visible_page_draws(&self, viewport: PhysicalSize<u32>) -> Vec<PageDrawInfo> {
        let height = f64::from(viewport.height);
        self.page_draws(viewport)
            .into_iter()
            .filter(|draw| draw.y + draw.height >= -draw.height && draw.y <= height + draw.height)
            .collect()
    }

    fn viewport_point_to_page_point(
        &self,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) -> Option<PdfPagePoint> {
        self.page_draws(viewport).into_iter().find_map(|draw| {
            if cursor.x < draw.x
                || cursor.x > draw.x + draw.width
                || cursor.y < draw.y
                || cursor.y > draw.y + draw.height
            {
                return None;
            }

            Some(PdfPagePoint {
                page_index: draw.page_index,
                x: (cursor.x - draw.x) / draw.scale,
                y: (cursor.y - draw.y) / draw.scale,
            })
        })
    }

    fn chunk_viewport_rect(
        &self,
        page_index: usize,
        chunk_rect: &Rect,
        viewport: PhysicalSize<u32>,
    ) -> Option<PdfViewportRect> {
        let draw =
            self.page_draws(viewport).into_iter().find(|draw| draw.page_index == page_index)?;
        Some(PdfViewportRect {
            x: draw.x + chunk_rect.x0 * draw.scale,
            y: draw.y + chunk_rect.y0 * draw.scale,
            width: (chunk_rect.x1 - chunk_rect.x0).max(1.0) * draw.scale,
            height: (chunk_rect.y1 - chunk_rect.y0).max(1.0) * draw.scale,
        })
    }

    fn zoom_by(
        &mut self,
        factor: f64,
        cursor: PhysicalPosition<f64>,
        viewport: PhysicalSize<u32>,
    ) -> bool {
        let Some(old_zoom) = self.zoom_factor(viewport) else {
            return false;
        };
        let anchor = self.viewport_point_to_page_point(cursor, viewport);
        let new_zoom = (old_zoom * factor).clamp(MIN_MANUAL_ZOOM, MAX_MANUAL_ZOOM);
        if (new_zoom - old_zoom).abs() <= f64::EPSILON {
            return false;
        }

        self.zoom_mode = PdfZoomMode::Manual;
        self.manual_zoom = new_zoom;

        if let Some(anchor) = anchor {
            if let Some(draw) = self
                .page_draws(viewport)
                .into_iter()
                .find(|draw| draw.page_index == anchor.page_index)
            {
                let current_x = draw.x + anchor.x * draw.scale;
                let current_y = draw.y + anchor.y * draw.scale;
                self.scroll_x += cursor.x - current_x;
                self.scroll_y += cursor.y - current_y;
            }
        }

        self.clamp_horizontal_scroll(viewport);
        true
    }

    fn page_render_key(&self, page_index: usize, scale: f64) -> Option<PageRenderKey> {
        let metrics = self.document.as_ref()?.page_metrics.get(page_index)?;
        let bucketed_scale = quantize_render_scale(scale);
        let width_px =
            (metrics.width * bucketed_scale).round().clamp(1.0, f64::from(u16::MAX)) as u16;
        let height_px =
            (metrics.height * bucketed_scale).round().clamp(1.0, f64::from(u16::MAX)) as u16;
        Some(PageRenderKey { page_index, width_px, height_px })
    }

    fn insert_cached_page(&mut self, key: PageRenderKey, bitmap: CachedPageBitmap) {
        if let Some(previous) = self.cached_pages.insert(key, bitmap) {
            self.cached_page_bytes = self.cached_page_bytes.saturating_sub(previous.rgba.len());
        }
        let bitmap = self.cached_pages.get(&key).expect("cached page inserted");
        self.cached_page_bytes = self.cached_page_bytes.saturating_add(bitmap.rgba.len());
        self.cache_order.retain(|existing| existing != &key);
        self.cache_order.push_back(key);
        while self.cached_page_bytes > MAX_PAGE_CACHE_BYTES {
            if let Some(oldest) = self.cache_order.pop_front() {
                if let Some(removed) = self.cached_pages.remove(&oldest) {
                    self.cached_page_bytes =
                        self.cached_page_bytes.saturating_sub(removed.rgba.len());
                }
            }
        }
    }

    fn cached_page_for_draw(
        &self,
        desired_key: PageRenderKey,
    ) -> Option<(PageRenderKey, &CachedPageBitmap)> {
        if let Some(bitmap) = self.cached_pages.get(&desired_key) {
            return Some((desired_key, bitmap));
        }

        self.cached_pages
            .iter()
            .filter(|(key, _)| key.page_index == desired_key.page_index)
            .min_by_key(|(key, _)| {
                u32::from(key.width_px.abs_diff(desired_key.width_px))
                    + u32::from(key.height_px.abs_diff(desired_key.height_px))
            })
            .map(|(key, bitmap)| (*key, bitmap))
    }

    fn renderer_cache_key(&self, key: PageRenderKey) -> BitmapCacheKey {
        BitmapCacheKey {
            namespace: self.render_revision,
            entry: ((key.page_index as u64) << 32)
                | ((u64::from(key.width_px)) << 16)
                | u64::from(key.height_px),
        }
    }

    fn ensure_visible_page_bitmaps_for_snapshot(&mut self, viewport: PhysicalSize<u32>) {
        let requests = self.take_visible_render_requests(viewport);
        for request in requests {
            self.apply_rasterized_page(rasterize_pdf_page(&request));
        }
    }

    fn ensure_text_page(&mut self, page_index: usize) -> Option<&PdfTextPage> {
        if self.text_pages.get(page_index).and_then(Option::as_ref).is_some() {
            return self.text_pages.get(page_index).and_then(Option::as_ref);
        }

        let document = self.document.as_ref()?;
        let page_height = document.page_metrics.get(page_index)?.height;
        let mut extractor = PdfTextCollector::new(page_height);
        output_doc_page(&document.text_document, &mut extractor, page_index as u32 + 1).ok()?;

        let mut normalized_text = String::new();
        let mut normalized_chunk_indices = Vec::new();
        for (chunk_index, chunk) in extractor.chunks.iter().enumerate() {
            for character in chunk.text.chars() {
                for lower in character.to_lowercase() {
                    normalized_text.push(lower);
                    normalized_chunk_indices.push(chunk_index);
                }
            }
        }

        self.text_pages[page_index] = Some(PdfTextPage {
            chunks: extractor.chunks,
            normalized_text,
            normalized_chunk_indices,
        });
        self.text_pages.get(page_index).and_then(Option::as_ref)
    }

    fn collect_search_matches(&mut self, query: &str) -> Vec<PdfSearchMatch> {
        let mut matches = Vec::new();
        for page_index in 0..self.page_count() {
            let Some(page) = self.ensure_text_page(page_index) else {
                continue;
            };
            let mut search_start = 0usize;
            while let Some(found) = page.normalized_text[search_start..].find(query) {
                let match_start = search_start + found;
                let start = page.normalized_text[..match_start].chars().count();
                let end = start + query.chars().count();
                matches.push(PdfSearchMatch { page_index, start, end });
                let next_char_bytes = page.normalized_text[match_start..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
                search_start = match_start + next_char_bytes;
            }
        }
        matches
    }

    fn scroll_match_into_view(
        &mut self,
        search_match: PdfSearchMatch,
        viewport: PhysicalSize<u32>,
    ) {
        let Some(page) = self.ensure_text_page(search_match.page_index) else {
            return;
        };
        let mut y0 = f64::MAX;
        let mut y1 = 0.0f64;
        for &chunk_index in &page.normalized_chunk_indices[search_match.start..search_match.end] {
            let Some(chunk) = page.chunks.get(chunk_index) else {
                continue;
            };
            y0 = y0.min(chunk.rect.y0);
            y1 = y1.max(chunk.rect.y1);
        }
        if !y0.is_finite() || y1 <= y0 {
            return;
        }

        let draw = self
            .page_draws(viewport)
            .into_iter()
            .find(|draw| draw.page_index == search_match.page_index);
        let Some(draw) = draw else {
            return;
        };
        let match_mid_y = draw.y + ((y0 + y1) / 2.0) * draw.scale;
        self.scroll_y -= match_mid_y - (f64::from(viewport.height) / 2.0);
    }

    fn clear_search(&mut self) {
        self.last_search_query = None;
        self.search_matches.clear();
        self.active_search_match = None;
    }

    fn observe_auto_invert_page(&mut self, page_index: usize, bitmap: &CachedPageBitmap) {
        if self.auto_invert_decision.is_some()
            || self.checked_auto_invert_pages.len() >= AUTO_INVERT_MAX_CHECKED_PAGES
            || self.checked_auto_invert_pages.contains(&page_index)
        {
            return;
        }

        self.checked_auto_invert_pages.push(page_index);
        match auto_invert_decision(bitmap) {
            PdfAutoInvertDecision::Invert => self.auto_invert_decision = Some(true),
            PdfAutoInvertDecision::KeepNormal => self.auto_invert_decision = Some(false),
            PdfAutoInvertDecision::Inconclusive
                if self.checked_auto_invert_pages.len() >= AUTO_INVERT_MAX_CHECKED_PAGES =>
            {
                self.auto_invert_decision = Some(false);
            },
            PdfAutoInvertDecision::Inconclusive => (),
        }
    }
}

impl Default for PdfViewState {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl PdfRasterizedPage {
    fn key(&self) -> PageRenderKey {
        PageRenderKey {
            page_index: self.page_index,
            width_px: self.width_px,
            height_px: self.height_px,
        }
    }
}

#[derive(Default)]
struct PdfTextCollector {
    page_height: f64,
    chunks: Vec<PdfTextChunk>,
    last_end_x: Option<f64>,
    last_baseline_y: Option<f64>,
    pending_word_gap: bool,
    pending_line_break: bool,
}

impl PdfTextCollector {
    fn new(page_height: f64) -> Self {
        Self { page_height, ..Default::default() }
    }

    fn flush_pending_spacing(&mut self, x: f64, y: f64, font_size: f64) {
        if self.pending_line_break {
            self.chunks.push(PdfTextChunk {
                text: String::from("\n"),
                rect: Rect::new(x, y, x + 1.0, y + font_size.max(1.0)),
            });
            self.pending_line_break = false;
            self.pending_word_gap = false;
            return;
        }

        if self.pending_word_gap {
            self.chunks.push(PdfTextChunk {
                text: String::from(" "),
                rect: Rect::new(x, y, x + font_size.max(1.0) * 0.35, y + font_size.max(1.0)),
            });
            self.pending_word_gap = false;
        }
    }
}

impl OutputDev for PdfTextCollector {
    fn begin_page(
        &mut self,
        _page_num: u32,
        media_box: &PdfExtractMediaBox,
        _art_box: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        self.page_height = media_box.ury - media_box.lly;
        self.last_end_x = None;
        self.last_baseline_y = None;
        self.pending_word_gap = false;
        self.pending_line_break = false;
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &PdfExtractTransform,
        width: f64,
        spacing: f64,
        font_size: f64,
        character: &str,
    ) -> Result<(), OutputError> {
        let transformed_font_size =
            ((trm.m11 * font_size).abs() * (trm.m22 * font_size).abs()).sqrt().max(1.0);
        let x = trm.m31;
        let y = trm.m32;
        self.flush_pending_spacing(x, y, transformed_font_size);

        let width_px = (width * font_size + spacing).abs().max(transformed_font_size * 0.2);
        self.chunks.push(PdfTextChunk {
            text: character.to_string(),
            rect: Rect::new(x, y, x + width_px, y + transformed_font_size),
        });
        self.last_end_x = Some(x + width_px);
        self.last_baseline_y = Some(y);
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), OutputError> {
        if !self.chunks.is_empty() {
            self.pending_word_gap = true;
        }
        Ok(())
    }

    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> Result<(), OutputError> {
        if !self.chunks.is_empty() {
            self.pending_line_break = true;
        }
        Ok(())
    }
}

pub(crate) fn load_pdf_source(source: &str) -> Result<LoadedPdfSource, String> {
    if let Some(path) = crate::macos::open_url::local_file_path(source) {
        return load_local_pdf(&path, source);
    }

    let response = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .timeout_write(WRITE_TIMEOUT)
        .build()
        .get(source)
        .call()
        .map_err(|err| format!("pdf request failed: {err}"))?;

    if response.status() >= 400 {
        return Err(format!("pdf request failed with HTTP {}", response.status()));
    }

    let final_source = response.get_url().to_string();
    let mut bytes = Vec::new();
    let mut reader = response.into_reader().take((MAX_REMOTE_PDF_BYTES + 1) as u64);
    reader.read_to_end(&mut bytes).map_err(|err| format!("pdf download failed: {err}"))?;
    if bytes.len() > MAX_REMOTE_PDF_BYTES {
        return Err(format!("pdf download exceeded {} bytes", MAX_REMOTE_PDF_BYTES));
    }
    if !bytes.windows(5).any(|window| window == b"%PDF-") {
        return Err(String::from("download did not contain a PDF header"));
    }

    Ok(LoadedPdfSource {
        title: pdf_title_for_source(&final_source),
        source: final_source,
        bytes: Arc::from(bytes),
    })
}

pub(crate) fn pdf_title_for_source(source: &str) -> String {
    if let Some(path) = crate::macos::open_url::local_file_path(source) {
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

fn load_local_pdf(path: &Path, source: &str) -> Result<LoadedPdfSource, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|err| format!("failed to read PDF: {err}"))?;

    if !bytes.windows(5).any(|window| window == b"%PDF-") {
        return Err(String::from("file does not contain a PDF header"));
    }

    Ok(LoadedPdfSource {
        title: pdf_title_for_source(source),
        source: source.to_string(),
        bytes: Arc::from(bytes),
    })
}

fn page_quad(draw: PageDrawInfo, source_width: f32, source_height: f32) -> ImageRenderQuad {
    ImageRenderQuad {
        dest_x_px: draw.x as f32,
        dest_y_px: draw.y as f32,
        dest_width_px: draw.width as f32,
        dest_height_px: draw.height as f32,
        uv_top_left: (0.0, 0.0),
        uv_top_right: (source_width, 0.0),
        uv_bottom_left: (0.0, source_height),
        uv_bottom_right: (source_width, source_height),
    }
}

fn selection_page_rect(selection: SelectionState) -> Rect {
    Rect::new(
        selection.anchor.x.min(selection.focus.x),
        selection.anchor.y.min(selection.focus.y),
        selection.anchor.x.max(selection.focus.x),
        selection.anchor.y.max(selection.focus.y),
    )
}

fn next_pdf_render_revision() -> u64 {
    NEXT_PDF_RENDER_REVISION.fetch_add(1, Ordering::Relaxed)
}

fn quantize_render_scale(scale: f64) -> f64 {
    (scale / RENDER_SCALE_BUCKET).round().max(1.0) * RENDER_SCALE_BUCKET
}

fn auto_invert_metrics(bitmap: &CachedPageBitmap) -> Option<PdfAutoInvertMetrics> {
    let total_pixels = bitmap.rgba.len() / 4;
    if total_pixels == 0 {
        return None;
    }

    let sample_count = total_pixels.min(AUTO_INVERT_MAX_SAMPLED_PIXELS);
    let mut luminance_sum = 0.0;
    let mut white_pixels = 0usize;
    let mut dark_pixels = 0usize;
    for sample_index in 0..sample_count {
        let pixel_index = sample_index * total_pixels / sample_count;
        let offset = pixel_index * 4;
        let alpha = f64::from(bitmap.rgba[offset + 3]);
        let r = f64::from(bitmap.rgba[offset]) + (255.0 - alpha);
        let g = f64::from(bitmap.rgba[offset + 1]) + (255.0 - alpha);
        let b = f64::from(bitmap.rgba[offset + 2]) + (255.0 - alpha);
        let luminance = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
        luminance_sum += luminance;
        if luminance >= AUTO_INVERT_WHITE_LUMINANCE_THRESHOLD {
            white_pixels += 1;
        }
        if luminance <= AUTO_INVERT_DARK_LUMINANCE_THRESHOLD {
            dark_pixels += 1;
        }
    }

    let sample_count = sample_count as f64;
    Some(PdfAutoInvertMetrics {
        average_luminance: luminance_sum / sample_count,
        white_ratio: white_pixels as f64 / sample_count,
        dark_ratio: dark_pixels as f64 / sample_count,
    })
}

fn auto_invert_decision(bitmap: &CachedPageBitmap) -> PdfAutoInvertDecision {
    let Some(metrics) = auto_invert_metrics(bitmap) else {
        return PdfAutoInvertDecision::Inconclusive;
    };
    if metrics.dark_ratio < AUTO_INVERT_MIN_DARK_RATIO {
        return PdfAutoInvertDecision::Inconclusive;
    }
    if metrics.white_ratio >= AUTO_INVERT_MIN_WHITE_RATIO
        && metrics.dark_ratio <= AUTO_INVERT_MAX_DARK_RATIO
        && metrics.average_luminance >= AUTO_INVERT_MIN_AVERAGE_LUMINANCE
    {
        return PdfAutoInvertDecision::Invert;
    }

    PdfAutoInvertDecision::KeepNormal
}

fn invert_pdf_bitmap(bitmap: CachedPageBitmap) -> CachedPageBitmap {
    let mut rgba = bitmap.rgba.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3];
        pixel[0] = alpha.saturating_sub(pixel[0]);
        pixel[1] = alpha.saturating_sub(pixel[1]);
        pixel[2] = alpha.saturating_sub(pixel[2]);
    }

    CachedPageBitmap { width: bitmap.width, height: bitmap.height, rgba: Arc::from(rgba) }
}

pub(crate) fn rasterize_pdf_page(request: &PdfRenderRequest) -> PdfRasterizedPage {
    let bitmap = request.pdf.pages().get(request.page_index).map(|page| {
        let pixmap = hayro::render(
            page,
            &InterpreterSettings::default(),
            &RenderSettings {
                x_scale: f32::from(request.width_px) / page.render_dimensions().0.max(1.0),
                y_scale: f32::from(request.height_px) / page.render_dimensions().1.max(1.0),
                width: Some(request.width_px),
                height: Some(request.height_px),
            },
        );

        CachedPageBitmap {
            width: u32::from(pixmap.width()),
            height: u32::from(pixmap.height()),
            rgba: Arc::from(pixmap.take_u8()),
        }
    });

    PdfRasterizedPage {
        revision: request.revision,
        page_index: request.page_index,
        width_px: request.width_px,
        height_px: request.height_px,
        bitmap,
    }
}

fn normalize_for_search(query: &str) -> String {
    query.chars().flat_map(char::to_lowercase).collect()
}

#[allow(dead_code)]
fn fill_rect(canvas: &mut RgbaImage, rect: PdfViewportRect, color: Rgba<u8>) {
    let x0 = rect.x.floor().max(0.0) as u32;
    let y0 = rect.y.floor().max(0.0) as u32;
    let x1 = (rect.x + rect.width).ceil().max(0.0) as u32;
    let y1 = (rect.y + rect.height).ceil().max(0.0) as u32;
    for y in y0.min(canvas.height())..y1.min(canvas.height()) {
        for x in x0.min(canvas.width())..x1.min(canvas.width()) {
            let pixel = canvas.get_pixel_mut(x, y);
            let alpha = f32::from(color.0[3]) / 255.0;
            pixel.0[0] =
                ((1.0 - alpha) * f32::from(pixel.0[0]) + alpha * f32::from(color.0[0])) as u8;
            pixel.0[1] =
                ((1.0 - alpha) * f32::from(pixel.0[1]) + alpha * f32::from(color.0[1])) as u8;
            pixel.0[2] =
                ((1.0 - alpha) * f32::from(pixel.0[2]) + alpha * f32::from(color.0[2])) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use url::Url;

    fn fixture_pdf_source() -> LoadedPdfSource {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-browser.pdf");
        let bytes = std::fs::read(&path).expect("read fixture pdf");
        let source = Url::from_file_path(&path).expect("file url").to_string();
        LoadedPdfSource {
            title: String::from("agent-browser.pdf"),
            source,
            bytes: Arc::from(bytes),
        }
    }

    fn solid_bitmap(width: u32, height: u32, rgba: [u8; 4]) -> CachedPageBitmap {
        let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..(width as usize * height as usize) {
            bytes.extend_from_slice(&rgba);
        }
        CachedPageBitmap { width, height, rgba: Arc::from(bytes) }
    }

    fn white_page_with_dark_text_bitmap() -> CachedPageBitmap {
        let width = 200u32;
        let height = 200u32;
        let mut bytes = vec![255u8; width as usize * height as usize * 4];
        for pixel in bytes.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        for y in 24..176 {
            if y % 18 >= 5 {
                continue;
            }
            for x in 28..172 {
                let offset = ((y * width + x) * 4) as usize;
                bytes[offset] = 0;
                bytes[offset + 1] = 0;
                bytes[offset + 2] = 0;
            }
        }
        CachedPageBitmap { width, height, rgba: Arc::from(bytes) }
    }

    fn assert_decision(bitmap: CachedPageBitmap, decision: PdfAutoInvertDecision) {
        assert_eq!(auto_invert_decision(&bitmap), decision);
    }

    #[test]
    fn load_pdf_source_reads_fixture_pdf() {
        let loaded = load_pdf_source(&fixture_pdf_source().source).expect("load fixture pdf");
        assert_eq!(loaded.title, "agent-browser.pdf");
        assert!(loaded.bytes.windows(5).any(|window| window == b"%PDF-"));
    }

    #[test]
    fn pdf_title_uses_file_name_for_local_source() {
        let loaded = fixture_pdf_source();
        assert_eq!(pdf_title_for_source(&loaded.source), "agent-browser.pdf");
    }

    #[test]
    fn auto_invert_detects_white_page_with_dark_text() {
        assert_decision(white_page_with_dark_text_bitmap(), PdfAutoInvertDecision::Invert);
    }

    #[test]
    fn auto_invert_rejects_dark_page() {
        assert_decision(solid_bitmap(200, 200, [0, 0, 0, 255]), PdfAutoInvertDecision::KeepNormal);
    }

    #[test]
    fn auto_invert_treats_blank_white_page_as_inconclusive() {
        assert_decision(
            solid_bitmap(200, 200, [255, 255, 255, 255]),
            PdfAutoInvertDecision::Inconclusive,
        );
    }

    #[test]
    fn auto_invert_rejects_mixed_contrast_page() {
        let width = 200u32;
        let height = 200u32;
        let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            let pixel = if y < height / 2 { [255, 255, 255, 255] } else { [0, 0, 0, 255] };
            for _ in 0..width {
                bytes.extend_from_slice(&pixel);
            }
        }
        assert_decision(
            CachedPageBitmap { width, height, rgba: Arc::from(bytes) },
            PdfAutoInvertDecision::KeepNormal,
        );
    }

    #[test]
    fn fit_page_zoom_uses_anchor_page_without_recursive_layout() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");
        let viewport = PhysicalSize::new(1280, 800);
        assert!(state.zoom_fit_page(viewport));
        let zoom = state.zoom_factor(viewport).expect("fit page zoom factor");
        assert!(zoom.is_finite());
        assert!(zoom > 0.0);
        assert_eq!(state.current_page(viewport), 1);
    }

    #[test]
    fn pan_by_preserves_horizontal_centering_when_document_fits_viewport() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");

        let viewport = PhysicalSize::new(1280, 800);
        let before = state.page_draws(viewport);
        assert!(state.pan_by(18.0, 24.0, viewport));
        let after = state.page_draws(viewport);

        assert!((after[0].x - before[0].x).abs() < 0.001);
        assert!((after[0].y - before[0].y - 24.0).abs() < 0.001);
    }

    #[test]
    fn pan_by_clamps_horizontal_offsets_when_document_overflows_viewport() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");

        let viewport = PhysicalSize::new(400, 800);
        assert!(state.zoom_actual(viewport));
        let before = state.page_draws(viewport);
        assert!(before[0].width > f64::from(viewport.width));

        assert!(state.pan_by(10_000.0, 0.0, viewport));
        let right_clamped = state.page_draws(viewport);
        assert!((right_clamped[0].x - 0.0).abs() < 0.001);

        assert!(state.pan_by(-20_000.0, 0.0, viewport));
        let left_clamped = state.page_draws(viewport);
        let expected_left = f64::from(viewport.width) - before[0].width;
        assert!((left_clamped[0].x - expected_left).abs() < 0.001);
    }

    #[test]
    fn auto_invert_locks_after_first_decisive_page() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");

        let inconclusive = solid_bitmap(100, 100, [255, 255, 255, 255]);
        let decisive = white_page_with_dark_text_bitmap();
        state.observe_auto_invert_page(0, &inconclusive);
        assert_eq!(state.auto_invert_decision, None);

        state.observe_auto_invert_page(1, &decisive);
        assert_eq!(state.auto_invert_decision, Some(true));

        state.observe_auto_invert_page(2, &solid_bitmap(100, 100, [0, 0, 0, 255]));
        assert_eq!(state.auto_invert_decision, Some(true));
    }

    #[test]
    fn auto_invert_defaults_to_off_after_three_inconclusive_pages() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");

        let blank = solid_bitmap(100, 100, [255, 255, 255, 255]);
        state.observe_auto_invert_page(0, &blank);
        state.observe_auto_invert_page(1, &blank);
        state.observe_auto_invert_page(2, &blank);
        assert_eq!(state.auto_invert_decision, Some(false));
    }

    #[test]
    fn theme_change_invalidates_cache_when_effective_inversion_changes() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.auto_invert_decision = Some(true);
        state.cached_pages.insert(
            PageRenderKey { page_index: 0, width_px: 10, height_px: 10 },
            solid_bitmap(10, 10, [255, 255, 255, 255]),
        );
        state.cache_order.push_back(PageRenderKey { page_index: 0, width_px: 10, height_px: 10 });
        let old_revision = state.render_revision;

        assert!(state.set_dark_mode_enabled(true));
        assert!(state.cached_pages.is_empty());
        assert!(state.cache_order.is_empty());
        assert_ne!(state.render_revision, old_revision);
    }

    #[test]
    fn toggle_dark_mode_override_cycles_forced_and_auto() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.dark_mode_enabled = true;
        state.auto_invert_decision = Some(true);
        let auto_revision = state.render_revision;

        assert!(state.toggle_dark_mode_override());
        assert_eq!(state.dark_mode_override, PdfDarkModeOverride::Forced(false));
        assert_ne!(state.render_revision, auto_revision);
        let forced_revision = state.render_revision;

        assert!(state.toggle_dark_mode_override());
        assert_eq!(state.dark_mode_override, PdfDarkModeOverride::Auto);
        assert_ne!(state.render_revision, forced_revision);
    }

    #[test]
    fn zoom_by_keeps_the_cursor_anchored() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");

        let viewport = PhysicalSize::new(1280, 800);
        let cursor = PhysicalPosition::new(640.0, 400.0);
        let before = state
            .viewport_point_to_page_point(cursor, viewport)
            .expect("cursor maps into page before zoom");

        assert!(state.zoom_by(1.5, cursor, viewport));

        let after = state
            .viewport_point_to_page_point(cursor, viewport)
            .expect("cursor maps into page after zoom");
        assert_eq!(after.page_index, before.page_index);
        assert!((after.x - before.x).abs() < 0.001);
        assert!((after.y - before.y).abs() < 0.001);
    }

    #[test]
    fn take_visible_render_requests_marks_pages_pending() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");

        let viewport = PhysicalSize::new(1280, 800);
        let requests = state.take_visible_render_requests(viewport);
        assert!(!requests.is_empty());
        assert!(requests.len() <= MAX_PREFETCH_RENDER_REQUESTS_PER_PASS);

        let duplicate_requests = state.take_visible_render_requests(viewport);
        assert!(duplicate_requests.is_empty());
    }

    #[test]
    fn apply_rasterized_page_ignores_stale_revisions() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");

        let viewport = PhysicalSize::new(1280, 800);
        let request = state
            .take_visible_render_requests(viewport)
            .into_iter()
            .next()
            .expect("render request");
        let mut stale = rasterize_pdf_page(&request);
        stale.revision = stale.revision.saturating_sub(1);
        assert!(!state.apply_rasterized_page(stale));
        assert!(state.visible_page_images(viewport).is_empty());

        let current = rasterize_pdf_page(&request);
        assert!(state.apply_rasterized_page(current));
        assert!(!state.visible_page_images(viewport).is_empty());
    }

    #[test]
    fn visible_page_images_reuse_nearest_cached_bucket_while_new_zoom_renders_queue() {
        let mut state = PdfViewState::new(fixture_pdf_source().source.clone());
        state.set_loaded(fixture_pdf_source()).expect("load fixture into pdf view");

        let viewport = PhysicalSize::new(1280, 800);
        let cursor = PhysicalPosition::new(640.0, 400.0);
        let request = state
            .take_visible_render_requests(viewport)
            .into_iter()
            .next()
            .expect("render request");
        assert!(state.apply_rasterized_page(rasterize_pdf_page(&request)));
        assert!(!state.visible_page_images(viewport).is_empty());

        assert!(state.zoom_by(1.5, cursor, viewport));

        let requests_after_zoom = state.take_visible_render_requests(viewport);
        assert!(!requests_after_zoom.is_empty());
        let page_images = state.visible_page_images(viewport);
        assert!(!page_images.is_empty());
        let (_, bitmap, quad) = &page_images[0];
        assert_eq!(quad.uv_top_right.0, bitmap.width as f32);
        assert_eq!(quad.uv_bottom_left.1, bitmap.height as f32);
    }
}
