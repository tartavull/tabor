//! Terminal window context.

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::collections::VecDeque;
use std::error::Error;
use std::fs::File;
#[cfg(target_os = "macos")]
use std::io::Cursor;
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::Mutex;
use std::time::Instant;

#[cfg(target_os = "macos")]
use base64::Engine;
#[cfg(target_os = "macos")]
use base64::engine::general_purpose::STANDARD as BASE64;
use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use glutin::platform::x11::X11GlConfigExt;
#[cfg(target_os = "macos")]
use image::{DynamicImage, RgbaImage};
use log::info;
#[cfg(target_os = "macos")]
use serde::Deserialize;
use serde_json as json;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Event as WinitEvent, Ime, Modifiers, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
#[cfg(target_os = "macos")]
use winit::window::CursorIcon;
use winit::window::WindowId;

use tabor_terminal::event::{Event as TerminalEvent, Notify, OnResize};
use tabor_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use tabor_terminal::grid::{Dimensions, Scroll};
use tabor_terminal::index::{Column, Direction, Line, Point};
use tabor_terminal::sync::FairMutex;
#[cfg(target_os = "macos")]
use tabor_terminal::term::MIN_COLUMNS;
use tabor_terminal::term::cell::Flags;
use tabor_terminal::term::test::TermSize;
use tabor_terminal::term::{ResizeAnchor, Term, TermMode};
use tabor_terminal::tty;
use tabor_terminal::vte::ansi::{CursorShape, NamedColor};

use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
#[cfg(unix)]
use crate::config::Action;
use crate::config::UiConfig;
#[cfg(not(windows))]
use crate::daemon::{foreground_process_name, foreground_process_path};
use crate::display::browser_layout::{BrowserViewMode, BrowserViewportLayout};
use crate::display::color::Rgb;
use crate::display::content::RenderableContentContext;
use crate::display::hint::HintState;
use crate::display::terminal_layout::{TerminalViewMode, TerminalViewportLayout};
use crate::display::window::Window;
use crate::display::{Display, DisplayUpdateOptions, SizeInfo};
#[cfg(target_os = "macos")]
use crate::display::{TabPanelEditOutcome, TabPanelEditTarget};
#[cfg(target_os = "macos")]
use crate::event::WebCommand;
use crate::event::{
    ActionContext, CommandFooterFeedback, CommandHistory, CommandState, Event, EventProxy,
    EventType, InlineSearchState, Mouse, MultiColumnCommand, MultiColumnCommandScope, SearchState,
    TouchPurpose, request_image_load, request_pdf_load, request_pdf_raster,
    request_web_cursor_update,
};
#[cfg(unix)]
use crate::input::ActionContext as _;
#[cfg(unix)]
use crate::ipc;
#[cfg(unix)]
use crate::ipc::{
    AgentActResult, AgentAction, AgentElementDetail, AgentEvent, AgentObservation, AgentPdf,
    AgentScreenshot, IpcBrowserAccelerationInfo, IpcBrowserAccelerationState,
    IpcBrowserLayoutState, IpcCefPumpMetrics, IpcError, IpcErrorCode, IpcImageDebugSnapshot,
    IpcImageLoadState, IpcImageScaleMode, IpcImageViewState, IpcInspectorMessage,
    IpcInspectorSession, IpcInspectorTarget, IpcPdfViewState, IpcRuntimeMetrics, IpcTabActivity,
    IpcTabGroup, IpcTabId, IpcTabKind, IpcTabPanelState, IpcTabState, IpcTerminalCell,
    IpcTerminalCursor, IpcTerminalCursorShape, IpcTerminalLayoutState, IpcTerminalLayoutStrip,
    IpcTerminalObservation, IpcTerminalPoint, IpcTerminalRead, IpcTerminalReadScope,
    IpcTerminalSelection, IpcTerminalSessionState, IpcTerminalViewportLine, IpcTerminalVisualPoint,
    IpcTouchPhase, IpcWebCloseMetrics, IpcWebFrameDeliveryMode, IpcWebMode, IpcWebViewMetrics,
    IpcWindowDebugButton, IpcWindowDebugRect, IpcWindowDebugSnapshot, IpcWindowDebugState,
    SocketReply, TabSelection, TerminalKeyInput,
};
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
use crate::scheduler::Scheduler;
use crate::tab_panel::TabActivity;
use crate::tabs::TabId;
use crate::window_kind::WindowKind;
#[cfg(unix)]
use crate::workspace::{
    self, PersistedTerminalState, WorkspaceGroupLayout, WorkspaceLayout, WorkspaceTabKind,
    WorkspaceTabLayout,
};
use crate::{input, renderer};

#[cfg(target_os = "macos")]
use crate::macos::favicon::{FaviconImage, fetch_favicon, resolve_favicon_url};
#[cfg(target_os = "macos")]
use crate::macos::image_view::{ImageLoadState, ImageScaleMode, ImageViewState};
#[cfg(target_os = "macos")]
use crate::macos::open_url::{OpenUrlKind, classify_open_url};
#[cfg(target_os = "macos")]
use crate::macos::pdf_view::{
    LoadedPdfSource, PdfLoadState, PdfRasterizedPage, PdfViewState, PdfZoomMode,
};
#[cfg(target_os = "macos")]
use crate::macos::web_commands::WebCommandState;
#[cfg(target_os = "macos")]
use crate::macos::webview::{WebAccelerationState, WebView};
#[cfg(target_os = "macos")]
use crate::tab_panel::TabFavicon;
#[cfg(target_os = "macos")]
use crate::tab_panel_icons::FIRST_DYNAMIC_FAVICON_CHAR;
#[cfg(target_os = "macos")]
use serde_json::Value as JsonValue;

struct TabState {
    id: TabId,
    title: String,
    custom_title: Option<String>,
    program_name: String,
    kind: WindowKind,
    activity: TabActivity,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    search_state: SearchState,
    inline_search_state: InlineSearchState,
    command_state: CommandState,
    command_footer_feedback: CommandFooterFeedback,
    terminal_view_mode: TerminalViewMode,
    terminal_multi_column_count_override: Option<usize>,
    browser_view_mode: BrowserViewMode,
    browser_multi_column_count_override: Option<usize>,
    mouse: Mouse,
    touch: TouchPurpose,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    #[cfg(target_os = "macos")]
    web_view: Option<WebView>,
    #[cfg(target_os = "macos")]
    image_view: Option<ImageViewState>,
    #[cfg(target_os = "macos")]
    pdf_view: Option<PdfViewState>,
    #[cfg(target_os = "macos")]
    web_command_state: WebCommandState,
    #[cfg(target_os = "macos")]
    agent_runtime: AgentRuntimeState,
    #[cfg(target_os = "macos")]
    favicon: Option<TabFavicon>,
    #[cfg(target_os = "macos")]
    favicon_pending: bool,
    notifier: TabNotifier,
    terminal_runtime: TerminalRuntimeState,
}

enum TabNotifier {
    Local {
        notifier: Notifier,
        #[cfg(unix)]
        terminal_id: Option<u64>,
    },
    #[cfg(unix)]
    Disconnected,
}

impl Notify for TabNotifier {
    fn notify<B>(&self, bytes: B)
    where
        B: Into<std::borrow::Cow<'static, [u8]>>,
    {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }

        match self {
            Self::Local { notifier, .. } => notifier.notify(bytes),
            #[cfg(unix)]
            Self::Disconnected => {},
        }
    }
}

impl OnResize for TabNotifier {
    fn on_resize(&mut self, window_size: tabor_terminal::event::WindowSize) {
        match self {
            Self::Local {
                notifier,
                #[cfg(unix)]
                terminal_id,
            } => {
                notifier.on_resize(window_size);
                #[cfg(unix)]
                if let Some(terminal_id) = terminal_id {
                    workspace::record_terminal_resize(*terminal_id, window_size);
                }
            },
            #[cfg(unix)]
            Self::Disconnected => {},
        }
    }
}

impl TabNotifier {
    fn close_terminal(&mut self) {
        match self {
            Self::Local { notifier, .. } => {
                let _ = notifier.0.send(Msg::Shutdown);
            },
            #[cfg(unix)]
            Self::Disconnected => {},
        }
    }

    fn detach(&mut self) {
        if let Self::Local { notifier, .. } = self {
            let _ = notifier.0.send(Msg::Shutdown);
        }
    }
}

#[derive(Clone)]
enum TerminalRuntimeState {
    Local {
        #[cfg(not(windows))]
        master_fd: RawFd,
        #[cfg(not(windows))]
        shell_pid: u32,
        #[cfg(unix)]
        terminal_id: Option<u64>,
        launch_options: tty::Options,
    },
    #[cfg(unix)]
    Disconnected {
        terminal_id: u64,
        launch_options: tty::Options,
        working_directory: Option<PathBuf>,
        clean_exit: bool,
        exit_code: Option<i32>,
    },
}

impl TerminalRuntimeState {
    #[cfg(not(windows))]
    fn master_fd_shell_pid(&self) -> Option<(RawFd, u32)> {
        match self {
            Self::Local { master_fd, shell_pid, .. } => Some((*master_fd, *shell_pid)),
            #[cfg(unix)]
            Self::Disconnected { .. } => None,
        }
    }

    fn working_directory_hint(&self) -> Option<PathBuf> {
        match self {
            Self::Local { launch_options, .. } => launch_options.working_directory.clone(),
            #[cfg(unix)]
            Self::Disconnected { working_directory, launch_options, .. } => {
                working_directory.clone().or_else(|| launch_options.working_directory.clone())
            },
        }
    }

    fn launch_options(&self) -> &tty::Options {
        match self {
            Self::Local { launch_options, .. } => launch_options,
            #[cfg(unix)]
            Self::Disconnected { launch_options, .. } => launch_options,
        }
    }

    #[cfg(unix)]
    fn terminal_id(&self) -> Option<u64> {
        match self {
            Self::Local { terminal_id, .. } => *terminal_id,
            Self::Disconnected { terminal_id, .. } => Some(*terminal_id),
        }
    }
}

#[cfg(target_os = "macos")]
struct ClosedTab {
    kind: WindowKind,
    browser_view_mode: BrowserViewMode,
    browser_multi_column_count_override: Option<usize>,
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
struct MultiColumnDefaults {
    terminal: Option<usize>,
    browser: Option<usize>,
}

#[derive(Clone)]
enum TerminalSpawnMode {
    NewLocal {
        terminal_id: Option<u64>,
    },
    #[cfg(unix)]
    RestoredLocalShell(Box<RestoredTerminalSpawn>),
}

#[cfg(unix)]
#[derive(Clone)]
struct RestoredTerminalSpawn {
    terminal_id: u64,
    launch_options: tty::Options,
    preview_lines: Option<Vec<String>>,
    title: Option<String>,
    working_directory: Option<PathBuf>,
}

struct LocalTerminalSpawnRequest<'a> {
    terminal: &'a Arc<FairMutex<Term<EventProxy>>>,
    event_proxy: &'a EventProxy,
    pty_config: &'a tty::Options,
    title: &'a str,
    window_id: WindowId,
    size_info: SizeInfo,
    ref_test: bool,
    terminal_id: Option<u64>,
}

fn terminal_view_mode_for_count(count: Option<usize>) -> TerminalViewMode {
    match count {
        Some(count) if count > 1 => TerminalViewMode::MultiColumn,
        _ => TerminalViewMode::Normal,
    }
}

fn browser_view_mode_for_count(count: Option<usize>) -> BrowserViewMode {
    match count {
        Some(count) if count > 1 => BrowserViewMode::MultiColumn,
        _ => BrowserViewMode::Normal,
    }
}

#[cfg(unix)]
fn non_empty_workspace_group_layouts(
    layout: &WorkspaceLayout,
) -> impl Iterator<Item = (usize, &WorkspaceGroupLayout)> {
    layout.groups.iter().enumerate().filter(|(_, group)| !group.tabs.is_empty())
}

fn sync_terminal_tab_geometry(
    display: &Display,
    config: &UiConfig,
    multi_column_defaults: MultiColumnDefaults,
    tab: &mut TabState,
) {
    if !tab.kind.is_terminal() {
        return;
    }

    let layout =
        WindowContext::terminal_viewport_for_tab(display, config, multi_column_defaults, tab);
    let new_size = layout.logical_size(&display.size_info);
    let mut terminal = tab.terminal.lock();
    if terminal.screen_lines() != new_size.screen_lines()
        || terminal.columns() != new_size.columns()
    {
        tab.notifier.on_resize(new_size.into());
        terminal.resize_with_anchor(new_size, ResizeAnchor::Top);
    }
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn spawn_local_terminal_runtime(
    request: LocalTerminalSpawnRequest<'_>,
) -> Result<(String, TabNotifier, TerminalRuntimeState), Box<dyn Error>> {
    let LocalTerminalSpawnRequest {
        terminal,
        event_proxy,
        pty_config,
        title,
        window_id,
        size_info,
        ref_test,
        terminal_id,
    } = request;

    let pty = tty::new(pty_config, size_info.into(), window_id.into())?;

    #[cfg(not(windows))]
    let master_fd = pty.file().as_raw_fd();
    #[cfg(not(windows))]
    let shell_pid = pty.child().id();

    let output_observer = if let Some(terminal_id) = terminal_id {
        Some(Box::new({
            let observer =
                workspace::register_live_terminal(workspace::LiveTerminalRegistration {
                    terminal_id,
                    terminal: Arc::clone(terminal),
                    launch_options: pty_config.clone(),
                    title: Some(title.to_string()),
                    #[cfg(not(windows))]
                    program_name: foreground_process_name(master_fd, shell_pid).unwrap_or_default(),
                    #[cfg(windows)]
                    program_name: String::new(),
                    working_directory: pty_config.working_directory.clone(),
                    #[cfg(not(windows))]
                    master_fd,
                    #[cfg(not(windows))]
                    shell_pid,
                })?;
            move |bytes: &[u8]| observer.observe(bytes)
        }) as Box<dyn FnMut(&[u8]) + Send>)
    } else {
        None
    };

    let event_loop = PtyEventLoop::new(
        Arc::clone(terminal),
        event_proxy.clone(),
        pty,
        output_observer,
        pty_config.drain_on_exit,
        ref_test,
    )?;

    let loop_tx = event_loop.channel();
    let _io_thread = event_loop.spawn();

    #[cfg(not(windows))]
    let program_name = foreground_process_name(master_fd, shell_pid).unwrap_or_default();
    #[cfg(windows)]
    let program_name = String::new();

    Ok((
        program_name,
        TabNotifier::Local {
            notifier: Notifier(loop_tx),
            #[cfg(unix)]
            terminal_id,
        },
        TerminalRuntimeState::Local {
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            #[cfg(unix)]
            terminal_id,
            launch_options: pty_config.clone(),
        },
    ))
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default, Clone, PartialEq)]
struct WebCloseMetrics {
    count: u64,
    last_ms: Option<f64>,
    max_ms: f64,
    total_ms: f64,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct AgentRuntimeState {
    preload_registered: bool,
    injected_once: bool,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
struct AgentScreenshotMeta {
    width: u32,
    height: u32,
    #[serde(default)]
    dpr: Option<f64>,
    #[serde(default)]
    scroll_x: Option<i64>,
    #[serde(default)]
    scroll_y: Option<i64>,
}

#[cfg(unix)]
pub(crate) struct IpcEventContext<'a> {
    pub event_loop: &'a ActiveEventLoop,
    pub event_proxy: &'a EventLoopProxy<Event>,
    pub clipboard: &'a mut Clipboard,
    pub scheduler: &'a mut Scheduler,
}

#[cfg(unix)]
pub(crate) struct WindowDebugMouseDrag {
    pub start: PhysicalPosition<f64>,
    pub end: PhysicalPosition<f64>,
    pub steps: Option<usize>,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct PendingAgentScreenshot {
    meta: Option<Result<AgentScreenshotMeta, String>>,
    data_base64: Option<Result<String, String>>,
}

#[cfg(target_os = "macos")]
impl WebCloseMetrics {
    fn record_close(&mut self, elapsed_ms: f64) {
        self.count = self.count.saturating_add(1);
        self.last_ms = Some(elapsed_ms);
        self.max_ms = self.max_ms.max(elapsed_ms);
        self.total_ms += elapsed_ms;
    }

