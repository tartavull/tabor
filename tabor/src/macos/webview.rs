use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::ffi::c_void;
use std::rc::Rc as StdRc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSModalResponse, NSModalResponseAbort,
    NSTextField, NSWindow,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseButton};
use winit::keyboard::ModifiersState;

use super::cef_host::{CefHostSupervisor, RemoteViewEvent, RemoteViewInbox};
use super::cef_host_protocol::{
    HostCommand, HostEvent, HostFrameEditCommand, HostGeometry, HostJsDialogKind, HostMouseButton,
    HostMouseEvent, HostRect, HostSurfaceElement, HostSurfaceFormat, RequestId, ViewId,
    unix_deadline_after,
};
use super::cef_surface_transport::{ReceivedIoSurface, SurfaceFrame};
use super::webview_cef;
use crate::display::browser_layout::BrowserViewportLayout;
use crate::ipc::{
    AGENT_APP_OPERATION_TIMEOUT, AGENT_APP_UPLOAD_TIMEOUT, AgentDownload, IpcError, IpcErrorCode,
};

const MAX_PENDING_REQUESTS: usize = 256;
const MAX_DEVTOOLS_EVENTS: usize = 2048;
const MAX_DEVTOOLS_EVENT_BYTES: usize = 256 * 1024;
const MAX_DEVTOOLS_EVENTS_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEFERRED_COMMANDS: usize = 4096;

