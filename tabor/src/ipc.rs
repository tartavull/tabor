//! Tabor socket IPC.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Result as IoResult, Write};
use std::net::Shutdown;
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
use crate::config::{Action, MouseAction, SearchAction, ViAction};
use crate::config::ui_config::Program;
use crate::event::{Event, EventType};
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
}

impl From<&WindowKind> for IpcTabKind {
    fn from(kind: &WindowKind) -> Self {
        match kind {
            WindowKind::Terminal => Self::Terminal,
            WindowKind::Web { url } => Self::Web { url: url.clone() },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcTabActivity {
    pub has_unseen_output: bool,
    pub last_output_ms_ago: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
pub enum IpcRequest {
    Ping,
    GetCapabilities,
    ListTabs,
    GetTabState { tab_id: IpcTabId },
    CreateTab { options: WindowOptions },
    CloseTab { tab_id: Option<IpcTabId> },
    SelectTab { selection: TabSelection },
    MoveTab {
        tab_id: IpcTabId,
        target_group_id: Option<usize>,
        target_index: Option<usize>,
    },
    SetTabTitle { tab_id: Option<IpcTabId>, title: Option<String> },
    SetGroupName { group_id: usize, name: Option<String> },
    RestoreClosedTab,
    OpenUrl { url: String, target: UrlTarget },
    SetWebUrl { tab_id: Option<IpcTabId>, url: String },
    ReloadWeb { tab_id: Option<IpcTabId> },
    OpenInspector { tab_id: Option<IpcTabId> },
    GetTabPanel,
    SetTabPanel { enabled: Option<bool>, width: Option<usize> },
    DispatchAction { tab_id: Option<IpcTabId>, action: IpcAction },
    SendInput { tab_id: Option<IpcTabId>, text: String },
    RunCommandBar { tab_id: Option<IpcTabId>, input: String },
    ListInspectorTargets,
    AttachInspector { tab_id: Option<IpcTabId>, target_id: Option<u64> },
    DetachInspector { session_id: String },
    SendInspectorMessage { session_id: String, message: String },
    PollInspectorMessages { session_id: String, max: Option<usize> },
    SetConfig(IpcConfig),
    GetConfig(IpcGetConfig),
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
            | IpcRequest::SetWebUrl { tab_id, .. } => *tab_id,
            IpcRequest::OpenUrl { target, .. } => match target {
                UrlTarget::TabId { tab_id } => Some(*tab_id),
                _ => None,
            },
            IpcRequest::SelectTab { selection } => match selection {
                TabSelection::ById { tab_id } => Some(*tab_id),
                _ => None,
            },
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
    TabPanel { panel: IpcTabPanelState },
    InspectorTargets { targets: Vec<IpcInspectorTarget> },
    InspectorAttached { session: IpcInspectorSession },
    InspectorMessages { messages: Vec<IpcInspectorMessage> },
    Config { config: serde_json::Value },
    Error { error: IpcError },
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
    name.chars()
        .filter(|ch| *ch != '_' && *ch != '-')
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
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
    fn create_tab(&mut self, options: WindowOptions) -> Result<TabId, IpcError>;
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
    fn set_tab_panel(&mut self, enabled: Option<bool>, width: Option<usize>) -> Result<(), IpcError>;
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
}

pub fn handle_request<C: IpcContext>(ctx: &mut C, request: IpcRequest) -> IpcResponse {
    let now = Instant::now();

    let response = match request {
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
            Some(tab) => IpcResponse {
                reply: SocketReply::TabState { tab },
                close_window: false,
            },
            None => IpcResponse {
                reply: reply_error(IpcErrorCode::NotFound, "Tab not found"),
                close_window: false,
            },
        },
        IpcRequest::CreateTab { options } => match ctx.create_tab(options) {
            Ok(tab_id) => IpcResponse {
                reply: SocketReply::TabCreated { tab_id: tab_id.into() },
                close_window: false,
            },
            Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
            }
        },
        IpcRequest::SelectTab { selection } => match ctx.select_tab(selection) {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
        },
        IpcRequest::MoveTab {
            tab_id,
            target_group_id,
            target_index,
        } => match ctx.move_tab(tab_id.into(), target_group_id, target_index) {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
            }
        },
        IpcRequest::SetGroupName { group_id, name } => match ctx.set_group_name(group_id, name) {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
        },
        IpcRequest::RestoreClosedTab => match ctx.restore_closed_tab() {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
        },
        IpcRequest::OpenUrl { url, target } => {
            let result = match target {
                UrlTarget::NewTab => ctx.open_url_new_tab(url).map(|id| Some(id)),
                UrlTarget::TabId { tab_id } => ctx.open_url_in_tab(tab_id.into(), url).map(|_| None),
                UrlTarget::Current => match ctx.active_tab_id() {
                    Some(tab_id) => match ctx.tab_kind(tab_id) {
                        Some(IpcTabKind::Web { .. }) => ctx.open_url_in_tab(tab_id, url).map(|_| None),
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
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
            }
        },
        IpcRequest::GetTabPanel => IpcResponse {
            reply: SocketReply::TabPanel { panel: ctx.tab_panel_state() },
            close_window: false,
        },
        IpcRequest::SetTabPanel { enabled, width } => match ctx.set_tab_panel(enabled, width) {
            Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
            Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
                    return IpcResponse { reply: SocketReply::Error { error: err }, close_window: false };
                },
            };
            match ctx.dispatch_action(tab_id, action) {
                Ok(()) => IpcResponse { reply: reply_ok(), close_window: false },
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
                Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
            }
        },
        IpcRequest::ListInspectorTargets => match ctx.list_inspector_targets() {
            Ok(targets) => IpcResponse {
                reply: SocketReply::InspectorTargets { targets },
                close_window: false,
            },
            Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
            Err(err) => IpcResponse { reply: SocketReply::Error { error: err }, close_window: false },
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
        IpcRequest::SetConfig(..) | IpcRequest::GetConfig(..) => IpcResponse {
            reply: reply_error(IpcErrorCode::InvalidRequest, "Config requests must be handled at the IPC router"),
            close_window: false,
        },
    };

    response
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

    let listener = UnixListener::bind(&socket_path)?;

    unsafe { env::set_var(TABOR_SOCKET_ENV, socket_path.as_os_str()) };
    if options.daemon {
        println!("TABOR_SOCKET={}; export TABOR_SOCKET", socket_path.display());
    }

    // Spawn a thread to listen on the IPC socket.
    thread::spawn_named("socket listener", move || {
        let mut data = String::new();
        for stream in listener.incoming().filter_map(Result::ok) {
            data.clear();
            let mut reader = BufReader::new(&stream);

            match reader.read_line(&mut data) {
                Ok(0) | Err(_) => continue,
                Ok(_) => (),
            };

            // Read pending events on socket.
            let message: IpcRequest = match serde_json::from_str(&data) {
                Ok(message) => message,
                Err(err) => {
                    warn!("Failed to convert data from socket: {err}");
                    continue;
                },
            };

            let event = Event::new(EventType::IpcRequest(message, Arc::new(stream)), None);
            let _ = event_proxy.send_event(event);
        }
    });

    Ok(socket_path)
}

/// Send a message to the active Tabor socket.
pub fn send_message(socket: Option<PathBuf>, message: IpcRequest) -> IoResult<Option<SocketReply>> {
    let message_json = serde_json::to_string(&message)?;
    send_raw_message(socket, &message_json)
}

/// Send raw JSON to the active Tabor socket.
pub fn send_raw_message(socket: Option<PathBuf>, message_json: &str) -> IoResult<Option<SocketReply>> {
    let mut socket = find_socket(socket)?;

    socket.write_all(message_json.as_bytes())?;
    let _ = socket.flush();
    socket.shutdown(Shutdown::Write)?;

    read_reply(&socket)
}

/// Read IPC responses.
fn read_reply(stream: &UnixStream) -> IoResult<Option<SocketReply>> {
    let mut buffer = String::new();
    let mut reader = BufReader::new(stream);
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
    env::temp_dir()
}

/// Find the IPC socket path.
fn find_socket(socket_path: Option<PathBuf>) -> IoResult<UnixStream> {
    // Handle --socket CLI override.
    if let Some(socket_path) = socket_path {
        // Ensure we inform the user about an invalid path.
        return UnixStream::connect(&socket_path).map_err(|err| {
            let message = format!("invalid socket path {socket_path:?}");
            IoError::new(err.kind(), message)
        });
    }

    // Handle environment variable.
    if let Ok(path) = env::var(TABOR_SOCKET_ENV) {
        let socket_path = PathBuf::from(path);
        if let Ok(socket) = UnixStream::connect(socket_path) {
            return Ok(socket);
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
            Ok(socket) => return Ok(socket),
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
        web_supported: bool,
        inspector_targets: Vec<IpcInspectorTarget>,
        inspector_sessions: HashMap<String, IpcInspectorSession>,
        inspector_messages: HashMap<String, VecDeque<String>>,
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
                web_supported,
                inspector_targets: Vec::new(),
                inspector_sessions: HashMap::new(),
                inspector_messages: HashMap::new(),
            };
            context.add_tab(IpcTabKind::Terminal);
            context
        }

        fn add_tab(&mut self, kind: IpcTabKind) -> TabId {
            let index = self.next_index;
            self.next_index += 1;
            let tab_id = TabId::new(index, 0);
            let title = format!("tab-{index}");
            let tab = MockTab {
                id: tab_id,
                title,
                custom_title: None,
                program_name: String::new(),
                kind,
            };
            self.tabs.insert(tab_id, tab);

            if self.groups.is_empty() {
                let group = MockGroup { id: self.next_group_id, name: None, tabs: Vec::new() };
                self.next_group_id += 1;
                self.groups.push(group);
            }

            self.groups[0].tabs.push(tab_id);
            self.active = Some(tab_id);
            tab_id
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
            })
        }

