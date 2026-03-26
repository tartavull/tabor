//! Tabor socket IPC.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Result as IoResult, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use std::{env, fs, process};

use log::{error, warn};
use std::result::Result;
use winit::event_loop::EventLoopProxy;

use tabor_terminal::thread;
use tabor_terminal::vi_mode::ViMotion;

use crate::cli::{IpcConfig, IpcGetConfig, Options, WindowOptions};
use crate::config::ui_config::Program;
use crate::config::{Action, MouseAction, SearchAction, ViAction};
use crate::display::browser_layout::{BrowserViewMode, BrowserViewportRect};
use crate::display::terminal_layout::TerminalViewMode;
use crate::event::{Event, EventType};
#[cfg(target_os = "macos")]
use crate::macos;
#[cfg(target_os = "macos")]
use crate::macos::web_commands::WebMode;
use crate::tabs::TabId;
use crate::window_kind::WindowKind;

/// Environment variable name for the IPC socket path.
const TABOR_SOCKET_ENV: &str = "TABOR_SOCKET";

const IPC_PROTOCOL_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IpcTabId {
    pub index: u32,
    pub generation: u32,
}

impl From<TabId> for IpcTabId {
    fn from(tab_id: TabId) -> Self {
        Self { index: tab_id.index, generation: tab_id.generation }
    }
}