pub struct WebView {
    view_id: ViewId,
    supervisor: Arc<CefHostSupervisor>,
    inbox: Arc<RemoteViewInbox>,
    state: RefCell<RemoteWebViewState>,
    layout: BrowserViewportLayout,
    mouse_button_flags: u32,
    last_mouse_event: Option<HostMouseEvent>,
    dialog_state: StdRc<RefCell<RemoteJsDialogState>>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAccelerationState {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebFrameDeliveryMode {
    CefInternal,
    #[default]
    CefHostIpc,
}

#[derive(Debug, Copy, Clone)]
pub struct WebSurfaceRef {
    pub io_surface: *mut c_void,
    pub width: usize,
    pub height: usize,
    pub format: cef::ColorType,
}

#[derive(Debug, Copy, Clone)]
pub struct WebPopupSurfaceRef {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub surface: WebSurfaceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebAccelerationInfo {
    pub state: WebAccelerationState,
    pub frame_delivery_mode: WebFrameDeliveryMode,
    pub failure_reason: Option<String>,
    pub main_surface_width: Option<usize>,
    pub main_surface_height: Option<usize>,
    pub popup_surface_width: Option<usize>,
    pub popup_surface_height: Option<usize>,
}

struct RemoteSurface {
    io_surface: ReceivedIoSurface,
    width: usize,
    height: usize,
    format: cef::ColorType,
}

impl RemoteSurface {
    fn new(
        io_surface: ReceivedIoSurface,
        width: usize,
        height: usize,
        format: HostSurfaceFormat,
    ) -> Self {
        let format = match format {
            HostSurfaceFormat::Bgra8888 => cef::ColorType::BGRA_8888,
            HostSurfaceFormat::Rgba8888 => cef::ColorType::RGBA_8888,
        };
        super::register_accelerated_surface();
        Self { io_surface, width, height, format }
    }

    fn as_public_ref(&self) -> WebSurfaceRef {
        WebSurfaceRef {
            io_surface: self.io_surface.as_ptr(),
            width: self.width,
            height: self.height,
            format: self.format,
        }
    }
}

impl Drop for RemoteSurface {
    fn drop(&mut self) {
        super::unregister_accelerated_surface();
    }
}

struct RemotePopupSurface {
    rect: HostRect,
    surface: RemoteSurface,
}

struct RemoteDevToolsEvent {
    id: u64,
    payload: String,
}

enum PendingCompletion {
    Evaluate(Box<dyn FnOnce(Result<Option<String>, IpcError>)>),
    DevTools(Box<dyn FnOnce(Result<JsonValue, String>)>),
    FileInput(Box<dyn FnOnce(Result<String, IpcError>)>),
}

#[derive(Clone, Copy)]
enum EvaluationRuntime {
    Raw,
    Agent,
}

#[derive(Clone, Copy)]
struct PendingRequestToken {
    request_id: RequestId,
    expires_at_unix_millis: u64,
}

struct PendingRequest {
    deadline: Instant,
    completion: PendingCompletion,
}

struct RemoteWebViewState {
    ready: bool,
    generation: u64,
    host_pid: Option<u32>,
    crashes: u64,
    acceleration_state: WebAccelerationState,
    acceleration_failure: Option<String>,
    main_surface: Option<RemoteSurface>,
    popup_surface: Option<RemotePopupSurface>,
    title: Option<String>,
    last_title: Option<String>,
    url: Option<String>,
    last_url: Option<String>,
    downloads: Vec<AgentDownload>,
    devtools_events: VecDeque<RemoteDevToolsEvent>,
    devtools_event_bytes: usize,
    next_devtools_event_id: u64,
    pending: HashMap<RequestId, PendingRequest>,
    deferred: VecDeque<HostCommand>,
    agent_event_capture_requested: bool,
    inspector_sessions: usize,
}

impl RemoteWebViewState {
    fn new(url: &str) -> Self {
        Self {
            ready: false,
            generation: 0,
            host_pid: None,
            crashes: 0,
            acceleration_state: WebAccelerationState::Pending,
            acceleration_failure: None,
            main_surface: None,
            popup_surface: None,
            title: None,
            last_title: None,
            url: Some(if url.is_empty() { String::from("about:blank") } else { url.to_string() }),
            last_url: None,
            downloads: Vec::new(),
            devtools_events: VecDeque::new(),
            devtools_event_bytes: 0,
            next_devtools_event_id: 1,
            pending: HashMap::new(),
            deferred: VecDeque::new(),
            agent_event_capture_requested: false,
            inspector_sessions: 0,
        }
    }

    fn push_devtools_event(&mut self, mut payload: String) {
        if payload.len() > MAX_DEVTOOLS_EVENT_BYTES {
            payload = serde_json::json!({
                "method": "Tabor.eventTruncated",
                "params": { "originalBytes": payload.len() },
            })
            .to_string();
        }
        let id = self.next_devtools_event_id;
        self.next_devtools_event_id = self.next_devtools_event_id.saturating_add(1);
        self.devtools_event_bytes = self.devtools_event_bytes.saturating_add(payload.len());
        self.devtools_events.push_back(RemoteDevToolsEvent { id, payload });
        while self.devtools_events.len() > MAX_DEVTOOLS_EVENTS
            || self.devtools_event_bytes > MAX_DEVTOOLS_EVENTS_BYTES
        {
            let event = self.devtools_events.pop_front().expect("event queue is not empty");
            self.devtools_event_bytes =
                self.devtools_event_bytes.saturating_sub(event.payload.len());
        }
    }
}

enum RequestCompletion {
    Evaluate(Box<dyn FnOnce(Result<Option<String>, IpcError>)>, Result<Option<String>, IpcError>),
    DevTools(Box<dyn FnOnce(Result<JsonValue, String>)>, Result<JsonValue, String>),
    FileInput(Box<dyn FnOnce(Result<String, IpcError>)>, Result<String, IpcError>),
}

fn command_request_id(command: &HostCommand) -> Option<RequestId> {
    command.request_id()
}

fn fail_pending_request(request: PendingRequest, error: &str) {
    match request.completion {
        PendingCompletion::Evaluate(callback) => {
            callback(Err(IpcError::new(IpcErrorCode::Internal, error)))
        },
        PendingCompletion::DevTools(callback) => callback(Err(error.to_string())),
        PendingCompletion::FileInput(callback) => {
            callback(Err(IpcError::new(IpcErrorCode::Internal, error)))
        },
    }
}

fn take_failed_request_completions(
    state: &mut RemoteWebViewState,
    error: &str,
) -> Vec<RequestCompletion> {
    state.deferred.retain(|command| command_request_id(command).is_none());
    state
        .pending
        .drain()
        .map(|(_, pending)| match pending.completion {
            PendingCompletion::Evaluate(callback) => RequestCompletion::Evaluate(
                callback,
                Err(IpcError::new(IpcErrorCode::Internal, error)),
            ),
            PendingCompletion::DevTools(callback) => {
                RequestCompletion::DevTools(callback, Err(error.to_string()))
            },
            PendingCompletion::FileInput(callback) => RequestCompletion::FileInput(
                callback,
                Err(IpcError::new(IpcErrorCode::Internal, error)),
            ),
        })
        .collect()
}

fn take_expired_request_completions(
    state: &mut RemoteWebViewState,
    now: Instant,
) -> Vec<RequestCompletion> {
    let expired = state
        .pending
        .iter()
        .filter_map(|(request_id, pending)| (pending.deadline <= now).then_some(*request_id))
        .collect::<Vec<_>>();
    state.deferred.retain(|command| match command_request_id(command) {
        Some(request_id) => !expired.contains(&request_id),
        None => true,
    });
    expired
        .into_iter()
        .filter_map(|request_id| state.pending.remove(&request_id))
        .map(|pending| match pending.completion {
            PendingCompletion::Evaluate(callback) => RequestCompletion::Evaluate(
                callback,
                Err(IpcError::new(IpcErrorCode::Timeout, "CEF host request timed out")),
            ),
            PendingCompletion::DevTools(callback) => RequestCompletion::DevTools(
                callback,
                Err(String::from("CEF host request timed out")),
            ),
            PendingCompletion::FileInput(callback) => RequestCompletion::FileInput(
                callback,
                Err(IpcError::new(IpcErrorCode::Timeout, "CEF host request timed out")),
            ),
        })
        .collect()
}

fn finish_request_completions(completions: Vec<RequestCompletion>) {
    for completion in completions {
        match completion {
            RequestCompletion::Evaluate(callback, result) => callback(result),
            RequestCompletion::DevTools(callback, result) => callback(result),
            RequestCompletion::FileInput(callback, result) => callback(result),
        }
    }
}

struct PendingRemoteDialog {
    dialog_id: u64,
    alert: Retained<NSAlert>,
    prompt_field: Option<Retained<NSTextField>>,
    _completion: RcBlock<dyn Fn(NSModalResponse)>,
}

struct RemoteJsDialogState {
    parent_window: Retained<NSWindow>,
    active: Option<PendingRemoteDialog>,
    supervisor: Arc<CefHostSupervisor>,
    view_id: ViewId,
}

impl WebView {
    pub fn new(
        window: &crate::display::window::Window,
        _size_info: &crate::display::SizeInfo,
        layout: BrowserViewportLayout,
        tab_id: crate::tabs::TabId,
        url: &str,
        proxy: &winit::event_loop::EventLoopProxy<crate::event::Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let _mtm = MainThreadMarker::new()
            .ok_or_else(|| std::io::Error::other("WebView must be created on main thread"))?;
        let supervisor = CefHostSupervisor::shared()?;
        let view_id = supervisor.allocate_view_id();
        let inbox = RemoteViewInbox::new(proxy.clone(), window.id(), tab_id);
        let geometry = host_geometry(window, &layout);
        let parent_window = webview_cef::ns_window(webview_cef::ns_view(window)?)?;
        supervisor
            .register(view_id, normalized_url(url), geometry, &inbox)
            .map_err(std::io::Error::other)?;
        super::register_webview();

        Ok(Self {
            view_id,
            supervisor: supervisor.clone(),
            inbox,
            state: RefCell::new(RemoteWebViewState::new(url)),
            layout,
            mouse_button_flags: 0,
            last_mouse_event: None,
            dialog_state: StdRc::new(RefCell::new(RemoteJsDialogState {
                parent_window,
                active: None,
                supervisor,
                view_id,
            })),
        })
    }

    pub(crate) fn host_view_id(&self) -> ViewId {
        self.view_id
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.send_state(HostCommand::SetVisible { view_id: self.view_id, visible });
    }

    pub fn set_focus(&mut self, focus: bool) {
        self.send_state(HostCommand::SetFocus { view_id: self.view_id, focused: focus });
    }

    pub fn sync_editable_focus(&mut self, editable: bool) {
        self.send_state(HostCommand::SyncEditableFocus { view_id: self.view_id, editable });
    }

    pub fn restore_native_focus(&mut self, _window: &crate::display::window::Window) -> bool {
        // The AppKit content view remains first responder in Tabor. Key and IME events are
        // explicitly forwarded to the isolated browser host.
        false
    }

    pub fn update_frame(
        &mut self,
        window: &crate::display::window::Window,
        _size_info: &crate::display::SizeInfo,
        layout: BrowserViewportLayout,
    ) {
        self.layout = layout;
        let geometry = host_geometry(window, &self.layout);
        self.send_state(HostCommand::UpdateGeometry { view_id: self.view_id, geometry });
    }

    pub fn acceleration_info(&self) -> WebAccelerationInfo {
        self.drain_events();
        let state = self.state.borrow();
        WebAccelerationInfo {
            state: state.acceleration_state,
            frame_delivery_mode: WebFrameDeliveryMode::CefHostIpc,
            failure_reason: state.acceleration_failure.clone(),
            main_surface_width: state.main_surface.as_ref().map(|surface| surface.width),
            main_surface_height: state.main_surface.as_ref().map(|surface| surface.height),
            popup_surface_width: state.popup_surface.as_ref().map(|popup| popup.surface.width),
            popup_surface_height: state.popup_surface.as_ref().map(|popup| popup.surface.height),
        }
    }

    pub(crate) fn process_inbox_events(&self) {
        self.drain_events();
    }

    pub fn load_url(&mut self, url: &str) -> bool {
        let url = normalized_url(url);
        {
            let mut state = self.state.borrow_mut();
            state.url = Some(url.clone());
            state.title = None;
            state.last_title = None;
            state.last_url = None;
        }
        self.send_state(HostCommand::LoadUrl { view_id: self.view_id, url })
    }

    pub fn reload(&mut self) {
        self.send_or_defer(HostCommand::Reload { view_id: self.view_id });
    }

    pub fn go_back(&mut self) {
        self.send_or_defer(HostCommand::GoBack { view_id: self.view_id });
    }

    pub fn go_forward(&mut self) {
        self.send_or_defer(HostCommand::GoForward { view_id: self.view_id });
    }

    pub fn handle_key_input(
        &mut self,
        _window: &crate::display::window::Window,
        key: &KeyEvent,
        text: &str,
        modifiers: ModifiersState,
    ) -> bool {
        let (events, forwarded) = webview_cef::encode_host_key_input(key, text, modifiers);
        if events.is_empty() {
            return forwarded;
        }
        self.send_or_defer(HostCommand::KeyEvents {
            view_id: self.view_id,
            events,
            invalidate_after: matches!(key.state, ElementState::Pressed),
        });
        forwarded
    }

    pub fn handle_mouse_input(
        &mut self,
        window: &crate::display::window::Window,
        position: PhysicalPosition<f64>,
        state: ElementState,
        button: MouseButton,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        let button_flag = webview_cef::cef_mouse_button_flag(button);
        let event_button_flags = match state {
            ElementState::Pressed => self.mouse_button_flags | button_flag,
            ElementState::Released => self.mouse_button_flags & !button_flag,
        };
        let Some(event) = self.mouse_event(window, position, modifiers, event_button_flags) else {
            return false;
        };
        self.mouse_button_flags = event_button_flags;
        self.last_mouse_event = Some(event);
        let button = match button {
            MouseButton::Left => HostMouseButton::Left,
            MouseButton::Right => HostMouseButton::Right,
            MouseButton::Middle
            | MouseButton::Back
            | MouseButton::Forward
            | MouseButton::Other(_) => HostMouseButton::Middle,
        };
        self.send_or_defer(HostCommand::MouseClick {
            view_id: self.view_id,
            event,
            button,
            mouse_up: matches!(state, ElementState::Released),
            click_count: 1,
        });
        true
    }

    pub fn handle_mouse_move(
        &mut self,
        window: &crate::display::window::Window,
        position: PhysicalPosition<f64>,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        let Some(event) = self.mouse_event(window, position, modifiers, self.mouse_button_flags)
        else {
            self.handle_mouse_leave();
            return false;
        };
        self.last_mouse_event = Some(event);
        self.send_or_defer(HostCommand::MouseMove {
            view_id: self.view_id,
            event,
            mouse_leave: false,
        });
        true
    }

    pub fn handle_mouse_leave(&mut self) {
        let event = self.last_mouse_event.unwrap_or_default();
        self.send_or_defer(HostCommand::MouseMove {
            view_id: self.view_id,
            event,
            mouse_leave: true,
        });
    }

    pub fn handle_mouse_wheel(
        &mut self,
        window: &crate::display::window::Window,
        position: PhysicalPosition<f64>,
        delta_x: f64,
        delta_y: f64,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        let Some(event) = self.mouse_event(window, position, modifiers, self.mouse_button_flags)
        else {
            return false;
        };
        self.last_mouse_event = Some(event);
        self.send_or_defer(HostCommand::MouseWheel {
            view_id: self.view_id,
            event,
            delta_x: delta_x.round() as i32,
            delta_y: webview_cef::scaled_browser_wheel_delta_y(&self.layout, delta_y).round()
                as i32,
        });
        true
    }

    pub fn handle_ime_commit(&mut self, text: &str) {
        self.send_or_defer(HostCommand::ImeCommit {
            view_id: self.view_id,
            text: text.to_string(),
        });
    }

    pub fn handle_ime_preedit(&mut self, text: &str, cursor_offset: Option<(usize, usize)>) {
        self.send_or_defer(HostCommand::ImePreedit {
            view_id: self.view_id,
            text: text.to_string(),
            cursor_offset,
        });
    }

    pub fn cancel_ime_composition(&mut self) {
        self.send_or_defer(HostCommand::ImeCancel { view_id: self.view_id });
    }

    pub fn exec_js(&mut self, script: &str) {
        self.eval_js_string(script, |_| {});
    }

    pub fn copy_selection(&mut self) {
        self.frame_edit(HostFrameEditCommand::Copy);
    }

    pub fn cut_selection(&mut self) {
        self.frame_edit(HostFrameEditCommand::Cut);
    }

    pub fn paste(&mut self) {
        self.frame_edit(HostFrameEditCommand::Paste);
    }

    pub fn eval_js_string<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        self.evaluate(script, false, move |result| callback(result.ok().flatten()));
    }

    pub fn agent_eval_js_string_result<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        self.renew_agent_event_capture();
        self.agent_evaluate_with_timeout(script, false, AGENT_APP_OPERATION_TIMEOUT, callback);
    }

    pub fn agent_eval_js_string_with_user_gesture_and_timeout<F>(
        &mut self,
        script: &str,
        timeout: Duration,
        callback: F,
    ) where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        self.renew_agent_event_capture();
        self.agent_evaluate_with_timeout(script, true, timeout, callback);
    }

