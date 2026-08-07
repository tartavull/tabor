use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc as StdRc;
use std::time::{Duration, Instant};

use cef::{
    CefString, Client, DevToolsMessageObserver, DisplayHandler, DownloadHandler, FocusHandler,
    ImplBeforeDownloadCallback, ImplBrowser, ImplBrowserHost, ImplClient,
    ImplDevToolsMessageObserver, ImplDictionaryValue, ImplDisplayHandler, ImplDownloadHandler,
    ImplDownloadItem, ImplFocusHandler, ImplFrame, ImplJsdialogCallback, ImplJsdialogHandler,
    ImplLifeSpanHandler, ImplListValue, ImplMediaAccessCallback, ImplPermissionHandler,
    ImplPermissionPromptCallback, ImplProcessMessage, ImplRenderHandler, ImplTask, JsdialogHandler,
    LifeSpanHandler, PermissionHandler, PermissionRequestResult, RenderHandler, Task, WrapClient,
    WrapDevToolsMessageObserver, WrapDisplayHandler, WrapDownloadHandler, WrapFocusHandler,
    WrapJsdialogHandler, WrapLifeSpanHandler, WrapPermissionHandler, WrapRenderHandler, WrapTask,
    rc::Rc,
};
use log::debug;
use objc2::encode::{Encode, Encoding};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, msg_send};
use objc2_app_kit::NSWindow;
use serde_json::{Map as JsonMap, Value as JsonValue};
use winit::event::{ElementState, KeyEvent, MouseButton};
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::raw_window_handle::RawWindowHandle;

use super::cef_host::HostEventSender;
use super::cef_host_protocol::{
    HostEvent, HostFrameEditCommand, HostGeometry, HostJsDialogKind, HostKeyEvent,
    HostKeyEventKind, HostMouseButton, HostMouseEvent, HostRect, HostSurfaceElement,
    HostSurfaceFormat, SurfaceLeaseId, ViewId,
};
use super::cef_surface_transport::{SurfaceSendRequest, SurfaceSender};
use super::keycodes::macos_scancode_from_physical_key;
use super::webview::WebAccelerationState;
use crate::display::browser_layout::BrowserViewportLayout;
use crate::display::window::Window;
#[cfg(test)]
use crate::event::{Event, EventType};
use crate::ipc::{AgentDownload, IpcError, IpcErrorCode};
#[cfg(test)]
use crate::tabs::TabId;
#[cfg(test)]
use winit::window::WindowId;
#[cfg(target_pointer_width = "32")]
type CGFloat = f32;
#[cfg(target_pointer_width = "64")]
type CGFloat = f64;

#[repr(C)]
struct CGPoint {
    x: CGFloat,
    y: CGFloat,
}

// SAFETY: The struct is `repr(C)`, and the encoding is correct.
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[CGFloat::ENCODING, CGFloat::ENCODING]);
}

#[repr(C)]
struct CGSize {
    width: CGFloat,
    height: CGFloat,
}

// SAFETY: The struct is `repr(C)`, and the encoding is correct.
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[CGFloat::ENCODING, CGFloat::ENCODING]);
}

#[repr(C)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

// SAFETY: The struct is `repr(C)`, and the encoding is correct.
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

type DevToolsCallback = Box<dyn FnOnce(Result<JsonValue, IpcError>)>;
type AgentReadyCallback = Box<dyn FnOnce(Result<(), IpcError>)>;

const AGENT_BOOTSTRAP_TEMPLATE: &str = include_str!("agent_bootstrap.js");
const AGENT_RUNTIME_VERSION_PLACEHOLDER: &str = "__TABOR_AGENT_RUNTIME_VERSION__";
const AGENT_RUNTIME_VERSION: u32 = 2;

const MAX_DEVTOOLS_EVENTS: usize = 2048;
const MAX_DEVTOOLS_EVENT_BYTES: usize = 256 * 1024;
const MAX_DEVTOOLS_EVENTS_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_DEVTOOLS_CALLS: usize = 256;
#[cfg(test)]
const DEVTOOLS_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const AGENT_EVENT_CAPTURE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

fn devtools_error(message: impl Into<String>) -> IpcError {
    IpcError::new(IpcErrorCode::Internal, message)
}

fn devtools_timeout() -> IpcError {
    IpcError::new(IpcErrorCode::Timeout, "DevTools method timed out")
}

#[derive(Clone)]
struct DevToolsEvent {
    id: u64,
    payload: String,
}

struct PendingDevToolsCall {
    callback: DevToolsCallback,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AgentRuntimePhase {
    #[default]
    Unregistered,
    Registering,
    Registered,
    Establishing,
    Ready,
}

struct AgentReadyWaiter {
    deadline: Instant,
    callback: AgentReadyCallback,
}

enum AgentRuntimeQueueAction {
    Run(AgentReadyWaiter),
    Wait,
    Register,
    Establish,
}

#[derive(Default)]
struct AgentRuntime {
    phase: AgentRuntimePhase,
    waiters: VecDeque<AgentReadyWaiter>,
}

impl AgentRuntime {
    fn enqueue(&mut self, waiter: AgentReadyWaiter) -> AgentRuntimeQueueAction {
        match self.phase {
            AgentRuntimePhase::Ready => AgentRuntimeQueueAction::Run(waiter),
            AgentRuntimePhase::Registering | AgentRuntimePhase::Establishing => {
                self.waiters.push_back(waiter);
                AgentRuntimeQueueAction::Wait
            },
            AgentRuntimePhase::Registered => {
                self.phase = AgentRuntimePhase::Establishing;
                self.waiters.push_back(waiter);
                AgentRuntimeQueueAction::Establish
            },
            AgentRuntimePhase::Unregistered => {
                self.phase = AgentRuntimePhase::Registering;
                self.waiters.push_back(waiter);
                AgentRuntimeQueueAction::Register
            },
        }
    }

    fn registration_succeeded(&mut self) {
        debug_assert_eq!(self.phase, AgentRuntimePhase::Registering);
        self.phase = AgentRuntimePhase::Establishing;
    }

    fn registration_failed(&mut self) -> VecDeque<AgentReadyWaiter> {
        debug_assert_eq!(self.phase, AgentRuntimePhase::Registering);
        self.phase = AgentRuntimePhase::Unregistered;
        std::mem::take(&mut self.waiters)
    }

    fn finish_establishment(&mut self, succeeded: bool) -> VecDeque<AgentReadyWaiter> {
        debug_assert_eq!(self.phase, AgentRuntimePhase::Establishing);
        self.phase =
            if succeeded { AgentRuntimePhase::Ready } else { AgentRuntimePhase::Registered };
        std::mem::take(&mut self.waiters)
    }
}

struct DevToolsState {
    next_message_id: i32,
    pending: HashMap<i32, PendingDevToolsCall>,
    events: VecDeque<DevToolsEvent>,
    retained_event_bytes: usize,
    next_event_id: u64,
    agent_event_capture_deadline: Option<Instant>,
    inspector_sessions: usize,
    agent_event_domains_enabled: bool,
}

struct AutomationState {
    downloads: HashMap<u32, AgentDownload>,
    download_order: Vec<u32>,
    download_dir: PathBuf,
    show_download_dialog: bool,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

#[derive(Debug)]
struct AcceleratedSurface {
    io_surface: NonNull<c_void>,
    width: usize,
    height: usize,
    format: cef::ColorType,
}

impl AcceleratedSurface {
    fn from_info(info: &cef::AcceleratedPaintInfo) -> Result<Self, String> {
        let width = info.extra.coded_size.width.max(0) as usize;
        let height = info.extra.coded_size.height.max(0) as usize;
        let Some(io_surface) = NonNull::new(info.shared_texture_io_surface) else {
            return Err(String::from("Accelerated paint returned a null IOSurface"));
        };

        if width == 0 || height == 0 {
            return Err(String::from("Accelerated paint returned an empty IOSurface"));
        }

        if info.format != cef::ColorType::BGRA_8888 && info.format != cef::ColorType::RGBA_8888 {
            return Err(format!("Unsupported accelerated color format: {:?}", info.format));
        }

        unsafe {
            CFRetain(io_surface.as_ptr().cast());
        }
        super::register_accelerated_surface();

        Ok(Self { io_surface, width, height, format: info.format })
    }

    fn retained_clone(&self) -> Self {
        unsafe {
            CFRetain(self.io_surface.as_ptr().cast());
        }
        super::register_accelerated_surface();
        Self {
            io_surface: self.io_surface,
            width: self.width,
            height: self.height,
            format: self.format,
        }
    }
}

#[derive(Clone)]
struct HostPaintBridge {
    view_id: ViewId,
    sender: HostEventSender,
    surface_sender: SurfaceSender,
    next_lease_id: StdRc<Cell<u64>>,
    leases: StdRc<RefCell<HashMap<SurfaceLeaseId, AcceleratedSurface>>>,
}

impl HostPaintBridge {
    fn new(view_id: ViewId, sender: HostEventSender, surface_sender: SurfaceSender) -> Self {
        Self {
            view_id,
            sender,
            surface_sender,
            next_lease_id: StdRc::new(Cell::new(1)),
            leases: StdRc::new(RefCell::new(HashMap::new())),
        }
    }

    fn publish(
        &self,
        element: HostSurfaceElement,
        surface: &AcceleratedSurface,
        popup_rect: Option<cef::Rect>,
    ) {
        let lease_id = self.next_lease_id.get();
        self.next_lease_id.set(lease_id.saturating_add(1));
        let format = match surface.format {
            cef::ColorType::BGRA_8888 => HostSurfaceFormat::Bgra8888,
            cef::ColorType::RGBA_8888 => HostSurfaceFormat::Rgba8888,
            _ => return,
        };
        self.leases.borrow_mut().insert(lease_id, surface.retained_clone());
        let popup_rect = popup_rect.map(|rect| HostRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        });
        if let Err(error) = self.surface_sender.send(SurfaceSendRequest {
            view_id: self.view_id,
            lease_id,
            element,
            io_surface: surface.io_surface.as_ptr(),
            width: surface.width,
            height: surface.height,
            format,
            popup_rect,
        }) {
            self.leases.borrow_mut().remove(&lease_id);
            debug!("Failed to send accelerated CEF frame: {error}");
        }
    }

    fn release(&self, lease_id: SurfaceLeaseId) {
        self.leases.borrow_mut().remove(&lease_id);
    }

    fn popup_closed(&self) {
        self.sender.send(HostEvent::PopupClosed { view_id: self.view_id });
    }
}

impl Drop for AcceleratedSurface {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.io_surface.as_ptr().cast());
        }
        super::unregister_accelerated_surface();
    }
}

#[derive(Debug, Default)]
struct PopupSurfaceState {
    rect: cef::Rect,
    surface: Option<AcceleratedSurface>,
}

#[derive(Debug)]
struct PaintState {
    layout: BrowserViewportLayout,
    screen_rect: cef::Rect,
    scale_factor: f32,
    acceleration_state: WebAccelerationState,
    main: Option<AcceleratedSurface>,
    popup: PopupSurfaceState,
    failure_reason: Option<String>,
}

impl PaintState {
    fn new(layout: BrowserViewportLayout, screen_rect: cef::Rect, scale_factor: f64) -> Self {
        Self {
            layout,
            screen_rect,
            scale_factor: browser_device_scale_factor(scale_factor),
            acceleration_state: WebAccelerationState::Pending,
            main: None,
            popup: PopupSurfaceState::default(),
            failure_reason: None,
        }
    }

    fn update_geometry(
        &mut self,
        layout: BrowserViewportLayout,
        screen_rect: cef::Rect,
        scale_factor: f64,
    ) {
        self.layout = layout;
        self.screen_rect = screen_rect;
        self.scale_factor = browser_device_scale_factor(scale_factor);
    }

    fn set_main_surface(&mut self, surface: AcceleratedSurface) {
        self.main = Some(surface);
        self.acceleration_state = WebAccelerationState::Ready;
        self.failure_reason = None;
    }

    fn set_popup_surface(&mut self, surface: AcceleratedSurface) {
        self.popup.surface = Some(surface);
    }

    fn clear_popup_surface(&mut self) {
        self.popup.surface = None;
    }

    fn fail(&mut self, reason: impl Into<String>) {
        if self.acceleration_state == WebAccelerationState::Pending {
            super::record_accelerated_startup_failure();
        }
        self.acceleration_state = WebAccelerationState::Failed;
        self.failure_reason = Some(reason.into());
        self.main = None;
        self.popup.surface = None;
    }
}

#[derive(Clone)]
enum WebViewEventTarget {
    Host {
        view_id: ViewId,
        sender: HostEventSender,
    },
    #[cfg(test)]
    Recorder {
        window_id: WindowId,
        tab_id: TabId,
        events: StdRc<RefCell<Vec<Event>>>,
    },
}

impl WebViewEventTarget {
    fn host(view_id: ViewId, sender: HostEventSender) -> Self {
        Self::Host { view_id, sender }
    }

    fn dirty(&self) {
        match self {
            #[cfg(test)]
            Self::Recorder { window_id, tab_id, events } => {
                let event = Event::for_tab(EventType::WebViewDirty, *window_id, *tab_id);
                events.borrow_mut().push(event);
            },
            Self::Host { .. } => (),
        }
    }

    fn editable_focus(&self, editable: bool) {
        match self {
            Self::Host { view_id, sender } => {
                sender.send(HostEvent::EditableFocus { view_id: *view_id, editable });
            },
            #[cfg(test)]
            Self::Recorder { .. } => (),
        }
    }

    fn open_url(&self, url: String, new_tab: bool) {
        match self {
            Self::Host { view_id, sender } => {
                sender.send(HostEvent::OpenUrl { view_id: *view_id, url, new_tab });
            },
            #[cfg(test)]
            Self::Recorder { .. } => (),
        }
    }

    fn title(&self, title: String) {
        match self {
            Self::Host { view_id, sender } => {
                sender.send(HostEvent::Title { view_id: *view_id, title });
            },
            #[cfg(test)]
            Self::Recorder { .. } => (),
        }
    }

    fn url(&self, url: String) {
        match self {
            Self::Host { view_id, sender } => {
                sender.send(HostEvent::Url { view_id: *view_id, url });
            },
            #[cfg(test)]
            Self::Recorder { .. } => (),
        }
    }

    fn downloads(&self, downloads: Vec<AgentDownload>) {
        match self {
            Self::Host { view_id, sender } => {
                sender.send(HostEvent::Downloads { view_id: *view_id, downloads });
            },
            #[cfg(test)]
            Self::Recorder { .. } => (),
        }
    }

    fn devtools_event(&self, id: u64, payload: String) {
        match self {
            Self::Host { view_id, sender } => {
                sender.send(HostEvent::DevToolsEvent { view_id: *view_id, id, payload });
            },
            #[cfg(test)]
            Self::Recorder { .. } => (),
        }
    }

    fn acceleration_failed(&self, reason: String) {
        match self {
            Self::Host { view_id, sender } => {
                sender.send(HostEvent::AccelerationFailed { view_id: *view_id, reason });
            },
            #[cfg(test)]
            Self::Recorder { .. } => (),
        }
    }
}

#[derive(Clone)]
struct WebViewDirtyNotifier(WebViewEventTarget);

impl WebViewDirtyNotifier {
    fn new(target: WebViewEventTarget) -> Self {
        Self(target)
    }

    fn send(&self) {
        self.0.dirty();
    }

    #[cfg(test)]
    fn recorder(window_id: WindowId, tab_id: TabId) -> (Self, StdRc<RefCell<Vec<Event>>>) {
        let events = StdRc::new(RefCell::new(Vec::new()));
        let target = WebViewEventTarget::Recorder { window_id, tab_id, events: events.clone() };
        (Self(target), events)
    }
}

#[derive(Clone)]
struct WebEditableFocusNotifier(WebViewEventTarget);

impl WebEditableFocusNotifier {
    fn send(&self, editable: bool) {
        self.0.editable_focus(editable);
    }
}

