use std::cmp::{max, min};

use serde::{Deserialize, Serialize};

use tabor_terminal::event::WindowSize;
use tabor_terminal::grid::Dimensions;
use tabor_terminal::index::{Column, Point};
use tabor_terminal::term::{self, Term};

use crate::config::terminal::{MultiColumnOrder, MultiColumnTerminalConfig};
use crate::display::SizeInfo;
use crate::display::auxiliary_regions::EarAwareTopRegions;

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalViewMode {
    #[default]
    Normal,
    MultiColumn,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TerminalLogicalSize {
    columns: usize,
    screen_lines: usize,
    cell_width: u16,
    cell_height: u16,
}

impl TerminalLogicalSize {
    pub fn new(columns: usize, screen_lines: usize, cell_width: u16, cell_height: u16) -> Self {
        Self { columns, screen_lines, cell_width, cell_height }
    }
}

impl Dimensions for TerminalLogicalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

impl From<TerminalLogicalSize> for WindowSize {
    fn from(size: TerminalLogicalSize) -> Self {
        Self {
            num_cols: size.columns as u16,
            num_lines: size.screen_lines as u16,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TerminalViewportStripGeometry {
    pub start_column: usize,
    pub column_count: usize,
    pub y_offset_px: usize,
    pub visual_line_count: usize,
    pub logical_start_line: usize,
    pub logical_line_count: usize,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TerminalViewportLayout {
    mode: TerminalViewMode,
    order: MultiColumnOrder,
    content_line_offset: usize,
    target_columns: usize,
    visual_columns: usize,
    visual_lines: usize,
    logical_columns: usize,
    logical_lines: usize,
    strip_count: usize,
    gutter_columns: usize,
    extra_gutter_columns: usize,
    left_padding_columns: usize,
    right_padding_columns: usize,
    padding_x_px: usize,
    cell_width_px: usize,
    ear_aware_regions: Option<EarAwareTopRegions>,
    ear_extra_lines: usize,
}

impl TerminalViewportLayout {
    pub fn normal(size_info: SizeInfo) -> Self {
        Self {
            mode: TerminalViewMode::Normal,
            order: MultiColumnOrder::StartLeft,
            content_line_offset: 0,
            target_columns: size_info.columns(),
            visual_columns: size_info.columns(),
            visual_lines: size_info.screen_lines(),
            logical_columns: size_info.columns(),
            logical_lines: size_info.screen_lines(),
            strip_count: 1,
            gutter_columns: 0,
            extra_gutter_columns: 0,
            left_padding_columns: 0,
            right_padding_columns: 0,
            padding_x_px: size_info.padding_x() as usize,
            cell_width_px: size_info.cell_width() as usize,
            ear_aware_regions: None,
            ear_extra_lines: 0,
        }
    }

    pub fn new(
        size_info: &SizeInfo,
        mode: TerminalViewMode,
        config: &MultiColumnTerminalConfig,
        exact_strip_count: Option<usize>,
        ear_aware_regions: Option<EarAwareTopRegions>,
    ) -> Self {
        let visual_columns = size_info.columns();
        let visual_lines = size_info.screen_lines();
        let ear_extra_lines = ear_aware_regions.map_or(0, |regions| {
            regions.reclaim_top_px / (size_info.cell_height() as usize).max(1)
        });

        if mode != TerminalViewMode::MultiColumn {
            return Self::normal(*size_info);
        }

        if let Some(exact_strip_count) = exact_strip_count {
            let strip_count = exact_strip_count.max(1).min(max(visual_columns, 1));
            let logical_columns = max(visual_columns / strip_count, 1);
            let used_columns = strip_count * logical_columns;
            let leftover_columns = visual_columns.saturating_sub(used_columns);
            let gutter_slots = strip_count.saturating_sub(1);
            let gutter_columns = leftover_columns.checked_div(gutter_slots).unwrap_or(0);
            let extra_gutter_columns = leftover_columns.checked_rem(gutter_slots).unwrap_or(0);
            let mut layout = Self {
                mode,
                order: config.order,
                content_line_offset: 0,
                target_columns: logical_columns,
                visual_columns,
                visual_lines,
                logical_columns,
                logical_lines: 0,
                strip_count,
                gutter_columns,
                extra_gutter_columns,
                left_padding_columns: 0,
                right_padding_columns: 0,
                padding_x_px: size_info.padding_x() as usize,
                cell_width_px: size_info.cell_width() as usize,
                ear_aware_regions,
                ear_extra_lines,
            };
            layout.logical_lines = layout.total_logical_capacity();
            return layout;
        }

        let target_columns = max(config.target_columns, 1);
        let logical_columns = min(target_columns, visual_columns);
        let strip_count = max(visual_columns / logical_columns, 1);
        let used_columns = strip_count * logical_columns;
        let leftover_columns = visual_columns.saturating_sub(used_columns);
        let gutter_slots = strip_count.saturating_sub(1);
        let gutter_columns = leftover_columns.checked_div(gutter_slots).unwrap_or(0);
        let extra_gutter_columns = leftover_columns.checked_rem(gutter_slots).unwrap_or(0);
        let mut layout = Self {
            mode,
            order: config.order,
            content_line_offset: 0,
            target_columns,
            visual_columns,
            visual_lines,
            logical_columns,
            logical_lines: 0,
            strip_count,
            gutter_columns,
            extra_gutter_columns,
            left_padding_columns: 0,
            right_padding_columns: 0,
            padding_x_px: size_info.padding_x() as usize,
            cell_width_px: size_info.cell_width() as usize,
            ear_aware_regions,
            ear_extra_lines,
        };
        layout.logical_lines = layout.total_logical_capacity();
        layout
    }

    pub fn strip_count(&self) -> usize {
        self.strip_count
    }

    pub fn target_columns(&self) -> usize {
        self.target_columns
    }

    pub fn logical_size(&self, size_info: &SizeInfo) -> TerminalLogicalSize {
        TerminalLogicalSize::new(
            self.logical_columns,
            self.logical_lines,
            size_info.cell_width() as u16,
            size_info.cell_height() as u16,
        )
    }

    pub fn visual_damage_lines(&self) -> usize {
        (0..self.strip_count)
            .map(|strip_index| self.visual_strip_line_capacity(strip_index))
            .max()
            .unwrap_or(self.visual_lines)
    }

    pub fn is_multi_column(&self) -> bool {
        self.mode == TerminalViewMode::MultiColumn
    }

    pub fn with_terminal_content<T>(mut self, term: &Term<T>) -> Self {
        self.logical_columns = min(self.logical_columns, term.columns());
        self.logical_lines = min(self.logical_lines, term.screen_lines());
        self.target_columns = min(self.target_columns, self.logical_columns);
        self.content_line_offset = 0;
        self
    }

    pub fn strip_geometries(&self) -> Vec<TerminalViewportStripGeometry> {
        let mut strips = Vec::with_capacity(self.strip_count);

        for visual_strip_index in 0..self.strip_count {
            let logical_strip_index = self.logical_strip_index_for_visual(visual_strip_index);
            let logical_start_line = self.logical_strip_start_line(logical_strip_index);
            let logical_line_count = self
                .logical_lines
                .saturating_sub(logical_start_line)
                .min(self.logical_strip_line_capacity(logical_strip_index));

            strips.push(TerminalViewportStripGeometry {
                start_column: self.strip_start_column(visual_strip_index),
                column_count: self.logical_columns,
                y_offset_px: self.visual_strip_y_offset_px(visual_strip_index),
                visual_line_count: self.visual_strip_line_capacity(visual_strip_index),
                logical_start_line,
                logical_line_count,
            });
        }

        strips
    }

    pub fn y_offset_px_for_visual_column(&self, visual_column: usize) -> usize {
        self.strip_column_at_visual(visual_column)
            .map_or(0, |(visual_strip_index, _)| self.visual_strip_y_offset_px(visual_strip_index))
    }

    pub fn visual_point_for_logical_viewport(&self, point: Point<usize>) -> Option<Point<usize>> {
        let line = point.line.checked_add(self.content_line_offset)?;
        if line >= self.logical_lines || point.column.0 >= self.logical_columns {
            return None;
        }

        let (logical_strip_index, line) = self.logical_strip_for_line(line)?;
        let visual_strip_index = self.visual_strip_index_for_logical(logical_strip_index);
        let column = self.strip_start_column(visual_strip_index) + point.column.0;

        Some(Point::new(line, Column(column)))
    }

    pub fn logical_viewport_point_for_visual(&self, point: Point<usize>) -> Option<Point<usize>> {
        let (visual_strip_index, strip_column) = self.strip_column_at_visual(point.column.0)?;
        if point.line >= self.visual_strip_line_capacity(visual_strip_index) {
            return None;
        }

        let logical_strip_index = self.logical_strip_index_for_visual(visual_strip_index);
        let line = self.logical_strip_start_line(logical_strip_index).saturating_add(point.line);
        let line = line.checked_sub(self.content_line_offset)?;
        if line >= self.logical_lines || strip_column >= self.logical_columns {
            return None;
        }

        Some(Point::new(line, Column(strip_column)))
    }

    pub fn logical_terminal_point_for_visual(
        &self,
        point: Point<usize>,
        display_offset: usize,
    ) -> Option<Point> {
        self.logical_viewport_point_for_visual(point)
            .map(|point| term::viewport_to_point(display_offset, point))
    }

    pub fn logical_terminal_point_from_pixels_clamped(
        &self,
        size_info: &SizeInfo,
        x: usize,
        y: usize,
        display_offset: usize,
    ) -> Point {
        let visual_point = self.visual_point_from_pixels_clamped(size_info, x, y);
        self.logical_terminal_point_for_visual(visual_point, display_offset).unwrap_or_else(|| {
            let (visual_strip_index, strip_column) = self
                .strip_column_at_visual(visual_point.column.0)
                .expect("clamped visual point should always land inside a strip cell, not padding");
            let logical_strip_index = self.logical_strip_index_for_visual(visual_strip_index);
            let display_line = self
                .logical_strip_start_line(logical_strip_index)
                .saturating_add(visual_point.line);
            let clamped_display_line = display_line.clamp(
                self.content_line_offset,
                self.content_line_offset + self.visible_logical_lines().saturating_sub(1),
            );
            let logical_line = clamped_display_line.saturating_sub(self.content_line_offset);
            term::viewport_to_point(display_offset, Point::new(logical_line, Column(strip_column)))
        })
    }

    pub fn contains_point(&self, size_info: &SizeInfo, x: usize, y: usize) -> bool {
        let Some(visual_point) = self.visual_point_from_pixels(size_info, x, y) else {
            return false;
        };

        self.logical_viewport_point_for_visual(visual_point).is_some()
    }

    fn visual_point_from_pixels(
        &self,
        size_info: &SizeInfo,
        x: usize,
        y: usize,
    ) -> Option<Point<usize>> {
        if !Self::contains_x(size_info, x) {
            return None;
        }

        let col = Self::visual_column_from_pixels(size_info, x)?;
        let (visual_strip_index, _) = self.strip_column_at_visual(col)?;
        let line = self.visual_line_from_pixels(size_info, y, visual_strip_index)?;
        Some(Point::new(line, Column(col)))
    }

    fn visual_point_from_pixels_clamped(
        &self,
        size_info: &SizeInfo,
        x: usize,
        y: usize,
    ) -> Point<usize> {
        let visual_columns = size_info.columns().saturating_sub(1);
        let col = Self::visual_column_from_pixels_clamped(size_info, x);
        let col = min(col, visual_columns);
        let col = self.clamp_visual_column(col);
        let (visual_strip_index, _) = self
            .strip_column_at_visual(col)
            .expect("clamped visual column should always resolve to a strip");
        let line = self.visual_line_from_pixels_clamped(size_info, y, visual_strip_index);
        Point::new(line, Column(col))
    }

    fn contains_x(size_info: &SizeInfo, x: usize) -> bool {
        x <= (size_info.padding_x() + size_info.columns() as f32 * size_info.cell_width()) as usize
            && x > size_info.padding_x() as usize
    }

    fn visual_column_from_pixels(size_info: &SizeInfo, x: usize) -> Option<usize> {
        let padding_x = size_info.padding_x() as usize;
        let cell_width = size_info.cell_width() as usize;
        let local_x = x.checked_sub(padding_x + 1)?;
        Some(min(local_x / cell_width, size_info.columns().saturating_sub(1)))
    }

    fn visual_line_from_pixels(
        &self,
        size_info: &SizeInfo,
        y: usize,
        visual_strip_index: usize,
    ) -> Option<usize> {
        let padding_y = self.strip_top_padding_px(size_info, visual_strip_index);
        let cell_height = size_info.cell_height() as usize;
        let local_y = y.checked_sub(padding_y + 1)?;
        let line = local_y / cell_height;
        (line < self.visual_strip_line_capacity(visual_strip_index)).then_some(line)
    }

    fn visual_column_from_pixels_clamped(size_info: &SizeInfo, x: usize) -> usize {
        let padding_x = size_info.padding_x() as usize;
        let cell_width = size_info.cell_width() as usize;
        let local_x = x.saturating_sub(padding_x);
        min(local_x / cell_width, size_info.columns().saturating_sub(1))
    }

    fn visual_line_from_pixels_clamped(
        &self,
        size_info: &SizeInfo,
        y: usize,
        visual_strip_index: usize,
    ) -> usize {
        let padding_y = self.strip_top_padding_px(size_info, visual_strip_index);
        let cell_height = size_info.cell_height() as usize;
        let local_y = y.saturating_sub(padding_y);
        min(
            local_y / cell_height,
            self.visual_strip_line_capacity(visual_strip_index).saturating_sub(1),
        )
    }

    fn strip_start_column(&self, strip_index: usize) -> usize {
        self.left_padding_columns
            + strip_index * self.logical_columns
            + self.cumulative_gutter_columns_before_strip(strip_index)
    }

    fn cumulative_gutter_columns_before_strip(&self, strip_index: usize) -> usize {
        let gutter_count = strip_index.min(self.strip_count.saturating_sub(1));
        gutter_count * self.gutter_columns + min(gutter_count, self.extra_gutter_columns)
    }

    fn visual_strip_index_for_logical(&self, logical_strip_index: usize) -> usize {
        match self.order {
            MultiColumnOrder::StartLeft => logical_strip_index,
            MultiColumnOrder::EndLeft => self.strip_count - 1 - logical_strip_index,
        }
    }

    fn logical_strip_index_for_visual(&self, visual_strip_index: usize) -> usize {
        match self.order {
            MultiColumnOrder::StartLeft => visual_strip_index,
            MultiColumnOrder::EndLeft => self.strip_count - 1 - visual_strip_index,
        }
    }

    fn total_logical_capacity(&self) -> usize {
        (0..self.strip_count).map(|strip_index| self.logical_strip_line_capacity(strip_index)).sum()
    }

    fn logical_strip_for_line(&self, line: usize) -> Option<(usize, usize)> {
        let mut start_line = 0usize;
        for logical_strip_index in 0..self.strip_count {
            let strip_lines = self.logical_strip_line_capacity(logical_strip_index);
            let end_line = start_line.saturating_add(strip_lines);
            if line < end_line {
                return Some((logical_strip_index, line - start_line));
            }
            start_line = end_line;
        }

        None
    }

    fn logical_strip_start_line(&self, logical_strip_index: usize) -> usize {
        (0..logical_strip_index).map(|index| self.logical_strip_line_capacity(index)).sum()
    }

    fn logical_strip_line_capacity(&self, logical_strip_index: usize) -> usize {
        self.visual_strip_line_capacity(self.visual_strip_index_for_logical(logical_strip_index))
    }

    fn visual_strip_line_capacity(&self, visual_strip_index: usize) -> usize {
        self.visual_lines
            + usize::from(self.visual_strip_y_offset_px(visual_strip_index) > 0)
                * self.ear_extra_lines
    }

    fn visual_strip_y_offset_px(&self, visual_strip_index: usize) -> usize {
        self.ear_aware_regions
            .filter(|regions| {
                regions.span_fits_auxiliary_region(
                    self.strip_start_x_px(visual_strip_index),
                    self.strip_width_px(),
                )
            })
            .map_or(0, |regions| regions.reclaim_top_px)
    }

    fn strip_start_x_px(&self, strip_index: usize) -> usize {
        self.padding_x_px + self.strip_start_column(strip_index) * self.cell_width_px
    }

    fn strip_width_px(&self) -> usize {
        self.logical_columns * self.cell_width_px
    }

    fn strip_top_padding_px(&self, size_info: &SizeInfo, visual_strip_index: usize) -> usize {
        (size_info.padding_y() as usize)
            .saturating_sub(self.visual_strip_y_offset_px(visual_strip_index))
    }

    fn strip_column_at_visual(&self, visual_column: usize) -> Option<(usize, usize)> {
        if visual_column < self.left_padding_columns
            || visual_column >= self.visual_columns.saturating_sub(self.right_padding_columns)
        {
            return None;
        }

        for strip_index in 0..self.strip_count {
            let start = self.strip_start_column(strip_index);
            let end = start + self.logical_columns;
            if (start..end).contains(&visual_column) {
                return Some((strip_index, visual_column - start));
            }
        }

        None
    }

    fn clamp_visual_column(&self, visual_column: usize) -> usize {
        if self.strip_count == 1 {
            return self.strip_start_column(0)
                + min(
                    visual_column.saturating_sub(self.left_padding_columns),
                    self.logical_columns - 1,
                );
        }

        if visual_column < self.left_padding_columns {
            return self.strip_start_column(0);
        }

        let right_edge = self.visual_columns.saturating_sub(self.right_padding_columns);
        if visual_column >= right_edge {
            return self.strip_start_column(self.strip_count - 1) + self.logical_columns - 1;
        }

        for strip_index in 0..self.strip_count {
            let start = self.strip_start_column(strip_index);
            let end = start + self.logical_columns;
            if visual_column < start {
                return start;
            }
            if (start..end).contains(&visual_column) {
                return visual_column;
            }

            if strip_index + 1 < self.strip_count {
                let next_start = self.strip_start_column(strip_index + 1);
                if visual_column < next_start {
                    let left = end - 1;
                    let right = next_start;
                    return if visual_column - left <= right - visual_column {
                        left
                    } else {
                        right
                    };
                }
            }
        }

        self.strip_start_column(self.strip_count - 1) + self.logical_columns - 1
    }

    fn visible_logical_lines(&self) -> usize {
        self.logical_lines.saturating_sub(self.content_line_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabor_terminal::event::VoidListener;
    use tabor_terminal::index::Line;
    use tabor_terminal::term::{Config as TermConfig, Term};

    fn size(columns: usize, lines: usize) -> SizeInfo {
        SizeInfo::new(columns as f32, lines as f32, 1., 1., 0., 0., 0., false)
    }

    fn notched_size() -> SizeInfo {
        SizeInfo::new_with_vertical_padding(300., 120., 10., 20., 0., 0., 40., 0., false)
    }

    fn ear_regions() -> EarAwareTopRegions {
        EarAwareTopRegions {
            reclaim_top_px: 40,
            left: Some(crate::display::auxiliary_regions::AuxiliaryTopRegion { x: 0, width: 100 }),
            right: Some(crate::display::auxiliary_regions::AuxiliaryTopRegion {
                x: 200,
                width: 100,
            }),
        }
    }

    #[test]
    fn multi_column_exact_fit_uses_all_strips() {
        let layout = TerminalViewportLayout::new(
            &size(300, 40),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );

        assert_eq!(layout.strip_count(), 3);
        assert_eq!(layout.logical_size(&size(300, 40)).columns(), 100);
        assert_eq!(layout.logical_size(&size(300, 40)).screen_lines(), 120);
        assert_eq!(layout.gutter_columns, 0);
    }

    #[test]
    fn multi_column_uses_extra_space_as_gutters() {
        let layout = TerminalViewportLayout::new(
            &size(250, 40),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );

        assert_eq!(layout.strip_count(), 2);
        assert_eq!(layout.left_padding_columns, 0);
        assert_eq!(layout.right_padding_columns, 0);
        assert_eq!(layout.gutter_columns, 50);
        assert_eq!(layout.extra_gutter_columns, 0);
    }

    #[test]
    fn exact_strip_count_preserves_requested_column_count() {
        let layout = TerminalViewportLayout::new(
            &size(251, 40),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            Some(3),
            None,
        );

        assert_eq!(layout.strip_count(), 3);
        assert_eq!(layout.target_columns(), 83);
        assert_eq!(layout.logical_size(&size(251, 40)).screen_lines(), 120);
    }

    #[test]
    fn logical_visual_roundtrip() {
        let layout = TerminalViewportLayout::new(
            &size(250, 10),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );
        let logical = Point::new(13, Column(7));
        let visual = layout.visual_point_for_logical_viewport(logical).unwrap();

        assert_eq!(visual, Point::new(3, Column(157)));
        assert_eq!(layout.logical_viewport_point_for_visual(visual), Some(logical));
    }

    #[test]
    fn multi_column_distributes_extra_space_between_strips() {
        let layout = TerminalViewportLayout::new(
            &size(333, 10),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );

        assert_eq!(layout.strip_count(), 3);
        assert_eq!(layout.left_padding_columns, 0);
        assert_eq!(layout.right_padding_columns, 0);
        assert_eq!(layout.strip_start_column(0), 0);
        assert_eq!(layout.strip_start_column(1), 117);
        assert_eq!(layout.strip_start_column(2), 233);
    }

    #[test]
    fn multi_column_default_places_eof_in_rightmost_strip() {
        let layout = TerminalViewportLayout::new(
            &size(300, 4),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );
        let eof = Point::new(11, Column(0));

        assert_eq!(layout.visual_point_for_logical_viewport(eof), Some(Point::new(3, Column(200))));
    }

    #[test]
    fn multi_column_short_content_starts_at_first_strip_top() {
        let layout = TerminalViewportLayout::new(
            &size(300, 4),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );

        assert_eq!(
            layout.visual_point_for_logical_viewport(Point::new(0, Column(0))),
            Some(Point::new(0, Column(0))),
        );
        assert_eq!(
            layout.logical_viewport_point_for_visual(Point::new(0, Column(0))),
            Some(Point::new(0, Column(0))),
        );
    }

    #[test]
    fn multi_column_clamped_pixel_mapping_stays_in_first_strip() {
        let size_info = size(300, 4);
        let layout = TerminalViewportLayout::new(
            &size_info,
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );

        assert_eq!(
            layout.logical_terminal_point_from_pixels_clamped(&size_info, 0, 0, 0),
            Point::new(Line(0), Column(0)),
        );
    }

    #[test]
    fn with_terminal_content_clamps_resize_race_to_live_term_dimensions() {
        let size_info = size(120, 40);
        let term_size = size(80, 24);
        let term = Term::new(TermConfig::default(), &term_size, VoidListener);
        let layout = TerminalViewportLayout::new(
            &size_info,
            TerminalViewMode::Normal,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        )
        .with_terminal_content(&term);

        assert_eq!(layout.logical_size(&size_info).columns(), 80);
        assert_eq!(layout.logical_size(&size_info).screen_lines(), 24);
        assert_eq!(
            layout.logical_terminal_point_from_pixels_clamped(&size_info, 119, 39, 0),
            Point::new(Line(23), Column(79)),
        );
    }

    #[test]
    fn multi_column_end_left_places_eof_in_leftmost_strip() {
        let layout = TerminalViewportLayout::new(
            &size(300, 4),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig {
                order: MultiColumnOrder::EndLeft,
                ..MultiColumnTerminalConfig::default()
            },
            None,
            None,
        );
        let eof = Point::new(11, Column(0));

        assert_eq!(layout.visual_point_for_logical_viewport(eof), Some(Point::new(3, Column(0))));
    }

    #[test]
    fn gutter_is_not_text_area() {
        let layout = TerminalViewportLayout::new(
            &size(205, 10),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );
        let gutter = Point::new(0, Column(102));

        assert!(layout.logical_viewport_point_for_visual(Point::new(0, Column(99))).is_some());
        assert_eq!(layout.logical_viewport_point_for_visual(gutter), None);
        assert_eq!(layout.logical_viewport_point_for_visual(Point::new(0, Column(104))), None);
        assert!(layout.logical_viewport_point_for_visual(Point::new(0, Column(105))).is_some());
    }

    #[test]
    fn ear_aware_strips_extend_outer_columns_and_roundtrip_pixels() {
        let size_info = notched_size();
        let layout = TerminalViewportLayout::new(
            &size_info,
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            Some(3),
            Some(ear_regions()),
        );

        assert_eq!(layout.logical_size(&size_info).columns(), 10);
        assert_eq!(layout.logical_size(&size_info).screen_lines(), 16);
        assert_eq!(layout.visual_damage_lines(), 6);
        assert_eq!(
            layout.strip_geometries(),
            vec![
                TerminalViewportStripGeometry {
                    start_column: 0,
                    column_count: 10,
                    y_offset_px: 40,
                    visual_line_count: 6,
                    logical_start_line: 0,
                    logical_line_count: 6,
                },
                TerminalViewportStripGeometry {
                    start_column: 10,
                    column_count: 10,
                    y_offset_px: 0,
                    visual_line_count: 4,
                    logical_start_line: 6,
                    logical_line_count: 4,
                },
                TerminalViewportStripGeometry {
                    start_column: 20,
                    column_count: 10,
                    y_offset_px: 40,
                    visual_line_count: 6,
                    logical_start_line: 10,
                    logical_line_count: 6,
                },
            ]
        );
        assert_eq!(
            layout.visual_point_for_logical_viewport(Point::new(13, Column(2))),
            Some(Point::new(3, Column(22)))
        );
        assert_eq!(
            layout.logical_viewport_point_for_visual(Point::new(3, Column(22))),
            Some(Point::new(13, Column(2)))
        );
        assert!(layout.contains_point(&size_info, 5, 5));
        assert!(!layout.contains_point(&size_info, 105, 5));
        assert_eq!(
            layout.logical_terminal_point_from_pixels_clamped(&size_info, 5, 5, 0),
            Point::new(Line(0), Column(0)),
        );
    }

    #[test]
    fn ear_aware_strip_geometry_preserves_end_left_logical_order() {
        let size_info = notched_size();
        let layout = TerminalViewportLayout::new(
            &size_info,
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig {
                order: MultiColumnOrder::EndLeft,
                ..MultiColumnTerminalConfig::default()
            },
            Some(3),
            Some(ear_regions()),
        );

        let strips = layout.strip_geometries();
        assert_eq!(strips[0].logical_start_line, 10);
        assert_eq!(strips[1].logical_start_line, 6);
        assert_eq!(strips[2].logical_start_line, 0);
        assert_eq!(
            layout.visual_point_for_logical_viewport(Point::new(0, Column(0))),
            Some(Point::new(0, Column(20)))
        );
    }
}