    pub fn devtools_command_json<F>(
        &mut self,
        method: &str,
        params: Option<JsonValue>,
        callback: F,
    ) -> Result<(), String>
    where
        F: FnOnce(Result<JsonValue, String>) + 'static,
    {
        self.drain_events();
        let request = self
            .insert_pending(
                PendingCompletion::DevTools(Box::new(callback)),
                AGENT_APP_OPERATION_TIMEOUT,
            )
            .map_err(|_| String::from("Too many pending CEF host requests"))?;
        self.send_or_defer(HostCommand::DevTools {
            view_id: self.view_id,
            request_id: request.request_id,
            method: method.to_string(),
            params,
            expires_at_unix_millis: request.expires_at_unix_millis,
        });
        Ok(())
    }

    pub fn devtools_events_since(&self, last_id: u64, max: usize) -> (Vec<String>, u64) {
        self.drain_events();
        if max == 0 {
            return (Vec::new(), last_id);
        }
        let state = self.state.borrow();
        let mut events = Vec::new();
        let mut newest = last_id;
        for event in &state.devtools_events {
            if event.id <= last_id {
                continue;
            }
            let payload = serde_json::from_str::<JsonValue>(&event.payload)
                .ok()
                .and_then(|value| match value {
                    JsonValue::Object(mut object) => {
                        object.insert(String::from("id"), JsonValue::from(event.id));
                        Some(JsonValue::Object(object).to_string())
                    },
                    _ => None,
                })
                .unwrap_or_else(|| {
                    serde_json::json!({ "id": event.id, "payload": event.payload }).to_string()
                });
            events.push(payload);
            newest = event.id;
            if events.len() >= max {
                break;
            }
        }
        (events, newest)
    }

