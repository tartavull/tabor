#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId {
    pub index: u32,
    pub generation: u32,
}

impl TabId {
    pub fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    pub fn slot_index(self) -> usize {
        self.index as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum TabCommand {
    SelectNext,
    SelectPrevious,
    SelectIndex(usize),
    SelectLast,
}