fn handle_ime_composition_range_change(
    browser: Option<&cef::Browser>,
    dirty_notifier: &WebViewDirtyNotifier,
    _selected_range: Option<&cef::Range>,
    _character_bounds: Option<&[cef::Rect]>,
) {
    if let Some(browser) = browser {
        invalidate_browser_surfaces(browser);
    }
    dirty_notifier.send();
}

fn handle_text_selection_change(
    browser: Option<&cef::Browser>,
    dirty_notifier: &WebViewDirtyNotifier,
    _selected_text: Option<&cef::CefString>,
    _selected_range: Option<&cef::Range>,
) {
    if let Some(browser) = browser {
        invalidate_browser_surfaces(browser);
    }
    dirty_notifier.send();
}

fn web_editable_focus_arg(args: Option<cef::ListValue>) -> Option<bool> {
    let args = args?;
    let index = super::cef::WEB_EDITABLE_FOCUS_EDITABLE_ARG_INDEX;
    if args.size() <= index || args.get_type(index) != cef::ValueType::BOOL {
        return None;
    }
    Some(args.bool(index) != 0)
}

fn parse_web_editable_focus_message(
    source_process: cef::ProcessId,
    message_name: &str,
    editable: Option<bool>,
) -> Option<bool> {
    if source_process != cef::ProcessId::RENDERER {
        return None;
    }
    if message_name != super::cef::WEB_EDITABLE_FOCUS_MESSAGE_NAME {
        return None;
    }
    editable
}

fn handle_client_process_message(
    editable_focus_notifier: &WebEditableFocusNotifier,
    source_process: cef::ProcessId,
    message: Option<&mut cef::ProcessMessage>,
) -> ::std::os::raw::c_int {
    let Some(message) = message else {
        return 0;
    };
    let message_name = {
        let name = message.name();
        CefString::from(&name).to_string()
    };
    let editable = parse_web_editable_focus_message(
        source_process,
        message_name.as_str(),
        web_editable_focus_arg(message.argument_list()),
    );
    let Some(editable) = editable else {
        return 0;
    };

    editable_focus_notifier.send(editable);
    1
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WebFocusPolicyState {
    host_focused: bool,
    editable_focused: bool,
    native_focus_armed: bool,
}

#[derive(Clone, Default)]
struct WebFocusPolicy {
    state: StdRc<RefCell<WebFocusPolicyState>>,
}

impl WebFocusPolicy {
    fn new() -> Self {
        Self::default()
    }

    fn set_host_focused(&self, host_focused: bool) {
        let mut state = self.state.borrow_mut();
        state.host_focused = host_focused;
        if !host_focused {
            state.native_focus_armed = false;
        }
    }

    fn set_editable_focused(&self, editable_focused: bool) {
        let mut state = self.state.borrow_mut();
        state.editable_focused = editable_focused;
        if !editable_focused {
            state.native_focus_armed = false;
        }
    }

    fn host_focused(&self) -> bool {
        self.state.borrow().host_focused
    }

    fn arm_native_focus(&self) {
        self.state.borrow_mut().native_focus_armed = true;
    }

    fn disarm_native_focus(&self) {
        self.state.borrow_mut().native_focus_armed = false;
    }

    fn allows_browser_focus(&self) -> bool {
        let state = self.state.borrow();
        state.host_focused && (state.editable_focused || state.native_focus_armed)
    }
}

cef::wrap_focus_handler! {
    struct TaborFocusHandler {
        focus_policy: WebFocusPolicy,
    }

    impl FocusHandler {
        fn on_take_focus(&self, _browser: Option<&mut cef::Browser>, _next: ::std::os::raw::c_int) {
            self.focus_policy.disarm_native_focus();
        }

        fn on_set_focus(
            &self,
            _browser: Option<&mut cef::Browser>,
            _source: cef::FocusSource,
        ) -> ::std::os::raw::c_int {
            if self.focus_policy.allows_browser_focus() { 0 } else { 1 }
        }

        fn on_got_focus(&self, _browser: Option<&mut cef::Browser>) {
            self.focus_policy.disarm_native_focus();
        }
    }
}

fn browser_device_scale_factor(scale_factor: f64) -> f32 {
    scale_factor.max(f32::MIN_POSITIVE as f64) as f32
}

fn browser_screen_point(
    layout: &BrowserViewportLayout,
    screen_rect: cef::Rect,
    view_x: i32,
    view_y: i32,
) -> Option<(i32, i32)> {
    let view_x = usize::try_from(view_x).ok()?;
    let view_y = usize::try_from(view_y).ok()?;
    let (visual_x, visual_y) = layout.visual_point_for_logical(view_x, view_y)?;
    let viewport = layout.viewport();
    Some((
        screen_rect.x + visual_x as i32 - viewport.x as i32,
        screen_rect.y + visual_y as i32 - viewport.y as i32,
    ))
}

pub(super) fn scaled_browser_wheel_delta_y(layout: &BrowserViewportLayout, delta_y: f64) -> f64 {
    delta_y * layout.column_count().max(1) as f64
}

fn invalidate_browser_surfaces(browser: &cef::Browser) {
    let Some(host) = browser.host() else {
        return;
    };

    host.invalidate(cef::PaintElementType::VIEW);
    host.invalidate(cef::PaintElementType::POPUP);
}

#[cfg(test)]
fn should_invalidate_after_key_input(state: ElementState) -> bool {
    matches!(state, ElementState::Pressed)
}

fn should_invalidate_after_frame_edit(command: FrameEditCommand) -> bool {
    !matches!(command, FrameEditCommand::Copy)
}

impl AutomationState {
    fn new() -> Self {
        let download_dir = super::default_download_dir();
        let show_download_dialog = super::should_show_download_dialog();
        Self {
            downloads: HashMap::new(),
            download_order: Vec::new(),
            download_dir,
            show_download_dialog,
        }
    }

    fn downloads(&self) -> Vec<AgentDownload> {
        self.download_order.iter().filter_map(|id| self.downloads.get(id).cloned()).collect()
    }

    fn update_download(&mut self, download: AgentDownload) {
        if !self.downloads.contains_key(&download.id) {
            self.download_order.push(download.id);
        }
        self.downloads.insert(download.id, download);
    }

    fn next_download_path(&self, suggested_name: &str) -> PathBuf {
        let suggested_name =
            if suggested_name.trim().is_empty() { "download.bin" } else { suggested_name };
        self.download_dir.join(suggested_name)
    }
}

impl DevToolsState {
    fn new() -> Self {
        Self {
            next_message_id: 1,
            pending: HashMap::new(),
            events: VecDeque::new(),
            retained_event_bytes: 0,
            next_event_id: 1,
            agent_event_capture_deadline: None,
            inspector_sessions: 0,
            agent_event_domains_enabled: false,
        }
    }

    fn next_id(&mut self) -> i32 {
        let id = self.next_message_id;
        self.next_message_id += 1;
        id
    }

    fn record_event(&mut self, method: &str, params: Option<&JsonValue>) -> DevToolsEvent {
        let mut object = JsonMap::new();
        object.insert("method".to_string(), JsonValue::String(method.to_string()));
        if let Some(params) = params {
            object.insert("params".to_string(), params.clone());
        }
        let mut payload = JsonValue::Object(object).to_string();
        if payload.len() > MAX_DEVTOOLS_EVENT_BYTES {
            payload = serde_json::json!({
                "method": method,
                "params": {
                    "taborTruncated": true,
                    "originalBytes": payload.len(),
                },
            })
            .to_string();
        }
        self.push_event(payload)
    }

    fn record_truncated_event(&mut self, method: &str, original_bytes: usize) -> DevToolsEvent {
        let payload = serde_json::json!({
            "method": method,
            "params": {
                "taborTruncated": true,
                "originalBytes": original_bytes,
            },
        })
        .to_string();
        self.push_event(payload)
    }

    fn push_event(&mut self, payload: String) -> DevToolsEvent {
        let id = self.next_event_id;
        self.next_event_id = self.next_event_id.saturating_add(1);
        self.retained_event_bytes = self.retained_event_bytes.saturating_add(payload.len());
        self.events.push_back(DevToolsEvent { id, payload });
        while self.events.len() > MAX_DEVTOOLS_EVENTS
            || self.retained_event_bytes > MAX_DEVTOOLS_EVENTS_BYTES
        {
            let event = self.events.pop_front().expect("DevTools event queue is not empty");
            self.retained_event_bytes =
                self.retained_event_bytes.saturating_sub(event.payload.len());
        }
        self.events.back().expect("pushed DevTools event is retained").clone()
    }

    #[cfg(test)]
    fn latest_event_id(&self) -> u64 {
        self.next_event_id.saturating_sub(1)
    }

    #[cfg(test)]
    fn events_since(&self, last_id: u64, max: usize) -> (Vec<String>, u64) {
        if max == 0 {
            return (Vec::new(), last_id);
        }

        let mut out = Vec::new();
        let mut newest = last_id;
        for event in &self.events {
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
                    serde_json::json!({
                        "id": event.id,
                        "payload": event.payload,
                    })
                    .to_string()
                });
            out.push(payload);
            newest = event.id;
            if out.len() >= max {
                break;
            }
        }
        (out, newest)
    }

    #[cfg(test)]
    fn insert_pending(
        &mut self,
        callback: DevToolsCallback,
        now: Instant,
        timeout: Duration,
    ) -> Result<i32, DevToolsCallback> {
        let Some(deadline) = now.checked_add(timeout) else {
            return Err(callback);
        };

        self.insert_pending_at(callback, deadline)
    }

    fn insert_pending_at(
        &mut self,
        callback: DevToolsCallback,
        deadline: Instant,
    ) -> Result<i32, DevToolsCallback> {
        if self.pending.len() >= MAX_PENDING_DEVTOOLS_CALLS {
            return Err(callback);
        }

        let id = self.next_id();
        self.pending.insert(id, PendingDevToolsCall { callback, deadline });
        Ok(id)
    }

    fn take_expired_callbacks(&mut self, now: Instant) -> Vec<DevToolsCallback> {
        let expired_ids = self
            .pending
            .iter()
            .filter_map(|(id, pending)| (pending.deadline <= now).then_some(*id))
            .collect::<Vec<_>>();

        expired_ids
            .into_iter()
            .filter_map(|id| self.pending.remove(&id).map(|pending| pending.callback))
            .collect()
    }

    fn renew_agent_event_capture(&mut self, now: Instant) -> bool {
        self.agent_event_capture_deadline = Some(now + AGENT_EVENT_CAPTURE_IDLE_TIMEOUT);
        if self.agent_event_domains_enabled {
            return false;
        }
        self.agent_event_domains_enabled = true;
        true
    }

    fn retain_inspector_session(&mut self) -> bool {
        self.inspector_sessions += 1;
        if self.agent_event_domains_enabled {
            return false;
        }
        self.agent_event_domains_enabled = true;
        true
    }

    fn release_inspector_session(&mut self, now: Instant) -> bool {
        assert!(self.inspector_sessions > 0, "unbalanced DevTools inspector session release");
        self.inspector_sessions -= 1;
        self.expire_agent_event_capture(now)
    }

    fn should_record_events(&mut self, now: Instant) -> (bool, bool) {
        let disable_domains = self.expire_agent_event_capture(now);
        let agent_capture_active =
            self.agent_event_capture_deadline.is_some_and(|deadline| now < deadline);
        (agent_capture_active || self.inspector_sessions > 0, disable_domains)
    }

    fn expire_agent_event_capture(&mut self, now: Instant) -> bool {
        if self.agent_event_capture_deadline.is_some_and(|deadline| now >= deadline) {
            self.agent_event_capture_deadline = None;
        }
        if self.agent_event_capture_deadline.is_some()
            || self.inspector_sessions > 0
            || !self.agent_event_domains_enabled
        {
            return false;
        }

        true
    }

    fn mark_agent_event_domains_disabled(&mut self) {
        self.agent_event_domains_enabled = false;
    }
}

fn fail_expired_devtools_callbacks(state: &StdRc<RefCell<DevToolsState>>, now: Instant) {
    let callbacks = state.borrow_mut().take_expired_callbacks(now);
    for callback in callbacks {
        callback(Err(devtools_timeout()));
    }
}

cef::wrap_task! {
    struct ExpireDevToolsCallTask {
        state: StdRc<RefCell<DevToolsState>>,
        id: i32,
    }

    impl Task {
        fn execute(&self) {
            let callback = self
                .state
                .borrow_mut()
                .pending
                .remove(&self.id)
                .map(|pending| pending.callback);
            if let Some(callback) = callback {
                callback(Err(devtools_timeout()));
            }
        }
    }
}

fn insert_scheduled_devtools_call(
    state: &StdRc<RefCell<DevToolsState>>,
    callback: DevToolsCallback,
    deadline: Instant,
) -> Result<i32, (DevToolsCallback, IpcError)> {
    let Some(remaining) =
        deadline.checked_duration_since(Instant::now()).filter(|remaining| !remaining.is_zero())
    else {
        return Err((callback, devtools_timeout()));
    };
    let id = state
        .borrow_mut()
        .insert_pending_at(callback, deadline)
        .map_err(|callback| (callback, devtools_error("Too many pending DevTools methods")))?;
    let delay_ms = match i64::try_from(remaining.as_millis()) {
        Ok(delay_ms) => delay_ms,
        Err(_) => {
            let callback = state
                .borrow_mut()
                .pending
                .remove(&id)
                .expect("inserted DevTools callback")
                .callback;
            return Err((callback, devtools_error("DevTools timeout is too large")));
        },
    };
    let mut task = ExpireDevToolsCallTask::new(StdRc::clone(state), id);
    if cef::post_delayed_task(cef::ThreadId::UI, Some(&mut task), delay_ms) != 0 {
        return Ok(id);
    }

    let callback =
        state.borrow_mut().pending.remove(&id).expect("inserted DevTools callback").callback;
    Err((callback, devtools_error("Could not schedule DevTools timeout")))
}

const AGENT_EVENT_DOMAINS: [&str; 4] = ["Network", "Page", "Runtime", "Log"];

fn set_agent_event_domains(
    browser: &cef::Browser,
    state: &StdRc<RefCell<DevToolsState>>,
    enabled: bool,
) -> bool {
    let Some(host) = browser.host() else {
        return false;
    };

    let command = if enabled { "enable" } else { "disable" };
    let mut all_dispatched = true;
    for domain in AGENT_EVENT_DOMAINS {
        let method = CefString::from(format!("{domain}.{command}").as_str());
        let id = state.borrow_mut().next_id();
        all_dispatched &= host.execute_dev_tools_method(id, Some(&method), None) != 0;
    }
    all_dispatched
}

cef::wrap_dev_tools_message_observer! {
    struct TaborDevToolsObserver {
        state: StdRc<RefCell<DevToolsState>>,
        target: WebViewEventTarget,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut cef::Browser>,
            message_id: i32,
            success: i32,
            result: Option<&[u8]>,
        ) {
            let now = Instant::now();
            let (callback, expired_callbacks) = {
                let mut state = self.state.borrow_mut();
                let expired_callbacks = state.take_expired_callbacks(now);
                let callback = state.pending.remove(&message_id).map(|pending| pending.callback);
                (callback, expired_callbacks)
            };
            for callback in expired_callbacks {
                callback(Err(devtools_timeout()));
            }

            let Some(callback) = callback else {
                return;
            };

            if success == 0 {
                callback(Err(devtools_error("DevTools method failed")));
                return;
            }

            let payload = result
                .and_then(|bytes| serde_json::from_slice::<JsonValue>(bytes).ok())
                .unwrap_or(JsonValue::Null);
            callback(Ok(payload));
        }

        fn on_dev_tools_event(
            &self,
            browser: Option<&mut cef::Browser>,
            method: Option<&cef::CefString>,
            params: Option<&[u8]>,
        ) {
            let now = Instant::now();
            let (should_record, disable_domains, expired_callbacks) = {
                let mut state = self.state.borrow_mut();
                let expired_callbacks = state.take_expired_callbacks(now);
                let (should_record, disable_domains) = state.should_record_events(now);
                (should_record, disable_domains, expired_callbacks)
            };
            for callback in expired_callbacks {
                callback(Err(devtools_timeout()));
            }
            if disable_domains
                && browser
                    .as_deref()
                    .is_some_and(|browser| set_agent_event_domains(browser, &self.state, false))
            {
                self.state.borrow_mut().mark_agent_event_domains_disabled();
            }
            if !should_record {
                return;
            }

            let Some(method) = method else {
                return;
            };
            let method = method.to_string();
            if method.is_empty() {
                return;
            }
            if let Some(params) = params {
                if params.len() > MAX_DEVTOOLS_EVENT_BYTES {
                    let event =
                        self.state.borrow_mut().record_truncated_event(&method, params.len());
                    self.target.devtools_event(event.id, event.payload);
                    return;
                }
            }
            let params = params.and_then(|bytes| serde_json::from_slice::<JsonValue>(bytes).ok());
            let event = self.state.borrow_mut().record_event(&method, params.as_ref());
            self.target.devtools_event(event.id, event.payload);
        }
    }
}