    pub fn latest_devtools_event_id(&self) -> u64 {
        self.drain_events();
        self.state.borrow().next_devtools_event_id.saturating_sub(1)
    }

    pub fn renew_agent_event_capture(&self) {
        self.state.borrow_mut().agent_event_capture_requested = true;
        self.send_or_defer_shared(HostCommand::RenewAgentEventCapture { view_id: self.view_id });
    }

    pub fn retain_inspector_session(&self) {
        self.state.borrow_mut().inspector_sessions += 1;
        self.send_or_defer_shared(HostCommand::RetainInspectorSession { view_id: self.view_id });
    }

    pub fn release_inspector_session(&self) {
        let mut state = self.state.borrow_mut();
        assert!(state.inspector_sessions > 0, "unbalanced inspector session release");
        state.inspector_sessions -= 1;
        drop(state);
        self.send_or_defer_shared(HostCommand::ReleaseInspectorSession { view_id: self.view_id });
    }

    pub fn set_file_input_files<F>(&self, element_id: &str, paths: Vec<String>, callback: F)
    where
        F: FnOnce(Result<String, IpcError>) + 'static,
    {
        self.drain_events();
        self.renew_agent_event_capture();
        let pending = PendingCompletion::FileInput(Box::new(callback));
        let request = match self.insert_pending(pending, AGENT_APP_UPLOAD_TIMEOUT) {
            Ok(request) => request,
            Err(PendingCompletion::FileInput(callback)) => {
                callback(Err(IpcError::new(
                    IpcErrorCode::Internal,
                    "Unable to queue CEF host file input request",
                )));
                return;
            },
            Err(_) => unreachable!("file input inserted a non-file-input request"),
        };
        self.send_or_defer_shared(HostCommand::SetFileInputFiles {
            view_id: self.view_id,
            request_id: request.request_id,
            element_id: element_id.to_string(),
            paths,
            expires_at_unix_millis: request.expires_at_unix_millis,
        });
    }

