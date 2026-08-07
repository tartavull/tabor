#[cfg(any(unix, test))]
use std::cmp::max;

use serde::{Deserialize, Serialize};

#[cfg(any(unix, test))]
use tabor_terminal::grid::Dimensions;

#[cfg(any(unix, test))]
use crate::config::browser::MultiColumnBrowserConfig;
#[cfg(any(unix, test))]
use crate::display::SizeInfo;
#[cfg(any(unix, test))]
use crate::display::auxiliary_regions::EarAwareTopRegions;

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

#[cfg(any(target_os = "macos", test))]
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

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BrowserViewportColumn {
    rect: BrowserViewportRect,
    logical_y: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserViewportLayout {
    mode: BrowserViewMode,
    viewport: BrowserViewportRect,
    target_width_px: usize,
    logical_width: usize,
    logical_height: usize,
    columns: Vec<BrowserViewportColumn>,
}

#[cfg(any(unix, test))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct BrowserColumnLayoutParams {
    target_width_px: usize,
    logical_width: usize,
    column_count: usize,
    left_padding_px: usize,
}

#[cfg(any(unix, test))]
impl BrowserViewportLayout {
    #[cfg(any(unix, test))]
    pub fn normal(viewport: BrowserViewportRect, target_width_px: usize) -> Self {
        Self {
            mode: BrowserViewMode::Normal,
            viewport,
            target_width_px: max(target_width_px, 1),
            logical_width: viewport.width,
            logical_height: viewport.height,
            columns: vec![BrowserViewportColumn { rect: viewport, logical_y: 0 }],
        }
    }

    pub fn new(
        size_info: &SizeInfo,
        scale_factor: f64,
        mode: BrowserViewMode,
        config: &MultiColumnBrowserConfig,
        exact_column_count: Option<usize>,
        ear_aware_regions: Option<EarAwareTopRegions>,
    ) -> Self {
        let target_width_px = max(config.target_width_px, 1);
        let viewport = Self::viewport_from_size_info(size_info, scale_factor);

        if mode != BrowserViewMode::MultiColumn {
            return Self::normal(viewport, target_width_px);
        }

        if let Some(exact_column_count) = exact_column_count {
            let column_count = exact_column_count.max(1).min(max(viewport.width, 1));
            let logical_width = max(viewport.width / column_count, 1);
            let left_padding_px =
                viewport.width.saturating_sub(logical_width.saturating_mul(column_count));
            return Self::with_columns(
                mode,
                viewport,
                scale_factor,
                BrowserColumnLayoutParams {
                    target_width_px: logical_width,
                    logical_width,
                    column_count,
                    left_padding_px,
                },
                ear_aware_regions,
            );
        }

        let column_count = max(viewport.width / target_width_px, 1);
        if column_count == 1 {
            return Self {
                mode,
                viewport,
                target_width_px,
                logical_width: viewport.width,
                logical_height: viewport.height,
                columns: vec![BrowserViewportColumn { rect: viewport, logical_y: 0 }],
            };
        }

        let logical_width = max(viewport.width / column_count, 1);
        let left_padding_px =
            viewport.width.saturating_sub(logical_width.saturating_mul(column_count));

        Self::with_columns(
            mode,
            viewport,
            scale_factor,
            BrowserColumnLayoutParams {
                target_width_px,
                logical_width,
                column_count,
                left_padding_px,
            },
            ear_aware_regions,
        )
    }

    pub fn mode(&self) -> BrowserViewMode {
        self.mode
    }

    pub fn viewport(&self) -> BrowserViewportRect {
        self.viewport
    }

