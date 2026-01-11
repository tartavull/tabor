//! Process window events.

use crate::ConfigMonitor;
use glutin::config::GetGlConfig;
use std::borrow::Cow;
use std::cmp::min;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::ffi::OsStr;
use std::fmt::Debug;
#[cfg(not(windows))]
use std::os::unix::io::RawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
#[cfg(unix)]
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, f32, mem};

use ahash::RandomState;
use crossfont::Size as FontSize;
use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
use log::{debug, error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{
    ElementState, Event as WinitEvent, Ime, KeyEvent, Modifiers, MouseButton, StartCause,
    Touch as TouchEvent, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop, EventLoopProxy};
#[cfg(target_os = "macos")]
use winit::platform::macos::ActiveEventLoopExtMacOS;
use winit::raw_window_handle::HasDisplayHandle;
use winit::keyboard::{Key, ModifiersState, NamedKey};
#[cfg(target_os = "macos")]
use winit::window::CursorIcon;
use winit::window::WindowId;

use tabor_terminal::event::{Event as TerminalEvent, EventListener, Notify};
use tabor_terminal::event_loop::Notifier;
use tabor_terminal::grid::{BidirectionalIterator, Dimensions, Scroll};
use tabor_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use tabor_terminal::selection::{Selection, SelectionType};
use tabor_terminal::term::cell::Flags;
use tabor_terminal::term::search::{Match, RegexSearch};
use tabor_terminal::term::{self, ClipboardType, Term, TermMode};
use tabor_terminal::vte::ansi::NamedColor;

#[cfg(unix)]
use crate::cli::ParsedOptions;
use crate::cli::{Options as CliOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::Action;
use crate::config::ui_config::{HintAction, HintInternalAction};
use crate::config::{self, UiConfig};
#[cfg(not(windows))]
use crate::daemon::foreground_process_path;
use crate::daemon::spawn_daemon;
use crate::display::color::Rgb;
use crate::display::hint::HintMatch;
use crate::display::window::{ImeInhibitor, Window};
use crate::display::{Display, Preedit, SizeInfo};
use crate::input::{self, ActionContext as _, FONT_SIZE_STEP};
#[cfg(unix)]
use crate::ipc::{self, IpcRequest, SocketReply};
use crate::logging::{LOG_TARGET_CONFIG, LOG_TARGET_WINIT};
use crate::message_bar::{Message, MessageBuffer};
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::tab_panel::TAB_ACTIVITY_TICK_INTERVAL;
use crate::tabs::{TabCommand, TabId};
use crate::web_url::normalize_web_url;
use crate::window_kind::WindowKind;
use crate::window_context::WindowContext;
#[cfg(target_os = "macos")]
use objc2_app_kit::NSEventModifierFlags;
#[cfg(target_os = "macos")]
use crate::macos::web_commands::{self, WebActions, WebCommandState, WebHintAction, WebKey};
#[cfg(target_os = "macos")]
use crate::macos::web_cursor::{web_cursor_from_css, web_cursor_script, WEB_CURSOR_BOOTSTRAP};
#[cfg(target_os = "macos")]
use crate::macos::favicon::FaviconImage;
#[cfg(target_os = "macos")]
use crate::macos::webview::WebView;
#[cfg(target_os = "macos")]
use url::Url;

/// Duration after the last user input until an unlimited search is performed.
pub const TYPING_SEARCH_DELAY: Duration = Duration::from_millis(500);

/// Minimum delay between foreground process name refreshes.
const FOREGROUND_PROCESS_REFRESH: Duration = Duration::from_millis(500);

#[cfg(target_os = "macos")]
const WEB_HINTS_BOOTSTRAP: &str = r##"
(function() {
  if (window.__taborHints) {
    return;
  }
  const alphabet = "asdfghjklqwertyuiopzxcvbnm";
  function makeLabel(index) {
    const base = alphabet.length;
    let label = "";
    while (true) {
      label = alphabet[index % base] + label;
      index = Math.floor(index / base) - 1;
      if (index < 0) {
        break;
      }
    }
    return label;
  }
  function isVisible(el) {
    const rect = el.getBoundingClientRect();
    if (!rect || rect.width === 0 || rect.height === 0) return false;
    const style = window.getComputedStyle(el);
    if (style.visibility === "hidden" || style.display === "none") return false;
    return rect.bottom >= 0 && rect.right >= 0 &&
      rect.top <= window.innerHeight && rect.left <= window.innerWidth;
  }
  function clearState() {
    if (window.__taborHintsState && window.__taborHintsState.container) {
      window.__taborHintsState.container.remove();
    }
    window.__taborHintsState = null;
  }
  function start() {
    clearState();
    const links = Array.from(document.querySelectorAll("a[href]"));
    const container = document.createElement("div");
    container.id = "__tabor_hint_container";
    container.style.position = "absolute";
    container.style.top = "0";
    container.style.left = "0";
    container.style.zIndex = "2147483647";
    container.style.pointerEvents = "none";
    const hints = [];
    let index = 0;
    for (const el of links) {
      if (!isVisible(el)) continue;
      const rect = el.getBoundingClientRect();
      const label = makeLabel(index++);
      const marker = document.createElement("div");
      marker.textContent = label;
      marker.style.position = "absolute";
      marker.style.left = (window.scrollX + rect.left) + "px";
      marker.style.top = (window.scrollY + rect.top) + "px";
      marker.style.background = "#ffd24d";
      marker.style.color = "#000";
      marker.style.fontSize = "12px";
      marker.style.fontFamily = "Menlo, Monaco, monospace";
      marker.style.padding = "1px 2px";
      marker.style.borderRadius = "2px";
      marker.style.boxShadow = "0 1px 2px rgba(0,0,0,0.35)";
      container.appendChild(marker);
      hints.push({ label: label, href: el.href, marker: marker });
    }
    document.body.appendChild(container);
    window.__taborHintsState = { container: container, hints: hints };
    return hints.length;
  }
  function update(keys) {
    const state = window.__taborHintsState;
    if (!state) return "";
    let matched = null;
    for (const hint of state.hints) {
      if (hint.label.indexOf(keys) === 0) {
        hint.marker.style.display = "block";
        if (hint.label === keys) {
          matched = hint;
        }
      } else {
        hint.marker.style.display = "none";
      }
    }
    if (matched) {
      clearState();
      return matched.href || "";
    }
    return "";
  }
  function cancel() {
    clearState();
  }
  window.__taborHints = { start: start, update: update, cancel: cancel };
})();
"##;

#[cfg(target_os = "macos")]
const WEB_HELP_HTML: &str = r#"<pre style="margin:0;font-family:Menlo,Monaco,monospace;font-size:12px;line-height:1.4;">
Navigation:
  j/k/h/l    scroll
  d/u        half page
  gg/G       top/bottom
  zH/zL      far left/right
Links & inputs:
  f/F        open link / open in new tab
  yf         copy link URL
  gi         focus input (insert mode)
Find & visual:
  /          find
  n/N        next/previous match
  v/V        visual/visual line
  y          copy selection (visual)
History & URL:
  H/L        back/forward
  yy         copy URL
  p/P        open clipboard URL / new tab
  gu/gU      up one level / root
Tabs & omnibar:
  t          new tab
  x/X        close/restore tab
  J/K        prev/next tab
  g0/g$      first/last tab
  o/O        omnibar / new tab
  b/B        bookmarks / new tab
  T          tab search
Misc:
  r          reload
  gs         view source
  [[/]]      previous/next link
  m/`        set/jump mark
  ?          help
</pre>"#;

#[cfg(target_os = "macos")]
const WEB_CURSOR_THROTTLE: Duration = Duration::from_millis(100);

/// Maximum number of lines for the blocking search while still typing the search regex.
const MAX_SEARCH_WHILE_TYPING: Option<usize> = Some(1000);

/// Maximum number of search terms stored in the history.
const MAX_SEARCH_HISTORY_SIZE: usize = 255;

/// Touch zoom speed.
const TOUCH_ZOOM_FACTOR: f32 = 0.01;

/// Cooldown between invocations of the bell command.
const BELL_CMD_COOLDOWN: Duration = Duration::from_millis(100);

/// The event processor.
///
/// Stores some state from received events and dispatches actions when they are
/// triggered.
pub struct Processor {
    pub config_monitor: Option<ConfigMonitor>,

    clipboard: Clipboard,
    scheduler: Scheduler,
    initial_window_options: Option<WindowOptions>,
    initial_window_error: Option<Box<dyn Error>>,
    windows: HashMap<WindowId, WindowContext, RandomState>,
    proxy: EventLoopProxy<Event>,
    gl_config: Option<GlutinConfig>,
    #[cfg(unix)]
    global_ipc_options: ParsedOptions,
    cli_options: CliOptions,
    config: Rc<UiConfig>,
}

#[cfg(unix)]
struct IpcWindowContext<'a> {
    window: &'a mut WindowContext,
    event_loop: &'a ActiveEventLoop,
    event_proxy: &'a EventLoopProxy<Event>,
    clipboard: &'a mut Clipboard,
    scheduler: &'a mut Scheduler,
}

#[cfg(unix)]
impl ipc::IpcContext for IpcWindowContext<'_> {
    fn active_tab_id(&self) -> Option<TabId> {
        self.window.active_tab_id()
    }

    fn list_tabs(&self, now: Instant) -> Vec<ipc::IpcTabGroup> {
        self.window.ipc_tab_groups(now)
    }

    fn tab_state(&self, tab_id: TabId, now: Instant) -> Option<ipc::IpcTabState> {
        self.window.ipc_tab_state(tab_id, now)
    }

    fn tab_kind(&self, tab_id: TabId) -> Option<ipc::IpcTabKind> {
        self.window.ipc_tab_kind(tab_id)
    }

    fn create_tab(&mut self, options: WindowOptions) -> Result<TabId, ipc::IpcError> {
        self.window.ipc_create_tab(options, self.event_proxy)
    }

    fn close_tab(&mut self, tab_id: TabId) -> Result<bool, ipc::IpcError> {
        self.window.ipc_close_tab(tab_id)
    }

    fn select_tab(&mut self, selection: ipc::TabSelection) -> Result<(), ipc::IpcError> {
        self.window.ipc_select_tab(selection)
    }

    fn move_tab(
        &mut self,
        tab_id: TabId,
        target_group_id: Option<usize>,
        target_index: Option<usize>,
    ) -> Result<(), ipc::IpcError> {
        self.window
            .ipc_move_tab(tab_id, target_group_id, target_index)
    }

    fn set_tab_title(&mut self, tab_id: TabId, title: Option<String>) -> Result<(), ipc::IpcError> {
        self.window.ipc_set_tab_title(tab_id, title)
    }

    fn set_group_name(&mut self, group_id: usize, name: Option<String>) -> Result<(), ipc::IpcError> {
        self.window.ipc_set_group_name(group_id, name)
    }

    fn restore_closed_tab(&mut self) -> Result<(), ipc::IpcError> {
        self.window.ipc_restore_closed_tab(self.event_proxy)
    }

    fn open_url_in_tab(&mut self, tab_id: TabId, url: String) -> Result<(), ipc::IpcError> {
        self.window.ipc_open_url_in_tab(tab_id, url, self.event_proxy)
    }

    fn open_url_new_tab(&mut self, url: String) -> Result<TabId, ipc::IpcError> {
        self.window.ipc_open_url_new_tab(url, self.event_proxy)
    }

    fn reload_web(&mut self, tab_id: TabId) -> Result<(), ipc::IpcError> {
        self.window
            .ipc_reload_web(tab_id, self.event_loop, self.event_proxy, self.clipboard, self.scheduler)
    }

    fn open_inspector(&mut self, tab_id: TabId) -> Result<(), ipc::IpcError> {
        self.window.ipc_open_inspector(
            tab_id,
            self.event_loop,
            self.event_proxy,
            self.clipboard,
            self.scheduler,
        )
    }

    fn tab_panel_state(&self) -> ipc::IpcTabPanelState {
        self.window.ipc_tab_panel_state()
    }

    fn set_tab_panel(&mut self, enabled: Option<bool>, width: Option<usize>) -> Result<(), ipc::IpcError> {
        self.window.ipc_set_tab_panel(enabled, width)
    }

    fn dispatch_action(&mut self, tab_id: TabId, action: Action) -> Result<(), ipc::IpcError> {
        self.window.ipc_dispatch_action(
            tab_id,
            action,
            self.event_loop,
            self.event_proxy,
            self.clipboard,
            self.scheduler,
        )
    }

    fn send_input(&mut self, tab_id: TabId, text: String) -> Result<(), ipc::IpcError> {
        self.window.ipc_send_input(tab_id, text)
    }

    fn run_command_bar(&mut self, tab_id: TabId, input: String) -> Result<(), ipc::IpcError> {
        self.window.ipc_run_command_bar(
            tab_id,
            input,
            self.event_loop,
            self.event_proxy,
            self.clipboard,
            self.scheduler,
        )
    }

    fn list_inspector_targets(&mut self) -> Result<Vec<ipc::IpcInspectorTarget>, ipc::IpcError> {
        self.window.ipc_list_inspector_targets()
    }

    fn attach_inspector(
        &mut self,
        tab_id: Option<TabId>,
        target_id: Option<u64>,
    ) -> Result<ipc::IpcInspectorSession, ipc::IpcError> {
        self.window.ipc_attach_inspector(tab_id, target_id)
    }

    fn detach_inspector(&mut self, session_id: String) -> Result<(), ipc::IpcError> {
        self.window.ipc_detach_inspector(session_id)
    }

    fn send_inspector_message(
        &mut self,
        session_id: String,
        message: String,
    ) -> Result<(), ipc::IpcError> {
        self.window.ipc_send_inspector_message(session_id, message)
    }

    fn poll_inspector_messages(
        &mut self,
        session_id: String,
        max: Option<usize>,
    ) -> Result<Vec<ipc::IpcInspectorMessage>, ipc::IpcError> {
        self.window.ipc_poll_inspector_messages(session_id, max)
    }
}

impl Processor {
    /// Create a new event processor.
    pub fn new(
        config: UiConfig,
        cli_options: CliOptions,
        event_loop: &EventLoop<Event>,
    ) -> Processor {
        let proxy = event_loop.create_proxy();
        let scheduler = Scheduler::new(proxy.clone());
        let initial_window_options = Some(cli_options.window_options.clone());

        // Disable all device events, since we don't care about them.
        event_loop.listen_device_events(DeviceEvents::Never);

        // SAFETY: Since this takes a pointer to the winit event loop, it MUST be dropped first,
        // which is done in `loop_exiting`.
        let clipboard = unsafe { Clipboard::new(event_loop.display_handle().unwrap().as_raw()) };

        // Create a config monitor.
        //
        // The monitor watches the config file for changes and reloads it. Pending
        // config changes are processed in the main loop.
        let mut config_monitor = None;
        if config.live_config_reload() {
            config_monitor =
                ConfigMonitor::new(config.config_paths.clone(), event_loop.create_proxy());
        }

        Processor {
            initial_window_options,
            initial_window_error: None,
            cli_options,
            proxy,
            scheduler,
            gl_config: None,
            config: Rc::new(config),
            clipboard,
            windows: Default::default(),
            #[cfg(unix)]
            global_ipc_options: Default::default(),
            config_monitor,
        }
    }

    /// Create initial window and load GL platform.
    ///
    /// This will initialize the OpenGL Api and pick a config that
    /// will be used for the rest of the windows.
    pub fn create_initial_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_options: WindowOptions,
    ) -> Result<(), Box<dyn Error>> {
        let window_context = WindowContext::initial(
            event_loop,
            self.proxy.clone(),
            self.config.clone(),
            window_options,
        )?;

        self.gl_config = Some(window_context.display.gl_context().config());
        let window_id = window_context.id();
        self.windows.insert(window_id, window_context);

        Ok(())
    }

    /// Create a new terminal window.
    pub fn create_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        options: WindowOptions,
    ) -> Result<(), Box<dyn Error>> {
        let gl_config = self.gl_config.as_ref().unwrap();

        // Override config with CLI/IPC options.
        let mut config_overrides = options.config_overrides();
        #[cfg(unix)]
        config_overrides.extend_from_slice(&self.global_ipc_options);
        let mut config = self.config.clone();
        config = config_overrides.override_config_rc(config);

        let window_context = WindowContext::additional(
            gl_config,
            event_loop,
            self.proxy.clone(),
            config,
            options,
            config_overrides,
        )?;

        let window_id = window_context.id();
        self.windows.insert(window_id, window_context);
        Ok(())
    }

    fn ensure_tab_activity_tick(&mut self, window_id: WindowId) {
        let timer_id = TimerId::new(Topic::TabActivityTick, window_id);
        if self.scheduler.scheduled(timer_id) {
            return;
        }

        let event = Event::new(EventType::TabActivityTick, window_id);
        self.scheduler.schedule(event, TAB_ACTIVITY_TICK_INTERVAL, true, timer_id);
    }

    /// Run the event loop.
    ///
    /// The result is exit code generate from the loop.
    pub fn run(&mut self, event_loop: EventLoop<Event>) -> Result<(), Box<dyn Error>> {
        let result = event_loop.run_app(self);
        match self.initial_window_error.take() {
            Some(initial_window_error) => Err(initial_window_error),
            _ => result.map_err(Into::into),
        }
    }

    #[cfg(unix)]
    fn handle_ipc_request(
        &mut self,
        event_loop: &ActiveEventLoop,
        request: IpcRequest,
    ) -> SocketReply {
        match request {
            IpcRequest::SetConfig(ipc_config) => {
                let window_id = ipc_config
                    .window_id
                    .and_then(|id| u64::try_from(id).ok())
                    .map(WindowId::from);

                let mut options = ParsedOptions::from_options(&ipc_config.options);

                for (_, window_context) in self
                    .windows
                    .iter_mut()
                    .filter(|(id, _)| window_id.is_none() || window_id == Some(**id))
                {
                    if ipc_config.reset {
                        window_context.reset_window_config(self.config.clone());
                    } else {
                        window_context.add_window_config(self.config.clone(), &options);
                    }
                }

                if window_id.is_none() {
                    if ipc_config.reset {
                        self.global_ipc_options.clear();
                    } else {
                        self.global_ipc_options.append(&mut options);
                    }
                }

                ipc::reply_ok()
            },
            IpcRequest::GetConfig(ipc_config) => {
                let window_id = ipc_config
                    .window_id
                    .and_then(|id| u64::try_from(id).ok())
                    .map(WindowId::from);

                let config = match self.windows.iter().find(|(id, _)| window_id == Some(**id)) {
                    Some((_, window_context)) => window_context.config(),
                    None => &self.global_ipc_options.override_config_rc(self.config.clone()),
                };

                match serde_json::to_value(config) {
                    Ok(config_json) => SocketReply::Config { config: config_json },
                    Err(err) => ipc::reply_error(
                        ipc::IpcErrorCode::Internal,
                        format!("Failed config serialization: {err}"),
                    ),
                }
            },
            request => {
                let window_id = match self.window_for_ipc_request(&request) {
                    Ok(window_id) => window_id,
                    Err(reply) => return reply,
                };

                let window_context = match self.windows.get_mut(&window_id) {
                    Some(window_context) => window_context,
                    None => {
                        return ipc::reply_error(
                            ipc::IpcErrorCode::NotFound,
                            "Target window not found",
                        );
                    },
                };

                let mut ipc_context = IpcWindowContext {
                    window: window_context,
                    event_loop,
                    event_proxy: &self.proxy,
                    clipboard: &mut self.clipboard,
                    scheduler: &mut self.scheduler,
                };

                let response = ipc::handle_request(&mut ipc_context, request);
                if response.close_window {
                    self.close_window(event_loop, window_id);
                }

                response.reply
            },
        }
    }

    #[cfg(unix)]
    fn window_for_ipc_request(&self, request: &IpcRequest) -> Result<WindowId, SocketReply> {
        if let Some(tab_id) = request.target_tab_id() {
            let tab_id: TabId = tab_id.into();
            let mut matches = self
                .windows
                .iter()
                .filter_map(|(id, window)| window.has_tab(tab_id).then_some(*id))
                .collect::<Vec<_>>();

            matches.sort();
            matches.dedup();

            return match matches.len() {
                0 => Err(ipc::reply_error(ipc::IpcErrorCode::NotFound, "Tab not found")),
                1 => Ok(matches[0]),
                _ => Err(ipc::reply_error(
                    ipc::IpcErrorCode::Ambiguous,
                    "Tab exists in multiple windows",
                )),
            };
        }

        if let Some(session_id) = request.target_inspector_session_id() {
            let mut matches = self
                .windows
                .iter()
                .filter_map(|(id, window)| window.has_inspector_session(session_id).then_some(*id))
                .collect::<Vec<_>>();

            matches.sort();
            matches.dedup();

            return match matches.len() {
                0 => Err(ipc::reply_error(
                    ipc::IpcErrorCode::NotFound,
                    "Inspector session not found",
                )),
                1 => Ok(matches[0]),
                _ => Err(ipc::reply_error(
                    ipc::IpcErrorCode::Ambiguous,
                    "Inspector session exists in multiple windows",
                )),
            };
        }

        let focused = self
            .windows
            .iter()
            .find_map(|(id, window)| window.is_focused().then_some(*id));
        if let Some(focused) = focused {
            return Ok(focused);
        }

        if self.windows.len() == 1 {
            return Ok(*self.windows.keys().next().unwrap());
        }

        Err(ipc::reply_error(
            ipc::IpcErrorCode::NotFound,
            "No focused window",
        ))
    }

    fn close_window(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId) {
        let window_context = match self.windows.entry(window_id) {
            Entry::Occupied(window_context) => window_context.remove(),
            _ => return,
        };

        self.scheduler.unschedule_window(window_context.id());

        if self.windows.is_empty() && !self.cli_options.daemon {
            if self.config.debug.ref_test {
                window_context.write_ref_test_results();
            }

            event_loop.exit();
        }
    }

    /// Check if an event is irrelevant and can be skipped.
    fn skip_window_event(event: &WindowEvent) -> bool {
        matches!(
            event,
            WindowEvent::KeyboardInput { is_synthetic: true, .. }
                | WindowEvent::ActivationTokenDone { .. }
                | WindowEvent::DoubleTapGesture { .. }
                | WindowEvent::TouchpadPressure { .. }
                | WindowEvent::RotationGesture { .. }
                | WindowEvent::CursorEntered { .. }
                | WindowEvent::PinchGesture { .. }
                | WindowEvent::AxisMotion { .. }
                | WindowEvent::PanGesture { .. }
                | WindowEvent::HoveredFileCancelled
                | WindowEvent::Destroyed
                | WindowEvent::ThemeChanged(_)
                | WindowEvent::HoveredFile(_)
                | WindowEvent::Moved(_)
        )
    }
}

impl ApplicationHandler<Event> for Processor {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        #[cfg(target_os = "macos")]
        if cause == StartCause::Init {
            event_loop.set_allows_automatic_window_tabbing(false);
        }

        if cause != StartCause::Init || self.cli_options.daemon {
            return;
        }

        if let Some(window_options) = self.initial_window_options.take() {
            if let Err(err) = self.create_initial_window(event_loop, window_options) {
                self.initial_window_error = Some(err);
                event_loop.exit();
                return;
            }
        }

        info!("Initialisation complete");
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.config.debug.print_events {
            info!(target: LOG_TARGET_WINIT, "{event:?}");
        }

        // Ignore all events we do not care about.
        if Self::skip_window_event(&event) {
            return;
        }

        let window_context = match self.windows.get_mut(&window_id) {
            Some(window_context) => window_context,
            None => return,
        };

        let is_redraw = matches!(event, WindowEvent::RedrawRequested);

        window_context.handle_event(
            #[cfg(target_os = "macos")]
            _event_loop,
            &self.proxy,
            &mut self.clipboard,
            &mut self.scheduler,
            WinitEvent::WindowEvent { window_id, event },
        );

        if is_redraw {
            window_context.draw(&mut self.scheduler);
        }

    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        if self.config.debug.print_events {
            info!(target: LOG_TARGET_WINIT, "{event:?}");
        }

        let Event { window_id, tab_id, payload } = event;

        // Handle events which don't mandate the WindowId.
        match (payload, window_id) {
            #[cfg(unix)]
            (EventType::IpcRequest(request, stream), _) => {
                let reply = self.handle_ipc_request(event_loop, request);
                if let Ok(mut stream) = stream.try_clone() {
                    ipc::send_reply(&mut stream, reply);
                }
            },
            (EventType::CreateTab(options), Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(&window_id) {
                    if let Err(err) = window_context.create_tab(options, &self.proxy) {
                        error!("Could not create tab: {err:?}");
                    }
                }
            },
            (EventType::TabCommand(command), Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(&window_id) {
                    window_context.handle_tab_command(command);
                }
            },
            #[cfg(target_os = "macos")]
            (EventType::CloseTab(tab_id), Some(window_id)) => {
                let Some(window_context) = self.windows.get_mut(&window_id) else {
                    return;
                };

                let should_close_window = window_context.close_tab(tab_id);

                if should_close_window {
                    self.close_window(event_loop, window_id);
                }
            },
            #[cfg(target_os = "macos")]
            (EventType::RestoreTab, Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(&window_id) {
                    if let Err(err) = window_context.restore_closed_tab(&self.proxy) {
                        error!("Could not restore tab: {err:?}");
                    }
                }
            },
            #[cfg(target_os = "macos")]
            (EventType::TabSearch(query), Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(&window_id) {
                    window_context.select_tab_by_query(&query);
                }
            },
            (EventType::ConfigReload(path), _) => {
                // Clear config logs from message bar for all terminals.
                for window_context in self.windows.values_mut() {
                    if !window_context.message_buffer.is_empty() {
                        window_context.message_buffer.remove_target(LOG_TARGET_CONFIG);
                        window_context.display.pending_update.dirty = true;
                    }
                }

                // Load config and update each terminal.
                if let Ok(config) = config::reload(&path, &mut self.cli_options) {
                    self.config = Rc::new(config);

                    // Restart config monitor if imports changed.
                    if let Some(monitor) = self.config_monitor.take() {
                        let paths = &self.config.config_paths;
                        self.config_monitor = if monitor.needs_restart(paths) {
                            monitor.shutdown();
                            ConfigMonitor::new(paths.clone(), self.proxy.clone())
                        } else {
                            Some(monitor)
                        };
                    }

                    for window_context in self.windows.values_mut() {
                        window_context.update_config(self.config.clone());
                    }
                }
            },
            // Create a new terminal window.
            (EventType::CreateWindow(options), _) => {
                // XXX Ensure that no context is current when creating a new window,
                // otherwise it may lock the backing buffer of the
                // surface of current context when asking
                // e.g. EGL on Wayland to create a new context.
                for window_context in self.windows.values_mut() {
                    window_context.display.make_not_current();
                }

                if self.gl_config.is_none() {
                    // Handle initial window creation in daemon mode.
                    if let Err(err) = self.create_initial_window(event_loop, options) {
                        self.initial_window_error = Some(err);
                        event_loop.exit();
                    }
                } else if let Err(err) = self.create_window(event_loop, options) {
                    error!("Could not open window: {err:?}");
                }
            },
            // Process events affecting all windows.
            (payload, None) => {
                let event = WinitEvent::UserEvent(Event { window_id: None, tab_id, payload });
                for window_context in self.windows.values_mut() {
                    window_context.handle_event(
                        #[cfg(target_os = "macos")]
                        event_loop,
                        &self.proxy,
                        &mut self.clipboard,
                        &mut self.scheduler,
                        event.clone(),
                    );
                }
            },
            (EventType::Terminal(TerminalEvent::Wakeup), Some(window_id)) => {
                let mut schedule_tick = false;
                if let Some(window_context) = self.windows.get_mut(&window_id) {
                    let is_web = tab_id
                        .and_then(|id| window_context.tab_kind(id))
                        .is_some_and(WindowKind::is_web);
                    let is_active =
                        tab_id.is_some_and(|id| Some(id) == window_context.active_tab_id());
                    if !is_web {
                        if let Some(tab_id) = tab_id {
                            window_context.note_terminal_output(tab_id, is_active);
                        }
                        if window_context.tab_panel_enabled()
                            && window_context.has_active_terminal_output(Instant::now())
                        {
                            schedule_tick = true;
                        }
                    }
                    if !is_web && (tab_id.is_none() || is_active) {
                        window_context.dirty = true;
                        if window_context.display.window.has_frame {
                            window_context.display.window.request_redraw();
                        }
                    }

                    if !is_web && is_active {
                        let timer_id = TimerId::new(Topic::ForegroundProcess, window_id);
                        if !self.scheduler.scheduled(timer_id) {
                            if let Some(tab_id) = tab_id {
                                let event = Event::for_tab(
                                    EventType::UpdateTabProgramName,
                                    window_id,
                                    tab_id,
                                );
                                self.scheduler.schedule(
                                    event,
                                    FOREGROUND_PROCESS_REFRESH,
                                    false,
                                    timer_id,
                                );
                            }
                        }
                    }
                }
                if schedule_tick {
                    self.ensure_tab_activity_tick(window_id);
                }
            },
            (EventType::Terminal(TerminalEvent::Exit | TerminalEvent::ChildExit(_)), Some(window_id)) => {
                let Some(tab_id) = tab_id else {
                    return;
                };

                let Some(window_context) = self.windows.get_mut(&window_id) else {
                    return;
                };

                if window_context
                    .tab_kind(tab_id)
                    .is_some_and(WindowKind::is_web)
                {
                    return;
                }

                if window_context.display.window.hold {
                    return;
                }

                let should_close_window = window_context.close_tab(tab_id);

                if should_close_window {
                    let window_context = match self.windows.entry(window_id) {
                        Entry::Occupied(window_context) => window_context.remove(),
                        _ => return,
                    };

                    // Unschedule pending events.
                    self.scheduler.unschedule_window(window_context.id());

                    if self.windows.is_empty() && !self.cli_options.daemon {
                        if self.config.debug.ref_test {
                            window_context.write_ref_test_results();
                        }

                        event_loop.exit();
                    }
                }
            },
            (EventType::TabActivityTick, Some(window_id)) => {
                let Some(window_context) = self.windows.get_mut(&window_id) else {
                    return;
                };

                let timer_id = TimerId::new(Topic::TabActivityTick, window_id);
                if !window_context.tab_panel_enabled() {
                    self.scheduler.unschedule(timer_id);
                    return;
                }

                if !window_context.has_active_terminal_output(Instant::now()) {
                    self.scheduler.unschedule(timer_id);
                }

                window_context.dirty = true;
                if window_context.display.window.has_frame {
                    window_context.display.window.request_redraw();
                }
            },
            // NOTE: This event bypasses batching to minimize input latency.
            (EventType::Frame, Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(&window_id) {
                    window_context.display.window.has_frame = true;
                    if window_context.dirty {
                        window_context.display.window.request_redraw();
                    }
                }
            },
            (payload, Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(&window_id) {
                    window_context.handle_event(
                        #[cfg(target_os = "macos")]
                        event_loop,
                        &self.proxy,
                        &mut self.clipboard,
                        &mut self.scheduler,
                        WinitEvent::UserEvent(Event { window_id: Some(window_id), tab_id, payload }),
                    );
                }
            },
        };
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.config.debug.print_events {
            info!(target: LOG_TARGET_WINIT, "About to wait");
        }

        // Dispatch event to all windows.
        for window_context in self.windows.values_mut() {
            window_context.handle_event(
                #[cfg(target_os = "macos")]
                event_loop,
                &self.proxy,
                &mut self.clipboard,
                &mut self.scheduler,
                WinitEvent::AboutToWait,
            );
        }

        // Update the scheduler after event processing to ensure
        // the event loop deadline is as accurate as possible.
        let control_flow = match self.scheduler.update() {
            Some(instant) => ControlFlow::WaitUntil(instant),
            None => ControlFlow::Wait,
        };
        event_loop.set_control_flow(control_flow);
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if self.config.debug.print_events {
            info!("Exiting the event loop");
        }

        match self.gl_config.take().map(|config| config.display()) {
            #[cfg(not(target_os = "macos"))]
            Some(glutin::display::Display::Egl(display)) => {
                // Ensure that all the windows are dropped, so the destructors for
                // Renderer and contexts ran.
                self.windows.clear();

                // SAFETY: the display is being destroyed after destroying all the
                // windows, thus no attempt to access the EGL state will be made.
                unsafe {
                    display.terminate();
                }
            },
            _ => (),
        }

        // SAFETY: The clipboard must be dropped before the event loop, so use the nop clipboard
        // as a safe placeholder.
        self.clipboard = Clipboard::new_nop();
    }
}

/// Tabor events.
#[derive(Debug, Clone)]
pub struct Event {
    /// Limit event to a specific window.
    window_id: Option<WindowId>,

    /// Limit event to a specific tab.
    tab_id: Option<TabId>,

    /// Event payload.
    payload: EventType,
}

impl Event {
    pub fn new<I: Into<Option<WindowId>>>(payload: EventType, window_id: I) -> Self {
        Self { window_id: window_id.into(), tab_id: None, payload }
    }

    pub fn for_tab(payload: EventType, window_id: WindowId, tab_id: TabId) -> Self {
        Self { window_id: Some(window_id), tab_id: Some(tab_id), payload }
    }

    pub fn window_id(&self) -> Option<WindowId> {
        self.window_id
    }

    pub fn tab_id(&self) -> Option<TabId> {
        self.tab_id
    }

    pub fn payload(&self) -> &EventType {
        &self.payload
    }
}

impl From<Event> for WinitEvent<Event> {
    fn from(event: Event) -> Self {
        WinitEvent::UserEvent(event)
    }
}

/// Tabor events.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub enum WebCommand {
    OpenUrl { url: String, new_tab: bool },
    CopyToClipboard { text: String },
    SetMark {
        name: char,
        url: String,
        scroll_x: f64,
        scroll_y: f64,
    },
}

#[derive(Debug, Clone)]
pub enum EventType {
    Terminal(TerminalEvent),
    ConfigReload(PathBuf),
    Message(Message),
    Scroll(Scroll),
    CreateWindow(WindowOptions),
    CreateTab(WindowOptions),
    TabCommand(TabCommand),
    #[cfg(target_os = "macos")]
    WebCommand(WebCommand),
    #[cfg(target_os = "macos")]
    WebFavicon { page_url: String, icon: Option<FaviconImage> },
    #[cfg(target_os = "macos")]
    WebCursor { cursor: Option<CursorIcon> },
    #[cfg(target_os = "macos")]
    WebCursorRequest,
    #[cfg(target_os = "macos")]
    CloseTab(TabId),
    #[cfg(target_os = "macos")]
    RestoreTab,
    #[cfg(target_os = "macos")]
    TabSearch(String),
    #[cfg(unix)]
    IpcRequest(IpcRequest, Arc<UnixStream>),
    BlinkCursor,
    BlinkCursorTimeout,
    TabActivityTick,
    SearchNext,
    UpdateTabProgramName,
    Frame,
}

impl From<TerminalEvent> for EventType {
    fn from(event: TerminalEvent) -> Self {
        Self::Terminal(event)
    }
}

/// Regex search state.
pub struct SearchState {
    /// Search direction.
    pub direction: Direction,

    /// Current position in the search history.
    pub history_index: Option<usize>,

    /// Change in display offset since the beginning of the search.
    display_offset_delta: i32,

    /// Search origin in viewport coordinates relative to original display offset.
    origin: Point,

    /// Focused match during active search.
    focused_match: Option<Match>,

    /// Search regex and history.
    ///
    /// During an active search, the first element is the user's current input.
    ///
    /// While going through history, the [`SearchState::history_index`] will point to the element
    /// in history which is currently being previewed.
    history: VecDeque<String>,

    /// Compiled search automatons.
    dfas: Option<RegexSearch>,
}

impl SearchState {
    /// Search regex text if a search is active.
    pub fn regex(&self) -> Option<&String> {
        self.history_index.and_then(|index| self.history.get(index))
    }

    /// Direction of the search from the search origin.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Focused match during vi-less search.
    pub fn focused_match(&self) -> Option<&Match> {
        self.focused_match.as_ref()
    }

    /// Clear the focused match.
    pub fn clear_focused_match(&mut self) {
        self.focused_match = None;
    }

    /// Active search dfas.
    pub fn dfas(&mut self) -> Option<&mut RegexSearch> {
        self.dfas.as_mut()
    }

    /// Search regex text if a search is active.
    fn regex_mut(&mut self) -> Option<&mut String> {
        self.history_index.and_then(move |index| self.history.get_mut(index))
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            direction: Direction::Right,
            display_offset_delta: Default::default(),
            focused_match: Default::default(),
            history_index: Default::default(),
            history: Default::default(),
            origin: Default::default(),
            dfas: Default::default(),
        }
    }
}

/// Command bar state.
pub struct CommandState {
    active: bool,
    prompt: char,
    input: String,
    completion: Option<CommandCompletion>,
}

struct CommandCompletion {
    prefix: String,
    index: usize,
}

impl CommandState {
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn text(&self) -> &str {
        &self.input
    }

    fn start(&mut self) {
        self.start_with(':');
    }

    fn start_with(&mut self, prompt: char) {
        self.active = true;
        self.prompt = prompt;
        self.input.clear();
        self.input.push(prompt);
        self.completion = None;
    }

    pub(crate) fn start_with_input(&mut self, prompt: char, input: &str) {
        self.start_with(prompt);
        self.input.push_str(input);
    }

    fn prompt_len(&self) -> usize {
        self.prompt.len_utf8()
    }

    pub fn cancel(&mut self) {
        self.active = false;
        self.input.clear();
        self.prompt = ':';
        self.completion = None;
    }

    fn take(&mut self) -> String {
        let input = self.input.trim().to_string();
        self.input.clear();
        self.active = false;
        self.prompt = ':';
        self.completion = None;
        input
    }

    fn clear_completion(&mut self) {
        self.completion = None;
    }
}

impl Default for CommandState {
    fn default() -> Self {
        Self { active: false, prompt: ':', input: String::new(), completion: None }
    }
}

/// URL history for command bar completions.
pub struct CommandHistory {
    urls: Vec<String>,
}

impl CommandHistory {
    pub(crate) fn record_url(&mut self, url: String) {
        if url.is_empty() {
            return;
        }

        if let Some(existing) = self.urls.iter().position(|entry| entry == &url) {
            self.urls.remove(existing);
        }

        self.urls.insert(0, url);

        const MAX_HISTORY: usize = 50;
        if self.urls.len() > MAX_HISTORY {
            self.urls.truncate(MAX_HISTORY);
        }
    }

    fn complete(&self, prefix: &str, last_index: Option<usize>) -> Option<(String, usize)> {
        if self.urls.is_empty() {
            return None;
        }

        let mut start = last_index.map(|index| index + 1).unwrap_or(0);
        if start >= self.urls.len() {
            start = 0;
        }

        for (index, entry) in self.urls.iter().enumerate().skip(start) {
            if entry.starts_with(prefix) {
                return Some((entry.clone(), index));
            }
        }

        if start > 0 {
            for (index, entry) in self.urls.iter().enumerate().take(start) {
                if entry.starts_with(prefix) {
                    return Some((entry.clone(), index));
                }
            }
        }

        None
    }
}

impl Default for CommandHistory {
    fn default() -> Self {
        Self { urls: Vec::new() }
    }
}

/// Vi inline search state.
pub struct InlineSearchState {
    /// Whether inline search is currently waiting for search character input.
    pub char_pending: bool,
    pub character: Option<char>,

    direction: Direction,
    stop_short: bool,
}

impl Default for InlineSearchState {
    fn default() -> Self {
        Self {
            direction: Direction::Right,
            char_pending: Default::default(),
            stop_short: Default::default(),
            character: Default::default(),
        }
    }
}

pub struct ActionContext<'a, N, T> {
    pub notifier: &'a mut N,
    pub terminal: &'a mut Term<T>,
    pub clipboard: &'a mut Clipboard,
    pub mouse: &'a mut Mouse,
    pub touch: &'a mut TouchPurpose,
    pub modifiers: &'a mut Modifiers,
    pub display: &'a mut Display,
    pub message_buffer: &'a mut MessageBuffer,
    pub config: &'a UiConfig,
    pub cursor_blink_timed_out: &'a mut bool,
    pub prev_bell_cmd: &'a mut Option<Instant>,
    pub command_state: &'a mut CommandState,
    pub command_history: &'a mut CommandHistory,
    pub tab_id: TabId,
    pub tab_kind: &'a mut WindowKind,
    #[cfg(target_os = "macos")]
    pub web_view: Option<&'a mut WebView>,
    #[cfg(target_os = "macos")]
    pub web_command_state: &'a mut WebCommandState,
    #[cfg(target_os = "macos")]
    pub event_loop: &'a ActiveEventLoop,
    pub event_proxy: &'a EventLoopProxy<Event>,
    pub scheduler: &'a mut Scheduler,
    pub search_state: &'a mut SearchState,
    pub inline_search_state: &'a mut InlineSearchState,
    pub dirty: &'a mut bool,
    pub occluded: &'a mut bool,
    pub preserve_title: bool,
    #[cfg(not(windows))]
    pub master_fd: RawFd,
    #[cfg(not(windows))]
    pub shell_pid: u32,
}

#[cfg(target_os = "macos")]
pub(crate) fn request_web_cursor_update(
    web_view: &mut WebView,
    web_command_state: &mut WebCommandState,
    display: &Display,
    position: PhysicalPosition<f64>,
    event_proxy: &EventLoopProxy<Event>,
    scheduler: &mut Scheduler,
    window_id: WindowId,
    tab_id: TabId,
) {
    web_command_state.set_last_cursor_pos(position);

    let scale_factor = display.window.scale_factor as f64;
    let size_info = display.size_info;
    let origin_x = f64::from(size_info.padding_x()) / scale_factor;
    let origin_y = f64::from(size_info.padding_y()) / scale_factor;
    let width = f64::from(size_info.width() - size_info.padding_x() - size_info.padding_right())
        / scale_factor;
    let height = f64::from(size_info.cell_height() * size_info.screen_lines() as f32)
        / scale_factor;

    let local_x = position.x / scale_factor - origin_x;
    let local_y = position.y / scale_factor - origin_y;

    if local_x < 0.0 || local_y < 0.0 || local_x >= width || local_y >= height {
        return;
    }

    if web_command_state.cursor_pending() {
        return;
    }

    let now = Instant::now();
    let timer_id = TimerId::new(Topic::WebCursor, window_id);
    if let Some(last_request) = web_command_state.last_cursor_request() {
        let elapsed = now.saturating_duration_since(last_request);
        if elapsed < WEB_CURSOR_THROTTLE {
            let delay = WEB_CURSOR_THROTTLE - elapsed;
            let event = Event::for_tab(EventType::WebCursorRequest, window_id, tab_id);
            scheduler.unschedule(timer_id);
            scheduler.schedule(event, delay, false, timer_id);
            return;
        }
    }

    scheduler.unschedule(timer_id);

    if !web_command_state.cursor_bootstrapped() {
        web_view.exec_js(WEB_CURSOR_BOOTSTRAP);
        web_command_state.set_cursor_bootstrapped(true);
    }

    web_command_state.set_cursor_pending(true);
    web_command_state.set_last_cursor_request(now);

    let proxy = event_proxy.clone();
    let script = web_cursor_script(local_x, local_y);

    web_view.eval_js_string(&script, move |result| {
        let cursor = result.as_deref().and_then(web_cursor_from_css);
        let event = Event::for_tab(EventType::WebCursor { cursor }, window_id, tab_id);
        let _ = proxy.send_event(event);
    });
}

impl<'a, N: Notify + 'a, T: EventListener> input::ActionContext<T> for ActionContext<'a, N, T> {
    #[inline]
    fn write_to_pty<B: Into<Cow<'static, [u8]>>>(&self, val: B) {
        self.notifier.notify(val);
    }

    /// Request a redraw.
    #[inline]
    fn mark_dirty(&mut self) {
        *self.dirty = true;
    }

    #[inline]
    fn size_info(&self) -> SizeInfo {
        self.display.size_info
    }

    fn scroll(&mut self, scroll: Scroll) {
        let old_offset = self.terminal.grid().display_offset() as i32;

        let old_vi_cursor = self.terminal.vi_mode_cursor;
        self.terminal.scroll_display(scroll);

        let lines_changed = old_offset - self.terminal.grid().display_offset() as i32;

        // Keep track of manual display offset changes during search.
        if self.search_active() {
            self.search_state.display_offset_delta += lines_changed;
        }

        let vi_mode = self.terminal.mode().contains(TermMode::VI);

        // Update selection.
        if vi_mode && self.terminal.selection.as_ref().is_some_and(|s| !s.is_empty()) {
            self.update_selection(self.terminal.vi_mode_cursor.point, Side::Right);
        } else if self.mouse.left_button_state == ElementState::Pressed
            || self.mouse.right_button_state == ElementState::Pressed
        {
            let display_offset = self.terminal.grid().display_offset();
            let point = self.mouse.point(&self.size_info(), display_offset);
            self.update_selection(point, self.mouse.cell_side);
        }

        // Scrolling inside Vi mode moves the cursor, so start typing.
        if vi_mode {
            self.on_typing_start();
        }

        // Update dirty if actually scrolled or moved Vi cursor in Vi mode.
        *self.dirty |=
            lines_changed != 0 || (vi_mode && old_vi_cursor != self.terminal.vi_mode_cursor);
    }

    // Copy text selection.
    fn copy_selection(&mut self, ty: ClipboardType) {
        let text = match self.terminal.selection_to_string().filter(|s| !s.is_empty()) {
            Some(text) => text,
            None => return,
        };

        if ty == ClipboardType::Selection && self.config.selection.save_to_clipboard {
            self.clipboard.store(ClipboardType::Clipboard, text.clone());
        }
        self.clipboard.store(ty, text);
    }

    fn selection_is_empty(&self) -> bool {
        self.terminal.selection.as_ref().is_none_or(Selection::is_empty)
    }

    fn clear_selection(&mut self) {
        // Clear the selection on the terminal.
        let selection = self.terminal.selection.take();
        // Mark the terminal as dirty when selection wasn't empty.
        *self.dirty |= selection.is_some_and(|s| !s.is_empty());
    }

    fn update_selection(&mut self, mut point: Point, side: Side) {
        let mut selection = match self.terminal.selection.take() {
            Some(selection) => selection,
            None => return,
        };

        // Treat motion over message bar like motion over the last line.
        point.line = min(point.line, self.terminal.bottommost_line());

        // Update selection.
        selection.update(point, side);

        // Move vi cursor and expand selection.
        if self.terminal.mode().contains(TermMode::VI) && !self.search_active() {
            self.terminal.vi_mode_cursor.point = point;
            selection.include_all();
        }

        self.terminal.selection = Some(selection);
        *self.dirty = true;
    }

    fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        self.terminal.selection = Some(Selection::new(ty, point, side));
        *self.dirty = true;

        input::ActionContext::copy_selection(self, ClipboardType::Selection);
    }

    fn toggle_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        match &mut self.terminal.selection {
            Some(selection) if selection.ty == ty && !selection.is_empty() => {
                input::ActionContext::clear_selection(self);
            },
            Some(selection) if !selection.is_empty() => {
                selection.ty = ty;
                *self.dirty = true;

                input::ActionContext::copy_selection(self, ClipboardType::Selection);
            },
            _ => self.start_selection(ty, point, side),
        }
    }

    #[inline]
    fn mouse_mode(&self) -> bool {
        self.terminal.mode().intersects(TermMode::MOUSE_MODE)
            && !self.terminal.mode().contains(TermMode::VI)
    }

    #[inline]
    fn mouse_mut(&mut self) -> &mut Mouse {
        self.mouse
    }

    #[inline]
    fn mouse(&self) -> &Mouse {
        self.mouse
    }

    #[inline]
    fn touch_purpose(&mut self) -> &mut TouchPurpose {
        self.touch
    }

    #[inline]
    fn modifiers(&mut self) -> &mut Modifiers {
        self.modifiers
    }

    #[inline]
    fn window(&mut self) -> &mut Window {
        &mut self.display.window
    }

    #[inline]
    fn display(&mut self) -> &mut Display {
        self.display
    }

    #[inline]
    fn terminal(&self) -> &Term<T> {
        self.terminal
    }

    #[inline]
    fn terminal_mut(&mut self) -> &mut Term<T> {
        self.terminal
    }

    #[inline]
    fn window_kind(&self) -> &WindowKind {
        self.tab_kind
    }

    #[cfg(target_os = "macos")]
    fn web_request_cursor_update(&mut self, position: PhysicalPosition<f64>) {
        if !self.tab_kind.is_web() {
            return;
        }

        let Some(web_view) = self.web_view.as_mut() else {
            return;
        };

        request_web_cursor_update(
            web_view,
            self.web_command_state,
            &self.display,
            position,
            self.event_proxy,
            self.scheduler,
            self.display.window.id(),
            self.tab_id,
        );
    }

    fn spawn_new_instance(&mut self) {
        let mut env_args = env::args();
        let tabor = env_args.next().unwrap();

        let mut args: Vec<String> = Vec::new();

        // Reuse the arguments passed to Tabor for the new instance.
        #[allow(clippy::while_let_on_iterator)]
        while let Some(arg) = env_args.next() {
            // New instances shouldn't inherit command.
            if arg == "-e" || arg == "--command" {
                break;
            }

            // On unix, the working directory of the foreground shell is used by `start_daemon`.
            #[cfg(not(windows))]
            if arg == "--working-directory" {
                let _ = env_args.next();
                continue;
            }

            args.push(arg);
        }

        self.spawn_daemon(&tabor, &args);
    }

    #[cfg(not(windows))]
    fn create_new_window(&mut self) {
        let mut options = WindowOptions::default();
        options.terminal_options.working_directory =
            foreground_process_path(self.master_fd, self.shell_pid).ok();
        let _ = self.event_proxy.send_event(Event::new(EventType::CreateWindow(options), None));
    }

    #[cfg(windows)]
    fn create_new_window(&mut self) {
        let _ = self
            .event_proxy
            .send_event(Event::new(EventType::CreateWindow(WindowOptions::default()), None));
    }

    fn create_new_tab(&mut self) {
        let mut options = WindowOptions::default();
        #[cfg(not(windows))]
        {
            options.terminal_options.working_directory =
                foreground_process_path(self.master_fd, self.shell_pid).ok();
        }

        let event = Event::new(EventType::CreateTab(options), self.display.window.id());
        let _ = self.event_proxy.send_event(event);
    }

    #[cfg(target_os = "macos")]
    fn select_next_tab(&mut self) {
        let event =
            Event::new(EventType::TabCommand(TabCommand::SelectNext), self.display.window.id());
        let _ = self.event_proxy.send_event(event);
    }

    #[cfg(target_os = "macos")]
    fn select_previous_tab(&mut self) {
        let event =
            Event::new(EventType::TabCommand(TabCommand::SelectPrevious), self.display.window.id());
        let _ = self.event_proxy.send_event(event);
    }

    #[cfg(target_os = "macos")]
    fn select_tab_at_index(&mut self, index: usize) {
        let event = Event::new(
            EventType::TabCommand(TabCommand::SelectIndex(index)),
            self.display.window.id(),
        );
        let _ = self.event_proxy.send_event(event);
    }

    #[cfg(target_os = "macos")]
    fn select_last_tab(&mut self) {
        let event =
            Event::new(EventType::TabCommand(TabCommand::SelectLast), self.display.window.id());
        let _ = self.event_proxy.send_event(event);
    }

    fn spawn_daemon<I, S>(&self, program: &str, args: I)
    where
        I: IntoIterator<Item = S> + Debug + Copy,
        S: AsRef<OsStr>,
    {
        #[cfg(not(windows))]
        let result = spawn_daemon(program, args, self.master_fd, self.shell_pid);
        #[cfg(windows)]
        let result = spawn_daemon(program, args);

        match result {
            Ok(_) => debug!("Launched {program} with args {args:?}"),
            Err(err) => warn!("Unable to launch {program} with args {args:?}: {err}"),
        }
    }

    fn change_font_size(&mut self, delta: f32) {
        // Round to pick integral px steps, since fonts look better on them.
        let new_size = self.display.font_size.as_px().round() + delta;
        self.display.font_size = FontSize::from_px(new_size);
        let font = self.config.font.clone().with_size(self.display.font_size);
        self.display.pending_update.set_font(font);
    }

    fn reset_font_size(&mut self) {
        let scale_factor = self.display.window.scale_factor as f32;
        self.display.font_size = self.config.font.size().scale(scale_factor);
        self.display
            .pending_update
            .set_font(self.config.font.clone().with_size(self.display.font_size));
    }

    #[inline]
    fn pop_message(&mut self) {
        if !self.message_buffer.is_empty() {
            self.display.pending_update.dirty = true;
            self.message_buffer.pop();
        }
    }

    #[inline]
    fn start_search(&mut self, direction: Direction) {
        // Only create new history entry if the previous regex wasn't empty.
        if self.search_state.history.front().is_none_or(|regex| !regex.is_empty()) {
            self.search_state.history.push_front(String::new());
            self.search_state.history.truncate(MAX_SEARCH_HISTORY_SIZE);
        }

        self.search_state.history_index = Some(0);
        self.search_state.direction = direction;
        self.search_state.focused_match = None;

        // Store original search position as origin and reset location.
        if self.terminal.mode().contains(TermMode::VI) {
            self.search_state.origin = self.terminal.vi_mode_cursor.point;
            self.search_state.display_offset_delta = 0;

            // Adjust origin for content moving upward on search start.
            if self.terminal.grid().cursor.point.line + 1 == self.terminal.screen_lines() {
                self.search_state.origin.line -= 1;
            }
        } else {
            let viewport_top = Line(-(self.terminal.grid().display_offset() as i32)) - 1;
            let viewport_bottom = viewport_top + self.terminal.bottommost_line();
            let last_column = self.terminal.last_column();
            self.search_state.origin = match direction {
                Direction::Right => Point::new(viewport_top, Column(0)),
                Direction::Left => Point::new(viewport_bottom, last_column),
            };
        }

        // Remove vi mode IME inhibitor, so the user can input the target character.
        self.window().set_ime_inhibitor(ImeInhibitor::VI, false);

        self.display.damage_tracker.frame().mark_fully_damaged();
        self.display.pending_update.dirty = true;
    }

    #[inline]
    fn start_seeded_search(&mut self, direction: Direction, text: String) {
        let origin = self.terminal.vi_mode_cursor.point;

        // Start new search.
        input::ActionContext::clear_selection(self);
        self.start_search(direction);

        // Enter initial selection text.
        for c in text.chars() {
            if let '$' | '('..='+' | '?' | '['..='^' | '{'..='}' = c {
                self.search_input('\\');
            }
            self.search_input(c);
        }

        // Leave search mode.
        self.confirm_search();

        if !self.terminal.mode().contains(TermMode::VI) {
            return;
        }

        // Find the target vi cursor point by going to the next match to the right of the origin,
        // then jump to the next search match in the target direction.
        let target = self.search_next(origin, Direction::Right, Side::Right).and_then(|rm| {
            let regex_match = match direction {
                Direction::Right => {
                    let origin = rm.end().add(self.terminal, Boundary::None, 1);
                    self.search_next(origin, Direction::Right, Side::Left)?
                },
                Direction::Left => {
                    let origin = rm.start().sub(self.terminal, Boundary::None, 1);
                    self.search_next(origin, Direction::Left, Side::Left)?
                },
            };
            Some(*regex_match.start())
        });

        // Move the vi cursor to the target position.
        if let Some(target) = target {
            self.terminal_mut().vi_goto_point(target);
            self.mark_dirty();
        }
    }

    #[inline]
    fn confirm_search(&mut self) {
        // Just cancel search when not in vi mode.
        if !self.terminal.mode().contains(TermMode::VI) {
            self.cancel_search();
            return;
        }

        // Force unlimited search if the previous one was interrupted.
        let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
        if self.scheduler.scheduled(timer_id) {
            self.goto_match(None);
        }

        self.exit_search();
    }

    #[inline]
    fn cancel_search(&mut self) {
        if self.terminal.mode().contains(TermMode::VI) {
            // Recover pre-search state in vi mode.
            self.search_reset_state();
        } else if let Some(focused_match) = &self.search_state.focused_match {
            // Create a selection for the focused match.
            let start = *focused_match.start();
            let end = *focused_match.end();
            self.start_selection(SelectionType::Simple, start, Side::Left);
            self.update_selection(end, Side::Right);
            input::ActionContext::copy_selection(self, ClipboardType::Selection);
        }

        self.search_state.dfas = None;

        self.exit_search();
    }

    #[inline]
    fn search_input(&mut self, c: char) {
        match self.search_state.history_index {
            Some(0) => (),
            // When currently in history, replace active regex with history on change.
            Some(index) => {
                self.search_state.history[0] = self.search_state.history[index].clone();
                self.search_state.history_index = Some(0);
            },
            None => return,
        }
        let regex = &mut self.search_state.history[0];

        match c {
            // Handle backspace/ctrl+h.
            '\x08' | '\x7f' => {
                let _ = regex.pop();
            },
            // Add ascii and unicode text.
            ' '..='~' | '\u{a0}'..='\u{10ffff}' => regex.push(c),
            // Ignore non-printable characters.
            _ => return,
        }

        if !self.terminal.mode().contains(TermMode::VI) {
            // Clear selection so we do not obstruct any matches.
            self.terminal.selection = None;
        }

        self.update_search();
    }

    #[inline]
    fn search_pop_word(&mut self) {
        if let Some(regex) = self.search_state.regex_mut() {
            *regex = regex.trim_end().to_owned();
            regex.truncate(regex.rfind(' ').map_or(0, |i| i + 1));
            self.update_search();
        }
    }

    /// Go to the previous regex in the search history.
    #[inline]
    fn search_history_previous(&mut self) {
        let index = match &mut self.search_state.history_index {
            None => return,
            Some(index) if *index + 1 >= self.search_state.history.len() => return,
            Some(index) => index,
        };

        *index += 1;
        self.update_search();
    }

    /// Go to the previous regex in the search history.
    #[inline]
    fn search_history_next(&mut self) {
        let index = match &mut self.search_state.history_index {
            Some(0) | None => return,
            Some(index) => index,
        };

        *index -= 1;
        self.update_search();
    }

    #[inline]
    fn advance_search_origin(&mut self, direction: Direction) {
        // Use focused match as new search origin if available.
        if let Some(focused_match) = &self.search_state.focused_match {
            let new_origin = match direction {
                Direction::Right => focused_match.end().add(self.terminal, Boundary::None, 1),
                Direction::Left => focused_match.start().sub(self.terminal, Boundary::None, 1),
            };

            self.terminal.scroll_to_point(new_origin);

            self.search_state.display_offset_delta = 0;
            self.search_state.origin = new_origin;
        }

        // Search for the next match using the supplied direction.
        let search_direction = mem::replace(&mut self.search_state.direction, direction);
        self.goto_match(None);
        self.search_state.direction = search_direction;

        // If we found a match, we set the search origin right in front of it to make sure that
        // after modifications to the regex the search is started without moving the focused match
        // around.
        let focused_match = match &self.search_state.focused_match {
            Some(focused_match) => focused_match,
            None => return,
        };

        // Set new origin to the left/right of the match, depending on search direction.
        let new_origin = match self.search_state.direction {
            Direction::Right => *focused_match.start(),
            Direction::Left => *focused_match.end(),
        };

        // Store the search origin with display offset by checking how far we need to scroll to it.
        let old_display_offset = self.terminal.grid().display_offset() as i32;
        self.terminal.scroll_to_point(new_origin);
        let new_display_offset = self.terminal.grid().display_offset() as i32;
        self.search_state.display_offset_delta = new_display_offset - old_display_offset;

        // Store origin and scroll back to the match.
        self.terminal.scroll_display(Scroll::Delta(-self.search_state.display_offset_delta));
        self.search_state.origin = new_origin;
    }

    /// Find the next search match.
    fn search_next(&mut self, origin: Point, direction: Direction, side: Side) -> Option<Match> {
        self.search_state
            .dfas
            .as_mut()
            .and_then(|dfas| self.terminal.search_next(dfas, origin, direction, side, None))
    }

    #[inline]
    fn search_direction(&self) -> Direction {
        self.search_state.direction
    }

    #[inline]
    fn search_active(&self) -> bool {
        self.search_state.history_index.is_some()
    }

    #[inline]
    fn command_active(&self) -> bool {
        self.command_state.is_active()
    }

    fn toggle_command_bar(&mut self) {
        if self.command_state.is_active() {
            self.command_state.cancel();
        } else {
            if self.search_active() {
                self.cancel_search();
            }
            #[cfg(target_os = "macos")]
            if self.tab_kind.is_web() {
                let url = normalize_web_url("chat.com");
                let mut options = WindowOptions::default();
                options.window_kind = WindowKind::Web { url: url.clone() };
                options.command_input = Some(String::from("o "));
                #[cfg(not(windows))]
                {
                    options.terminal_options.working_directory =
                        foreground_process_path(self.master_fd, self.shell_pid).ok();
                }

                let event = Event::new(EventType::CreateTab(options), self.display.window.id());
                self.command_history.record_url(url);
                let _ = self.event_proxy.send_event(event);
                return;
            }
            self.command_state.start();
        }

        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        *self.dirty = true;
    }

    fn confirm_command(&mut self) {
        let input = self.command_state.take();
        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        *self.dirty = true;
        self.run_command(input);
    }

    fn cancel_command(&mut self) {
        if !self.command_state.is_active() {
            return;
        }

        self.command_state.cancel();
        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        *self.dirty = true;
    }

    fn command_autocomplete(&mut self) {
        if !self.command_state.is_active() {
            return;
        }

        let input_snapshot = self.command_state.input.clone();
        let Some((start, prefix)) = command_url_prefix(&input_snapshot) else {
            return;
        };

        let prefix = prefix.to_string();
        let last_index = self.command_state.completion.as_ref().and_then(|state| {
            if state.prefix == prefix {
                Some(state.index)
            } else {
                None
            }
        });

        let Some((completion, index)) = self.command_history.complete(&prefix, last_index) else {
            return;
        };

        let mut input = input_snapshot[..start].to_string();
        if !input.ends_with(' ') {
            input.push(' ');
        }
        input.push_str(&completion);

        self.command_state.input = input;
        self.command_state.completion = Some(CommandCompletion {
            prefix,
            index,
        });

        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        *self.dirty = true;
    }

    fn command_input(&mut self, c: char) {
        if !self.command_state.is_active() {
            return;
        }

        let prompt_len = self.command_state.prompt_len();
        match c {
            '\x08' | '\x7f' => {
                if self.command_state.input.len() > prompt_len {
                    self.command_state.input.pop();
                }
            },
            '\x15' => {
                self.command_state.input.clear();
                self.command_state.input.push(self.command_state.prompt);
            },
            '\x17' => self.command_pop_word(),
            ' '..='~' | '\u{a0}'..='\u{10ffff}' => self.command_state.input.push(c),
            _ => return,
        }

        self.command_state.clear_completion();
        *self.dirty = true;
    }

    fn command_pop_word(&mut self) {
        if !self.command_state.is_active() {
            return;
        }

        let prompt_len = self.command_state.prompt_len();
        let mut end = self.command_state.input.len();

        while end > prompt_len {
            let ch = self.command_state.input[..end].chars().last().unwrap();
            if !ch.is_whitespace() {
                break;
            }
            end -= ch.len_utf8();
        }

        while end > prompt_len {
            let ch = self.command_state.input[..end].chars().last().unwrap();
            if ch.is_whitespace() {
                break;
            }
            end -= ch.len_utf8();
        }

        self.command_state.input.truncate(end.max(prompt_len));
        self.command_state.clear_completion();
        *self.dirty = true;
    }

    /// Handle keyboard typing start.
    ///
    /// This will temporarily disable some features like terminal cursor blinking or the mouse
    /// cursor.
    ///
    /// All features are re-enabled again automatically.
    #[inline]
    fn on_typing_start(&mut self) {
        // Disable cursor blinking.
        let timer_id = TimerId::new(Topic::BlinkCursor, self.display.window.id());
        if self.scheduler.unschedule(timer_id).is_some() {
            self.schedule_blinking();

            // Mark the cursor as visible and queue redraw if the cursor was hidden.
            if mem::take(&mut self.display.cursor_hidden) {
                *self.dirty = true;
            }
        } else if *self.cursor_blink_timed_out {
            self.update_cursor_blinking();
        }

        // Hide mouse cursor.
        if self.config.mouse.hide_when_typing && self.display.window.mouse_visible() {
            self.display.window.set_mouse_visible(false);

            // Request hint highlights update, since the mouse may have been hovering a hint.
            self.mouse.hint_highlight_dirty = true
        }
    }

    /// Process a new character for keyboard hints.
    fn hint_input(&mut self, c: char) {
        if let Some(hint) = self.display.hint_state.keyboard_input(self.terminal, c) {
            self.mouse.block_hint_launcher = false;
            self.trigger_hint(&hint);
        }
        *self.dirty = true;
    }

    /// Trigger a hint action.
    fn trigger_hint(&mut self, hint: &HintMatch) {
        if self.mouse.block_hint_launcher {
            return;
        }

        let hint_bounds = hint.bounds();
        let text = match hint.text(self.terminal) {
            Some(text) => text,
            None => return,
        };

        match &hint.action() {
            // Launch an external program.
            HintAction::Command(command) => {
                let mut args = command.args().to_vec();
                args.push(text.into());
                self.spawn_daemon(command.program(), &args);
            },
            // Copy the text to the clipboard.
            HintAction::Action(HintInternalAction::Copy) => {
                self.clipboard.store(ClipboardType::Clipboard, text);
            },
            // Write the text to the PTY/search.
            HintAction::Action(HintInternalAction::Paste) => self.paste(&text, true),
            // Select the text.
            HintAction::Action(HintInternalAction::Select) => {
                self.start_selection(SelectionType::Simple, *hint_bounds.start(), Side::Left);
                self.update_selection(*hint_bounds.end(), Side::Right);
                input::ActionContext::copy_selection(self, ClipboardType::Selection);
            },
            // Move the vi mode cursor.
            HintAction::Action(HintInternalAction::MoveViModeCursor) => {
                // Enter vi mode if we're not in it already.
                if !self.terminal.mode().contains(TermMode::VI) {
                    self.terminal.toggle_vi_mode();
                }

                self.terminal.vi_goto_point(*hint_bounds.start());
                self.mark_dirty();
            },
        }
    }

    /// Expand the selection to the current mouse cursor position.
    #[inline]
    fn expand_selection(&mut self) {
        let control = self.modifiers().state().control_key();
        let selection_type = match self.mouse().click_state {
            ClickState::None => return,
            _ if control => SelectionType::Block,
            ClickState::Click => SelectionType::Simple,
            ClickState::DoubleClick => SelectionType::Semantic,
            ClickState::TripleClick => SelectionType::Lines,
        };

        // Load mouse point, treating message bar and padding as the closest cell.
        let display_offset = self.terminal().grid().display_offset();
        let point = self.mouse().point(&self.size_info(), display_offset);

        let cell_side = self.mouse().cell_side;

        let selection = match &mut self.terminal_mut().selection {
            Some(selection) => selection,
            None => return,
        };

        selection.ty = selection_type;
        self.update_selection(point, cell_side);

        // Move vi mode cursor to mouse click position.
        if self.terminal().mode().contains(TermMode::VI) && !self.search_active() {
            self.terminal_mut().vi_mode_cursor.point = point;
        }
    }

    /// Get the semantic word at the specified point.
    fn semantic_word(&self, point: Point) -> String {
        let terminal = self.terminal();
        let grid = terminal.grid();

        // Find the next semantic word boundary to the right.
        let mut end = terminal.semantic_search_right(point);

        // Get point at which skipping over semantic characters has led us back to the
        // original character.
        let start_cell = &grid[point];
        let search_end = if start_cell.flags.intersects(Flags::LEADING_WIDE_CHAR_SPACER) {
            point.add(terminal, Boundary::None, 2)
        } else if start_cell.flags.intersects(Flags::WIDE_CHAR) {
            point.add(terminal, Boundary::None, 1)
        } else {
            point
        };

        // Keep moving until we're not on top of a semantic escape character.
        let semantic_chars = terminal.semantic_escape_chars();
        loop {
            let cell = &grid[end];

            // Get cell's character, taking wide characters into account.
            let c = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                grid[end.sub(terminal, Boundary::None, 1)].c
            } else {
                cell.c
            };

            if !semantic_chars.contains(c) {
                break;
            }

            end = terminal.semantic_search_right(end.add(terminal, Boundary::None, 1));

            // Stop if the entire grid is only semantic escape characters.
            if end == search_end {
                return String::new();
            }
        }

        // Find the beginning of the semantic word.
        let start = terminal.semantic_search_left(end);

        terminal.bounds_to_string(start, end)
    }

    /// Handle beginning of terminal text input.
    fn on_terminal_input_start(&mut self) {
        self.on_typing_start();
        input::ActionContext::clear_selection(self);

        if self.terminal().grid().display_offset() != 0 {
            self.scroll(Scroll::Bottom);
        }
    }

    /// Paste a text into the terminal.
    fn paste(&mut self, text: &str, bracketed: bool) {
        if self.search_active() {
            for c in text.chars() {
                self.search_input(c);
            }
        } else if self.inline_search_state.char_pending {
            self.inline_search_input(text);
        } else if bracketed && self.terminal().mode().contains(TermMode::BRACKETED_PASTE) {
            self.on_terminal_input_start();

            self.write_to_pty(&b"\x1b[200~"[..]);

            // Write filtered escape sequences.
            //
            // We remove `\x1b` to ensure it's impossible for the pasted text to write the bracketed
            // paste end escape `\x1b[201~` and `\x03` since some shells incorrectly terminate
            // bracketed paste when they receive it.
            let filtered = text.replace(['\x1b', '\x03'], "");
            self.write_to_pty(filtered.into_bytes());

            self.write_to_pty(&b"\x1b[201~"[..]);
        } else {
            self.on_terminal_input_start();

            let payload = if bracketed {
                // In non-bracketed (ie: normal) mode, terminal applications cannot distinguish
                // pasted data from keystrokes.
                //
                // In theory, we should construct the keystrokes needed to produce the data we are
                // pasting... since that's neither practical nor sensible (and probably an
                // impossible task to solve in a general way), we'll just replace line breaks
                // (windows and unix style) with a single carriage return (\r, which is what the
                // Enter key produces).
                text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
            } else {
                // When we explicitly disable bracketed paste don't manipulate with the input,
                // so we pass user input as is.
                text.to_owned().into_bytes()
            };

            self.write_to_pty(payload);
        }
    }

    /// Toggle the vi mode status.
    #[inline]
    fn toggle_vi_mode(&mut self) {
        #[cfg(target_os = "macos")]
        if self.tab_kind.is_web() {
            if self.command_state.is_active() {
                self.cancel_command();
            } else {
                self.start_command_prompt(':', "");
            }
            return;
        }

        let was_in_vi_mode = self.terminal.mode().contains(TermMode::VI);
        if was_in_vi_mode {
            // If we had search running when leaving Vi mode we should mark terminal fully damaged
            // to cleanup highlighted results.
            if self.search_state.dfas.take().is_some() {
                self.display.damage_tracker.frame().mark_fully_damaged();
            }
        } else {
            input::ActionContext::clear_selection(self);
        }

        if self.search_active() {
            self.cancel_search();
        }

        // We don't want IME in Vi mode.
        self.window().set_ime_inhibitor(ImeInhibitor::VI, !was_in_vi_mode);

        self.terminal.toggle_vi_mode();

        *self.dirty = true;
    }

    /// Get vi inline search state.
    fn inline_search_state(&mut self) -> &mut InlineSearchState {
        self.inline_search_state
    }

    /// Start vi mode inline search.
    fn start_inline_search(&mut self, direction: Direction, stop_short: bool) {
        self.inline_search_state.stop_short = stop_short;
        self.inline_search_state.direction = direction;
        self.inline_search_state.char_pending = true;
        self.inline_search_state.character = None;
    }

    /// Jump to the next matching character in the line.
    fn inline_search_next(&mut self) {
        let direction = self.inline_search_state.direction;
        self.inline_search(direction);
    }

    /// Jump to the next matching character in the line.
    fn inline_search_previous(&mut self) {
        let direction = self.inline_search_state.direction.opposite();
        self.inline_search(direction);
    }

    /// Process input during inline search.
    fn inline_search_input(&mut self, text: &str) {
        // Ignore input with empty text, like modifier keys.
        let c = match text.chars().next() {
            Some(c) => c,
            None => return,
        };

        self.inline_search_state.char_pending = false;
        self.inline_search_state.character = Some(c);
        self.window().set_ime_inhibitor(ImeInhibitor::VI, true);

        // Immediately move to the captured character.
        self.inline_search_next();
    }

    fn message(&self) -> Option<&Message> {
        self.message_buffer.message()
    }

    fn config(&self) -> &UiConfig {
        self.config
    }

    #[cfg(target_os = "macos")]
    fn event_loop(&self) -> &ActiveEventLoop {
        self.event_loop
    }

    fn clipboard_mut(&mut self) -> &mut Clipboard {
        self.clipboard
    }

    fn scheduler_mut(&mut self) -> &mut Scheduler {
        self.scheduler
    }

    #[cfg(target_os = "macos")]
    fn web_handle_key(&mut self, key: &KeyEvent, text: &str) -> bool {
        if self.web_view.is_none() {
            return false;
        }

        let web_key = web_key_from_event(key);
        self.with_web_command_state(|state, ctx| {
            let before = state.status_label();
            let handled = web_commands::handle_key(state, ctx, web_key, text);
            if handled && before != state.status_label() {
                ctx.mark_dirty();
            }
            handled
        })
    }

    #[cfg(target_os = "macos")]
    fn web_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        if !self.tab_kind.is_web() {
            return;
        }

        let Some(position) = self.web_command_state.last_cursor_pos() else {
            return;
        };

        let modifiers = web_modifier_flags(self.modifiers().state());
        let Some(web_view) = self.web_view.as_mut() else {
            return;
        };
        web_view.handle_mouse_input(
            &self.display.window,
            &self.display.size_info,
            position,
            state,
            button,
            modifiers,
        );
    }
}

#[cfg(target_os = "macos")]
fn web_key_from_event(key: &KeyEvent) -> WebKey {
    match key.logical_key.as_ref() {
        Key::Named(NamedKey::Escape) => WebKey::Escape,
        Key::Named(NamedKey::Enter) => WebKey::Enter,
        Key::Named(NamedKey::Backspace) => WebKey::Backspace,
        Key::Named(NamedKey::Delete) => WebKey::Delete,
        Key::Named(NamedKey::Tab) => WebKey::Tab,
        Key::Named(NamedKey::ArrowLeft) => WebKey::ArrowLeft,
        Key::Named(NamedKey::ArrowRight) => WebKey::ArrowRight,
        Key::Named(NamedKey::ArrowUp) => WebKey::ArrowUp,
        Key::Named(NamedKey::ArrowDown) => WebKey::ArrowDown,
        _ => WebKey::Other,
    }
}

#[cfg(target_os = "macos")]
fn web_modifier_flags(mods: ModifiersState) -> NSEventModifierFlags {
    let mut flags = NSEventModifierFlags::empty();
    if mods.shift_key() {
        flags.insert(NSEventModifierFlags::Shift);
    }
    if mods.control_key() {
        flags.insert(NSEventModifierFlags::Control);
    }
    if mods.alt_key() {
        flags.insert(NSEventModifierFlags::Option);
    }
    if mods.super_key() {
        flags.insert(NSEventModifierFlags::Command);
    }
    flags
}

impl<'a, N: Notify + 'a, T: EventListener> ActionContext<'a, N, T> {
    #[cfg(target_os = "macos")]
    fn with_web_command_state<R>(
        &mut self,
        f: impl FnOnce(&mut WebCommandState, &mut Self) -> R,
    ) -> R {
        let state_ptr = self.web_command_state as *mut WebCommandState;
        // SAFETY: WebCommandState is stored outside ActionContext; WebActions implementations
        // do not access web_command_state directly, so we can split the mutable borrow here.
        unsafe { f(&mut *state_ptr, self) }
    }

    fn update_search(&mut self) {
        let regex = match self.search_state.regex() {
            Some(regex) => regex,
            None => return,
        };

        // Hide cursor while typing into the search bar.
        if self.config.mouse.hide_when_typing {
            self.display.window.set_mouse_visible(false);
        }

        if regex.is_empty() {
            // Stop search if there's nothing to search for.
            self.search_reset_state();
            self.search_state.dfas = None;
        } else {
            // Create search dfas for the new regex string.
            self.search_state.dfas = RegexSearch::new(regex).ok();

            // Update search highlighting.
            self.goto_match(MAX_SEARCH_WHILE_TYPING);
        }

        *self.dirty = true;
    }

    /// Reset terminal to the state before search was started.
    fn search_reset_state(&mut self) {
        // Unschedule pending timers.
        let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
        self.scheduler.unschedule(timer_id);

        // Clear focused match.
        self.search_state.focused_match = None;

        // The viewport reset logic is only needed for vi mode, since without it our origin is
        // always at the current display offset instead of at the vi cursor position which we need
        // to recover to.
        if !self.terminal.mode().contains(TermMode::VI) {
            return;
        }

        // Reset display offset and cursor position.
        self.terminal.vi_mode_cursor.point = self.search_state.origin;
        self.terminal.scroll_display(Scroll::Delta(self.search_state.display_offset_delta));
        self.search_state.display_offset_delta = 0;

        *self.dirty = true;
    }

    /// Jump to the first regex match from the search origin.
    fn goto_match(&mut self, mut limit: Option<usize>) {
        let dfas = match &mut self.search_state.dfas {
            Some(dfas) => dfas,
            None => return,
        };

        // Limit search only when enough lines are available to run into the limit.
        limit = limit.filter(|&limit| limit <= self.terminal.total_lines());

        // Jump to the next match.
        let direction = self.search_state.direction;
        let clamped_origin = self.search_state.origin.grid_clamp(self.terminal, Boundary::Grid);
        match self.terminal.search_next(dfas, clamped_origin, direction, Side::Left, limit) {
            Some(regex_match) => {
                let old_offset = self.terminal.grid().display_offset() as i32;

                if self.terminal.mode().contains(TermMode::VI) {
                    // Move vi cursor to the start of the match.
                    self.terminal.vi_goto_point(*regex_match.start());
                } else {
                    // Select the match when vi mode is not active.
                    self.terminal.scroll_to_point(*regex_match.start());
                }

                // Update the focused match.
                self.search_state.focused_match = Some(regex_match);

                // Store number of lines the viewport had to be moved.
                let display_offset = self.terminal.grid().display_offset();
                self.search_state.display_offset_delta += old_offset - display_offset as i32;

                // Since we found a result, we require no delayed re-search.
                let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
                self.scheduler.unschedule(timer_id);
            },
            // Reset viewport only when we know there is no match, to prevent unnecessary jumping.
            None if limit.is_none() => self.search_reset_state(),
            None => {
                // Schedule delayed search if we ran into our search limit.
                let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
                if !self.scheduler.scheduled(timer_id) {
                    let event = Event::new(EventType::SearchNext, self.display.window.id());
                    self.scheduler.schedule(event, TYPING_SEARCH_DELAY, false, timer_id);
                }

                // Clear focused match.
                self.search_state.focused_match = None;
            },
        }

        *self.dirty = true;
    }

    /// Cleanup the search state.
    fn exit_search(&mut self) {
        let vi_mode = self.terminal.mode().contains(TermMode::VI);
        self.window().set_ime_inhibitor(ImeInhibitor::VI, vi_mode);

        self.display.damage_tracker.frame().mark_fully_damaged();
        self.display.pending_update.dirty = true;
        self.search_state.history_index = None;

        // Clear focused match.
        self.search_state.focused_match = None;
    }

    /// Update the cursor blinking state.
    fn update_cursor_blinking(&mut self) {
        // Get config cursor style.
        let mut cursor_style = self.config.cursor.style;
        let vi_mode = self.terminal.mode().contains(TermMode::VI);
        if vi_mode {
            cursor_style = self.config.cursor.vi_mode_style.unwrap_or(cursor_style);
        }

        // Check terminal cursor style.
        let terminal_blinking = self.terminal.cursor_style().blinking;
        let mut blinking = cursor_style.blinking_override().unwrap_or(terminal_blinking);
        blinking &= (vi_mode || self.terminal().mode().contains(TermMode::SHOW_CURSOR))
            && self.display().ime.preedit().is_none();

        // Update cursor blinking state.
        let window_id = self.display.window.id();
        self.scheduler.unschedule(TimerId::new(Topic::BlinkCursor, window_id));
        self.scheduler.unschedule(TimerId::new(Topic::BlinkTimeout, window_id));

        // Reset blinking timeout.
        *self.cursor_blink_timed_out = false;

        if blinking && self.terminal.is_focused {
            self.schedule_blinking();
            self.schedule_blinking_timeout();
        } else {
            self.display.cursor_hidden = false;
            *self.dirty = true;
        }
    }

    fn schedule_blinking(&mut self) {
        let window_id = self.display.window.id();
        let timer_id = TimerId::new(Topic::BlinkCursor, window_id);
        let event = Event::new(EventType::BlinkCursor, window_id);
        let blinking_interval = Duration::from_millis(self.config.cursor.blink_interval());
        self.scheduler.schedule(event, blinking_interval, true, timer_id);
    }

    fn schedule_blinking_timeout(&mut self) {
        let blinking_timeout = self.config.cursor.blink_timeout();
        if blinking_timeout == Duration::ZERO {
            return;
        }

        let window_id = self.display.window.id();
        let event = Event::new(EventType::BlinkCursorTimeout, window_id);
        let timer_id = TimerId::new(Topic::BlinkTimeout, window_id);

        self.scheduler.schedule(event, blinking_timeout, false, timer_id);
    }

    /// Perform vi mode inline search in the specified direction.
    fn inline_search(&mut self, direction: Direction) {
        let c = match self.inline_search_state.character {
            Some(c) => c,
            None => return,
        };
        let mut buf = [0; 4];
        let search_character = c.encode_utf8(&mut buf);

        // Find next match in this line.
        let vi_point = self.terminal.vi_mode_cursor.point;
        let point = match direction {
            Direction::Right => self.terminal.inline_search_right(vi_point, search_character),
            Direction::Left => self.terminal.inline_search_left(vi_point, search_character),
        };

        // Jump to point if there's a match.
        if let Ok(mut point) = point {
            if self.inline_search_state.stop_short {
                let grid = self.terminal.grid();
                point = match direction {
                    Direction::Right => {
                        grid.iter_from(point).prev().map_or(point, |cell| cell.point)
                    },
                    Direction::Left => {
                        grid.iter_from(point).next().map_or(point, |cell| cell.point)
                    },
                };
            }

            self.terminal.vi_goto_point(point);
            self.mark_dirty();
        }
    }

    pub(crate) fn run_command(&mut self, input: String) {
        if let Some(find_query) = input.strip_prefix('/') {
            let query = find_query.trim();
            if query.is_empty() {
                return;
            }

        #[cfg(target_os = "macos")]
        if self.tab_kind.is_web() {
            self.with_web_command_state(|state, ctx| {
                web_commands::find(state, ctx, query, false);
            });
            return;
        }

            self.push_command_error(String::from("Find is only available in web tabs"));
            return;
        }

        let trimmed = input.strip_prefix(':').unwrap_or(&input).trim();
        if trimmed.is_empty() {
            return;
        }

        let mut parts = trimmed.split_whitespace();
        let Some(command) = parts.next() else {
            return;
        };

        match command {
            "o" | "O" | "b" | "B" => {
                let url = parts.collect::<Vec<_>>().join(" ");
                if url.is_empty() {
                    self.push_command_error(format!("Missing URL for :{command}"));
                    return;
                }

                let url = normalize_web_url(&url);
                if matches!(command, "O" | "B") {
                    self.open_web_url_new_tab(url);
                } else {
                    self.open_web_url(url);
                }
            },
            "T" => {
                let query = parts.collect::<Vec<_>>().join(" ");
                if query.is_empty() {
                    self.push_command_error(String::from("Missing tab query for :T"));
                    return;
                }
                #[cfg(target_os = "macos")]
                {
                    let event = Event::new(
                        EventType::TabSearch(query),
                        self.display.window.id(),
                    );
                    let _ = self.event_proxy.send_event(event);
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = query;
                    self.push_command_error(String::from("Tab search is only available on macOS"));
                }
            },
            "r" => {
                self.reload_web();
            },
            "inspect" | "inspector" | "devtools" => {
                self.open_web_inspector();
            },
            _ => {
                self.push_command_error(format!("Unknown command: {command}"));
            },
        }
    }

    fn open_web_url(&mut self, url: String) {
        match &mut *self.tab_kind {
            WindowKind::Web { url: current_url } => {
                *current_url = url.clone();
                #[cfg(target_os = "macos")]
                if let Some(web_view) = self.web_view.as_mut() {
                    if web_view.load_url(&url) {
                        self.command_history.record_url(url);
                        return;
                    }
                }

                self.push_command_error(String::from("Failed to load URL"));
            },
            WindowKind::Terminal => {
                let mut options = WindowOptions::default();
                options.window_kind = WindowKind::Web { url };
                #[cfg(not(windows))]
                {
                    options.terminal_options.working_directory =
                        foreground_process_path(self.master_fd, self.shell_pid).ok();
                }
                let record_url = match &options.window_kind {
                    WindowKind::Web { url } => Some(url.clone()),
                    WindowKind::Terminal => None,
                };
                let event = Event::new(EventType::CreateTab(options), self.display.window.id());
                if let Some(url) = record_url {
                    self.command_history.record_url(url);
                }
                let _ = self.event_proxy.send_event(event);
            },
        }
    }

    fn open_web_url_new_tab(&mut self, url: String) {
        let mut options = WindowOptions::default();
        options.window_kind = WindowKind::Web { url: url.clone() };
        #[cfg(not(windows))]
        {
            options.terminal_options.working_directory =
                foreground_process_path(self.master_fd, self.shell_pid).ok();
        }

        let event = Event::new(EventType::CreateTab(options), self.display.window.id());
        self.command_history.record_url(url);
        let _ = self.event_proxy.send_event(event);
    }

    pub(crate) fn reload_web(&mut self) {
        match &*self.tab_kind {
            WindowKind::Web { .. } => {
                #[cfg(target_os = "macos")]
                if let Some(web_view) = self.web_view.as_mut() {
                    web_view.reload();
                    self.web_command_state.set_cursor_bootstrapped(false);
                    self.web_command_state.clear_last_cursor_request();
                    self.display.pending_update.dirty = true;
                    self.display.damage_tracker.frame().mark_fully_damaged();
                    *self.dirty = true;
                    return;
                }

                self.push_command_error(String::from("Web view is unavailable"));
            },
            WindowKind::Terminal => {
                self.push_command_error(String::from("No active web tab to reload"));
            },
        }
    }

    pub(crate) fn open_web_inspector(&mut self) {
        match &*self.tab_kind {
            WindowKind::Web { .. } => {
                #[cfg(target_os = "macos")]
                if let Some(web_view) = self.web_view.as_mut() {
                    if web_view.show_inspector() {
                        return;
                    }
                }

                self.push_command_error(String::from("Web inspector is unavailable"));
            },
            WindowKind::Terminal => {
                self.push_command_error(String::from("No active web tab to inspect"));
            },
        }
    }

    fn push_command_error(&mut self, message: String) {
        self.message_buffer
            .push(Message::new(message, crate::message_bar::MessageType::Error));
        self.display.pending_update.dirty = true;
    }
}

#[cfg(target_os = "macos")]
impl<'a, N: Notify + 'a, T: EventListener> ActionContext<'a, N, T> {
    fn js_string(value: &str) -> String {
        serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
    }

    fn start_command_prompt(&mut self, prompt: char, input: &str) {
        if self.command_state.is_active() {
            self.command_state.cancel();
        }
        if self.search_active() {
            self.cancel_search();
        }

        self.command_state.start_with_input(prompt, input);
        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        *self.dirty = true;
    }

    fn web_exec_js(&mut self, script: &str) {
        if let Some(web_view) = self.web_view.as_mut() {
            web_view.exec_js(script);
        }
    }

    fn web_eval_js_string<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        if let Some(web_view) = self.web_view.as_mut() {
            web_view.eval_js_string(script, callback);
        }
    }

    fn current_web_url(&mut self) -> Option<String> {
        if let Some(view) = self.web_view.as_ref() {
            if let Some(url) = view.current_url() {
                return Some(url);
            }
        }

        match &*self.tab_kind {
            WindowKind::Web { url } if !url.is_empty() => Some(url.clone()),
            _ => None,
        }
    }

    fn web_clear_selection(&mut self) {
        self.web_exec_js("window.getSelection().removeAllRanges();");
    }

    fn web_blur_active_element(&mut self) {
        self.web_exec_js("if (document.activeElement) { document.activeElement.blur(); }");
    }

    fn web_hints_start(&mut self, _action: WebHintAction) {
        self.web_exec_js(&format!("{WEB_HINTS_BOOTSTRAP}\nwindow.__taborHints.start();"));
    }

    fn web_hints_update(&mut self, keys: &str, action: WebHintAction) {
        let script = format!(
            "{WEB_HINTS_BOOTSTRAP}\nwindow.__taborHints.update({});",
            Self::js_string(keys)
        );
        let proxy = self.event_proxy.clone();
        let window_id = self.display.window.id();
        let tab_id = self.tab_id;

        self.web_eval_js_string(&script, move |result| {
            let Some(url) = result.filter(|url| !url.is_empty()) else {
                return;
            };

            let command = match action {
                WebHintAction::Open => WebCommand::OpenUrl { url, new_tab: false },
                WebHintAction::OpenNewTab => WebCommand::OpenUrl { url, new_tab: true },
                WebHintAction::CopyLink => WebCommand::CopyToClipboard { text: url },
            };

            let event = Event::for_tab(EventType::WebCommand(command), window_id, tab_id);
            let _ = proxy.send_event(event);
        });
    }

    fn web_hints_cancel(&mut self) {
        self.web_exec_js("if (window.__taborHints) { window.__taborHints.cancel(); }");
    }

    fn web_request_mark_set(&mut self, name: char, url: String) {
        let script = "JSON.stringify({x: window.scrollX, y: window.scrollY})";
        let proxy = self.event_proxy.clone();
        let window_id = self.display.window.id();
        let tab_id = self.tab_id;

        self.web_eval_js_string(script, move |result| {
            let Some(result) = result else {
                return;
            };
            let value: serde_json::Value = match serde_json::from_str(&result) {
                Ok(value) => value,
                Err(_) => return,
            };
            let scroll_x = value.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let scroll_y = value.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let command = WebCommand::SetMark { name, url, scroll_x, scroll_y };
            let event = Event::for_tab(EventType::WebCommand(command), window_id, tab_id);
            let _ = proxy.send_event(event);
        });
    }

    fn web_start_visual_selection(&mut self) {
        let script = r#"(function() {
  const sel = window.getSelection();
  if (!sel) return;
  if (sel.rangeCount === 0) {
    let range = null;
    const x = window.innerWidth / 2;
    const y = window.innerHeight / 2;
    if (document.caretRangeFromPoint) {
      range = document.caretRangeFromPoint(x, y);
    } else if (document.caretPositionFromPoint) {
      const pos = document.caretPositionFromPoint(x, y);
      if (pos) {
        range = document.createRange();
        range.setStart(pos.offsetNode, pos.offset);
      }
    }
    if (!range) {
      range = document.createRange();
      range.setStart(document.body, 0);
    }
    range.collapse(true);
    sel.removeAllRanges();
    sel.addRange(range);
  }
})();"#;
        self.web_exec_js(script);
    }

    fn web_visual_move(&mut self, direction: &str, granularity: &str) {
        let script = format!(
            "(function() {{ const sel = window.getSelection(); if (!sel) return; sel.modify('extend', '{}', '{}'); }})();",
            direction, granularity
        );
        self.web_exec_js(&script);
    }

    fn web_caret_move(&mut self, direction: &str, granularity: &str) {
        let script = format!(
            "(function() {{ const sel = window.getSelection(); if (!sel) return; sel.modify('move', '{}', '{}'); }})();",
            direction, granularity
        );
        self.web_exec_js(&script);
    }

    fn web_copy_selection(&mut self) {
        let proxy = self.event_proxy.clone();
        let window_id = self.display.window.id();
        let tab_id = self.tab_id;
        self.web_eval_js_string("window.getSelection().toString()", move |result| {
            let Some(text) = result.filter(|text| !text.is_empty()) else {
                return;
            };
            let command = WebCommand::CopyToClipboard { text };
            let event = Event::for_tab(EventType::WebCommand(command), window_id, tab_id);
            let _ = proxy.send_event(event);
        });
    }

    fn web_scroll_by(&mut self, dx: f64, dy: f64) {
        let script = format!("window.scrollBy({dx}, {dy});");
        self.web_exec_js(&script);
    }

    fn web_scroll_to(&mut self, x: f64, y: f64) {
        let script = format!("window.scrollTo({x}, {y});");
        self.web_exec_js(&script);
    }

    fn web_scroll_half_page(&mut self, down: bool) {
        let direction = if down { 1.0 } else { -1.0 };
        let script = format!("window.scrollBy(0, window.innerHeight / 2 * {direction});");
        self.web_exec_js(&script);
    }

    fn web_scroll_top(&mut self) {
        self.web_exec_js("window.scrollTo(window.scrollX, 0);");
    }

    fn web_scroll_bottom(&mut self) {
        self.web_exec_js(
            "window.scrollTo(window.scrollX, Math.max(document.body.scrollHeight, document.documentElement.scrollHeight));",
        );
    }

    fn web_scroll_far_left(&mut self) {
        self.web_exec_js("window.scrollTo(0, window.scrollY);");
    }

    fn web_scroll_far_right(&mut self) {
        self.web_exec_js(
            "window.scrollTo(Math.max(document.body.scrollWidth, document.documentElement.scrollWidth), window.scrollY);",
        );
    }

    fn web_go_back(&mut self) {
        if let Some(web_view) = self.web_view.as_mut() {
            web_view.go_back();
        }
    }

    fn web_go_forward(&mut self) {
        if let Some(web_view) = self.web_view.as_mut() {
            web_view.go_forward();
        }
    }

    fn web_open_command_bar(&mut self, input: &str) {
        self.start_command_prompt(':', input);
    }

    fn web_start_find(&mut self) {
        self.start_command_prompt('/', "");
    }

    fn web_find(&mut self, query: &str, backwards: bool) {
        let script = format!(
            "window.find({}, false, {}, true, false, true, false);",
            Self::js_string(query),
            if backwards { "true" } else { "false" }
        );
        self.web_exec_js(&script);
    }

    fn web_focus_input(&mut self) {
        let script = r#"(function() {
  const el = document.querySelector("input, textarea, select, [contenteditable='true']");
  if (el) {
    el.focus();
    if (el.select) { el.select(); }
  }
})();"#;
        self.web_exec_js(script);
    }

    fn web_view_source(&mut self) {
        let Some(current) = self.current_web_url() else {
            self.push_command_error(String::from("No active URL"));
            return;
        };
        let url = if current.starts_with("view-source:") {
            current
        } else {
            format!("view-source:{current}")
        };
        self.open_web_url(url);
    }

    fn web_follow_rel(&mut self, rel: &str) {
        let rel = Self::js_string(rel);
        let script = format!(
            "(function() {{
  const rel = {rel};
  const link = document.querySelector(`link[rel~=\"${{rel}}\"], a[rel~=\"${{rel}}\"]`);
  if (link && link.href) {{
    window.location.href = link.href;
    return;
  }}
  const pattern = rel === \"prev\" ? /(prev|previous)/i : /(next)/i;
  for (const a of Array.from(document.querySelectorAll(\"a[href]\"))) {{
    const text = (a.textContent || \"\").trim();
    if (pattern.test(text)) {{
      window.location.href = a.href;
      return;
    }}
  }}
}})();"
        );
        self.web_exec_js(&script);
    }

    fn web_copy_url(&mut self) {
        let Some(url) = self.current_web_url() else {
            self.push_command_error(String::from("No active URL"));
            return;
        };
        self.clipboard.store(ClipboardType::Clipboard, url);
    }

    fn web_open_clipboard(&mut self, new_tab: bool) {
        let raw = self.clipboard.load(ClipboardType::Clipboard);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            self.push_command_error(String::from("Clipboard is empty"));
            return;
        }

        let url = normalize_web_url(trimmed);
        if new_tab {
            self.open_web_url_new_tab(url);
        } else {
            self.open_web_url(url);
        }
    }

    fn web_new_tab(&mut self) {
        self.open_web_url_new_tab(String::from("about:blank"));
    }

    fn web_close_tab(&mut self) {
        let event = Event::new(EventType::CloseTab(self.tab_id), self.display.window.id());
        let _ = self.event_proxy.send_event(event);
    }

    fn web_restore_tab(&mut self) {
        let event = Event::new(EventType::RestoreTab, self.display.window.id());
        let _ = self.event_proxy.send_event(event);
    }

    fn web_show_help(&mut self) {
        let html = Self::js_string(WEB_HELP_HTML);
        let script = format!(
            "(function() {{
  const existing = document.getElementById(\"__tabor_help\");
  if (existing) {{ existing.remove(); }}
  const overlay = document.createElement(\"div\");
  overlay.id = \"__tabor_help\";
  overlay.style.position = \"fixed\";
  overlay.style.top = \"10%\";
  overlay.style.left = \"10%\";
  overlay.style.right = \"10%\";
  overlay.style.maxHeight = \"80%\";
  overlay.style.overflow = \"auto\";
  overlay.style.background = \"rgba(20,20,20,0.92)\";
  overlay.style.color = \"#f2f2f2\";
  overlay.style.padding = \"16px\";
  overlay.style.borderRadius = \"8px\";
  overlay.style.boxShadow = \"0 12px 40px rgba(0,0,0,0.45)\";
  overlay.style.zIndex = \"2147483647\";
  overlay.innerHTML = {html};
  document.body.appendChild(overlay);
}})();"
        );
        self.web_exec_js(&script);
    }

    fn web_hide_help(&mut self) {
        self.web_exec_js(
            "(function() { const existing = document.getElementById(\"__tabor_help\"); if (existing) { existing.remove(); } })();",
        );
    }

    fn web_up_url(&mut self, root: bool) {
        let Some(current) = self.current_web_url() else {
            self.push_command_error(String::from("No active URL"));
            return;
        };

        let current = current.strip_prefix("view-source:").unwrap_or(&current);
        let Ok(mut parsed) = Url::parse(current) else {
            self.push_command_error(String::from("Invalid URL"));
            return;
        };

        if parsed.cannot_be_a_base() {
            self.push_command_error(String::from("Unsupported URL"));
            return;
        }

        parsed.set_query(None);
        parsed.set_fragment(None);

        if root {
            parsed.set_path("/");
        } else if let Some(segments) = parsed.path_segments() {
            let mut parts: Vec<_> = segments.collect();
            if !parts.is_empty() {
                parts.pop();
            }
            let mut new_path = String::from("/");
            if !parts.is_empty() {
                new_path.push_str(&parts.join("/"));
                new_path.push('/');
            }
            parsed.set_path(&new_path);
        }

        self.open_web_url(parsed.to_string());
    }
}