cef::wrap_display_handler! {
    struct TaborDisplayHandler {
        title: StdRc<RefCell<Option<String>>>,
        target: WebViewEventTarget,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut cef::Browser>,
            frame: Option<&mut cef::Frame>,
            url: Option<&cef::CefString>,
        ) {
            if frame.is_some_and(|frame| frame.is_main() != 0) {
                if let Some(url) = url {
                    self.target.url(url.to_string());
                }
            }
        }

        fn on_title_change(&self, _browser: Option<&mut cef::Browser>, title: Option<&cef::CefString>) {
            if let Some(title) = title {
                let title = title.to_string();
                *self.title.borrow_mut() = Some(title.clone());
                self.target.title(title);
            }
        }
    }
}

cef::wrap_life_span_handler! {
    struct TaborLifeSpanHandler {
        target: WebViewEventTarget,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&cef::CefString>,
            _target_frame_name: Option<&cef::CefString>,
            _target_disposition: cef::WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&cef::PopupFeatures>,
            _window_info: Option<&mut cef::WindowInfo>,
            _client: Option<&mut Option<cef::Client>>,
            _settings: Option<&mut cef::BrowserSettings>,
            _extra_info: Option<&mut Option<cef::DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let Some(target_url) = target_url else {
                return 0;
            };
            let url = target_url.to_string();
            if url.is_empty() || url.eq_ignore_ascii_case("about:blank") {
                return 0;
            }

            self.target.open_url(url, true);
            1
        }
    }
}

cef::wrap_render_handler! {
    struct TaborRenderHandler {
        paint_state: StdRc<RefCell<PaintState>>,
        dirty_notifier: WebViewDirtyNotifier,
        host_bridge: Option<HostPaintBridge>,
    }

    impl RenderHandler {
        fn root_screen_rect(
            &self,
            _browser: Option<&mut cef::Browser>,
            rect: Option<&mut cef::Rect>,
        ) -> ::std::os::raw::c_int {
            let Some(rect) = rect else {
                return 0;
            };

            *rect = self.paint_state.borrow().screen_rect.clone();
            1
        }

        fn view_rect(&self, _browser: Option<&mut cef::Browser>, rect: Option<&mut cef::Rect>) {
            let Some(rect) = rect else {
                return;
            };

            let paint_state = self.paint_state.borrow();
            *rect = cef::Rect {
                x: 0,
                y: 0,
                width: paint_state.layout.logical_width() as i32,
                height: paint_state.layout.logical_height() as i32,
            };
        }

        fn screen_point(
            &self,
            _browser: Option<&mut cef::Browser>,
            view_x: ::std::os::raw::c_int,
            view_y: ::std::os::raw::c_int,
            screen_x: Option<&mut ::std::os::raw::c_int>,
            screen_y: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let paint_state = self.paint_state.borrow();
            let Some((mapped_x, mapped_y)) =
                browser_screen_point(
                    &paint_state.layout,
                    paint_state.screen_rect.clone(),
                    view_x,
                    view_y,
                )
            else {
                return 0;
            };
            if let Some(screen_x) = screen_x {
                *screen_x = mapped_x;
            }
            if let Some(screen_y) = screen_y {
                *screen_y = mapped_y;
            }
            1
        }

        fn screen_info(
            &self,
            _browser: Option<&mut cef::Browser>,
            screen_info: Option<&mut cef::ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(screen_info) = screen_info else {
                return 0;
            };

            let paint_state = self.paint_state.borrow();
            screen_info.device_scale_factor = paint_state.scale_factor;
            screen_info.depth = 32;
            screen_info.depth_per_component = 8;
            screen_info.is_monochrome = 0;
            screen_info.rect = paint_state.screen_rect.clone();
            screen_info.available_rect = paint_state.screen_rect.clone();
            1
        }

        fn on_popup_show(&self, _browser: Option<&mut cef::Browser>, show: ::std::os::raw::c_int) {
            if show == 0 {
                self.paint_state.borrow_mut().clear_popup_surface();
                if let Some(host_bridge) = &self.host_bridge {
                    host_bridge.popup_closed();
                }
                self.dirty_notifier.send();
            }
        }

        fn on_popup_size(&self, _browser: Option<&mut cef::Browser>, rect: Option<&cef::Rect>) {
            let Some(rect) = rect else {
                return;
            };

            self.paint_state.borrow_mut().popup.rect = rect.clone();
        }

        fn on_paint(
            &self,
            _browser: Option<&mut cef::Browser>,
            type_: cef::PaintElementType,
            _dirty_rects: Option<&[cef::Rect]>,
            _buffer: *const u8,
            _width: ::std::os::raw::c_int,
            _height: ::std::os::raw::c_int,
        ) {
            super::record_unexpected_cpu_paint();
            if type_ != cef::PaintElementType::VIEW {
                return;
            }

            let mut paint_state = self.paint_state.borrow_mut();
            if paint_state.acceleration_state == WebAccelerationState::Pending {
                let reason =
                    String::from("Received CPU paint callback before accelerated rendering became ready");
                paint_state.fail(reason.clone());
                self.dirty_notifier.0.acceleration_failed(reason);
            }
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut cef::Browser>,
            type_: cef::PaintElementType,
            _dirty_rects: Option<&[cef::Rect]>,
            info: Option<&cef::AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else {
                if type_ == cef::PaintElementType::VIEW {
                    let mut paint_state = self.paint_state.borrow_mut();
                    if paint_state.acceleration_state == WebAccelerationState::Pending {
                        let reason =
                            String::from("Accelerated paint callback was missing IOSurface info");
                        paint_state.fail(reason.clone());
                        self.dirty_notifier.0.acceleration_failed(reason);
                    }
                }
                return;
            };

            let surface = match AcceleratedSurface::from_info(info) {
                Ok(surface) => surface,
                Err(err) => {
                    let mut paint_state = self.paint_state.borrow_mut();
                    if type_ == cef::PaintElementType::VIEW {
                        paint_state.fail(err.clone());
                        self.dirty_notifier.0.acceleration_failed(err);
                    } else if type_ == cef::PaintElementType::POPUP {
                        paint_state.clear_popup_surface();
                    }
                    return;
                },
            };

            let popup_rect = (type_ == cef::PaintElementType::POPUP)
                .then(|| self.paint_state.borrow().popup.rect.clone());
            if let Some(host_bridge) = &self.host_bridge {
                let element = if type_ == cef::PaintElementType::VIEW {
                    HostSurfaceElement::View
                } else if type_ == cef::PaintElementType::POPUP {
                    HostSurfaceElement::Popup
                } else {
                    return;
                };
                host_bridge.publish(element, &surface, popup_rect);
            }

            let mut paint_state = self.paint_state.borrow_mut();
            match type_ {
                t if t == cef::PaintElementType::VIEW => paint_state.set_main_surface(surface),
                t if t == cef::PaintElementType::POPUP => paint_state.set_popup_surface(surface),
                _ => return,
            }
            super::record_accelerated_frame();

            self.dirty_notifier.send();
        }

        fn on_ime_composition_range_changed(
            &self,
            browser: Option<&mut cef::Browser>,
            selected_range: Option<&cef::Range>,
            character_bounds: Option<&[cef::Rect]>,
        ) {
            handle_ime_composition_range_change(
                browser.as_deref(),
                &self.dirty_notifier,
                selected_range,
                character_bounds,
            );
        }

        fn on_text_selection_changed(
            &self,
            browser: Option<&mut cef::Browser>,
            selected_text: Option<&cef::CefString>,
            selected_range: Option<&cef::Range>,
        ) {
            handle_text_selection_change(
                browser.as_deref(),
                &self.dirty_notifier,
                selected_text,
                selected_range,
            );
        }
    }
}

cef::wrap_download_handler! {
    struct TaborDownloadHandler {
        automation_state: StdRc<RefCell<AutomationState>>,
        target: WebViewEventTarget,
    }

    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut cef::Browser>,
            _url: Option<&cef::CefString>,
            _request_method: Option<&cef::CefString>,
        ) -> ::std::os::raw::c_int {
            1
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut cef::Browser>,
            download_item: Option<&mut cef::DownloadItem>,
            suggested_name: Option<&cef::CefString>,
            callback: Option<&mut cef::BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else {
                return 0;
            };
            let suggested_name = suggested_name
                .map(|name| name.to_string())
                .or_else(|| {
                    download_item.as_ref().map(|item| {
                        let suggested_name = item.suggested_file_name();
                        CefString::from(&suggested_name).to_string()
                    })
                })
                .unwrap_or_else(|| String::from("download.bin"));
            let state = self.automation_state.borrow_mut();
            let path = state.next_download_path(&suggested_name);
            let show_dialog = if state.show_download_dialog { 1 } else { 0 };
            let _ = fs::create_dir_all(path.parent().unwrap_or(&state.download_dir));
            let download_path = CefString::from(path.to_string_lossy().as_ref());
            callback.cont(Some(&download_path), show_dialog);
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut cef::Browser>,
            download_item: Option<&mut cef::DownloadItem>,
            _callback: Option<&mut cef::DownloadItemCallback>,
        ) {
            let Some(item) = download_item else {
                return;
            };
            let state = if item.is_complete() != 0 {
                "complete"
            } else if item.is_canceled() != 0 {
                "canceled"
            } else if item.is_interrupted() != 0 {
                "interrupted"
            } else if item.is_paused() != 0 {
                "paused"
            } else if item.is_in_progress() != 0 {
                "in_progress"
            } else {
                "unknown"
            };
            self.automation_state.borrow_mut().update_download(AgentDownload {
                id: item.id(),
                state: state.to_string(),
                url: {
                    let url = item.url();
                    CefString::from(&url).to_string()
                },
                suggested_name: {
                    let suggested_name = item.suggested_file_name();
                    CefString::from(&suggested_name).to_string()
                },
                full_path: {
                    let path = {
                        let full_path = item.full_path();
                        CefString::from(&full_path).to_string()
                    };
                    (!path.is_empty()).then_some(path)
                },
                mime_type: {
                    let mime_type = {
                        let mime_type = item.mime_type();
                        CefString::from(&mime_type).to_string()
                    };
                    (!mime_type.is_empty()).then_some(mime_type)
                },
                percent_complete: (item.percent_complete() >= 0)
                    .then_some(i64::from(item.percent_complete())),
                total_bytes: (item.total_bytes() > 0).then_some(item.total_bytes()),
                received_bytes: (item.received_bytes() > 0).then_some(item.received_bytes()),
            });
            self.target.downloads(self.automation_state.borrow().downloads());
        }
    }
}

#[cfg(not(feature = "passkey-webauthn"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionDecision {
    Allow,
    Deny,
}

#[cfg(not(feature = "passkey-webauthn"))]
fn all_known_permission_mask() -> u32 {
    use cef::PermissionRequestTypes as Permission;

    Permission::AR_SESSION.get_raw()
        | Permission::CAMERA_PAN_TILT_ZOOM.get_raw()
        | Permission::CAMERA_STREAM.get_raw()
        | Permission::CAPTURED_SURFACE_CONTROL.get_raw()
        | Permission::CLIPBOARD.get_raw()
        | Permission::TOP_LEVEL_STORAGE_ACCESS.get_raw()
        | Permission::DISK_QUOTA.get_raw()
        | Permission::LOCAL_FONTS.get_raw()
        | Permission::GEOLOCATION.get_raw()
        | Permission::HAND_TRACKING.get_raw()
        | Permission::IDENTITY_PROVIDER.get_raw()
        | Permission::IDLE_DETECTION.get_raw()
        | Permission::MIC_STREAM.get_raw()
        | Permission::MIDI_SYSEX.get_raw()
        | Permission::MULTIPLE_DOWNLOADS.get_raw()
        | Permission::NOTIFICATIONS.get_raw()
        | Permission::KEYBOARD_LOCK.get_raw()
        | Permission::POINTER_LOCK.get_raw()
        | Permission::PROTECTED_MEDIA_IDENTIFIER.get_raw()
        | Permission::REGISTER_PROTOCOL_HANDLER.get_raw()
        | Permission::STORAGE_ACCESS.get_raw()
        | Permission::VR_SESSION.get_raw()
        | Permission::WEB_APP_INSTALLATION.get_raw()
        | Permission::WINDOW_MANAGEMENT.get_raw()
        | Permission::FILE_SYSTEM_ACCESS.get_raw()
        | Permission::LOCAL_NETWORK.get_raw()
}

#[cfg(not(feature = "passkey-webauthn"))]
fn blocked_permission_mask() -> u32 {
    use cef::PermissionRequestTypes as Permission;

    let mut blocked = Permission::AR_SESSION.get_raw()
        | Permission::VR_SESSION.get_raw()
        | Permission::HAND_TRACKING.get_raw();
    if super::distribution_channel().is_mac_app_store() {
        blocked |= Permission::FILE_SYSTEM_ACCESS.get_raw() | Permission::LOCAL_NETWORK.get_raw();
    }
    blocked
}

#[cfg(not(feature = "passkey-webauthn"))]
fn logged_allowed_permission_mask() -> u32 {
    use cef::PermissionRequestTypes as Permission;

    Permission::CAMERA_PAN_TILT_ZOOM.get_raw()
        | Permission::CAMERA_STREAM.get_raw()
        | Permission::MIC_STREAM.get_raw()
}

#[cfg(not(feature = "passkey-webauthn"))]
fn permission_decision(requested_permissions: u32) -> PermissionDecision {
    if requested_permissions == 0 {
        return PermissionDecision::Deny;
    }

    let unknown_permissions = requested_permissions & !all_known_permission_mask();
    if unknown_permissions != 0 {
        return PermissionDecision::Deny;
    }

    if requested_permissions & blocked_permission_mask() != 0 {
        return PermissionDecision::Deny;
    }

    PermissionDecision::Allow
}

#[cfg(not(feature = "passkey-webauthn"))]
fn media_access_decision(requested_permissions: u32) -> PermissionDecision {
    use cef::MediaAccessPermissionTypes as Permission;

    if requested_permissions == 0 {
        return PermissionDecision::Deny;
    }

    let allowed_media_permissions =
        Permission::DEVICE_AUDIO_CAPTURE.get_raw() | Permission::DEVICE_VIDEO_CAPTURE.get_raw();
    if requested_permissions & !allowed_media_permissions != 0 {
        return PermissionDecision::Deny;
    }

    PermissionDecision::Allow
}

#[cfg(not(feature = "passkey-webauthn"))]
fn permission_request_result(requested_permissions: u32) -> PermissionRequestResult {
    match permission_decision(requested_permissions) {
        PermissionDecision::Allow => PermissionRequestResult::ACCEPT,
        PermissionDecision::Deny => PermissionRequestResult::DENY,
    }
}

#[cfg(not(feature = "passkey-webauthn"))]
fn should_block_media_access_request(requested_permissions: u32) -> bool {
    matches!(media_access_decision(requested_permissions), PermissionDecision::Deny)
}

#[cfg(not(feature = "passkey-webauthn"))]
fn should_log_allowed_permission_request(requested_permissions: u32) -> bool {
    requested_permissions & logged_allowed_permission_mask() != 0
}

