//! Terminal window context.

use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use glutin::platform::x11::X11GlConfigExt;
use log::info;
use serde_json as json;
use winit::event::{Event as WinitEvent, Ime, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::WindowId;
#[cfg(target_os = "macos")]
use winit::window::CursorIcon;

use tabor_terminal::event::{Event as TerminalEvent, Notify, OnResize};
use tabor_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use tabor_terminal::grid::{Dimensions, Scroll};
use tabor_terminal::index::Direction;
use tabor_terminal::sync::FairMutex;
use tabor_terminal::term::test::TermSize;
use tabor_terminal::term::{Term, TermMode};
#[cfg(target_os = "macos")]
use tabor_terminal::term::MIN_COLUMNS;
use tabor_terminal::tty;
use tabor_terminal::vte::ansi::NamedColor;

use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
#[cfg(unix)]
use crate::config::Action;
use crate::config::UiConfig;
#[cfg(not(windows))]
use crate::daemon::foreground_process_name;
use crate::display::Display;
use crate::display::color::Rgb;
use crate::display::window::Window;
#[cfg(target_os = "macos")]
use crate::display::{TabPanelEditOutcome, TabPanelEditTarget};
use crate::event::{
    request_web_cursor_update, ActionContext, CommandHistory, CommandState, Event, EventProxy,
    EventType, InlineSearchState, Mouse, SearchState, TouchPurpose,
};
#[cfg(target_os = "macos")]
use crate::event::WebCommand;
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
#[cfg(unix)]
use crate::ipc::{
    IpcError, IpcErrorCode, IpcInspectorMessage, IpcInspectorSession, IpcInspectorTarget,
    IpcTabActivity, IpcTabGroup, IpcTabKind, IpcTabPanelState, IpcTabState, TabSelection,
};
use crate::scheduler::Scheduler;
use crate::tab_panel::TabActivity;
use crate::tabs::TabId;
use crate::window_kind::WindowKind;
use crate::{input, renderer};

#[cfg(target_os = "macos")]
use crate::macos::web_commands::WebCommandState;
#[cfg(target_os = "macos")]
use crate::macos::favicon::{fetch_favicon, resolve_favicon_url, FaviconImage};
#[cfg(target_os = "macos")]
use crate::macos::webview::WebView;
#[cfg(target_os = "macos")]
use crate::macos::remote_inspector::{
    match_tab_for_target, match_target_for_tab, InspectorError, InspectorTabInfo,
    RemoteInspectorClient,
};
#[cfg(target_os = "macos")]
use crate::tab_panel::TabFavicon;

struct TabState {
    id: TabId,
    title: String,
    custom_title: Option<String>,
    program_name: String,
    kind: WindowKind,
    activity: TabActivity,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    notifier: Notifier,
    search_state: SearchState,
    inline_search_state: InlineSearchState,
    command_state: CommandState,
    mouse: Mouse,
    touch: TouchPurpose,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    #[cfg(target_os = "macos")]
    web_view: Option<WebView>,
    #[cfg(target_os = "macos")]
    web_command_state: WebCommandState,
    #[cfg(target_os = "macos")]
    favicon: Option<TabFavicon>,
    #[cfg(target_os = "macos")]
    favicon_pending: bool,
    #[cfg(not(windows))]
    master_fd: RawFd,
    #[cfg(not(windows))]
    shell_pid: u32,
}

#[cfg(target_os = "macos")]
struct ClosedTab {
    kind: WindowKind,
}

#[cfg(target_os = "macos")]
const WEB_FAVICON_JS: &str = r#"
(() => {
  const link = document.querySelector('link[rel~="icon"]');
  return link && link.href ? link.href : '';
})()
"#;

impl TabState {
    fn panel_title(&self) -> String {
        if let Some(custom_title) = &self.custom_title {
            return custom_title.clone();
        }

        if self.kind.is_web() {
            return self.title.clone();
        }

        if self.program_name.is_empty() {
            return self.title.clone();
        }

        self.program_name.clone()
    }
}

struct TabSlot {
    generation: u32,
    tab: Option<TabState>,
}

struct TabGroup {
    id: usize,
    name: Option<String>,
    tabs: Vec<TabId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrawMode {
    Terminal,
    Web,
}

fn draw_mode(kind: &WindowKind) -> DrawMode {
    if kind.is_web() {
        DrawMode::Web
    } else {
        DrawMode::Terminal
    }
}

struct TabManager {
    slots: Vec<TabSlot>,
    free: Vec<usize>,
    active: Option<TabId>,
    groups: Vec<TabGroup>,
    next_group_id: usize,
}

impl TabManager {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            active: None,
            groups: Vec::new(),
            next_group_id: 1,
        }
    }

    fn allocate_id(&mut self) -> TabId {
        if let Some(index) = self.free.pop() {
            let generation = self.slots[index].generation;
            TabId::new(index as u32, generation)
        } else {
            let index = self.slots.len();
            self.slots.push(TabSlot { generation: 0, tab: None });
            TabId::new(index as u32, 0)
        }
    }

    fn insert(&mut self, tab_id: TabId, tab: TabState) {
        if self.slots.len() <= tab_id.slot_index() {
            self.slots.resize_with(tab_id.slot_index() + 1, || TabSlot {
                generation: 0,
                tab: None,
            });
        }

        let slot = &mut self.slots[tab_id.slot_index()];
        slot.tab = Some(tab);

        if self.groups.is_empty() {
            let group = self.new_group();
            self.groups.push(group);
        }

        let target_index = self
            .active
            .and_then(|active| self.groups.iter().position(|group| group.tabs.contains(&active)))
            .unwrap_or(0);

        if !self.groups[target_index].tabs.contains(&tab_id) {
            self.groups[target_index].tabs.push(tab_id);
        }

        if self.active.is_none() {
            self.active = Some(tab_id);
        }
    }

    fn get(&self, tab_id: TabId) -> Option<&TabState> {
        self.slots.get(tab_id.slot_index()).and_then(|slot| {
            (slot.generation == tab_id.generation).then_some(()).and_then(|_| slot.tab.as_ref())
        })
    }

    fn get_mut(&mut self, tab_id: TabId) -> Option<&mut TabState> {
        self.slots.get_mut(tab_id.slot_index()).and_then(|slot| {
            (slot.generation == tab_id.generation).then_some(()).and_then(|_| slot.tab.as_mut())
        })
    }

    fn active_id(&self) -> Option<TabId> {
        self.active
    }

    fn active(&self) -> Option<&TabState> {
        self.active.and_then(|id| self.get(id))
    }

    fn active_mut(&mut self) -> Option<&mut TabState> {
        let active = self.active?;
        self.get_mut(active)
    }

    fn set_active(&mut self, tab_id: TabId) -> bool {
        if self.get(tab_id).is_none() {
            return false;
        }

        if self.active == Some(tab_id) {
            return false;
        }

        self.active = Some(tab_id);
        true
    }

    fn iter(&self) -> impl Iterator<Item = &TabState> {
        self.slots.iter().filter_map(|slot| slot.tab.as_ref())
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut TabState> {
        self.slots.iter_mut().filter_map(|slot| slot.tab.as_mut())
    }

    fn prune_empty_groups(&mut self) {
        self.groups.retain(|group| !group.tabs.is_empty());
        for (index, group) in self.groups.iter_mut().enumerate() {
            group.id = index + 1;
        }
        self.next_group_id = self.groups.len() + 1;
    }

    fn remove(&mut self, tab_id: TabId) -> Option<TabState> {
        let slot = self.slots.get_mut(tab_id.slot_index())?;
        if slot.generation != tab_id.generation {
            return None;
        }

        let tab = slot.tab.take()?;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(tab_id.slot_index());

        for group in &mut self.groups {
            group.tabs.retain(|id| *id != tab_id);
        }
        self.prune_empty_groups();

        if self.active == Some(tab_id) {
            self.active = self.ordered_tabs().first().copied();
        }

        Some(tab)
    }

    fn move_tab(
        &mut self,
        tab_id: TabId,
        target_group_id: Option<usize>,
        target_index: Option<usize>,
    ) -> bool {
        if self.get(tab_id).is_none() {
            return false;
        }

        let mut origin_group_id = None;
        let mut origin_index = None;
        let mut origin_len = 0;
        for group in &self.groups {
            if let Some(pos) = group.tabs.iter().position(|id| *id == tab_id) {
                origin_group_id = Some(group.id);
                origin_index = Some(pos);
                origin_len = group.tabs.len();
                break;
            }
        }

        let origin_group_id = match origin_group_id {
            Some(id) => id,
            None => return false,
        };
        if target_group_id == Some(origin_group_id) && origin_len <= 1 {
            return false;
        }

        let origin_group_removed = origin_len == 1 && target_group_id != Some(origin_group_id);
        let mut target_group_id = target_group_id;
        if origin_group_removed {
            if let Some(id) = target_group_id {
                if id > origin_group_id {
                    target_group_id = Some(id - 1);
                }
            }
        }

        for group in &mut self.groups {
            group.tabs.retain(|id| *id != tab_id);
        }
        self.prune_empty_groups();

        let mut target_index = target_index;
        let group_index = if let Some(group_id) = target_group_id {
            self.groups.iter().position(|group| group.id == group_id)
        } else {
            None
        };

        let group_index = group_index.unwrap_or_else(|| {
            let group = self.new_group();
            self.groups.push(group);
            self.groups.len() - 1
        });

        let group_id = self.groups[group_index].id;
        if Some(group_id) == Some(origin_group_id) {
            if let (Some(origin_index), Some(target_index_value)) = (origin_index, target_index) {
                if target_index_value > origin_index {
                    target_index = Some(target_index_value.saturating_sub(1));
                }
            }
        }

        let group = &mut self.groups[group_index];
        let insert_index = target_index.unwrap_or(group.tabs.len()).min(group.tabs.len());
        group.tabs.insert(insert_index, tab_id);
        true
    }

    fn move_group(&mut self, group_id: usize, target_index: usize) -> bool {
        let Some(from_index) = self.groups.iter().position(|group| group.id == group_id) else {
            return false;
        };

        let target_index = target_index.min(self.groups.len());
        let insert_index =
            if target_index > from_index { target_index.saturating_sub(1) } else { target_index };

        if insert_index == from_index {
            return false;
        }

        let group = self.groups.remove(from_index);
        self.groups.insert(insert_index, group);
        true
    }

    fn ordered_tabs(&self) -> Vec<TabId> {
        self.groups
            .iter()
            .flat_map(|group| group.tabs.iter().copied())
            .filter(|id| self.get(*id).is_some())
            .collect()
    }

    fn set_title(&mut self, tab_id: TabId, title: String) -> bool {
        let Some(tab) = self.get_mut(tab_id) else {
            return false;
        };

        if tab.title == title {
            return false;
        }

        tab.title = title;
        true
    }

    fn set_custom_title(&mut self, tab_id: TabId, title: Option<String>) -> bool {
        let Some(tab) = self.get_mut(tab_id) else {
            return false;
        };

        if tab.custom_title.as_deref() == title.as_deref() {
            return false;
        }

        tab.custom_title = title;
        true
    }

    fn custom_title(&self, tab_id: TabId) -> Option<&str> {
        self.get(tab_id).and_then(|tab| tab.custom_title.as_deref())
    }

    fn tab_label(&self, tab_id: TabId) -> Option<String> {
        self.get(tab_id).map(|tab| tab.panel_title())
    }

    fn set_group_name(&mut self, group_id: usize, name: Option<String>) -> bool {
        let Some(group) = self.groups.iter_mut().find(|group| group.id == group_id) else {
            return false;
        };

        if group.name.as_deref() == name.as_deref() {
            return false;
        }

        group.name = name;
        true
    }

    fn group_name(&self, group_id: usize) -> Option<&str> {
        self.groups
            .iter()
            .find(|group| group.id == group_id)
            .and_then(|group| group.name.as_deref())
    }

    fn group_for_tab(&self, tab_id: TabId) -> Option<(usize, usize)> {
        for group in &self.groups {
            if let Some(index) = group.tabs.iter().position(|id| *id == tab_id) {
                return Some((group.id, index));
            }
        }
        None
    }

    fn set_program_name(&mut self, tab_id: TabId, program_name: String) -> bool {
        let Some(tab) = self.get_mut(tab_id) else {
            return false;
        };

        if tab.program_name == program_name {
            return false;
        }

        tab.program_name = program_name;
        true
    }

    fn panel_groups(&self) -> Vec<crate::tab_panel::TabPanelGroup> {
        let active = self.active;
        self.groups
            .iter()
            .map(|group| crate::tab_panel::TabPanelGroup {
                id: group.id,
                label: match group.name.as_deref() {
                    Some(name) if !name.is_empty() => name.to_string(),
                    _ => format!("group {}", group.id),
                },
                tabs: group
                    .tabs
                    .iter()
                    .filter_map(|tab_id| {
                        self.get(*tab_id).map(|tab| crate::tab_panel::TabPanelTab {
                            tab_id: *tab_id,
                            title: tab.panel_title(),
                            is_active: Some(*tab_id) == active,
                            kind: crate::window_kind::TabKind::from(&tab.kind),
                            activity: if tab.kind.is_web() {
                                None
                            } else {
                                Some(tab.activity.clone())
                            },
                            #[cfg(target_os = "macos")]
                            favicon: tab.favicon.clone(),
                        })
                    })
                    .collect(),
            })
            .collect()
    }

    fn select_by_index(&self, index: usize) -> Option<TabId> {
        let tabs = self.ordered_tabs();
        tabs.get(index).copied()
    }

    fn select_next(&self) -> Option<TabId> {
        let tabs = self.ordered_tabs();
        let active = self.active?;
        let pos = tabs.iter().position(|id| *id == active)?;
        tabs.get((pos + 1) % tabs.len()).copied()
    }

    fn select_previous(&self) -> Option<TabId> {
        let tabs = self.ordered_tabs();
        let active = self.active?;
        let pos = tabs.iter().position(|id| *id == active)?;
        let prev = if pos == 0 { tabs.len() - 1 } else { pos - 1 };
        tabs.get(prev).copied()
    }

    fn select_last(&self) -> Option<TabId> {
        let tabs = self.ordered_tabs();
        tabs.last().copied()
    }

    fn new_group(&mut self) -> TabGroup {
        let id = self.next_group_id;
        self.next_group_id += 1;
        TabGroup { id, name: None, tabs: Vec::new() }
    }

    fn preview_group_id(&self) -> usize {
        self.next_group_id
    }
}

/// Event context for one individual Tabor window.
pub struct WindowContext {
    pub message_buffer: MessageBuffer,
    pub display: Display,
    pub dirty: bool,
    command_history: CommandHistory,
    event_queue: Vec<WinitEvent<Event>>,
    tabs: TabManager,
    #[cfg(target_os = "macos")]
    closed_tabs: Vec<ClosedTab>,
    #[cfg(target_os = "macos")]
    next_favicon_id: u64,
    #[cfg(target_os = "macos")]
    next_favicon_char: u32,
    #[cfg(target_os = "macos")]
    remote_inspector: Option<RemoteInspectorClient>,
    modifiers: Modifiers,
    occluded: bool,
    window_focused: bool,
    preserve_title: bool,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
}

impl WindowContext {
    /// Create initial window context that does bootstrapping the graphics API we're going to use.
    pub fn initial(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Windows has different order of GL platform initialization compared to any other platform;
        // it requires the window first.
        #[cfg(windows)]
        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        #[cfg(windows)]
        let raw_window_handle = Some(window.raw_window_handle());

        #[cfg(not(windows))]
        let raw_window_handle = None;

        let gl_display = renderer::platform::create_gl_display(
            raw_display_handle,
            raw_window_handle,
            config.debug.prefer_egl,
        )?;
        let gl_config = renderer::platform::pick_gl_config(&gl_display, raw_window_handle)?;

        #[cfg(not(windows))]
        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)?;

        let display = Display::new(window, gl_context, &config, false)?;

        Self::new(display, config, options, proxy)
    }

    /// Create additional context with the graphics platform other windows are using.
    pub fn additional(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let gl_display = gl_config.display();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Check if new window should join an existing tab panel group.
        let tabbed = false;

        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let raw_window_handle = window.raw_window_handle();
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, gl_config, Some(raw_window_handle))?;

        let display = Display::new(window, gl_context, &config, tabbed)?;

        let mut window_context = Self::new(display, config, options, proxy)?;

        // Set the config overrides at startup.
        //
        // These are already applied to `config`, so no update is necessary.
        window_context.window_config = config_overrides;

        Ok(window_context)
    }

    /// Create a new terminal window context.
    fn new(
        display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let preserve_title = options.window_identity.title.is_some();

        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        let mut tabs = TabManager::new();
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);
        let first_tab =
            Self::spawn_tab(&mut tabs, &display, &config, pty_config, &proxy, options.window_kind)?;

        // Create context for the Tabor window.
        let mut context = WindowContext {
            preserve_title,
            display,
            config,
            message_buffer: Default::default(),
            command_history: Default::default(),
            window_config: Default::default(),
            event_queue: Default::default(),
            modifiers: Default::default(),
            occluded: Default::default(),
            window_focused: Default::default(),
            tabs,
            #[cfg(target_os = "macos")]
            closed_tabs: Default::default(),
            #[cfg(target_os = "macos")]
            next_favicon_id: 0,
            #[cfg(target_os = "macos")]
            next_favicon_char: 0xE000,
            #[cfg(target_os = "macos")]
            remote_inspector: None,
            dirty: Default::default(),
        };

        context.set_active_tab(first_tab);
        context.refresh_tab_panel();
        Ok(context)
    }

    fn spawn_tab(
        tabs: &mut TabManager,
        display: &Display,
        config: &UiConfig,
        pty_config: tty::Options,
        proxy: &EventLoopProxy<Event>,
        window_kind: WindowKind,
    ) -> Result<TabId, Box<dyn Error>> {
        let tab_id = tabs.allocate_id();
        let event_proxy = EventProxy::new(proxy.clone(), display.window.id(), tab_id);

        let terminal = Term::new(config.term_options(), &display.size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        let pty = tty::new(&pty_config, display.size_info.into(), display.window.id().into())?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
        )?;

        let loop_tx = event_loop.channel();
        let _io_thread = event_loop.spawn();

        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        #[cfg(not(target_os = "macos"))]
        if matches!(window_kind, WindowKind::Web { .. }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Web tabs are only supported on macOS",
            )
            .into());
        }

        #[cfg(target_os = "macos")]
        let web_view = match &window_kind {
            WindowKind::Web { url } => Some(WebView::new(&display.window, &display.size_info, url)?),
            WindowKind::Terminal => None,
        };

        let title = match &window_kind {
            WindowKind::Terminal => config.window.identity.title.clone(),
            WindowKind::Web { url } => {
                if url.is_empty() {
                    String::from("Browser")
                } else {
                    url.clone()
                }
            },
        };

        let tab = TabState {
            id: tab_id,
            title,
            custom_title: None,
            program_name: String::new(),
            kind: window_kind,
            activity: TabActivity::default(),
            terminal,
            notifier: Notifier(loop_tx),
            search_state: Default::default(),
            inline_search_state: Default::default(),
            command_state: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            #[cfg(target_os = "macos")]
            web_view,
            #[cfg(target_os = "macos")]
            web_command_state: Default::default(),
            #[cfg(target_os = "macos")]
            favicon: None,
            #[cfg(target_os = "macos")]
            favicon_pending: false,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
        };

        tabs.insert(tab_id, tab);
        Ok(tab_id)
    }

    #[cfg(target_os = "macos")]
    fn refresh_tab_panel(&mut self) {
        if !self.display.tab_panel.is_enabled() {
            return;
        }

        let groups = self.tabs.panel_groups();
        let new_group_id = Some(self.tabs.preview_group_id());
        if self.display.set_tab_panel_groups(groups, new_group_id) {
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn refresh_tab_panel(&mut self) {}

    pub(crate) fn note_terminal_output(&mut self, tab_id: TabId, is_active: bool) {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };

        if tab.kind.is_web() {
            return;
        }

        tab.activity.note_output(Instant::now(), is_active);
        self.refresh_tab_panel();
    }

    pub(crate) fn has_active_terminal_output(&self, now: Instant) -> bool {
        self.tabs
            .iter()
            .any(|tab| !tab.kind.is_web() && tab.activity.is_active(now))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn tab_panel_enabled(&self) -> bool {
        self.display.tab_panel.is_enabled()
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn tab_panel_enabled(&self) -> bool {
        false
    }

    fn begin_tab_rename(&mut self, tab_id: TabId) {
        let Some(label) = self.tabs.tab_label(tab_id) else {
            return;
        };

        if let Some(active_tab) = self.tabs.active_mut() {
            if active_tab.command_state.is_active() {
                active_tab.command_state.cancel();
            }

            if active_tab.search_state.history_index.is_some() {
                active_tab.search_state.history_index = None;
                active_tab.search_state.clear_focused_match();
            }
        }

        if self.display.tab_panel.begin_edit_tab(tab_id, label) {
            self.display.pending_update.dirty = true;
            self.display.damage_tracker.frame().mark_fully_damaged();
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    fn begin_group_rename(&mut self, group_id: usize) {
        let name = self
            .tabs
            .group_name(group_id)
            .map(str::to_string)
            .unwrap_or_else(|| format!("group {group_id}"));
        if let Some(active_tab) = self.tabs.active_mut() {
            if active_tab.command_state.is_active() {
                active_tab.command_state.cancel();
            }

            if active_tab.search_state.history_index.is_some() {
                active_tab.search_state.history_index = None;
                active_tab.search_state.clear_focused_match();
            }
        }

        if self.display.tab_panel.begin_edit_group(group_id, name) {
            self.display.pending_update.dirty = true;
            self.display.damage_tracker.frame().mark_fully_damaged();
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn set_tab_panel_width_px(&mut self, width_px: f32) {
        let scale_factor = self.display.window.scale_factor as f32;
        let padding_x = self.config.window.padding(scale_factor).0;
        let cell_width = self.display.size_info.cell_width();
        let viewport_width = self.display.size_info.width();

        let available_cols = ((viewport_width - 2.0 * padding_x) / cell_width).floor() as isize;
        let max_panel_cols = (available_cols - MIN_COLUMNS as isize).max(0) as usize;
        let target_px = if max_panel_cols == 0 {
            0.0
        } else {
            let min_width = cell_width;
            let max_width = max_panel_cols as f32 * cell_width;
            width_px.clamp(min_width, max_width)
        };

        let target_logical = (target_px / scale_factor).round() as usize;
        if target_logical == self.config.window.tab_panel.width {
            return;
        }

        let option = format!("window.tab_panel.width={target_logical}");
        let parsed = toml::from_str(&option)
            .expect("failed to parse tab panel width override");

        if let Some(existing) = self
            .window_config
            .iter_mut()
            .find(|(key, _)| key.trim_start().starts_with("window.tab_panel.width"))
        {
            *existing = (option, parsed);
        } else {
            self.window_config.push((option, parsed));
        }

        self.update_config(self.config.clone());
    }

    fn update_webview_visibility(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let active_id = self.tabs.active_id();
            for tab in self.tabs.iter_mut() {
                let Some(web_view) = tab.web_view.as_mut() else {
                    continue;
                };

                let visible = Some(tab.id) == active_id;
                web_view.set_visible(visible);
                if visible {
                    web_view.update_frame(&self.display.window, &self.display.size_info);
                }
            }
        }
    }

    fn update_active_web_title(&mut self, event_proxy: &EventLoopProxy<Event>) {
        #[cfg(target_os = "macos")]
        {
            let mut pending_scroll = None;
            let mut url_update = None;
            let mut favicon_request = None;
            let mut favicon_cleared = false;
            let title = {
                let Some(active_tab) = self.tabs.active_mut() else {
                    return;
                };

                let Some(web_view) = active_tab.web_view.as_mut() else {
                    return;
                };

                let title = web_view.poll_title().map(|title| (active_tab.id, title));
                if let Some(url) = web_view.poll_url() {
                    if let WindowKind::Web { url: current_url } = &mut active_tab.kind {
                        *current_url = url.clone();
                    }
                    active_tab.web_command_state.set_cursor_bootstrapped(false);
                    active_tab.web_command_state.clear_last_cursor_request();
                    active_tab.favicon = None;
                    active_tab.favicon_pending = false;
                    favicon_cleared = true;
                    favicon_request = Some((active_tab.id, url.clone()));
                    pending_scroll = active_tab.web_command_state.take_pending_scroll(&url);
                    url_update = Some(url);
                }

                title
            };

            if let Some((tab_id, title)) = title {
                self.update_tab_title(tab_id, title);
            }

            if let Some(url) = url_update.clone() {
                self.command_history.record_url(url);
            }

            if let Some((scroll_x, scroll_y)) = pending_scroll {
                if let Some(active_tab) = self.tabs.active_mut() {
                    if let Some(web_view) = active_tab.web_view.as_mut() {
                        web_view.exec_js(&format!("window.scrollTo({scroll_x}, {scroll_y});"));
                    }
                }
            }

            if favicon_cleared {
                self.refresh_tab_panel();
            }

            if let Some((tab_id, url)) = favicon_request {
                self.request_web_favicon(tab_id, url, event_proxy);
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn request_web_favicon(
        &mut self,
        tab_id: TabId,
        page_url: String,
        event_proxy: &EventLoopProxy<Event>,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };
        if tab.favicon_pending {
            return;
        }
        let Some(web_view) = tab.web_view.as_mut() else {
            return;
        };

        tab.favicon_pending = true;

        let proxy = event_proxy.clone();
        let window_id = self.display.window.id();
        web_view.eval_js_string(WEB_FAVICON_JS, move |result| {
            let hint = result.unwrap_or_default();
            let icon_url = resolve_favicon_url(&page_url, &hint);

            match icon_url {
                Some(icon_url) => {
                    std::thread::spawn(move || {
                        let icon = fetch_favicon(&icon_url);
                        let event = Event::for_tab(
                            EventType::WebFavicon { page_url, icon },
                            window_id,
                            tab_id,
                        );
                        let _ = proxy.send_event(event);
                    });
                },
                None => {
                    let event = Event::for_tab(
                        EventType::WebFavicon { page_url, icon: None },
                        window_id,
                        tab_id,
                    );
                    let _ = proxy.send_event(event);
                },
            }
        });
    }

    #[cfg(target_os = "macos")]
    fn handle_web_favicon(
        &mut self,
        tab_id: TabId,
        page_url: String,
        icon: Option<FaviconImage>,
    ) {
        let Some(tab) = self.tabs.get(tab_id) else {
            return;
        };
        let WindowKind::Web { url } = &tab.kind else {
            return;
        };
        if url != &page_url {
            return;
        }

        let Some(icon) = icon else {
            if let Some(tab) = self.tabs.get_mut(tab_id) {
                tab.favicon_pending = false;
            }
            return;
        };

        let id = self.next_favicon_id;
        self.next_favicon_id = self.next_favicon_id.wrapping_add(1);
        let character = self.allocate_favicon_char();
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };
        tab.favicon_pending = false;
        tab.favicon = Some(TabFavicon::new(id, character, Arc::new(icon)));
        self.refresh_tab_panel();
        self.dirty = true;
    }

    #[cfg(target_os = "macos")]
    fn handle_web_cursor(&mut self, tab_id: TabId, cursor: Option<CursorIcon>) {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };

        tab.web_command_state.set_cursor_pending(false);
        if !tab.kind.is_web() {
            return;
        }

        let Some(cursor) = cursor else {
            return;
        };

        if tab.web_command_state.last_cursor() == Some(cursor) {
            return;
        }

        tab.web_command_state.set_last_cursor(cursor);
        if Some(tab_id) == self.tabs.active_id() {
            self.display.window.set_mouse_cursor(cursor);
        }
    }

    #[cfg(target_os = "macos")]
    fn handle_web_cursor_request(
        &mut self,
        tab_id: TabId,
        event_proxy: &EventLoopProxy<Event>,
        scheduler: &mut Scheduler,
    ) {
        if Some(tab_id) != self.tabs.active_id() {
            return;
        }

        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };
        if !tab.kind.is_web() {
            return;
        }

        let Some(position) = tab.web_command_state.last_cursor_pos() else {
            return;
        };
        let Some(web_view) = tab.web_view.as_mut() else {
            return;
        };

        request_web_cursor_update(
            web_view,
            &mut tab.web_command_state,
            &self.display,
            position,
            event_proxy,
            scheduler,
            self.display.window.id(),
            tab_id,
        );
    }

    #[cfg(target_os = "macos")]
    fn allocate_favicon_char(&mut self) -> char {
        const BMP_END: u32 = 0xF8FF;
        const SUP_START: u32 = 0xF0000;
        const SUP_END: u32 = 0xFFFFD;

        if self.next_favicon_char == BMP_END + 1 {
            self.next_favicon_char = SUP_START;
        }
        if self.next_favicon_char > SUP_END {
            panic!("Ran out of favicon glyph slots");
        }

        let value = self.next_favicon_char;
        self.next_favicon_char = self.next_favicon_char.saturating_add(1);
        char::from_u32(value).expect("Invalid favicon glyph")
    }

    fn set_active_tab(&mut self, tab_id: TabId) {
        let previous = self.tabs.active_id();
        if self.tabs.get(tab_id).is_none() {
            return;
        }

        let changed = self.tabs.set_active(tab_id);

        if changed {
            self.update_tab_program_name(tab_id);
        }

        if changed {
            if let Some(prev_id) = previous {
                if let Some(prev_tab) = self.tabs.get_mut(prev_id) {
                    if !prev_tab.kind.is_web() {
                        prev_tab.terminal.lock().is_focused = false;
                    }
                }
            }
        }

        if let Some(active_tab) = self.tabs.get_mut(tab_id) {
            if !active_tab.kind.is_web() {
                active_tab.terminal.lock().is_focused = self.window_focused;
                active_tab.activity.mark_seen();
            } else {
                #[cfg(target_os = "macos")]
                {
                    self.display.window.set_mouse_cursor(CursorIcon::Default);
                    active_tab.web_command_state.set_last_cursor(CursorIcon::Default);
                    active_tab.web_command_state.set_cursor_pending(false);
                }
            }
            if !self.preserve_title && self.config.window.dynamic_title {
                let title = active_tab.custom_title.clone().unwrap_or_else(|| active_tab.title.clone());
                self.display.window.set_title(title);
            }
        }

        if changed {
            if let Some(previous_id) = previous {
                if let Some(previous_tab) = self.tabs.get_mut(previous_id) {
                    previous_tab.command_state.cancel();
                    #[cfg(target_os = "macos")]
                    previous_tab.web_command_state.reset_mode();
                }
            }
            if let Some(active_tab) = self.tabs.active_mut() {
                active_tab.command_state.cancel();
                #[cfg(target_os = "macos")]
                active_tab.web_command_state.reset_mode();
            }
            self.display.tab_panel.cancel_edit();
            self.update_webview_visibility();
            self.display.pending_update.dirty = true;
            self.display.damage_tracker.frame().mark_fully_damaged();
            self.refresh_tab_panel();
            self.dirty = true;
        }
    }

    pub(crate) fn create_tab(
        &mut self,
        options: WindowOptions,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<TabId, Box<dyn Error>> {
        let mut pty_config = self.config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);
        let command_input = options.command_input.clone();
        let tab_id = Self::spawn_tab(
            &mut self.tabs,
            &self.display,
            &self.config,
            pty_config,
            proxy,
            options.window_kind,
        )?;
        self.set_active_tab(tab_id);
        if let Some(input) = command_input.as_deref() {
            if let Some(active_tab) = self.tabs.active_mut() {
                active_tab.command_state.start_with_input(':', input);
                self.display.pending_update.dirty = true;
                self.display.damage_tracker.frame().mark_fully_damaged();
                self.dirty = true;
            }
        }
        Ok(tab_id)
    }

    pub(crate) fn handle_tab_command(&mut self, command: crate::tabs::TabCommand) {
        let target = match command {
            crate::tabs::TabCommand::SelectNext => self.tabs.select_next(),
            crate::tabs::TabCommand::SelectPrevious => self.tabs.select_previous(),
            crate::tabs::TabCommand::SelectIndex(index) => self.tabs.select_by_index(index),
            crate::tabs::TabCommand::SelectLast => self.tabs.select_last(),
        };

        if let Some(tab_id) = target {
            self.set_active_tab(tab_id);
        }
    }

    pub(crate) fn active_tab_id(&self) -> Option<TabId> {
        self.tabs.active_id()
    }

    pub(crate) fn tab_kind(&self, tab_id: TabId) -> Option<&WindowKind> {
        self.tabs.get(tab_id).map(|tab| &tab.kind)
    }

    pub(crate) fn close_tab(&mut self, tab_id: TabId) -> bool {
        let was_active = self.tabs.active_id() == Some(tab_id);
        let Some(tab) = self.tabs.remove(tab_id) else {
            return false;
        };

        #[cfg(target_os = "macos")]
        if tab.kind.is_web() {
            self.closed_tabs.push(ClosedTab {
                kind: tab.kind.clone(),
            });
            const MAX_CLOSED_TABS: usize = 10;
            if self.closed_tabs.len() > MAX_CLOSED_TABS {
                self.closed_tabs.remove(0);
            }
        }

        let _ = tab.notifier.0.send(Msg::Shutdown);

        if was_active {
            if let Some(active_id) = self.tabs.active_id() {
                self.set_active_tab(active_id);
            }
        }

        self.refresh_tab_panel();
        self.dirty = true;

        self.tabs.active_id().is_none()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn restore_closed_tab(
        &mut self,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<(), Box<dyn Error>> {
        let Some(closed) = self.closed_tabs.pop() else {
            return Ok(());
        };

        let mut options = WindowOptions::default();
        options.window_kind = closed.kind;
        let _ = self.create_tab(options, proxy)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn open_web_url_in_tab(
        &mut self,
        tab_id: TabId,
        url: String,
    ) -> Result<(), String> {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return Err(String::from("Tab not found"));
        };

        if let WindowKind::Web { url: current_url } = &mut tab.kind {
            *current_url = url.clone();
            if let Some(web_view) = tab.web_view.as_mut() {
                if web_view.load_url(&url) {
                    self.command_history.record_url(url.clone());
                    self.update_tab_title(tab_id, url);
                    return Ok(());
                }
            }
            return Err(String::from("Failed to load URL"));
        }

        Err(String::from("Not a web tab"))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn open_web_url_new_tab(
        &mut self,
        url: String,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<(), Box<dyn Error>> {
        let mut options = WindowOptions::default();
        options.window_kind = WindowKind::Web { url: url.clone() };
        let _ = self.create_tab(options, proxy)?;
        self.command_history.record_url(url);
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn is_focused(&self) -> bool {
        self.window_focused
    }

    #[cfg(unix)]
    pub(crate) fn has_tab(&self, tab_id: TabId) -> bool {
        self.tabs.get(tab_id).is_some()
    }

    #[cfg(unix)]
    pub(crate) fn ipc_tab_groups(&self, now: Instant) -> Vec<IpcTabGroup> {
        let active = self.tabs.active_id();
        self.tabs
            .groups
            .iter()
            .map(|group| {
                let tabs = group
                    .tabs
                    .iter()
                    .enumerate()
                    .filter_map(|(index, tab_id)| {
                        let tab = self.tabs.get(*tab_id)?;
                        let activity = if tab.kind.is_web() {
                            None
                        } else {
                            Some(Self::ipc_activity(&tab.activity, now))
                        };
                        Some(IpcTabState {
                            tab_id: (*tab_id).into(),
                            group_id: group.id,
                            index,
                            is_active: Some(*tab_id) == active,
                            title: tab.title.clone(),
                            custom_title: tab.custom_title.clone(),
                            program_name: tab.program_name.clone(),
                            kind: IpcTabKind::from(&tab.kind),
                            activity,
                        })
                    })
                    .collect();

                IpcTabGroup { id: group.id, name: group.name.clone(), tabs }
            })
            .collect()
    }

    #[cfg(unix)]
    pub(crate) fn ipc_tab_state(&self, tab_id: TabId, now: Instant) -> Option<IpcTabState> {
        let tab = self.tabs.get(tab_id)?;
        let (group_id, index) = self.tabs.group_for_tab(tab_id)?;
        let activity = if tab.kind.is_web() {
            None
        } else {
            Some(Self::ipc_activity(&tab.activity, now))
        };
        Some(IpcTabState {
            tab_id: tab_id.into(),
            group_id,
            index,
            is_active: Some(tab_id) == self.tabs.active_id(),
            title: tab.title.clone(),
            custom_title: tab.custom_title.clone(),
            program_name: tab.program_name.clone(),
            kind: IpcTabKind::from(&tab.kind),
            activity,
        })
    }

    #[cfg(unix)]
    pub(crate) fn ipc_tab_kind(&self, tab_id: TabId) -> Option<IpcTabKind> {
        self.tabs.get(tab_id).map(|tab| IpcTabKind::from(&tab.kind))
    }

    #[cfg(unix)]
    pub(crate) fn ipc_create_tab(
        &mut self,
        options: WindowOptions,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<TabId, IpcError> {
        self.create_tab(options, proxy).map_err(|err| {
            IpcError::new(IpcErrorCode::Internal, format!("Could not create tab: {err}"))
        })
    }

    #[cfg(unix)]
    pub(crate) fn ipc_close_tab(&mut self, tab_id: TabId) -> Result<bool, IpcError> {
        if self.tabs.get(tab_id).is_none() {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        }
        Ok(self.close_tab(tab_id))
    }

    #[cfg(unix)]
    pub(crate) fn ipc_select_tab(&mut self, selection: TabSelection) -> Result<(), IpcError> {
        let target = match selection {
            TabSelection::Active => return Ok(()),
            TabSelection::Next => self.tabs.select_next(),
            TabSelection::Previous => self.tabs.select_previous(),
            TabSelection::Last => self.tabs.select_last(),
            TabSelection::ByIndex { index } => self.tabs.select_by_index(index),
            TabSelection::ById { tab_id } => {
                let tab_id = tab_id.into();
                if self.tabs.get(tab_id).is_some() {
                    Some(tab_id)
                } else {
                    None
                }
            },
        };

        let Some(tab_id) = target else {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        };

        self.set_active_tab(tab_id);
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn ipc_move_tab(
        &mut self,
        tab_id: TabId,
        target_group_id: Option<usize>,
        target_index: Option<usize>,
    ) -> Result<(), IpcError> {
        if !self.tabs.move_tab(tab_id, target_group_id, target_index) {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        }
        self.refresh_tab_panel();
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn ipc_set_tab_title(
        &mut self,
        tab_id: TabId,
        title: Option<String>,
    ) -> Result<(), IpcError> {
        if self.tabs.get(tab_id).is_none() {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        }
        if self.tabs.set_custom_title(tab_id, title) {
            self.refresh_tab_panel();
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn ipc_set_group_name(
        &mut self,
        group_id: usize,
        name: Option<String>,
    ) -> Result<(), IpcError> {
        if !self.tabs.set_group_name(group_id, name) {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Group not found"));
        }
        self.refresh_tab_panel();
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn ipc_restore_closed_tab(
        &mut self,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            return self
                .restore_closed_tab(proxy)
                .map_err(|err| IpcError::new(IpcErrorCode::Internal, err.to_string()));
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = proxy;
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Restore closed tabs is only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_open_url_in_tab(
        &mut self,
        tab_id: TabId,
        url: String,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            let _ = proxy;
            return self
                .open_web_url_in_tab(tab_id, url)
                .map_err(|err| IpcError::new(IpcErrorCode::InvalidRequest, err));
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, url, proxy);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Web tabs are only supported on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_open_url_new_tab(
        &mut self,
        url: String,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<TabId, IpcError> {
        #[cfg(target_os = "macos")]
        {
            let mut options = WindowOptions::default();
            options.window_kind = WindowKind::Web { url: url.clone() };
            let tab_id = self
                .create_tab(options, proxy)
                .map_err(|err| IpcError::new(IpcErrorCode::Internal, err.to_string()))?;
            self.command_history.record_url(url);
            return Ok(tab_id);
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (url, proxy);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Web tabs are only supported on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_reload_web(
        &mut self,
        tab_id: TabId,
        event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            return self.with_action_context(
                tab_id,
                event_loop,
                event_proxy,
                clipboard,
                scheduler,
                |ctx| {
                    ctx.reload_web();
                },
            );
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, event_loop, event_proxy, clipboard, scheduler);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Web tabs are only supported on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_open_inspector(
        &mut self,
        tab_id: TabId,
        event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            return self.with_action_context(
                tab_id,
                event_loop,
                event_proxy,
                clipboard,
                scheduler,
                |ctx| {
                    ctx.open_web_inspector();
                },
            );
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, event_loop, event_proxy, clipboard, scheduler);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Web tabs are only supported on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_tab_panel_state(&self) -> IpcTabPanelState {
        IpcTabPanelState {
            enabled: self.config.window.tab_panel.enabled,
            width: self.config.window.tab_panel.width,
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_set_tab_panel(
        &mut self,
        enabled: Option<bool>,
        width: Option<usize>,
    ) -> Result<(), IpcError> {
        if enabled.is_none() && width.is_none() {
            return Err(IpcError::new(
                IpcErrorCode::InvalidRequest,
                "No tab panel options provided",
            ));
        }

        let mut options = Vec::new();
        if let Some(enabled) = enabled {
            options.push(format!("window.tab_panel.enabled={enabled}"));
        }
        if let Some(width) = width {
            options.push(format!("window.tab_panel.width={width}"));
        }

        let parsed = ParsedOptions::from_options(&options);
        self.add_window_config(self.config.clone(), &parsed);
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn ipc_dispatch_action(
        &mut self,
        tab_id: TabId,
        action: Action,
        event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Result<(), IpcError> {
        self.with_action_context(tab_id, event_loop, event_proxy, clipboard, scheduler, |ctx| {
            input::execute_action(ctx, &action);
        })
    }

    #[cfg(unix)]
    pub(crate) fn ipc_send_input(&mut self, tab_id: TabId, text: String) -> Result<(), IpcError> {
        if self.tabs.get(tab_id).is_none() {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        }
        if self.tabs.active_id() != Some(tab_id) {
            self.set_active_tab(tab_id);
        }
        let Some(tab) = self.tabs.get(tab_id) else {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        };
        tab.notifier.notify(text.into_bytes());
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn ipc_run_command_bar(
        &mut self,
        tab_id: TabId,
        input: String,
        event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Result<(), IpcError> {
        self.with_action_context(tab_id, event_loop, event_proxy, clipboard, scheduler, |ctx| {
            ctx.run_command(input);
        })
    }

    #[cfg(unix)]
    pub(crate) fn ipc_list_inspector_targets(
        &mut self,
    ) -> Result<Vec<IpcInspectorTarget>, IpcError> {
        #[cfg(not(target_os = "macos"))]
        {
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Remote inspector is only supported on macOS",
            ))
        }

        #[cfg(target_os = "macos")]
        {
            self.ensure_remote_inspector()?;
            let targets = self
                .remote_inspector
                .as_ref()
                .expect("remote inspector should be initialized")
                .list_targets()
                .map_err(map_inspector_error)?;
            let tabs = self.inspector_tabs();
            let pid = std::process::id();
            let mapped = targets
                .iter()
                .map(|target| {
                    let tab_id = match_tab_for_target(target, &tabs, pid);
                    IpcInspectorTarget {
                        target_id: target.target_id,
                        target_type: target.target_type.clone(),
                        url: target.url.clone(),
                        title: target.title.clone(),
                        override_name: target.override_name.clone(),
                        host_app_identifier: target.host_app_identifier.clone(),
                        tab_id: tab_id.map(Into::into),
                    }
                })
                .collect();
            Ok(mapped)
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_attach_inspector(
        &mut self,
        tab_id: Option<TabId>,
        target_id: Option<u64>,
    ) -> Result<IpcInspectorSession, IpcError> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, target_id);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Remote inspector is only supported on macOS",
            ))
        }

        #[cfg(target_os = "macos")]
        {
            if tab_id.is_none() && target_id.is_none() {
                return Err(IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    "tab_id or target_id must be provided",
                ));
            }

            self.ensure_remote_inspector()?;
            let targets = self
                .remote_inspector
                .as_ref()
                .expect("remote inspector should be initialized")
                .list_targets()
                .map_err(map_inspector_error)?;
            let resolved_target = if let Some(target_id) = target_id {
                target_id
            } else {
                let tab_id =
                    tab_id.ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
                let tab_info = self.inspector_tab_info(tab_id)?;
                match_target_for_tab(&targets, &tab_info, std::process::id())
                    .map_err(map_inspector_error)?
            };

            let resolved_tab_id = if let Some(tab_id) = tab_id {
                tab_id
            } else {
                let target = targets
                    .iter()
                    .find(|target| target.target_id == resolved_target)
                    .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Target not found"))?;
                match_tab_for_target(target, &self.inspector_tabs(), std::process::id())
                    .ok_or_else(|| {
                        IpcError::new(
                            IpcErrorCode::Ambiguous,
                            "Target does not map to a web tab",
                        )
                    })?
            };

            let session = self
                .remote_inspector
                .as_ref()
                .expect("remote inspector should be initialized")
                .attach(resolved_tab_id, resolved_target)
                .map_err(map_inspector_error)?;

            Ok(IpcInspectorSession {
                session_id: session.session_id,
                target_id: session.target_id,
                tab_id: session.tab_id.into(),
            })
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_detach_inspector(&mut self, session_id: String) -> Result<(), IpcError> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = session_id;
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Remote inspector is only supported on macOS",
            ))
        }

        #[cfg(target_os = "macos")]
        {
            self.ensure_remote_inspector()?;
            self.remote_inspector
                .as_ref()
                .expect("remote inspector should be initialized")
                .detach(&session_id)
                .map_err(map_inspector_error)?;
            Ok(())
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_send_inspector_message(
        &mut self,
        session_id: String,
        message: String,
    ) -> Result<(), IpcError> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (session_id, message);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Remote inspector is only supported on macOS",
            ))
        }

        #[cfg(target_os = "macos")]
        {
            self.ensure_remote_inspector()?;
            self.remote_inspector
                .as_ref()
                .expect("remote inspector should be initialized")
                .send_message(&session_id, &message)
                .map_err(map_inspector_error)?;
            Ok(())
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_poll_inspector_messages(
        &mut self,
        session_id: String,
        max: Option<usize>,
    ) -> Result<Vec<IpcInspectorMessage>, IpcError> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (session_id, max);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Remote inspector is only supported on macOS",
            ))
        }

        #[cfg(target_os = "macos")]
        {
            self.ensure_remote_inspector()?;
            let messages = self
                .remote_inspector
                .as_ref()
                .expect("remote inspector should be initialized")
                .poll_messages(&session_id, max)
                .map_err(map_inspector_error)?;
            let mapped = messages
                .into_iter()
                .map(|message| IpcInspectorMessage {
                    session_id: message.session_id,
                    payload: message.payload,
                })
                .collect();
            Ok(mapped)
        }
    }

    #[cfg(unix)]
    pub(crate) fn has_inspector_session(&self, session_id: &str) -> bool {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = session_id;
            false
        }

        #[cfg(target_os = "macos")]
        {
            self.remote_inspector
                .as_ref()
                .is_some_and(|inspector| inspector.has_session(session_id))
        }
    }

    #[cfg(unix)]
    fn with_action_context<F>(
        &mut self,
        tab_id: TabId,
        event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        f: F,
    ) -> Result<(), IpcError>
    where
        F: FnOnce(&mut ActionContext<'_, Notifier, EventProxy>),
    {
        if self.tabs.get(tab_id).is_none() {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        }

        if self.tabs.active_id() != Some(tab_id) {
            self.set_active_tab(tab_id);
        }

        let old_is_searching = self
            .tabs
            .active()
            .is_some_and(|tab| tab.search_state.history_index.is_some());

        {
            let Some(active_tab) = self.tabs.active_mut() else {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            };

            let mut terminal = active_tab.terminal.lock();
            let mut context = ActionContext {
                cursor_blink_timed_out: &mut active_tab.cursor_blink_timed_out,
                prev_bell_cmd: &mut active_tab.prev_bell_cmd,
                message_buffer: &mut self.message_buffer,
                inline_search_state: &mut active_tab.inline_search_state,
                search_state: &mut active_tab.search_state,
                command_state: &mut active_tab.command_state,
                command_history: &mut self.command_history,
                tab_id: active_tab.id,
                tab_kind: &mut active_tab.kind,
                #[cfg(target_os = "macos")]
                web_view: active_tab.web_view.as_mut(),
                #[cfg(target_os = "macos")]
                web_command_state: &mut active_tab.web_command_state,
                modifiers: &mut self.modifiers,
                notifier: &mut active_tab.notifier,
                display: &mut self.display,
                mouse: &mut active_tab.mouse,
                touch: &mut active_tab.touch,
                dirty: &mut self.dirty,
                occluded: &mut self.occluded,
                terminal: &mut terminal,
                #[cfg(not(windows))]
                master_fd: active_tab.master_fd,
                #[cfg(not(windows))]
                shell_pid: active_tab.shell_pid,
                preserve_title: self.preserve_title,
                config: &self.config,
                event_proxy,
                #[cfg(target_os = "macos")]
                event_loop,
                clipboard,
                scheduler,
            };

            f(&mut context);
        }

        self.apply_ipc_display_update(old_is_searching);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn ensure_remote_inspector(&mut self) -> Result<(), IpcError> {
        if self.remote_inspector.is_none() {
            let inspector = RemoteInspectorClient::connect().map_err(map_inspector_error)?;
            self.remote_inspector = Some(inspector);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn inspector_tabs(&self) -> Vec<InspectorTabInfo> {
        self.tabs
            .iter()
            .filter_map(|tab| {
                let WindowKind::Web { url } = &tab.kind else {
                    return None;
                };
                Some(InspectorTabInfo {
                    tab_id: tab.id,
                    url: if url.is_empty() { None } else { Some(url.clone()) },
                    title: if tab.title.is_empty() { None } else { Some(tab.title.clone()) },
                    override_name: tab.custom_title.clone(),
                })
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn inspector_tab_info(&self, tab_id: TabId) -> Result<InspectorTabInfo, IpcError> {
        let tab = self
            .tabs
            .get(tab_id)
            .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
        let WindowKind::Web { url } = &tab.kind else {
            return Err(IpcError::new(
                IpcErrorCode::InvalidRequest,
                "Tab is not a web tab",
            ));
        };
        Ok(InspectorTabInfo {
            tab_id: tab.id,
            url: if url.is_empty() { None } else { Some(url.clone()) },
            title: if tab.title.is_empty() { None } else { Some(tab.title.clone()) },
            override_name: tab.custom_title.clone(),
        })
    }


    #[cfg(unix)]
    fn apply_ipc_display_update(&mut self, old_is_searching: bool) {
        if self.display.pending_update.dirty {
            if let Some(active_id) = self.tabs.active_id() {
                Self::submit_display_update(
                    active_id,
                    &mut self.tabs,
                    &mut self.display,
                    &self.message_buffer,
                    old_is_searching,
                    &self.config,
                );
                self.dirty = true;
            }
        }
    }

    #[cfg(unix)]
    fn ipc_activity(activity: &TabActivity, now: Instant) -> IpcTabActivity {
        IpcTabActivity {
            has_unseen_output: activity.has_unseen_output,
            last_output_ms_ago: activity
                .last_output
                .map(|last| now.saturating_duration_since(last).as_millis() as u64),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn select_tab_by_query(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }

        let needle = query.to_lowercase();
        let match_id = self.tabs.iter().find_map(|tab| {
            let title = tab.title.to_lowercase();
            let url_match = match &tab.kind {
                WindowKind::Web { url } => url.to_lowercase().contains(&needle),
                WindowKind::Terminal => false,
            };

            if title.contains(&needle) || url_match {
                Some(tab.id)
            } else {
                None
            }
        });

        if let Some(tab_id) = match_id {
            self.set_active_tab(tab_id);
        } else {
            self.message_buffer.push(crate::message_bar::Message::new(
                format!("No matching tab for \"{query}\""),
                crate::message_bar::MessageType::Warning,
            ));
            self.display.pending_update.dirty = true;
        }
    }

    fn update_tab_title(&mut self, tab_id: TabId, title: String) {
        let custom_title = self.tabs.custom_title(tab_id).map(str::to_string);
        if self.tabs.set_title(tab_id, title.clone()) {
            if Some(tab_id) == self.tabs.active_id()
                && !self.preserve_title
                && self.config.window.dynamic_title
            {
                let window_title = custom_title.clone().unwrap_or(title);
                self.display.window.set_title(window_title);
            }
            if custom_title.is_none() {
                self.refresh_tab_panel();
            }
        }
    }

    pub(crate) fn rename_tab(&mut self, tab_id: TabId, name: Option<String>) {
        if !self.tabs.set_custom_title(tab_id, name.clone()) {
            return;
        }

        if Some(tab_id) == self.tabs.active_id()
            && !self.preserve_title
            && self.config.window.dynamic_title
        {
            let title = match name {
                Some(title) => title,
                None => self.tabs.get(tab_id).map(|tab| tab.title.clone()).unwrap_or_default(),
            };
            self.display.window.set_title(title);
        }

        self.refresh_tab_panel();
    }

    pub(crate) fn rename_group(&mut self, group_id: usize, name: Option<String>) {
        let name = name.and_then(|name| {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return None;
            }

            if trimmed == format!("group {group_id}") {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        if self.tabs.set_group_name(group_id, name) {
            self.refresh_tab_panel();
        }
    }

    #[cfg(not(windows))]
    fn update_tab_program_name(&mut self, tab_id: TabId) -> bool {
        let Some(tab) = self.tabs.get(tab_id) else {
            return false;
        };

        if tab.kind.is_web() {
            return false;
        }

        let Ok(program_name) = foreground_process_name(tab.master_fd, tab.shell_pid) else {
            return false;
        };

        self.tabs.set_program_name(tab_id, program_name)
    }

    #[cfg(windows)]
    fn update_tab_program_name(&mut self, _tab_id: TabId) -> bool {
        false
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let old_config = mem::replace(&mut self.config, new_config);

        // Apply ipc config if there are overrides.
        self.config = self.window_config.override_config_rc(self.config.clone());

        self.display.update_config(&self.config);
        for tab in self.tabs.iter_mut() {
            tab.terminal.lock().set_options(self.config.term_options());
        }

        // Reload cursor if its thickness has changed.
        if (old_config.cursor.thickness() - self.config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.font != self.config.font {
            let scale_factor = self.display.window.scale_factor as f32;
            // Do not update font size if it has been changed at runtime.
            if self.display.font_size == old_config.font.size().scale(scale_factor) {
                self.display.font_size = self.config.font.size().scale(scale_factor);
            }

            let font = self.config.font.clone().with_size(self.display.font_size);
            self.display.pending_update.set_font(font);
        }

        // Always reload the theme to account for auto-theme switching.
        self.display.window.set_theme(self.config.window.theme());

        // Update display if either padding options or resize increments were changed.
        let window_config = &old_config.window;
        if window_config.padding(1.) != self.config.window.padding(1.)
            || window_config.dynamic_padding != self.config.window.dynamic_padding
            || window_config.resize_increments != self.config.window.resize_increments
            || window_config.tab_panel != self.config.window.tab_panel
        {
            self.display.pending_update.dirty = true;
        }

        // Update title on config reload according to the following table.
        //
        // │cli │ dynamic_title │ current_title == old_config ││ set_title │
        // │ Y  │       _       │              _              ││     N     │
        // │ N  │       Y       │              Y              ││     Y     │
        // │ N  │       Y       │              N              ││     N     │
        // │ N  │       N       │              _              ││     Y     │
        if !self.preserve_title
            && (!self.config.window.dynamic_title
                || self.display.window.title() == old_config.window.identity.title)
        {
            self.display.window.set_title(self.config.window.identity.title.clone());
        }

        let opaque = self.config.window_opacity() >= 1.;

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(opaque);

        #[cfg(target_os = "macos")]
        self.display.window.set_option_as_alt(self.config.window.option_as_alt());

        // Change opacity and blur state.
        self.display.window.set_transparent(!opaque);
        self.display.window.set_blur(self.config.window.blur);

        // Update hint keys.
        self.display.hint_state.update_alphabet(self.config.hints.alphabet());

        // Update cursor blinking.
        let event = Event::new(TerminalEvent::CursorBlinkingChange.into(), None);
        self.event_queue.push(event.into());

        self.dirty = true;
    }

    /// Get reference to the window's configuration.
    #[cfg(unix)]
    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    /// Clear the window config overrides.
    #[cfg(unix)]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.clear();

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(unix)]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.extend_from_slice(options);

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler) {
        self.display.window.requested_redraw = false;

        if self.occluded {
            return;
        }

        self.dirty = false;

        // Force the display to process any pending display update.
        self.display.process_renderer_update();

        // Request immediate re-draw if visual bell animation is not finished yet.
        if !self.display.visual_bell.completed() {
            // We can get an OS redraw which bypasses tabor's frame throttling, thus
            // marking the window as dirty when we don't have frame yet.
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            } else {
                self.dirty = true;
            }
        }

        // Redraw the window.
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };

        match draw_mode(&tab.kind) {
            DrawMode::Web => {
                self.display.draw_web(
                    scheduler,
                    &self.message_buffer,
                    &self.config,
                    &tab.command_state,
                );
            },
            DrawMode::Terminal => {
                let terminal = tab.terminal.lock();
                self.display.draw(
                    terminal,
                    scheduler,
                    &self.message_buffer,
                    &self.config,
                    &mut tab.search_state,
                    &tab.command_state,
                );
            },
        }
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        #[cfg(target_os = "macos")]
        if self.handle_tab_panel_event(&event, event_proxy) {
            return;
        }

        match event {
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() {
                    return;
                }

                // Continue to process all pending events.
            },
            event => {
                self.event_queue.push(event);
                return;
            },
        }

        let active_id = self.tabs.active_id();
        let mut pending_events = Vec::new();
        let events: Vec<_> = self.event_queue.drain(..).collect();

        for event in events {
            if let WinitEvent::WindowEvent { event: WindowEvent::Focused(is_focused), .. } = &event {
                self.window_focused = *is_focused;
            }

            if let WinitEvent::UserEvent(event) = &event {
                match event.payload() {
                    #[cfg(target_os = "macos")]
                    EventType::WebCommand(command) => {
                        self.handle_web_command_event(event, command, clipboard, event_proxy);
                        continue;
                    },
                    #[cfg(target_os = "macos")]
                    EventType::WebFavicon { page_url, icon } => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };
                        self.handle_web_favicon(tab_id, page_url.clone(), icon.clone());
                        continue;
                    },
                    #[cfg(target_os = "macos")]
                    EventType::WebCursor { cursor } => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };
                        self.handle_web_cursor(tab_id, *cursor);
                        continue;
                    },
                    #[cfg(target_os = "macos")]
                    EventType::WebCursorRequest => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };
                        self.handle_web_cursor_request(tab_id, event_proxy, scheduler);
                        continue;
                    },
                    EventType::Terminal(term_event) => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };

                        if self
                            .tabs
                            .get(tab_id)
                            .is_some_and(|tab| tab.kind.is_web())
                        {
                            continue;
                        }

                        match term_event {
                            TerminalEvent::Title(title) => {
                                self.update_tab_title(tab_id, title.clone());
                            },
                            TerminalEvent::ResetTitle => {
                                let title = self.config.window.identity.title.clone();
                                self.update_tab_title(tab_id, title);
                            },
                            _ => (),
                        }

                        if Some(tab_id) != active_id {
                            self.handle_inactive_terminal_event(tab_id, term_event, clipboard);
                            continue;
                        }
                    },
                    EventType::UpdateTabProgramName => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };

                        if Some(tab_id) == active_id && self.update_tab_program_name(tab_id) {
                            self.refresh_tab_panel();
                        }
                        continue;
                    },
                    _ => (),
                }
            }

            pending_events.push(event);
        }

        let old_is_searching = self
            .tabs
            .active()
            .is_some_and(|tab| tab.search_state.history_index.is_some());

        {
            let Some(active_tab) = self.tabs.active_mut() else {
                return;
            };

            let mut terminal = active_tab.terminal.lock();
            let context = ActionContext {
                cursor_blink_timed_out: &mut active_tab.cursor_blink_timed_out,
                prev_bell_cmd: &mut active_tab.prev_bell_cmd,
                message_buffer: &mut self.message_buffer,
                inline_search_state: &mut active_tab.inline_search_state,
                search_state: &mut active_tab.search_state,
                command_state: &mut active_tab.command_state,
                command_history: &mut self.command_history,
                tab_id: active_tab.id,
                tab_kind: &mut active_tab.kind,
                #[cfg(target_os = "macos")]
                web_view: active_tab.web_view.as_mut(),
                #[cfg(target_os = "macos")]
                web_command_state: &mut active_tab.web_command_state,
                modifiers: &mut self.modifiers,
                notifier: &mut active_tab.notifier,
                display: &mut self.display,
                mouse: &mut active_tab.mouse,
                touch: &mut active_tab.touch,
                dirty: &mut self.dirty,
                occluded: &mut self.occluded,
                terminal: &mut terminal,
                #[cfg(not(windows))]
                master_fd: active_tab.master_fd,
                #[cfg(not(windows))]
                shell_pid: active_tab.shell_pid,
                preserve_title: self.preserve_title,
                config: &self.config,
                event_proxy,
                #[cfg(target_os = "macos")]
                event_loop,
                clipboard,
                scheduler,
            };
            let mut processor = input::Processor::new(context);

            for event in pending_events {
                processor.handle_event(event);
            }
        }

        // Process DisplayUpdate events.
        if self.display.pending_update.dirty {
            if let Some(active_id) = self.tabs.active_id() {
                Self::submit_display_update(
                    active_id,
                    &mut self.tabs,
                    &mut self.display,
                    &self.message_buffer,
                    old_is_searching,
                    &self.config,
                );
                self.dirty = true;
            }
        }

        let Some(active_tab) = self.tabs.active_mut() else {
            return;
        };

        if self.dirty || active_tab.mouse.hint_highlight_dirty {
            if !active_tab.kind.is_web() {
                let terminal = active_tab.terminal.lock();
                self.dirty |= self.display.update_highlighted_hints(
                    &terminal,
                    &self.config,
                    &active_tab.mouse,
                    self.modifiers.state(),
                );
            }
            active_tab.mouse.hint_highlight_dirty = false;
        }

        self.update_active_web_title(event_proxy);

        // Don't call `request_redraw` when event is `RedrawRequested` since the `dirty` flag
        // represents the current frame, but redraw is for the next frame.
        if self.dirty
            && self.display.window.has_frame
            && !self.occluded
            && !matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. })
        {
            self.display.window.request_redraw();
        }
    }

    #[cfg(target_os = "macos")]
    fn handle_tab_panel_event(
        &mut self,
        event: &WinitEvent<Event>,
        event_proxy: &EventLoopProxy<Event>,
    ) -> bool {
        if !self.display.tab_panel.is_enabled() {
            return false;
        }

        match event {
            WinitEvent::WindowEvent {
                event: WindowEvent::CursorMoved { position, .. },
                ..
            } => {
                let update = self.display.tab_panel.cursor_moved(*position, &self.display.size_info);
                if let Some(width_px) = update.resize_width {
                    self.set_tab_panel_width_px(width_px);
                }
                if update.needs_redraw {
                    self.dirty = true;
                    if self.display.window.has_frame {
                        self.display.window.request_redraw();
                    }
                }
                if update.capture {
                    if let Some(cursor) = update.cursor {
                        self.display.window.set_mouse_cursor(cursor);
                    }
                }
                update.capture
            },
            WinitEvent::WindowEvent {
                event: WindowEvent::MouseInput { state, button, .. },
                ..
            } => {
                let update =
                    self.display.tab_panel.mouse_input(*state, *button, &self.display.size_info);

                if let Some(command) = update.command {
                    match command {
                        crate::tab_panel::TabPanelCommand::Focus(tab_id) => {
                            self.set_active_tab(tab_id);
                        },
                        crate::tab_panel::TabPanelCommand::Close(tab_id) => {
                            let event =
                                Event::new(EventType::CloseTab(tab_id), self.display.window.id());
                            let _ = event_proxy.send_event(event);
                        },
                        crate::tab_panel::TabPanelCommand::Move {
                            tab_id,
                            target_group_id,
                            target_index,
                        } => {
                            if self.tabs.move_tab(tab_id, target_group_id, target_index) {
                                self.refresh_tab_panel();
                            }
                        },
                        crate::tab_panel::TabPanelCommand::MoveGroup { group_id, target_index } => {
                            if self.tabs.move_group(group_id, target_index) {
                                self.refresh_tab_panel();
                            }
                        },
                        crate::tab_panel::TabPanelCommand::RenameTab(tab_id) => {
                            self.begin_tab_rename(tab_id);
                        },
                        crate::tab_panel::TabPanelCommand::RenameGroup(group_id) => {
                            self.begin_group_rename(group_id);
                        },
                    }
                }

                if update.capture {
                    if update.needs_redraw {
                        self.dirty = true;
                        if self.display.window.has_frame {
                            self.display.window.request_redraw();
                        }
                    }
                    return true;
                }

                false
            },
            WinitEvent::WindowEvent {
                event: WindowEvent::KeyboardInput { event, is_synthetic: false, .. },
                ..
            } => {
                if !self.display.tab_panel.is_editing() {
                    return false;
                }

                let outcome = self.display.tab_panel.handle_key_event(event);
                let needs_redraw = self.apply_tab_panel_edit_outcome(outcome);
                if needs_redraw {
                    self.dirty = true;
                    if self.display.window.has_frame {
                        self.display.window.request_redraw();
                    }
                }
                true
            },
            WinitEvent::WindowEvent { event: WindowEvent::Ime(ime), .. } => {
                if !self.display.tab_panel.is_editing() {
                    return false;
                }

                let outcome = match ime {
                    Ime::Commit(text) => self.display.tab_panel.handle_ime_commit(text),
                    Ime::Preedit(_, _) | Ime::Enabled | Ime::Disabled => TabPanelEditOutcome::None,
                };
                let needs_redraw = self.apply_tab_panel_edit_outcome(outcome);
                if needs_redraw {
                    self.dirty = true;
                    if self.display.window.has_frame {
                        self.display.window.request_redraw();
                    }
                }
                true
            },
            WinitEvent::WindowEvent {
                event: WindowEvent::MouseWheel { .. },
                ..
            } => self.display.tab_panel.should_capture_last(),
            _ => false,
        }
    }

    #[cfg(target_os = "macos")]
    fn apply_tab_panel_edit_outcome(&mut self, outcome: TabPanelEditOutcome) -> bool {
        match outcome {
            TabPanelEditOutcome::None => false,
            TabPanelEditOutcome::Changed | TabPanelEditOutcome::Cancelled => true,
            TabPanelEditOutcome::Commit(commit) => {
                let trimmed = commit.text.trim();
                let name = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };

                match commit.target {
                    TabPanelEditTarget::Tab(tab_id) => self.rename_tab(tab_id, name),
                    TabPanelEditTarget::Group(group_id) => self.rename_group(group_id, name),
                }

                true
            },
        }
    }

    #[cfg(target_os = "macos")]
    fn handle_web_command_event(
        &mut self,
        event: &Event,
        command: &WebCommand,
        clipboard: &mut Clipboard,
        event_proxy: &EventLoopProxy<Event>,
    ) {
        match command {
            WebCommand::CopyToClipboard { text } => {
                if !text.is_empty() {
                    clipboard.store(tabor_terminal::term::ClipboardType::Clipboard, text.clone());
                }
                if let Some(tab_id) = event.tab_id().or(self.tabs.active_id()) {
                    if let Some(tab) = self.tabs.get_mut(tab_id) {
                        tab.web_command_state.reset_mode();
                    }
                }
            },
            WebCommand::OpenUrl { url, new_tab } => {
                if *new_tab {
                    if let Err(err) = self.open_web_url_new_tab(url.clone(), event_proxy) {
                        self.message_buffer.push(crate::message_bar::Message::new(
                            format!("Failed to open URL: {err}"),
                            crate::message_bar::MessageType::Error,
                        ));
                        self.display.pending_update.dirty = true;
                    }
                    return;
                }

                let Some(tab_id) = event.tab_id().or(self.tabs.active_id()) else {
                    return;
                };

                if let Err(message) = self.open_web_url_in_tab(tab_id, url.clone()) {
                    self.message_buffer.push(crate::message_bar::Message::new(
                        message,
                        crate::message_bar::MessageType::Error,
                    ));
                    self.display.pending_update.dirty = true;
                }
                if let Some(tab) = self.tabs.get_mut(tab_id) {
                    tab.web_command_state.reset_mode();
                }
            },
            WebCommand::SetMark {
                name,
                url,
                scroll_x,
                scroll_y,
            } => {
                let Some(tab_id) = event.tab_id().or(self.tabs.active_id()) else {
                    return;
                };
                if let Some(tab) = self.tabs.get_mut(tab_id) {
                    tab.web_command_state
                        .set_mark(*name, url.clone(), *scroll_x, *scroll_y);
                }
            },
        }
    }

    fn handle_inactive_terminal_event(
        &mut self,
        tab_id: TabId,
        event: &TerminalEvent,
        clipboard: &mut Clipboard,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };

        if tab.kind.is_web() {
            return;
        }

        match event {
            TerminalEvent::ClipboardStore(clipboard_type, content) => {
                if tab.terminal.lock().is_focused {
                    clipboard.store(*clipboard_type, content.clone());
                }
            },
            TerminalEvent::ClipboardLoad(clipboard_type, format) => {
                if tab.terminal.lock().is_focused {
                    let text = format(clipboard.load(*clipboard_type).as_str());
                    tab.notifier.notify(text.into_bytes());
                }
            },
            TerminalEvent::ColorRequest(index, format) => {
                let terminal = tab.terminal.lock();
                let color = match terminal.colors()[*index] {
                    Some(color) => Rgb(color),
                    None if *index == NamedColor::Cursor as usize => return,
                    None => self.display.colors[*index],
                };
                tab.notifier.notify(format(color.0).into_bytes());
            },
            TerminalEvent::TextAreaSizeRequest(format) => {
                let text = format(self.display.size_info.into());
                tab.notifier.notify(text.into_bytes());
            },
            TerminalEvent::PtyWrite(text) => {
                tab.notifier.notify(text.clone().into_bytes());
            },
            _ => (),
        }
    }

    /// ID of this terminal context.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// Write the ref test results to the disk.
    pub fn write_ref_test_results(&self) {
        let Some(tab) = self.tabs.active() else {
            return;
        };

        // Dump grid state.
        let mut grid = tab.terminal.lock().grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");

        let size_info = &self.display.size_info;
        let size = TermSize::new(size_info.columns(), size_info.screen_lines());
        let serialized_size = json::to_string(&size).expect("serialize size");

        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");

        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");

        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }

    /// Submit the pending changes to the `Display`.
    fn submit_display_update(
        active_id: TabId,
        tabs: &mut TabManager,
        display: &mut Display,
        message_buffer: &MessageBuffer,
        old_is_searching: bool,
        config: &UiConfig,
    ) {
        {
            let Some(active_tab) = tabs.get_mut(active_id) else {
                return;
            };

            let mut terminal = active_tab.terminal.lock();
            let web_status_bar = active_tab.kind.is_web();

            // Compute cursor positions before resize.
            let num_lines = terminal.screen_lines();
            let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
            let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
                terminal.vi_mode_cursor.point.line == num_lines - 1
            } else {
                active_tab.search_state.direction == Direction::Left
            };

            display.handle_update(
                &mut terminal,
                &mut active_tab.notifier,
                message_buffer,
                &mut active_tab.search_state,
                web_status_bar,
                config,
            );

            let new_is_searching = active_tab.search_state.history_index.is_some();
            if !old_is_searching && new_is_searching {
                // Scroll on search start to make sure origin is visible with minimal viewport motion.
                let display_offset = terminal.grid().display_offset();
                if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                    terminal.scroll_display(Scroll::Delta(1));
                } else if display_offset != 0 && origin_at_bottom {
                    terminal.scroll_display(Scroll::Delta(-1));
                }
            }
        }

        #[cfg(target_os = "macos")]
        for tab in tabs.iter_mut() {
            if let Some(web_view) = tab.web_view.as_mut() {
                web_view.update_frame(&display.window, &display.size_info);
            }
        }

        let new_size = display.size_info;
        for tab in tabs.iter_mut() {
            if tab.id == active_id {
                continue;
            }

            let mut tab_terminal = tab.terminal.lock();
            if tab_terminal.screen_lines() != new_size.screen_lines()
                || tab_terminal.columns() != new_size.columns()
            {
                tab.notifier.on_resize(new_size.into());
                tab_terminal.resize(new_size);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn map_inspector_error(error: InspectorError) -> IpcError {
    match error {
        InspectorError::PermissionDenied => {
            IpcError::new(IpcErrorCode::PermissionDenied, "Inspector permission denied")
        },
        InspectorError::ConnectionFailed(message) => {
            IpcError::new(IpcErrorCode::Internal, message)
        },
        InspectorError::Timeout => {
            IpcError::new(IpcErrorCode::Timeout, "Inspector request timed out")
        },
        InspectorError::NotFound(message) => IpcError::new(IpcErrorCode::NotFound, message),
        InspectorError::Ambiguous(message) => IpcError::new(IpcErrorCode::Ambiguous, message),
        InspectorError::InvalidMessage(message) => {
            IpcError::new(IpcErrorCode::InvalidRequest, message)
        },
    }
}

impl Drop for WindowContext {
    fn drop(&mut self) {
        // Shutdown each tab's PTY.
        for tab in self.tabs.iter_mut() {
            let _ = tab.notifier.0.send(Msg::Shutdown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_mode_selects_web() {
        let mode = draw_mode(&WindowKind::Web { url: String::from("about:blank") });
        assert_eq!(mode, DrawMode::Web);
    }

    #[test]
    fn draw_mode_selects_terminal() {
        let mode = draw_mode(&WindowKind::Terminal);
        assert_eq!(mode, DrawMode::Terminal);
    }
}