#[cfg(target_os = "macos")]
impl<'a, N: Notify + 'a, T: EventListener> WebActions for ActionContext<'a, N, T> {
    fn scroll_by(&mut self, dx: f64, dy: f64) {
        self.web_scroll_by(dx, dy);
    }

    fn scroll_half_page(&mut self, down: bool) {
        self.web_scroll_half_page(down);
    }

    fn scroll_top(&mut self) {
        self.web_scroll_top();
    }

    fn scroll_bottom(&mut self) {
        self.web_scroll_bottom();
    }

    fn scroll_far_left(&mut self) {
        self.web_scroll_far_left();
    }

    fn scroll_far_right(&mut self) {
        self.web_scroll_far_right();
    }

    fn scroll_to(&mut self, x: f64, y: f64) {
        self.web_scroll_to(x, y);
    }

    fn go_back(&mut self) {
        self.web_go_back();
    }

    fn go_forward(&mut self) {
        self.web_go_forward();
    }

    fn open_command_bar(&mut self, input: &str) {
        self.web_open_command_bar(input);
    }

    fn start_find_prompt(&mut self) {
        self.web_start_find();
    }

    fn find(&mut self, query: &str, backwards: bool) {
        self.web_find(query, backwards);
    }

    fn hints_start(&mut self, action: WebHintAction) {
        self.web_hints_start(action);
    }

    fn hints_update(&mut self, keys: &str, action: WebHintAction) {
        self.web_hints_update(keys, action);
    }

    fn hints_cancel(&mut self) {
        self.web_hints_cancel();
    }

    fn copy_selection(&mut self) {
        self.web_copy_selection();
    }

    fn clear_selection(&mut self) {
        self.web_clear_selection();
    }

    fn start_visual_selection(&mut self) {
        self.web_start_visual_selection();
    }

    fn visual_move(&mut self, direction: &str, granularity: &str) {
        self.web_visual_move(direction, granularity);
    }

    fn focus_input(&mut self) {
        self.web_focus_input();
    }

    fn blur_active_element(&mut self) {
        self.web_blur_active_element();
    }

    fn insert_text(&mut self, text: &str) {
        let script =
            format!("document.execCommand('insertText', false, {});", Self::js_string(text));
        self.web_exec_js(&script);
    }

    fn delete_backward(&mut self) {
        self.web_exec_js("document.execCommand('deleteBackward');");
    }

    fn delete_forward(&mut self) {
        self.web_exec_js("document.execCommand('deleteForward');");
    }

    fn insert_paragraph(&mut self) {
        self.web_exec_js("document.execCommand('insertParagraph');");
    }

    fn insert_tab(&mut self) {
        let script =
            format!("document.execCommand('insertText', false, {});", Self::js_string("\t"));
        self.web_exec_js(&script);
    }

    fn caret_move(&mut self, direction: &str, granularity: &str) {
        self.web_caret_move(direction, granularity);
    }

    fn view_source(&mut self) {
        self.web_view_source();
    }

    fn follow_rel(&mut self, rel: &str) {
        self.web_follow_rel(rel);
    }

    fn copy_url(&mut self) {
        self.web_copy_url();
    }

    fn open_clipboard(&mut self, new_tab: bool) {
        self.web_open_clipboard(new_tab);
    }

    fn up_url(&mut self, root: bool) {
        self.web_up_url(root);
    }

    fn new_tab(&mut self) {
        self.web_new_tab();
    }

    fn close_tab(&mut self) {
        self.web_close_tab();
    }

    fn restore_tab(&mut self) {
        self.web_restore_tab();
    }

    fn select_previous_tab(&mut self) {
        input::ActionContext::select_previous_tab(self);
    }

    fn select_next_tab(&mut self) {
        input::ActionContext::select_next_tab(self);
    }

    fn select_tab_at_index(&mut self, index: usize) {
        input::ActionContext::select_tab_at_index(self, index);
    }

    fn select_last_tab(&mut self) {
        input::ActionContext::select_last_tab(self);
    }

    fn reload(&mut self) {
        self.reload_web();
    }

    fn show_help(&mut self) {
        self.web_show_help();
    }

    fn hide_help(&mut self) {
        self.web_hide_help();
    }

    fn request_mark_set(&mut self, name: char, url: String) {
        self.web_request_mark_set(name, url);
    }

    fn current_url(&mut self) -> Option<String> {
        self.current_web_url()
    }

    fn open_url(&mut self, url: String) {
        self.open_web_url(url);
    }

    fn push_error(&mut self, message: String) {
        self.push_command_error(message);
    }
}