#[cfg(not(feature = "passkey-webauthn"))]
fn log_permission_request(
    decision: PermissionDecision,
    source: &str,
    requesting_origin: Option<&cef::CefString>,
    requested_permissions: u32,
) {
    let origin = requesting_origin
        .map(|origin| origin.to_string())
        .unwrap_or_else(|| String::from("<unknown>"));
    let decision = match decision {
        PermissionDecision::Allow => "Accepted",
        PermissionDecision::Deny => "Denied",
    };
    debug!(
        "{decision} CEF permission request (source={source}, origin={origin}, mask=0x{requested_permissions:08x})"
    );
}

#[cfg(not(feature = "passkey-webauthn"))]
cef::wrap_permission_handler! {
    struct TaborPermissionHandler {}

    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            requesting_origin: Option<&cef::CefString>,
            requested_permissions: u32,
            callback: Option<&mut cef::MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            if should_block_media_access_request(requested_permissions) {
                if let Some(callback) = callback {
                    callback.cancel();
                }
                log_permission_request(
                    PermissionDecision::Deny,
                    "media_access",
                    requesting_origin,
                    requested_permissions,
                );
                return 1;
            }

            if let Some(callback) = callback {
                callback.cont(requested_permissions);
            }
            log_permission_request(
                PermissionDecision::Allow,
                "media_access",
                requesting_origin,
                requested_permissions,
            );
            1
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut cef::Browser>,
            _prompt_id: u64,
            requesting_origin: Option<&cef::CefString>,
            requested_permissions: u32,
            callback: Option<&mut cef::PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            let result = permission_request_result(requested_permissions);
            if let Some(callback) = callback {
                callback.cont(result);
            }
            match result {
                PermissionRequestResult::ACCEPT
                    if should_log_allowed_permission_request(requested_permissions) =>
                {
                    log_permission_request(
                        PermissionDecision::Allow,
                        "prompt",
                        requesting_origin,
                        requested_permissions,
                    );
                },
                PermissionRequestResult::DENY => {
                    log_permission_request(
                        PermissionDecision::Deny,
                        "prompt",
                        requesting_origin,
                        requested_permissions,
                    );
                },
                _ => (),
            }
            1
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JsDialogKind {
    Alert,
    Confirm,
    Prompt,
    BeforeUnload { is_reload: bool },
}

struct HostJsDialogState {
    view_id: ViewId,
    sender: HostEventSender,
    next_dialog_id: u64,
    pending: HashMap<u64, cef::JsdialogCallback>,
}

#[derive(Clone)]
struct JsDialogBackend(StdRc<RefCell<HostJsDialogState>>);

impl JsDialogBackend {
    fn has_active(&self) -> bool {
        !self.0.borrow().pending.is_empty()
    }

    fn present(
        &self,
        kind: JsDialogKind,
        origin_url: Option<&cef::CefString>,
        message_text: &cef::CefString,
        default_prompt_text: Option<&cef::CefString>,
        callback: cef::JsdialogCallback,
    ) -> bool {
        let mut state = self.0.borrow_mut();
        if !state.pending.is_empty() {
            return false;
        }
        let dialog_id = state.next_dialog_id;
        state.next_dialog_id = state.next_dialog_id.saturating_add(1);
        state.pending.insert(dialog_id, callback);
        let kind = match kind {
            JsDialogKind::Alert => HostJsDialogKind::Alert,
            JsDialogKind::Confirm => HostJsDialogKind::Confirm,
            JsDialogKind::Prompt => HostJsDialogKind::Prompt,
            JsDialogKind::BeforeUnload { is_reload: true } => HostJsDialogKind::BeforeUnloadReload,
            JsDialogKind::BeforeUnload { is_reload: false } => {
                HostJsDialogKind::BeforeUnloadNavigate
            },
        };
        let event = HostEvent::JsDialog {
            view_id: state.view_id,
            dialog_id,
            kind,
            origin_url: origin_url.map(ToString::to_string),
            message_text: message_text.to_string(),
            default_prompt_text: default_prompt_text.map(ToString::to_string),
        };
        if state.sender.send(event) {
            true
        } else {
            state.pending.remove(&dialog_id);
            false
        }
    }

    fn cancel(&self) {
        let mut state = self.0.borrow_mut();
        let dialog_ids = state.pending.keys().copied().collect::<Vec<_>>();
        state.pending.clear();
        for dialog_id in dialog_ids {
            state.sender.send(HostEvent::JsDialogClosed { view_id: state.view_id, dialog_id });
        }
    }

    fn complete(&self, dialog_id: u64, accepted: bool, prompt_text: Option<&str>) {
        let Some(callback) = self.0.borrow_mut().pending.remove(&dialog_id) else {
            return;
        };
        if let Some(prompt_text) = prompt_text {
            callback.cont(i32::from(accepted), Some(&CefString::from(prompt_text)));
        } else {
            callback.cont(i32::from(accepted), None);
        }
    }
}

cef::wrap_jsdialog_handler! {
    struct TaborJsDialogHandler {
        backend: JsDialogBackend,
    }

    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut cef::Browser>,
            origin_url: Option<&cef::CefString>,
            dialog_type: cef::JsdialogType,
            message_text: Option<&cef::CefString>,
            default_prompt_text: Option<&cef::CefString>,
            callback: Option<&mut cef::JsdialogCallback>,
            suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let kind = match dialog_type {
                cef::JsdialogType::ALERT => JsDialogKind::Alert,
                cef::JsdialogType::CONFIRM => JsDialogKind::Confirm,
                cef::JsdialogType::PROMPT => JsDialogKind::Prompt,
                _ => return 0,
            };
            if self.backend.has_active() {
                if let Some(suppress_message) = suppress_message {
                    *suppress_message = 1;
                }
                return 0;
            }

            let shown = self.backend.present(
                kind,
                origin_url,
                message_text.expect("CEF JavaScript dialogs must provide message text"),
                default_prompt_text,
                callback.expect("CEF JavaScript dialogs must provide a callback").clone(),
            );
            i32::from(shown)
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut cef::Browser>,
            message_text: Option<&cef::CefString>,
            is_reload: ::std::os::raw::c_int,
            callback: Option<&mut cef::JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            let shown = self.backend.present(
                JsDialogKind::BeforeUnload { is_reload: is_reload != 0 },
                None,
                message_text.expect("CEF before-unload dialogs must provide message text"),
                None,
                callback.expect("CEF before-unload dialogs must provide a callback").clone(),
            );
            i32::from(shown)
        }

        fn on_reset_dialog_state(&self, _browser: Option<&mut cef::Browser>) {
            self.backend.cancel();
        }

        fn on_dialog_closed(&self, _browser: Option<&mut cef::Browser>) {
            self.backend.cancel();
        }
    }
}

#[derive(Clone)]
struct TaborClientWebState {
    jsdialog_handler: cef::JsdialogHandler,
    test_use_default_js_dialog: bool,
    editable_focus_notifier: WebEditableFocusNotifier,
}

#[cfg(not(feature = "passkey-webauthn"))]
cef::wrap_client! {
    struct TaborClient {
        display_handler: cef::DisplayHandler,
        render_handler: cef::RenderHandler,
        download_handler: cef::DownloadHandler,
        focus_handler: cef::FocusHandler,
        life_span_handler: cef::LifeSpanHandler,
        web_state: TaborClientWebState,
        permission_handler: cef::PermissionHandler,
    }

    impl Client {
        fn display_handler(&self) -> Option<cef::DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn render_handler(&self) -> Option<cef::RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn download_handler(&self) -> Option<cef::DownloadHandler> {
            Some(self.download_handler.clone())
        }

        fn focus_handler(&self) -> Option<cef::FocusHandler> {
            Some(self.focus_handler.clone())
        }

        fn life_span_handler(&self) -> Option<cef::LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn jsdialog_handler(&self) -> Option<cef::JsdialogHandler> {
            (!self.web_state.test_use_default_js_dialog)
                .then(|| self.web_state.jsdialog_handler.clone())
        }

        fn on_process_message_received(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            source_process: cef::ProcessId,
            message: Option<&mut cef::ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            handle_client_process_message(
                &self.web_state.editable_focus_notifier,
                source_process,
                message,
            )
        }

        fn permission_handler(&self) -> Option<cef::PermissionHandler> {
            Some(self.permission_handler.clone())
        }
    }
}

#[cfg(feature = "passkey-webauthn")]
cef::wrap_client! {
    struct TaborClient {
        display_handler: cef::DisplayHandler,
        render_handler: cef::RenderHandler,
        download_handler: cef::DownloadHandler,
        focus_handler: cef::FocusHandler,
        life_span_handler: cef::LifeSpanHandler,
        web_state: TaborClientWebState,
    }

    impl Client {
        fn display_handler(&self) -> Option<cef::DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn render_handler(&self) -> Option<cef::RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn download_handler(&self) -> Option<cef::DownloadHandler> {
            Some(self.download_handler.clone())
        }

        fn focus_handler(&self) -> Option<cef::FocusHandler> {
            Some(self.focus_handler.clone())
        }

        fn life_span_handler(&self) -> Option<cef::LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn jsdialog_handler(&self) -> Option<cef::JsdialogHandler> {
            (!self.web_state.test_use_default_js_dialog)
                .then(|| self.web_state.jsdialog_handler.clone())
        }

        fn on_process_message_received(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            source_process: cef::ProcessId,
            message: Option<&mut cef::ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            handle_client_process_message(
                &self.web_state.editable_focus_notifier,
                source_process,
                message,
            )
        }
    }
}

#[derive(Clone, Copy)]
enum FrameEditCommand {
    Copy,
    Cut,
    Paste,
}

cef::wrap_task! {
    struct FrameEditTask {
        browser: cef::Browser,
        command: FrameEditCommand,
    }

    impl Task {
        fn execute(&self) {
            let frame = self.browser.focused_frame().or_else(|| self.browser.main_frame());
            let Some(frame) = frame else {
                return;
            };

            match self.command {
                FrameEditCommand::Copy => frame.copy(),
                FrameEditCommand::Cut => frame.cut(),
                FrameEditCommand::Paste => frame.paste(),
            }

            if should_invalidate_after_frame_edit(self.command) {
                invalidate_browser_surfaces(&self.browser);
            }
        }
    }
}

cef::wrap_task! {
    struct CloseBrowserTask {
        browser: cef::Browser,
    }

    impl Task {
        fn execute(&self) {
            close_browser_resources(&self.browser);
        }
    }
}

pub struct WebView {
    browser: cef::Browser,
    focus_policy: WebFocusPolicy,
    paint_state: StdRc<RefCell<PaintState>>,
    devtools_state: StdRc<RefCell<DevToolsState>>,
    agent_runtime: StdRc<RefCell<AgentRuntime>>,
    js_dialog_backend: JsDialogBackend,
    host_bridge: Option<HostPaintBridge>,
    _devtools_observer: cef::DevToolsMessageObserver,
    _devtools_registration: Option<cef::Registration>,
    _client: cef::Client,
}

fn browser_settings() -> cef::BrowserSettings {
    cef::BrowserSettings {
        javascript_access_clipboard: cef::State::ENABLED,
        javascript_dom_paste: cef::State::ENABLED,
        windowless_frame_rate: 60,
        ..cef::BrowserSettings::default()
    }
}

impl WebView {
    pub(super) fn new_host(
        parent_view: *mut c_void,
        geometry: HostGeometry,
        view_id: ViewId,
        url: &str,
        sender: HostEventSender,
        surface_sender: SurfaceSender,
    ) -> Result<Self, Box<dyn Error>> {
        let _mtm = MainThreadMarker::new().ok_or_else(|| {
            std::io::Error::other("Hosted WebView must be created on main thread")
        })?;
        crate::macos::cef::ensure_initialized_for_host()?;
        super::register_webview();
        let mut web_view_constructed = false;

        let result = (|| {
            let target = WebViewEventTarget::host(view_id, sender.clone());
            let screen_rect = cef::Rect {
                x: geometry.screen_rect.x,
                y: geometry.screen_rect.y,
                width: geometry.screen_rect.width,
                height: geometry.screen_rect.height,
            };
            let paint_state = StdRc::new(RefCell::new(PaintState::new(
                geometry.layout,
                screen_rect,
                geometry.scale_factor,
            )));
            let host_bridge = HostPaintBridge::new(view_id, sender.clone(), surface_sender.clone());
            let render_handler = TaborRenderHandler::new(
                paint_state.clone(),
                WebViewDirtyNotifier::new(target.clone()),
                Some(host_bridge.clone()),
            );
            let mut window_info = cef::WindowInfo::default().set_as_windowless(parent_view);
            window_info.shared_texture_enabled = 1;

            let display_handler =
                TaborDisplayHandler::new(StdRc::new(RefCell::new(None)), target.clone());
            let download_handler = TaborDownloadHandler::new(
                StdRc::new(RefCell::new(AutomationState::new())),
                target.clone(),
            );
            let focus_policy = WebFocusPolicy::new();
            let focus_handler = TaborFocusHandler::new(focus_policy.clone());
            let life_span_handler = TaborLifeSpanHandler::new(target.clone());
            let js_dialog_backend = JsDialogBackend(StdRc::new(RefCell::new(HostJsDialogState {
                view_id,
                sender,
                next_dialog_id: 1,
                pending: HashMap::new(),
            })));
            let jsdialog_handler = TaborJsDialogHandler::new(js_dialog_backend.clone());
            let editable_focus_notifier = WebEditableFocusNotifier(target.clone());
            let web_state = TaborClientWebState {
                jsdialog_handler,
                test_use_default_js_dialog: false,
                editable_focus_notifier,
            };
            #[cfg(not(feature = "passkey-webauthn"))]
            let mut client = {
                let permission_handler = TaborPermissionHandler::new();
                TaborClient::new(
                    display_handler,
                    render_handler,
                    download_handler,
                    focus_handler,
                    life_span_handler,
                    web_state,
                    permission_handler,
                )
            };
            #[cfg(feature = "passkey-webauthn")]
            let mut client = TaborClient::new(
                display_handler,
                render_handler,
                download_handler,
                focus_handler,
                life_span_handler,
                web_state,
            );

            let browser_settings = browser_settings();
            let initial_url = if url.is_empty() { "about:blank" } else { url };
            let browser = cef::browser_host_create_browser_sync(
                Some(&window_info),
                Some(&mut client),
                Some(&CefString::from(initial_url)),
                Some(&browser_settings),
                None,
                None,
            )
            .ok_or_else(|| std::io::Error::other("Failed to create hosted CEF browser"))?;

            if let Some(host) = browser.host() {
                host.set_windowless_frame_rate(60);
                host.notify_screen_info_changed();
                host.was_resized();
                host.invalidate(cef::PaintElementType::VIEW);
                host.invalidate(cef::PaintElementType::POPUP);
            }

            let devtools_state = StdRc::new(RefCell::new(DevToolsState::new()));
            let mut observer = TaborDevToolsObserver::new(devtools_state.clone(), target);
            let registration = browser
                .host()
                .and_then(|host| host.add_dev_tools_message_observer(Some(&mut observer)));

            let web_view = Self {
                browser,
                focus_policy,
                paint_state,
                devtools_state,
                agent_runtime: StdRc::new(RefCell::new(AgentRuntime::default())),
                js_dialog_backend,
                host_bridge: Some(host_bridge),
                _devtools_observer: observer,
                _devtools_registration: registration,
                _client: client,
            };
            web_view_constructed = true;
            Ok(web_view)
        })();

        if result.is_err() && !web_view_constructed {
            super::unregister_webview();
        }
        result
    }

    pub fn set_visible(&mut self, visible: bool) {
        if let Some(host) = self.browser.host() {
            host.was_hidden(if visible { 0 } else { 1 });
        }
        if visible {
            invalidate_browser_surfaces(&self.browser);
        }
    }

    pub fn set_focus(&mut self, focus: bool) {
        self.focus_policy.set_host_focused(focus);
        if let Some(host) = self.browser.host() {
            host.set_focus(if focus { 1 } else { 0 });
        }
    }

    pub fn sync_editable_focus(&mut self, editable: bool) {
        self.focus_policy.set_editable_focused(editable);
        invalidate_browser_surfaces(&self.browser);
    }