    fn to_ipc(&self) -> IpcWebCloseMetrics {
        IpcWebCloseMetrics {
            count: self.count,
            last_ms: self.last_ms,
            max_ms: self.max_ms,
            total_ms: self.total_ms,
        }
    }
}

#[cfg(target_os = "macos")]
struct CefInspectorSession {
    tab_id: TabId,
    last_event_id: u64,
}

#[cfg(target_os = "macos")]
struct CefInspectorState {
    next_session_id: u64,
    sessions: HashMap<String, CefInspectorSession>,
    pending: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
}

#[cfg(target_os = "macos")]
impl CefInspectorState {
    fn new() -> Self {
        Self {
            next_session_id: 1,
            sessions: HashMap::new(),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn next_session_id(&mut self, target_id: u64) -> String {
        let id = self.next_session_id;
        self.next_session_id = self.next_session_id.saturating_add(1);
        format!("cef:{target_id}:{id}")
    }

    fn register_session(&self, session_id: &str) {
        let mut pending = self.pending.lock().unwrap();
        pending.entry(session_id.to_string()).or_default();
    }

    fn remove_session(&self, session_id: &str) {
        let mut pending = self.pending.lock().unwrap();
        pending.remove(session_id);
    }

    fn remove_sessions_for_tab(&mut self, tab_id: TabId) {
        let session_ids = self
            .sessions
            .iter()
            .filter(|&(_session_id, session)| session.tab_id == tab_id)
            .map(|(session_id, _session)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            self.sessions.remove(&session_id);
            self.remove_session(&session_id);
        }
    }

    fn drain_messages(&self, session_id: &str, max: usize) -> Vec<String> {
        let mut pending = self.pending.lock().unwrap();
        let Some(queue) = pending.get_mut(session_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while out.len() < max {
            let Some(payload) = queue.pop_front() else {
                break;
            };
            out.push(payload);
        }
        out
    }
}

#[cfg(target_os = "macos")]
const WEB_FAVICON_JS: &str = r#"
(() => {
  const link = document.querySelector('link[rel~="icon"]');
  const href = link ? (link.getAttribute("href") || link.href || "") : "";
  const baseURI = document.baseURI || "";
  const referrer = document.referrer || "";
  return JSON.stringify({ href, baseURI, referrer });
})()
"#;

#[cfg(target_os = "macos")]
const AGENT_BOOTSTRAP_JS: &str = r#"
(() => {
  const VERSION = 2;
  if (window.__taborAgent && window.__taborAgent.version === VERSION) {
    return;
  }

  const state = window.__taborAgentState || (window.__taborAgentState = {
    nextId: 1,
    revision: 1,
    inflight: 0,
    observersInstalled: false,
    networkInstalled: false,
    dialogsInstalled: false,
    nextDialogDecision: null
  });

  const bump = () => { state.revision += 1; };
  const doc = () => document;
  const round = (value) => Math.round(Number(value) || 0);

  if (!state.observersInstalled) {
    const observer = new MutationObserver(() => { bump(); });
    observer.observe(document, {
      subtree: true,
      childList: true,
      attributes: true,
      characterData: true
    });
    window.addEventListener("hashchange", bump);
    window.addEventListener("popstate", bump);
    state.observersInstalled = true;
  }

  if (!state.networkInstalled) {
    const finish = () => {
      state.inflight = Math.max(0, state.inflight - 1);
      bump();
    };
    if (typeof window.fetch === "function") {
      const originalFetch = window.fetch.bind(window);
      window.fetch = (...args) => {
        state.inflight += 1;
        bump();
        try {
          return Promise.resolve(originalFetch(...args)).finally(finish);
        } catch (error) {
          finish();
          throw error;
        }
      };
    }
    const originalSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.send = function(...args) {
      state.inflight += 1;
      bump();
      this.addEventListener("loadend", finish, { once: true });
      return originalSend.apply(this, args);
    };
    state.networkInstalled = true;
  }

  if (!state.dialogsInstalled) {
    const consumeDialogDecision = () => {
      const decision = state.nextDialogDecision || { accept: false, text: null };
      state.nextDialogDecision = null;
      bump();
      return decision;
    };

    window.alert = (message) => {
      state.lastDialog = {
        kind: "alert",
        message: String(message ?? ""),
        default_prompt_text: null
      };
      consumeDialogDecision();
    };

    window.confirm = (message) => {
      state.lastDialog = {
        kind: "confirm",
        message: String(message ?? ""),
        default_prompt_text: null
      };
      return !!consumeDialogDecision().accept;
    };

    window.prompt = (message, defaultText = "") => {
      state.lastDialog = {
        kind: "prompt",
        message: String(message ?? ""),
        default_prompt_text: defaultText == null ? null : String(defaultText)
      };
      const decision = consumeDialogDecision();
      if (!decision.accept) return null;
      if (decision.text != null) return String(decision.text);
      return defaultText == null ? "" : String(defaultText);
    };

    state.dialogsInstalled = true;
  }

  const visible = (el) => {
    if (!el) return false;
    const rect = el.getBoundingClientRect();
    if (!rect || rect.width <= 0 || rect.height <= 0) return false;
    const style = window.getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden") return false;
    return rect.bottom >= 0 && rect.right >= 0 &&
      rect.top <= window.innerHeight && rect.left <= window.innerWidth;
  };

  const editable = (el) => {
    if (!el) return false;
    return !!el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName);
  };

  const interactive = (el) => {
    if (!visible(el)) return false;
    if (editable(el)) return true;
    if (el.tagName === "A" && el.href) return true;
    if (["BUTTON", "SUMMARY"].includes(el.tagName)) return true;
    if (el.onclick) return true;
    if (el.getAttribute("role")) return true;
    if (el.tabIndex >= 0) return true;
    return false;
  };

  const role = (el) => {
    return (
      el.getAttribute("role") ||
      (el.tagName || "").toLowerCase()
    );
  };

  const compactText = (value) => {
    if (value == null) return null;
    const text = String(value).replace(/\s+/g, " ").trim();
    if (!text) return null;
    return text.length > 96 ? text.slice(0, 96) : text;
  };

  const name = (el) => {
    return compactText(
      el.getAttribute("aria-label") ||
      el.getAttribute("placeholder") ||
      el.getAttribute("title") ||
      el.innerText ||
      el.textContent ||
      el.id ||
      el.value ||
      ""
    ) || "";
  };

  const value = (el) => {
    if (!editable(el)) return null;
    if ("value" in el) return compactText(el.value || "");
    return compactText(el.textContent || "");
  };

  const checked = (el) => {
    if ("checked" in el) return !!el.checked;
    return null;
  };

  const placeholder = (el) => compactText(el.getAttribute("placeholder") || "");

  const inputType = (el) => {
    if (!el || !el.tagName) return null;
    if (el.tagName === "INPUT") return String(el.getAttribute("type") || "text").toLowerCase();
    if (el.tagName === "TEXTAREA") return "textarea";
    if (el.tagName === "SELECT") return "select";
    if (el.isContentEditable) return "contenteditable";
    return null;
  };

  const bbox = (el) => {
    if (!el) return null;
    const rect = el.getBoundingClientRect();
    if (!rect) return null;
    return {
      x: round(rect.left),
      y: round(rect.top),
      width: round(rect.width),
      height: round(rect.height)
    };
  };

  const center = (el) => {
    const rect = bbox(el);
    if (!rect) return null;
    return {
      x: rect.x + Math.floor(rect.width / 2),
      y: rect.y + Math.floor(rect.height / 2)
    };
  };

  const optionNames = (el) => {
    if (!el || el.tagName !== "SELECT" || !el.options) return null;
    const values = Array.from(el.options)
      .map((option) => compactText(option.label || option.textContent || option.value || ""))
      .filter(Boolean);
    return values.length ? values : null;
  };

  const idFor = (el) => {
    let id = el.getAttribute("data-tabor-agent-id");
    if (!id) {
      id = state.nextId.toString(36);
      state.nextId += 1;
      el.setAttribute("data-tabor-agent-id", id);
    }
    return id;
  };

  const resolve = (id) => {
    return Array.from(doc().querySelectorAll("[data-tabor-agent-id]"))
      .find((el) => el.getAttribute("data-tabor-agent-id") === id) || null;
  };

  const elements = () => {
    const out = [];
    const seen = new Set();
    const selector = [
      "a[href]",
      "button",
      "input",
      "textarea",
      "select",
      "summary",
      "[role]",
      "[contenteditable=\"true\"]",
      "[tabindex]"
    ].join(",");
    for (const el of Array.from(doc().querySelectorAll(selector))) {
      if (!interactive(el)) continue;
      const id = idFor(el);
      if (seen.has(id)) continue;
      seen.add(id);
      out.push({
        id,
        role: role(el),
        name: name(el),
        value: value(el),
        editable: editable(el),
        disabled: !!el.disabled,
        checked: checked(el)
      });
    }
    return out;
  };

  const observe = () => ({
    revision: state.revision,
    url: window.location.href,
    title: doc().title || "",
    ready_state: doc().readyState || "",
    pending_requests: state.inflight,
    elements: elements()
  });

  const inspect = (id) => {
    const el = resolve(id);
    if (!el) return { error: "element not found" };
    return {
      id,
      role: role(el),
      name: name(el),
      value: value(el),
      text: compactText(el.innerText || el.textContent || ""),
      href: el.getAttribute("href") || el.href || null,
      placeholder: placeholder(el),
      input_type: inputType(el),
      bbox: bbox(el),
      center: center(el),
      editable: editable(el),
      disabled: !!el.disabled,
      checked: checked(el),
      options: optionNames(el)
    };
  };

  const dispatchKeyboard = (type, key, modifiers) => {
    const target = doc().activeElement || doc().body;
    const init = {
      key,
      ctrlKey: !!modifiers.control,
      altKey: !!modifiers.alt,
      shiftKey: !!modifiers.shift,
      metaKey: !!modifiers.super_key,
      bubbles: true,
      cancelable: true
    };
    target.dispatchEvent(new KeyboardEvent(type, init));
  };

  const dispatchKey = (key, modifiers) => {
    dispatchKeyboard("keydown", key, modifiers);
    dispatchKeyboard("keyup", key, modifiers);
  };

  const fill = (el, text) => {
    el.focus();
    if ("value" in el) {
      el.value = text;
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      return;
    }
    if (el.isContentEditable) {
      el.textContent = text;
      el.dispatchEvent(new InputEvent("input", { bubbles: true, data: text }));
      return;
    }
    throw new Error("element is not editable");
  };

  const typeText = (text) => {
    const target = doc().activeElement || doc().body;
    if (!target) return;
    if ("value" in target) {
      const current = String(target.value || "");
      const next = `${current}${text}`;
      target.value = next;
      target.dispatchEvent(new Event("input", { bubbles: true }));
      return;
    }
    if (target.isContentEditable) {
      target.textContent = `${target.textContent || ""}${text}`;
      target.dispatchEvent(new InputEvent("input", { bubbles: true, data: text }));
      return;
    }
    target.dispatchEvent(new InputEvent("input", { bubbles: true, data: text }));
  };

  const buttonCode = (button) => {
    const name = String(button || "left").toLowerCase();
    if (name === "right") return 2;
    if (name === "middle") return 1;
    return 0;
  };

  const targetAt = (x, y) => doc().elementFromPoint(x, y) || doc().body;

  const mouseInit = (x, y, button, clickCount = 1) => ({
    clientX: x,
    clientY: y,
    button: buttonCode(button),
    buttons: 1,
    detail: clickCount,
    bubbles: true,
    cancelable: true
  });

  const hoverAt = (x, y) => {
    const target = targetAt(x, y);
    target.dispatchEvent(new MouseEvent("mouseover", mouseInit(x, y, "left")));
    target.dispatchEvent(new MouseEvent("mousemove", mouseInit(x, y, "left")));
  };

  const hoverElement = (el) => {
    const point = center(el);
    if (!point) throw new Error("element has no bounding box");
    el.dispatchEvent(new MouseEvent("mouseover", mouseInit(point.x, point.y, "left")));
    el.dispatchEvent(new MouseEvent("mousemove", mouseInit(point.x, point.y, "left")));
    return point;
  };

  const mouseDownAt = (x, y, button) => {
    const target = targetAt(x, y);
    target.dispatchEvent(new MouseEvent("mousedown", mouseInit(x, y, button)));
  };

  const mouseUpAt = (x, y, button) => {
    const target = targetAt(x, y);
    target.dispatchEvent(new MouseEvent("mouseup", mouseInit(x, y, button)));
  };

  const clickAt = (x, y, button, clickCount = 1) => {
    const target = targetAt(x, y);
    hoverAt(x, y);
    target.dispatchEvent(new MouseEvent("mousedown", mouseInit(x, y, button, clickCount)));
    target.dispatchEvent(new MouseEvent("mouseup", mouseInit(x, y, button, clickCount)));
    target.dispatchEvent(new MouseEvent("click", mouseInit(x, y, button, clickCount)));
    if (clickCount >= 2) {
      target.dispatchEvent(new MouseEvent("dblclick", mouseInit(x, y, button, clickCount)));
    }
    if (buttonCode(button) === 0 && typeof target.click === "function") {
      target.click();
    }
  };

  const clickElement = (el, button, clickCount = 1) => {
    const point = hoverElement(el);
    el.dispatchEvent(new MouseEvent("mousedown", mouseInit(point.x, point.y, button, clickCount)));
    el.dispatchEvent(new MouseEvent("mouseup", mouseInit(point.x, point.y, button, clickCount)));
    if (buttonCode(button) === 0 && clickCount === 1 && typeof el.click === "function") {
      el.click();
      return;
    }
    el.dispatchEvent(new MouseEvent("click", mouseInit(point.x, point.y, button, clickCount)));
    if (clickCount >= 2) {
      el.dispatchEvent(new MouseEvent("dblclick", mouseInit(point.x, point.y, button, clickCount)));
    }
    if (buttonCode(button) === 0 && typeof el.click === "function") {
      el.click();
    }
  };

  const drag = (fromX, fromY, toX, toY) => {
    const source = targetAt(fromX, fromY);
    const destination = targetAt(toX, toY);
    const transfer = typeof DataTransfer === "function" ? new DataTransfer() : null;
    const eventInit = (x, y) => ({
      clientX: x,
      clientY: y,
      bubbles: true,
      cancelable: true,
      dataTransfer: transfer
    });
    source.dispatchEvent(new DragEvent("dragstart", eventInit(fromX, fromY)));
    destination.dispatchEvent(new DragEvent("dragenter", eventInit(toX, toY)));
    destination.dispatchEvent(new DragEvent("dragover", eventInit(toX, toY)));
    destination.dispatchEvent(new DragEvent("drop", eventInit(toX, toY)));
    source.dispatchEvent(new DragEvent("dragend", eventInit(toX, toY)));
  };

  const wheelAt = (dx, dy, x, y) => {
    const target = targetAt(x ?? Math.floor(window.innerWidth / 2), y ?? Math.floor(window.innerHeight / 2));
    target.dispatchEvent(new WheelEvent("wheel", {
      deltaX: dx,
      deltaY: dy,
      clientX: x ?? Math.floor(window.innerWidth / 2),
      clientY: y ?? Math.floor(window.innerHeight / 2),
      bubbles: true,
      cancelable: true
    }));
    window.scrollBy(dx, dy);
  };

  const upload = async (id) => {
    const el = resolve(id);
    if (!el) throw new Error("element not found");
    if (String(el.tagName || "").toLowerCase() !== "input" || inputType(el) !== "file") {
      throw new Error("element is not a file input");
    }
    if (typeof el.showPicker === "function") {
      try {
        el.showPicker();
      } catch (_error) {
        el.click();
      }
    } else {
      el.click();
    }
    const deadline = Date.now() + 5000;
    while (Date.now() < deadline) {
      if (el.files && el.files.length > 0) return inspect(id);
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error("upload timed out");
  };

  const screenshotMeta = () => ({
    width: round(window.innerWidth),
    height: round(window.innerHeight),
    dpr: Number(window.devicePixelRatio || 1),
    scroll_x: round(window.scrollX || 0),
    scroll_y: round(window.scrollY || 0)
  });

  const waitFor = async (spec) => {
    const timeoutMs = spec.timeout_ms || 10000;
    if (spec.ms != null) {
      await new Promise((resolve) => setTimeout(resolve, spec.ms));
      return;
    }
    const deadline = Date.now() + timeoutMs;
    let stableAt = 0;
    while (Date.now() < deadline) {
      let ok = true;
      if (spec.id) ok = !!resolve(spec.id);
      if (ok && spec.text) {
        const body = doc().body ? (doc().body.innerText || doc().body.textContent || "") : "";
        ok = body.includes(spec.text);
      }
      if (ok && spec.url_contains) ok = window.location.href.includes(spec.url_contains);
      if (ok && spec.load) {
        const mode = String(spec.load).toLowerCase();
        if (mode === "domcontentloaded" || mode === "domcontent") {
          ok = doc().readyState === "interactive" || doc().readyState === "complete";
        } else if (mode === "networkidle" || mode === "network_idle") {
          const idle = state.inflight === 0 && doc().readyState === "complete";
          if (!idle) {
            stableAt = 0;
            ok = false;
          } else if (!stableAt) {
            stableAt = Date.now();
            ok = false;
          } else {
            ok = Date.now() - stableAt >= 500;
          }
        } else {
          ok = doc().readyState === "complete";
        }
      }
      if (ok) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error("wait timed out");
  };

  const act = async (actions, includeObservation) => {
    const results = [];
    const nextDialogDecision = (startIndex) => {
      for (let index = startIndex + 1; index < actions.length; index += 1) {
        const action = actions[index];
        if (action.__dialogConsumed) continue;
        if (action.type === "dialog_accept") {
          action.__dialogConsumed = true;
          return { accept: true, text: action.text ?? null };
        }
        if (action.type === "dialog_dismiss") {
          action.__dialogConsumed = true;
          return { accept: false, text: null };
        }
      }
      return { accept: false, text: null };
    };

    for (let index = 0; index < actions.length; index += 1) {
      const action = actions[index];
      try {
        if (action.__dialogConsumed) {
          results.push({ index, ok: true });
          continue;
        }
        state.nextDialogDecision = nextDialogDecision(index);
        if (action.type === "goto") {
          window.location.href = action.url;
        } else if (action.type === "click") {
          const el = resolve(action.id);
          if (!el) throw new Error("element not found");
          el.scrollIntoView({ block: "center", inline: "center" });
          clickElement(el, "left", 1);
        } else if (action.type === "hover") {
          const el = resolve(action.id);
          if (!el) throw new Error("element not found");
          hoverElement(el);
        } else if (action.type === "hover_at") {
          hoverAt(action.x, action.y);
        } else if (action.type === "click_at") {
          clickAt(action.x, action.y, action.button || "left", action.click_count || 1);
        } else if (action.type === "mouse_down") {
          mouseDownAt(action.x, action.y, action.button || "left");
        } else if (action.type === "mouse_up") {
          mouseUpAt(action.x, action.y, action.button || "left");
        } else if (action.type === "drag") {
          drag(action.from_x, action.from_y, action.to_x, action.to_y);
        } else if (action.type === "fill") {
          const el = resolve(action.id);
          if (!el) throw new Error("element not found");
          fill(el, action.text);
        } else if (action.type === "press") {
          dispatchKey(action.key, action.modifiers || {});
        } else if (action.type === "key_down") {
          dispatchKeyboard("keydown", action.key, action.modifiers || {});
        } else if (action.type === "key_up") {
          dispatchKeyboard("keyup", action.key, action.modifiers || {});
        } else if (action.type === "type") {
          typeText(action.text);
        } else if (action.type === "paste") {
          typeText(action.text);
        } else if (action.type === "scroll") {
          window.scrollBy(action.dx || 0, action.dy || 0);
        } else if (action.type === "wheel") {
          wheelAt(action.dx, action.dy, action.x, action.y);
        } else if (action.type === "dialog_accept" || action.type === "dialog_dismiss") {
          // Consumed by alert/confirm/prompt interception when needed.
        } else if (action.type === "wait") {
          await waitFor(action);
        } else {
          throw new Error("unsupported action");
        }
        results.push({ index, ok: true });
      } catch (error) {
        results.push({
          index,
          ok: false,
          error: error && error.message ? String(error.message) : String(error)
        });
        break;
      } finally {
        state.nextDialogDecision = null;
      }
    }
    return {
      results,
      observation: includeObservation ? observe() : null
    };
  };

  window.__taborAgent = {
    version: VERSION,
    observe,
    inspect,
    act,
    upload,
    screenshotMeta
  };
})()
"#;

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
struct WebFaviconHint {
    #[serde(default)]
    href: String,
    #[serde(default, rename = "baseURI")]
    base_uri: String,
    #[serde(default)]
    referrer: String,
}

#[cfg(target_os = "macos")]
fn parse_web_favicon_hint(raw: &str) -> WebFaviconHint {
    json::from_str(raw).unwrap_or_else(|_| WebFaviconHint {
        href: raw.to_string(),
        base_uri: String::new(),
        referrer: String::new(),
    })
}

#[cfg(target_os = "macos")]
fn is_unhelpful_favicon_base(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower == "about:blank" || lower.starts_with("about:") || lower.starts_with("data:")
}

#[cfg(target_os = "macos")]
fn select_favicon_base(page_url: &str, base_uri: &str, referrer: &str) -> String {
    if !is_unhelpful_favicon_base(page_url) {
        return page_url.to_string();
    }
    if !is_unhelpful_favicon_base(base_uri) {
        return base_uri.to_string();
    }
    if !is_unhelpful_favicon_base(referrer) {
        return referrer.to_string();
    }
    page_url.to_string()
}

#[cfg(target_os = "macos")]
fn ensure_agent_runtime(web_view: &mut WebView, state: &mut AgentRuntimeState) {
    if state.preload_registered {
        return;
    }

    let _ = web_view.devtools_command_json(
        "Page.addScriptToEvaluateOnNewDocument",
        Some(json::json!({ "source": AGENT_BOOTSTRAP_JS })),
        |_| {},
    );
    state.preload_registered = true;
}

#[cfg(target_os = "macos")]
fn agent_script(state: &mut AgentRuntimeState, expression: &str) -> String {
    let mut script = agent_script_prefix(state);
    script.push_str("JSON.stringify(");
    script.push_str(expression);
    script.push(')');
    script
}

#[cfg(target_os = "macos")]
fn agent_object_script(state: &mut AgentRuntimeState, expression: &str) -> String {
    let mut script = agent_script_prefix(state);
    script.push_str(expression);
    script
}

#[cfg(target_os = "macos")]
fn agent_script_prefix(state: &mut AgentRuntimeState) -> String {
    let mut script = String::new();
    if !state.injected_once {
        script.push_str(AGENT_BOOTSTRAP_JS);
        script.push('\n');
        state.injected_once = true;
    }
    script
}

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
    Image,
    Pdf,
}

fn draw_mode(kind: &WindowKind) -> DrawMode {
    if kind.is_web() {
        DrawMode::Web
    } else if kind.is_image() {
        DrawMode::Image
    } else if kind.is_pdf() {
        DrawMode::Pdf
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

    fn insert(
        &mut self,
        tab_id: TabId,
        tab: TabState,
        group_id: Option<usize>,
        group_name: Option<String>,
    ) -> Result<(), String> {
        if self.slots.len() <= tab_id.slot_index() {
            self.slots
                .resize_with(tab_id.slot_index() + 1, || TabSlot { generation: 0, tab: None });
        }

        let slot = &mut self.slots[tab_id.slot_index()];
        slot.tab = Some(tab);

        let group_name = group_name.filter(|name| !name.is_empty());
        if self.groups.is_empty() && group_id.is_none() && group_name.is_none() {
            let group = self.new_group();
            self.groups.push(group);
        }
        let target_index = if let Some(group_id) = group_id {
            self.groups
                .iter()
                .position(|group| group.id == group_id)
                .ok_or_else(|| String::from("Group not found"))?
        } else if let Some(name) = group_name {
            if let Some(index) =
                self.groups.iter().position(|group| group.name.as_deref() == Some(&name))
            {
                index
            } else {
                let mut group = self.new_group();
                group.name = Some(name);
                self.groups.push(group);
                self.groups.len() - 1
            }
        } else {
            self.active
                .and_then(|active| {
                    self.groups.iter().position(|group| group.tabs.contains(&active))
                })
                .unwrap_or(0)
        };

        if !self.groups[target_index].tabs.contains(&tab_id) {
            self.groups[target_index].tabs.push(tab_id);
        }

        if self.active.is_none() {
            self.active = Some(tab_id);
        }
        Ok(())
    }

    fn get(&self, tab_id: TabId) -> Option<&TabState> {
        self.slots.get(tab_id.slot_index()).and_then(|slot| {
            (slot.generation == tab_id.generation).then_some(()).and(slot.tab.as_ref())
        })
    }

    fn get_mut(&mut self, tab_id: TabId) -> Option<&mut TabState> {
        self.slots.get_mut(tab_id.slot_index()).and_then(|slot| {
            (slot.generation == tab_id.generation).then_some(()).and(slot.tab.as_mut())
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

    fn create_group(&mut self, name: Option<String>) -> usize {
        let mut group = self.new_group();
        group.name = name.filter(|name| !name.is_empty());
        let group_id = group.id;
        self.groups.push(group);
        group_id
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
    cef_inspector: CefInspectorState,
    #[cfg(target_os = "macos")]
    web_close_metrics: WebCloseMetrics,
    #[cfg(target_os = "macos")]
    macos_fullscreen_or_simple_fullscreen: bool,
    modifiers: Modifiers,
    occluded: bool,
    window_focused: bool,
    preserve_title: bool,
    window_config: ParsedOptions,
    default_terminal_multi_column_count: Option<usize>,
    default_browser_multi_column_count: Option<usize>,
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

        let command_input = if matches!(&options.window_kind, WindowKind::Terminal) {
            options.terminal_options.command_input()
        } else {
            None
        };
        let mut tabs = TabManager::new();
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);
        let first_tab = Self::spawn_tab(
            &mut tabs,
            &display,
            &config,
            MultiColumnDefaults::default(),
            pty_config,
            &proxy,
            options.window_kind,
            None,
            None,
            None,
        )?;

        #[cfg(target_os = "macos")]
        let macos_fullscreen_or_simple_fullscreen = {
            let (native_fullscreen, simple_fullscreen, _) = display.window.macos_fullscreen_flags();
            native_fullscreen || simple_fullscreen
        };

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
            next_favicon_char: FIRST_DYNAMIC_FAVICON_CHAR,
            #[cfg(target_os = "macos")]
            cef_inspector: CefInspectorState::new(),
            #[cfg(target_os = "macos")]
            web_close_metrics: WebCloseMetrics::default(),
            #[cfg(target_os = "macos")]
            macos_fullscreen_or_simple_fullscreen,
            default_terminal_multi_column_count: Default::default(),
            default_browser_multi_column_count: Default::default(),
            dirty: Default::default(),
        };

        context.set_active_tab(first_tab);
        context.send_startup_input(first_tab, command_input);
        context.refresh_tab_panel();
        Ok(context)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_tab(
        tabs: &mut TabManager,
        display: &Display,
        config: &UiConfig,
        multi_column_defaults: MultiColumnDefaults,
        pty_config: tty::Options,
        proxy: &EventLoopProxy<Event>,
        window_kind: WindowKind,
        group_id: Option<usize>,
        group_name: Option<String>,
        terminal_spawn_mode: Option<TerminalSpawnMode>,
    ) -> Result<TabId, Box<dyn Error>> {
        let tab_id = tabs.allocate_id();
        let event_proxy = EventProxy::new(proxy.clone(), display.window.id(), tab_id);
        let size_info =
            display.size_info_for_status_lines(usize::from(window_kind.has_status_bar()));

        let terminal = Term::new(config.term_options(), &size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        #[cfg(not(target_os = "macos"))]
        if matches!(window_kind, WindowKind::Web { .. }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Web tabs are only supported on macOS",
            )
            .into());
        }

        #[cfg(target_os = "macos")]
        let browser_view_mode = match &window_kind {
            WindowKind::Web { .. } => browser_view_mode_for_count(multi_column_defaults.browser),
            WindowKind::Terminal | WindowKind::Image { .. } | WindowKind::Pdf { .. } => {
                BrowserViewMode::default()
            },
        };

        #[cfg(target_os = "macos")]
        let web_view = match &window_kind {
            WindowKind::Web { url } => {
                let layout = Self::browser_viewport_for_kind(
                    display,
                    config,
                    &window_kind,
                    browser_view_mode,
                    multi_column_defaults.browser,
                );
                Some(WebView::new(&display.window, &size_info, layout, tab_id, url, proxy)?)
            },
            WindowKind::Terminal | WindowKind::Image { .. } | WindowKind::Pdf { .. } => None,
        };

        #[cfg(target_os = "macos")]
        let image_view = match &window_kind {
            WindowKind::Image { source } => Some(ImageViewState::new(source.clone())),
            WindowKind::Terminal | WindowKind::Web { .. } | WindowKind::Pdf { .. } => None,
        };
        #[cfg(target_os = "macos")]
        let image_source = match &window_kind {
            WindowKind::Image { source } => Some(source.clone()),
            WindowKind::Terminal | WindowKind::Web { .. } | WindowKind::Pdf { .. } => None,
        };
        #[cfg(target_os = "macos")]
        let pdf_view = match &window_kind {
            WindowKind::Pdf { source } => {
                let mut pdf_view = PdfViewState::new(source.clone());
                pdf_view.set_dark_mode_enabled(matches!(
                    config.window.theme(),
                    Some(winit::window::Theme::Dark)
                ));
                Some(pdf_view)
            },
            WindowKind::Terminal | WindowKind::Web { .. } | WindowKind::Image { .. } => None,
        };
        #[cfg(target_os = "macos")]
        let pdf_source = match &window_kind {
            WindowKind::Pdf { source } => Some(source.clone()),
            WindowKind::Terminal | WindowKind::Web { .. } | WindowKind::Image { .. } => None,
        };

        let default_title = match &window_kind {
            WindowKind::Terminal => config.window.identity.title.clone(),
            WindowKind::Web { url } => {
                if url.is_empty() {
                    String::from("Browser")
                } else {
                    url.clone()
                }
            },
            WindowKind::Image { source } => ImageViewState::new(source.clone()).title,
            WindowKind::Pdf { source } => PdfViewState::new(source.clone()).title,
        };
        let terminal_view_mode = match &window_kind {
            WindowKind::Terminal => terminal_view_mode_for_count(multi_column_defaults.terminal),
            WindowKind::Web { .. } | WindowKind::Image { .. } | WindowKind::Pdf { .. } => {
                TerminalViewMode::default()
            },
        };

        let (title, program_name, notifier, terminal_runtime) = if window_kind.is_terminal() {
            #[cfg(unix)]
            {
                match terminal_spawn_mode
                    .unwrap_or(TerminalSpawnMode::NewLocal { terminal_id: None })
                {
                    TerminalSpawnMode::NewLocal { terminal_id } => {
                        let terminal_id = Some(match terminal_id {
                            Some(terminal_id) => terminal_id,
                            None => workspace::allocate_terminal_id()?,
                        });
                        let (program_name, notifier, terminal_runtime) =
                            spawn_local_terminal_runtime(LocalTerminalSpawnRequest {
                                terminal: &terminal,
                                event_proxy: &event_proxy,
                                pty_config: &pty_config,
                                title: &default_title,
                                window_id: display.window.id(),
                                size_info,
                                ref_test: config.debug.ref_test,
                                terminal_id,
                            })?;
                        (default_title.clone(), program_name, notifier, terminal_runtime)
                    },
                    TerminalSpawnMode::RestoredLocalShell(restored) => {
                        let RestoredTerminalSpawn {
                            terminal_id,
                            mut launch_options,
                            preview_lines,
                            title,
                            working_directory,
                        } = *restored;
                        if let Some(working_directory) = working_directory {
                            launch_options.working_directory = Some(working_directory);
                        }
                        let restored_title = title.unwrap_or_else(|| default_title.clone());
                        if let Some(preview_lines) = preview_lines {
                            terminal.lock().apply_preview_lines(&preview_lines);
                        }
                        let (program_name, notifier, terminal_runtime) =
                            spawn_local_terminal_runtime(LocalTerminalSpawnRequest {
                                terminal: &terminal,
                                event_proxy: &event_proxy,
                                pty_config: &launch_options,
                                title: &restored_title,
                                window_id: display.window.id(),
                                size_info,
                                ref_test: config.debug.ref_test,
                                terminal_id: Some(terminal_id),
                            })?;
                        (restored_title, program_name, notifier, terminal_runtime)
                    },
                }
            }

            #[cfg(not(unix))]
            {
                let (program_name, notifier, terminal_runtime) =
                    spawn_local_terminal_runtime(LocalTerminalSpawnRequest {
                        terminal: &terminal,
                        event_proxy: &event_proxy,
                        pty_config: &pty_config,
                        title: &default_title,
                        window_id: display.window.id(),
                        size_info,
                        ref_test: config.debug.ref_test,
                        terminal_id: None,
                    })?;
                (default_title.clone(), program_name, notifier, terminal_runtime)
            }
        } else {
            let (program_name, notifier, terminal_runtime) =
                spawn_local_terminal_runtime(LocalTerminalSpawnRequest {
                    terminal: &terminal,
                    event_proxy: &event_proxy,
                    pty_config: &pty_config,
                    title: &default_title,
                    window_id: display.window.id(),
                    size_info,
                    ref_test: config.debug.ref_test,
                    terminal_id: None,
                })?;
            (default_title.clone(), program_name, notifier, terminal_runtime)
        };

        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        let tab = TabState {
            id: tab_id,
            title,
            custom_title: None,
            program_name,
            kind: window_kind,
            activity: TabActivity::default(),
            terminal,
            search_state: Default::default(),
            inline_search_state: Default::default(),
            command_state: Default::default(),
            command_footer_feedback: Default::default(),
            terminal_view_mode,
            terminal_multi_column_count_override: None,
            #[cfg(target_os = "macos")]
            browser_view_mode,
            #[cfg(not(target_os = "macos"))]
            browser_view_mode: Default::default(),
            browser_multi_column_count_override: None,
            mouse: Default::default(),
            touch: Default::default(),
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            #[cfg(target_os = "macos")]
            web_view,
            #[cfg(target_os = "macos")]
            image_view,
            #[cfg(target_os = "macos")]
            pdf_view,
            #[cfg(target_os = "macos")]
            web_command_state: Default::default(),
            #[cfg(target_os = "macos")]
            agent_runtime: Default::default(),
            #[cfg(target_os = "macos")]
            favicon: None,
            #[cfg(target_os = "macos")]
            favicon_pending: false,
            notifier,
            terminal_runtime,
        };

        tabs.insert(tab_id, tab, group_id, group_name).map_err(std::io::Error::other)?;
        #[cfg(target_os = "macos")]
        if let Some(source) = image_source {
            request_image_load(proxy, display.window.id(), tab_id, source);
        }
        #[cfg(target_os = "macos")]
        if let Some(source) = pdf_source {
            request_pdf_load(proxy, display.window.id(), tab_id, source);
        }
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

        if !tab.kind.is_terminal() {
            return;
        }

        tab.activity.note_output(Instant::now(), is_active);
        self.refresh_tab_panel();
    }

    pub(crate) fn has_active_terminal_output(&self, now: Instant) -> bool {
        self.tabs.iter().any(|tab| tab.kind.is_terminal() && tab.activity.is_active(now))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn has_web_tab(&self) -> bool {
        self.tabs.iter().any(|tab| tab.kind.is_web())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn active_tab_is_web(&self) -> bool {
        self.tabs.active().is_some_and(|tab| tab.kind.is_web())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn tab_panel_enabled(&self) -> bool {
        self.display.tab_panel.is_enabled()
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn tab_panel_enabled(&self) -> bool {
        false
    }

    #[cfg(target_os = "macos")]
    fn sync_macos_fullscreen_transition(&mut self) {
        let (native_fullscreen, simple_fullscreen, _) =
            self.display.window.macos_fullscreen_flags();
        let fullscreen_or_simple_fullscreen = native_fullscreen || simple_fullscreen;

        if fullscreen_or_simple_fullscreen == self.macos_fullscreen_or_simple_fullscreen {
            return;
        }

        self.macos_fullscreen_or_simple_fullscreen = fullscreen_or_simple_fullscreen;
        if !native_fullscreen {
            self.display.window.clear_macos_notch_ear_windows();
        }
        self.display.pending_update.set_dimensions(self.display.window.inner_size());
        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.dirty = true;
        if self.display.window.has_frame {
            self.display.window.request_redraw();
        }
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
        let parsed = toml::from_str(&option).expect("failed to parse tab panel width override");

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
            let multi_column_defaults = self.multi_column_defaults();

            for tab in self.tabs.iter_mut() {
                let layout = Self::browser_viewport_for_tab(
                    &self.display,
                    &self.config,
                    multi_column_defaults,
                    tab,
                );
                let Some(web_view) = tab.web_view.as_mut() else {
                    continue;
                };

                let visible = Some(tab.id) == active_id;
                web_view.set_visible(visible);
                web_view.set_focus(visible);
                web_view.update_frame(&self.display.window, &self.display.size_info, layout);
            }

            let restored_web_focus =
                active_id.and_then(|tab_id| self.tabs.get_mut(tab_id)).and_then(|tab| {
                    tab.web_view
                        .as_mut()
                        .map(|web_view| web_view.restore_native_focus(&self.display.window))
                }) == Some(true);

            if active_id.is_some() && !restored_web_focus {
                self.display.window.focus_content_view();
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

            if url_update.is_some() {
                self.dirty = true;
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
            let hint = parse_web_favicon_hint(&result.unwrap_or_default());
            let base_url = select_favicon_base(&page_url, &hint.base_uri, &hint.referrer);
            let icon_url = resolve_favicon_url(&base_url, &hint.href);

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
    fn handle_web_favicon(&mut self, tab_id: TabId, page_url: String, icon: Option<FaviconImage>) {
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
    fn handle_image_loaded(
        &mut self,
        tab_id: TabId,
        image: Result<crate::macos::image_view::LoadedImage, String>,
    ) {
        let is_active = Some(tab_id) == self.tabs.active_id();
        let viewport = winit::dpi::PhysicalSize::new(
            self.display.size_info.width() as u32,
            self.display.size_info.height() as u32,
        );
        let (title, error_message);

        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                return;
            };
            let WindowKind::Image { source } = &tab.kind else {
                return;
            };
            let Some(image_view) = tab.image_view.as_mut() else {
                return;
            };

            match image {
                Ok(image) => {
                    if &image.source != source {
                        return;
                    }
                    image_view.set_loaded(image, viewport);
                    title = Some(image_view.title.clone());
                    error_message = None;
                },
                Err(message) => {
                    image_view.set_error(message.clone());
                    title = Some(image_view.title.clone());
                    error_message = Some(message);
                },
            }
        }

        if let Some(title) = title {
            self.update_tab_title(tab_id, title);
        }
        if let Some(message) = error_message {
            self.message_buffer.push(crate::message_bar::Message::new(
                message,
                crate::message_bar::MessageType::Error,
            ));
        }
        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.refresh_tab_panel();
        if is_active {
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn handle_pdf_loaded(
        &mut self,
        tab_id: TabId,
        pdf: Result<LoadedPdfSource, String>,
        event_proxy: &EventLoopProxy<Event>,
    ) {
        let is_active = Some(tab_id) == self.tabs.active_id();
        let viewport = winit::dpi::PhysicalSize::new(
            self.display.size_info.width() as u32,
            self.display.size_info.height() as u32,
        );
        let (title, error_message);
        let mut raster_requests = Vec::new();

        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                return;
            };
            let WindowKind::Pdf { source } = &tab.kind else {
                return;
            };
            let Some(pdf_view) = tab.pdf_view.as_mut() else {
                return;
            };

            match pdf {
                Ok(pdf) => {
                    if &pdf.source != source {
                        return;
                    }
                    if let Err(message) = pdf_view.set_loaded(pdf) {
                        pdf_view.set_error(message.clone());
                        title = Some(pdf_view.title.clone());
                        error_message = Some(message);
                    } else {
                        raster_requests = pdf_view.take_visible_render_requests(viewport);
                        title = Some(pdf_view.title.clone());
                        error_message = None;
                    }
                },
                Err(message) => {
                    pdf_view.set_error(message.clone());
                    title = Some(pdf_view.title.clone());
                    error_message = Some(message);
                },
            }
        }

        if let Some(title) = title {
            self.update_tab_title(tab_id, title);
        }
        if let Some(message) = error_message {
            self.message_buffer.push(crate::message_bar::Message::new(
                message,
                crate::message_bar::MessageType::Error,
            ));
        }
        for request in raster_requests {
            request_pdf_raster(event_proxy, self.display.window.id(), tab_id, request);
        }
        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.refresh_tab_panel();
        if is_active {
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn handle_pdf_page_rasterized(
        &mut self,
        tab_id: TabId,
        raster: PdfRasterizedPage,
        event_proxy: &EventLoopProxy<Event>,
    ) {
        let is_active = Some(tab_id) == self.tabs.active_id();
        let viewport = winit::dpi::PhysicalSize::new(
            self.display.size_info.width() as u32,
            self.display.size_info.height() as u32,
        );
        let mut raster_requests = Vec::new();

        let changed = {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                return;
            };
            let Some(pdf_view) = tab.pdf_view.as_mut() else {
                return;
            };

            let changed = pdf_view.apply_rasterized_page(raster);
            if changed {
                raster_requests = pdf_view.take_visible_render_requests(viewport);
            }
            changed
        };

        for request in raster_requests {
            request_pdf_raster(event_proxy, self.display.window.id(), tab_id, request);
        }

        if !changed {
            return;
        }

        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        if is_active {
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
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
    fn handle_web_editable_focus(&mut self, tab_id: TabId, editable: bool) {
        let is_active = Some(tab_id) == self.tabs.active_id();
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };
        if !tab.kind.is_web() {
            return;
        }

        let before = tab.web_command_state.mode();
        tab.web_command_state.sync_editable_focus(editable);
        if let Some(web_view) = tab.web_view.as_mut() {
            web_view.sync_editable_focus(editable);
            if is_active && !web_view.restore_native_focus(&self.display.window) {
                self.display.window.focus_content_view();
            }
        }
        if tab.web_command_state.mode() == before {
            return;
        }

        self.display.pending_update.dirty = true;
        if is_active {
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
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
        let browser_view_mode = tab.browser_view_mode;
        let browser_multi_column_count =
            tab.browser_multi_column_count_override.or(self.default_browser_multi_column_count);
        let Some(web_view) = tab.web_view.as_mut() else {
            return;
        };

        request_web_cursor_update(
            web_view,
            &mut tab.web_command_state,
            &self.display,
            &self.config,
            browser_view_mode,
            browser_multi_column_count,
            position,
            event_proxy,
            scheduler,
            self.display.window.id(),
            tab_id,
        );
    }

    #[cfg(target_os = "macos")]
    fn allocate_favicon_char(&mut self) -> char {
        self.next_favicon_char = normalize_favicon_char_counter(self.next_favicon_char);

        let value = self.next_favicon_char;
        self.next_favicon_char = self.next_favicon_char.saturating_add(1);
        char::from_u32(value).expect("Invalid favicon glyph")
    }

    fn set_active_tab(&mut self, tab_id: TabId) {
        let previous = self.tabs.active_id();
        if self.tabs.get(tab_id).is_none() {
            return;
        }

        #[cfg(target_os = "macos")]
        let previous_was_web =
            previous.and_then(|prev_id| self.tabs.get(prev_id).map(|tab| tab.kind.is_web()));

        let changed = self.tabs.set_active(tab_id);

        if changed {
            self.update_tab_program_name(tab_id);
            if self.tabs.get(tab_id).is_some_and(|tab| !tab.kind.is_web()) {
                let refresh = Event::for_tab(
                    EventType::UpdateTabProgramName,
                    self.display.window.id(),
                    tab_id,
                );
                self.event_queue.push(refresh.into());
            }
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
            if active_tab.kind.is_web() {
                #[cfg(target_os = "macos")]
                {
                    self.display.window.set_mouse_cursor(CursorIcon::Default);
                    active_tab.web_command_state.set_last_cursor(CursorIcon::Default);
                    active_tab.web_command_state.set_cursor_pending(false);
                }
            } else {
                active_tab.terminal.lock().is_focused = self.window_focused;
                active_tab.activity.mark_seen();
            }
            if !self.preserve_title && self.config.window.dynamic_title {
                let title =
                    active_tab.custom_title.clone().unwrap_or_else(|| active_tab.title.clone());
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
            #[cfg(target_os = "macos")]
            if previous_was_web == Some(true)
                && self.tabs.get(tab_id).is_some_and(|tab| !tab.kind.is_web())
            {
                self.display.window.focus_content_view();
            }
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
        self.create_tab_internal(options, proxy, None, None, None)
    }

    pub(crate) fn create_tab_in_group(
        &mut self,
        options: WindowOptions,
        group_id: Option<usize>,
        group_name: Option<String>,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<TabId, Box<dyn Error>> {
        self.create_tab_internal(options, proxy, group_id, group_name, None)
    }

    fn create_tab_internal(
        &mut self,
        options: WindowOptions,
        proxy: &EventLoopProxy<Event>,
        group_id: Option<usize>,
        group_name: Option<String>,
        terminal_spawn_mode: Option<TerminalSpawnMode>,
    ) -> Result<TabId, Box<dyn Error>> {
        let terminal_command_input = if matches!(&options.window_kind, WindowKind::Terminal) {
            options.terminal_options.command_input()
        } else {
            None
        };
        let mut pty_config = self.config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);
        let command_input = options.command_input.clone();
        let multi_column_defaults = self.multi_column_defaults();
        let tab_id = Self::spawn_tab(
            &mut self.tabs,
            &self.display,
            &self.config,
            multi_column_defaults,
            pty_config,
            proxy,
            options.window_kind,
            group_id,
            group_name,
            terminal_spawn_mode,
        )?;
        self.set_active_tab(tab_id);
        self.send_startup_input(tab_id, terminal_command_input);
        if let Some(input) = command_input.as_deref() {
            if let Some(active_tab) = self.tabs.active_mut() {
                active_tab.command_state.start_with_input(':', input);
                #[cfg(target_os = "macos")]
                if active_tab.kind.is_web() {
                    if let Some(web_view) = active_tab.web_view.as_mut() {
                        web_view.set_focus(false);
                    }
                    self.display.window.focus_content_view();
                }

                self.display.pending_update.dirty = true;
                self.display.damage_tracker.frame().mark_fully_damaged();
                self.dirty = true;
            }
        }
        Ok(tab_id)
    }

    #[cfg(unix)]
    pub(crate) fn workspace_layout(&self) -> WorkspaceLayout {
        let active_tab = self.tabs.active_id();
        let groups = self
            .tabs
            .groups
            .iter()
            .filter(|group| !group.tabs.is_empty())
            .enumerate()
            .map(|(group_index, group)| {
                let tabs = group
                    .tabs
                    .iter()
                    .enumerate()
                    .filter_map(|(tab_index, tab_id)| {
                        let tab = self.tabs.get(*tab_id)?;
                        let persistent_id = format!("g{group_index}-t{tab_index}");
                        let kind = match &tab.kind {
                            WindowKind::Terminal => WorkspaceTabKind::Terminal {
                                terminal_id: tab
                                    .terminal_runtime
                                    .terminal_id()
                                    .expect("terminal tabs require workspace ids"),
                                launch_options: tab.terminal_runtime.launch_options().clone(),
                            },
                            WindowKind::Web { url } => WorkspaceTabKind::Web {
                                url: url.clone(),
                                browser_view_mode: tab.browser_view_mode,
                                browser_multi_column_count_override: tab
                                    .browser_multi_column_count_override,
                            },
                            WindowKind::Image { source } => {
                                WorkspaceTabKind::Image { source: source.clone() }
                            },
                            WindowKind::Pdf { source } => {
                                WorkspaceTabKind::Pdf { source: source.clone() }
                            },
                        };
                        Some(WorkspaceTabLayout {
                            persistent_id,
                            custom_title: tab.custom_title.clone(),
                            terminal_view_mode: tab.terminal_view_mode,
                            terminal_multi_column_count_override: tab
                                .terminal_multi_column_count_override,
                            kind,
                        })
                    })
                    .collect();
                WorkspaceGroupLayout { name: group.name.clone(), tabs }
            })
            .collect::<Vec<_>>();

        let active_tab_id =
            self.tabs.groups.iter().filter(|group| !group.tabs.is_empty()).enumerate().find_map(
                |(group_index, group)| {
                    group.tabs.iter().enumerate().find_map(|(tab_index, tab_id)| {
                        (Some(*tab_id) == active_tab)
                            .then(|| format!("g{group_index}-t{tab_index}"))
                    })
                },
            );

        WorkspaceLayout {
            protocol_version: workspace::WORKSPACE_PROTOCOL_VERSION,
            active_tab_id,
            groups,
        }
    }

    #[cfg(unix)]
    pub(crate) fn restore_workspace_layout(
        &mut self,
        layout: &WorkspaceLayout,
        persisted_terminals: &HashMap<u64, PersistedTerminalState>,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<(), Box<dyn Error>> {
        if non_empty_workspace_group_layouts(layout).next().is_none() {
            return Ok(());
        }

        let existing_tabs = self.tabs.ordered_tabs();
        for tab_id in existing_tabs {
            let _ = self.close_tab(tab_id);
        }

        let mut active_tab_id = None;
        let multi_column_defaults = self.multi_column_defaults();
        for (restored_group_count, (_, group)) in
            non_empty_workspace_group_layouts(layout).enumerate()
        {
            let mut created_group_id = None;
            if restored_group_count > 0 {
                created_group_id = Some(self.tabs.create_group(group.name.clone()));
            }

            for tab_layout in &group.tabs {
                let (window_kind, pty_config, terminal_spawn_mode, restored_terminal) =
                    match &tab_layout.kind {
                        WorkspaceTabKind::Terminal { terminal_id, launch_options } => {
                            let restored_terminal = persisted_terminals.get(terminal_id).cloned();
                            let mut pty_config = restored_terminal
                                .as_ref()
                                .map(|terminal| terminal.launch_options.clone())
                                .unwrap_or_else(|| launch_options.clone());
                            if let Some(working_directory) = restored_terminal
                                .as_ref()
                                .and_then(|terminal| terminal.working_directory.clone())
                            {
                                pty_config.working_directory = Some(working_directory);
                            }
                            let preview_lines = restored_terminal
                                .as_ref()
                                .and_then(|terminal| {
                                    workspace::load_terminal_preview_lines(*terminal_id, terminal)
                                        .ok()
                                })
                                .flatten();
                            let terminal_spawn_mode = Some(TerminalSpawnMode::RestoredLocalShell(
                                Box::new(RestoredTerminalSpawn {
                                    terminal_id: *terminal_id,
                                    launch_options: restored_terminal
                                        .as_ref()
                                        .map(|terminal| terminal.launch_options.clone())
                                        .unwrap_or_else(|| launch_options.clone()),
                                    preview_lines,
                                    title: restored_terminal
                                        .as_ref()
                                        .and_then(|terminal| terminal.title.clone()),
                                    working_directory: restored_terminal
                                        .as_ref()
                                        .and_then(|terminal| terminal.working_directory.clone()),
                                }),
                            ));
                            (
                                WindowKind::Terminal,
                                pty_config,
                                terminal_spawn_mode,
                                restored_terminal,
                            )
                        },
                        WorkspaceTabKind::Web {
                            url,
                            browser_view_mode: _,
                            browser_multi_column_count_override: _,
                        } => (
                            WindowKind::Web { url: url.clone() },
                            self.config.pty_config(),
                            None,
                            None,
                        ),
                        WorkspaceTabKind::Image { source } => (
                            WindowKind::Image { source: source.clone() },
                            self.config.pty_config(),
                            None,
                            None,
                        ),
                        WorkspaceTabKind::Pdf { source } => (
                            WindowKind::Pdf { source: source.clone() },
                            self.config.pty_config(),
                            None,
                            None,
                        ),
                    };

                let group_name = if restored_group_count == 0 && created_group_id.is_none() {
                    group.name.clone()
                } else {
                    None
                };

                let tab_id = Self::spawn_tab(
                    &mut self.tabs,
                    &self.display,
                    &self.config,
                    multi_column_defaults,
                    pty_config,
                    proxy,
                    window_kind,
                    created_group_id,
                    group_name,
                    terminal_spawn_mode,
                )?;

                if let Some(tab) = self.tabs.get_mut(tab_id) {
                    tab.custom_title = tab_layout.custom_title.clone();
                    tab.terminal_view_mode = tab_layout.terminal_view_mode;
                    tab.terminal_multi_column_count_override =
                        tab_layout.terminal_multi_column_count_override;
                    if let Some(saved_terminal) = restored_terminal.as_ref() {
                        if tab.custom_title.is_none() {
                            tab.title =
                                saved_terminal.title.clone().unwrap_or_else(|| tab.title.clone());
                        }
                        tab.program_name = saved_terminal.program_name.clone();
                    }
                    if let WorkspaceTabKind::Web {
                        url: _,
                        browser_view_mode,
                        browser_multi_column_count_override,
                    } = &tab_layout.kind
                    {
                        tab.browser_view_mode = *browser_view_mode;
                        tab.browser_multi_column_count_override =
                            *browser_multi_column_count_override;
                    }
                    sync_terminal_tab_geometry(
                        &self.display,
                        &self.config,
                        multi_column_defaults,
                        tab,
                    );
                }

                if layout.active_tab_id.as_deref() == Some(tab_layout.persistent_id.as_str()) {
                    active_tab_id = Some(tab_id);
                }
            }
        }

        if let Some(tab_id) = active_tab_id.or_else(|| self.tabs.active_id()) {
            self.set_active_tab(tab_id);
        }
        self.refresh_tab_panel();
        self.display.pending_update.dirty = true;
        self.dirty = true;
        Ok(())
    }

    fn multi_column_defaults(&self) -> MultiColumnDefaults {
        MultiColumnDefaults {
            terminal: self.default_terminal_multi_column_count,
            browser: self.default_browser_multi_column_count,
        }
    }

    fn send_startup_input(&mut self, tab_id: TabId, input: Option<String>) {
        let Some(mut input) = input else {
            return;
        };
        if !input.ends_with('\n') {
            input.push('\n');
        }
        let Some(tab) = self.tabs.get(tab_id) else {
            return;
        };
        tab.notifier.notify(input.into_bytes());
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

    pub(crate) fn set_multi_column_count(&mut self, tab_id: TabId, command: MultiColumnCommand) {
        let Some(tab_kind_is_web) = self.tabs.get(tab_id).map(|tab| tab.kind.is_web()) else {
            return;
        };

        match command.scope {
            MultiColumnCommandScope::Local => {
                let Some(tab) = self.tabs.get_mut(tab_id) else {
                    return;
                };
                if tab_kind_is_web {
                    tab.browser_multi_column_count_override = Some(command.count);
                    tab.browser_view_mode = browser_view_mode_for_count(Some(command.count));
                } else {
                    tab.terminal_multi_column_count_override = Some(command.count);
                    tab.terminal_view_mode = terminal_view_mode_for_count(Some(command.count));
                }
            },
            MultiColumnCommandScope::Global => {
                if tab_kind_is_web {
                    self.default_browser_multi_column_count = Some(command.count);
                    for tab in self.tabs.iter_mut() {
                        if !tab.kind.is_web() {
                            continue;
                        }
                        if tab.id == tab_id {
                            tab.browser_multi_column_count_override = None;
                        }
                        if tab.browser_multi_column_count_override.is_none() {
                            tab.browser_view_mode =
                                browser_view_mode_for_count(Some(command.count));
                        }
                    }
                } else {
                    self.default_terminal_multi_column_count = Some(command.count);
                    for tab in self.tabs.iter_mut() {
                        if tab.kind.is_web() {
                            continue;
                        }
                        if tab.id == tab_id {
                            tab.terminal_multi_column_count_override = None;
                        }
                        if tab.terminal_multi_column_count_override.is_none() {
                            tab.terminal_view_mode =
                                terminal_view_mode_for_count(Some(command.count));
                        }
                    }
                }
            },
        }

        let old_is_searching = self.active_is_searching();
        self.display.pending_update.dirty = true;
        self.apply_pending_display_update(old_is_searching);
    }

    pub(crate) fn active_tab_id(&self) -> Option<TabId> {
        self.tabs.active_id()
    }

    pub(crate) fn tab_kind(&self, tab_id: TabId) -> Option<&WindowKind> {
        self.tabs.get(tab_id).map(|tab| &tab.kind)
    }

    pub(crate) fn close_tab(&mut self, tab_id: TabId) -> bool {
        let was_active = self.tabs.active_id() == Some(tab_id);
        #[cfg(target_os = "macos")]
        let close_started = Instant::now();
        #[cfg(target_os = "macos")]
        let mut closed_web = false;
        #[cfg(target_os = "macos")]
        if was_active {
            if let Some(tab) = self.tabs.get_mut(tab_id) {
                if let Some(web_view) = tab.web_view.as_mut() {
                    web_view.set_focus(false);
                    web_view.set_visible(false);
                }
            }
        }

        let Some(tab) = self.tabs.remove(tab_id) else {
            return false;
        };

        #[cfg(target_os = "macos")]
        if tab.kind.is_web() {
            closed_web = true;
            self.closed_tabs.push(ClosedTab {
                kind: tab.kind.clone(),
                browser_view_mode: tab.browser_view_mode,
                browser_multi_column_count_override: tab.browser_multi_column_count_override,
            });
            const MAX_CLOSED_TABS: usize = 10;
            if self.closed_tabs.len() > MAX_CLOSED_TABS {
                self.closed_tabs.remove(0);
            }
            self.cef_inspector.remove_sessions_for_tab(tab_id);
        }

        let mut tab = tab;
        tab.notifier.close_terminal();
        #[cfg(unix)]
        if let Some(terminal_id) = tab.terminal_runtime.terminal_id() {
            let _ = workspace::remove_terminal(terminal_id);
        }

        #[cfg(target_os = "macos")]
        if closed_web {
            let elapsed_ms = close_started.elapsed().as_secs_f64() * 1_000.0;
            self.web_close_metrics.record_close(elapsed_ms);
        }

        if was_active {
            if let Some(active_id) = self.tabs.active_id() {
                // close_tab preselects the next active tab inside TabManager::remove.
                // Reset the marker so set_active_tab runs full focus/visibility updates.
                self.tabs.active = None;
                self.set_active_tab(active_id);
            }
        }

        self.refresh_tab_panel();
        self.dirty = true;

        self.tabs.active_id().is_none()
    }

    #[cfg(unix)]
    pub(crate) fn record_terminal_exit(&mut self, tab_id: TabId, exit_code: Option<i32>) {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };
        if !tab.kind.is_terminal() {
            return;
        }

        let TerminalRuntimeState::Local {
            terminal_id,
            launch_options,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
        } = &tab.terminal_runtime
        else {
            return;
        };

        let Some(terminal_id) = *terminal_id else {
            return;
        };

        let working_directory = {
            #[cfg(not(windows))]
            {
                foreground_process_path(*master_fd, *shell_pid)
                    .ok()
                    .or_else(|| launch_options.working_directory.clone())
            }
            #[cfg(windows)]
            {
                launch_options.working_directory.clone()
            }
        };

        workspace::update_terminal_metadata(
            terminal_id,
            Some(tab.title.clone()),
            Some(tab.program_name.clone()),
            working_directory.clone(),
            exit_code,
            Some(true),
        );
        workspace::checkpoint_terminal(terminal_id, true);

        tab.notifier = TabNotifier::Disconnected;
        tab.terminal_runtime = TerminalRuntimeState::Disconnected {
            terminal_id,
            launch_options: launch_options.clone(),
            working_directory,
            clean_exit: true,
            exit_code,
        };
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
        let tab_id = self.create_tab(options, proxy)?;
        if let Some(tab) = self.tabs.get_mut(tab_id) {
            tab.browser_view_mode = closed.browser_view_mode;
            tab.browser_multi_column_count_override = closed.browser_multi_column_count_override;
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn open_web_url_in_tab(&mut self, tab_id: TabId, url: String) -> Result<(), String> {
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
    pub(crate) fn open_url_in_tab(
        &mut self,
        tab_id: TabId,
        url: String,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<(), String> {
        match classify_open_url(&url) {
            OpenUrlKind::Web => self.open_web_url_in_tab(tab_id, url),
            OpenUrlKind::Image => {
                let Some(tab) = self.tabs.get_mut(tab_id) else {
                    return Err(String::from("Tab not found"));
                };
                let WindowKind::Image { source } = &mut tab.kind else {
                    return Err(String::from("Not an image tab"));
                };
                *source = url.clone();
                let Some(image_view) = tab.image_view.as_mut() else {
                    return Err(String::from("Image view is unavailable"));
                };
                image_view.reset_source(url.clone());
                let title = image_view.title.clone();
                self.command_history.record_url(url.clone());
                request_image_load(proxy, self.display.window.id(), tab_id, url);
                self.update_tab_title(tab_id, title);
                self.display.pending_update.dirty = true;
                self.display.damage_tracker.frame().mark_fully_damaged();
                self.dirty = true;
                Ok(())
            },
            OpenUrlKind::Pdf => {
                let Some(tab) = self.tabs.get_mut(tab_id) else {
                    return Err(String::from("Tab not found"));
                };
                let WindowKind::Pdf { source } = &mut tab.kind else {
                    return Err(String::from("Not a PDF tab"));
                };
                *source = url.clone();
                let Some(pdf_view) = tab.pdf_view.as_mut() else {
                    return Err(String::from("PDF view is unavailable"));
                };
                pdf_view.reset_source(url.clone());
                let title = pdf_view.title.clone();
                self.command_history.record_url(url.clone());
                request_pdf_load(proxy, self.display.window.id(), tab_id, url);
                self.update_tab_title(tab_id, title);
                self.display.pending_update.dirty = true;
                self.display.damage_tracker.frame().mark_fully_damaged();
                self.dirty = true;
                Ok(())
            },
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn open_url_new_tab(
        &mut self,
        url: String,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<TabId, Box<dyn Error>> {
        let mut options = WindowOptions::default();
        options.window_kind = match classify_open_url(&url) {
            OpenUrlKind::Web => WindowKind::Web { url: url.clone() },
            OpenUrlKind::Image => WindowKind::Image { source: url.clone() },
            OpenUrlKind::Pdf => WindowKind::Pdf { source: url.clone() },
        };
        let tab_id = self.create_tab(options, proxy)?;
        self.command_history.record_url(url);
        Ok(tab_id)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn open_web_url_new_tab(
        &mut self,
        url: String,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<(), Box<dyn Error>> {
        let _ = self.open_url_new_tab(url, proxy)?;
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
        let multi_column_defaults = self.multi_column_defaults();
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
                        let activity = if tab.kind.is_terminal() {
                            Some(Self::ipc_activity(&tab.activity, now))
                        } else {
                            None
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
                            web_mode: Self::ipc_web_mode(tab),
                            terminal_session: Self::ipc_terminal_session(tab),
                            terminal_layout: Self::ipc_terminal_layout(
                                &self.display,
                                &self.config,
                                multi_column_defaults,
                                tab,
                            ),
                            browser_layout: Self::ipc_browser_layout(
                                &self.display,
                                &self.config,
                                multi_column_defaults,
                                tab,
                            ),
                            image_view: Self::ipc_image_view(tab, &self.display),
                            pdf_view: Self::ipc_pdf_view(tab, &self.display),
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
        let multi_column_defaults = self.multi_column_defaults();
        let activity = if tab.kind.is_terminal() {
            Some(Self::ipc_activity(&tab.activity, now))
        } else {
            None
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
            web_mode: Self::ipc_web_mode(tab),
            terminal_session: Self::ipc_terminal_session(tab),
            terminal_layout: Self::ipc_terminal_layout(
                &self.display,
                &self.config,
                multi_column_defaults,
                tab,
            ),
            browser_layout: Self::ipc_browser_layout(
                &self.display,
                &self.config,
                multi_column_defaults,
                tab,
            ),
            image_view: Self::ipc_image_view(tab, &self.display),
            pdf_view: Self::ipc_pdf_view(tab, &self.display),
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
        group_id: Option<usize>,
        group_name: Option<String>,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<TabId, IpcError> {
        self.create_tab_in_group(options, group_id, group_name, proxy).map_err(|err| {
            IpcError::new(IpcErrorCode::Internal, format!("Could not create tab: {err}"))
        })
    }

    #[cfg(unix)]
    pub(crate) fn ipc_create_group(&mut self, name: Option<String>) -> Result<usize, IpcError> {
        let group_id = self.tabs.create_group(name);
        self.refresh_tab_panel();
        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.dirty = true;
        Ok(group_id)
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
                if self.tabs.get(tab_id).is_some() { Some(tab_id) } else { None }
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
            self.restore_closed_tab(proxy)
                .map_err(|err| IpcError::new(IpcErrorCode::Internal, err.to_string()))
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
            self.open_url_in_tab(tab_id, url, proxy)
                .map_err(|err| IpcError::new(IpcErrorCode::InvalidRequest, err))
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, url, proxy);
            Err(IpcError::new(IpcErrorCode::Unsupported, "Web tabs are only supported on macOS"))
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
            let tab_id = self
                .open_url_new_tab(url, proxy)
                .map_err(|err| IpcError::new(IpcErrorCode::Internal, err.to_string()))?;
            Ok(tab_id)
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (url, proxy);
            Err(IpcError::new(IpcErrorCode::Unsupported, "Web tabs are only supported on macOS"))
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
            self.with_action_context(tab_id, event_loop, event_proxy, clipboard, scheduler, |ctx| {
                ctx.reload_web();
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, event_loop, event_proxy, clipboard, scheduler);
            Err(IpcError::new(IpcErrorCode::Unsupported, "Web tabs are only supported on macOS"))
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
            self.with_action_context(tab_id, event_loop, event_proxy, clipboard, scheduler, |ctx| {
                ctx.open_web_inspector();
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, event_loop, event_proxy, clipboard, scheduler);
            Err(IpcError::new(IpcErrorCode::Unsupported, "Web tabs are only supported on macOS"))
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
        let old_is_searching = self.active_is_searching();
        self.add_window_config(self.config.clone(), &parsed);
        self.apply_pending_display_update(old_is_searching);
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn ipc_runtime_metrics(&self) -> Result<IpcRuntimeMetrics, IpcError> {
        #[cfg(target_os = "macos")]
        {
            let webview = crate::macos::webview_metrics();
            let cef_pump = crate::macos::cef::cef_pump_metrics();
            Ok(IpcRuntimeMetrics {
                webview: Some(IpcWebViewMetrics {
                    live: webview.live as u64,
                    created: webview.created,
                    dropped: webview.dropped,
                    accelerated_frames: webview.accelerated_frames,
                    frame_delivery_mode: match webview.frame_delivery_mode {
                        crate::macos::webview::WebFrameDeliveryMode::CefInternal => {
                            IpcWebFrameDeliveryMode::CefInternal
                        },
                    },
                    external_begin_frames: webview.external_begin_frames,
                    accelerated_startup_failures: webview.accelerated_startup_failures,
                    unexpected_cpu_paints: webview.unexpected_cpu_paints,
                    live_accelerated_surfaces: webview.live_accelerated_surfaces,
                }),
                web_close: Some(self.web_close_metrics.to_ipc()),
                cef_pump: Some(IpcCefPumpMetrics {
                    scheduled: cef_pump.scheduled,
                    executed: cef_pump.executed,
                    coalesced: cef_pump.coalesced,
                    last_requested_delay_ms: cef_pump.last_requested_delay_ms,
                    last_effective_delay_ms: cef_pump.last_effective_delay_ms,
                    last_run_ms_ago: cef_pump.last_run_ms_ago,
                    hidden_throttle_active: cef_pump.hidden_throttle_active,
                }),
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            Ok(IpcRuntimeMetrics::default())
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_state(&self) -> Result<IpcWindowDebugState, IpcError> {
        #[cfg(target_os = "macos")]
        {
            self.display.window.macos_window_debug_state().ok_or_else(|| {
                IpcError::new(IpcErrorCode::Unsupported, "Window debug state unavailable")
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Window debug state is only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_snapshot(
        &mut self,
        scheduler: &mut Scheduler,
        highlight_notch_ears: bool,
    ) -> Result<IpcWindowDebugSnapshot, IpcError> {
        #[cfg(target_os = "macos")]
        {
            self.draw_inner(scheduler, highlight_notch_ears);

            let state = self.ipc_window_debug_state()?;
            let (main_width, main_height, pixels) = self.display.window_snapshot_rgba();
            let main_image =
                RgbaImage::from_raw(main_width, main_height, pixels).ok_or_else(|| {
                    IpcError::new(
                        IpcErrorCode::Internal,
                        "Failed to construct window snapshot image",
                    )
                })?;
            let debug_notch_ears_enabled = highlight_notch_ears
                || std::env::var_os("TABOR_DEBUG_NOTCH_EARS").is_some_and(|value| value != "0");

            let (snapshot_screen_points, image) = if state.notch_ears_active {
                let mut canvas = RgbaImage::new(
                    Self::round_snapshot_dimension(
                        state.screen_frame_points.width * state.scale_factor,
                    )?,
                    Self::round_snapshot_dimension(
                        state.screen_frame_points.height * state.scale_factor,
                    )?,
                );

                let content_rect = Self::screen_rect_to_snapshot_pixels(
                    state.content_frame_screen_points,
                    state.screen_frame_points,
                    state.scale_factor,
                );
                Self::blit_window_snapshot(&mut canvas, &main_image, content_rect);

                if debug_notch_ears_enabled {
                    for rect in [
                        state.auxiliary_top_left_screen_points,
                        state.auxiliary_top_right_screen_points,
                    ] {
                        let rect = Self::screen_rect_to_snapshot_pixels(
                            rect,
                            state.screen_frame_points,
                            state.scale_factor,
                        );
                        Self::fill_snapshot_rect(&mut canvas, rect, image::Rgba([255, 0, 0, 255]));
                    }
                }

                (state.screen_frame_points, canvas)
            } else {
                (state.content_frame_screen_points, main_image)
            };

            let mut png = Cursor::new(Vec::new());
            let width = image.width();
            let height = image.height();
            DynamicImage::ImageRgba8(image).write_to(&mut png, image::ImageFormat::Png).map_err(
                |err| {
                    IpcError::new(
                        IpcErrorCode::Internal,
                        format!("Failed to encode window snapshot PNG: {err}"),
                    )
                },
            )?;

            Ok(IpcWindowDebugSnapshot {
                png_base64: BASE64.encode(png.into_inner()),
                width,
                height,
                snapshot_screen_points,
                state,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (scheduler, highlight_notch_ears);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Window debug snapshots are only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_image_snapshot(
        &mut self,
    ) -> Result<IpcImageDebugSnapshot, IpcError> {
        #[cfg(target_os = "macos")]
        {
            let Some(tab_id) = self.tabs.active_id() else {
                return Err(IpcError::new(IpcErrorCode::NotFound, "No active tab"));
            };
            let Some(tab) = self.tabs.get(tab_id) else {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            };
            if !tab.kind.is_image() {
                return Err(IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    "Active tab is not an image tab",
                ));
            }
            let Some(image_view) = tab.image_view.as_ref() else {
                return Err(IpcError::new(IpcErrorCode::Internal, "Image view is unavailable"));
            };
            let viewport = PhysicalSize::new(
                self.display.size_info.width() as u32,
                self.display.size_info.height() as u32,
            );
            let background = self.config.colors.primary.background;
            let image = image_view
                .debug_snapshot(viewport, [background.r, background.g, background.b, 255])
                .ok_or_else(|| {
                    IpcError::new(
                        IpcErrorCode::InvalidRequest,
                        "Active image tab is not ready for snapshots",
                    )
                })?;

            let mut png = Cursor::new(Vec::new());
            let width = image.width();
            let height = image.height();
            DynamicImage::ImageRgba8(image).write_to(&mut png, image::ImageFormat::Png).map_err(
                |err| {
                    IpcError::new(
                        IpcErrorCode::Internal,
                        format!("Failed to encode image snapshot PNG: {err}"),
                    )
                },
            )?;

            Ok(IpcImageDebugSnapshot { png_base64: BASE64.encode(png.into_inner()), width, height })
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Image debug snapshots are only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_image_pinch(
        &mut self,
        delta: f64,
        phase: IpcTouchPhase,
    ) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            let viewport = PhysicalSize::new(
                self.display.size_info.width() as u32,
                self.display.size_info.height() as u32,
            );
            let cursor = PhysicalPosition::new(
                f64::from(viewport.width) / 2.0,
                f64::from(viewport.height) / 2.0,
            );
            let changed = {
                let Some(tab_id) = self.tabs.active_id() else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "No active tab"));
                };
                let Some(tab) = self.tabs.get_mut(tab_id) else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
                };
                if !tab.kind.is_image() {
                    return Err(IpcError::new(
                        IpcErrorCode::InvalidRequest,
                        "Active tab is not an image tab",
                    ));
                }
                let Some(image_view) = tab.image_view.as_mut() else {
                    return Err(IpcError::new(IpcErrorCode::Internal, "Image view is unavailable"));
                };
                image_view.pinch_gesture(delta, phase.into(), cursor, viewport)
            };
            if changed {
                self.display.pending_update.dirty = true;
                self.display.damage_tracker.frame().mark_fully_damaged();
                self.dirty = true;
            }
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (delta, phase);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Image debug gestures are only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_image_smart_magnify(&mut self) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            let viewport = PhysicalSize::new(
                self.display.size_info.width() as u32,
                self.display.size_info.height() as u32,
            );
            let changed = {
                let Some(tab_id) = self.tabs.active_id() else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "No active tab"));
                };
                let Some(tab) = self.tabs.get_mut(tab_id) else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
                };
                if !tab.kind.is_image() {
                    return Err(IpcError::new(
                        IpcErrorCode::InvalidRequest,
                        "Active tab is not an image tab",
                    ));
                }
                let Some(image_view) = tab.image_view.as_mut() else {
                    return Err(IpcError::new(IpcErrorCode::Internal, "Image view is unavailable"));
                };
                image_view.smart_magnify(viewport)
            };
            if changed {
                self.display.pending_update.dirty = true;
                self.display.damage_tracker.frame().mark_fully_damaged();
                self.dirty = true;
            }
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Image debug gestures are only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_image_rotate(
        &mut self,
        delta: f32,
        phase: IpcTouchPhase,
    ) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            let viewport = PhysicalSize::new(
                self.display.size_info.width() as u32,
                self.display.size_info.height() as u32,
            );
            let changed = {
                let Some(tab_id) = self.tabs.active_id() else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "No active tab"));
                };
                let Some(tab) = self.tabs.get_mut(tab_id) else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
                };
                if !tab.kind.is_image() {
                    return Err(IpcError::new(
                        IpcErrorCode::InvalidRequest,
                        "Active tab is not an image tab",
                    ));
                }
                let Some(image_view) = tab.image_view.as_mut() else {
                    return Err(IpcError::new(IpcErrorCode::Internal, "Image view is unavailable"));
                };
                image_view.rotation_gesture(delta, phase.into(), viewport)
            };
            if changed {
                self.display.pending_update.dirty = true;
                self.display.damage_tracker.frame().mark_fully_damaged();
                self.dirty = true;
            }
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (delta, phase);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Image debug gestures are only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_image_pressure(
        &mut self,
        pressure: f32,
        stage: i64,
    ) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            let viewport = PhysicalSize::new(
                self.display.size_info.width() as u32,
                self.display.size_info.height() as u32,
            );
            let cursor = PhysicalPosition::new(
                f64::from(viewport.width) / 2.0,
                f64::from(viewport.height) / 2.0,
            );
            let changed = {
                let Some(tab_id) = self.tabs.active_id() else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "No active tab"));
                };
                let Some(tab) = self.tabs.get_mut(tab_id) else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
                };
                if !tab.kind.is_image() {
                    return Err(IpcError::new(
                        IpcErrorCode::InvalidRequest,
                        "Active tab is not an image tab",
                    ));
                }
                let Some(image_view) = tab.image_view.as_mut() else {
                    return Err(IpcError::new(IpcErrorCode::Internal, "Image view is unavailable"));
                };
                let _ = pressure;
                image_view.touchpad_pressure(stage, cursor)
            };
            if changed {
                self.display.pending_update.dirty = true;
                self.display.damage_tracker.frame().mark_fully_damaged();
                self.dirty = true;
            }
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (pressure, stage);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Image debug gestures are only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_press_standard_button(
        &self,
        button: IpcWindowDebugButton,
    ) -> Result<(), IpcError> {
        #[cfg(target_os = "macos")]
        {
            self.display.window.macos_press_standard_window_button(button).map_err(|err| {
                IpcError::new(
                    IpcErrorCode::Unsupported,
                    format!("Failed to press macOS standard button: {err}"),
                )
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = button;
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Window standard buttons are only available on macOS",
            ))
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_window_debug_mouse_drag(
        &mut self,
        drag: WindowDebugMouseDrag,
        context: IpcEventContext<'_>,
    ) -> Result<(), IpcError> {
        let Some(tab_id) = self.tabs.active_id() else {
            return Err(IpcError::new(IpcErrorCode::NotFound, "No active tab"));
        };
        if !self.tabs.get(tab_id).is_some_and(|tab| tab.kind.is_web()) {
            return Err(IpcError::new(IpcErrorCode::InvalidRequest, "Active tab is not a web tab"));
        }

        #[cfg(target_os = "macos")]
        self.display.window.focus_window();

        let steps = drag.steps.unwrap_or(24).max(1);
        self.with_action_context(
            tab_id,
            context.event_loop,
            context.event_proxy,
            context.clipboard,
            context.scheduler,
            move |ctx| {
                let start = drag.start;
                let end = drag.end;
                ctx.web_mouse_move(start);
                ctx.web_request_cursor_update(start);
                ctx.web_mouse_input(ElementState::Pressed, MouseButton::Left);
                for step in 1..=steps {
                    let t = step as f64 / steps as f64;
                    let point = PhysicalPosition::new(
                        start.x + (end.x - start.x) * t,
                        start.y + (end.y - start.y) * t,
                    );
                    ctx.web_mouse_move(point);
                    ctx.web_request_cursor_update(point);
                }
                ctx.web_mouse_input(ElementState::Released, MouseButton::Left);
            },
        )
    }

    #[cfg(target_os = "macos")]
    fn round_snapshot_dimension(value: f64) -> Result<u32, IpcError> {
        if value <= 0.0 {
            return Err(IpcError::new(
                IpcErrorCode::Internal,
                format!("Invalid snapshot dimension: {value}"),
            ));
        }

        Ok(value.round() as u32)
    }

    #[cfg(target_os = "macos")]
    fn screen_rect_to_snapshot_pixels(
        rect: IpcWindowDebugRect,
        snapshot_screen_points: IpcWindowDebugRect,
        scale_factor: f64,
    ) -> (i64, i64, i64, i64) {
        let local_x = (rect.x - snapshot_screen_points.x) * scale_factor;
        let local_y =
            (snapshot_screen_points.height - (rect.y - snapshot_screen_points.y) - rect.height)
                * scale_factor;
        let width = rect.width * scale_factor;
        let height = rect.height * scale_factor;
        let x0 = local_x.floor() as i64;
        let y0 = local_y.floor() as i64;
        let x1 = (local_x + width).ceil() as i64;
        let y1 = (local_y + height).ceil() as i64;

        (x0, y0, x1, y1)
    }

    #[cfg(target_os = "macos")]
    fn blit_window_snapshot(
        canvas: &mut RgbaImage,
        source: &RgbaImage,
        target_rect: (i64, i64, i64, i64),
    ) {
        let (x0, y0, _, _) = target_rect;
        for (source_x, source_y, pixel) in source.enumerate_pixels() {
            let target_x = x0 + i64::from(source_x);
            let target_y = y0 + i64::from(source_y);
            if target_x < 0
                || target_y < 0
                || target_x >= i64::from(canvas.width())
                || target_y >= i64::from(canvas.height())
            {
                continue;
            }
            canvas.put_pixel(target_x as u32, target_y as u32, *pixel);
        }
    }

    #[cfg(target_os = "macos")]
    fn fill_snapshot_rect(
        canvas: &mut RgbaImage,
        rect: (i64, i64, i64, i64),
        color: image::Rgba<u8>,
    ) {
        let (x0, y0, x1, y1) = rect;
        let clipped_x0 = x0.max(0).min(i64::from(canvas.width()));
        let clipped_y0 = y0.max(0).min(i64::from(canvas.height()));
        let clipped_x1 = x1.max(clipped_x0).min(i64::from(canvas.width()));
        let clipped_y1 = y1.max(clipped_y0).min(i64::from(canvas.height()));

        if clipped_x0 >= clipped_x1 || clipped_y0 >= clipped_y1 {
            return;
        }

        for y in clipped_y0 as u32..clipped_y1 as u32 {
            for x in clipped_x0 as u32..clipped_x1 as u32 {
                canvas.put_pixel(x, y, color);
            }
        }
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
    pub(crate) fn ipc_terminal_key(
        &mut self,
        tab_id: TabId,
        input: TerminalKeyInput,
        event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Result<(), IpcError> {
        let Some(tab) = self.tabs.get(tab_id) else {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        };
        if tab.kind.is_web() {
            return Err(IpcError::new(IpcErrorCode::InvalidRequest, "Tab is not a terminal tab"));
        }

        let bytes = terminal_key_bytes(input)?;
        if bytes.is_empty() {
            return Ok(());
        }

        self.with_action_context(
            tab_id,
            event_loop,
            event_proxy,
            clipboard,
            scheduler,
            move |ctx| {
                input::ActionContext::on_terminal_input_start(ctx);
                input::ActionContext::write_to_pty(ctx, bytes);
            },
        )
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
            let targets = self
                .tabs
                .iter()
                .filter_map(|tab| {
                    let WindowKind::Web { url } = &tab.kind else {
                        return None;
                    };
                    let target_id = Self::cef_target_id(tab.id);
                    Some(IpcInspectorTarget {
                        target_id,
                        target_type: Some(String::from("page")),
                        url: if url.is_empty() { None } else { Some(url.clone()) },
                        title: if tab.title.is_empty() { None } else { Some(tab.title.clone()) },
                        override_name: tab.custom_title.clone(),
                        host_app_identifier: Some(String::from("tabor")),
                        tab_id: Some(IpcTabId::from(tab.id)),
                    })
                })
                .collect();
            Ok(targets)
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
            let resolved_tab_id = if let Some(tab_id) = tab_id {
                tab_id
            } else if let Some(target_id) = target_id {
                Self::tab_id_from_target_id(target_id)
            } else {
                return Err(IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    "tab_id or target_id required",
                ));
            };

            let target_id = Self::cef_target_id(resolved_tab_id);

            let Some(tab) = self.tabs.get_mut(resolved_tab_id) else {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            };
            if !tab.kind.is_web() {
                return Err(IpcError::new(IpcErrorCode::InvalidRequest, "Tab is not a web tab"));
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                return Err(IpcError::new(IpcErrorCode::Unsupported, "Web view is unavailable"));
            };

            let last_event_id = web_view.latest_devtools_event_id();
            let session_id = self.cef_inspector.next_session_id(target_id);
            self.cef_inspector.sessions.insert(
                session_id.clone(),
                CefInspectorSession { tab_id: resolved_tab_id, last_event_id },
            );
            self.cef_inspector.register_session(&session_id);

            Ok(IpcInspectorSession {
                session_id,
                target_id,
                tab_id: IpcTabId::from(resolved_tab_id),
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
            let existed = self.cef_inspector.sessions.remove(&session_id);
            if existed.is_none() {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Inspector session not found"));
            }
            self.cef_inspector.remove_session(&session_id);
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
            let tab_id = self
                .cef_inspector
                .sessions
                .get(&session_id)
                .map(|session| session.tab_id)
                .ok_or_else(|| {
                    IpcError::new(IpcErrorCode::NotFound, "Inspector session not found")
                })?;

            let value: JsonValue = json::from_str(&message).map_err(|err| {
                IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    format!("Invalid inspector message: {err}"),
                )
            })?;
            let Some(object) = value.as_object() else {
                return Err(IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    "Inspector message must be a JSON object",
                ));
            };
            let method =
                object.get("method").and_then(|value| value.as_str()).ok_or_else(|| {
                    IpcError::new(IpcErrorCode::InvalidRequest, "Inspector message missing method")
                })?;
            let id = object.get("id").and_then(|value| value.as_i64()).ok_or_else(|| {
                IpcError::new(IpcErrorCode::InvalidRequest, "Inspector message missing id")
            })?;
            let params = object.get("params").cloned();

            let Some(tab) = self.tabs.get_mut(tab_id) else {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            };
            let Some(web_view) = tab.web_view.as_mut() else {
                return Err(IpcError::new(IpcErrorCode::Unsupported, "Web view is unavailable"));
            };

            let pending = Arc::clone(&self.cef_inspector.pending);
            let session_id = session_id.clone();
            let callback = move |result: Result<JsonValue, String>| {
                let payload = match result {
                    Ok(result) => json::json!({ "id": id, "result": result }),
                    Err(err) => json::json!({ "id": id, "error": { "message": err } }),
                };
                let message = payload.to_string();
                let mut pending = pending.lock().unwrap();
                if let Some(queue) = pending.get_mut(&session_id) {
                    queue.push_back(message);
                }
            };

            web_view
                .devtools_command_json(method, params, callback)
                .map_err(|err| IpcError::new(IpcErrorCode::Internal, err))?;

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
            const DEFAULT_MAX: usize = 256;
            let max = max.unwrap_or(DEFAULT_MAX);

            let (tab_id, last_event_id) = self
                .cef_inspector
                .sessions
                .get(&session_id)
                .map(|session| (session.tab_id, session.last_event_id))
                .ok_or_else(|| {
                    IpcError::new(IpcErrorCode::NotFound, "Inspector session not found")
                })?;

            let mut payloads = self.cef_inspector.drain_messages(&session_id, max);

            if payloads.len() < max {
                let Some(tab) = self.tabs.get_mut(tab_id) else {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
                };
                let Some(web_view) = tab.web_view.as_mut() else {
                    return Err(IpcError::new(
                        IpcErrorCode::Unsupported,
                        "Web view is unavailable",
                    ));
                };
                let remaining = max - payloads.len();
                let (events, newest_id) = web_view.devtools_events_since(last_event_id, remaining);
                if newest_id != last_event_id {
                    if let Some(session) = self.cef_inspector.sessions.get_mut(&session_id) {
                        session.last_event_id = newest_id;
                    }
                }
                payloads.extend(events);
            }

            let messages = payloads
                .into_iter()
                .map(|payload| IpcInspectorMessage { session_id: session_id.clone(), payload })
                .collect();
            Ok(messages)
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
            self.cef_inspector.sessions.contains_key(session_id)
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_agent_observe(&mut self, tab_id: Option<TabId>, stream: Arc<UnixStream>) {
        let Some(tab_id) = tab_id.or(self.tabs.active_id()) else {
            send_stream_error(&stream, IpcErrorCode::NotFound, "No active tab");
            return;
        };

        #[cfg(not(target_os = "macos"))]
        {
            let _ = tab_id;
            send_stream_error(
                &stream,
                IpcErrorCode::Unsupported,
                "Agent control is only supported on macOS",
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                send_stream_error(&stream, IpcErrorCode::NotFound, "Tab not found");
                return;
            };
            if !tab.kind.is_web() {
                send_stream_error(&stream, IpcErrorCode::InvalidRequest, "Tab is not a web tab");
                return;
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                send_stream_error(&stream, IpcErrorCode::Unsupported, "Web view is unavailable");
                return;
            };

            ensure_agent_runtime(web_view, &mut tab.agent_runtime);
            let script = agent_script(&mut tab.agent_runtime, "window.__taborAgent.observe()");
            let stream = Arc::clone(&stream);
            web_view.eval_js_string(&script, move |result| {
                let reply = match result {
                    Some(raw) => match json::from_str::<AgentObservation>(&raw) {
                        Ok(observation) => SocketReply::AgentObservation { observation },
                        Err(err) => ipc::reply_error(
                            IpcErrorCode::Internal,
                            format!("Invalid agent observation: {err}"),
                        ),
                    },
                    None => ipc::reply_error(
                        IpcErrorCode::Internal,
                        "Agent observe returned no payload",
                    ),
                };
                if let Ok(mut stream) = stream.try_clone() {
                    ipc::send_reply(&mut stream, reply);
                }
            });
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_agent_inspect(
        &mut self,
        tab_id: Option<TabId>,
        element_id: String,
        stream: Arc<UnixStream>,
    ) {
        let Some(tab_id) = tab_id.or(self.tabs.active_id()) else {
            send_stream_error(&stream, IpcErrorCode::NotFound, "No active tab");
            return;
        };

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, element_id);
            send_stream_error(
                &stream,
                IpcErrorCode::Unsupported,
                "Agent control is only supported on macOS",
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                send_stream_error(&stream, IpcErrorCode::NotFound, "Tab not found");
                return;
            };
            if !tab.kind.is_web() {
                send_stream_error(&stream, IpcErrorCode::InvalidRequest, "Tab is not a web tab");
                return;
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                send_stream_error(&stream, IpcErrorCode::Unsupported, "Web view is unavailable");
                return;
            };

            ensure_agent_runtime(web_view, &mut tab.agent_runtime);
            let expression =
                format!("window.__taborAgent.inspect({})", json::to_string(&element_id).unwrap());
            let script = agent_script(&mut tab.agent_runtime, &expression);
            let stream = Arc::clone(&stream);
            web_view.eval_js_string(&script, move |result| {
                let reply = match result {
                    Some(raw) => match json::from_str::<AgentElementDetail>(&raw) {
                        Ok(element) => SocketReply::AgentElement { element },
                        Err(err) => ipc::reply_error(
                            IpcErrorCode::Internal,
                            format!("Invalid agent element detail: {err}"),
                        ),
                    },
                    None => ipc::reply_error(
                        IpcErrorCode::Internal,
                        "Agent inspect returned no payload",
                    ),
                };
                if let Ok(mut stream) = stream.try_clone() {
                    ipc::send_reply(&mut stream, reply);
                }
            });
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_agent_screenshot(
        &mut self,
        tab_id: Option<TabId>,
        full_page: bool,
        stream: Arc<UnixStream>,
    ) {
        let Some(tab_id) = tab_id.or(self.tabs.active_id()) else {
            send_stream_error(&stream, IpcErrorCode::NotFound, "No active tab");
            return;
        };

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, full_page);
            send_stream_error(
                &stream,
                IpcErrorCode::Unsupported,
                "Agent control is only supported on macOS",
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                send_stream_error(&stream, IpcErrorCode::NotFound, "Tab not found");
                return;
            };
            if !tab.kind.is_web() {
                send_stream_error(&stream, IpcErrorCode::InvalidRequest, "Tab is not a web tab");
                return;
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                send_stream_error(&stream, IpcErrorCode::Unsupported, "Web view is unavailable");
                return;
            };

            ensure_agent_runtime(web_view, &mut tab.agent_runtime);

            let pending = Arc::new(Mutex::new(PendingAgentScreenshot::default()));
            let script =
                agent_script(&mut tab.agent_runtime, "window.__taborAgent.screenshotMeta()");
            let stream_for_meta = Arc::clone(&stream);
            let pending_for_meta = Arc::clone(&pending);
            web_view.eval_js_string(&script, move |result| {
                let meta = match result {
                    Some(raw) => json::from_str::<AgentScreenshotMeta>(&raw)
                        .map_err(|err| format!("Invalid screenshot metadata: {err}")),
                    None => Err(String::from("Agent screenshot metadata returned no payload")),
                };
                {
                    let mut pending = pending_for_meta.lock().unwrap();
                    pending.meta = Some(meta);
                }
                finish_pending_agent_screenshot(&pending_for_meta, &stream_for_meta);
            });

            let stream_for_capture = Arc::clone(&stream);
            let pending_for_capture = Arc::clone(&pending);
            let params = json::json!({
                "format": "png",
                "captureBeyondViewport": full_page,
                "fromSurface": true,
            });
            let command = web_view.devtools_command_json(
                "Page.captureScreenshot",
                Some(params),
                move |result| {
                    let data_base64 = result.and_then(|payload| {
                        payload
                            .get("data")
                            .and_then(JsonValue::as_str)
                            .map(ToOwned::to_owned)
                            .ok_or_else(|| {
                                String::from("Page.captureScreenshot returned no image data")
                            })
                    });
                    {
                        let mut pending = pending_for_capture.lock().unwrap();
                        pending.data_base64 = Some(data_base64);
                    }
                    finish_pending_agent_screenshot(&pending_for_capture, &stream_for_capture);
                },
            );
            if let Err(err) = command {
                send_stream_error(
                    &stream,
                    IpcErrorCode::Internal,
                    &format!("Page.captureScreenshot failed: {err}"),
                );
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_agent_events(
        &mut self,
        tab_id: Option<TabId>,
        since: Option<u64>,
        max: Option<usize>,
        kinds: Option<Vec<String>>,
        stream: Arc<UnixStream>,
    ) {
        let Some(tab_id) = tab_id.or(self.tabs.active_id()) else {
            send_stream_error(&stream, IpcErrorCode::NotFound, "No active tab");
            return;
        };

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, since, max, kinds);
            send_stream_error(
                &stream,
                IpcErrorCode::Unsupported,
                "Agent control is only supported on macOS",
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                send_stream_error(&stream, IpcErrorCode::NotFound, "Tab not found");
                return;
            };
            if !tab.kind.is_web() {
                send_stream_error(&stream, IpcErrorCode::InvalidRequest, "Tab is not a web tab");
                return;
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                send_stream_error(&stream, IpcErrorCode::Unsupported, "Web view is unavailable");
                return;
            };

            let since = since.unwrap_or_default();
            let max = max.unwrap_or(200);
            let raw_limit = if kinds.is_some() { 2048 } else { max.max(1) };
            let kind_filter = kinds.map(|values| {
                values
                    .into_iter()
                    .map(|value| value.trim().to_ascii_lowercase())
                    .collect::<Vec<_>>()
            });
            let (payloads, last_event_id) = web_view.devtools_events_since(since, raw_limit);
            let mut events = Vec::new();
            for payload in payloads {
                let event = match parse_agent_event_payload(&payload) {
                    Ok(event) => event,
                    Err(err) => {
                        send_stream_error(
                            &stream,
                            IpcErrorCode::Internal,
                            &format!("Invalid agent event payload: {err}"),
                        );
                        return;
                    },
                };
                if let Some(kind_filter) = &kind_filter {
                    if !kind_filter.iter().any(|kind| kind == &event.kind) {
                        continue;
                    }
                }
                events.push(event);
                if events.len() >= max {
                    break;
                }
            }

            if let Ok(mut stream) = stream.try_clone() {
                ipc::send_reply(&mut stream, SocketReply::AgentEvents { last_event_id, events });
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_agent_pdf(&mut self, tab_id: Option<TabId>, stream: Arc<UnixStream>) {
        let Some(tab_id) = tab_id.or(self.tabs.active_id()) else {
            send_stream_error(&stream, IpcErrorCode::NotFound, "No active tab");
            return;
        };

        #[cfg(not(target_os = "macos"))]
        {
            let _ = tab_id;
            send_stream_error(
                &stream,
                IpcErrorCode::Unsupported,
                "Agent control is only supported on macOS",
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                send_stream_error(&stream, IpcErrorCode::NotFound, "Tab not found");
                return;
            };
            if !tab.kind.is_web() {
                send_stream_error(&stream, IpcErrorCode::InvalidRequest, "Tab is not a web tab");
                return;
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                send_stream_error(&stream, IpcErrorCode::Unsupported, "Web view is unavailable");
                return;
            };

            let stream_for_reply = Arc::clone(&stream);
            let command = web_view.devtools_command_json("Page.printToPDF", None, move |result| {
                let reply = match result {
                    Ok(payload) => match payload.get("data").and_then(JsonValue::as_str) {
                        Some(data_base64) => SocketReply::AgentPdf {
                            pdf: AgentPdf { data_base64: data_base64.to_string() },
                        },
                        None => ipc::reply_error(
                            IpcErrorCode::Internal,
                            "Page.printToPDF returned no PDF data",
                        ),
                    },
                    Err(err) => ipc::reply_error(
                        IpcErrorCode::Internal,
                        format!("Page.printToPDF failed: {err}"),
                    ),
                };
                if let Ok(mut stream) = stream_for_reply.try_clone() {
                    ipc::send_reply(&mut stream, reply);
                }
            });
            if let Err(err) = command {
                send_stream_error(
                    &stream,
                    IpcErrorCode::Internal,
                    &format!("Page.printToPDF failed: {err}"),
                );
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_agent_upload(
        &mut self,
        tab_id: Option<TabId>,
        element_id: String,
        paths: Vec<String>,
        stream: Arc<UnixStream>,
    ) {
        let Some(tab_id) = tab_id.or(self.tabs.active_id()) else {
            send_stream_error(&stream, IpcErrorCode::NotFound, "No active tab");
            return;
        };

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, element_id, paths);
            send_stream_error(
                &stream,
                IpcErrorCode::Unsupported,
                "Agent control is only supported on macOS",
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                send_stream_error(&stream, IpcErrorCode::NotFound, "Tab not found");
                return;
            };
            if !tab.kind.is_web() {
                send_stream_error(&stream, IpcErrorCode::InvalidRequest, "Tab is not a web tab");
                return;
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                send_stream_error(&stream, IpcErrorCode::Unsupported, "Web view is unavailable");
                return;
            };

            ensure_agent_runtime(web_view, &mut tab.agent_runtime);
            let stream_for_reply = Arc::clone(&stream);
            let command = web_view.set_file_input_files(&element_id, paths, move |result| {
                let reply = match result {
                    Ok(raw) => match json::from_str::<JsonValue>(&raw) {
                        Ok(value) => {
                            if let Some(error) = value.get("error").and_then(JsonValue::as_str) {
                                ipc::reply_error(IpcErrorCode::InvalidRequest, error)
                            } else {
                                match json::from_value::<AgentElementDetail>(value) {
                                    Ok(element) => SocketReply::AgentElement { element },
                                    Err(err) => ipc::reply_error(
                                        IpcErrorCode::Internal,
                                        format!(
                                            "Invalid agent upload result: {err}; payload={raw}"
                                        ),
                                    ),
                                }
                            }
                        },
                        Err(err) => ipc::reply_error(
                            IpcErrorCode::Internal,
                            format!("Invalid agent upload result: {err}; payload={raw}"),
                        ),
                    },
                    Err(err) => ipc::reply_error(IpcErrorCode::Internal, err),
                };
                if let Ok(mut stream) = stream_for_reply.try_clone() {
                    ipc::send_reply(&mut stream, reply);
                }
            });
            if let Err(err) = command {
                send_stream_error(
                    &stream,
                    IpcErrorCode::Internal,
                    &format!("Agent upload failed: {err}"),
                );
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_agent_downloads(&mut self, tab_id: Option<TabId>, stream: Arc<UnixStream>) {
        let Some(tab_id) = tab_id.or(self.tabs.active_id()) else {
            send_stream_error(&stream, IpcErrorCode::NotFound, "No active tab");
            return;
        };

        #[cfg(not(target_os = "macos"))]
        {
            let _ = tab_id;
            send_stream_error(
                &stream,
                IpcErrorCode::Unsupported,
                "Agent control is only supported on macOS",
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                send_stream_error(&stream, IpcErrorCode::NotFound, "Tab not found");
                return;
            };
            if !tab.kind.is_web() {
                send_stream_error(&stream, IpcErrorCode::InvalidRequest, "Tab is not a web tab");
                return;
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                send_stream_error(&stream, IpcErrorCode::Unsupported, "Web view is unavailable");
                return;
            };

            if let Ok(mut stream) = stream.try_clone() {
                ipc::send_reply(
                    &mut stream,
                    SocketReply::AgentDownloads { downloads: web_view.downloads() },
                );
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn ipc_agent_act(
        &mut self,
        tab_id: Option<TabId>,
        actions: Vec<AgentAction>,
        observe: bool,
        stream: Arc<UnixStream>,
    ) {
        let Some(tab_id) = tab_id.or(self.tabs.active_id()) else {
            send_stream_error(&stream, IpcErrorCode::NotFound, "No active tab");
            return;
        };

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, actions, observe);
            send_stream_error(
                &stream,
                IpcErrorCode::Unsupported,
                "Agent control is only supported on macOS",
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tab) = self.tabs.get_mut(tab_id) else {
                send_stream_error(&stream, IpcErrorCode::NotFound, "Tab not found");
                return;
            };
            if !tab.kind.is_web() {
                send_stream_error(&stream, IpcErrorCode::InvalidRequest, "Tab is not a web tab");
                return;
            }
            let Some(web_view) = tab.web_view.as_mut() else {
                send_stream_error(&stream, IpcErrorCode::Unsupported, "Web view is unavailable");
                return;
            };

            ensure_agent_runtime(web_view, &mut tab.agent_runtime);
            let actions_json = match json::to_string(&actions) {
                Ok(actions_json) => actions_json,
                Err(err) => {
                    send_stream_error(
                        &stream,
                        IpcErrorCode::InvalidRequest,
                        &format!("Invalid agent actions: {err}"),
                    );
                    return;
                },
            };
            let expression = format!("window.__taborAgent.act({actions_json}, {observe})");
            let script = agent_object_script(&mut tab.agent_runtime, &expression);
            let stream = Arc::clone(&stream);
            web_view.set_visible(true);
            web_view.set_focus(true);
            web_view.eval_js_string_with_user_gesture(&script, move |result| {
                let reply = match result {
                    Some(raw) => match json::from_str::<AgentActResult>(&raw) {
                        Ok(result) => SocketReply::AgentAct { result },
                        Err(err) => ipc::reply_error(
                            IpcErrorCode::Internal,
                            format!("Invalid agent action result: {err}"),
                        ),
                    },
                    None => {
                        ipc::reply_error(IpcErrorCode::Internal, "Agent act returned no payload")
                    },
                };
                if let Ok(mut stream) = stream.try_clone() {
                    ipc::send_reply(&mut stream, reply);
                }
            });
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
        F: FnOnce(&mut ActionContext<'_, TabNotifier, EventProxy>),
    {
        if self.tabs.get(tab_id).is_none() {
            return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
        }

        if self.tabs.active_id() != Some(tab_id) {
            self.set_active_tab(tab_id);
        }

        let old_is_searching = self.active_is_searching();

        {
            let Some(active_tab) = self.tabs.active_mut() else {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            };

            let mut terminal = active_tab.terminal.lock();
            let mut context = ActionContext {
                cursor_blink_timed_out: &mut active_tab.cursor_blink_timed_out,
                prev_bell_cmd: &mut active_tab.prev_bell_cmd,
                message_buffer: &mut self.message_buffer,
                command_footer_feedback: &mut active_tab.command_footer_feedback,
                inline_search_state: &mut active_tab.inline_search_state,
                search_state: &mut active_tab.search_state,
                command_state: &mut active_tab.command_state,
                command_history: &mut self.command_history,
                tab_id: active_tab.id,
                tab_kind: &mut active_tab.kind,
                terminal_view_mode: &mut active_tab.terminal_view_mode,
                #[cfg(target_os = "macos")]
                browser_view_mode: &mut active_tab.browser_view_mode,
                #[cfg(target_os = "macos")]
                browser_multi_column_count: active_tab
                    .browser_multi_column_count_override
                    .or(self.default_browser_multi_column_count),
                #[cfg(target_os = "macos")]
                web_view: active_tab.web_view.as_mut(),
                #[cfg(target_os = "macos")]
                image_view: active_tab.image_view.as_mut(),
                #[cfg(target_os = "macos")]
                pdf_view: active_tab.pdf_view.as_mut(),
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
                foreground_process: active_tab.terminal_runtime.master_fd_shell_pid(),
                #[cfg(not(windows))]
                working_directory_hint: active_tab.terminal_runtime.working_directory_hint(),
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

        self.apply_pending_display_update(old_is_searching);
        if self.dirty && self.display.window.has_frame && !self.occluded {
            self.display.window.request_redraw();
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn cef_target_id(tab_id: TabId) -> u64 {
        (u64::from(tab_id.generation) << 32) | u64::from(tab_id.index)
    }

    #[cfg(target_os = "macos")]
    fn tab_id_from_target_id(target_id: u64) -> TabId {
        let index = (target_id & 0xFFFF_FFFF) as u32;
        let generation = (target_id >> 32) as u32;
        TabId::new(index, generation)
    }

    fn active_is_searching(&self) -> bool {
        self.tabs.active().is_some_and(|tab| tab.search_state.history_index.is_some())
    }

    fn apply_pending_display_update(&mut self, old_is_searching: bool) {
        if !self.display.pending_update.dirty {
            return;
        }

        if let Some(active_id) = self.tabs.active_id() {
            let multi_column_defaults = self.multi_column_defaults();
            Self::submit_display_update(
                active_id,
                &mut self.tabs,
                &mut self.display,
                old_is_searching,
                &self.config,
                multi_column_defaults,
            );
            self.dirty = true;
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

    fn terminal_viewport_for_kind(
        display: &Display,
        config: &UiConfig,
        kind: &WindowKind,
        terminal_view_mode: TerminalViewMode,
        exact_multi_column_count: Option<usize>,
    ) -> TerminalViewportLayout {
        let size_info = display.size_info_for_status_lines(usize::from(kind.has_status_bar()));
        if kind.is_terminal() {
            TerminalViewportLayout::new(
                &size_info,
                terminal_view_mode,
                &config.terminal.multi_column,
                exact_multi_column_count,
                display.ear_aware_top_regions(&size_info),
            )
        } else {
            TerminalViewportLayout::normal(size_info)
        }
    }

    fn terminal_viewport_for_tab(
        display: &Display,
        config: &UiConfig,
        multi_column_defaults: MultiColumnDefaults,
        tab: &TabState,
    ) -> TerminalViewportLayout {
        Self::terminal_viewport_for_kind(
            display,
            config,
            &tab.kind,
            tab.terminal_view_mode,
            tab.terminal_multi_column_count_override.or(multi_column_defaults.terminal),
        )
    }

    fn browser_viewport_for_kind(
        display: &Display,
        config: &UiConfig,
        kind: &WindowKind,
        browser_view_mode: BrowserViewMode,
        exact_column_count: Option<usize>,
    ) -> BrowserViewportLayout {
        let size_info = display.size_info_for_status_lines(usize::from(kind.has_status_bar()));
        BrowserViewportLayout::new(
            &size_info,
            display.window.scale_factor,
            browser_view_mode,
            &config.browser.multi_column,
            exact_column_count,
            display.ear_aware_top_regions(&size_info),
        )
    }

    fn browser_viewport_for_tab(
        display: &Display,
        config: &UiConfig,
        multi_column_defaults: MultiColumnDefaults,
        tab: &TabState,
    ) -> BrowserViewportLayout {
        Self::browser_viewport_for_kind(
            display,
            config,
            &tab.kind,
            tab.browser_view_mode,
            tab.browser_multi_column_count_override.or(multi_column_defaults.browser),
        )
    }

    #[cfg(unix)]
    fn ipc_web_mode(tab: &TabState) -> Option<IpcWebMode> {
        if !tab.kind.is_web() {
            return None;
        }

        Some(IpcWebMode::from(tab.web_command_state.mode()))
    }

    #[cfg(unix)]
    fn ipc_terminal_session(tab: &TabState) -> Option<IpcTerminalSessionState> {
        if !tab.kind.is_terminal() {
            return None;
        }

        Some(match &tab.terminal_runtime {
            TerminalRuntimeState::Local { .. } => IpcTerminalSessionState::Live,
            TerminalRuntimeState::Disconnected { clean_exit, exit_code, .. } => {
                IpcTerminalSessionState::Restored { clean_exit: *clean_exit, exit_code: *exit_code }
            },
        })
    }

    #[cfg(unix)]
    fn ipc_terminal_layout(
        display: &Display,
        config: &UiConfig,
        multi_column_defaults: MultiColumnDefaults,
        tab: &TabState,
    ) -> Option<IpcTerminalLayoutState> {
        if !tab.kind.is_terminal() {
            return None;
        }

        let layout = Self::terminal_viewport_for_tab(display, config, multi_column_defaults, tab);
        Some(IpcTerminalLayoutState {
            mode: tab.terminal_view_mode,
            target_columns: layout.target_columns(),
            strip_count: layout.strip_count(),
            strips: layout
                .strip_geometries()
                .into_iter()
                .map(|strip| IpcTerminalLayoutStrip {
                    start_column: strip.start_column,
                    column_count: strip.column_count,
                    y_offset_px: strip.y_offset_px,
                    visual_line_count: strip.visual_line_count,
                    logical_start_line: strip.logical_start_line,
                    logical_line_count: strip.logical_line_count,
                })
                .collect(),
        })
    }

    #[cfg(unix)]
    pub(crate) fn ipc_terminal_observe(
        &mut self,
        tab_id: TabId,
    ) -> Result<IpcTerminalObservation, IpcError> {
        let multi_column_defaults = self.multi_column_defaults();
        let tab = self
            .tabs
            .get(tab_id)
            .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
        Self::terminal_observation_for_tab(
            &self.display,
            &self.config,
            multi_column_defaults,
            tab,
            tab_id,
        )
    }

    #[cfg(unix)]
    pub(crate) fn ipc_terminal_read(
        &mut self,
        tab_id: TabId,
        scope: IpcTerminalReadScope,
        max_lines: Option<usize>,
    ) -> Result<IpcTerminalRead, IpcError> {
        let multi_column_defaults = self.multi_column_defaults();
        let tab = self
            .tabs
            .get(tab_id)
            .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
        Self::terminal_read_for_tab(
            &self.display,
            &self.config,
            multi_column_defaults,
            tab,
            tab_id,
            scope,
            max_lines,
        )
    }

    #[cfg(unix)]
    pub(crate) fn ipc_terminal_screenshot(
        &mut self,
        tab_id: TabId,
        scheduler: &mut Scheduler,
    ) -> Result<AgentScreenshot, IpcError> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tab_id, scheduler);
            Err(IpcError::new(
                IpcErrorCode::Unsupported,
                "Terminal screenshots are only available on macOS",
            ))
        }

        #[cfg(target_os = "macos")]
        {
            let Some(active_tab_id) = self.tabs.active_id() else {
                return Err(IpcError::new(IpcErrorCode::NotFound, "No active tab"));
            };
            if active_tab_id != tab_id {
                return Err(IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    "Terminal screenshots require the target tab to be active",
                ));
            }

            let multi_column_defaults = self.multi_column_defaults();
            let (size_info, strip_geometries) = {
                let tab = self
                    .tabs
                    .get(tab_id)
                    .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
                if !tab.kind.is_terminal() {
                    return Err(IpcError::new(
                        IpcErrorCode::InvalidRequest,
                        "Tab is not a terminal tab",
                    ));
                }
                let layout = Self::terminal_viewport_for_tab(
                    &self.display,
                    &self.config,
                    multi_column_defaults,
                    tab,
                );
                let size_info =
                    self.display.size_info_for_status_lines(usize::from(tab.kind.has_status_bar()));
                (size_info, layout.strip_geometries())
            };

            self.draw_inner(scheduler, false);

            let (image_width, image_height, pixels) = self.display.window_snapshot_rgba();
            let image =
                RgbaImage::from_raw(image_width, image_height, pixels).ok_or_else(|| {
                    IpcError::new(
                        IpcErrorCode::Internal,
                        "Failed to construct terminal screenshot image",
                    )
                })?;

            let cell_width = (size_info.cell_width() as usize).max(1);
            let cell_height = (size_info.cell_height() as usize).max(1);
            let left_column =
                strip_geometries.iter().map(|strip| strip.start_column).min().unwrap_or(0);
            let right_column = strip_geometries
                .iter()
                .map(|strip| strip.start_column + strip.column_count)
                .max()
                .unwrap_or(0);
            let top_px = strip_geometries
                .iter()
                .map(|strip| (size_info.padding_y() as usize).saturating_sub(strip.y_offset_px))
                .min()
                .unwrap_or(size_info.padding_y() as usize);
            let bottom_px = strip_geometries
                .iter()
                .map(|strip| {
                    (size_info.padding_y() as usize).saturating_sub(strip.y_offset_px)
                        + strip.visual_line_count * cell_height
                })
                .max()
                .unwrap_or(top_px);

            let left_px = size_info.padding_x() as usize + left_column * cell_width;
            let right_px = size_info.padding_x() as usize + right_column * cell_width;

            let crop_x =
                left_px.min(image_width as usize).min(image_width.saturating_sub(1) as usize);
            let crop_y =
                top_px.min(image_height as usize).min(image_height.saturating_sub(1) as usize);
            let crop_width =
                right_px.saturating_sub(left_px).max(1).min(image_width as usize - crop_x);
            let crop_height =
                bottom_px.saturating_sub(top_px).max(1).min(image_height as usize - crop_y);

            let cropped = image::imageops::crop_imm(
                &image,
                crop_x as u32,
                crop_y as u32,
                crop_width as u32,
                crop_height as u32,
            )
            .to_image();
            let mut cursor = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(cropped)
                .write_to(&mut cursor, image::ImageFormat::Png)
                .map_err(|err| {
                    IpcError::new(
                        IpcErrorCode::Internal,
                        format!("Failed to encode terminal screenshot: {err}"),
                    )
                })?;

            Ok(AgentScreenshot {
                data_base64: BASE64.encode(cursor.into_inner()),
                width: crop_width as u32,
                height: crop_height as u32,
                dpr: Some(self.display.window.scale_factor),
                scroll_x: None,
                scroll_y: None,
            })
        }
    }

    #[cfg(unix)]
    fn terminal_observation_for_tab(
        display: &Display,
        config: &UiConfig,
        multi_column_defaults: MultiColumnDefaults,
        tab: &TabState,
        tab_id: TabId,
    ) -> Result<IpcTerminalObservation, IpcError> {
        if !tab.kind.is_terminal() {
            return Err(IpcError::new(IpcErrorCode::InvalidRequest, "Tab is not a terminal tab"));
        }

        let layout = Self::terminal_viewport_for_tab(display, config, multi_column_defaults, tab);
        let layout_state = Self::ipc_terminal_layout(display, config, multi_column_defaults, tab)
            .expect("terminal tabs should have terminal layout");
        let size_info = display.size_info_for_status_lines(usize::from(tab.kind.has_status_bar()));
        let terminal = tab.terminal.lock();
        let terminal_content = terminal.renderable_content();
        let logical_cursor = terminal_content.cursor.point;
        let selection_range = terminal_content.selection;
        let display_offset = terminal_content.display_offset;
        let viewport_lines =
            Self::terminal_viewport_lines(&terminal, &layout_state, display_offset);
        let selection = Self::terminal_selection(selection_range, &terminal);
        let mut hint_state = HintState::new(config.hints.alphabet());
        let mut content = crate::display::content::RenderableContent::new(
            config,
            &mut hint_state,
            &terminal,
            RenderableContentContext {
                colors: display.colors,
                size: size_info,
                layout,
                search: None,
                focused_match: None,
                cursor_hidden: tab.cursor_blink_timed_out,
                ime_preedit_active: false,
            },
        );

        let cells = content
            .by_ref()
            .map(|cell| IpcTerminalCell {
                character: cell.character,
                logical_point: Self::terminal_point(cell.logical_point),
                visual_point: IpcTerminalVisualPoint {
                    line: cell.point.line,
                    column: cell.point.column.0,
                    y_offset_px: cell.y_offset_px,
                },
                fg: Self::rgb_hex(cell.fg),
                bg: Self::rgb_hex(cell.bg),
                underline: Self::rgb_hex(cell.underline),
                bg_alpha: cell.bg_alpha,
                flags: Self::terminal_flag_names(cell.flags),
            })
            .collect();
        let renderable_cursor = content.cursor();
        let cursor = Some(IpcTerminalCursor {
            logical_point: Self::terminal_point(logical_cursor),
            visual_point: (renderable_cursor.shape() != CursorShape::Hidden).then_some(
                IpcTerminalVisualPoint {
                    line: renderable_cursor.point().line,
                    column: renderable_cursor.point().column.0,
                    y_offset_px: renderable_cursor.y_offset_px(),
                },
            ),
            shape: Self::terminal_cursor_shape(renderable_cursor.shape()),
            color: Self::rgb_hex(renderable_cursor.color()),
            width: renderable_cursor.width().get(),
        });

        Ok(IpcTerminalObservation {
            tab_id: tab_id.into(),
            title: tab.title.clone(),
            program_name: tab.program_name.clone(),
            session: Self::ipc_terminal_session(tab)
                .expect("terminal tabs should have terminal session"),
            display_offset,
            layout: layout_state,
            viewport_lines,
            cursor,
            selection,
            cells,
        })
    }

    #[cfg(unix)]
    fn terminal_read_for_tab(
        display: &Display,
        config: &UiConfig,
        multi_column_defaults: MultiColumnDefaults,
        tab: &TabState,
        tab_id: TabId,
        scope: IpcTerminalReadScope,
        max_lines: Option<usize>,
    ) -> Result<IpcTerminalRead, IpcError> {
        if !tab.kind.is_terminal() {
            return Err(IpcError::new(IpcErrorCode::InvalidRequest, "Tab is not a terminal tab"));
        }

        let layout_state = Self::ipc_terminal_layout(display, config, multi_column_defaults, tab)
            .expect("terminal tabs should have terminal layout");
        let terminal = tab.terminal.lock();
        let display_offset = terminal.grid().display_offset();
        let selection =
            Self::terminal_selection(terminal.renderable_content().selection, &terminal);

        let read = match scope {
            IpcTerminalReadScope::Viewport => IpcTerminalRead {
                tab_id: tab_id.into(),
                scope,
                display_offset: Some(display_offset),
                layout: Some(layout_state.clone()),
                viewport_lines: Self::terminal_viewport_lines(
                    &terminal,
                    &layout_state,
                    display_offset,
                ),
                lines: Vec::new(),
                selection,
            },
            IpcTerminalReadScope::Buffer => IpcTerminalRead {
                tab_id: tab_id.into(),
                scope,
                display_offset: None,
                layout: None,
                viewport_lines: Vec::new(),
                lines: terminal.export_preview_lines(max_lines.unwrap_or(200)),
                selection: None,
            },
            IpcTerminalReadScope::Selection => IpcTerminalRead {
                tab_id: tab_id.into(),
                scope,
                display_offset: None,
                layout: None,
                viewport_lines: Vec::new(),
                lines: Vec::new(),
                selection,
            },
        };

        Ok(read)
    }

    #[cfg(unix)]
    fn terminal_viewport_lines(
        terminal: &Term<EventProxy>,
        layout: &IpcTerminalLayoutState,
        display_offset: usize,
    ) -> Vec<IpcTerminalViewportLine> {
        layout
            .strips
            .iter()
            .enumerate()
            .flat_map(|(strip_index, strip)| {
                (0..strip.logical_line_count).map(move |line_index| {
                    let viewport_line = strip.logical_start_line + line_index;
                    let logical_line = viewport_line as i32 - display_offset as i32;
                    let point = Point::new(Line(logical_line), Column(0));
                    let text = terminal.bounds_to_string(
                        point,
                        Point::new(Line(logical_line), terminal.last_column()),
                    );
                    IpcTerminalViewportLine {
                        strip_index,
                        logical_line,
                        visual_line: line_index,
                        text,
                    }
                })
            })
            .collect()
    }

    #[cfg(unix)]
    fn terminal_selection(
        selection_range: Option<tabor_terminal::selection::SelectionRange>,
        terminal: &Term<EventProxy>,
    ) -> Option<IpcTerminalSelection> {
        let range = selection_range?;
        let text = terminal.selection_to_string()?;
        Some(IpcTerminalSelection {
            start: Self::terminal_point(range.start),
            end: Self::terminal_point(range.end),
            is_block: range.is_block,
            text,
        })
    }

    #[cfg(unix)]
    fn terminal_point(point: Point) -> IpcTerminalPoint {
        IpcTerminalPoint { line: point.line.0, column: point.column.0 }
    }

    #[cfg(unix)]
    fn terminal_cursor_shape(shape: CursorShape) -> IpcTerminalCursorShape {
        match shape {
            CursorShape::Hidden => IpcTerminalCursorShape::Hidden,
            CursorShape::Block => IpcTerminalCursorShape::Block,
            CursorShape::Underline => IpcTerminalCursorShape::Underline,
            CursorShape::Beam => IpcTerminalCursorShape::Beam,
            CursorShape::HollowBlock => IpcTerminalCursorShape::HollowBlock,
        }
    }

    #[cfg(unix)]
    fn terminal_flag_names(flags: Flags) -> Vec<String> {
        const FLAG_NAMES: &[(Flags, &str)] = &[
            (Flags::INVERSE, "inverse"),
            (Flags::BOLD, "bold"),
            (Flags::ITALIC, "italic"),
            (Flags::UNDERLINE, "underline"),
            (Flags::WRAPLINE, "wrapline"),
            (Flags::WIDE_CHAR, "wide_char"),
            (Flags::WIDE_CHAR_SPACER, "wide_char_spacer"),
            (Flags::DIM, "dim"),
            (Flags::HIDDEN, "hidden"),
            (Flags::STRIKEOUT, "strikeout"),
            (Flags::LEADING_WIDE_CHAR_SPACER, "leading_wide_char_spacer"),
            (Flags::DOUBLE_UNDERLINE, "double_underline"),
            (Flags::UNDERCURL, "undercurl"),
            (Flags::DOTTED_UNDERLINE, "dotted_underline"),
            (Flags::DASHED_UNDERLINE, "dashed_underline"),
        ];

        FLAG_NAMES
            .iter()
            .filter_map(|(flag, name)| flags.contains(*flag).then_some((*name).to_string()))
            .collect()
    }

    #[cfg(unix)]
    fn rgb_hex(color: Rgb) -> String {
        let (r, g, b) = color.as_tuple();
        format!("#{r:02x}{g:02x}{b:02x}")
    }

    #[cfg(unix)]
    fn ipc_browser_layout(
        display: &Display,
        config: &UiConfig,
        multi_column_defaults: MultiColumnDefaults,
        tab: &TabState,
    ) -> Option<IpcBrowserLayoutState> {
        if !tab.kind.is_web() {
            return None;
        }

        let layout = Self::browser_viewport_for_tab(display, config, multi_column_defaults, tab);
        let columns = (0..layout.column_count())
            .filter_map(|column_index| layout.column_rect(column_index))
            .collect();
        let acceleration = tab
            .web_view
            .as_ref()
            .map(|web_view| web_view.acceleration_info())
            .expect("web tabs should have a WebView");

        Some(IpcBrowserLayoutState {
            mode: layout.mode(),
            target_width_px: layout.target_width_px(),
            logical_width: layout.logical_width(),
            logical_height: layout.logical_height(),
            column_count: layout.column_count(),
            viewport: layout.viewport(),
            columns,
            acceleration: IpcBrowserAccelerationInfo {
                state: match acceleration.state {
                    WebAccelerationState::Pending => IpcBrowserAccelerationState::Pending,
                    WebAccelerationState::Ready => IpcBrowserAccelerationState::Ready,
                    WebAccelerationState::Failed => IpcBrowserAccelerationState::Failed,
                },
                frame_delivery_mode: match acceleration.frame_delivery_mode {
                    crate::macos::webview::WebFrameDeliveryMode::CefInternal => {
                        IpcWebFrameDeliveryMode::CefInternal
                    },
                },
                main_surface_width: acceleration.main_surface_width,
                main_surface_height: acceleration.main_surface_height,
                popup_surface_width: acceleration.popup_surface_width,
                popup_surface_height: acceleration.popup_surface_height,
            },
        })
    }

    #[cfg(all(unix, target_os = "macos"))]
    fn ipc_image_view(tab: &TabState, display: &Display) -> Option<IpcImageViewState> {
        let image_view = tab.image_view.as_ref()?;
        let (width, height) = image_view
            .bitmap()
            .map(|bitmap| (Some(bitmap.width), Some(bitmap.height)))
            .unwrap_or((None, None));
        let error = match &image_view.load_state {
            ImageLoadState::Error(message) => Some(message.clone()),
            ImageLoadState::Loading | ImageLoadState::Ready => None,
        };
        let viewport = winit::dpi::PhysicalSize::new(
            display.size_info.width() as u32,
            display.size_info.height() as u32,
        );

        Some(IpcImageViewState {
            source: image_view.source.clone(),
            state: match image_view.load_state {
                ImageLoadState::Loading => IpcImageLoadState::Loading,
                ImageLoadState::Ready => IpcImageLoadState::Ready,
                ImageLoadState::Error(_) => IpcImageLoadState::Error,
            },
            scale_mode: match image_view.scale_mode {
                ImageScaleMode::Fit => IpcImageScaleMode::Fit,
                ImageScaleMode::Fill => IpcImageScaleMode::Fill,
                ImageScaleMode::Actual => IpcImageScaleMode::Actual,
                ImageScaleMode::Manual => IpcImageScaleMode::Manual,
            },
            zoom: image_view.zoom_factor(viewport).unwrap_or(1.0),
            rotation_quarter_turns: image_view.rotation_quarter_turns,
            width,
            height,
            error,
        })
    }

    #[cfg(all(unix, target_os = "macos"))]
    fn ipc_pdf_view(tab: &TabState, display: &Display) -> Option<IpcPdfViewState> {
        let pdf_view = tab.pdf_view.as_ref()?;
        let error = match &pdf_view.load_state {
            PdfLoadState::Error(message) => Some(message.clone()),
            PdfLoadState::Loading | PdfLoadState::Ready => None,
        };
        let viewport = winit::dpi::PhysicalSize::new(
            display.size_info.width() as u32,
            display.size_info.height() as u32,
        );

        Some(IpcPdfViewState {
            source: pdf_view.source.clone(),
            state: match pdf_view.load_state {
                PdfLoadState::Loading => ipc::IpcPdfLoadState::Loading,
                PdfLoadState::Ready => ipc::IpcPdfLoadState::Ready,
                PdfLoadState::Error(_) => ipc::IpcPdfLoadState::Error,
            },
            page_count: pdf_view.page_count(),
            current_page: pdf_view.current_page(viewport),
            zoom_mode: match pdf_view.zoom_mode {
                PdfZoomMode::FitWidth => ipc::IpcPdfZoomMode::FitWidth,
                PdfZoomMode::FitPage => ipc::IpcPdfZoomMode::FitPage,
                PdfZoomMode::Actual => ipc::IpcPdfZoomMode::Actual,
                PdfZoomMode::Manual => ipc::IpcPdfZoomMode::Manual,
            },
            zoom: pdf_view.zoom_factor(viewport).unwrap_or(1.0),
            search_query: pdf_view.search_query().map(str::to_string),
            search_match_count: pdf_view.active_search_match_count(),
            error,
        })
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn ipc_image_view(_tab: &TabState, _display: &Display) -> Option<IpcImageViewState> {
        None
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn ipc_pdf_view(_tab: &TabState, _display: &Display) -> Option<IpcPdfViewState> {
        None
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
                WindowKind::Image { source } => source.to_lowercase().contains(&needle),
                WindowKind::Pdf { source } => source.to_lowercase().contains(&needle),
                WindowKind::Terminal => false,
            };

            if title.contains(&needle) || url_match { Some(tab.id) } else { None }
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

            if trimmed == format!("group {group_id}") { None } else { Some(trimmed.to_string()) }
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
        if tab.kind.is_image() {
            return false;
        }

        let program_name = match &tab.terminal_runtime {
            TerminalRuntimeState::Local {
                #[cfg(not(windows))]
                master_fd,
                #[cfg(not(windows))]
                shell_pid,
                ..
            } => foreground_process_name(*master_fd, *shell_pid).ok(),
            #[cfg(unix)]
            TerminalRuntimeState::Disconnected { .. } => Some(tab.program_name.clone()),
        };

        let Some(program_name) = program_name else {
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

        if old_config.browser != self.config.browser {
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

    #[cfg(target_os = "macos")]
    pub fn refresh_pdf_dark_mode(&mut self, event_proxy: &EventLoopProxy<Event>) {
        let dark_mode_enabled =
            matches!(self.config.window.theme(), Some(winit::window::Theme::Dark));
        let viewport = winit::dpi::PhysicalSize::new(
            self.display.size_info.width() as u32,
            self.display.size_info.height() as u32,
        );
        let mut changed = false;
        let mut raster_requests = Vec::new();
        for tab in self.tabs.iter_mut() {
            let Some(pdf_view) = tab.pdf_view.as_mut() else {
                continue;
            };
            if !pdf_view.set_dark_mode_enabled(dark_mode_enabled) {
                continue;
            }

            changed = true;
            raster_requests.extend(
                pdf_view
                    .take_visible_render_requests(viewport)
                    .into_iter()
                    .map(|request| (tab.id, request)),
            );
        }

        for (tab_id, request) in raster_requests {
            request_pdf_raster(event_proxy, self.display.window.id(), tab_id, request);
        }

        if !changed {
            return;
        }

        self.display.pending_update.dirty = true;
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.refresh_tab_panel();
        self.dirty = true;
        if self.display.window.has_frame {
            self.display.window.request_redraw();
        }
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

    fn draw_inner(
        &mut self,
        scheduler: &mut Scheduler,
        #[cfg(target_os = "macos")] highlight_notch_ears: bool,
    ) {
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
        let multi_column_defaults = self.multi_column_defaults();
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };

        match draw_mode(&tab.kind) {
            DrawMode::Web => {
                let url = match &tab.kind {
                    WindowKind::Web { url } => url.as_str(),
                    WindowKind::Image { source } => source.as_str(),
                    WindowKind::Pdf { source } => source.as_str(),
                    WindowKind::Terminal => "",
                };
                let browser_layout = Self::browser_viewport_for_tab(
                    &self.display,
                    &self.config,
                    multi_column_defaults,
                    tab,
                );
                self.display.draw_web(
                    scheduler,
                    &self.message_buffer,
                    &self.config,
                    url,
                    &tab.command_state,
                    #[cfg(target_os = "macos")]
                    crate::display::MacOsWebDraw::new(
                        tab.web_view.as_ref(),
                        browser_layout,
                        highlight_notch_ears,
                    ),
                );
            },
            DrawMode::Image =>
            {
                #[cfg(target_os = "macos")]
                if let Some(image_view) = tab.image_view.as_ref() {
                    self.display.draw_image(
                        scheduler,
                        &self.message_buffer,
                        &self.config,
                        image_view,
                        &tab.command_state,
                        highlight_notch_ears,
                    );
                }
            },
            DrawMode::Pdf =>
            {
                #[cfg(target_os = "macos")]
                if let Some(pdf_view) = tab.pdf_view.as_mut() {
                    self.display.draw_pdf(
                        scheduler,
                        &self.message_buffer,
                        &self.config,
                        pdf_view,
                        &tab.command_state,
                        highlight_notch_ears,
                    );
                }
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
                    tab.command_footer_feedback.message(),
                    #[cfg(target_os = "macos")]
                    highlight_notch_ears,
                );
            },
        }
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler) {
        self.draw_inner(
            scheduler,
            #[cfg(target_os = "macos")]
            false,
        );
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
        self.sync_macos_fullscreen_transition();
        #[cfg(target_os = "macos")]
        if self.handle_tab_panel_event(&event, event_proxy) {
            let old_is_searching = self.active_is_searching();
            self.apply_pending_display_update(old_is_searching);
            return;
        }

        match event {
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() && !self.display.pending_update.dirty {
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
            if let WinitEvent::WindowEvent { event: WindowEvent::Focused(is_focused), .. } = &event
            {
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
                    EventType::WebEditableFocus { editable } => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };
                        self.handle_web_editable_focus(tab_id, *editable);
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
                    #[cfg(target_os = "macos")]
                    EventType::WebViewDirty => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };
                        if Some(tab_id) == active_id {
                            self.dirty = true;
                            if self.display.window.has_frame {
                                self.display.window.request_redraw();
                            }
                        }
                        continue;
                    },
                    #[cfg(target_os = "macos")]
                    EventType::ImageLoaded { image } => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };
                        self.handle_image_loaded(tab_id, image.clone());
                        continue;
                    },
                    #[cfg(target_os = "macos")]
                    EventType::PdfLoaded { pdf } => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };
                        self.handle_pdf_loaded(tab_id, pdf.clone(), event_proxy);
                        continue;
                    },
                    #[cfg(target_os = "macos")]
                    EventType::PdfPageRasterized { raster } => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };
                        self.handle_pdf_page_rasterized(tab_id, raster.clone(), event_proxy);
                        continue;
                    },
                    EventType::Terminal(term_event) => {
                        let Some(tab_id) = event.tab_id() else {
                            continue;
                        };

                        if self.tabs.get(tab_id).is_some_and(|tab| !tab.kind.is_terminal()) {
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

        let old_is_searching = self.active_is_searching();

        {
            let Some(active_tab) = self.tabs.active_mut() else {
                return;
            };

            let mut terminal = active_tab.terminal.lock();
            let context = ActionContext {
                cursor_blink_timed_out: &mut active_tab.cursor_blink_timed_out,
                prev_bell_cmd: &mut active_tab.prev_bell_cmd,
                message_buffer: &mut self.message_buffer,
                command_footer_feedback: &mut active_tab.command_footer_feedback,
                inline_search_state: &mut active_tab.inline_search_state,
                search_state: &mut active_tab.search_state,
                command_state: &mut active_tab.command_state,
                command_history: &mut self.command_history,
                tab_id: active_tab.id,
                tab_kind: &mut active_tab.kind,
                terminal_view_mode: &mut active_tab.terminal_view_mode,
                #[cfg(target_os = "macos")]
                browser_view_mode: &mut active_tab.browser_view_mode,
                #[cfg(target_os = "macos")]
                browser_multi_column_count: active_tab
                    .browser_multi_column_count_override
                    .or(self.default_browser_multi_column_count),
                #[cfg(target_os = "macos")]
                web_view: active_tab.web_view.as_mut(),
                #[cfg(target_os = "macos")]
                image_view: active_tab.image_view.as_mut(),
                #[cfg(target_os = "macos")]
                pdf_view: active_tab.pdf_view.as_mut(),
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
                foreground_process: active_tab.terminal_runtime.master_fd_shell_pid(),
                #[cfg(not(windows))]
                working_directory_hint: active_tab.terminal_runtime.working_directory_hint(),
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

        self.apply_pending_display_update(old_is_searching);

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
                event: WindowEvent::CursorMoved { position, .. }, ..
            } => {
                let update =
                    self.display.tab_panel.cursor_moved(*position, &self.display.size_info);
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
                        crate::tab_panel::TabPanelCommand::WindowClose => {
                            self.display.window.request_close();
                        },
                        crate::tab_panel::TabPanelCommand::WindowMinimize => {
                            self.display.window.set_minimized(true);
                        },
                        crate::tab_panel::TabPanelCommand::WindowToggleFullscreen => {
                            self.display.window.toggle_fullscreen();
                            self.display.pending_update.dirty = true;
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
            WinitEvent::WindowEvent { event: WindowEvent::MouseWheel { .. }, .. } => {
                self.display.tab_panel.should_capture_last()
            },
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
                let name = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };

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
            WebCommand::SetMark { name, url, scroll_x, scroll_y } => {
                let Some(tab_id) = event.tab_id().or(self.tabs.active_id()) else {
                    return;
                };
                if let Some(tab) = self.tabs.get_mut(tab_id) {
                    tab.web_command_state.set_mark(*name, url.clone(), *scroll_x, *scroll_y);
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
        let multi_column_defaults = self.multi_column_defaults();
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
                let logical_size = Self::terminal_viewport_for_tab(
                    &self.display,
                    &self.config,
                    multi_column_defaults,
                    tab,
                )
                .logical_size(&self.display.size_info);
                let text = format(logical_size.into());
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

        let logical_size = Self::terminal_viewport_for_tab(
            &self.display,
            &self.config,
            self.multi_column_defaults(),
            tab,
        )
        .logical_size(&self.display.size_info);
        let size = TermSize::new(logical_size.columns(), logical_size.screen_lines());
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
        old_is_searching: bool,
        config: &UiConfig,
        multi_column_defaults: MultiColumnDefaults,
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
                &mut active_tab.search_state,
                DisplayUpdateOptions {
                    web_status_bar,
                    terminal_view_mode: Some(active_tab.terminal_view_mode),
                    exact_multi_column_count: active_tab
                        .terminal_multi_column_count_override
                        .or(multi_column_defaults.terminal),
                },
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
            let layout =
                Self::browser_viewport_for_tab(display, config, multi_column_defaults, tab);
            if let Some(web_view) = tab.web_view.as_mut() {
                web_view.update_frame(&display.window, &display.size_info, layout);
            }
        }

        for tab in tabs.iter_mut() {
            if tab.id == active_id {
                continue;
            }

            let layout =
                Self::terminal_viewport_for_tab(display, config, multi_column_defaults, tab);
            let new_size = layout.logical_size(&display.size_info);
            let mut tab_terminal = tab.terminal.lock();
            if tab_terminal.screen_lines() != new_size.screen_lines()
                || tab_terminal.columns() != new_size.columns()
            {
                tab.notifier.on_resize(new_size.into());
                tab_terminal.resize_with_anchor(new_size, ResizeAnchor::Top);
            }
        }
    }
}

#[cfg(unix)]
fn terminal_key_bytes(input: TerminalKeyInput) -> Result<Vec<u8>, IpcError> {
    if input.state != crate::ipc::WebKeyState::Down {
        return Ok(Vec::new());
    }

    let text = match input.text {
        Some(text) => text,
        None => default_terminal_key_text(&input.key)?,
    };

    if text.is_empty() {
        return Ok(Vec::new());
    }

    let mut bytes = Vec::with_capacity(text.len() + 1);
    if input.modifiers.alt {
        bytes.push(0x1b);
    }

    if input.modifiers.control {
        if let Some(control) = terminal_control_byte(&text) {
            bytes.push(control);
            return Ok(bytes);
        }
    }

    bytes.extend_from_slice(text.as_bytes());
    Ok(bytes)
}

#[cfg(unix)]
fn default_terminal_key_text(key: &str) -> Result<String, IpcError> {
    let trimmed = key.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let text = match lowered.as_str() {
        "enter" => "\r".to_string(),
        "tab" => "\t".to_string(),
        "backspace" => "\u{7f}".to_string(),
        "space" => " ".to_string(),
        _ => {
            let mut chars = trimmed.chars();
            let Some(ch) = chars.next() else {
                return Err(IpcError::new(IpcErrorCode::InvalidRequest, "key is empty"));
            };
            if chars.next().is_some() {
                return Err(IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    "terminal_key requires text for multi-character keys",
                ));
            }
            ch.to_string()
        },
    };

    Ok(text)
}

#[cfg(unix)]
fn terminal_control_byte(text: &str) -> Option<u8> {
    let mut chars = text.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }

    if ch.is_ascii_alphabetic() {
        return Some((ch.to_ascii_uppercase() as u8) & 0x1f);
    }

    match ch {
        '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

#[cfg(unix)]
fn send_stream_error(stream: &Arc<UnixStream>, code: IpcErrorCode, message: &str) {
    if let Ok(mut stream) = stream.try_clone() {
        ipc::send_reply(&mut stream, ipc::reply_error(code, message));
    }
}

#[cfg(target_os = "macos")]
fn finish_pending_agent_screenshot(
    pending: &Arc<Mutex<PendingAgentScreenshot>>,
    stream: &Arc<UnixStream>,
) {
    let Some((meta, data_base64)) = ({
        let mut pending = pending.lock().unwrap();
        if pending.meta.is_none() || pending.data_base64.is_none() {
            None
        } else {
            Some((pending.meta.take().unwrap(), pending.data_base64.take().unwrap()))
        }
    }) else {
        return;
    };

    let reply = match (meta, data_base64) {
        (Ok(meta), Ok(data_base64)) => SocketReply::AgentScreenshot {
            screenshot: AgentScreenshot {
                data_base64,
                width: meta.width,
                height: meta.height,
                dpr: meta.dpr,
                scroll_x: meta.scroll_x,
                scroll_y: meta.scroll_y,
            },
        },
        (Err(err), _) | (_, Err(err)) => ipc::reply_error(IpcErrorCode::Internal, err),
    };

    if let Ok(mut stream) = stream.try_clone() {
        ipc::send_reply(&mut stream, reply);
    }
}

#[cfg(target_os = "macos")]
fn parse_agent_event_payload(payload: &str) -> Result<AgentEvent, String> {
    let value = json::from_str::<JsonValue>(payload).map_err(|err| err.to_string())?;
    let method = value
        .get("method")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| String::from("missing method"))?;
    let id =
        value.get("id").and_then(JsonValue::as_u64).ok_or_else(|| String::from("missing id"))?;
    let params = value.get("params").cloned();
    Ok(AgentEvent { id, kind: agent_event_kind(method), method: method.to_string(), params })
}

#[cfg(target_os = "macos")]
fn agent_event_kind(method: &str) -> String {
    if method.starts_with("Network.") {
        return String::from("network");
    }
    if method.starts_with("Runtime.consoleAPICalled")
        || method.starts_with("Runtime.exception")
        || method.starts_with("Log.entryAdded")
    {
        return String::from("console");
    }
    if method.starts_with("Page.download") {
        return String::from("download");
    }
    if method.starts_with("Page.javascriptDialog") {
        return String::from("dialog");
    }
    if let Some((prefix, _)) = method.split_once('.') {
        return prefix.to_ascii_lowercase();
    }
    String::from("other")
}

#[cfg(target_os = "macos")]
fn normalize_favicon_char_counter(next_favicon_char: u32) -> u32 {
    const BMP_END: u32 = 0xF8FF;
    const SUP_START: u32 = 0xF0000;
    const SUP_END: u32 = 0xFFFFD;

    let next_favicon_char = if next_favicon_char < FIRST_DYNAMIC_FAVICON_CHAR {
        FIRST_DYNAMIC_FAVICON_CHAR
    } else if next_favicon_char > BMP_END && next_favicon_char < SUP_START {
        SUP_START
    } else {
        next_favicon_char
    };

    if next_favicon_char > SUP_END {
        panic!("Ran out of favicon glyph slots");
    }

    next_favicon_char
}

impl Drop for WindowContext {
    fn drop(&mut self) {
        // Local PTYs are tied to the UI process. Broker-backed terminals outlive the UI.
        for tab in self.tabs.iter_mut() {
            tab.notifier.detach();
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

    #[cfg(target_os = "macos")]
    #[test]
    fn dynamic_favicon_chars_start_after_static_sidebar_icons() {
        assert_eq!(normalize_favicon_char_counter(0xE000), FIRST_DYNAMIC_FAVICON_CHAR);
        assert_eq!(normalize_favicon_char_counter(0xE00F), FIRST_DYNAMIC_FAVICON_CHAR);
        assert_eq!(normalize_favicon_char_counter(0xF900), 0xF0000);
    }

    #[cfg(unix)]
    #[test]
    fn non_empty_workspace_group_layouts_skip_empty_groups() {
        let layout = WorkspaceLayout {
            protocol_version: workspace::WORKSPACE_PROTOCOL_VERSION,
            active_tab_id: None,
            groups: vec![
                WorkspaceGroupLayout { name: None, tabs: Vec::new() },
                WorkspaceGroupLayout {
                    name: Some(String::from("test")),
                    tabs: vec![WorkspaceTabLayout {
                        persistent_id: String::from("g1-t0"),
                        custom_title: None,
                        terminal_view_mode: TerminalViewMode::Normal,
                        terminal_multi_column_count_override: None,
                        kind: WorkspaceTabKind::Terminal {
                            terminal_id: 1,
                            launch_options: tty::Options::default(),
                        },
                    }],
                },
            ],
        };

        let groups = non_empty_workspace_group_layouts(&layout)
            .map(|(index, group)| (index, group.name.clone(), group.tabs.len()))
            .collect::<Vec<_>>();

        assert_eq!(groups, vec![(1, Some(String::from("test")), 1)]);
    }
}