fn command_url_prefix(input: &str) -> Option<(usize, &str)> {
    let bytes = input.as_bytes();
    if bytes.len() < 2 || bytes[0] != b':' {
        return None;
    }

    let cmd = bytes[1] as char;
    if !matches!(cmd, 'o' | 'O' | 'b' | 'B') {
        return None;
    }

    if bytes.len() > 2 && bytes[2] != b' ' {
        return None;
    }

    let rest = &input[2..];
    let trimmed = rest.trim_start();
    let start = input.len() - trimmed.len();
    Some((start, trimmed))
}

#[cfg(test)]
mod tests {
    use super::{CommandHistory, command_url_prefix};

    #[test]
    fn command_url_prefix_parses_basic() {
        assert_eq!(command_url_prefix(":o"), Some((2, "")));
        assert_eq!(command_url_prefix(":o "), Some((3, "")));
        assert_eq!(command_url_prefix(":o  test"), Some((4, "test")));
        assert_eq!(command_url_prefix(":O test"), Some((3, "test")));
        assert_eq!(command_url_prefix(":b test"), Some((3, "test")));
        assert_eq!(command_url_prefix(":B test"), Some((3, "test")));
    }

    #[test]
    fn command_url_prefix_rejects_non_open() {
        assert_eq!(command_url_prefix(":r"), None);
        assert_eq!(command_url_prefix(":open"), None);
        assert_eq!(command_url_prefix(":t"), None);
    }