    pub(super) fn update_host_geometry(&mut self, geometry: HostGeometry) {
        {
            let mut paint_state = self.paint_state.borrow_mut();
            paint_state.update_geometry(
                geometry.layout,
                cef::Rect {
                    x: geometry.screen_rect.x,
                    y: geometry.screen_rect.y,
                    width: geometry.screen_rect.width,
                    height: geometry.screen_rect.height,
                },
                geometry.scale_factor,
            );
        }
        if let Some(host) = self.browser.host() {
            host.notify_screen_info_changed();
            host.was_resized();
        }
        invalidate_browser_surfaces(&self.browser);
    }

    pub fn load_url(&mut self, url: &str) -> bool {
        let url = if url.is_empty() { "about:blank" } else { url };
        if let Some(frame) = self.browser.main_frame() {
            frame.load_url(Some(&CefString::from(url)));
            return true;
        }
        false
    }

    pub fn reload(&mut self) {
        self.browser.reload();
    }

    pub fn go_back(&mut self) {
        self.browser.go_back();
    }

    pub fn go_forward(&mut self) {
        self.browser.go_forward();
    }

    pub(super) fn host_mouse_click(
        &mut self,
        event: HostMouseEvent,
        button: HostMouseButton,
        mouse_up: bool,
        click_count: i32,
    ) {
        if !mouse_up && matches!(button, HostMouseButton::Left) {
            self.prepare_browser_focus_for_mouse_input();
        }
        let event = cef::MouseEvent { x: event.x, y: event.y, modifiers: event.modifiers };
        let button = match button {
            HostMouseButton::Left => cef::MouseButtonType::LEFT,
            HostMouseButton::Right => cef::MouseButtonType::RIGHT,
            HostMouseButton::Middle => cef::MouseButtonType::MIDDLE,
        };
        if let Some(host) = self.browser.host() {
            host.send_mouse_click_event(Some(&event), button, i32::from(mouse_up), click_count);
        }
        invalidate_browser_surfaces(&self.browser);
    }

    fn prepare_browser_focus_for_mouse_input(&mut self) {
        if !self.focus_policy.host_focused() {
            return;
        }

        // Changing the NSWindow first responder inside the active mouse press path can desync
        // AppKit/WindowServer button tracking. Arm browser focus here and defer native responder
        // restoration to the explicit editable-focus handoff path.
        self.focus_policy.arm_native_focus();
        if let Some(host) = self.browser.host() {
            host.set_focus(1);
        }
    }

    pub(super) fn host_mouse_move(&mut self, event: HostMouseEvent, mouse_leave: bool) {
        let event = cef::MouseEvent { x: event.x, y: event.y, modifiers: event.modifiers };
        if let Some(host) = self.browser.host() {
            host.send_mouse_move_event(Some(&event), i32::from(mouse_leave));
        }
    }

    pub(super) fn host_mouse_wheel(&mut self, event: HostMouseEvent, delta_x: i32, delta_y: i32) {
        let event = cef::MouseEvent { x: event.x, y: event.y, modifiers: event.modifiers };
        if let Some(host) = self.browser.host() {
            host.send_mouse_wheel_event(Some(&event), delta_x, delta_y);
        }
        invalidate_browser_surfaces(&self.browser);
    }

    pub fn handle_ime_commit(&mut self, text: &str) {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");
        let Some(host) = self.browser.host() else {
            return;
        };

        let text = CefString::from(text);
        host.ime_commit_text(Some(&text), None, text.to_string().chars().count() as i32);
        host.ime_finish_composing_text(0);
        invalidate_browser_surfaces(&self.browser);
    }

    pub fn handle_ime_preedit(&mut self, text: &str, cursor_offset: Option<(usize, usize)>) {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");
        let Some(host) = self.browser.host() else {
            return;
        };

        if text.is_empty() {
            host.ime_cancel_composition();
            invalidate_browser_surfaces(&self.browser);
            return;
        }

        let text = CefString::from(text);
        let replacement_range = None;
        let selection_range =
            cursor_offset.map(|(from, to)| cef::Range { from: from as u32, to: to as u32 });
        let underline = cef::CompositionUnderline {
            range: cef::Range { from: 0, to: text.to_string().chars().count() as u32 },
            color: 0xFFFF_FFFF,
            background_color: 0,
            thick: 0,
            style: cef::CompositionUnderlineStyle::SOLID,
            ..cef::CompositionUnderline::default()
        };

        host.ime_set_composition(
            Some(&text),
            Some(&[underline]),
            replacement_range.as_ref(),
            selection_range.as_ref(),
        );
        invalidate_browser_surfaces(&self.browser);
    }

    pub fn cancel_ime_composition(&mut self) {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");
        if let Some(host) = self.browser.host() {
            host.ime_cancel_composition();
            invalidate_browser_surfaces(&self.browser);
        }
    }

    pub(super) fn host_key_events(&mut self, events: Vec<HostKeyEvent>, invalidate_after: bool) {
        let events = events
            .into_iter()
            .map(|event| cef::KeyEvent {
                type_: match event.kind {
                    HostKeyEventKind::KeyDown => cef::KeyEventType::KEYDOWN,
                    HostKeyEventKind::KeyUp => cef::KeyEventType::KEYUP,
                    HostKeyEventKind::Char => cef::KeyEventType::CHAR,
                },
                modifiers: event.modifiers,
                windows_key_code: event.windows_key_code,
                native_key_code: event.native_key_code,
                is_system_key: 0,
                character: event.character,
                unmodified_character: event.unmodified_character,
                focus_on_editable_field: i32::from(event.focus_on_editable_field),
                ..cef::KeyEvent::default()
            })
            .collect::<Vec<_>>();
        if let Some(host) = self.browser.host() {
            for event in &events {
                host.send_key_event(Some(event));
            }
        }
        if invalidate_after {
            invalidate_browser_surfaces(&self.browser);
        }
    }

    fn eval_js_string_impl<F>(
        &mut self,
        script: &str,
        user_gesture: bool,
        deadline: Instant,
        callback: F,
    ) where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        let mut params = match cef::dictionary_value_create() {
            Some(params) => params,
            None => {
                callback(Err(devtools_error("Could not create Runtime.evaluate parameters")));
                return;
            },
        };
        dict_set_string(&mut params, "expression", script);
        dict_set_bool(&mut params, "returnByValue", true);
        dict_set_bool(&mut params, "awaitPromise", true);
        dict_set_bool(&mut params, "userGesture", user_gesture);

        let browser = self.browser.clone();
        self.devtools_execute("Runtime.evaluate", Some(params), deadline, move |result| {
            let output = result.and_then(|payload| runtime_result_to_string(&payload));
            invalidate_browser_surfaces(&browser);
            callback(output);
        });
    }

    pub(super) fn eval_js_string_impl_for_host<F>(
        &mut self,
        script: &str,
        user_gesture: bool,
        deadline: Instant,
        callback: F,
    ) where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        self.eval_js_string_impl(script, user_gesture, deadline, callback);
    }

    pub(super) fn agent_eval_js_string_impl_for_host<F>(
        &self,
        script: &str,
        user_gesture: bool,
        deadline: Instant,
        callback: F,
    ) where
        F: FnOnce(Result<Option<String>, IpcError>) + 'static,
    {
        let browser = self.browser.clone();
        let devtools_state = self.devtools_state.clone();
        let script = script.to_string();
        self.run_when_agent_ready(deadline, move |readiness| match readiness {
            Ok(()) => runtime_evaluate_with(
                &browser,
                &devtools_state,
                &script,
                user_gesture,
                deadline,
                callback,
            ),
            Err(error) => callback(Err(error)),
        });
    }

    fn run_when_agent_ready<F>(&self, deadline: Instant, callback: F)
    where
        F: FnOnce(Result<(), IpcError>) + 'static,
    {
        let waiter = AgentReadyWaiter { deadline, callback: Box::new(callback) };
        let action = self.agent_runtime.borrow_mut().enqueue(waiter);
        match action {
            AgentRuntimeQueueAction::Run(waiter) => {
                run_agent_ready_waiters(VecDeque::from([waiter]), Ok(()));
            },
            AgentRuntimeQueueAction::Wait => {},
            AgentRuntimeQueueAction::Register => dispatch_agent_runtime_registration(
                &self.browser,
                &self.devtools_state,
                &self.agent_runtime,
                deadline,
            ),
            AgentRuntimeQueueAction::Establish => dispatch_agent_runtime_establishment(
                &self.browser,
                &self.devtools_state,
                &self.agent_runtime,
                deadline,
            ),
        }
    }

    fn run_frame_edit_command(&self, command: FrameEditCommand) {
        if should_run_frame_edit_inline(
            cef::currently_on(cef::ThreadId::UI) == 1,
            super::cef_handling_send_event(),
        ) {
            let frame = self.browser.focused_frame().or_else(|| self.browser.main_frame());
            let Some(frame) = frame else {
                return;
            };

            match command {
                FrameEditCommand::Copy => frame.copy(),
                FrameEditCommand::Cut => frame.cut(),
                FrameEditCommand::Paste => frame.paste(),
            }
            if should_invalidate_after_frame_edit(command) {
                invalidate_browser_surfaces(&self.browser);
            }
        } else {
            let mut task = FrameEditTask::new(self.browser.clone(), command);
            let _ = cef::post_task(cef::ThreadId::UI, Some(&mut task));
        }
    }

    pub(super) fn host_frame_edit(&self, command: HostFrameEditCommand) {
        self.run_frame_edit_command(match command {
            HostFrameEditCommand::Copy => FrameEditCommand::Copy,
            HostFrameEditCommand::Cut => FrameEditCommand::Cut,
            HostFrameEditCommand::Paste => FrameEditCommand::Paste,
        });
    }

    pub fn devtools_command_json<F>(
        &self,
        method: &str,
        params: Option<JsonValue>,
        deadline: Instant,
        callback: F,
    ) -> Result<(), IpcError>
    where
        F: FnOnce(Result<JsonValue, IpcError>) + 'static,
    {
        let params = match params {
            None => None,
            Some(JsonValue::Null) => None,
            Some(value) => Some(json_to_cef_dictionary(&value).map_err(devtools_error)?),
        };

        self.devtools_execute_checked(method, params, deadline, callback)
    }

    pub fn renew_agent_event_capture(&self) {
        fail_expired_devtools_callbacks(&self.devtools_state, Instant::now());
        let should_enable =
            self.devtools_state.borrow_mut().renew_agent_event_capture(Instant::now());
        if should_enable && !set_agent_event_domains(&self.browser, &self.devtools_state, true) {
            self.devtools_state.borrow_mut().mark_agent_event_domains_disabled();
        }
    }

    pub fn retain_inspector_session(&self) {
        let should_enable = self.devtools_state.borrow_mut().retain_inspector_session();
        if should_enable && !set_agent_event_domains(&self.browser, &self.devtools_state, true) {
            self.devtools_state.borrow_mut().mark_agent_event_domains_disabled();
        }
    }

    pub fn release_inspector_session(&self) {
        let should_disable =
            self.devtools_state.borrow_mut().release_inspector_session(Instant::now());
        if should_disable && set_agent_event_domains(&self.browser, &self.devtools_state, false) {
            self.devtools_state.borrow_mut().mark_agent_event_domains_disabled();
        }
    }

    pub fn set_file_input_files<F>(
        &self,
        element_id: &str,
        paths: Vec<String>,
        deadline: Instant,
        callback: F,
    ) where
        F: FnOnce(Result<String, IpcError>) + 'static,
    {
        let browser = self.browser.clone();
        let state = self.devtools_state.clone();
        let callback: SharedResultCallback<String> =
            StdRc::new(RefCell::new(Some(Box::new(callback))));
        let selector = format!("[data-tabor-agent-id=\"{element_id}\"]");
        let inspect_script = format!(
            "(async () => {{\
                const id = {id};\
                const selector = {selector};\
                const el = document.querySelector(selector);\
                if (!el) throw new Error(\"element not found\");\
                el.dispatchEvent(new Event(\"input\", {{ bubbles: true }}));\
                el.dispatchEvent(new Event(\"change\", {{ bubbles: true }}));\
                return window.__taborAgent.inspect(id);\
            }})()",
            id = serde_json::to_string(element_id).unwrap(),
            selector = serde_json::to_string(&selector).unwrap(),
        );

        let callback_for_readiness = callback.clone();
        self.run_when_agent_ready(deadline, move |readiness| {
            if let Err(error) = readiness {
                finish_result_callback(&callback_for_readiness, Err(error));
                return;
            }

            let callback_for_root = callback.clone();
            let callback_for_root_dispatch = callback.clone();
            let browser_for_root = browser.clone();
            let state_for_root = state.clone();
            let root = devtools_command_json_with(
                &browser_for_root,
                &state_for_root,
                "DOM.getDocument",
                Some(serde_json::json!({ "depth": 0 })),
                deadline,
                move |result| {
                    let root_id = match result {
                        Ok(payload) => payload
                            .get("root")
                            .and_then(|root| root.get("nodeId"))
                            .and_then(JsonValue::as_u64),
                        Err(err) => {
                            finish_result_callback(&callback_for_root, Err(err));
                            return;
                        },
                    };
                    let Some(root_id) = root_id else {
                        finish_result_callback(
                            &callback_for_root,
                            Err(devtools_error("DOM.getDocument returned no root node id")),
                        );
                        return;
                    };

                    let browser = browser.clone();
                    let state = state.clone();
                    let selector = selector.clone();
                    let inspect_script = inspect_script.clone();
                    let paths = paths.clone();
                    let callback = callback_for_root.clone();
                    let browser_for_query = browser.clone();
                    let state_for_query = state.clone();
                    let query = devtools_command_json_with(
                        &browser_for_query,
                        &state_for_query,
                        "DOM.querySelector",
                        Some(serde_json::json!({
                            "nodeId": root_id,
                            "selector": selector,
                        })),
                        deadline,
                        move |result| {
                            let node_id = match result {
                                Ok(payload) => payload.get("nodeId").and_then(JsonValue::as_u64),
                                Err(err) => {
                                    finish_result_callback(&callback, Err(err));
                                    return;
                                },
                            };
                            let Some(node_id) = node_id.filter(|node_id| *node_id != 0) else {
                                finish_result_callback(
                                    &callback,
                                    Err(devtools_error("element not found")),
                                );
                                return;
                            };

                            let browser = browser.clone();
                            let state = state.clone();
                            let callback_for_set = callback.clone();
                            let browser_for_set = browser.clone();
                            let state_for_set = state.clone();
                            let set_files = devtools_command_json_with(
                                &browser_for_set,
                                &state_for_set,
                                "DOM.setFileInputFiles",
                                Some(serde_json::json!({
                                    "nodeId": node_id,
                                    "files": paths,
                                })),
                                deadline,
                                move |result| match result {
                                    Ok(_) => {
                                        let callback_for_eval = callback_for_set.clone();
                                        let browser_for_eval = browser.clone();
                                        let state_for_eval = state.clone();
                                        runtime_evaluate_with(
                                            &browser_for_eval,
                                            &state_for_eval,
                                            &inspect_script,
                                            false,
                                            deadline,
                                            move |result| match result {
                                                Ok(Some(raw)) => finish_result_callback(
                                                    &callback_for_eval,
                                                    Ok(raw),
                                                ),
                                                Ok(None) => finish_result_callback(
                                                    &callback_for_eval,
                                                    Err(devtools_error(
                                                        "Runtime.evaluate returned no payload",
                                                    )),
                                                ),
                                                Err(error) => finish_result_callback(
                                                    &callback_for_eval,
                                                    Err(error),
                                                ),
                                            },
                                        );
                                    },
                                    Err(err) => finish_result_callback(&callback_for_set, Err(err)),
                                },
                            );
                            if let Err(err) = set_files {
                                finish_result_callback(&callback, Err(err));
                            }
                        },
                    );
                    if let Err(err) = query {
                        finish_result_callback(&callback_for_root, Err(err));
                    }
                },
            );
            if let Err(error) = root {
                finish_result_callback(&callback_for_root_dispatch, Err(error));
            }
        });
    }

    pub fn show_inspector(&mut self) -> bool {
        let Some(host) = self.browser.host() else {
            return false;
        };
        host.show_dev_tools(None, None, None, None);
        true
    }

    pub(super) fn release_host_surface_lease(&mut self, lease_id: SurfaceLeaseId) {
        if let Some(host_bridge) = &self.host_bridge {
            host_bridge.release(lease_id);
        }
    }

    pub(super) fn complete_host_js_dialog(
        &mut self,
        dialog_id: u64,
        accepted: bool,
        prompt_text: Option<&str>,
    ) {
        self.js_dialog_backend.complete(dialog_id, accepted, prompt_text);
    }

    fn devtools_execute<F>(
        &self,
        method: &str,
        params: Option<cef::DictionaryValue>,
        deadline: Instant,
        callback: F,
    ) where
        F: FnOnce(Result<JsonValue, IpcError>) + 'static,
    {
        let Some(host) = self.browser.host() else {
            callback(Err(devtools_error("DevTools host unavailable")));
            return;
        };

        let method = CefString::from(method);
        let mut params = params;
        fail_expired_devtools_callbacks(&self.devtools_state, Instant::now());
        let id = match insert_scheduled_devtools_call(
            &self.devtools_state,
            Box::new(callback),
            deadline,
        ) {
            Ok(id) => id,
            Err((callback, error)) => {
                callback(Err(error));
                return;
            },
        };

        let ok = host.execute_dev_tools_method(id, Some(&method), params.as_mut());
        if ok == 0 {
            let callback = {
                let mut state = self.devtools_state.borrow_mut();
                state.pending.remove(&id).map(|pending| pending.callback)
            };
            if let Some(callback) = callback {
                callback(Err(devtools_error("DevTools method dispatch failed")));
            }
        }
    }

    fn devtools_execute_checked<F>(
        &self,
        method: &str,
        params: Option<cef::DictionaryValue>,
        deadline: Instant,
        callback: F,
    ) -> Result<(), IpcError>
    where
        F: FnOnce(Result<JsonValue, IpcError>) + 'static,
    {
        let Some(host) = self.browser.host() else {
            return Err(devtools_error("DevTools host unavailable"));
        };

        let method = CefString::from(method);
        let mut params = params;
        fail_expired_devtools_callbacks(&self.devtools_state, Instant::now());
        let id = match insert_scheduled_devtools_call(
            &self.devtools_state,
            Box::new(callback),
            deadline,
        ) {
            Ok(id) => id,
            Err((_, error)) => return Err(error),
        };

        let ok = host.execute_dev_tools_method(id, Some(&method), params.as_mut());
        if ok == 0 {
            let _ = {
                let mut state = self.devtools_state.borrow_mut();
                state.pending.remove(&id)
            };
            return Err(devtools_error("DevTools method dispatch failed"));
        }

        Ok(())
    }
}