    pub fn downloads(&self) -> Vec<AgentDownload> {
        self.drain_events();
        self.state.borrow().downloads.clone()
    }

    pub fn poll_title(&mut self) -> Option<String> {
        self.drain_events();
        let mut state = self.state.borrow_mut();
        let title = state.title.clone()?;
        if state.last_title.as_deref() == Some(&title) {
            return None;
        }
        state.last_title = Some(title.clone());
        Some(title)
    }

    pub fn poll_url(&mut self) -> Option<String> {
        self.drain_events();
        let mut state = self.state.borrow_mut();
        let url = state.url.clone()?;
        if state.last_url.as_deref() == Some(&url) {
            return None;
        }
        state.last_url = Some(url.clone());
        Some(url)
    }

    pub fn current_url(&self) -> Option<String> {
        self.drain_events();
        self.state.borrow().url.clone()
    }

    pub fn show_inspector(&mut self) -> bool {
        self.send_or_defer(HostCommand::ShowInspector { view_id: self.view_id });
        true
    }

    pub fn with_surfaces<R>(
        &self,
        func: impl FnOnce(Option<WebSurfaceRef>, Option<WebPopupSurfaceRef>) -> R,
    ) -> R {
        self.drain_events();
        let state = self.state.borrow();
        let main = state.main_surface.as_ref().map(RemoteSurface::as_public_ref);
        let popup = state.popup_surface.as_ref().map(|popup| WebPopupSurfaceRef {
            x: popup.rect.x.max(0) as usize,
            y: popup.rect.y.max(0) as usize,
            width: popup.rect.width.max(0) as usize,
            height: popup.rect.height.max(0) as usize,
            surface: popup.surface.as_public_ref(),
        });
        func(main, popup)
    }

    fn frame_edit(&self, command: HostFrameEditCommand) {
        self.send_or_defer_shared(HostCommand::FrameEdit { view_id: self.view_id, command });
    }

