#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct AuxiliaryTopRegion {
    pub x: usize,
    pub width: usize,
}

impl AuxiliaryTopRegion {
    pub fn right(self) -> usize {
        self.x.saturating_add(self.width)
    }

    pub fn contains_span(self, start_x: usize, width: usize) -> bool {
        let end_x = start_x.saturating_add(width);
        start_x >= self.x && end_x <= self.right()
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct EarAwareTopRegions {
    pub reclaim_top_px: usize,
    pub left: Option<AuxiliaryTopRegion>,
    pub right: Option<AuxiliaryTopRegion>,
}

impl EarAwareTopRegions {
    pub fn span_fits_auxiliary_region(self, start_x: usize, width: usize) -> bool {
        self.reclaim_top_px > 0
            && (self.left.is_some_and(|region| region.contains_span(start_x, width))
                || self.right.is_some_and(|region| region.contains_span(start_x, width)))
    }
}