    #[test]
    fn command_history_records_most_recent() {
        let mut history = CommandHistory::default();
        history.record_url(String::from("https://example.com"));
        history.record_url(String::from("https://rust-lang.org"));
        history.record_url(String::from("https://example.com"));

        assert_eq!(history.urls[0], "https://example.com");
        assert_eq!(history.urls[1], "https://rust-lang.org");
    }

    #[test]
    fn command_history_cycles_completion() {
        let mut history = CommandHistory::default();
        history.record_url(String::from("https://example.com"));
        history.record_url(String::from("https://rust-lang.org"));

        let (first, first_index) = history.complete("https://", None).unwrap();
        assert_eq!(first, "https://rust-lang.org");

        let (second, _) = history.complete("https://", Some(first_index)).unwrap();
        assert_eq!(second, "https://example.com");
    }

}

/// Identified purpose of the touch input.
#[derive(Default, Debug)]
pub enum TouchPurpose {
    #[default]
    None,
    Select(TouchEvent),
    Scroll(TouchEvent),
    Zoom(TouchZoom),
    ZoomPendingSlot(TouchEvent),
    Tap(TouchEvent),
    Invalid(HashSet<u64, RandomState>),
}

/// Touch zooming state.
#[derive(Debug)]
pub struct TouchZoom {
    slots: (TouchEvent, TouchEvent),
    fractions: f32,
}