    fn evaluate<F>(&self, script: &str, user_gesture: bool, callback: F)
    where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        self.evaluate_with_timeout(script, user_gesture, AGENT_APP_OPERATION_TIMEOUT, callback);
    }

    fn evaluate_with_timeout<F>(
        &self,
        script: &str,
        user_gesture: bool,
        timeout: Duration,
        callback: F,
    ) where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        self.evaluate_with_runtime(EvaluationRuntime::Raw, script, user_gesture, timeout, callback);
    }

    fn agent_evaluate_with_timeout<F>(
        &self,
        script: &str,
        user_gesture: bool,
        timeout: Duration,
        callback: F,
    ) where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        self.evaluate_with_runtime(
            EvaluationRuntime::Agent,
            script,
            user_gesture,
            timeout,
            callback,
        );
    }

    fn evaluate_with_runtime<F>(
        &self,
        runtime: EvaluationRuntime,
        script: &str,
        user_gesture: bool,
        timeout: Duration,
        callback: F,
    ) where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        self.drain_events();
        let pending = PendingCompletion::Evaluate(Box::new(callback));
        let request = match self.insert_pending(pending, timeout) {
            Ok(request) => request,
            Err(PendingCompletion::Evaluate(callback)) => {
                callback(Err(IpcError::new(
                    IpcErrorCode::Internal,
                    "Unable to queue CEF host evaluation",
                )));
                return;
            },
            Err(_) => unreachable!("evaluate inserted a non-evaluate request"),
        };
        let command = match runtime {
            EvaluationRuntime::Raw => HostCommand::Evaluate {
                view_id: self.view_id,
                request_id: request.request_id,
                script: script.to_string(),
                user_gesture,
                expires_at_unix_millis: request.expires_at_unix_millis,
            },
            EvaluationRuntime::Agent => HostCommand::AgentEvaluate {
                view_id: self.view_id,
                request_id: request.request_id,
                script: script.to_string(),
                user_gesture,
                expires_at_unix_millis: request.expires_at_unix_millis,
            },
        };
        self.send_or_defer_shared(command);
    }

    fn insert_pending(
        &self,
        completion: PendingCompletion,
        timeout: Duration,
    ) -> Result<PendingRequestToken, PendingCompletion> {
        let Ok(expires_at_unix_millis) = unix_deadline_after(timeout) else {
            return Err(completion);
        };
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return Err(completion);
        };
        let mut state = self.state.borrow_mut();
        if state.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(completion);
        }
        let request_id = self.supervisor.allocate_request_id();
        state.pending.insert(request_id, PendingRequest { deadline, completion });
        drop(state);
        self.inbox.notify_request_deadline_changed();
        Ok(PendingRequestToken { request_id, expires_at_unix_millis })
    }

    fn send_state(&self, command: HostCommand) -> bool {
        self.supervisor.send(command)
    }

    fn send_or_defer(&self, command: HostCommand) {
        self.send_or_defer_shared(command);
    }

    fn send_or_defer_shared(&self, command: HostCommand) {
        self.drain_events();
        let ready = self.state.borrow().ready;
        if ready {
            self.send_live_command(command);
            return;
        }
        let completion = {
            let mut state = self.state.borrow_mut();
            if command_request_id(&command)
                .is_some_and(|request_id| !state.pending.contains_key(&request_id))
            {
                return;
            }
            let completion = if state.deferred.len() >= MAX_DEFERRED_COMMANDS {
                state
                    .deferred
                    .pop_front()
                    .and_then(|command| command_request_id(&command))
                    .and_then(|request_id| state.pending.remove(&request_id))
            } else {
                None
            };
            state.deferred.push_back(command);
            completion
        };
        if let Some(completion) = completion {
            fail_pending_request(completion, "CEF host command queue is full");
        }
    }

    fn send_live_command(&self, command: HostCommand) {
        let request_id = command_request_id(&command);
        if request_id
            .is_some_and(|request_id| !self.state.borrow().pending.contains_key(&request_id))
        {
            return;
        }
        if self.supervisor.send(command) {
            return;
        }
        let completion =
            request_id.and_then(|request_id| self.state.borrow_mut().pending.remove(&request_id));
        if let Some(completion) = completion {
            fail_pending_request(completion, "CEF host supervisor stopped");
        }
    }

    fn mouse_event(
        &self,
        window: &crate::display::window::Window,
        position: PhysicalPosition<f64>,
        modifiers: objc2_app_kit::NSEventModifierFlags,
        button_flags: u32,
    ) -> Option<HostMouseEvent> {
        let scale_factor = window.scale_factor.max(f64::MIN_POSITIVE);
        let x = (position.x / scale_factor).floor().max(0.0) as usize;
        let y = (position.y / scale_factor).floor().max(0.0) as usize;
        let (x, y) = self.layout.logical_point_for_visual(x, y)?;
        Some(HostMouseEvent {
            x: x as i32,
            y: y as i32,
            modifiers: webview_cef::cef_mouse_event_flags(modifiers, button_flags),
        })
    }

    fn drain_events(&self) {
        let events = self.inbox.drain();
        if events.is_empty() {
            return;
        }
        let mut completions = Vec::new();
        let mut dialogs = Vec::new();
        let mut closed_dialogs = Vec::new();
        let mut acquired_surfaces = Vec::new();
        let mut commands_after_ready = Vec::new();

        {
            let mut state = self.state.borrow_mut();
            for event in events {
                match event {
                    RemoteViewEvent::Connected { pid, generation } => {
                        state.host_pid = Some(pid);
                        state.generation = generation;
                        state.ready = false;
                        state.acceleration_state = WebAccelerationState::Pending;
                        state.acceleration_failure = None;
                    },
                    RemoteViewEvent::Unavailable { error, crashes } => {
                        state.ready = false;
                        state.host_pid = None;
                        state.crashes = crashes;
                        state.acceleration_state = WebAccelerationState::Failed;
                        state.acceleration_failure = Some(error.clone());
                        state.main_surface = None;
                        state.popup_surface = None;
                        completions.extend(take_failed_request_completions(&mut state, &error));
                        closed_dialogs.push(None);
                    },
                    RemoteViewEvent::Frame(SurfaceFrame {
                        lease_id,
                        element,
                        surface,
                        width,
                        height,
                        format,
                        popup_rect,
                        ..
                    }) => {
                        let surface = RemoteSurface::new(surface, width, height, format);
                        state.acceleration_state = WebAccelerationState::Ready;
                        state.acceleration_failure = None;
                        match element {
                            HostSurfaceElement::View => state.main_surface = Some(surface),
                            HostSurfaceElement::Popup => {
                                let rect = popup_rect.unwrap_or_default();
                                state.popup_surface = Some(RemotePopupSurface { rect, surface });
                            },
                        }
                        super::record_accelerated_frame();
                        acquired_surfaces.push(lease_id);
                    },
                    RemoteViewEvent::Host(event) => match event {
                        HostEvent::ViewReady { .. } => {
                            state.ready = true;
                            commands_after_ready.extend(state.deferred.drain(..));
                            if state.agent_event_capture_requested {
                                commands_after_ready.push(HostCommand::RenewAgentEventCapture {
                                    view_id: self.view_id,
                                });
                            }
                            for _ in 0..state.inspector_sessions {
                                commands_after_ready.push(HostCommand::RetainInspectorSession {
                                    view_id: self.view_id,
                                });
                            }
                        },
                        HostEvent::ViewFailed { error, .. } => {
                            state.ready = false;
                            if state.acceleration_state == WebAccelerationState::Pending {
                                super::record_accelerated_startup_failure();
                            }
                            state.acceleration_state = WebAccelerationState::Failed;
                            state.acceleration_failure = Some(error.clone());
                            state.main_surface = None;
                            state.popup_surface = None;
                            completions.extend(take_failed_request_completions(&mut state, &error));
                        },
                        HostEvent::AccelerationFailed { reason, .. } => {
                            if state.acceleration_state == WebAccelerationState::Pending {
                                super::record_accelerated_startup_failure();
                            }
                            state.acceleration_state = WebAccelerationState::Failed;
                            state.acceleration_failure = Some(reason);
                            state.main_surface = None;
                            state.popup_surface = None;
                        },
                        HostEvent::PopupClosed { .. } => state.popup_surface = None,
                        HostEvent::Title { title, .. } => state.title = Some(title),
                        HostEvent::Url { url, .. } => state.url = Some(url),
                        HostEvent::Downloads { downloads, .. } => state.downloads = downloads,
                        HostEvent::EvaluateResult { request_id, result, .. } => {
                            if let Some(PendingRequest {
                                completion: PendingCompletion::Evaluate(callback),
                                ..
                            }) = state.pending.remove(&request_id)
                            {
                                completions.push(RequestCompletion::Evaluate(callback, result));
                            }
                        },
                        HostEvent::DevToolsResult { request_id, result, .. } => {
                            if let Some(PendingRequest {
                                completion: PendingCompletion::DevTools(callback),
                                ..
                            }) = state.pending.remove(&request_id)
                            {
                                completions.push(RequestCompletion::DevTools(callback, result));
                            }
                        },
                        HostEvent::DevToolsEvent { payload, .. } => {
                            state.push_devtools_event(payload);
                        },
                        HostEvent::FileInputResult { request_id, result, .. } => {
                            if let Some(PendingRequest {
                                completion: PendingCompletion::FileInput(callback),
                                ..
                            }) = state.pending.remove(&request_id)
                            {
                                completions.push(RequestCompletion::FileInput(callback, result));
                            }
                        },
                        HostEvent::JsDialog { .. } => dialogs.push(event),
                        HostEvent::JsDialogClosed { dialog_id, .. } => {
                            closed_dialogs.push(Some(dialog_id));
                        },
                        HostEvent::TestResult { .. } => (),
                        HostEvent::Ready { .. }
                        | HostEvent::EditableFocus { .. }
                        | HostEvent::OpenUrl { .. } => (),
                    },
                }
            }
        }

        for lease_id in acquired_surfaces {
            let _ = self
                .supervisor
                .send(HostCommand::SurfaceAcquired { view_id: self.view_id, lease_id });
        }
        for command in commands_after_ready {
            self.send_live_command(command);
        }
        for dialog in dialogs {
            present_remote_js_dialog(&self.dialog_state, dialog);
        }
        for dialog_id in closed_dialogs {
            close_remote_js_dialog(&self.dialog_state, dialog_id, false);
        }
        finish_request_completions(completions);
    }

    pub fn expire_pending_requests(&self, now: Instant) -> Option<Instant> {
        self.drain_events();
        let (completions, next_deadline) = {
            let mut state = self.state.borrow_mut();
            let completions = take_expired_request_completions(&mut state, now);
            let next_deadline = state.pending.values().map(|pending| pending.deadline).min();
            (completions, next_deadline)
        };
        finish_request_completions(completions);
        next_deadline
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.drain_events();
        let completions = {
            let mut state = self.state.borrow_mut();
            take_failed_request_completions(&mut state, "web view closed")
        };
        finish_request_completions(completions);
        close_remote_js_dialog(&self.dialog_state, None, true);
        self.supervisor.unregister(self.view_id);
        super::unregister_webview();
    }
}

