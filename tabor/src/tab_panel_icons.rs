use crossfont::{BitmapBuffer, Metrics, RasterizedGlyph};

use crate::display::SizeInfo;

pub const TAB_PANEL_CLOSE_CHAR: char = '\u{e000}';
pub const TAB_PANEL_ACTIVITY_FILLED_CHAR: char = '\u{e001}';
pub const TAB_PANEL_ACTIVITY_OUTLINE_CHAR: char = '\u{e002}';
pub const TAB_PANEL_WEB_GLOBE_CHAR: char = '\u{e003}';
pub const FIRST_DYNAMIC_FAVICON_CHAR: u32 = 0xE010;

const ICON_SLOT_SCALE: f32 = 2.0;
const ICON_SIZE_SCALE: f32 = 0.9;
const CLOSE_INSET_RATIO: f32 = 0.18;
const STROKE_RATIO: f32 = 0.14;
const GLOBE_VERTICAL_RATIO: f32 = 0.42;
const GLOBE_HORIZONTAL_RATIO: f32 = 0.38;
const SUPERSAMPLE_GRID: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabPanelIconKind {
    Close,
    ActivityFilled,
    ActivityOutline,
    WebFallback,
}

impl TabPanelIconKind {
    pub const fn character(self) -> char {
        match self {
            Self::Close => TAB_PANEL_CLOSE_CHAR,
            Self::ActivityFilled => TAB_PANEL_ACTIVITY_FILLED_CHAR,
            Self::ActivityOutline => TAB_PANEL_ACTIVITY_OUTLINE_CHAR,
            Self::WebFallback => TAB_PANEL_WEB_GLOBE_CHAR,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TabPanelIconSlotLayout {
    cell_width_px: i32,
    cell_height_px: i32,
    slot_width_px: i32,
    text_offset_y_px: i32,
}

impl TabPanelIconSlotLayout {
    pub(crate) fn new(size_info: &SizeInfo, text_offset_y: f32) -> Self {
        let cell_width_px = size_info.cell_width().round().max(1.0) as i32;
        let cell_height_px = size_info.cell_height().round().max(1.0) as i32;
        let slot_width_px = (cell_width_px as f32 * ICON_SLOT_SCALE).round().max(1.0) as i32;
        let text_offset_y_px = text_offset_y.round() as i32;
        Self { cell_width_px, cell_height_px, slot_width_px, text_offset_y_px }
    }

    pub(crate) fn advance_px(self) -> i32 {
        self.cell_width_px
    }

    pub(crate) fn slot_width_cols(self) -> usize {
        self.slot_width_px
            .max(1)
            .saturating_add(self.cell_width_px.max(1) - 1)
            .checked_div(self.cell_width_px.max(1))
            .unwrap_or(1) as usize
    }

    pub(crate) fn icon_square_px(self) -> i32 {
        self.slot_width_px.min(self.cell_height_px).max(1)
    }

    pub(crate) fn scaled_square_px(self, scale: f32) -> i32 {
        ((self.icon_square_px() as f32) * scale).round().max(1.0) as i32
    }

    pub(crate) fn glyph_position(
        self,
        glyph_width_px: i32,
        glyph_height_px: i32,
        metrics: Metrics,
    ) -> (i32, i32) {
        let offset_x = (self.slot_width_px - glyph_width_px).max(0) / 2;
        let offset_y = (self.cell_height_px - glyph_height_px).max(0) / 2 - self.text_offset_y_px;
        let top = self.cell_height_px - offset_y + metrics.descent.round() as i32;
        (offset_x, top)
    }
}

pub(crate) fn tab_panel_icon_slot_layout(
    size_info: &SizeInfo,
    text_offset_y: f32,
) -> TabPanelIconSlotLayout {
    TabPanelIconSlotLayout::new(size_info, text_offset_y)
}

pub fn rasterized_tab_panel_icon_glyph(
    kind: TabPanelIconKind,
    size_info: &SizeInfo,
    metrics: Metrics,
    text_offset_y: f32,
) -> RasterizedGlyph {
    let layout = tab_panel_icon_slot_layout(size_info, text_offset_y);
    let icon_size_px = layout.scaled_square_px(ICON_SIZE_SCALE) as usize;
    let stroke_px = ((icon_size_px as f32) * STROKE_RATIO).round().max(2.0);
    let mut canvas = MonochromeCanvas::new(icon_size_px, icon_size_px);

    match kind {
        TabPanelIconKind::Close => draw_close_icon(&mut canvas, stroke_px),
        TabPanelIconKind::ActivityFilled => draw_filled_circle_icon(&mut canvas),
        TabPanelIconKind::ActivityOutline => draw_outline_circle_icon(&mut canvas, stroke_px),
        TabPanelIconKind::WebFallback => draw_globe_icon(&mut canvas, stroke_px),
    }

    let (left, top) = layout.glyph_position(icon_size_px as i32, icon_size_px as i32, metrics);

    RasterizedGlyph {
        character: kind.character(),
        width: icon_size_px as i32,
        height: icon_size_px as i32,
        top,
        left,
        advance: (layout.advance_px(), 0),
        buffer: BitmapBuffer::Rgb(canvas.into_rgb()),
    }
}

fn draw_close_icon(canvas: &mut MonochromeCanvas, stroke_px: f32) {
    let size = canvas.width as f32;
    let inset = (size * CLOSE_INSET_RATIO).round().max(1.0);
    let start = inset;
    let end = size - inset;
    let half_stroke = stroke_px / 2.0;

    canvas.paint(|x, y| {
        distance_to_segment(x, y, start, start, end, end) <= half_stroke
            || distance_to_segment(x, y, start, end, end, start) <= half_stroke
    });
}

fn draw_filled_circle_icon(canvas: &mut MonochromeCanvas) {
    let center = canvas.width as f32 / 2.0;
    let radius = center - 1.0;
    canvas.paint(|x, y| distance(x, y, center, center) <= radius);
}

fn draw_outline_circle_icon(canvas: &mut MonochromeCanvas, stroke_px: f32) {
    let center = canvas.width as f32 / 2.0;
    let radius = center - 1.0;
    let inner = (radius - stroke_px).max(0.0);

    canvas.paint(|x, y| {
        let dist = distance(x, y, center, center);
        dist <= radius && dist >= inner
    });
}

fn draw_globe_icon(canvas: &mut MonochromeCanvas, stroke_px: f32) {
    let center = canvas.width as f32 / 2.0;
    let radius = center - 1.0;
    let inner = (radius - stroke_px).max(0.0);
    let vertical_radius = radius * GLOBE_VERTICAL_RATIO;
    let horizontal_radius = radius * GLOBE_HORIZONTAL_RATIO;

    canvas.paint(|x, y| {
        let dist = distance(x, y, center, center);
        if dist <= radius && dist >= inner {
            return true;
        }

        ellipse_ring_contains(
            x,
            y,
            center,
            center,
            vertical_radius,
            radius - stroke_px / 2.0,
            stroke_px,
        ) || ellipse_ring_contains(
            x,
            y,
            center,
            center,
            radius - stroke_px / 2.0,
            horizontal_radius,
            stroke_px,
        )
    });
}

fn ellipse_ring_contains(
    x: f32,
    y: f32,
    center_x: f32,
    center_y: f32,
    radius_x: f32,
    radius_y: f32,
    stroke_px: f32,
) -> bool {
    if radius_x <= 0.0 || radius_y <= 0.0 {
        return false;
    }

    let dx = x - center_x;
    let dy = y - center_y;
    let normalized_radius =
        ((dx * dx) / (radius_x * radius_x) + (dy * dy) / (radius_y * radius_y)).sqrt();
    let tolerance = (stroke_px / 2.0) / radius_x.min(radius_y).max(1.0);
    (normalized_radius - 1.0).abs() <= tolerance
}

fn distance(x: f32, y: f32, center_x: f32, center_y: f32) -> f32 {
    let dx = x - center_x;
    let dy = y - center_y;
    (dx * dx + dy * dy).sqrt()
}

fn distance_to_segment(x: f32, y: f32, start_x: f32, start_y: f32, end_x: f32, end_y: f32) -> f32 {
    let dx = end_x - start_x;
    let dy = end_y - start_y;
    if dx == 0.0 && dy == 0.0 {
        return distance(x, y, start_x, start_y);
    }

    let t = (((x - start_x) * dx + (y - start_y) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let nearest_x = start_x + t * dx;
    let nearest_y = start_y + t * dy;
    distance(x, y, nearest_x, nearest_y)
}

struct MonochromeCanvas {
    width: usize,
    height: usize,
    alpha: Vec<u8>,
}

impl MonochromeCanvas {
    fn new(width: usize, height: usize) -> Self {
        Self { width, height, alpha: vec![0; width * height] }
    }

    fn paint<F>(&mut self, contains: F)
    where
        F: Fn(f32, f32) -> bool,
    {
        for y in 0..self.height {
            for x in 0..self.width {
                let coverage = sample_coverage(x, y, &contains);
                if coverage <= 0.0 {
                    continue;
                }

                let index = y * self.width + x;
                let alpha = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
                self.alpha[index] = self.alpha[index].max(alpha);
            }
        }
    }

    fn into_rgb(self) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(self.alpha.len() * 3);
        for alpha in self.alpha {
            rgb.push(alpha);
            rgb.push(alpha);
            rgb.push(alpha);
        }
        rgb
    }
}

fn sample_coverage<F>(x: usize, y: usize, contains: &F) -> f32
where
    F: Fn(f32, f32) -> bool,
{
    let mut covered = 0usize;
    let total = SUPERSAMPLE_GRID * SUPERSAMPLE_GRID;
    for sub_y in 0..SUPERSAMPLE_GRID {
        for sub_x in 0..SUPERSAMPLE_GRID {
            let sample_x = x as f32 + (sub_x as f32 + 0.5) / SUPERSAMPLE_GRID as f32;
            let sample_y = y as f32 + (sub_y as f32 + 0.5) / SUPERSAMPLE_GRID as f32;
            if contains(sample_x, sample_y) {
                covered += 1;
            }
        }
    }
    covered as f32 / total as f32
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
    fn reserved_private_use_range_leaves_room_for_static_icons() {
        assert_eq!(TAB_PANEL_CLOSE_CHAR as u32, 0xE000);
        assert_eq!(TAB_PANEL_WEB_GLOBE_CHAR as u32, 0xE003);
        assert_eq!(FIRST_DYNAMIC_FAVICON_CHAR, 0xE010);
    }

    #[test]
    fn activity_icon_is_centered_within_generated_glyph() {
        let size_info = SizeInfo::new(200.0, 100.0, 10.0, 24.0, 0.0, 0.0, 0.0, false);
        let glyph = rasterized_tab_panel_icon_glyph(
            TabPanelIconKind::ActivityFilled,
            &size_info,
            TEST_METRICS,
            -2.0,
        );
        let width = glyph.width as usize;
        let height = glyph.height as usize;
        let BitmapBuffer::Rgb(buffer) = &glyph.buffer else {
            panic!("expected monochrome icon buffer");
        };
        let bounds =
            nonzero_bounds(buffer, width, height).expect("activity icon should draw pixels");
        let top_padding = bounds.1;
        let bottom_padding = height - 1 - bounds.3;
        let left_padding = bounds.0;
        let right_padding = width - 1 - bounds.2;
        assert!((top_padding as isize - bottom_padding as isize).abs() <= 1);
        assert!((left_padding as isize - right_padding as isize).abs() <= 1);
    }

    #[test]
    fn close_icon_draws_both_diagonals() {
        let size_info = SizeInfo::new(200.0, 100.0, 10.0, 24.0, 0.0, 0.0, 0.0, false);
        let glyph = rasterized_tab_panel_icon_glyph(
            TabPanelIconKind::Close,
            &size_info,
            TEST_METRICS,
            -2.0,
        );
        let width = glyph.width as usize;
        let height = glyph.height as usize;
        let BitmapBuffer::Rgb(buffer) = &glyph.buffer else {
            panic!("expected monochrome icon buffer");
        };
        let quadrants = quadrant_hits(buffer, width, height);
        assert!(quadrants.iter().all(|count| *count > 0), "close icon should touch all quadrants");
    }

    fn nonzero_bounds(
        buffer: &[u8],
        width: usize,
        height: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut found = false;

        for y in 0..height {
            for x in 0..width {
                let index = (y * width + x) * 3;
                if buffer[index] == 0 && buffer[index + 1] == 0 && buffer[index + 2] == 0 {
                    continue;
                }
                found = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }

        found.then_some((min_x, min_y, max_x, max_y))
    }

    fn quadrant_hits(buffer: &[u8], width: usize, height: usize) -> [usize; 4] {
        let mut hits = [0usize; 4];
        let mid_x = width / 2;
        let mid_y = height / 2;
        for y in 0..height {
            for x in 0..width {
                let index = (y * width + x) * 3;
                if buffer[index] == 0 && buffer[index + 1] == 0 && buffer[index + 2] == 0 {
                    continue;
                }
                let quadrant = match (x < mid_x, y < mid_y) {
                    (true, true) => 0,
                    (false, true) => 1,
                    (true, false) => 2,
                    (false, false) => 3,
                };
                hits[quadrant] += 1;
            }
        }
        hits
    }
}
