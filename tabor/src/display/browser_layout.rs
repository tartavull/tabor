use std::cmp::max;

use serde::{Deserialize, Serialize};

use tabor_terminal::grid::Dimensions;

use crate::config::browser::MultiColumnBrowserConfig;
use crate::display::SizeInfo;

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserViewMode {
    #[default]
    Normal,
    MultiColumn,
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserViewportRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
impl BrowserViewportRect {
    fn right(&self) -> usize {
        self.x.saturating_add(self.width)
    }

    fn bottom(&self) -> usize {
        self.y.saturating_add(self.height)
    }

    fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct BrowserViewportLayout {
    mode: BrowserViewMode,
    viewport: BrowserViewportRect,
    target_width_px: usize,
    logical_width: usize,
    logical_height: usize,
    column_count: usize,
    left_padding_px: usize,
}

impl BrowserViewportLayout {
    pub fn normal(viewport: BrowserViewportRect, target_width_px: usize) -> Self {
        Self {
            mode: BrowserViewMode::Normal,
            viewport,
            target_width_px: max(target_width_px, 1),
            logical_width: viewport.width,
            logical_height: viewport.height,
            column_count: 1,
            left_padding_px: 0,
        }
    }

    pub fn new(
        size_info: &SizeInfo,
        scale_factor: f64,
        mode: BrowserViewMode,
        config: &MultiColumnBrowserConfig,
    ) -> Self {
        let target_width_px = max(config.target_width_px, 1);
        let viewport = Self::viewport_from_size_info(size_info, scale_factor);

        if mode != BrowserViewMode::MultiColumn {
            return Self::normal(viewport, target_width_px);
        }

        let column_count = max(viewport.width / target_width_px, 1);
        if column_count == 1 {
            return Self {
                mode,
                viewport,
                target_width_px,
                logical_width: viewport.width,
                logical_height: viewport.height,
                column_count,
                left_padding_px: 0,
            };
        }

        let logical_width = max(viewport.width / column_count, 1);
        let left_padding_px =
            viewport.width.saturating_sub(logical_width.saturating_mul(column_count));

        Self {
            mode,
            viewport,
            target_width_px,
            logical_width,
            logical_height: viewport.height.saturating_mul(column_count),
            column_count,
            left_padding_px,
        }
    }

    pub fn mode(&self) -> BrowserViewMode {
        self.mode
    }

    pub fn viewport(&self) -> BrowserViewportRect {
        self.viewport
    }

    pub fn target_width_px(&self) -> usize {
        self.target_width_px
    }

    pub fn logical_width(&self) -> usize {
        self.logical_width
    }

    pub fn logical_height(&self) -> usize {
        self.logical_height
    }

    pub fn column_count(&self) -> usize {
        self.column_count
    }

    pub fn column_rect(&self, column_index: usize) -> Option<BrowserViewportRect> {
        if column_index >= self.column_count {
            return None;
        }

        Some(BrowserViewportRect {
            x: self.column_start_x(column_index),
            y: self.viewport.y,
            width: self.logical_width,
            height: self.viewport.height,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn visual_point_for_logical(&self, x: usize, y: usize) -> Option<(usize, usize)> {
        if self.viewport.height == 0 || x >= self.logical_width || y >= self.logical_height {
            return None;
        }

        let column_index = y / self.viewport.height;
        if column_index >= self.column_count {
            return None;
        }

        Some((
            self.column_start_x(column_index).saturating_add(x),
            self.viewport.y.saturating_add(y % self.viewport.height),
        ))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn logical_point_for_visual(&self, x: usize, y: usize) -> Option<(usize, usize)> {
        if self.viewport.height == 0 || !self.viewport.contains(x, y) {
            return None;
        }

        let y = y - self.viewport.y;
        let (column_index, column_x) = self.column_x_at_visual(x)?;
        Some((column_x, column_index * self.viewport.height + y))
    }

    fn viewport_from_size_info(size_info: &SizeInfo, scale_factor: f64) -> BrowserViewportRect {
        let scale_factor = scale_factor.max(f64::MIN_POSITIVE);
        let available_width =
            (size_info.width() - size_info.padding_x() - size_info.padding_right()).max(0.0);
        let available_height = (size_info.cell_height() * size_info.screen_lines() as f32).max(0.0);

        BrowserViewportRect {
            x: (f64::from(size_info.padding_x()) / scale_factor) as usize,
            y: (f64::from(size_info.padding_y()) / scale_factor) as usize,
            width: (f64::from(available_width) / scale_factor) as usize,
            height: (f64::from(available_height) / scale_factor) as usize,
        }
    }

    fn column_start_x(&self, column_index: usize) -> usize {
        self.viewport.x + self.left_padding_px + column_index * self.logical_width
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn column_x_at_visual(&self, x: usize) -> Option<(usize, usize)> {
        for column_index in 0..self.column_count {
            let start = self.column_start_x(column_index);
            let end = start.saturating_add(self.logical_width);
            if (start..end).contains(&x) {
                return Some((column_index, x - start));
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: usize, height: usize) -> SizeInfo {
        SizeInfo::new(width as f32, height as f32, 1., 1., 0., 0., 0., false)
    }

    fn multi_column_layout(
        width: usize,
        height: usize,
        target_width_px: usize,
    ) -> BrowserViewportLayout {
        BrowserViewportLayout::new(
            &size(width, height),
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig { target_width_px },
        )
    }

    #[test]
    fn normal_mode_uses_single_full_width_viewport() {
        let layout = BrowserViewportLayout::new(
            &size(1200, 600),
            1.0,
            BrowserViewMode::Normal,
            &MultiColumnBrowserConfig::default(),
        );

        assert_eq!(layout.mode(), BrowserViewMode::Normal);
        assert_eq!(layout.viewport(), BrowserViewportRect { x: 0, y: 0, width: 1200, height: 600 });
        assert_eq!(layout.logical_width(), 1200);
        assert_eq!(layout.logical_height(), 600);
        assert_eq!(layout.column_count(), 1);
    }

    #[test]
    fn multi_column_uses_contiguous_columns_with_no_gap() {
        let layout = BrowserViewportLayout::new(
            &size(1950, 600),
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
        );

        assert_eq!(layout.column_count(), 2);
        assert_eq!(layout.logical_width(), 975);
        assert_eq!(
            layout.column_rect(0),
            Some(BrowserViewportRect { x: 0, y: 0, width: 975, height: 600 })
        );
        assert_eq!(
            layout.column_rect(1),
            Some(BrowserViewportRect { x: 975, y: 0, width: 975, height: 600 })
        );
    }

    #[test]
    fn multi_column_roundtrips_logical_and_visual_points() {
        let layout = BrowserViewportLayout::new(
            &size(1950, 600),
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
        );
        let logical = (17, 745);
        let visual = layout.visual_point_for_logical(logical.0, logical.1).unwrap();

        assert_eq!(visual, (992, 145));
        assert_eq!(layout.logical_point_for_visual(visual.0, visual.1), Some(logical));
    }

    #[test]
    fn multi_column_remainder_becomes_left_padding() {
        let layout_1100 = multi_column_layout(1100, 600, 400);
        assert_eq!(layout_1100.column_count(), 2);
        assert_eq!(layout_1100.logical_width(), 550);
        assert_eq!(
            layout_1100.column_rect(0),
            Some(BrowserViewportRect { x: 0, y: 0, width: 550, height: 600 })
        );
        assert_eq!(
            layout_1100.column_rect(1),
            Some(BrowserViewportRect { x: 550, y: 0, width: 550, height: 600 })
        );

        let layout_1101 = multi_column_layout(1101, 600, 400);
        assert_eq!(layout_1101.column_count(), 2);
        assert_eq!(layout_1101.logical_width(), 550);
        assert_eq!(
            layout_1101.column_rect(0),
            Some(BrowserViewportRect { x: 1, y: 0, width: 550, height: 600 })
        );
        assert_eq!(
            layout_1101.column_rect(1),
            Some(BrowserViewportRect { x: 551, y: 0, width: 550, height: 600 })
        );

        let layout_1103 = multi_column_layout(1103, 600, 400);
        assert_eq!(layout_1103.column_count(), 2);
        assert_eq!(layout_1103.logical_width(), 551);
        assert_eq!(
            layout_1103.column_rect(0),
            Some(BrowserViewportRect { x: 1, y: 0, width: 551, height: 600 })
        );
        assert_eq!(
            layout_1103.column_rect(1),
            Some(BrowserViewportRect { x: 552, y: 0, width: 551, height: 600 })
        );
    }

    #[test]
    fn multi_column_single_column_uses_full_viewport_width() {
        let layout = BrowserViewportLayout::new(
            &size(950, 600),
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
        );

        assert_eq!(layout.mode(), BrowserViewMode::MultiColumn);
        assert_eq!(layout.column_count(), 1);
        assert_eq!(layout.logical_width(), 950);
        assert_eq!(
            layout.column_rect(0),
            Some(BrowserViewportRect { x: 0, y: 0, width: 950, height: 600 })
        );
    }

    #[test]
    fn viewport_matches_webview_frame_basis() {
        let mut size_info = SizeInfo::new(3600., 1200., 10., 20., 40., 20., 30., false);
        size_info.reserve_lines(1);

        let layout = BrowserViewportLayout::new(
            &size_info,
            2.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
        );

        assert_eq!(
            layout.viewport(),
            BrowserViewportRect { x: 20, y: 15, width: 1770, height: 560 }
        );
        assert_eq!(layout.logical_width(), 1770);
        assert_eq!(layout.logical_height(), 560);
    }
}