fn agent_bootstrap_source() -> String {
    let placeholder_count =
        AGENT_BOOTSTRAP_TEMPLATE.matches(AGENT_RUNTIME_VERSION_PLACEHOLDER).count();
    assert_eq!(
        placeholder_count, 1,
        "agent bootstrap must contain exactly one runtime version placeholder"
    );
    AGENT_BOOTSTRAP_TEMPLATE.replacen(
        AGENT_RUNTIME_VERSION_PLACEHOLDER,
        &AGENT_RUNTIME_VERSION.to_string(),
        1,
    )
}

fn agent_runtime_establishment_expression() -> String {
    let bootstrap = agent_bootstrap_source();
    format!("{bootstrap};\nwindow.__taborAgent ? window.__taborAgent.version : null")
}

fn validate_agent_runtime_registration(result: &JsonValue) -> Result<(), IpcError> {
    result
        .get("identifier")
        .and_then(JsonValue::as_str)
        .filter(|identifier| !identifier.trim().is_empty())
        .map(|_| ())
        .ok_or_else(|| devtools_error("Agent runtime registration returned no identifier"))
}

fn dispatch_agent_runtime_registration(
    browser: &cef::Browser,
    devtools_state: &StdRc<RefCell<DevToolsState>>,
    agent_runtime: &StdRc<RefCell<AgentRuntime>>,
    deadline: Instant,
) {
    let browser_for_establishment = browser.clone();
    let state_for_establishment = devtools_state.clone();
    let runtime_for_result = agent_runtime.clone();
    let dispatch = devtools_command_json_with(
        browser,
        devtools_state,
        "Page.addScriptToEvaluateOnNewDocument",
        Some(serde_json::json!({
            "source": agent_bootstrap_source(),
            "runImmediately": true,
        })),
        deadline,
        move |result| {
            let registration = result
                .and_then(|result| validate_agent_runtime_registration(&result).map(|_| result));
            match registration {
                Ok(_) => {
                    runtime_for_result.borrow_mut().registration_succeeded();
                    dispatch_agent_runtime_establishment(
                        &browser_for_establishment,
                        &state_for_establishment,
                        &runtime_for_result,
                        deadline,
                    );
                },
                Err(error) => fail_agent_runtime_registration(&runtime_for_result, error),
            }
        },
    );
    if let Err(error) = dispatch {
        fail_agent_runtime_registration(agent_runtime, error);
    }
}

fn fail_agent_runtime_registration(agent_runtime: &StdRc<RefCell<AgentRuntime>>, error: IpcError) {
    let waiters = agent_runtime.borrow_mut().registration_failed();
    run_agent_ready_waiters(waiters, Err(error));
}

fn dispatch_agent_runtime_establishment(
    browser: &cef::Browser,
    devtools_state: &StdRc<RefCell<DevToolsState>>,
    agent_runtime: &StdRc<RefCell<AgentRuntime>>,
    deadline: Instant,
) {
    let runtime_for_result = agent_runtime.clone();
    runtime_evaluate_with(
        browser,
        devtools_state,
        &agent_runtime_establishment_expression(),
        false,
        deadline,
        move |result| {
            complete_agent_runtime_establishment(
                &runtime_for_result,
                validate_agent_runtime_version(result),
            );
        },
    );
}

fn validate_agent_runtime_version(
    result: Result<Option<String>, IpcError>,
) -> Result<(), IpcError> {
    let actual =
        result?.ok_or_else(|| devtools_error("Agent runtime establishment had no value"))?;
    let expected = AGENT_RUNTIME_VERSION.to_string();
    if actual == expected {
        return Ok(());
    }
    Err(devtools_error(format!(
        "Agent runtime version mismatch: expected {expected}, got {actual}"
    )))
}

fn complete_agent_runtime_establishment(
    agent_runtime: &StdRc<RefCell<AgentRuntime>>,
    result: Result<(), IpcError>,
) {
    let waiters = agent_runtime.borrow_mut().finish_establishment(result.is_ok());
    run_agent_ready_waiters(waiters, result);
}

fn run_agent_ready_waiters(waiters: VecDeque<AgentReadyWaiter>, readiness: Result<(), IpcError>) {
    for waiter in waiters {
        let result = if Instant::now() >= waiter.deadline {
            Err(IpcError::new(IpcErrorCode::Timeout, "Agent runtime readiness timed out"))
        } else {
            readiness.clone()
        };
        (waiter.callback)(result);
    }
}

fn should_run_frame_edit_inline(on_ui_thread: bool, handling_send_event: bool) -> bool {
    on_ui_thread && !handling_send_event
}

impl Drop for WebView {
    fn drop(&mut self) {
        let callbacks = self
            .devtools_state
            .borrow_mut()
            .pending
            .drain()
            .map(|(_, pending)| pending.callback)
            .collect::<Vec<_>>();
        for callback in callbacks {
            callback(Err(devtools_error("web view closed")));
        }
        if MainThreadMarker::new().is_some() {
            self.js_dialog_backend.cancel();
        }
        if cef::currently_on(cef::ThreadId::UI) == 1 {
            close_browser_resources(&self.browser);
        } else {
            let mut task = CloseBrowserTask::new(self.browser.clone());
            let _ = cef::post_task(cef::ThreadId::UI, Some(&mut task));
        }

        super::unregister_webview();
    }
}

fn devtools_command_json_with<F>(
    browser: &cef::Browser,
    state: &StdRc<RefCell<DevToolsState>>,
    method: &str,
    params: Option<JsonValue>,
    deadline: Instant,
    callback: F,
) -> Result<(), IpcError>
where
    F: FnOnce(Result<JsonValue, IpcError>) + 'static,
{
    let params = match params {
        None => None,
        Some(JsonValue::Null) => None,
        Some(value) => Some(json_to_cef_dictionary(&value).map_err(devtools_error)?),
    };

    let Some(host) = browser.host() else {
        return Err(devtools_error("DevTools host unavailable"));
    };

    let method = CefString::from(method);
    let mut params = params;
    fail_expired_devtools_callbacks(state, Instant::now());
    let id = match insert_scheduled_devtools_call(state, Box::new(callback), deadline) {
        Ok(id) => id,
        Err((_, error)) => return Err(error),
    };

    let ok = host.execute_dev_tools_method(id, Some(&method), params.as_mut());
    if ok == 0 {
        let _ = {
            let mut state = state.borrow_mut();
            state.pending.remove(&id)
        };
        return Err(devtools_error("DevTools method dispatch failed"));
    }

    Ok(())
}

fn runtime_evaluate_with<F>(
    browser: &cef::Browser,
    state: &StdRc<RefCell<DevToolsState>>,
    script: &str,
    user_gesture: bool,
    deadline: Instant,
    callback: F,
) where
    F: FnOnce(Result<Option<String>, IpcError>) + 'static,
{
    let callback: SharedResultCallback<Option<String>> =
        StdRc::new(RefCell::new(Some(Box::new(callback))));
    let callback_for_result = callback.clone();
    let dispatch = devtools_command_json_with(
        browser,
        state,
        "Runtime.evaluate",
        Some(serde_json::json!({
            "expression": script,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": user_gesture,
        })),
        deadline,
        move |result| {
            finish_result_callback(
                &callback_for_result,
                result.and_then(|payload| runtime_result_to_string(&payload)),
            );
        },
    );
    if let Err(error) = dispatch {
        finish_result_callback(&callback, Err(error));
    }
}

type SharedResultCallback<T> = StdRc<RefCell<Option<Box<dyn FnOnce(Result<T, IpcError>)>>>>;

fn finish_result_callback<T>(callback: &SharedResultCallback<T>, result: Result<T, IpcError>) {
    if let Some(callback) = callback.borrow_mut().take() {
        callback(result);
    }
}

pub(super) fn cef_mouse_button_flag(button: MouseButton) -> u32 {
    use cef::sys::cef_event_flags_t;

    match button {
        MouseButton::Left => cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0,
        MouseButton::Middle => cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0,
        MouseButton::Right => cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0,
        MouseButton::Back | MouseButton::Forward | MouseButton::Other(_) => 0,
    }
}

pub(super) fn cef_mouse_event_flags(
    modifiers: objc2_app_kit::NSEventModifierFlags,
    button_flags: u32,
) -> u32 {
    use cef::sys::cef_event_flags_t;

    let mut flags = cef_event_flags_t::EVENTFLAG_NONE;
    if modifiers.contains(objc2_app_kit::NSEventModifierFlags::Shift) {
        flags |= cef_event_flags_t::EVENTFLAG_SHIFT_DOWN;
    }
    if modifiers.contains(objc2_app_kit::NSEventModifierFlags::Control) {
        flags |= cef_event_flags_t::EVENTFLAG_CONTROL_DOWN;
    }
    if modifiers.contains(objc2_app_kit::NSEventModifierFlags::Option) {
        flags |= cef_event_flags_t::EVENTFLAG_ALT_DOWN;
    }
    if modifiers.contains(objc2_app_kit::NSEventModifierFlags::Command) {
        flags |= cef_event_flags_t::EVENTFLAG_COMMAND_DOWN;
    }

    flags.0 | button_flags
}

fn cef_event_flags(modifiers: ModifiersState, repeat: bool, location: KeyLocation) -> u32 {
    use cef::sys::cef_event_flags_t;

    let mut flags = cef_event_flags_t::EVENTFLAG_NONE;
    if modifiers.shift_key() {
        flags |= cef_event_flags_t::EVENTFLAG_SHIFT_DOWN;
    }
    if modifiers.control_key() {
        flags |= cef_event_flags_t::EVENTFLAG_CONTROL_DOWN;
    }
    if modifiers.alt_key() {
        flags |= cef_event_flags_t::EVENTFLAG_ALT_DOWN;
    }
    if modifiers.super_key() {
        flags |= cef_event_flags_t::EVENTFLAG_COMMAND_DOWN;
    }
    if repeat {
        flags |= cef_event_flags_t::EVENTFLAG_IS_REPEAT;
    }
    if matches!(location, KeyLocation::Numpad) {
        flags |= cef_event_flags_t::EVENTFLAG_IS_KEY_PAD;
    }

    flags.0
}

pub(super) fn encode_host_key_input(
    key: &KeyEvent,
    text: &str,
    modifiers: ModifiersState,
) -> (Vec<HostKeyEvent>, bool) {
    let key_without_modifiers = key.key_without_modifiers();
    let base_flags = cef_event_flags(modifiers, key.repeat, key.location);
    let windows_key_code = cef_windows_key_code_from_key(&key_without_modifiers);
    let scancode = macos_scancode_from_physical_key(key.physical_key).unwrap_or(0);
    let native_key_code = cef_native_key_code(scancode, modifiers);
    let (character, unmodified_character) =
        cef_characters_from_key_text(&key_without_modifiers, text);
    let should_send_char = character != 0 && !modifiers.super_key() && !modifiers.control_key();
    let mut events = Vec::new();
    match key.state {
        ElementState::Pressed => {
            if windows_key_code != 0 {
                events.push(HostKeyEvent {
                    kind: HostKeyEventKind::KeyDown,
                    modifiers: base_flags,
                    windows_key_code,
                    native_key_code,
                    character,
                    unmodified_character,
                    focus_on_editable_field: true,
                });
            }
            if should_send_char {
                events.push(HostKeyEvent {
                    kind: HostKeyEventKind::Char,
                    modifiers: base_flags,
                    windows_key_code: i32::from(character),
                    native_key_code,
                    character,
                    unmodified_character,
                    focus_on_editable_field: true,
                });
            }
        },
        ElementState::Released if windows_key_code != 0 => events.push(HostKeyEvent {
            kind: HostKeyEventKind::KeyUp,
            modifiers: base_flags,
            windows_key_code,
            native_key_code,
            character: 0,
            unmodified_character: 0,
            focus_on_editable_field: true,
        }),
        ElementState::Released => (),
    }
    let forwarded = windows_key_code != 0 || should_send_char;
    (events, forwarded)
}

fn cef_native_key_code(scancode: u16, modifiers: ModifiersState) -> i32 {
    let mut flags: i32 = 0;
    if modifiers.shift_key() {
        flags |= 1 << 17;
    }
    if modifiers.control_key() {
        flags |= 1 << 18;
    }
    if modifiers.alt_key() {
        flags |= 1 << 19;
    }
    if modifiers.super_key() {
        flags |= 1 << 20;
    }
    scancode as i32 | flags
}

fn cef_windows_key_code_from_key(key: &Key) -> i32 {
    match key {
        Key::Character(ch) => {
            let mut chars = ch.chars();
            let Some(c) = chars.next() else {
                return 0;
            };
            if chars.next().is_some() {
                return 0;
            }

            if c.is_ascii_alphabetic() {
                return c.to_ascii_uppercase() as i32;
            }
            if c.is_ascii_digit() {
                return c as i32;
            }

            match c {
                ' ' => 0x20,
                ';' => 0xBA,
                '=' => 0xBB,
                ',' => 0xBC,
                '-' => 0xBD,
                '.' => 0xBE,
                '/' => 0xBF,
                '`' => 0xC0,
                '[' => 0xDB,
                '\\' => 0xDC,
                ']' => 0xDD,
                '\'' => 0xDE,
                _ => 0,
            }
        },
        Key::Named(named) => match named {
            NamedKey::Backspace => 0x08,
            NamedKey::Tab => 0x09,
            NamedKey::Enter => 0x0D,
            NamedKey::Escape => 0x1B,
            NamedKey::Space => 0x20,
            NamedKey::PageUp => 0x21,
            NamedKey::PageDown => 0x22,
            NamedKey::End => 0x23,
            NamedKey::Home => 0x24,
            NamedKey::ArrowLeft => 0x25,
            NamedKey::ArrowUp => 0x26,
            NamedKey::ArrowRight => 0x27,
            NamedKey::ArrowDown => 0x28,
            NamedKey::Delete => 0x2E,
            NamedKey::Copy => 0x43,
            NamedKey::Paste => 0x56,
            NamedKey::Cut => 0x58,
            _ => 0,
        },
        _ => 0,
    }
}