impl TouchZoom {
    pub fn new(slots: (TouchEvent, TouchEvent)) -> Self {
        Self { slots, fractions: Default::default() }
    }

    /// Get slot distance change since last update.
    pub fn font_delta(&mut self, slot: TouchEvent) -> f32 {
        let old_distance = self.distance();

        // Update touch slots.
        if slot.id == self.slots.0.id {
            self.slots.0 = slot;
        } else {
            self.slots.1 = slot;
        }

        // Calculate font change in `FONT_SIZE_STEP` increments.
        let delta = (self.distance() - old_distance) * TOUCH_ZOOM_FACTOR + self.fractions;
        let font_delta = (delta.abs() / FONT_SIZE_STEP).floor() * FONT_SIZE_STEP * delta.signum();
        self.fractions = delta - font_delta;

        font_delta
    }

    /// Get active touch slots.
    pub fn slots(&self) -> (TouchEvent, TouchEvent) {
        self.slots
    }

    /// Calculate distance between slots.
    fn distance(&self) -> f32 {
        let delta_x = self.slots.0.location.x - self.slots.1.location.x;
        let delta_y = self.slots.0.location.y - self.slots.1.location.y;
        delta_x.hypot(delta_y) as f32
    }
}

/// State of the mouse.
#[derive(Debug)]
pub struct Mouse {
    pub left_button_state: ElementState,
    pub middle_button_state: ElementState,
    pub right_button_state: ElementState,
    pub last_click_timestamp: Instant,
    pub last_click_button: MouseButton,
    pub click_state: ClickState,
    pub accumulated_scroll: AccumulatedScroll,
    pub cell_side: Side,
    pub block_hint_launcher: bool,
    pub hint_highlight_dirty: bool,
    pub inside_text_area: bool,
    pub x: usize,
    pub y: usize,
}

