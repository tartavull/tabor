use std::cmp::{max, min};

use serde::{Deserialize, Serialize};

use tabor_terminal::event::WindowSize;
use tabor_terminal::grid::Dimensions;
use tabor_terminal::index::{Column, Point};
use tabor_terminal::term::{self, Term};

use crate::config::terminal::{MultiColumnOrder, MultiColumnTerminalConfig};
use crate::display::SizeInfo;

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
        }
    }

    pub fn new(
        size_info: &SizeInfo,
        mode: TerminalViewMode,
        config: &MultiColumnTerminalConfig,
    ) -> Self {
        let visual_columns = size_info.columns();
        let visual_lines = size_info.screen_lines();

        if mode != TerminalViewMode::MultiColumn {
            return Self::normal(*size_info);
        }

        let target_columns = max(config.target_columns, 1);
        let logical_columns = min(target_columns, visual_columns);
        let strip_count = max(visual_columns / logical_columns, 1);
        let used_columns = strip_count * logical_columns;
        let leftover_columns = visual_columns.saturating_sub(used_columns);
        let gutter_slots = strip_count.saturating_sub(1);
        let gutter_columns = if gutter_slots == 0 { 0 } else { leftover_columns / gutter_slots };
        let extra_gutter_columns =
            if gutter_slots == 0 { 0 } else { leftover_columns % gutter_slots };
        let logical_lines = visual_lines * strip_count;

        Self {
            mode,
            order: config.order,
            content_line_offset: 0,
            target_columns,
            visual_columns,
            visual_lines,
            logical_columns,
            logical_lines,
            strip_count,
            gutter_columns,
            extra_gutter_columns,
            left_padding_columns: 0,
            right_padding_columns: 0,
        }
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

    pub fn is_multi_column(&self) -> bool {
        self.mode == TerminalViewMode::MultiColumn
    }

    pub fn with_terminal_content<T>(mut self, _term: &Term<T>) -> Self {
        self.content_line_offset = 0;
        self
    }

    pub fn visual_point_for_logical_viewport(&self, point: Point<usize>) -> Option<Point<usize>> {
        let line = point.line.checked_add(self.content_line_offset)?;
        if line >= self.logical_lines || point.column.0 >= self.logical_columns {
            return None;
        }

        let logical_strip_index = line / self.visual_lines;
        let line = line % self.visual_lines;
        let visual_strip_index = self.visual_strip_index_for_logical(logical_strip_index);
        let column = self.strip_start_column(visual_strip_index) + point.column.0;

        Some(Point::new(line, Column(column)))
    }

    pub fn logical_viewport_point_for_visual(&self, point: Point<usize>) -> Option<Point<usize>> {
        if point.line >= self.visual_lines {
            return None;
        }

        let (visual_strip_index, strip_column) = self.strip_column_at_visual(point.column.0)?;
        let logical_strip_index = self.logical_strip_index_for_visual(visual_strip_index);
        let line = logical_strip_index * self.visual_lines + point.line;
        let line = line.checked_sub(self.content_line_offset)?;
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
            let display_line = logical_strip_index * self.visual_lines + visual_point.line;
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
        if !size_info.contains_point(x, y) {
            return None;
        }

        let col = Self::visual_column_from_pixels(size_info, x)?;
        let line = Self::visual_line_from_pixels(size_info, y)?;
        Some(Point::new(line, Column(col)))
    }

    fn visual_point_from_pixels_clamped(
        &self,
        size_info: &SizeInfo,
        x: usize,
        y: usize,
    ) -> Point<usize> {
        let visual_columns = size_info.columns().saturating_sub(1);
        let visual_lines = size_info.screen_lines().saturating_sub(1);
        let col = Self::visual_column_from_pixels_clamped(size_info, x);
        let line = Self::visual_line_from_pixels_clamped(size_info, y);
        let col = min(col, visual_columns);
        let line = min(line, visual_lines);
        let col = self.clamp_visual_column(col);
        Point::new(line, Column(col))
    }

    fn visual_column_from_pixels(size_info: &SizeInfo, x: usize) -> Option<usize> {
        let padding_x = size_info.padding_x() as usize;
        let cell_width = size_info.cell_width() as usize;
        let local_x = x.checked_sub(padding_x + 1)?;
        Some(min(local_x / cell_width, size_info.columns().saturating_sub(1)))
    }

    fn visual_line_from_pixels(size_info: &SizeInfo, y: usize) -> Option<usize> {
        let padding_y = size_info.padding_y() as usize;
        let cell_height = size_info.cell_height() as usize;
        let local_y = y.checked_sub(padding_y + 1)?;
        Some(min(local_y / cell_height, size_info.screen_lines().saturating_sub(1)))
    }

    fn visual_column_from_pixels_clamped(size_info: &SizeInfo, x: usize) -> usize {
        let padding_x = size_info.padding_x() as usize;
        let cell_width = size_info.cell_width() as usize;
        let local_x = x.saturating_sub(padding_x);
        min(local_x / cell_width, size_info.columns().saturating_sub(1))
    }

    fn visual_line_from_pixels_clamped(size_info: &SizeInfo, y: usize) -> usize {
        let padding_y = size_info.padding_y() as usize;
        let cell_height = size_info.cell_height() as usize;
        let local_y = y.saturating_sub(padding_y);
        min(local_y / cell_height, size_info.screen_lines().saturating_sub(1))
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
    use tabor_terminal::index::Line;

    fn size(columns: usize, lines: usize) -> SizeInfo {
        SizeInfo::new(columns as f32, lines as f32, 1., 1., 0., 0., 0., false)
    }

    #[test]
    fn multi_column_exact_fit_uses_all_strips() {
        let layout = TerminalViewportLayout::new(
            &size(300, 40),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
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
        );

        assert_eq!(layout.strip_count(), 2);
        assert_eq!(layout.left_padding_columns, 0);
        assert_eq!(layout.right_padding_columns, 0);
        assert_eq!(layout.gutter_columns, 50);
        assert_eq!(layout.extra_gutter_columns, 0);
    }

    #[test]
    fn logical_visual_roundtrip() {
        let layout = TerminalViewportLayout::new(
            &size(250, 10),
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
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
        );

        assert_eq!(
            layout.logical_terminal_point_from_pixels_clamped(&size_info, 0, 0, 0),
            Point::new(Line(0), Column(0)),
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
        );
        let gutter = Point::new(0, Column(102));

        assert!(layout.logical_viewport_point_for_visual(Point::new(0, Column(99))).is_some());
        assert_eq!(layout.logical_viewport_point_for_visual(gutter), None);
        assert_eq!(layout.logical_viewport_point_for_visual(Point::new(0, Column(104))), None);
        assert!(layout.logical_viewport_point_for_visual(Point::new(0, Column(105))).is_some());
    }
}