fn cef_characters_from_key_text(key: &Key, text: &str) -> (u16, u16) {
    let character = first_char_u16(text);
    let unmodified = match key {
        Key::Character(ch) => first_char_u16(ch),
        _ => 0,
    };
    (character, unmodified)
}

fn first_char_u16(text: &str) -> u16 {
    let mut chars = text.chars();
    let Some(ch) = chars.next() else {
        return 0;
    };
    if chars.next().is_some() {
        return 0;
    }
    if (ch as u32) > 0xFFFF {
        return 0;
    }
    ch as u16
}

fn runtime_result_to_string(payload: &JsonValue) -> Result<Option<String>, IpcError> {
    if let Some(details) = payload.get("exceptionDetails") {
        let description = details
            .pointer("/exception/description")
            .and_then(JsonValue::as_str)
            .or_else(|| details.get("text").and_then(JsonValue::as_str))
            .unwrap_or("JavaScript evaluation failed");
        return Err(devtools_error(description));
    }

    let result = payload.get("result").unwrap_or(payload);
    if let Some(value) = result.get("value") {
        if let Some(text) = value.as_str() {
            return Ok(Some(text.to_string()));
        }
        return Ok(Some(value.to_string()));
    }

    Ok(result.get("description").and_then(JsonValue::as_str).map(str::to_string))
}

fn dict_set_string(dict: &mut cef::DictionaryValue, key: &str, value: &str) {
    let key = CefString::from(key);
    let value = CefString::from(value);
    dict.set_string(Some(&key), Some(&value));
}

fn dict_set_bool(dict: &mut cef::DictionaryValue, key: &str, value: bool) {
    let key = CefString::from(key);
    dict.set_bool(Some(&key), if value { 1 } else { 0 });
}

fn dict_set_int(dict: &mut cef::DictionaryValue, key: &str, value: i32) {
    let key = CefString::from(key);
    dict.set_int(Some(&key), value);
}

fn dict_set_double(dict: &mut cef::DictionaryValue, key: &str, value: f64) {
    let key = CefString::from(key);
    dict.set_double(Some(&key), value);
}

fn dict_set_null(dict: &mut cef::DictionaryValue, key: &str) {
    let key = CefString::from(key);
    dict.set_null(Some(&key));
}

fn json_to_cef_dictionary(value: &JsonValue) -> Result<cef::DictionaryValue, String> {
    let Some(object) = value.as_object() else {
        return Err(String::from("params must be an object"));
    };
    let mut dict = cef::dictionary_value_create()
        .ok_or_else(|| String::from("Failed to create dictionary value"))?;
    for (key, value) in object {
        set_dict_value(&mut dict, key, value)?;
    }
    Ok(dict)
}

fn set_dict_value(
    dict: &mut cef::DictionaryValue,
    key: &str,
    value: &JsonValue,
) -> Result<(), String> {
    match value {
        JsonValue::Null => {
            dict_set_null(dict, key);
        },
        JsonValue::Bool(value) => {
            dict_set_bool(dict, key, *value);
        },
        JsonValue::Number(value) => {
            if let Some(int) = value.as_i64() {
                if int >= i64::from(i32::MIN) && int <= i64::from(i32::MAX) {
                    dict_set_int(dict, key, int as i32);
                } else {
                    dict_set_double(dict, key, int as f64);
                }
            } else if let Some(uint) = value.as_u64() {
                if uint <= i64::from(i32::MAX) as u64 {
                    dict_set_int(dict, key, uint as i32);
                } else {
                    dict_set_double(dict, key, uint as f64);
                }
            } else if let Some(float) = value.as_f64() {
                dict_set_double(dict, key, float);
            } else {
                return Err(String::from("Invalid numeric parameter"));
            }
        },
        JsonValue::String(value) => {
            dict_set_string(dict, key, value);
        },
        JsonValue::Array(values) => {
            let mut list = json_to_cef_list(values)?;
            let key = CefString::from(key);
            dict.set_list(Some(&key), Some(&mut list));
        },
        JsonValue::Object(_) => {
            let mut nested = json_to_cef_dictionary(value)?;
            let key = CefString::from(key);
            dict.set_dictionary(Some(&key), Some(&mut nested));
        },
    }
    Ok(())
}

fn json_to_cef_list(values: &[JsonValue]) -> Result<cef::ListValue, String> {
    let mut list =
        cef::list_value_create().ok_or_else(|| String::from("Failed to create list value"))?;
    list.set_size(values.len());
    for (index, value) in values.iter().enumerate() {
        set_list_value(&mut list, index, value)?;
    }
    Ok(list)
}

fn set_list_value(
    list: &mut cef::ListValue,
    index: usize,
    value: &JsonValue,
) -> Result<(), String> {
    match value {
        JsonValue::Null => {
            list.set_null(index);
        },
        JsonValue::Bool(value) => {
            list.set_bool(index, if *value { 1 } else { 0 });
        },
        JsonValue::Number(value) => {
            if let Some(int) = value.as_i64() {
                if int >= i64::from(i32::MIN) && int <= i64::from(i32::MAX) {
                    list.set_int(index, int as i32);
                } else {
                    list.set_double(index, int as f64);
                }
            } else if let Some(uint) = value.as_u64() {
                if uint <= i64::from(i32::MAX) as u64 {
                    list.set_int(index, uint as i32);
                } else {
                    list.set_double(index, uint as f64);
                }
            } else if let Some(float) = value.as_f64() {
                list.set_double(index, float);
            } else {
                return Err(String::from("Invalid numeric parameter"));
            }
        },
        JsonValue::String(value) => {
            let value = CefString::from(value.as_str());
            list.set_string(index, Some(&value));
        },
        JsonValue::Array(values) => {
            let mut nested = json_to_cef_list(values)?;
            list.set_list(index, Some(&mut nested));
        },
        JsonValue::Object(_) => {
            let mut nested = json_to_cef_dictionary(value)?;
            list.set_dictionary(index, Some(&mut nested));
        },
    }
    Ok(())
}

pub(super) fn ns_view(window: &Window) -> Result<*mut AnyObject, Box<dyn Error>> {
    match window.raw_window_handle() {
        RawWindowHandle::AppKit(handle) => Ok(handle.ns_view.as_ptr() as *mut AnyObject),
        _ => Err(std::io::Error::other("WebView requires an AppKit window").into()),
    }
}

pub(super) fn ns_window(view: *mut AnyObject) -> Result<Retained<NSWindow>, Box<dyn Error>> {
    let window: Option<Retained<NSWindow>> = unsafe { msg_send![view, window] };
    window.ok_or_else(|| std::io::Error::other("WebView parent NSView has no NSWindow").into())
}

pub(super) fn cef_screen_rect(window: &Window, layout: &BrowserViewportLayout) -> cef::Rect {
    let layout_viewport = layout.viewport();
    let fallback = cef::Rect {
        x: layout_viewport.x as i32,
        y: layout_viewport.y as i32,
        width: layout_viewport.width as i32,
        height: layout_viewport.height as i32,
    };

    let Ok(view) = ns_view(window) else {
        return fallback;
    };

    unsafe {
        let view_bounds: CGRect = msg_send![view, bounds];
        let window_rect: CGRect =
            msg_send![view, convertRect: view_bounds, toView: std::ptr::null_mut::<AnyObject>()];
        let ns_window: *mut AnyObject = msg_send![view, window];
        if ns_window.is_null() {
            return fallback;
        }
        let screen_rect: CGRect = msg_send![ns_window, convertRectToScreen: window_rect];
        cef::Rect {
            x: (screen_rect.origin.x + layout_viewport.x as CGFloat) as i32,
            y: (screen_rect.origin.y + layout_viewport.y as CGFloat) as i32,
            width: layout_viewport.width as i32,
            height: layout_viewport.height as i32,
        }
    }
}