    #[cfg(unix)]
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
        self.columns.len()
    }

    pub(crate) fn column(&self, column_index: usize) -> Option<BrowserViewportColumn> {
        self.columns.get(column_index).copied()
    }

    pub fn column_rect(&self, column_index: usize) -> Option<BrowserViewportRect> {
        self.column(column_index).map(|column| column.rect)
    }

    pub fn column_logical_y(&self, column_index: usize) -> Option<usize> {
        self.column(column_index).map(|column| column.logical_y)
    }

    #[cfg(any(target_os = "macos", test))]
    pub fn visual_point_for_logical(&self, x: usize, y: usize) -> Option<(usize, usize)> {
        if self.viewport.height == 0 || x >= self.logical_width || y >= self.logical_height {
            return None;
        }

        let column = self.columns.iter().find(|column| {
            (column.logical_y..column.logical_y + column.rect.height).contains(&y)
        })?;
        Some((column.rect.x.saturating_add(x), column.rect.y.saturating_add(y - column.logical_y)))
    }

    #[cfg(any(target_os = "macos", test))]
    pub fn logical_point_for_visual(&self, x: usize, y: usize) -> Option<(usize, usize)> {
        let column = self.columns.iter().find(|column| column.rect.contains(x, y))?;
        Some((x - column.rect.x, column.logical_y + (y - column.rect.y)))
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

    fn with_columns(
        mode: BrowserViewMode,
        viewport: BrowserViewportRect,
        scale_factor: f64,
        params: BrowserColumnLayoutParams,
        ear_aware_regions: Option<EarAwareTopRegions>,
    ) -> Self {
        let scale_factor = scale_factor.max(f64::MIN_POSITIVE);
        let reclaim_top_px = ear_aware_regions
            .map_or(0, |regions| ((regions.reclaim_top_px as f64) / scale_factor) as usize);
        let mut logical_y = 0;
        let mut columns = Vec::with_capacity(params.column_count);

        for column_index in 0..params.column_count {
            let x = viewport.x + params.left_padding_px + column_index * params.logical_width;
            let eligible_for_ear = ear_aware_regions.is_some_and(|regions| {
                regions.span_fits_auxiliary_region(
                    ((x as f64) * scale_factor) as usize,
                    ((params.logical_width as f64) * scale_factor) as usize,
                )
            });
            let y = if eligible_for_ear {
                viewport.y.saturating_sub(reclaim_top_px)
            } else {
                viewport.y
            };
            let height = viewport.height + usize::from(eligible_for_ear) * reclaim_top_px;
            columns.push(BrowserViewportColumn {
                rect: BrowserViewportRect { x, y, width: params.logical_width, height },
                logical_y,
            });
            logical_y = logical_y.saturating_add(height);
        }

        Self {
            mode,
            viewport,
            target_width_px: params.target_width_px,
            logical_width: params.logical_width,
            logical_height: logical_y,
            columns,
        }
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
            None,
            None,
        )
    }

    #[test]
    fn normal_mode_uses_single_full_width_viewport() {
        let layout = BrowserViewportLayout::new(
            &size(1200, 600),
            1.0,
            BrowserViewMode::Normal,
            &MultiColumnBrowserConfig::default(),
            None,
            None,
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
            None,
            None,
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
            None,
            None,
        );
        let logical = (17, 745);
        let visual = layout.visual_point_for_logical(logical.0, logical.1).unwrap();

        assert_eq!(visual, (992, 145));
        assert_eq!(layout.logical_point_for_visual(visual.0, visual.1), Some(logical));
    }

    #[test]
    fn exact_column_count_preserves_requested_column_count() {
        let layout = BrowserViewportLayout::new(
            &size(1103, 600),
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
            Some(3),
            None,
        );

        assert_eq!(layout.column_count(), 3);
        assert_eq!(layout.logical_width(), 367);
        assert_eq!(
            layout.column_rect(2),
            Some(BrowserViewportRect { x: 736, y: 0, width: 367, height: 600 })
        );
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
            None,
            None,
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
            None,
            None,
        );

        assert_eq!(
            layout.viewport(),
            BrowserViewportRect { x: 20, y: 15, width: 1770, height: 560 }
        );
        assert_eq!(layout.logical_width(), 1770);
        assert_eq!(layout.logical_height(), 560);
    }

    #[test]
    fn viewport_matches_webview_frame_basis_with_asymmetric_vertical_padding() {
        let mut size_info =
            SizeInfo::new_with_vertical_padding(3600., 1200., 10., 20., 40., 20., 60., 20., false);
        size_info.reserve_lines(1);

        let layout = BrowserViewportLayout::new(
            &size_info,
            2.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
            None,
            None,
        );

        assert_eq!(
            layout.viewport(),
            BrowserViewportRect { x: 20, y: 30, width: 1770, height: 550 }
        );
        assert_eq!(layout.logical_width(), 1770);
        assert_eq!(layout.logical_height(), 550);
    }

    #[test]
    fn ear_aware_columns_reclaim_top_band_only_when_fully_inside_ears() {
        let size_info =
            SizeInfo::new_with_vertical_padding(900., 680., 1., 1., 0., 0., 40., 0., false);
        let layout = BrowserViewportLayout::new(
            &size_info,
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
            Some(3),
            Some(EarAwareTopRegions {
                reclaim_top_px: 40,
                left: Some(crate::display::auxiliary_regions::AuxiliaryTopRegion {
                    x: 0,
                    width: 300,
                }),
                right: Some(crate::display::auxiliary_regions::AuxiliaryTopRegion {
                    x: 600,
                    width: 300,
                }),
            }),
        );

        assert_eq!(layout.column_count(), 3);
        assert_eq!(
            layout.column_rect(0),
            Some(BrowserViewportRect { x: 0, y: 0, width: 300, height: 680 })
        );
        assert_eq!(
            layout.column_rect(1),
            Some(BrowserViewportRect { x: 300, y: 40, width: 300, height: 640 })
        );
        assert_eq!(
            layout.column_rect(2),
            Some(BrowserViewportRect { x: 600, y: 0, width: 300, height: 680 })
        );
        assert_eq!(layout.column_logical_y(0), Some(0));
        assert_eq!(layout.column_logical_y(1), Some(680));
        assert_eq!(layout.column_logical_y(2), Some(1320));
        assert_eq!(layout.logical_height(), 2000);
        assert_eq!(layout.visual_point_for_logical(17, 700), Some((317, 60)));
        assert_eq!(layout.logical_point_for_visual(317, 20), None);
        assert_eq!(layout.logical_point_for_visual(317, 60), Some((17, 700)));
        assert_eq!(layout.logical_point_for_visual(617, 20), Some((17, 1340)));
    }

    #[test]
    fn ear_aware_columns_handle_physical_ear_regions_on_scaled_displays() {
        let size_info =
            SizeInfo::new_with_vertical_padding(1800., 1360., 1., 1., 0., 0., 80., 0., false);
        let layout = BrowserViewportLayout::new(
            &size_info,
            2.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
            Some(3),
            Some(EarAwareTopRegions {
                reclaim_top_px: 80,
                left: Some(crate::display::auxiliary_regions::AuxiliaryTopRegion {
                    x: 0,
                    width: 600,
                }),
                right: Some(crate::display::auxiliary_regions::AuxiliaryTopRegion {
                    x: 1200,
                    width: 600,
                }),
            }),
        );

        assert_eq!(layout.column_count(), 3);
        assert_eq!(
            layout.column_rect(0),
            Some(BrowserViewportRect { x: 0, y: 0, width: 300, height: 680 })
        );
        assert_eq!(
            layout.column_rect(1),
            Some(BrowserViewportRect { x: 300, y: 40, width: 300, height: 640 })
        );
        assert_eq!(
            layout.column_rect(2),
            Some(BrowserViewportRect { x: 600, y: 0, width: 300, height: 680 })
        );
        assert_eq!(layout.logical_height(), 2000);
    }
}