impl From<IpcTabId> for TabId {
    fn from(tab_id: IpcTabId) -> Self {
        TabId::new(tab_id.index, tab_id.generation)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcTabKind {
    Terminal,
    Web { url: String },
    Image { source: String },
}

impl From<&WindowKind> for IpcTabKind {
    fn from(kind: &WindowKind) -> Self {
        match kind {
            WindowKind::Terminal => Self::Terminal,
            WindowKind::Web { url } => Self::Web { url: url.clone() },
            WindowKind::Image { source } => Self::Image { source: source.clone() },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcTabActivity {
    pub has_unseen_output: bool,
    pub last_output_ms_ago: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcTerminalLayoutState {
    pub mode: TerminalViewMode,
    pub target_columns: usize,
    pub strip_count: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcBrowserAccelerationState {
    Pending,
    Ready,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcWebFrameDeliveryMode {
    CefInternal,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcBrowserAccelerationInfo {
    pub state: IpcBrowserAccelerationState,
    pub frame_delivery_mode: IpcWebFrameDeliveryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_surface_width: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_surface_height: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popup_surface_width: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popup_surface_height: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcBrowserLayoutState {
    pub mode: BrowserViewMode,
    pub target_width_px: usize,
    pub logical_width: usize,
    pub logical_height: usize,
    pub column_count: usize,
    pub viewport: BrowserViewportRect,
    pub columns: Vec<BrowserViewportRect>,
    pub acceleration: IpcBrowserAccelerationInfo,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcImageLoadState {
    Loading,
    Ready,
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcImageScaleMode {
    Fit,
    Fill,
    Actual,
    Manual,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IpcImageViewState {
    pub source: String,
    pub state: IpcImageLoadState,
    pub scale_mode: IpcImageScaleMode,
    pub zoom: f64,
    pub rotation_quarter_turns: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcWebMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    Hint,
    MarkSet,
    MarkJump,
}

#[cfg(target_os = "macos")]
impl From<WebMode> for IpcWebMode {
    fn from(mode: WebMode) -> Self {
        match mode {
            WebMode::Normal => Self::Normal,
            WebMode::Insert => Self::Insert,
            WebMode::Visual => Self::Visual,
            WebMode::VisualLine => Self::VisualLine,
            WebMode::Hint => Self::Hint,
            WebMode::MarkSet => Self::MarkSet,
            WebMode::MarkJump => Self::MarkJump,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IpcTabState {
    pub tab_id: IpcTabId,
    pub group_id: usize,
    pub index: usize,
    pub is_active: bool,
    pub title: String,
    pub custom_title: Option<String>,
    pub program_name: String,
    pub kind: IpcTabKind,
    pub activity: Option<IpcTabActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_mode: Option<IpcWebMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_layout: Option<IpcTerminalLayoutState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_layout: Option<IpcBrowserLayoutState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_view: Option<IpcImageViewState>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IpcTabGroup {
    pub id: usize,
    pub name: Option<String>,
    pub tabs: Vec<IpcTabState>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcTabPanelState {
    pub enabled: bool,
    pub width: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcInspectorTarget {
    pub target_id: u64,
    pub target_type: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub override_name: Option<String>,
    pub host_app_identifier: Option<String>,
    pub tab_id: Option<IpcTabId>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcInspectorSession {
    pub session_id: String,
    pub target_id: u64,
    pub tab_id: IpcTabId,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcInspectorMessage {
    pub session_id: String,
    pub payload: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcCapabilities {
    pub protocol_version: u32,
    pub platform: String,
    pub version: String,
    pub web_tabs: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct IpcRuntimeMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webview: Option<IpcWebViewMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_close: Option<IpcWebCloseMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cef_pump: Option<IpcCefPumpMetrics>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct IpcWindowDebugRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct IpcWindowDebugInsets {
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct IpcWindowDebugState {
    pub native_fullscreen: bool,
    pub simple_fullscreen: bool,
    pub winit_fullscreen: bool,
    #[serde(default)]
    pub real_ear_fullscreen_active: bool,
    pub is_miniaturized: bool,
    pub notch_ears_active: bool,
    pub scale_factor: f64,
    #[serde(default)]
    pub is_key_window: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_responder_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_view_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_number: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_ear_window_number: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_ear_window_number: Option<i64>,
    pub screen_frame_points: IpcWindowDebugRect,
    pub content_frame_screen_points: IpcWindowDebugRect,
    pub safe_area_insets_points: IpcWindowDebugInsets,
    pub auxiliary_top_left_screen_points: IpcWindowDebugRect,
    pub auxiliary_top_right_screen_points: IpcWindowDebugRect,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IpcWindowDebugSnapshot {
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
    pub snapshot_screen_points: IpcWindowDebugRect,
    pub state: IpcWindowDebugState,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcWindowDebugButton {
    Close,
    Minimize,
    Zoom,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentObservation {
    pub revision: u64,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub pending_requests: u64,
    pub elements: Vec<AgentElement>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPoint {
    pub x: i64,
    pub y: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentElement {
    pub id: String,
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub editable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentElementDetail {
    pub id: String,
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<AgentRect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<AgentPoint>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub editable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentActionReport {
    pub index: usize,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentActResult {
    pub results: Vec<AgentActionReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<AgentObservation>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentScreenshot {
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpr: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_x: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_y: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentPdf {
    pub data_base64: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentEvent {
    pub id: u64,
    pub kind: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentDownload {
    pub id: u32,
    pub state: String,
    pub url: String,
    pub suggested_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_complete: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_bytes: Option<i64>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcWebViewMetrics {
    pub live: u64,
    pub created: u64,
    pub dropped: u64,
    pub accelerated_frames: u64,
    pub frame_delivery_mode: IpcWebFrameDeliveryMode,
    pub external_begin_frames: u64,
    pub accelerated_startup_failures: u64,
    pub unexpected_cpu_paints: u64,
    pub live_accelerated_surfaces: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct IpcWebCloseMetrics {
    pub count: u64,
    pub last_ms: Option<f64>,
    pub max_ms: f64,
    pub total_ms: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcCefPumpMetrics {
    pub scheduled: u64,
    pub executed: u64,
    pub coalesced: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_requested_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_effective_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_ms_ago: Option<u64>,
    pub hidden_throttle_active: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    NotFound,
    InvalidRequest,
    Unsupported,
    Ambiguous,
    PermissionDenied,
    Timeout,
    Internal,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcError {
    pub code: IpcErrorCode,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TabSelection {
    Active,
    Next,
    Previous,
    Last,
    ByIndex { index: usize },
    ById { tab_id: IpcTabId },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UrlTarget {
    Current,
    NewTab,
    TabId { tab_id: IpcTabId },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcAction {
    Action { name: String },
    ViMotion { motion: ViMotion },
    ViAction { action: String },
    SearchAction { action: String },
    MouseAction { action: String },
    Esc { sequence: String },
    Command { program: Program },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentAction {
    Goto {
        url: String,
    },
    Click {
        id: String,
    },
    Hover {
        id: String,
    },
    HoverAt {
        x: i64,
        y: i64,
    },
    ClickAt {
        x: i64,
        y: i64,
        #[serde(default)]
        button: AgentMouseButton,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        click_count: Option<u8>,
    },
    MouseDown {
        x: i64,
        y: i64,
        #[serde(default)]
        button: AgentMouseButton,
    },
    MouseUp {
        x: i64,
        y: i64,
        #[serde(default)]
        button: AgentMouseButton,
    },
    Drag {
        from_x: i64,
        from_y: i64,
        to_x: i64,
        to_y: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steps: Option<u64>,
    },
    Fill {
        id: String,
        text: String,
    },
    Press {
        key: String,
        #[serde(default)]
        modifiers: WebKeyModifiers,
    },
    KeyDown {
        key: String,
        #[serde(default)]
        modifiers: WebKeyModifiers,
    },
    KeyUp {
        key: String,
        #[serde(default)]
        modifiers: WebKeyModifiers,
    },
    Type {
        text: String,
    },
    Paste {
        text: String,
    },
    Scroll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dx: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dy: Option<i64>,
    },
    Wheel {
        dx: i64,
        dy: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<i64>,
    },
    DialogAccept {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    DialogDismiss,
    Wait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url_contains: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        load: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    Ping,
    GetCapabilities,
    ListTabs,
    GetTabState {
        tab_id: IpcTabId,
    },
    CreateTab {
        options: WindowOptions,
        group_id: Option<usize>,
        group_name: Option<String>,
    },
    CreateGroup {
        name: Option<String>,
    },
    CloseTab {
        tab_id: Option<IpcTabId>,
    },
    SelectTab {
        selection: TabSelection,
    },
    MoveTab {
        tab_id: IpcTabId,
        target_group_id: Option<usize>,
        target_index: Option<usize>,
    },
    SetTabTitle {
        tab_id: Option<IpcTabId>,
        title: Option<String>,
    },
    SetGroupName {
        group_id: usize,
        name: Option<String>,
    },
    RestoreClosedTab,
    OpenUrl {
        url: String,
        target: UrlTarget,
    },
    SetWebUrl {
        tab_id: Option<IpcTabId>,
        url: String,
    },
    ReloadWeb {
        tab_id: Option<IpcTabId>,
    },
    OpenInspector {
        tab_id: Option<IpcTabId>,
    },
    GetTabPanel,
    SetTabPanel {
        enabled: Option<bool>,
        width: Option<usize>,
    },
    DispatchAction {
        tab_id: Option<IpcTabId>,
        action: IpcAction,
    },
    SendInput {
        tab_id: Option<IpcTabId>,
        text: String,
    },
    RunCommandBar {
        tab_id: Option<IpcTabId>,
        input: String,
    },
    ListInspectorTargets,
    AttachInspector {
        tab_id: Option<IpcTabId>,
        target_id: Option<u64>,
    },
    DetachInspector {
        session_id: String,
    },
    SendInspectorMessage {
        session_id: String,
        message: String,
    },
    PollInspectorMessages {
        session_id: String,
        max: Option<usize>,
    },
    AgentObserve {
        tab_id: Option<IpcTabId>,
    },
    AgentInspect {
        tab_id: Option<IpcTabId>,
        element_id: String,
    },
    AgentScreenshot {
        tab_id: Option<IpcTabId>,
        #[serde(default)]
        full_page: bool,
    },
    AgentEvents {
        tab_id: Option<IpcTabId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kinds: Option<Vec<String>>,
    },
    AgentPdf {
        tab_id: Option<IpcTabId>,
    },
    AgentUpload {
        tab_id: Option<IpcTabId>,
        element_id: String,
        paths: Vec<String>,
    },
    AgentDownloads {
        tab_id: Option<IpcTabId>,
    },
    AgentAct {
        tab_id: Option<IpcTabId>,
        actions: Vec<AgentAction>,
        observe: bool,
    },
    TerminalKey {
        tab_id: Option<IpcTabId>,
        input: TerminalKeyInput,
    },
    WindowDebugState,
    WindowDebugSnapshot {
        #[serde(default)]
        highlight_notch_ears: bool,
    },
    WindowDebugMouseDrag {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        #[serde(default)]
        steps: Option<usize>,
    },
    WindowDebugPressStandardButton {
        button: IpcWindowDebugButton,
    },
    RuntimeMetrics,
    SetConfig(IpcConfig),
    GetConfig(IpcGetConfig),
}

pub struct IpcRequestHelp {
    pub name: &'static str,
    pub summary: &'static str,
}

pub fn ipc_request_help() -> &'static [IpcRequestHelp] {
    &[
        IpcRequestHelp { name: "ping", summary: "Health check (pong)." },
        IpcRequestHelp { name: "get_capabilities", summary: "Protocol and platform capabilities." },
        IpcRequestHelp { name: "list_tabs", summary: "List tabs grouped by tab group." },
        IpcRequestHelp { name: "get_tab_state", summary: "Get state for a specific tab." },
        IpcRequestHelp { name: "create_tab", summary: "Create a new terminal or web tab." },
        IpcRequestHelp { name: "create_group", summary: "Create a new tab group." },
        IpcRequestHelp { name: "close_tab", summary: "Close a tab (defaults to active)." },
        IpcRequestHelp { name: "select_tab", summary: "Select a tab by position or id." },
        IpcRequestHelp { name: "move_tab", summary: "Move a tab to a group/index." },
        IpcRequestHelp { name: "set_tab_title", summary: "Set or clear a tab custom title." },
        IpcRequestHelp { name: "set_group_name", summary: "Set a tab group name." },
        IpcRequestHelp {
            name: "restore_closed_tab",
            summary: "Restore the most recently closed tab.",
        },
        IpcRequestHelp { name: "open_url", summary: "Open URL in current or new tab." },
        IpcRequestHelp { name: "set_web_url", summary: "Navigate a web tab." },
        IpcRequestHelp { name: "reload_web", summary: "Reload a web tab." },
        IpcRequestHelp { name: "open_inspector", summary: "Open Web Inspector for a web tab." },
        IpcRequestHelp { name: "get_tab_panel", summary: "Get tab panel state." },
        IpcRequestHelp { name: "set_tab_panel", summary: "Enable/disable tab panel or set width." },
        IpcRequestHelp { name: "dispatch_action", summary: "Dispatch a configured action." },
        IpcRequestHelp { name: "send_input", summary: "Send literal input text to a tab." },
        IpcRequestHelp { name: "run_command_bar", summary: "Open the command bar with input." },
        IpcRequestHelp { name: "list_inspector_targets", summary: "List Web Inspector targets." },
        IpcRequestHelp { name: "attach_inspector", summary: "Attach to a Web Inspector target." },
        IpcRequestHelp { name: "detach_inspector", summary: "Detach a Web Inspector session." },
        IpcRequestHelp { name: "send_inspector_message", summary: "Send raw Web Inspector JSON." },
        IpcRequestHelp {
            name: "poll_inspector_messages",
            summary: "Poll queued inspector messages.",
        },
        IpcRequestHelp { name: "agent_observe", summary: "Observe a web tab for agent control." },
        IpcRequestHelp { name: "agent_inspect", summary: "Inspect an observed web element." },
        IpcRequestHelp { name: "agent_screenshot", summary: "Capture a screenshot for a web tab." },
        IpcRequestHelp {
            name: "agent_events",
            summary: "Read console, network, and page agent events.",
        },
        IpcRequestHelp { name: "agent_pdf", summary: "Render a web tab as PDF." },
        IpcRequestHelp { name: "agent_upload", summary: "Upload local files to a file input." },
        IpcRequestHelp {
            name: "agent_downloads",
            summary: "List tracked downloads for a web tab.",
        },
        IpcRequestHelp { name: "agent_act", summary: "Run batched web agent actions." },
        IpcRequestHelp { name: "terminal_key", summary: "Dispatch terminal key input." },
        IpcRequestHelp {
            name: "window_debug_state",
            summary: "Read fullscreen and notch-side geometry for the active window.",
        },
        IpcRequestHelp {
            name: "window_debug_snapshot",
            summary: "Capture a PNG snapshot of the active window with debug geometry.",
        },
        IpcRequestHelp {
            name: "window_debug_mouse_drag",
            summary: "Dispatch a left-button drag through the active window input path.",
        },
        IpcRequestHelp {
            name: "window_debug_press_standard_button",
            summary: "Trigger a macOS standard window button on the active window.",
        },
        IpcRequestHelp {
            name: "runtime_metrics",
            summary: "Read runtime instrumentation metrics.",
        },
        IpcRequestHelp { name: "set_config", summary: "Apply runtime config overrides." },
        IpcRequestHelp { name: "get_config", summary: "Read runtime config." },
    ]
}

impl IpcRequest {
    pub fn target_tab_id(&self) -> Option<IpcTabId> {
        match self {
            IpcRequest::GetTabState { tab_id } => Some(*tab_id),
            IpcRequest::CloseTab { tab_id } => *tab_id,
            IpcRequest::MoveTab { tab_id, .. } => Some(*tab_id),
            IpcRequest::SetTabTitle { tab_id, .. } => *tab_id,
            IpcRequest::DispatchAction { tab_id, .. } => *tab_id,
            IpcRequest::SendInput { tab_id, .. } => *tab_id,
            IpcRequest::RunCommandBar { tab_id, .. } => *tab_id,
            IpcRequest::AttachInspector { tab_id, .. } => *tab_id,
            IpcRequest::OpenInspector { tab_id }
            | IpcRequest::ReloadWeb { tab_id }
            | IpcRequest::SetWebUrl { tab_id, .. }
            | IpcRequest::AgentObserve { tab_id }
            | IpcRequest::AgentInspect { tab_id, .. }
            | IpcRequest::AgentScreenshot { tab_id, .. }
            | IpcRequest::AgentEvents { tab_id, .. }
            | IpcRequest::AgentPdf { tab_id }
            | IpcRequest::AgentUpload { tab_id, .. }
            | IpcRequest::AgentDownloads { tab_id }
            | IpcRequest::AgentAct { tab_id, .. }
            | IpcRequest::TerminalKey { tab_id, .. } => *tab_id,
            IpcRequest::OpenUrl { target: UrlTarget::TabId { tab_id }, .. } => Some(*tab_id),
            IpcRequest::SelectTab { selection: TabSelection::ById { tab_id } } => Some(*tab_id),
            _ => None,
        }
    }

    pub fn target_inspector_session_id(&self) -> Option<&str> {
        match self {
            IpcRequest::DetachInspector { session_id }
            | IpcRequest::SendInspectorMessage { session_id, .. }
            | IpcRequest::PollInspectorMessages { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SocketReply {
    Ok,
    Pong,
    Capabilities { capabilities: IpcCapabilities },
    TabList { groups: Vec<IpcTabGroup> },
    TabState { tab: IpcTabState },
    TabCreated { tab_id: IpcTabId },
    GroupCreated { group_id: usize },
    TabPanel { panel: IpcTabPanelState },
    InspectorTargets { targets: Vec<IpcInspectorTarget> },
    InspectorAttached { session: IpcInspectorSession },
    InspectorMessages { messages: Vec<IpcInspectorMessage> },
    Config { config: serde_json::Value },
    AgentObservation { observation: AgentObservation },
    AgentElement { element: AgentElementDetail },
    AgentScreenshot { screenshot: AgentScreenshot },
    AgentEvents { last_event_id: u64, events: Vec<AgentEvent> },
    AgentPdf { pdf: AgentPdf },
    AgentDownloads { downloads: Vec<AgentDownload> },
    AgentAct { result: AgentActResult },
    WindowDebugState { state: IpcWindowDebugState },
    WindowDebugSnapshot { snapshot: IpcWindowDebugSnapshot },
    RuntimeMetrics { metrics: IpcRuntimeMetrics },
    Error { error: IpcError },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WebNetworkEntry {
    pub request_id: String,
    pub url: String,
    pub method: Option<String>,
    pub status: Option<u16>,
    pub resource_type: Option<String>,
    pub start_time: Option<f64>,
    pub end_time: Option<f64>,
    pub error_text: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebKeyState {
    Down,
    Up,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WebKeyModifiers {
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub control: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub super_key: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMouseButton {
    #[default]
    Left,
    Middle,
    Right,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TerminalKeyInput {
    pub key: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub modifiers: WebKeyModifiers,
    #[serde(default)]
    pub repeat: bool,
    pub state: WebKeyState,
}

impl IpcCapabilities {
    pub fn current() -> Self {
        Self {
            protocol_version: IPC_PROTOCOL_VERSION,
            platform: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            web_tabs: cfg!(target_os = "macos"),
        }
    }
}

impl IpcError {
    pub fn new(code: IpcErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

pub fn reply_error(code: IpcErrorCode, message: impl Into<String>) -> SocketReply {
    SocketReply::Error { error: IpcError::new(code, message) }
}

pub fn reply_ok() -> SocketReply {
    SocketReply::Ok
}

pub fn ipc_action_to_action(action: IpcAction) -> Result<Action, IpcError> {
    match action {
        IpcAction::Action { name } => parse_action_name::<Action>(&name, "action"),
        IpcAction::ViMotion { motion } => Ok(Action::ViMotion(motion)),
        IpcAction::ViAction { action } => {
            parse_action_name::<ViAction>(&action, "vi_action").map(Action::Vi)
        },
        IpcAction::SearchAction { action } => {
            parse_action_name::<SearchAction>(&action, "search_action").map(Action::Search)
        },
        IpcAction::MouseAction { action } => {
            parse_action_name::<MouseAction>(&action, "mouse_action").map(Action::Mouse)
        },
        IpcAction::Esc { sequence } => Ok(Action::Esc(sequence)),
        IpcAction::Command { program } => Ok(Action::Command(program)),
    }
}

fn normalize_action_name(name: &str) -> String {
    name.chars().filter(|ch| *ch != '_' && *ch != '-').map(|ch| ch.to_ascii_lowercase()).collect()
}

fn parse_action_name<T: DeserializeOwned>(name: &str, label: &str) -> Result<T, IpcError> {
    let normalized = normalize_action_name(name);
    let value = serde_json::Value::String(normalized);
    serde_json::from_value(value).map_err(|err| {
        IpcError::new(IpcErrorCode::InvalidRequest, format!("Invalid {label}: {err}"))
    })
}

pub struct IpcResponse {
    pub reply: SocketReply,
    pub close_window: bool,
}

pub trait IpcContext {
    fn active_tab_id(&self) -> Option<TabId>;
    fn list_tabs(&self, now: Instant) -> Vec<IpcTabGroup>;
    fn tab_state(&self, tab_id: TabId, now: Instant) -> Option<IpcTabState>;
    fn tab_kind(&self, tab_id: TabId) -> Option<IpcTabKind>;
    fn create_tab(
        &mut self,
        options: WindowOptions,
        group_id: Option<usize>,
        group_name: Option<String>,
    ) -> Result<TabId, IpcError>;
    fn create_group(&mut self, name: Option<String>) -> Result<usize, IpcError>;
    fn close_tab(&mut self, tab_id: TabId) -> Result<bool, IpcError>;
    fn select_tab(&mut self, selection: TabSelection) -> Result<(), IpcError>;
    fn move_tab(
        &mut self,
        tab_id: TabId,
        target_group_id: Option<usize>,
        target_index: Option<usize>,
    ) -> Result<(), IpcError>;
    fn set_tab_title(&mut self, tab_id: TabId, title: Option<String>) -> Result<(), IpcError>;
    fn set_group_name(&mut self, group_id: usize, name: Option<String>) -> Result<(), IpcError>;
    fn restore_closed_tab(&mut self) -> Result<(), IpcError>;
    fn open_url_in_tab(&mut self, tab_id: TabId, url: String) -> Result<(), IpcError>;
    fn open_url_new_tab(&mut self, url: String) -> Result<TabId, IpcError>;
    fn reload_web(&mut self, tab_id: TabId) -> Result<(), IpcError>;
    fn open_inspector(&mut self, tab_id: TabId) -> Result<(), IpcError>;
    fn tab_panel_state(&self) -> IpcTabPanelState;
    fn set_tab_panel(
        &mut self,
        enabled: Option<bool>,
        width: Option<usize>,
    ) -> Result<(), IpcError>;
    fn dispatch_action(&mut self, tab_id: TabId, action: Action) -> Result<(), IpcError>;
    fn send_input(&mut self, tab_id: TabId, text: String) -> Result<(), IpcError>;
    fn run_command_bar(&mut self, tab_id: TabId, input: String) -> Result<(), IpcError>;
    fn list_inspector_targets(&mut self) -> Result<Vec<IpcInspectorTarget>, IpcError>;
    fn attach_inspector(
        &mut self,
        tab_id: Option<TabId>,
        target_id: Option<u64>,
    ) -> Result<IpcInspectorSession, IpcError>;
    fn detach_inspector(&mut self, session_id: String) -> Result<(), IpcError>;
    fn send_inspector_message(
        &mut self,
        session_id: String,
        message: String,
    ) -> Result<(), IpcError>;
    fn poll_inspector_messages(
        &mut self,
        session_id: String,
        max: Option<usize>,
    ) -> Result<Vec<IpcInspectorMessage>, IpcError>;
    fn terminal_key(&mut self, tab_id: TabId, input: TerminalKeyInput) -> Result<(), IpcError>;
    fn window_debug_state(&mut self) -> Result<IpcWindowDebugState, IpcError>;
    fn window_debug_snapshot(
        &mut self,
        highlight_notch_ears: bool,
    ) -> Result<IpcWindowDebugSnapshot, IpcError>;
    fn window_debug_mouse_drag(
        &mut self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        steps: Option<usize>,
    ) -> Result<(), IpcError>;
    fn window_debug_press_standard_button(
        &mut self,
        button: IpcWindowDebugButton,
    ) -> Result<(), IpcError>;
    fn runtime_metrics(&mut self) -> Result<IpcRuntimeMetrics, IpcError>;
}

pub fn handle_request<C: IpcContext>(ctx: &mut C, request: IpcRequest) -> IpcResponse {
    let now = Instant::now();

    match request {
        IpcRequest::Ping => IpcResponse { reply: SocketReply::Pong, close_window: false },
        IpcRequest::GetCapabilities => IpcResponse {
            reply: SocketReply::Capabilities { capabilities: IpcCapabilities::current() },
            close_window: false,
        },
        IpcRequest::ListTabs => IpcResponse {
            reply: SocketReply::TabList { groups: ctx.list_tabs(now) },
            close_window: false,
        },
        IpcRequest::GetTabState { tab_id } => match ctx.tab_state(tab_id.into(), now) {
            Some(tab) => IpcResponse { reply: SocketReply::TabState { tab }, close_window: false },
            None => IpcResponse {
                reply: reply_error(IpcErrorCode::NotFound, "Tab not found"),
                close_window: false,
            },
        },
        IpcRequest::CreateTab { options, group_id, group_name } => {
            if group_id.is_some() && group_name.is_some() {
                return IpcResponse {
                    reply: reply_error(
                        IpcErrorCode::InvalidRequest,
                        "group_id and group_name are mutually exclusive",
                    ),
                    close_window: false,
                };
            }
            match ctx.create_tab(options, group_id, group_name) {
                Ok(tab_id) => IpcResponse {
                    reply: SocketReply::TabCreated { tab_id: tab_id.into() },
                    close_window: false,
                },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::CreateGroup { name } => match ctx.create_group(name) {
            Ok(group_id) => {
                IpcResponse { reply: SocketReply::GroupCreated { group_id }, close_window: false }
            },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::CloseTab { tab_id } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };

            match ctx.close_tab(tab_id) {
                Ok(close_window) => IpcResponse { reply: reply_ok(), close_window },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::SelectTab { selection } => match ctx.select_tab(selection) {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::MoveTab { tab_id, target_group_id, target_index } => {
            match ctx.move_tab(tab_id.into(), target_group_id, target_index) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::SetTabTitle { tab_id, title } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };
            match ctx.set_tab_title(tab_id, title) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::SetGroupName { group_id, name } => match ctx.set_group_name(group_id, name) {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::RestoreClosedTab => match ctx.restore_closed_tab() {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::OpenUrl { url, target } => {
            let result = match target {
                UrlTarget::NewTab => ctx.open_url_new_tab(url).map(Some),
                UrlTarget::TabId { tab_id } => {
                    ctx.open_url_in_tab(tab_id.into(), url).map(|_| None)
                },
                UrlTarget::Current => match ctx.active_tab_id() {
                    Some(tab_id) => match ctx.tab_kind(tab_id) {
                        Some(IpcTabKind::Web { .. } | IpcTabKind::Image { .. }) => {
                            ctx.open_url_in_tab(tab_id, url).map(|_| None)
                        },
                        Some(IpcTabKind::Terminal) => ctx.open_url_new_tab(url).map(Some),
                        None => Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found")),
                    },
                    None => Err(IpcError::new(IpcErrorCode::NotFound, "No active tab")),
                },
            };

            match result {
                Ok(Some(tab_id)) => IpcResponse {
                    reply: SocketReply::TabCreated { tab_id: tab_id.into() },
                    close_window: false,
                },
                Ok(None) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::SetWebUrl { tab_id, url } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };
            match ctx.open_url_in_tab(tab_id, url) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::ReloadWeb { tab_id } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };
            match ctx.reload_web(tab_id) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::OpenInspector { tab_id } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };
            match ctx.open_inspector(tab_id) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::GetTabPanel => IpcResponse {
            reply: SocketReply::TabPanel { panel: ctx.tab_panel_state() },
            close_window: false,
        },
        IpcRequest::SetTabPanel { enabled, width } => match ctx.set_tab_panel(enabled, width) {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::DispatchAction { tab_id, action } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };
            let action = match ipc_action_to_action(action) {
                Ok(action) => action,
                Err(err) => {
                    return IpcResponse {
                        reply: SocketReply::Error { error: err },
                        close_window: false,
                    };
                },
            };
            match ctx.dispatch_action(tab_id, action) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::SendInput { tab_id, text } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };
            match ctx.send_input(tab_id, text) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::RunCommandBar { tab_id, input } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };
            match ctx.run_command_bar(tab_id, input) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::ListInspectorTargets => match ctx.list_inspector_targets() {
            Ok(targets) => IpcResponse {
                reply: SocketReply::InspectorTargets { targets },
                close_window: false,
            },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::AttachInspector { tab_id, target_id } => {
            let tab_id = tab_id.map(Into::into);
            match ctx.attach_inspector(tab_id, target_id) {
                Ok(session) => IpcResponse {
                    reply: SocketReply::InspectorAttached { session },
                    close_window: false,
                },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::DetachInspector { session_id } => match ctx.detach_inspector(session_id) {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::SendInspectorMessage { session_id, message } => {
            match ctx.send_inspector_message(session_id, message) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::PollInspectorMessages { session_id, max } => {
            match ctx.poll_inspector_messages(session_id, max) {
                Ok(messages) => IpcResponse {
                    reply: SocketReply::InspectorMessages { messages },
                    close_window: false,
                },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::TerminalKey { tab_id, input } => {
            let tab_id = match tab_id.or_else(|| ctx.active_tab_id().map(IpcTabId::from)) {
                Some(tab_id) => tab_id.into(),
                None => {
                    return IpcResponse {
                        reply: reply_error(IpcErrorCode::NotFound, "No active tab"),
                        close_window: false,
                    };
                },
            };

            match ctx.terminal_key(tab_id, input) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::WindowDebugState => match ctx.window_debug_state() {
            Ok(state) => {
                IpcResponse { reply: SocketReply::WindowDebugState { state }, close_window: false }
            },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::WindowDebugSnapshot { highlight_notch_ears } => {
            match ctx.window_debug_snapshot(highlight_notch_ears) {
                Ok(snapshot) => IpcResponse {
                    reply: SocketReply::WindowDebugSnapshot { snapshot },
                    close_window: false,
                },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::WindowDebugMouseDrag { x0, y0, x1, y1, steps } => {
            match ctx.window_debug_mouse_drag(x0, y0, x1, y1, steps) {
                Ok(()) => IpcResponse { reply: SocketReply::Ok, close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::WindowDebugPressStandardButton { button } => {
            match ctx.window_debug_press_standard_button(button) {
                Ok(()) => IpcResponse { reply: SocketReply::Ok, close_window: false },
                Err(err) => {
                    IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
                },
            }
        },
        IpcRequest::RuntimeMetrics => match ctx.runtime_metrics() {
            Ok(metrics) => {
                IpcResponse { reply: SocketReply::RuntimeMetrics { metrics }, close_window: false }
            },
            Err(err) => {
                IpcResponse { reply: SocketReply::Error { error: err }, close_window: false }
            },
        },
        IpcRequest::AgentObserve { .. }
        | IpcRequest::AgentInspect { .. }
        | IpcRequest::AgentScreenshot { .. }
        | IpcRequest::AgentEvents { .. }
        | IpcRequest::AgentPdf { .. }
        | IpcRequest::AgentUpload { .. }
        | IpcRequest::AgentDownloads { .. }
        | IpcRequest::AgentAct { .. } => IpcResponse {
            reply: reply_error(
                IpcErrorCode::Unsupported,
                "Agent requests must be handled at the IPC router",
            ),
            close_window: false,
        },
        IpcRequest::SetConfig(..) | IpcRequest::GetConfig(..) => IpcResponse {
            reply: reply_error(
                IpcErrorCode::InvalidRequest,
                "Config requests must be handled at the IPC router",
            ),
            close_window: false,
        },
    }
}

/// Create an IPC socket.
pub fn spawn_ipc_socket(
    options: &Options,
    event_proxy: EventLoopProxy<Event>,
) -> IoResult<PathBuf> {
    // Create the IPC socket and export its path as env.

    let socket_path = options.socket.clone().unwrap_or_else(|| {
        let mut path = socket_dir();
        path.push(format!("{}-{}.sock", socket_prefix(), process::id()));
        path
    });

    cleanup_stale_socket(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)?;

    unsafe { env::set_var(TABOR_SOCKET_ENV, socket_path.as_os_str()) };
    if options.daemon {
        println!("TABOR_SOCKET={}; export TABOR_SOCKET", socket_path.display());
    }

    // Spawn a thread to listen on the IPC socket.
    thread::spawn_named("socket listener", move || {
        for stream in listener.incoming().filter_map(Result::ok) {
            let proxy = event_proxy.clone();
            thread::spawn_named("socket connection", move || {
                let stream = Arc::new(stream);
                let Ok(reader_stream) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(reader_stream);
                let mut data = String::new();

                loop {
                    data.clear();
                    match reader.read_line(&mut data) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => (),
                    };

                    let message: IpcRequest = match serde_json::from_str(&data) {
                        Ok(message) => message,
                        Err(err) => {
                            warn!("Failed to convert data from socket: {err}");
                            continue;
                        },
                    };

                    let event =
                        Event::new(EventType::IpcRequest(message, Arc::clone(&stream)), None);
                    let _ = proxy.send_event(event);
                }
            });
        }
    });

    Ok(socket_path)
}

fn cleanup_stale_socket(socket_path: &PathBuf) -> IoResult<()> {
    let Ok(metadata) = fs::symlink_metadata(socket_path) else {
        return Ok(());
    };

    if !metadata.file_type().is_socket() {
        return Ok(());
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => Err(IoError::new(
            ErrorKind::AddrInUse,
            format!("socket path already in use: {}", socket_path.display()),
        )),
        Err(err) if err.kind() == ErrorKind::ConnectionRefused => {
            fs::remove_file(socket_path)?;
            Ok(())
        },
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Send a message to the active Tabor socket.
pub fn send_message(socket: Option<PathBuf>, message: IpcRequest) -> IoResult<Option<SocketReply>> {
    let message_json = serde_json::to_string(&message)?;
    send_raw_message(socket, &message_json)
}

pub struct IpcConnection {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl IpcConnection {
    pub fn connect(socket: Option<PathBuf>) -> IoResult<Self> {
        let socket_path = resolve_socket_path(socket)?;
        let writer = UnixStream::connect(&socket_path)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self { writer, reader })
    }

    pub fn send_message(&mut self, message: &IpcRequest) -> IoResult<Option<SocketReply>> {
        let json = serde_json::to_string(message)?;
        self.send_raw(&json)
    }

    pub fn send_raw(&mut self, message_json: &str) -> IoResult<Option<SocketReply>> {
        self.writer.write_all(message_json.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        read_reply_line(&mut self.reader)
    }
}

/// Send raw JSON to the active Tabor socket.
pub fn send_raw_message(
    socket: Option<PathBuf>,
    message_json: &str,
) -> IoResult<Option<SocketReply>> {
    let mut connection = IpcConnection::connect(socket)?;
    connection.send_raw(message_json)
}

/// Read IPC responses.
fn read_reply_line<R: BufRead>(reader: &mut R) -> IoResult<Option<SocketReply>> {
    let mut buffer = String::new();
    if let Ok(0) | Err(_) = reader.read_line(&mut buffer) {
        return Ok(None);
    }

    let reply: SocketReply = serde_json::from_str(&buffer)
        .map_err(|err| IoError::other(format!("Invalid IPC format: {err}")))?;
    Ok(Some(reply))
}

/// Send IPC message reply.
pub fn send_reply(stream: &mut UnixStream, message: SocketReply) {
    if let Err(err) = send_reply_fallible(stream, message) {
        error!("Failed to send IPC reply: {err}");
    }
}

/// Send IPC message reply, returning possible errors.
fn send_reply_fallible(stream: &mut UnixStream, message: SocketReply) -> IoResult<()> {
    let json = serde_json::to_string(&message).map_err(IoError::other)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

/// Directory for the IPC socket file.
#[cfg(not(target_os = "macos"))]
fn socket_dir() -> PathBuf {
    xdg::BaseDirectories::with_prefix("tabor")
        .get_runtime_directory()
        .map(ToOwned::to_owned)
        .ok()
        .and_then(|path| fs::create_dir_all(&path).map(|_| path).ok())
        .unwrap_or_else(env::temp_dir)
}

/// Directory for the IPC socket file.
#[cfg(target_os = "macos")]
fn socket_dir() -> PathBuf {
    macos::runtime_tmp_dir()
}

/// Find the IPC socket path.
pub fn resolve_socket_path(socket_path: Option<PathBuf>) -> IoResult<PathBuf> {
    // Handle --socket CLI override.
    if let Some(socket_path) = socket_path {
        if socket_path.exists() {
            return Ok(socket_path);
        }
        let message = format!("invalid socket path {socket_path:?}");
        return Err(IoError::new(ErrorKind::NotFound, message));
    }

    // Handle environment variable.
    if let Ok(path) = env::var(TABOR_SOCKET_ENV) {
        let socket_path = PathBuf::from(path);
        if socket_path.exists() {
            return Ok(socket_path);
        }
    }

    // Search for sockets files.
    for entry in fs::read_dir(socket_dir())?.filter_map(|entry| entry.ok()) {
        let path = entry.path();

        // Skip files that aren't Tabor sockets.
        let socket_prefix = socket_prefix();
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|file| file.starts_with(&socket_prefix) && file.ends_with(".sock"))
            .is_none()
        {
            continue;
        }

        // Attempt to connect to the socket.
        match UnixStream::connect(&path) {
            Ok(_) => return Ok(path),
            // Delete orphan sockets.
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                let _ = fs::remove_file(&path);
            },
            // Ignore other errors like permission issues.
            Err(_) => (),
        }
    }

    Err(IoError::new(ErrorKind::NotFound, "no socket found"))
}

/// File prefix matching all available sockets.
///
/// This prefix will include display server information to allow for environments with multiple
/// display servers running for the same user.
#[cfg(not(target_os = "macos"))]
fn socket_prefix() -> String {
    let display = env::var("WAYLAND_DISPLAY").or_else(|_| env::var("DISPLAY")).unwrap_or_default();
    format!("Tabor-{}", display.replace('/', "-"))
}

/// File prefix matching all available sockets.
#[cfg(target_os = "macos")]
fn socket_prefix() -> String {
    String::from("Tabor")
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use super::*;

    #[derive(Clone)]
    struct MockTab {
        id: TabId,
        title: String,
        custom_title: Option<String>,
        program_name: String,
        kind: IpcTabKind,
        browser_layout: Option<IpcBrowserLayoutState>,
    }

    struct MockGroup {
        id: usize,
        name: Option<String>,
        tabs: Vec<TabId>,
    }

    struct MockContext {
        tabs: HashMap<TabId, MockTab>,
        groups: Vec<MockGroup>,
        active: Option<TabId>,
        next_index: u32,
        next_group_id: usize,
        tab_panel: IpcTabPanelState,
        last_action: Option<Action>,
        last_input: Option<String>,
        last_command: Option<String>,
        last_window_drag: Option<(f64, f64, f64, f64, Option<usize>)>,
        last_window_button: Option<IpcWindowDebugButton>,
        web_supported: bool,
        inspector_targets: Vec<IpcInspectorTarget>,
        inspector_sessions: HashMap<String, IpcInspectorSession>,
        inspector_messages: HashMap<String, VecDeque<String>>,
        window_debug_state: IpcWindowDebugState,
        window_debug_snapshot: IpcWindowDebugSnapshot,
        runtime_metrics: IpcRuntimeMetrics,
    }

    #[derive(Clone, Copy)]
    enum MockOpenUrlKind {
        Web,
        Image,
    }

    impl MockContext {
        fn new(web_supported: bool) -> Self {
            let mut context = Self {
                tabs: HashMap::new(),
                groups: Vec::new(),
                active: None,
                next_index: 1,
                next_group_id: 1,
                tab_panel: IpcTabPanelState { enabled: true, width: 240 },
                last_action: None,
                last_input: None,
                last_command: None,
                last_window_drag: None,
                last_window_button: None,
                web_supported,
                inspector_targets: Vec::new(),
                inspector_sessions: HashMap::new(),
                inspector_messages: HashMap::new(),
                window_debug_state: IpcWindowDebugState {
                    native_fullscreen: true,
                    simple_fullscreen: false,
                    winit_fullscreen: true,
                    real_ear_fullscreen_active: false,
                    is_miniaturized: false,
                    notch_ears_active: true,
                    scale_factor: 2.0,
                    is_key_window: true,
                    first_responder_class: Some(String::from("WinitView")),
                    content_view_class: Some(String::from("WinitView")),
                    window_number: Some(17),
                    left_ear_window_number: Some(18),
                    right_ear_window_number: Some(19),
                    screen_frame_points: IpcWindowDebugRect {
                        x: 0.0,
                        y: 0.0,
                        width: 1512.0,
                        height: 982.0,
                    },
                    content_frame_screen_points: IpcWindowDebugRect {
                        x: 0.0,
                        y: 0.0,
                        width: 1512.0,
                        height: 982.0,
                    },
                    safe_area_insets_points: IpcWindowDebugInsets {
                        top: 32.0,
                        left: 0.0,
                        bottom: 0.0,
                        right: 0.0,
                    },
                    auxiliary_top_left_screen_points: IpcWindowDebugRect {
                        x: 0.0,
                        y: 950.0,
                        width: 640.0,
                        height: 32.0,
                    },
                    auxiliary_top_right_screen_points: IpcWindowDebugRect {
                        x: 872.0,
                        y: 950.0,
                        width: 640.0,
                        height: 32.0,
                    },
                },
                window_debug_snapshot: IpcWindowDebugSnapshot {
                    png_base64: String::from("Zm9v"),
                    width: 3024,
                    height: 1964,
                    snapshot_screen_points: IpcWindowDebugRect {
                        x: 0.0,
                        y: 0.0,
                        width: 1512.0,
                        height: 982.0,
                    },
                    state: IpcWindowDebugState {
                        native_fullscreen: true,
                        simple_fullscreen: false,
                        winit_fullscreen: true,
                        real_ear_fullscreen_active: false,
                        is_miniaturized: false,
                        notch_ears_active: true,
                        scale_factor: 2.0,
                        is_key_window: true,
                        first_responder_class: Some(String::from("WinitView")),
                        content_view_class: Some(String::from("WinitView")),
                        window_number: Some(17),
                        left_ear_window_number: Some(18),
                        right_ear_window_number: Some(19),
                        screen_frame_points: IpcWindowDebugRect {
                            x: 0.0,
                            y: 0.0,
                            width: 1512.0,
                            height: 982.0,
                        },
                        content_frame_screen_points: IpcWindowDebugRect {
                            x: 0.0,
                            y: 0.0,
                            width: 1512.0,
                            height: 982.0,
                        },
                        safe_area_insets_points: IpcWindowDebugInsets {
                            top: 32.0,
                            left: 0.0,
                            bottom: 0.0,
                            right: 0.0,
                        },
                        auxiliary_top_left_screen_points: IpcWindowDebugRect {
                            x: 0.0,
                            y: 950.0,
                            width: 640.0,
                            height: 32.0,
                        },
                        auxiliary_top_right_screen_points: IpcWindowDebugRect {
                            x: 872.0,
                            y: 950.0,
                            width: 640.0,
                            height: 32.0,
                        },
                    },
                },
                runtime_metrics: IpcRuntimeMetrics {
                    webview: Some(IpcWebViewMetrics {
                        live: 1,
                        created: 1,
                        dropped: 0,
                        accelerated_frames: 12,
                        frame_delivery_mode: IpcWebFrameDeliveryMode::CefInternal,
                        external_begin_frames: 18,
                        accelerated_startup_failures: 0,
                        unexpected_cpu_paints: 0,
                        live_accelerated_surfaces: 1,
                    }),
                    web_close: Some(IpcWebCloseMetrics {
                        count: 0,
                        last_ms: None,
                        max_ms: 0.0,
                        total_ms: 0.0,
                    }),
                    cef_pump: Some(IpcCefPumpMetrics {
                        scheduled: 10,
                        executed: 9,
                        coalesced: 1,
                        last_requested_delay_ms: Some(4),
                        last_effective_delay_ms: Some(40),
                        last_run_ms_ago: Some(12),
                        hidden_throttle_active: true,
                    }),
                },
            };
            let _ = context.add_tab(IpcTabKind::Terminal, None, None);
            context
        }

        fn add_tab(
            &mut self,
            kind: IpcTabKind,
            group_id: Option<usize>,
            group_name: Option<String>,
        ) -> Result<TabId, IpcError> {
            let index = self.next_index;
            self.next_index += 1;
            let tab_id = TabId::new(index, 0);
            let title = format!("tab-{index}");
            let tab = MockTab {
                id: tab_id,
                title,
                custom_title: None,
                program_name: String::new(),
                browser_layout: match &kind {
                    IpcTabKind::Terminal => None,
                    IpcTabKind::Web { .. } => Some(IpcBrowserLayoutState {
                        mode: BrowserViewMode::MultiColumn,
                        target_width_px: 900,
                        logical_width: 900,
                        logical_height: 1200,
                        column_count: 2,
                        viewport: BrowserViewportRect { x: 0, y: 0, width: 1950, height: 600 },
                        columns: vec![
                            BrowserViewportRect { x: 0, y: 0, width: 900, height: 600 },
                            BrowserViewportRect { x: 1050, y: 0, width: 900, height: 600 },
                        ],
                        acceleration: IpcBrowserAccelerationInfo {
                            state: IpcBrowserAccelerationState::Ready,
                            frame_delivery_mode: IpcWebFrameDeliveryMode::CefInternal,
                            main_surface_width: Some(900),
                            main_surface_height: Some(1200),
                            popup_surface_width: None,
                            popup_surface_height: None,
                        },
                    }),
                    IpcTabKind::Image { .. } => None,
                },
                kind,
            };
            self.tabs.insert(tab_id, tab);

            if self.groups.is_empty() {
                let group = MockGroup { id: self.next_group_id, name: None, tabs: Vec::new() };
                self.next_group_id += 1;
                self.groups.push(group);
            }

            let group_index = if let Some(group_id) = group_id {
                self.groups
                    .iter()
                    .position(|group| group.id == group_id)
                    .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Group not found"))?
            } else if let Some(name) = group_name {
                if let Some(index) =
                    self.groups.iter().position(|group| group.name.as_deref() == Some(&name))
                {
                    index
                } else {
                    let group =
                        MockGroup { id: self.next_group_id, name: Some(name), tabs: Vec::new() };
                    self.next_group_id += 1;
                    self.groups.push(group);
                    self.groups.len() - 1
                }
            } else {
                0
            };

            self.groups[group_index].tabs.push(tab_id);
            self.active = Some(tab_id);
            Ok(tab_id)
        }

        fn group_for_tab(&self, tab_id: TabId) -> Option<(usize, usize)> {
            for group in &self.groups {
                if let Some(index) = group.tabs.iter().position(|id| *id == tab_id) {
                    return Some((group.id, index));
                }
            }
            None
        }

        fn tabs_ordered(&self) -> Vec<TabId> {
            self.groups.iter().flat_map(|group| group.tabs.iter().copied()).collect()
        }
    }

    impl IpcContext for MockContext {
        fn active_tab_id(&self) -> Option<TabId> {
            self.active
        }

        fn list_tabs(&self, _now: Instant) -> Vec<IpcTabGroup> {
            let active = self.active;
            self.groups
                .iter()
                .map(|group| {
                    let tabs = group
                        .tabs
                        .iter()
                        .enumerate()
                        .filter_map(|(index, tab_id)| {
                            let tab = self.tabs.get(tab_id)?;
                            Some(IpcTabState {
                                tab_id: tab.id.into(),
                                group_id: group.id,
                                index,
                                is_active: Some(tab.id) == active,
                                title: tab.title.clone(),
                                custom_title: tab.custom_title.clone(),
                                program_name: tab.program_name.clone(),
                                kind: tab.kind.clone(),
                                activity: None,
                                web_mode: None,
                                terminal_layout: None,
                                browser_layout: tab.browser_layout.clone(),
                                image_view: mock_image_view(&tab.kind),
                            })
                        })
                        .collect();
                    IpcTabGroup { id: group.id, name: group.name.clone(), tabs }
                })
                .collect()
        }

        fn tab_state(&self, tab_id: TabId, _now: Instant) -> Option<IpcTabState> {
            let tab = self.tabs.get(&tab_id)?;
            let (group_id, index) = self.group_for_tab(tab_id)?;
            Some(IpcTabState {
                tab_id: tab.id.into(),
                group_id,
                index,
                is_active: Some(tab.id) == self.active,
                title: tab.title.clone(),
                custom_title: tab.custom_title.clone(),
                program_name: tab.program_name.clone(),
                kind: tab.kind.clone(),
                activity: None,
                web_mode: None,
                terminal_layout: None,
                browser_layout: tab.browser_layout.clone(),
                image_view: mock_image_view(&tab.kind),
            })
        }

        fn tab_kind(&self, tab_id: TabId) -> Option<IpcTabKind> {
            self.tabs.get(&tab_id).map(|tab| tab.kind.clone())
        }

        fn create_tab(
            &mut self,
            options: WindowOptions,
            group_id: Option<usize>,
            group_name: Option<String>,
        ) -> Result<TabId, IpcError> {
            match options.window_kind {
                WindowKind::Terminal => self.add_tab(IpcTabKind::Terminal, group_id, group_name),
                WindowKind::Web { url } => {
                    if !self.web_supported {
                        return Err(IpcError::new(
                            IpcErrorCode::Unsupported,
                            "Web tabs are not supported",
                        ));
                    }
                    self.add_tab(IpcTabKind::Web { url }, group_id, group_name)
                },
                WindowKind::Image { source } => {
                    self.add_tab(IpcTabKind::Image { source }, group_id, group_name)
                },
            }
        }

        fn create_group(&mut self, name: Option<String>) -> Result<usize, IpcError> {
            let group_id = self.next_group_id;
            self.next_group_id += 1;
            self.groups.push(MockGroup { id: group_id, name, tabs: Vec::new() });
            Ok(group_id)
        }

        fn close_tab(&mut self, tab_id: TabId) -> Result<bool, IpcError> {
            if self.tabs.remove(&tab_id).is_none() {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            }
            for group in &mut self.groups {
                group.tabs.retain(|id| *id != tab_id);
            }
            self.groups.retain(|group| !group.tabs.is_empty());
            if self.active == Some(tab_id) {
                self.active = self.tabs_ordered().first().copied();
            }
            Ok(self.tabs.is_empty())
        }

        fn select_tab(&mut self, selection: TabSelection) -> Result<(), IpcError> {
            let target = match selection {
                TabSelection::Active => self.active,
                TabSelection::Next => {
                    let ordered = self.tabs_ordered();
                    let active = self
                        .active
                        .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "No active tab"))?;
                    let pos = ordered.iter().position(|id| *id == active).unwrap_or(0);
                    ordered.get((pos + 1) % ordered.len()).copied()
                },
                TabSelection::Previous => {
                    let ordered = self.tabs_ordered();
                    let active = self
                        .active
                        .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "No active tab"))?;
                    let pos = ordered.iter().position(|id| *id == active).unwrap_or(0);
                    let prev = if pos == 0 { ordered.len() - 1 } else { pos - 1 };
                    ordered.get(prev).copied()
                },
                TabSelection::Last => self.tabs_ordered().last().copied(),
                TabSelection::ByIndex { index } => self.tabs_ordered().get(index).copied(),
                TabSelection::ById { tab_id } => Some(tab_id.into()),
            };

            if let Some(tab_id) = target {
                if !self.tabs.contains_key(&tab_id) {
                    return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
                }
                self.active = Some(tab_id);
                return Ok(());
            }

            Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"))
        }

        fn move_tab(
            &mut self,
            tab_id: TabId,
            target_group_id: Option<usize>,
            target_index: Option<usize>,
        ) -> Result<(), IpcError> {
            if !self.tabs.contains_key(&tab_id) {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            }

            let mut origin_group_id = None;
            for group in &mut self.groups {
                if let Some(pos) = group.tabs.iter().position(|id| *id == tab_id) {
                    group.tabs.remove(pos);
                    origin_group_id = Some(group.id);
                    break;
                }
            }

            self.groups.retain(|group| !group.tabs.is_empty());

            let target_group_id = target_group_id.unwrap_or_else(|| {
                let id = self.next_group_id;
                self.next_group_id += 1;
                self.groups.push(MockGroup { id, name: None, tabs: Vec::new() });
                id
            });

            let group = self
                .groups
                .iter_mut()
                .find(|group| group.id == target_group_id)
                .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Group not found"))?;

            let insert_index = target_index.unwrap_or(group.tabs.len()).min(group.tabs.len());
            group.tabs.insert(insert_index, tab_id);

            if origin_group_id.is_none() {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            }

            Ok(())
        }

        fn set_tab_title(&mut self, tab_id: TabId, title: Option<String>) -> Result<(), IpcError> {
            let tab = self
                .tabs
                .get_mut(&tab_id)
                .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
            tab.custom_title = title;
            Ok(())
        }

        fn set_group_name(
            &mut self,
            group_id: usize,
            name: Option<String>,
        ) -> Result<(), IpcError> {
            let group = self
                .groups
                .iter_mut()
                .find(|group| group.id == group_id)
                .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Group not found"))?;
            group.name = name;
            Ok(())
        }

        fn restore_closed_tab(&mut self) -> Result<(), IpcError> {
            Ok(())
        }

        fn open_url_in_tab(&mut self, tab_id: TabId, url: String) -> Result<(), IpcError> {
            let tab = self
                .tabs
                .get_mut(&tab_id)
                .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
            match (&mut tab.kind, mock_open_url_kind(&url)) {
                (IpcTabKind::Web { url: tab_url }, MockOpenUrlKind::Web) => {
                    *tab_url = url;
                    Ok(())
                },
                (IpcTabKind::Image { source }, MockOpenUrlKind::Image) => {
                    *source = url;
                    Ok(())
                },
                (IpcTabKind::Terminal, _) => {
                    Err(IpcError::new(IpcErrorCode::InvalidRequest, "Not a web or image tab"))
                },
                (IpcTabKind::Web { .. }, MockOpenUrlKind::Image) => {
                    Err(IpcError::new(IpcErrorCode::InvalidRequest, "Not an image tab"))
                },
                (IpcTabKind::Image { .. }, MockOpenUrlKind::Web) => {
                    Err(IpcError::new(IpcErrorCode::InvalidRequest, "Not a web tab"))
                },
            }
        }

        fn open_url_new_tab(&mut self, url: String) -> Result<TabId, IpcError> {
            if !self.web_supported {
                return Err(IpcError::new(IpcErrorCode::Unsupported, "Web tabs are not supported"));
            }
            self.add_tab(
                match mock_open_url_kind(&url) {
                    MockOpenUrlKind::Web => IpcTabKind::Web { url },
                    MockOpenUrlKind::Image => IpcTabKind::Image { source: url },
                },
                None,
                None,
            )
        }

        fn reload_web(&mut self, tab_id: TabId) -> Result<(), IpcError> {
            let tab = self
                .tabs
                .get(&tab_id)
                .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
            match tab.kind {
                IpcTabKind::Web { .. } => Ok(()),
                IpcTabKind::Image { .. } => {
                    Err(IpcError::new(IpcErrorCode::InvalidRequest, "Not a web tab"))
                },
                IpcTabKind::Terminal => {
                    Err(IpcError::new(IpcErrorCode::InvalidRequest, "Not a web tab"))
                },
            }
        }

        fn open_inspector(&mut self, tab_id: TabId) -> Result<(), IpcError> {
            self.reload_web(tab_id)
        }

        fn tab_panel_state(&self) -> IpcTabPanelState {
            self.tab_panel.clone()
        }

        fn set_tab_panel(
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
            if let Some(enabled) = enabled {
                self.tab_panel.enabled = enabled;
            }
            if let Some(width) = width {
                self.tab_panel.width = width;
            }
            Ok(())
        }

        fn dispatch_action(&mut self, _tab_id: TabId, action: Action) -> Result<(), IpcError> {
            self.last_action = Some(action);
            Ok(())
        }

        fn send_input(&mut self, _tab_id: TabId, text: String) -> Result<(), IpcError> {
            self.last_input = Some(text);
            Ok(())
        }

        fn run_command_bar(&mut self, _tab_id: TabId, input: String) -> Result<(), IpcError> {
            self.last_command = Some(input);
            Ok(())
        }

        fn list_inspector_targets(&mut self) -> Result<Vec<IpcInspectorTarget>, IpcError> {
            Ok(self.inspector_targets.clone())
        }

        fn attach_inspector(
            &mut self,
            tab_id: Option<TabId>,
            target_id: Option<u64>,
        ) -> Result<IpcInspectorSession, IpcError> {
            let tab_id = tab_id
                .or(self.active)
                .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
            let target_id = target_id
                .or_else(|| self.inspector_targets.first().map(|target| target.target_id))
                .ok_or_else(|| {
                    IpcError::new(IpcErrorCode::NotFound, "Inspector target not found")
                })?;

            let session_id = format!("session-{}", self.inspector_sessions.len() + 1);
            let session = IpcInspectorSession {
                session_id: session_id.clone(),
                target_id,
                tab_id: tab_id.into(),
            };
            self.inspector_sessions.insert(session_id, session.clone());
            Ok(session)
        }

        fn detach_inspector(&mut self, session_id: String) -> Result<(), IpcError> {
            if self.inspector_sessions.remove(&session_id).is_none() {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Inspector session not found"));
            }
            self.inspector_messages.remove(&session_id);
            Ok(())
        }

        fn send_inspector_message(
            &mut self,
            session_id: String,
            message: String,
        ) -> Result<(), IpcError> {
            if !self.inspector_sessions.contains_key(&session_id) {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Inspector session not found"));
            }
            self.inspector_messages.entry(session_id).or_default().push_back(message);
            Ok(())
        }

        fn poll_inspector_messages(
            &mut self,
            session_id: String,
            max: Option<usize>,
        ) -> Result<Vec<IpcInspectorMessage>, IpcError> {
            let Some(messages) = self.inspector_messages.get_mut(&session_id) else {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Inspector session not found"));
            };

            let take = max.unwrap_or(messages.len());
            let mut drained = Vec::new();
            for _ in 0..take {
                let Some(payload) = messages.pop_front() else {
                    break;
                };
                drained.push(IpcInspectorMessage { session_id: session_id.clone(), payload });
            }
            Ok(drained)
        }

        fn terminal_key(
            &mut self,
            tab_id: TabId,
            _input: TerminalKeyInput,
        ) -> Result<(), IpcError> {
            if !self.tabs.contains_key(&tab_id) {
                return Err(IpcError::new(IpcErrorCode::NotFound, "Tab not found"));
            }
            Ok(())
        }

        fn window_debug_state(&mut self) -> Result<IpcWindowDebugState, IpcError> {
            Ok(self.window_debug_state.clone())
        }

        fn window_debug_snapshot(
            &mut self,
            _highlight_notch_ears: bool,
        ) -> Result<IpcWindowDebugSnapshot, IpcError> {
            Ok(self.window_debug_snapshot.clone())
        }

        fn window_debug_mouse_drag(
            &mut self,
            x0: f64,
            y0: f64,
            x1: f64,
            y1: f64,
            steps: Option<usize>,
        ) -> Result<(), IpcError> {
            self.last_window_drag = Some((x0, y0, x1, y1, steps));
            Ok(())
        }

        fn window_debug_press_standard_button(
            &mut self,
            button: IpcWindowDebugButton,
        ) -> Result<(), IpcError> {
            self.last_window_button = Some(button);
            Ok(())
        }

        fn runtime_metrics(&mut self) -> Result<IpcRuntimeMetrics, IpcError> {
            Ok(self.runtime_metrics.clone())
        }
    }

    fn mock_image_view(kind: &IpcTabKind) -> Option<IpcImageViewState> {
        let IpcTabKind::Image { source } = kind else {
            return None;
        };
        Some(IpcImageViewState {
            source: source.clone(),
            state: IpcImageLoadState::Loading,
            scale_mode: IpcImageScaleMode::Fit,
            zoom: 1.0,
            rotation_quarter_turns: 0,
            width: None,
            height: None,
            error: None,
        })
    }

    fn mock_open_url_kind(url: &str) -> MockOpenUrlKind {
        #[cfg(target_os = "macos")]
        {
            match crate::macos::image_view::classify_open_url(url) {
                crate::macos::image_view::OpenUrlKind::Web => MockOpenUrlKind::Web,
                crate::macos::image_view::OpenUrlKind::Image => MockOpenUrlKind::Image,
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = url;
            MockOpenUrlKind::Web
        }
    }

    #[test]
    fn ipc_handles_tab_lifecycle() {
        let mut ctx = MockContext::new(false);
        let initial_tab = ctx.active_tab_id().unwrap();

        let response = handle_request(
            &mut ctx,
            IpcRequest::CreateTab {
                options: WindowOptions::default(),
                group_id: None,
                group_name: None,
            },
        );
        match response.reply {
            SocketReply::TabCreated { tab_id } => {
                assert_ne!(initial_tab, tab_id.into());
            },
            _ => panic!("expected tab_created reply"),
        }

        let response = handle_request(
            &mut ctx,
            IpcRequest::SelectTab { selection: TabSelection::ByIndex { index: 0 } },
        );
        assert!(matches!(response.reply, SocketReply::Ok));

        let response = handle_request(
            &mut ctx,
            IpcRequest::SetTabTitle {
                tab_id: Some(initial_tab.into()),
                title: Some(String::from("renamed")),
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.tabs.get(&initial_tab).unwrap().custom_title.as_deref(), Some("renamed"));

        let response = handle_request(
            &mut ctx,
            IpcRequest::MoveTab {
                tab_id: initial_tab.into(),
                target_group_id: None,
                target_index: Some(0),
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.groups.len(), 2);

        let response =
            handle_request(&mut ctx, IpcRequest::CloseTab { tab_id: Some(initial_tab.into()) });
        assert!(matches!(response.reply, SocketReply::Ok));
        assert!(!response.close_window);

        let response = handle_request(&mut ctx, IpcRequest::CloseTab { tab_id: None });
        assert!(matches!(response.reply, SocketReply::Ok));
        assert!(response.close_window);
    }

    #[test]
    fn ipc_creates_group() {
        let mut ctx = MockContext::new(false);

        let response = handle_request(
            &mut ctx,
            IpcRequest::CreateGroup { name: Some(String::from("notifications")) },
        );

        let SocketReply::GroupCreated { group_id } = response.reply else {
            panic!("expected group_created reply");
        };
        assert!(ctx.groups.iter().any(|group| group.id == group_id));
    }

    #[test]
    fn ipc_handles_list_and_state() {
        let mut ctx = MockContext::new(true);
        let web_id = ctx
            .add_tab(IpcTabKind::Web { url: String::from("https://example.com") }, None, None)
            .expect("add tab");

        let response = handle_request(&mut ctx, IpcRequest::ListTabs);
        let SocketReply::TabList { groups } = response.reply else {
            panic!("expected tab_list reply");
        };
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tabs.len(), 2);

        let response = handle_request(&mut ctx, IpcRequest::GetTabState { tab_id: web_id.into() });
        let SocketReply::TabState { tab } = response.reply else {
            panic!("expected tab_state reply");
        };
        assert_eq!(tab.tab_id, web_id.into());
        let browser_layout = tab.browser_layout.expect("web tabs should report browser layout");
        assert_eq!(browser_layout.mode, BrowserViewMode::MultiColumn);
        assert_eq!(browser_layout.target_width_px, 900);
        assert_eq!(browser_layout.column_count, 2);
    }

    #[test]
    fn ipc_handles_web_and_panel_commands() {
        let mut ctx = MockContext::new(true);

        let response = handle_request(
            &mut ctx,
            IpcRequest::OpenUrl {
                url: String::from("https://example.com"),
                target: UrlTarget::NewTab,
            },
        );
        let SocketReply::TabCreated { tab_id } = response.reply else {
            panic!("expected tab_created reply");
        };

        let response = handle_request(
            &mut ctx,
            IpcRequest::SetWebUrl {
                tab_id: Some(tab_id),
                url: String::from("https://example.org"),
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));

        let response = handle_request(&mut ctx, IpcRequest::ReloadWeb { tab_id: Some(tab_id) });
        assert!(matches!(response.reply, SocketReply::Ok));

        let response = handle_request(&mut ctx, IpcRequest::OpenInspector { tab_id: Some(tab_id) });
        assert!(matches!(response.reply, SocketReply::Ok));

        let response = handle_request(
            &mut ctx,
            IpcRequest::SetTabPanel { enabled: Some(false), width: Some(200) },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert!(!ctx.tab_panel.enabled);

        let response = handle_request(&mut ctx, IpcRequest::GetTabPanel);
        let SocketReply::TabPanel { panel } = response.reply else {
            panic!("expected tab_panel reply");
        };
        assert_eq!(panel.width, 200);
    }

    #[test]
    fn ipc_handles_actions_and_input() {
        let mut ctx = MockContext::new(false);
        let tab_id = ctx.active_tab_id().unwrap();

        let response = handle_request(
            &mut ctx,
            IpcRequest::DispatchAction {
                tab_id: Some(tab_id.into()),
                action: IpcAction::Action { name: String::from("paste") },
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.last_action, Some(Action::Paste));

        let response = handle_request(
            &mut ctx,
            IpcRequest::DispatchAction {
                tab_id: Some(tab_id.into()),
                action: IpcAction::ViAction { action: String::from("toggle_normal_selection") },
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.last_action, Some(Action::Vi(ViAction::ToggleNormalSelection)));

        let response = handle_request(
            &mut ctx,
            IpcRequest::SendInput { tab_id: Some(tab_id.into()), text: String::from("ls\n") },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.last_input.as_deref(), Some("ls\n"));

        let response = handle_request(
            &mut ctx,
            IpcRequest::RunCommandBar {
                tab_id: Some(tab_id.into()),
                input: String::from(":o https://example.com"),
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.last_command.as_deref(), Some(":o https://example.com"));

        let response = handle_request(
            &mut ctx,
            IpcRequest::TerminalKey {
                tab_id: Some(tab_id.into()),
                input: TerminalKeyInput {
                    key: String::from("a"),
                    text: Some(String::from("a")),
                    modifiers: WebKeyModifiers::default(),
                    repeat: false,
                    state: WebKeyState::Down,
                },
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));

        let response = handle_request(&mut ctx, IpcRequest::RuntimeMetrics);
        let SocketReply::RuntimeMetrics { metrics } = response.reply else {
            panic!("expected runtime_metrics reply");
        };
        assert_eq!(metrics, ctx.runtime_metrics);

        let response = handle_request(&mut ctx, IpcRequest::WindowDebugState);
        let SocketReply::WindowDebugState { state } = response.reply else {
            panic!("expected window_debug_state reply");
        };
        assert_eq!(state, ctx.window_debug_state);

        let response = handle_request(
            &mut ctx,
            IpcRequest::WindowDebugSnapshot { highlight_notch_ears: true },
        );
        let SocketReply::WindowDebugSnapshot { snapshot } = response.reply else {
            panic!("expected window_debug_snapshot reply");
        };
        assert_eq!(snapshot, ctx.window_debug_snapshot);

        let response = handle_request(
            &mut ctx,
            IpcRequest::WindowDebugMouseDrag {
                x0: 10.0,
                y0: 20.0,
                x1: 110.0,
                y1: 40.0,
                steps: Some(8),
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.last_window_drag, Some((10.0, 20.0, 110.0, 40.0, Some(8))));

        let response = handle_request(
            &mut ctx,
            IpcRequest::WindowDebugPressStandardButton { button: IpcWindowDebugButton::Zoom },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.last_window_button, Some(IpcWindowDebugButton::Zoom));
    }

    #[test]
    fn ipc_handles_inspector_commands() {
        let mut ctx = MockContext::new(false);
        let tab_id = ctx.active_tab_id().unwrap();
        ctx.inspector_targets.push(IpcInspectorTarget {
            target_id: 42,
            target_type: Some(String::from("WIRTypeWebPage")),
            url: Some(String::from("https://example.com")),
            title: Some(String::from("Example")),
            override_name: None,
            host_app_identifier: Some(String::from("PID:123")),
            tab_id: Some(tab_id.into()),
        });

        let response = handle_request(&mut ctx, IpcRequest::ListInspectorTargets);
        let SocketReply::InspectorTargets { targets } = response.reply else {
            panic!("expected inspector_targets reply");
        };
        assert_eq!(targets.len(), 1);

        let response = handle_request(
            &mut ctx,
            IpcRequest::AttachInspector { tab_id: Some(tab_id.into()), target_id: Some(42) },
        );
        let SocketReply::InspectorAttached { session } = response.reply else {
            panic!("expected inspector_attached reply");
        };
        assert_eq!(session.target_id, 42);

        let response = handle_request(
            &mut ctx,
            IpcRequest::SendInspectorMessage {
                session_id: session.session_id.clone(),
                message: String::from("{\"id\":1,\"method\":\"Runtime.enable\"}"),
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));

        let response = handle_request(
            &mut ctx,
            IpcRequest::PollInspectorMessages {
                session_id: session.session_id.clone(),
                max: Some(10),
            },
        );
        let SocketReply::InspectorMessages { messages } = response.reply else {
            panic!("expected inspector_messages reply");
        };
        assert_eq!(messages.len(), 1);

        let response = handle_request(
            &mut ctx,
            IpcRequest::DetachInspector { session_id: session.session_id },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
    }
}