        fn tab_kind(&self, tab_id: TabId) -> Option<IpcTabKind> {
            self.tabs.get(&tab_id).map(|tab| tab.kind.clone())
        }

        fn create_tab(&mut self, options: WindowOptions) -> Result<TabId, IpcError> {
            match options.window_kind {
                WindowKind::Terminal => Ok(self.add_tab(IpcTabKind::Terminal)),
                WindowKind::Web { url } => {
                    if !self.web_supported {
                        return Err(IpcError::new(
                            IpcErrorCode::Unsupported,
                            "Web tabs are not supported",
                        ));
                    }
                    Ok(self.add_tab(IpcTabKind::Web { url }))
                },
            }
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
                    let active = self.active.ok_or_else(|| {
                        IpcError::new(IpcErrorCode::NotFound, "No active tab")
                    })?;
                    let pos = ordered.iter().position(|id| *id == active).unwrap_or(0);
                    ordered.get((pos + 1) % ordered.len()).copied()
                },
                TabSelection::Previous => {
                    let ordered = self.tabs_ordered();
                    let active = self.active.ok_or_else(|| {
                        IpcError::new(IpcErrorCode::NotFound, "No active tab")
                    })?;
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

        fn set_group_name(&mut self, group_id: usize, name: Option<String>) -> Result<(), IpcError> {
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
            match &mut tab.kind {
                IpcTabKind::Web { url: tab_url } => {
                    *tab_url = url;
                    Ok(())
                },
                IpcTabKind::Terminal => Err(IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    "Not a web tab",
                )),
            }
        }