fn normalized_url(url: &str) -> String {
    if url.is_empty() { String::from("about:blank") } else { url.to_string() }
}

fn host_geometry(
    window: &crate::display::window::Window,
    layout: &BrowserViewportLayout,
) -> HostGeometry {
    let rect = webview_cef::cef_screen_rect(window, layout);
    HostGeometry {
        layout: layout.clone(),
        screen_rect: HostRect { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        scale_factor: window.scale_factor,
    }
}

fn remote_dialog_title(kind: HostJsDialogKind, origin_url: Option<&str>) -> String {
    let mut title = match kind {
        HostJsDialogKind::Alert => String::from("JavaScript Alert"),
        HostJsDialogKind::Confirm => String::from("JavaScript Confirm"),
        HostJsDialogKind::Prompt => String::from("JavaScript Prompt"),
        HostJsDialogKind::BeforeUnloadReload => String::from("Reload this page?"),
        HostJsDialogKind::BeforeUnloadNavigate => String::from("Leave this page?"),
    };
    if let Some(origin_url) = origin_url.filter(|url| !url.is_empty()) {
        title.push_str(" - ");
        title.push_str(origin_url);
    }
    title
}

fn present_remote_js_dialog(state: &StdRc<RefCell<RemoteJsDialogState>>, event: HostEvent) {
    let HostEvent::JsDialog {
        dialog_id, kind, origin_url, message_text, default_prompt_text, ..
    } = event
    else {
        return;
    };
    if state.borrow().active.is_some() {
        let state = state.borrow();
        let _ = state.supervisor.send(HostCommand::JsDialogResult {
            view_id: state.view_id,
            dialog_id,
            accepted: false,
            prompt_text: None,
        });
        return;
    }

    let mtm = MainThreadMarker::new().expect("remote JavaScript dialog requires main thread");
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(&remote_dialog_title(kind, origin_url.as_deref())));
    alert.setInformativeText(&NSString::from_str(&message_text));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    if !matches!(kind, HostJsDialogKind::Alert) {
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    }

    let prompt_field = if matches!(kind, HostJsDialogKind::Prompt) {
        let field = NSTextField::initWithFrame(
            NSTextField::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 22.0)),
        );
        field.setStringValue(&NSString::from_str(default_prompt_text.as_deref().unwrap_or("")));
        alert.setAccessoryView(Some(&field));
        alert.window().setInitialFirstResponder(Some(&field));
        Some(field)
    } else {
        None
    };

    let weak_state = StdRc::downgrade(state);
    let completion: RcBlock<dyn Fn(NSModalResponse)> = RcBlock::new(move |response| {
        let Some(state) = weak_state.upgrade() else {
            return;
        };
        let (supervisor, view_id, prompt_text) = {
            let mut state = state.borrow_mut();
            let Some(active) = state.active.take() else {
                return;
            };
            if active.dialog_id != dialog_id {
                state.active = Some(active);
                return;
            }
            (
                state.supervisor.clone(),
                state.view_id,
                active.prompt_field.map(|field| field.stringValue().to_string()),
            )
        };
        let _ = supervisor.send(HostCommand::JsDialogResult {
            view_id,
            dialog_id,
            accepted: response == NSAlertFirstButtonReturn,
            prompt_text,
        });
    });
    let parent_window = state.borrow().parent_window.clone();
    state.borrow_mut().active = Some(PendingRemoteDialog {
        dialog_id,
        alert: alert.clone(),
        prompt_field: prompt_field.clone(),
        _completion: completion.clone(),
    });
    alert.beginSheetModalForWindow_completionHandler(&parent_window, Some(&completion));
    if let Some(prompt_field) = prompt_field {
        alert.window().makeFirstResponder(Some(&prompt_field));
    }
}