impl Default for Mouse {
    fn default() -> Mouse {
        Mouse {
            last_click_timestamp: Instant::now(),
            last_click_button: MouseButton::Left,
            left_button_state: ElementState::Released,
            middle_button_state: ElementState::Released,
            right_button_state: ElementState::Released,
            click_state: ClickState::None,
            cell_side: Side::Left,
            hint_highlight_dirty: Default::default(),
            block_hint_launcher: Default::default(),
            inside_text_area: Default::default(),
            accumulated_scroll: Default::default(),
            x: Default::default(),
            y: Default::default(),
        }
    }
}

impl Mouse {
    /// Convert mouse pixel coordinates to viewport point.
    ///
    /// If the coordinates are outside of the terminal grid, like positions inside the padding, the
    /// coordinates will be clamped to the closest grid coordinates.
    #[inline]
    pub fn point(&self, size: &SizeInfo, display_offset: usize) -> Point {
        let col = self.x.saturating_sub(size.padding_x() as usize) / (size.cell_width() as usize);
        let col = min(Column(col), size.last_column());

        let line = self.y.saturating_sub(size.padding_y() as usize) / (size.cell_height() as usize);
        let line = min(line, size.bottommost_line().0 as usize);

        term::viewport_to_point(display_offset, Point::new(line, col))
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ClickState {
    None,
    Click,
    DoubleClick,
    TripleClick,
}

/// The amount of scroll accumulated from the pointer events.
#[derive(Default, Debug)]
pub struct AccumulatedScroll {
    /// Scroll we should perform along `x` axis.
    pub x: f64,

    /// Scroll we should perform along `y` axis.
    pub y: f64,
}

impl input::Processor<EventProxy, ActionContext<'_, Notifier, EventProxy>> {
    /// Handle events from winit.
    pub fn handle_event(&mut self, event: WinitEvent<Event>) {
        match event {
            WinitEvent::UserEvent(Event { payload, .. }) => match payload {
                EventType::SearchNext => self.ctx.goto_match(None),
                EventType::Scroll(scroll) => self.ctx.scroll(scroll),
                EventType::BlinkCursor => {
                    // Only change state when timeout isn't reached, since we could get
                    // BlinkCursor and BlinkCursorTimeout events at the same time.
                    if !*self.ctx.cursor_blink_timed_out {
                        self.ctx.display.cursor_hidden ^= true;
                        *self.ctx.dirty = true;
                    }
                },
                EventType::BlinkCursorTimeout => {
                    // Disable blinking after timeout reached.
                    let timer_id = TimerId::new(Topic::BlinkCursor, self.ctx.display.window.id());
                    self.ctx.scheduler.unschedule(timer_id);
                    *self.ctx.cursor_blink_timed_out = true;
                    self.ctx.display.cursor_hidden = false;
                    *self.ctx.dirty = true;
                },
                #[cfg(target_os = "macos")]
                EventType::WebCommand(command) => {
                    self.ctx.handle_web_command(command);
                },
                // Add message only if it's not already queued.
                EventType::Message(message) if !self.ctx.message_buffer.is_queued(&message) => {
                    self.ctx.message_buffer.push(message);
                    self.ctx.display.pending_update.dirty = true;
                },
                EventType::Terminal(event) => match event {
                    TerminalEvent::Title(title) => {
                        if !self.ctx.preserve_title && self.ctx.config.window.dynamic_title {
                            self.ctx.window().set_title(title);
                        }
                    },
                    TerminalEvent::ResetTitle => {
                        let window_config = &self.ctx.config.window;
                        if !self.ctx.preserve_title && window_config.dynamic_title {
                            self.ctx.display.window.set_title(window_config.identity.title.clone());
                        }
                    },
                    TerminalEvent::Bell => {
                        // Set window urgency hint when window is not focused.
                        let focused = self.ctx.terminal.is_focused;
                        if !focused && self.ctx.terminal.mode().contains(TermMode::URGENCY_HINTS) {
                            self.ctx.window().set_urgent(true);
                        }

                        // Ring visual bell.
                        self.ctx.display.visual_bell.ring();

                        // Execute bell command.
                        if let Some(bell_command) = &self.ctx.config.bell.command {
                            if self
                                .ctx
                                .prev_bell_cmd
                                .is_none_or(|i| i.elapsed() >= BELL_CMD_COOLDOWN)
                            {
                                self.ctx.spawn_daemon(bell_command.program(), bell_command.args());

                                *self.ctx.prev_bell_cmd = Some(Instant::now());
                            }
                        }
                    },
                    TerminalEvent::ClipboardStore(clipboard_type, content) => {
                        if self.ctx.terminal.is_focused {
                            self.ctx.clipboard.store(clipboard_type, content);
                        }
                    },
                    TerminalEvent::ClipboardLoad(clipboard_type, format) => {
                        if self.ctx.terminal.is_focused {
                            let text = format(self.ctx.clipboard.load(clipboard_type).as_str());
                            self.ctx.write_to_pty(text.into_bytes());
                        }
                    },
                    TerminalEvent::ColorRequest(index, format) => {
                        let color = match self.ctx.terminal().colors()[index] {
                            Some(color) => Rgb(color),
                            // Ignore cursor color requests unless it was changed.
                            None if index == NamedColor::Cursor as usize => return,
                            None => self.ctx.display.colors[index],
                        };
                        self.ctx.write_to_pty(format(color.0).into_bytes());
                    },
                    TerminalEvent::TextAreaSizeRequest(format) => {
                        let text = format(self.ctx.size_info().into());
                        self.ctx.write_to_pty(text.into_bytes());
                    },
                    TerminalEvent::PtyWrite(text) => self.ctx.write_to_pty(text.into_bytes()),
                    TerminalEvent::MouseCursorDirty => self.reset_mouse_cursor(),
                    TerminalEvent::CursorBlinkingChange => self.ctx.update_cursor_blinking(),
                    TerminalEvent::Exit | TerminalEvent::ChildExit(_) | TerminalEvent::Wakeup => (),
                },
                #[cfg(unix)]
                EventType::IpcRequest(..) => (),
                #[cfg(target_os = "macos")]
                EventType::Message(_)
                | EventType::ConfigReload(_)
                | EventType::CreateWindow(_)
                | EventType::CreateTab(_)
                | EventType::TabCommand(_)
                | EventType::UpdateTabProgramName
                | EventType::TabActivityTick
                | EventType::CloseTab(_)
                | EventType::RestoreTab
                | EventType::WebFavicon { .. }
                | EventType::WebCursor { .. }
                | EventType::WebCursorRequest
                | EventType::TabSearch(_)
                | EventType::Frame => (),
                #[cfg(not(target_os = "macos"))]
                EventType::Message(_)
                | EventType::ConfigReload(_)
                | EventType::CreateWindow(_)
                | EventType::CreateTab(_)
                | EventType::TabCommand(_)
                | EventType::UpdateTabProgramName
                | EventType::TabActivityTick
                | EventType::Frame => (),
            },
            WinitEvent::WindowEvent { event, .. } => {
                match event {
                    WindowEvent::CloseRequested => {
                        // User asked to close the window, so no need to hold it.
                        self.ctx.window().hold = false;
                        #[cfg(target_os = "macos")]
                        if self.ctx.window_kind().is_web() {
                            let event = Event::new(
                                EventType::CloseTab(self.ctx.tab_id),
                                self.ctx.display.window.id(),
                            );
                            let _ = self.ctx.event_proxy.send_event(event);
                        } else {
                            self.ctx.terminal.exit();
                        }
                        #[cfg(not(target_os = "macos"))]
                        self.ctx.terminal.exit();
                    },
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        let old_scale_factor =
                            mem::replace(&mut self.ctx.window().scale_factor, scale_factor);

                        let display_update_pending = &mut self.ctx.display.pending_update;

                        // Rescale font size for the new factor.
                        let font_scale = scale_factor as f32 / old_scale_factor as f32;
                        self.ctx.display.font_size = self.ctx.display.font_size.scale(font_scale);

                        let font = self.ctx.config.font.clone();
                        display_update_pending.set_font(font.with_size(self.ctx.display.font_size));
                    },
                    WindowEvent::Resized(size) => {
                        // Ignore resize events to zero in any dimension, to avoid issues with Winit
                        // and the ConPTY. A 0x0 resize will also occur when the window is minimized
                        // on Windows.
                        if size.width == 0 || size.height == 0 {
                            return;
                        }

                        self.ctx.display.pending_update.set_dimensions(size);
                    },
                    WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                        self.key_input(event);
                    },
                    WindowEvent::ModifiersChanged(modifiers) => self.modifiers_input(modifiers),
                    WindowEvent::MouseInput { state, button, .. } => {
                        self.ctx.window().set_mouse_visible(true);
                        self.mouse_input(state, button);
                    },
                    WindowEvent::CursorMoved { position, .. } => {
                        self.ctx.window().set_mouse_visible(true);
                        self.mouse_moved(position);
                    },
                    WindowEvent::MouseWheel { delta, phase, .. } => {
                        self.ctx.window().set_mouse_visible(true);
                        self.mouse_wheel_input(delta, phase);
                    },
                    WindowEvent::Touch(touch) => self.touch(touch),
                    WindowEvent::Focused(is_focused) => {
                        self.ctx.terminal.is_focused = is_focused;

                        // When the unfocused hollow is used we must redraw on focus change.
                        if self.ctx.config.cursor.unfocused_hollow {
                            *self.ctx.dirty = true;
                        }

                        // Reset the urgency hint when gaining focus.
                        if is_focused {
                            self.ctx.window().set_urgent(false);
                        }

                        self.ctx.update_cursor_blinking();
                        self.on_focus_change(is_focused);

                        // Ensure IME is disabled while unfocused.
                        self.ctx.window().set_ime_inhibitor(ImeInhibitor::FOCUS, !is_focused);
                    },
                    WindowEvent::Occluded(occluded) => {
                        *self.ctx.occluded = occluded;
                    },
                    WindowEvent::DroppedFile(path) => {
                        let path: String = path.to_string_lossy().into();
                        self.ctx.paste(&(path + " "), true);
                    },
                    WindowEvent::CursorLeft { .. } => {
                        self.ctx.mouse.inside_text_area = false;

                        if self.ctx.display().highlighted_hint.is_some() {
                            *self.ctx.dirty = true;
                        }
                    },
                    WindowEvent::Ime(ime) => match ime {
                        Ime::Commit(text) => {
                            *self.ctx.dirty = true;
                            // Don't use bracketed paste for single char input.
                            self.ctx.paste(&text, text.chars().count() > 1);
                            self.ctx.update_cursor_blinking();
                        },
                        Ime::Preedit(text, cursor_offset) => {
                            let preedit =
                                (!text.is_empty()).then(|| Preedit::new(text, cursor_offset));

                            if self.ctx.display.ime.preedit() != preedit.as_ref() {
                                self.ctx.display.ime.set_preedit(preedit);
                                self.ctx.update_cursor_blinking();
                                *self.ctx.dirty = true;
                            }
                        },
                        Ime::Enabled => {
                            self.ctx.display.ime.set_enabled(true);
                            *self.ctx.dirty = true;
                        },
                        Ime::Disabled => {
                            self.ctx.display.ime.set_enabled(false);
                            *self.ctx.dirty = true;
                        },
                    },
                    WindowEvent::KeyboardInput { is_synthetic: true, .. }
                    | WindowEvent::ActivationTokenDone { .. }
                    | WindowEvent::DoubleTapGesture { .. }
                    | WindowEvent::TouchpadPressure { .. }
                    | WindowEvent::RotationGesture { .. }
                    | WindowEvent::CursorEntered { .. }
                    | WindowEvent::PinchGesture { .. }
                    | WindowEvent::AxisMotion { .. }
                    | WindowEvent::PanGesture { .. }
                    | WindowEvent::HoveredFileCancelled
                    | WindowEvent::Destroyed
                    | WindowEvent::ThemeChanged(_)
                    | WindowEvent::HoveredFile(_)
                    | WindowEvent::RedrawRequested
                    | WindowEvent::Moved(_) => (),
                }
            },
            WinitEvent::Suspended
            | WinitEvent::NewEvents { .. }
            | WinitEvent::DeviceEvent { .. }
            | WinitEvent::LoopExiting
            | WinitEvent::Resumed
            | WinitEvent::MemoryWarning
            | WinitEvent::AboutToWait => (),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventProxy {
    proxy: EventLoopProxy<Event>,
    window_id: WindowId,
    tab_id: TabId,
}

impl EventProxy {
    pub fn new(proxy: EventLoopProxy<Event>, window_id: WindowId, tab_id: TabId) -> Self {
        Self { proxy, window_id, tab_id }
    }

    /// Send an event to the event loop.
    pub fn send_event(&self, event: EventType) {
        let _ = self.proxy.send_event(Event::for_tab(event, self.window_id, self.tab_id));
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TerminalEvent) {
        let _ = self.proxy.send_event(Event::for_tab(event.into(), self.window_id, self.tab_id));
    }
}