        fn open_url_new_tab(&mut self, url: String) -> Result<TabId, IpcError> {
            if !self.web_supported {
                return Err(IpcError::new(
                    IpcErrorCode::Unsupported,
                    "Web tabs are not supported",
                ));
            }
            Ok(self.add_tab(IpcTabKind::Web { url }))
        }

        fn reload_web(&mut self, tab_id: TabId) -> Result<(), IpcError> {
            let tab = self
                .tabs
                .get(&tab_id)
                .ok_or_else(|| IpcError::new(IpcErrorCode::NotFound, "Tab not found"))?;
            match tab.kind {
                IpcTabKind::Web { .. } => Ok(()),
                IpcTabKind::Terminal => Err(IpcError::new(
                    IpcErrorCode::InvalidRequest,
                    "Not a web tab",
                )),
            }
        }

        fn open_inspector(&mut self, tab_id: TabId) -> Result<(), IpcError> {
            self.reload_web(tab_id)
        }

        fn tab_panel_state(&self) -> IpcTabPanelState {
            self.tab_panel.clone()
        }

        fn set_tab_panel(&mut self, enabled: Option<bool>, width: Option<usize>) -> Result<(), IpcError> {
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
                return Err(IpcError::new(
                    IpcErrorCode::NotFound,
                    "Inspector session not found",
                ));
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
                return Err(IpcError::new(
                    IpcErrorCode::NotFound,
                    "Inspector session not found",
                ));
            }
            self.inspector_messages
                .entry(session_id)
                .or_default()
                .push_back(message);
            Ok(())
        }

        fn poll_inspector_messages(
            &mut self,
            session_id: String,
            max: Option<usize>,
        ) -> Result<Vec<IpcInspectorMessage>, IpcError> {
            let Some(messages) = self.inspector_messages.get_mut(&session_id) else {
                return Err(IpcError::new(
                    IpcErrorCode::NotFound,
                    "Inspector session not found",
                ));
            };

            let take = max.unwrap_or(messages.len());
            let mut drained = Vec::new();
            for _ in 0..take {
                let Some(payload) = messages.pop_front() else {
                    break;
                };
                drained.push(IpcInspectorMessage {
                    session_id: session_id.clone(),
                    payload,
                });
            }
            Ok(drained)
        }
    }

    #[test]
    fn ipc_handles_tab_lifecycle() {
        let mut ctx = MockContext::new(false);
        let initial_tab = ctx.active_tab_id().unwrap();

        let response = handle_request(&mut ctx, IpcRequest::CreateTab { options: WindowOptions::default() });
        match response.reply {
            SocketReply::TabCreated { tab_id } => {
                assert_ne!(initial_tab, tab_id.into());
            },
            _ => panic!("expected tab_created reply"),
        }

        let response = handle_request(
            &mut ctx,
            IpcRequest::SelectTab {
                selection: TabSelection::ByIndex { index: 0 },
            },
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
        assert_eq!(
            ctx.tabs.get(&initial_tab).unwrap().custom_title.as_deref(),
            Some("renamed")
        );

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

        let response = handle_request(
            &mut ctx,
            IpcRequest::CloseTab {
                tab_id: Some(initial_tab.into()),
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert!(!response.close_window);

        let response = handle_request(&mut ctx, IpcRequest::CloseTab { tab_id: None });
        assert!(matches!(response.reply, SocketReply::Ok));
        assert!(response.close_window);
    }

    #[test]
    fn ipc_handles_list_and_state() {
        let mut ctx = MockContext::new(true);
        let web_id = ctx.add_tab(IpcTabKind::Web { url: String::from("https://example.com") });

        let response = handle_request(&mut ctx, IpcRequest::ListTabs);
        let SocketReply::TabList { groups } = response.reply else {
            panic!("expected tab_list reply");
        };
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tabs.len(), 2);

        let response = handle_request(
            &mut ctx,
            IpcRequest::GetTabState {
                tab_id: web_id.into(),
            },
        );
        let SocketReply::TabState { tab } = response.reply else {
            panic!("expected tab_state reply");
        };
        assert_eq!(tab.tab_id, web_id.into());
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
            IpcRequest::SetTabPanel {
                enabled: Some(false),
                width: Some(200),
            },
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
                action: IpcAction::ViAction {
                    action: String::from("toggle_normal_selection"),
                },
            },
        );
        assert!(matches!(response.reply, SocketReply::Ok));
        assert_eq!(ctx.last_action, Some(Action::Vi(ViAction::ToggleNormalSelection)));

        let response = handle_request(
            &mut ctx,
            IpcRequest::SendInput {
                tab_id: Some(tab_id.into()),
                text: String::from("ls\n"),
            },
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