fn close_remote_js_dialog(
    state: &StdRc<RefCell<RemoteJsDialogState>>,
    dialog_id: Option<u64>,
    notify_host: bool,
) {
    let (parent_window, active, supervisor, view_id) = {
        let mut state = state.borrow_mut();
        if dialog_id.is_some_and(|dialog_id| {
            state.active.as_ref().is_some_and(|active| active.dialog_id != dialog_id)
        }) {
            return;
        }
        let Some(active) = state.active.take() else {
            return;
        };
        (state.parent_window.clone(), active, state.supervisor.clone(), state.view_id)
    };
    let dialog_id = active.dialog_id;
    let sheet = active.alert.window();
    parent_window.endSheet_returnCode(&sheet, NSModalResponseAbort);
    if notify_host {
        let _ = supervisor.send(HostCommand::JsDialogResult {
            view_id,
            dialog_id,
            accepted: false,
            prompt_text: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{WebFrameDeliveryMode, normalized_url};

    #[test]
    fn empty_url_is_normalized_at_process_boundary() {
        assert_eq!(normalized_url(""), "about:blank");
        assert_eq!(normalized_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn hosted_frame_delivery_is_the_default() {
        assert_eq!(WebFrameDeliveryMode::default(), WebFrameDeliveryMode::CefHostIpc);
    }
}
