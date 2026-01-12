use std::time::{Duration, Instant};

use crate::tabs::TabId;
use crate::window_kind::TabKind;

#[cfg(target_os = "macos")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
use crate::macos::favicon::FaviconImage;

#[cfg(target_os = "macos")]
#[derive(Clone, Debug)]
pub struct TabFavicon {
    id: u64,
    pub character: char,
    pub image: Arc<FaviconImage>,
}

#[cfg(target_os = "macos")]
impl TabFavicon {
    pub fn new(id: u64, character: char, image: Arc<FaviconImage>) -> Self {
        Self { id, character, image }
    }
}

#[cfg(target_os = "macos")]
impl PartialEq for TabFavicon {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[cfg(target_os = "macos")]
impl Eq for TabFavicon {}

pub const TAB_ACTIVITY_ACTIVE_WINDOW: Duration = Duration::from_millis(3000);
pub const TAB_ACTIVITY_TICK_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabActivity {
    pub last_output: Option<Instant>,
    pub has_unseen_output: bool,
}

impl TabActivity {
    pub fn note_output(&mut self, now: Instant, seen: bool) {
        self.last_output = Some(now);
        self.has_unseen_output = !seen;
    }

    pub fn mark_seen(&mut self) {
        self.has_unseen_output = false;
    }

    pub fn is_active(&self, now: Instant) -> bool {
        self.last_output
            .is_some_and(|last| now.duration_since(last) <= TAB_ACTIVITY_ACTIVE_WINDOW)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabPanelTab {
    pub tab_id: TabId,
    pub title: String,
    pub is_active: bool,
    pub kind: TabKind,
    pub activity: Option<TabActivity>,
    #[cfg(target_os = "macos")]
    pub favicon: Option<TabFavicon>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabPanelGroup {
    pub id: usize,
    pub label: String,
    pub tabs: Vec<TabPanelTab>,
}

#[derive(Clone, Debug)]
pub enum TabPanelCommand {
    Focus(TabId),
    Close(TabId),
    Move {
        tab_id: TabId,
        target_group_id: Option<usize>,
        target_index: Option<usize>,
    },
    MoveGroup {
        group_id: usize,
        target_index: usize,
    },
    RenameTab(TabId),
    RenameGroup(usize),
}