fn close_browser_resources(browser: &cef::Browser) {
    if let Some(host) = browser.host() {
        host.close_browser(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AGENT_EVENT_CAPTURE_IDLE_TIMEOUT, AGENT_RUNTIME_VERSION, AGENT_RUNTIME_VERSION_PLACEHOLDER,
        AgentReadyWaiter, AgentRuntime, AgentRuntimePhase, AgentRuntimeQueueAction,
        DEVTOOLS_CALL_TIMEOUT, DevToolsState, MAX_DEVTOOLS_EVENT_BYTES, MAX_DEVTOOLS_EVENTS,
        MAX_DEVTOOLS_EVENTS_BYTES, MAX_PENDING_DEVTOOLS_CALLS, PaintState, WebFocusPolicy,
        WebViewDirtyNotifier, agent_bootstrap_source, agent_runtime_establishment_expression,
        browser_device_scale_factor, browser_screen_point, browser_settings, cef_mouse_button_flag,
        cef_mouse_event_flags, devtools_error, devtools_timeout,
        handle_ime_composition_range_change, handle_text_selection_change,
        parse_web_editable_focus_message, run_agent_ready_waiters, runtime_result_to_string,
        scaled_browser_wheel_delta_y, should_invalidate_after_frame_edit,
        should_invalidate_after_key_input, should_run_frame_edit_inline,
        validate_agent_runtime_registration, validate_agent_runtime_version,
    };
    #[cfg(not(feature = "passkey-webauthn"))]
    use super::{
        PermissionDecision, media_access_decision, permission_decision, permission_request_result,
        should_block_media_access_request, should_log_allowed_permission_request,
    };
    use crate::ipc::IpcErrorCode;
    use crate::macos::test_support::{EnvVarGuard, env_lock};
    use cef::sys::cef_event_flags_t;
    #[cfg(not(feature = "passkey-webauthn"))]
    use cef::{
        MediaAccessPermissionTypes as MediaPermission, PermissionRequestResult,
        PermissionRequestTypes as Permission,
    };
    use objc2_app_kit::NSEventModifierFlags;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};
    use winit::event::{ElementState, MouseButton};
    use winit::window::WindowId;

    #[test]
    fn agent_runtime_registration_requires_a_nonempty_identifier() {
        for malformed in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({ "identifier": null }),
            serde_json::json!({ "identifier": "" }),
            serde_json::json!({ "identifier": "  " }),
        ] {
            assert!(validate_agent_runtime_registration(&malformed).is_err());
        }
        assert!(
            validate_agent_runtime_registration(&serde_json::json!({
                "identifier": "preload-1"
            }))
            .is_ok()
        );

        let mut runtime = AgentRuntime::default();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(matches!(
            runtime.enqueue(AgentReadyWaiter { deadline, callback: Box::new(|_| {}) }),
            AgentRuntimeQueueAction::Register
        ));
        let _ = runtime.registration_failed();
        assert_eq!(runtime.phase, AgentRuntimePhase::Unregistered);
        assert!(matches!(
            runtime.enqueue(AgentReadyWaiter { deadline, callback: Box::new(|_| {}) }),
            AgentRuntimeQueueAction::Register
        ));
    }

    #[test]
    fn agent_runtime_registration_and_establishment_drain_waiters_in_fifo_order() {
        let completed = Rc::new(RefCell::new(Vec::new()));
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut runtime = AgentRuntime::default();

        for id in [1, 2] {
            let completed = completed.clone();
            let action = runtime.enqueue(AgentReadyWaiter {
                deadline,
                callback: Box::new(move |result| {
                    completed.borrow_mut().push((id, result.map_err(|error| error.code)));
                }),
            });
            if id == 1 {
                assert!(matches!(action, AgentRuntimeQueueAction::Register));
            } else {
                assert!(matches!(action, AgentRuntimeQueueAction::Wait));
            }
        }

        runtime.registration_succeeded();
        assert_eq!(runtime.phase, AgentRuntimePhase::Establishing);
        let waiters = runtime.finish_establishment(true);
        run_agent_ready_waiters(waiters, Ok(()));
        assert_eq!(runtime.phase, AgentRuntimePhase::Ready);
        assert_eq!(*completed.borrow(), vec![(1, Ok(())), (2, Ok(()))]);
    }

    #[test]
    fn agent_runtime_establishment_failure_retries_without_duplicate_registration() {
        let failed = Rc::new(RefCell::new(None));
        let failed_result = failed.clone();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut runtime = AgentRuntime::default();
        assert!(matches!(
            runtime.enqueue(AgentReadyWaiter {
                deadline,
                callback: Box::new(move |result| {
                    *failed_result.borrow_mut() = Some(result.map_err(|error| error.code));
                }),
            }),
            AgentRuntimeQueueAction::Register
        ));
        runtime.registration_succeeded();
        let waiters = runtime.finish_establishment(false);
        run_agent_ready_waiters(waiters, Err(devtools_error("establishment failed")));
        assert_eq!(runtime.phase, AgentRuntimePhase::Registered);
        assert_eq!(*failed.borrow(), Some(Err(IpcErrorCode::Internal)));
        let retried = Rc::new(RefCell::new(None));
        let retried_result = retried.clone();
        assert!(matches!(
            runtime.enqueue(AgentReadyWaiter {
                deadline,
                callback: Box::new(move |result| {
                    *retried_result.borrow_mut() = Some(result.map_err(|error| error.code));
                }),
            }),
            AgentRuntimeQueueAction::Establish
        ));
        assert_eq!(runtime.phase, AgentRuntimePhase::Establishing);
        let waiters = runtime.finish_establishment(true);
        run_agent_ready_waiters(waiters, Ok(()));
        assert_eq!(runtime.phase, AgentRuntimePhase::Ready);
        assert_eq!(*retried.borrow(), Some(Ok(())));
    }

    #[test]
    fn agent_runtime_does_not_run_expired_waiter_after_registration() {
        let outcome = Rc::new(RefCell::new(None));
        let outcome_for_callback = outcome.clone();
        let mut runtime = AgentRuntime::default();
        let action = runtime.enqueue(AgentReadyWaiter {
            deadline: Instant::now().checked_sub(Duration::from_millis(1)).unwrap(),
            callback: Box::new(move |result| {
                *outcome_for_callback.borrow_mut() = Some(result.map_err(|error| error.code));
            }),
        });
        assert!(matches!(action, AgentRuntimeQueueAction::Register));

        runtime.registration_succeeded();
        let waiters = runtime.finish_establishment(true);
        run_agent_ready_waiters(waiters, Ok(()));
        assert_eq!(*outcome.borrow(), Some(Err(IpcErrorCode::Timeout)));
    }

    #[test]
    fn agent_runtime_establishment_rejects_wrong_version() {
        let expected = AGENT_RUNTIME_VERSION.to_string();
        assert!(validate_agent_runtime_version(Ok(Some(expected.clone()))).is_ok());

        let error = validate_agent_runtime_version(Ok(Some(format!("{expected}0")))).unwrap_err();
        assert_eq!(error.code, IpcErrorCode::Internal);
        assert!(error.message.contains("version mismatch"));

        let bootstrap = agent_bootstrap_source();
        let expression = agent_runtime_establishment_expression();
        assert!(!expression.contains(AGENT_RUNTIME_VERSION_PLACEHOLDER));
        assert!(bootstrap.contains(&format!("const VERSION = {expected};")));
        assert!(expression.starts_with(&format!("{bootstrap};\n")));
        assert!(expression.ends_with("window.__taborAgent ? window.__taborAgent.version : null"));
    }

    #[test]
    fn agent_runtime_exception_payload_is_an_error() {
        let error = runtime_result_to_string(&serde_json::json!({
            "exceptionDetails": {
                "text": "Uncaught",
                "exception": { "description": "ReferenceError: missing is not defined" }
            }
        }))
        .unwrap_err();

        assert_eq!(error.code, IpcErrorCode::Internal);
        assert_eq!(error.message, "ReferenceError: missing is not defined");
    }

    #[test]
    fn devtools_event_stress_keeps_retention_bounded() {
        let mut state = DevToolsState::new();

        for request_id in 0..1_000_000 {
            state.push_event(format!(
                r#"{{"method":"Network.requestWillBeSent","params":{{"requestId":"{request_id}","request":{{"url":"chrome-extension://invalid/"}}}}}}"#
            ));
        }

        assert_eq!(state.events.len(), MAX_DEVTOOLS_EVENTS);
        assert!(state.retained_event_bytes <= MAX_DEVTOOLS_EVENTS_BYTES);
        assert_eq!(state.latest_event_id(), 1_000_000);
    }

    #[test]
    fn devtools_event_retention_is_bounded_by_bytes() {
        let mut state = DevToolsState::new();
        let payload = "x".repeat(64 * 1024);

        for _ in 0..MAX_DEVTOOLS_EVENTS {
            state.push_event(payload.clone());
        }

        assert!(state.events.len() < MAX_DEVTOOLS_EVENTS);
        assert!(state.retained_event_bytes <= MAX_DEVTOOLS_EVENTS_BYTES);
        assert_eq!(
            state.retained_event_bytes,
            state.events.iter().map(|event| event.payload.len()).sum::<usize>()
        );
    }

    #[test]
    fn oversized_devtools_event_params_are_replaced_with_metadata() {
        let mut state = DevToolsState::new();
        let params = serde_json::json!({ "data": "x".repeat(MAX_DEVTOOLS_EVENT_BYTES) });

        state.record_event("Runtime.consoleAPICalled", Some(&params));

        let (events, _) = state.events_since(0, 1);
        let event: serde_json::Value =
            serde_json::from_str(&events[0]).expect("valid retained event");
        assert_eq!(
            event.pointer("/params/taborTruncated").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        let original_bytes = event
            .pointer("/params/originalBytes")
            .and_then(serde_json::Value::as_u64)
            .expect("truncated event records its original size");
        assert!(original_bytes > MAX_DEVTOOLS_EVENT_BYTES as u64);
    }

    #[test]
    fn pending_devtools_calls_are_capped_and_expire() {
        let mut state = DevToolsState::new();
        let now = Instant::now();

        for _ in 0..MAX_PENDING_DEVTOOLS_CALLS {
            assert!(state.insert_pending(Box::new(|_| {}), now, DEVTOOLS_CALL_TIMEOUT).is_ok());
        }
        assert!(state.insert_pending(Box::new(|_| {}), now, DEVTOOLS_CALL_TIMEOUT).is_err());

        let timeout_result = Rc::new(RefCell::new(None));
        let timeout_result_for_callback = Rc::clone(&timeout_result);
        let started_at = now.checked_sub(DEVTOOLS_CALL_TIMEOUT).expect("valid test instant");
        state.pending.clear();
        assert!(
            state
                .insert_pending(
                    Box::new(move |result| *timeout_result_for_callback.borrow_mut() = Some(result)),
                    started_at,
                    DEVTOOLS_CALL_TIMEOUT,
                )
                .is_ok()
        );

        let callbacks = state.take_expired_callbacks(now);
        assert_eq!(callbacks.len(), 1);
        assert!(state.pending.is_empty());
        for callback in callbacks {
            callback(Err(devtools_timeout()));
        }
        assert_eq!(
            timeout_result
                .borrow()
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .map(|error| error.message.as_str()),
            Some("DevTools method timed out")
        );
    }

    #[test]
    fn agent_event_capture_expires_but_inspector_session_holds_it() {
        let mut state = DevToolsState::new();
        let now = Instant::now();

        assert_eq!(state.should_record_events(now), (false, false));
        assert!(state.renew_agent_event_capture(now));
        assert_eq!(state.should_record_events(now), (true, false));
        assert_eq!(
            state.should_record_events(now + AGENT_EVENT_CAPTURE_IDLE_TIMEOUT),
            (false, true)
        );
        state.mark_agent_event_domains_disabled();

        assert!(state.retain_inspector_session());
        assert_eq!(
            state.should_record_events(now + AGENT_EVENT_CAPTURE_IDLE_TIMEOUT),
            (true, false)
        );
        assert!(state.release_inspector_session(now + AGENT_EVENT_CAPTURE_IDLE_TIMEOUT));
    }

    #[test]
    fn browser_settings_enable_javascript_clipboard_access() {
        let settings = browser_settings();
        assert_eq!(settings.javascript_access_clipboard, cef::State::ENABLED);
        assert_eq!(settings.javascript_dom_paste, cef::State::ENABLED);
    }

    #[test]
    fn frame_edit_commands_defer_during_send_event() {
        assert!(should_run_frame_edit_inline(true, false));
        assert!(!should_run_frame_edit_inline(true, true));
        assert!(!should_run_frame_edit_inline(false, false));
    }

    #[test]
    fn key_presses_request_repaint_but_key_releases_do_not() {
        assert!(should_invalidate_after_key_input(ElementState::Pressed));
        assert!(!should_invalidate_after_key_input(ElementState::Released));
    }

    #[test]
    fn frame_edit_repaint_only_for_mutating_commands() {
        assert!(!should_invalidate_after_frame_edit(super::FrameEditCommand::Copy));
        assert!(should_invalidate_after_frame_edit(super::FrameEditCommand::Cut));
        assert!(should_invalidate_after_frame_edit(super::FrameEditCommand::Paste));
    }

    #[test]
    fn web_focus_policy_blocks_unfocused_and_unarmed_browser_focus() {
        let policy = WebFocusPolicy::new();
        assert!(!policy.allows_browser_focus());

        policy.set_host_focused(true);
        assert!(!policy.allows_browser_focus());
    }

    #[test]
    fn web_focus_policy_allows_armed_focus_before_editable_sync() {
        let policy = WebFocusPolicy::new();
        policy.set_host_focused(true);
        policy.arm_native_focus();
        assert!(policy.allows_browser_focus());

        policy.disarm_native_focus();
        assert!(!policy.allows_browser_focus());
    }

    #[test]
    fn web_focus_policy_allows_editable_focus_until_blur() {
        let policy = WebFocusPolicy::new();
        policy.set_host_focused(true);
        policy.set_editable_focused(true);
        assert!(policy.allows_browser_focus());

        policy.disarm_native_focus();
        assert!(policy.allows_browser_focus());

        policy.set_editable_focused(false);
        assert!(!policy.allows_browser_focus());
    }

    #[test]
    fn browser_device_scale_factor_uses_window_scale_factor() {
        assert_eq!(browser_device_scale_factor(2.0), 2.0);
        assert_eq!(browser_device_scale_factor(1.5), 1.5);
        assert!(browser_device_scale_factor(0.0) > 0.0);
    }

    #[test]
    fn paint_state_tracks_scale_factor_updates() {
        let layout = crate::display::browser_layout::BrowserViewportLayout::normal(
            crate::display::browser_layout::BrowserViewportRect {
                x: 0,
                y: 0,
                width: 900,
                height: 600,
            },
            900,
        );
        let mut paint_state =
            PaintState::new(layout.clone(), cef::Rect { x: 0, y: 0, width: 900, height: 600 }, 2.0);
        assert_eq!(paint_state.scale_factor, 2.0);

        paint_state.update_geometry(
            layout,
            cef::Rect { x: 10, y: 20, width: 900, height: 600 },
            1.5,
        );

        assert_eq!(paint_state.scale_factor, 1.5);
        assert_eq!(paint_state.screen_rect.x, 10);
        assert_eq!(paint_state.screen_rect.y, 20);
    }

    #[test]
    fn scaled_browser_wheel_delta_y_matches_visible_column_count() {
        let normal = crate::display::browser_layout::BrowserViewportLayout::normal(
            crate::display::browser_layout::BrowserViewportRect {
                x: 0,
                y: 0,
                width: 1100,
                height: 708,
            },
            900,
        );
        assert_eq!(scaled_browser_wheel_delta_y(&normal, 48.0), 48.0);

        let folded = crate::display::browser_layout::BrowserViewportLayout::new(
            &crate::display::SizeInfo::new(1100.0, 708.0, 1.0, 1.0, 0.0, 0.0, 0.0, false),
            1.0,
            crate::display::browser_layout::BrowserViewMode::MultiColumn,
            &crate::config::browser::MultiColumnBrowserConfig { target_width_px: 400 },
            None,
            None,
        );
        assert_eq!(folded.column_count(), 2);
        assert_eq!(scaled_browser_wheel_delta_y(&folded, 48.0), 96.0);
    }

    #[test]
    fn browser_screen_point_uses_folded_visual_coordinates() {
        let folded = crate::display::browser_layout::BrowserViewportLayout::new(
            &crate::display::SizeInfo::new(1950.0, 600.0, 1.0, 1.0, 0.0, 0.0, 0.0, false),
            1.0,
            crate::display::browser_layout::BrowserViewMode::MultiColumn,
            &crate::config::browser::MultiColumnBrowserConfig::default(),
            None,
            None,
        );
        let screen_rect = cef::Rect { x: 500, y: 200, width: 1950, height: 600 };

        assert_eq!(folded.column_count(), 2);
        assert_eq!(browser_screen_point(&folded, screen_rect, 17, 745), Some((1492, 345)));
    }

    #[test]
    fn mouse_button_flags_match_cef_button_state_bits() {
        assert_eq!(
            cef_mouse_button_flag(MouseButton::Left),
            cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0
        );
        assert_eq!(
            cef_mouse_button_flag(MouseButton::Middle),
            cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0
        );
        assert_eq!(
            cef_mouse_button_flag(MouseButton::Right),
            cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0
        );
        assert_eq!(cef_mouse_button_flag(MouseButton::Back), 0);
    }

    #[test]
    fn mouse_event_flags_include_keyboard_modifiers_and_pressed_buttons() {
        let modifiers = NSEventModifierFlags::Shift | NSEventModifierFlags::Command;
        let button_flags =
            cef_mouse_button_flag(MouseButton::Left) | cef_mouse_button_flag(MouseButton::Right);

        let flags = cef_mouse_event_flags(modifiers, button_flags);

        assert_ne!(flags & cef_event_flags_t::EVENTFLAG_SHIFT_DOWN.0, 0);
        assert_ne!(flags & cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0, 0);
        assert_ne!(flags & cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0, 0);
        assert_ne!(flags & cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0, 0);
        assert_eq!(flags & cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0, 0);
    }

    fn test_dirty_notifier(
        window_id: WindowId,
        tab_id: crate::tabs::TabId,
    ) -> (WebViewDirtyNotifier, std::rc::Rc<RefCell<Vec<crate::event::Event>>>) {
        WebViewDirtyNotifier::recorder(window_id, tab_id)
    }

    #[test]
    fn text_selection_changes_emit_webview_dirty() {
        let window_id = WindowId::dummy();
        let tab_id = crate::tabs::TabId::new(7, 11);
        let (dirty_notifier, events) = test_dirty_notifier(window_id, tab_id);

        handle_text_selection_change(
            None,
            &dirty_notifier,
            None,
            Some(&cef::Range { from: 0, to: 0 }),
        );

        let events = events.borrow();
        assert_eq!(events.len(), 1, "expected a dirty event after text selection changes");
        assert_eq!(events[0].window_id(), Some(window_id));
        assert_eq!(events[0].tab_id(), Some(tab_id));
        assert!(matches!(events[0].payload(), crate::event::EventType::WebViewDirty));
    }

    #[test]
    fn ime_composition_changes_emit_webview_dirty() {
        let window_id = WindowId::dummy();
        let tab_id = crate::tabs::TabId::new(7, 11);
        let (dirty_notifier, events) = test_dirty_notifier(window_id, tab_id);

        handle_ime_composition_range_change(
            None,
            &dirty_notifier,
            Some(&cef::Range { from: 0, to: 0 }),
            None,
        );

        let events = events.borrow();
        assert_eq!(events.len(), 1, "expected a dirty event after IME composition changes");
        assert_eq!(events[0].window_id(), Some(window_id));
        assert_eq!(events[0].tab_id(), Some(tab_id));
        assert!(matches!(events[0].payload(), crate::event::EventType::WebViewDirty));
    }

    #[test]
    fn web_editable_focus_message_accepts_renderer_bool_payload() {
        assert_eq!(
            parse_web_editable_focus_message(
                cef::ProcessId::RENDERER,
                super::super::cef::WEB_EDITABLE_FOCUS_MESSAGE_NAME,
                Some(true),
            ),
            Some(true)
        );
    }

    #[test]
    fn web_editable_focus_message_ignores_wrong_source_name_and_payload() {
        assert_eq!(
            parse_web_editable_focus_message(
                cef::ProcessId::BROWSER,
                super::super::cef::WEB_EDITABLE_FOCUS_MESSAGE_NAME,
                Some(true),
            ),
            None
        );
        assert_eq!(
            parse_web_editable_focus_message(cef::ProcessId::RENDERER, "other.message", Some(true)),
            None
        );
        assert_eq!(
            parse_web_editable_focus_message(
                cef::ProcessId::RENDERER,
                super::super::cef::WEB_EDITABLE_FOCUS_MESSAGE_NAME,
                None,
            ),
            None
        );
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_allows_known_safe_permissions() {
        let permissions = Permission::CAMERA_STREAM.get_raw() | Permission::MIC_STREAM.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert!(should_log_allowed_permission_request(permissions));
        assert_eq!(permission_request_result(permissions), PermissionRequestResult::ACCEPT);
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn media_access_policy_allows_device_audio_capture() {
        let permissions = MediaPermission::DEVICE_AUDIO_CAPTURE.get_raw();
        assert_eq!(media_access_decision(permissions), PermissionDecision::Allow);
        assert!(!should_block_media_access_request(permissions));
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn media_access_policy_allows_combined_device_capture() {
        let permissions = MediaPermission::DEVICE_AUDIO_CAPTURE.get_raw()
            | MediaPermission::DEVICE_VIDEO_CAPTURE.get_raw();
        assert_eq!(media_access_decision(permissions), PermissionDecision::Allow);
        assert!(!should_block_media_access_request(permissions));
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_denies_blocked_permissions() {
        let permissions = Permission::AR_SESSION.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Deny);
        assert_eq!(permission_decision(permissions), PermissionDecision::Deny);
        assert_eq!(permission_request_result(permissions), PermissionRequestResult::DENY);
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_denies_unknown_permissions() {
        let unknown_bit = 1_u32 << 30;
        assert_eq!(permission_decision(unknown_bit), PermissionDecision::Deny);
        assert_eq!(permission_decision(unknown_bit), PermissionDecision::Deny);
        assert_eq!(permission_request_result(unknown_bit), PermissionRequestResult::DENY);
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn media_access_policy_denies_desktop_capture() {
        let permissions = MediaPermission::DESKTOP_AUDIO_CAPTURE.get_raw();
        assert_eq!(media_access_decision(permissions), PermissionDecision::Deny);
        assert!(should_block_media_access_request(permissions));
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_denies_empty_permissions() {
        assert_eq!(permission_decision(0), PermissionDecision::Deny);
        assert_eq!(permission_decision(0), PermissionDecision::Deny);
        assert_eq!(permission_request_result(0), PermissionRequestResult::DENY);
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_allows_local_network_permission() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let _distribution = EnvVarGuard::unset("TABOR_DISTRIBUTION_CHANNEL");
        let permissions = Permission::LOCAL_NETWORK.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert_eq!(permission_request_result(permissions), PermissionRequestResult::ACCEPT);
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn mac_app_store_permission_policy_denies_local_network_permission() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "mac_app_store");
        let permissions = Permission::LOCAL_NETWORK.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Deny);
        assert_eq!(permission_request_result(permissions), PermissionRequestResult::DENY);
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_allows_clipboard_permission() {
        let permissions = Permission::CLIPBOARD.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert!(!should_log_allowed_permission_request(permissions));
        assert_eq!(permission_request_result(permissions), PermissionRequestResult::ACCEPT);
    }
}
