use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc as StdRc;

use cef::{
    CefString, Client, DevToolsMessageObserver, DisplayHandler, DownloadHandler, FocusHandler,
    ImplBeforeDownloadCallback, ImplBrowser, ImplBrowserHost, ImplClient,
    ImplDevToolsMessageObserver, ImplDictionaryValue, ImplDisplayHandler, ImplDownloadHandler,
    ImplDownloadItem, ImplFocusHandler, ImplFrame, ImplListValue, ImplMediaAccessCallback,
    ImplPermissionHandler, ImplPermissionPromptCallback, ImplProcessMessage, ImplRenderHandler,
    ImplTask, PermissionHandler, PermissionRequestResult, RenderHandler, Task, WrapClient,
    WrapDevToolsMessageObserver, WrapDisplayHandler, WrapDownloadHandler, WrapFocusHandler,
    WrapPermissionHandler, WrapRenderHandler, WrapTask, rc::Rc,
};
use log::debug;
use objc2::encode::{Encode, Encoding};
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, msg_send};
use serde_json::{Map as JsonMap, Value as JsonValue};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseButton};
use winit::event_loop::EventLoopProxy;
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::raw_window_handle::RawWindowHandle;
use winit::window::WindowId;

use super::keycodes::macos_scancode_from_physical_key;
use super::webview::{
    WebAccelerationInfo, WebAccelerationState, WebFrameDeliveryMode, WebPopupSurfaceRef,
    WebSurfaceRef,
};
use crate::display::SizeInfo;
use crate::display::browser_layout::BrowserViewportLayout;
use crate::display::window::Window;
use crate::event::Event;
use crate::ipc::{AgentDownload, WebNetworkEntry};
use crate::tabs::TabId;
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

type DevToolsCallback = Box<dyn FnOnce(Result<JsonValue, String>)>;

const MAX_DEVTOOLS_EVENTS: usize = 2048;

struct DevToolsEvent {
    id: u64,
    payload: String,
}

struct DevToolsState {
    next_message_id: i32,
    pending: HashMap<i32, DevToolsCallback>,
    entries: Vec<WebNetworkEntry>,
    index: HashMap<String, usize>,
    events: VecDeque<DevToolsEvent>,
    next_event_id: u64,
}

struct AutomationState {
    downloads: HashMap<u32, AgentDownload>,
    download_order: Vec<u32>,
    download_dir: PathBuf,
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

    fn as_public_ref(&self) -> WebSurfaceRef {
        WebSurfaceRef {
            io_surface: self.io_surface.as_ptr(),
            width: self.width,
            height: self.height,
            format: self.format,
        }
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

    fn acceleration_info(&self) -> WebAccelerationInfo {
        WebAccelerationInfo {
            state: self.acceleration_state,
            frame_delivery_mode: WebFrameDeliveryMode::CefInternal,
            main_surface_width: self.main.as_ref().map(|surface| surface.width),
            main_surface_height: self.main.as_ref().map(|surface| surface.height),
            popup_surface_width: self.popup.surface.as_ref().map(|surface| surface.width),
            popup_surface_height: self.popup.surface.as_ref().map(|surface| surface.height),
        }
    }
}

#[derive(Clone)]
struct WebViewDirtyNotifier {
    window_id: WindowId,
    tab_id: TabId,
    sender: WebViewDirtySender,
}

#[derive(Clone)]
enum WebViewDirtySender {
    Proxy(EventLoopProxy<Event>),
    #[cfg(test)]
    Recorder(StdRc<RefCell<Vec<Event>>>),
}

impl WebViewDirtyNotifier {
    fn new(proxy: EventLoopProxy<Event>, window_id: WindowId, tab_id: TabId) -> Self {
        Self { window_id, tab_id, sender: WebViewDirtySender::Proxy(proxy) }
    }

    fn event(&self) -> Event {
        Event::for_tab(crate::event::EventType::WebViewDirty, self.window_id, self.tab_id)
    }

    fn send(&self) {
        let event = self.event();
        match &self.sender {
            WebViewDirtySender::Proxy(proxy) => {
                let _ = proxy.send_event(event);
            },
            #[cfg(test)]
            WebViewDirtySender::Recorder(events) => {
                events.borrow_mut().push(event);
            },
        }
    }

    #[cfg(test)]
    fn recorder(window_id: WindowId, tab_id: TabId) -> (Self, StdRc<RefCell<Vec<Event>>>) {
        let events = StdRc::new(RefCell::new(Vec::new()));
        let notifier =
            Self { window_id, tab_id, sender: WebViewDirtySender::Recorder(events.clone()) };
        (notifier, events)
    }
}

#[derive(Clone)]
struct WebEditableFocusNotifier {
    window_id: WindowId,
    tab_id: TabId,
    proxy: EventLoopProxy<Event>,
}

impl WebEditableFocusNotifier {
    fn new(proxy: EventLoopProxy<Event>, window_id: WindowId, tab_id: TabId) -> Self {
        Self { window_id, tab_id, proxy }
    }

    fn send(&self, editable: bool) {
        let event = Event::for_tab(
            crate::event::EventType::WebEditableFocus { editable },
            self.window_id,
            self.tab_id,
        );
        let _ = self.proxy.send_event(event);
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

    fn editable_focused(&self) -> bool {
        self.state.borrow().editable_focused
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
    layout: BrowserViewportLayout,
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

fn scaled_browser_wheel_delta_y(layout: BrowserViewportLayout, delta_y: f64) -> f64 {
    delta_y * layout.column_count().max(1) as f64
}

fn invalidate_browser_surfaces(browser: &cef::Browser) {
    let Some(host) = browser.host() else {
        return;
    };

    host.invalidate(cef::PaintElementType::VIEW);
    host.invalidate(cef::PaintElementType::POPUP);
}

fn should_invalidate_after_key_input(state: ElementState) -> bool {
    matches!(state, ElementState::Pressed)
}

fn should_invalidate_after_frame_edit(command: FrameEditCommand) -> bool {
    !matches!(command, FrameEditCommand::Copy)
}

impl AutomationState {
    fn new() -> Self {
        let base_dir =
            home::home_dir().map(|path| path.join("Downloads")).unwrap_or_else(std::env::temp_dir);
        let download_dir = base_dir.join("Tabor");
        let _ = fs::create_dir_all(&download_dir);
        Self { downloads: HashMap::new(), download_order: Vec::new(), download_dir }
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
            entries: Vec::new(),
            index: HashMap::new(),
            events: VecDeque::new(),
            next_event_id: 1,
        }
    }

    fn next_id(&mut self) -> i32 {
        let id = self.next_message_id;
        self.next_message_id += 1;
        id
    }

    fn record_event(&mut self, method: &str, params: Option<&JsonValue>) {
        let mut object = JsonMap::new();
        object.insert("method".to_string(), JsonValue::String(method.to_string()));
        if let Some(params) = params {
            object.insert("params".to_string(), params.clone());
        }
        let payload = JsonValue::Object(object).to_string();
        self.push_event(payload);
    }

    fn push_event(&mut self, payload: String) {
        let id = self.next_event_id;
        self.next_event_id = self.next_event_id.saturating_add(1);
        self.events.push_back(DevToolsEvent { id, payload });
        while self.events.len() > MAX_DEVTOOLS_EVENTS {
            self.events.pop_front();
        }
    }

    fn latest_event_id(&self) -> u64 {
        self.next_event_id.saturating_sub(1)
    }

    fn events_since(&self, last_id: u64, max: usize) -> (Vec<String>, u64) {
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

    fn update_network_state(&mut self, method: &str, params: Option<&JsonValue>) {
        match method {
            "Network.requestWillBeSent" => {
                let request_id = params
                    .and_then(|p| p.get("requestId"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());
                let url = params
                    .and_then(|p| p.get("request"))
                    .and_then(|r| r.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if url.is_empty() {
                    return;
                }
                let method_name = params
                    .and_then(|p| p.get("request"))
                    .and_then(|r| r.get("method"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());
                let resource_type = params
                    .and_then(|p| p.get("type"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());
                let timestamp = params.and_then(|p| p.get("timestamp")).and_then(|v| v.as_f64());

                let request_id = request_id.unwrap_or_else(|| url.clone());
                if self.index.contains_key(&request_id) {
                    return;
                }
                let entry = WebNetworkEntry {
                    request_id: request_id.clone(),
                    url,
                    method: method_name,
                    status: None,
                    resource_type,
                    start_time: timestamp,
                    end_time: None,
                    error_text: None,
                };
                self.entries.push(entry);
                self.index.insert(request_id, self.entries.len() - 1);
            },
            "Network.responseReceived" => {
                let request_id = params
                    .and_then(|p| p.get("requestId"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());
                let Some(request_id) = request_id else {
                    return;
                };
                let status = params
                    .and_then(|p| p.get("response"))
                    .and_then(|r| r.get("status"))
                    .and_then(|v| v.as_f64())
                    .map(|v| v as u16);
                let resource_type = params
                    .and_then(|p| p.get("type"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());
                let timestamp = params.and_then(|p| p.get("timestamp")).and_then(|v| v.as_f64());
                let url = params
                    .and_then(|p| p.get("response"))
                    .and_then(|r| r.get("url"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());

                let index = match self.index.get(&request_id) {
                    Some(index) => *index,
                    None => {
                        let entry = WebNetworkEntry {
                            request_id: request_id.clone(),
                            url: url.unwrap_or_default(),
                            method: None,
                            status,
                            resource_type,
                            start_time: None,
                            end_time: timestamp,
                            error_text: None,
                        };
                        self.entries.push(entry);
                        self.index.insert(request_id, self.entries.len() - 1);
                        return;
                    },
                };

                let entry = &mut self.entries[index];
                entry.status = status.or(entry.status);
                entry.resource_type = resource_type.or_else(|| entry.resource_type.clone());
                entry.end_time = timestamp.or(entry.end_time);
                if let Some(url) = url {
                    entry.url = url;
                }
            },
            "Network.loadingFinished" => {
                let request_id = params
                    .and_then(|p| p.get("requestId"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());
                let Some(request_id) = request_id else {
                    return;
                };
                let timestamp = params.and_then(|p| p.get("timestamp")).and_then(|v| v.as_f64());
                if let Some(index) = self.index.get(&request_id).copied() {
                    self.entries[index].end_time = timestamp.or(self.entries[index].end_time);
                }
            },
            "Network.loadingFailed" => {
                let request_id = params
                    .and_then(|p| p.get("requestId"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());
                let Some(request_id) = request_id else {
                    return;
                };
                let error_text = params
                    .and_then(|p| p.get("errorText"))
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string());
                let timestamp = params.and_then(|p| p.get("timestamp")).and_then(|v| v.as_f64());
                if let Some(index) = self.index.get(&request_id).copied() {
                    self.entries[index].error_text = error_text;
                    self.entries[index].end_time = timestamp.or(self.entries[index].end_time);
                }
            },
            _ => {},
        }
    }
}

cef::wrap_dev_tools_message_observer! {
    struct TaborDevToolsObserver {
        state: StdRc<RefCell<DevToolsState>>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut cef::Browser>,
            message_id: i32,
            success: i32,
            result: Option<&[u8]>,
        ) {
            let callback = {
                let mut state = self.state.borrow_mut();
                state.pending.remove(&message_id)
            };

            let Some(callback) = callback else {
                return;
            };

            if success == 0 {
                callback(Err(String::from("DevTools method failed")));
                return;
            }

            let payload = result
                .and_then(|bytes| serde_json::from_slice::<JsonValue>(bytes).ok())
                .unwrap_or(JsonValue::Null);
            callback(Ok(payload));
        }

        fn on_dev_tools_event(
            &self,
            _browser: Option<&mut cef::Browser>,
            method: Option<&cef::CefString>,
            params: Option<&[u8]>,
        ) {
            let Some(method) = method else {
                return;
            };
            let method = method.to_string();
            if method.is_empty() {
                return;
            }
            let params = params.and_then(|bytes| serde_json::from_slice::<JsonValue>(bytes).ok());
            let mut state = self.state.borrow_mut();
            state.record_event(&method, params.as_ref());
            state.update_network_state(&method, params.as_ref());
        }
    }
}

cef::wrap_display_handler! {
    struct TaborDisplayHandler {
        title: StdRc<RefCell<Option<String>>>,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut cef::Browser>, title: Option<&cef::CefString>) {
            if let Some(title) = title {
                *self.title.borrow_mut() = Some(title.to_string());
            }
        }
    }
}

cef::wrap_render_handler! {
    struct TaborRenderHandler {
        paint_state: StdRc<RefCell<PaintState>>,
        dirty_notifier: WebViewDirtyNotifier,
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
                    paint_state.layout,
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
                paint_state.fail(
                    "Received CPU paint callback before accelerated rendering became ready",
                );
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
                        paint_state.fail("Accelerated paint callback was missing IOSurface info");
                    }
                }
                return;
            };

            let surface = match AcceleratedSurface::from_info(info) {
                Ok(surface) => surface,
                Err(err) => {
                    let mut paint_state = self.paint_state.borrow_mut();
                    if type_ == cef::PaintElementType::VIEW {
                        paint_state.fail(err);
                    } else if type_ == cef::PaintElementType::POPUP {
                        paint_state.clear_popup_surface();
                    }
                    return;
                },
            };

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
            let _ = fs::create_dir_all(path.parent().unwrap_or(&state.download_dir));
            let download_path = CefString::from(path.to_string_lossy().as_ref());
            callback.cont(Some(&download_path), 0);
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
        | Permission::LOCAL_NETWORK_ACCESS.get_raw()
}

#[cfg(not(feature = "passkey-webauthn"))]
fn blocked_permission_mask() -> u32 {
    use cef::PermissionRequestTypes as Permission;

    Permission::AR_SESSION.get_raw()
        | Permission::VR_SESSION.get_raw()
        | Permission::HAND_TRACKING.get_raw()
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
#[cfg(not(feature = "passkey-webauthn"))]
cef::wrap_client! {
    struct TaborClient {
        display_handler: cef::DisplayHandler,
        render_handler: cef::RenderHandler,
        download_handler: cef::DownloadHandler,
        focus_handler: cef::FocusHandler,
        editable_focus_notifier: WebEditableFocusNotifier,
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

        fn on_process_message_received(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            source_process: cef::ProcessId,
            message: Option<&mut cef::ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            handle_client_process_message(&self.editable_focus_notifier, source_process, message)
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
        editable_focus_notifier: WebEditableFocusNotifier,
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

        fn on_process_message_received(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            source_process: cef::ProcessId,
            message: Option<&mut cef::ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            handle_client_process_message(&self.editable_focus_notifier, source_process, message)
        }
    }
}

cef::wrap_task! {
    struct SendKeyTask {
        browser: cef::Browser,
        events: Vec<cef::KeyEvent>,
        invalidate_after: bool,
    }

    impl Task {
        fn execute(&self) {
            let Some(host) = self.browser.host() else {
                return;
            };
            for event in &self.events {
                host.send_key_event(Some(event));
            }
            if self.invalidate_after {
                invalidate_browser_surfaces(&self.browser);
            }
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
    last_title: Option<String>,
    last_url: Option<String>,
    focus_policy: WebFocusPolicy,
    title_state: StdRc<RefCell<Option<String>>>,
    paint_state: StdRc<RefCell<PaintState>>,
    devtools_state: StdRc<RefCell<DevToolsState>>,
    automation_state: StdRc<RefCell<AutomationState>>,
    last_mouse_event: Option<cef::MouseEvent>,
    mouse_button_flags: u32,
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
    pub fn new(
        window: &Window,
        _size_info: &SizeInfo,
        layout: BrowserViewportLayout,
        tab_id: TabId,
        url: &str,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let _mtm = MainThreadMarker::new()
            .ok_or_else(|| std::io::Error::other("WebView must be created on main thread"))?;

        crate::macos::cef::ensure_initialized()?;
        super::register_webview();
        let mut web_view_constructed = false;

        let result = (|| {
            let parent = ns_view(window)?;
            let screen_rect = cef_screen_rect(window, layout);
            let paint_state =
                StdRc::new(RefCell::new(PaintState::new(layout, screen_rect, window.scale_factor)));
            let render_handler = TaborRenderHandler::new(
                paint_state.clone(),
                WebViewDirtyNotifier::new(proxy.clone(), window.id(), tab_id),
            );
            let mut window_info = cef::WindowInfo::default().set_as_windowless(parent.cast());
            window_info.shared_texture_enabled = 1;

            let title_state = StdRc::new(RefCell::new(None));
            let automation_state = StdRc::new(RefCell::new(AutomationState::new()));
            let display_handler = TaborDisplayHandler::new(title_state.clone());
            let download_handler = TaborDownloadHandler::new(automation_state.clone());
            let focus_policy = WebFocusPolicy::new();
            let focus_handler = TaborFocusHandler::new(focus_policy.clone());
            let editable_focus_notifier =
                WebEditableFocusNotifier::new(proxy.clone(), window.id(), tab_id);
            #[cfg(not(feature = "passkey-webauthn"))]
            let mut client = {
                let permission_handler = TaborPermissionHandler::new();
                TaborClient::new(
                    display_handler,
                    render_handler,
                    download_handler,
                    focus_handler,
                    editable_focus_notifier,
                    permission_handler,
                )
            };
            #[cfg(feature = "passkey-webauthn")]
            let mut client = TaborClient::new(
                display_handler,
                render_handler,
                download_handler,
                focus_handler,
                editable_focus_notifier,
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
            .ok_or_else(|| std::io::Error::other("Failed to create CEF browser"))?;

            if let Some(host) = browser.host() {
                host.set_windowless_frame_rate(60);
                host.notify_screen_info_changed();
                host.was_resized();
                host.invalidate(cef::PaintElementType::VIEW);
                host.invalidate(cef::PaintElementType::POPUP);
            }

            let devtools_state = StdRc::new(RefCell::new(DevToolsState::new()));
            let mut observer = TaborDevToolsObserver::new(devtools_state.clone());
            let registration = browser
                .host()
                .and_then(|host| host.add_dev_tools_message_observer(Some(&mut observer)));

            let web_view = Self {
                browser,
                last_title: None,
                last_url: None,
                focus_policy,
                title_state,
                paint_state,
                devtools_state,
                automation_state,
                last_mouse_event: None,
                mouse_button_flags: 0,
                _devtools_observer: observer,
                _devtools_registration: registration,
                _client: client,
            };
            web_view_constructed = true;

            web_view.enable_devtools_domains();

            Ok(web_view)
        })();

        // Startup failures before `Self` is constructed still need to roll back the counter.
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

    pub fn restore_native_focus(&mut self, window: &Window) -> bool {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");
        let Some(view) = browser_view(&self.browser) else {
            return false;
        };

        if !self.focus_policy.host_focused() || !self.focus_policy.editable_focused() {
            return false;
        }

        self.focus_policy.arm_native_focus();
        let restored = window.focus_native_view(view);
        if restored {
            if let Some(host) = self.browser.host() {
                host.set_focus(1);
            }
            invalidate_browser_surfaces(&self.browser);
        } else {
            self.focus_policy.disarm_native_focus();
        }
        restored
    }

    pub fn update_frame(
        &mut self,
        window: &Window,
        _size_info: &SizeInfo,
        layout: BrowserViewportLayout,
    ) {
        {
            let screen_rect = cef_screen_rect(window, layout);
            let mut paint_state = self.paint_state.borrow_mut();
            paint_state.update_geometry(layout, screen_rect, window.scale_factor);
        }
        if let Some(host) = self.browser.host() {
            host.notify_screen_info_changed();
            host.was_resized();
        }
        invalidate_browser_surfaces(&self.browser);
    }

    pub fn acceleration_info(&self) -> WebAccelerationInfo {
        self.paint_state.borrow().acceleration_info()
    }

    pub fn load_url(&mut self, url: &str) -> bool {
        self.last_title = None;
        self.last_url = None;
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

    pub fn handle_mouse_input(
        &mut self,
        window: &Window,
        position: PhysicalPosition<f64>,
        state: ElementState,
        button: MouseButton,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");
        if matches!((state, button), (ElementState::Pressed, MouseButton::Left)) {
            self.prepare_native_focus_for_mouse_input(window);
        }
        let button_flag = cef_mouse_button_flag(button);
        let event_button_flags = match state {
            ElementState::Pressed => self.mouse_button_flags | button_flag,
            ElementState::Released => self.mouse_button_flags & !button_flag,
        };
        let Some(event) = self.mouse_event(window, position, modifiers, event_button_flags) else {
            return false;
        };

        let button_type = match button {
            MouseButton::Left => cef::MouseButtonType::LEFT,
            MouseButton::Right => cef::MouseButtonType::RIGHT,
            MouseButton::Middle => cef::MouseButtonType::MIDDLE,
            MouseButton::Back | MouseButton::Forward | MouseButton::Other(_) => {
                cef::MouseButtonType::MIDDLE
            },
        };

        let Some(host) = self.browser.host() else {
            return false;
        };
        self.last_mouse_event = Some(event.clone());
        self.mouse_button_flags = event_button_flags;

        match state {
            ElementState::Pressed => {
                host.send_mouse_click_event(Some(&event), button_type, 0, 1);
            },
            ElementState::Released => {
                host.send_mouse_click_event(Some(&event), button_type, 1, 1);
            },
        }

        invalidate_browser_surfaces(&self.browser);
        true
    }

    fn prepare_native_focus_for_mouse_input(&mut self, window: &Window) {
        if !self.focus_policy.host_focused() {
            return;
        }

        let Some(view) = browser_view(&self.browser) else {
            return;
        };

        self.focus_policy.arm_native_focus();
        if window.focus_native_view(view) {
            if let Some(host) = self.browser.host() {
                host.set_focus(1);
            }
        } else {
            self.focus_policy.disarm_native_focus();
        }
    }

    pub fn handle_mouse_move(
        &mut self,
        window: &Window,
        position: PhysicalPosition<f64>,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");
        let Some(event) = self.mouse_event(window, position, modifiers, self.mouse_button_flags)
        else {
            self.handle_mouse_leave();
            return false;
        };
        let Some(host) = self.browser.host() else {
            return false;
        };

        self.last_mouse_event = Some(event.clone());
        host.send_mouse_move_event(Some(&event), 0);
        if self.mouse_button_flags != 0 {
            invalidate_browser_surfaces(&self.browser);
        }
        true
    }

    pub fn handle_mouse_leave(&mut self) {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");
        let Some(host) = self.browser.host() else {
            return;
        };

        let event = self.last_mouse_event.clone().unwrap_or_default();
        host.send_mouse_move_event(Some(&event), 1);
    }

    pub fn handle_mouse_wheel(
        &mut self,
        window: &Window,
        position: PhysicalPosition<f64>,
        delta_x: f64,
        delta_y: f64,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");
        let Some(event) = self.mouse_event(window, position, modifiers, self.mouse_button_flags)
        else {
            return false;
        };
        let Some(host) = self.browser.host() else {
            return false;
        };

        let layout = self.paint_state.borrow().layout;
        let scaled_delta_y = scaled_browser_wheel_delta_y(layout, delta_y);

        self.last_mouse_event = Some(event.clone());
        host.send_mouse_wheel_event(
            Some(&event),
            delta_x.round() as i32,
            scaled_delta_y.round() as i32,
        );
        invalidate_browser_surfaces(&self.browser);
        true
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

    pub fn handle_key_input(
        &mut self,
        _window: &Window,
        key: &KeyEvent,
        text: &str,
        modifiers: ModifiersState,
    ) -> bool {
        let key_without_modifiers = key.key_without_modifiers();
        self.handle_key_input_inner(
            &key_without_modifiers,
            text,
            key.state,
            modifiers,
            key.repeat,
            key.location,
            key.physical_key,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_key_input_inner(
        &mut self,
        key: &Key,
        text: &str,
        state: ElementState,
        modifiers: ModifiersState,
        repeat: bool,
        location: KeyLocation,
        physical_key: winit::keyboard::PhysicalKey,
    ) -> bool {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");

        let base_flags = cef_event_flags(modifiers, repeat, location);
        let windows_key_code = cef_windows_key_code_from_key(key);
        let scancode = macos_scancode_from_physical_key(physical_key).unwrap_or(0);
        let native_key_code = cef_native_key_code(scancode, modifiers);
        let (character, unmodified_character) =
            cef_characters_from_key_text(key, if text.is_empty() { "" } else { text });
        let should_send_char = character != 0 && !modifiers.super_key() && !modifiers.control_key();

        let focus_on_editable_field = 1;
        let mut events = Vec::new();

        match state {
            ElementState::Pressed => {
                if windows_key_code != 0 {
                    let down = cef::KeyEvent {
                        type_: cef::KeyEventType::KEYDOWN,
                        modifiers: base_flags,
                        windows_key_code,
                        native_key_code,
                        is_system_key: 0,
                        character,
                        unmodified_character,
                        focus_on_editable_field,
                        ..cef::KeyEvent::default()
                    };
                    events.push(down);
                }

                if should_send_char {
                    let ch = cef::KeyEvent {
                        type_: cef::KeyEventType::CHAR,
                        modifiers: base_flags,
                        windows_key_code: character as i32,
                        native_key_code,
                        is_system_key: 0,
                        character,
                        unmodified_character,
                        focus_on_editable_field,
                        ..cef::KeyEvent::default()
                    };
                    events.push(ch);
                }
            },
            ElementState::Released => {
                if windows_key_code != 0 {
                    let up = cef::KeyEvent {
                        type_: cef::KeyEventType::KEYUP,
                        modifiers: base_flags,
                        windows_key_code,
                        native_key_code,
                        is_system_key: 0,
                        character: 0,
                        unmodified_character: 0,
                        focus_on_editable_field,
                        ..cef::KeyEvent::default()
                    };
                    events.push(up);
                }
            },
        }

        if events.is_empty() {
            return false;
        }

        let invalidate_after = should_invalidate_after_key_input(state);
        if cef::currently_on(cef::ThreadId::UI) == 1 {
            if let Some(host) = self.browser.host() {
                for event in &events {
                    host.send_key_event(Some(event));
                }
            }
            if invalidate_after {
                invalidate_browser_surfaces(&self.browser);
            }
        } else {
            let mut task = SendKeyTask::new(self.browser.clone(), events, invalidate_after);
            let _ = cef::post_task(cef::ThreadId::UI, Some(&mut task));
        }

        windows_key_code != 0 || should_send_char
    }

    pub fn exec_js(&mut self, script: &str) {
        self.eval_js_string(script, |_| {});
    }

    pub fn copy_selection(&mut self) {
        self.run_frame_edit_command(FrameEditCommand::Copy);
    }

    pub fn cut_selection(&mut self) {
        self.run_frame_edit_command(FrameEditCommand::Cut);
    }

    pub fn paste(&mut self) {
        self.run_frame_edit_command(FrameEditCommand::Paste);
    }

    pub fn eval_js_string<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        self.eval_js_string_impl(script, false, callback);
    }

    pub fn eval_js_string_with_user_gesture<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        self.eval_js_string_impl(script, true, callback);
    }

    fn eval_js_string_impl<F>(&mut self, script: &str, user_gesture: bool, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        let mut params = match cef::dictionary_value_create() {
            Some(params) => params,
            None => {
                callback(None);
                return;
            },
        };
        dict_set_string(&mut params, "expression", script);
        dict_set_bool(&mut params, "returnByValue", true);
        dict_set_bool(&mut params, "awaitPromise", true);
        dict_set_bool(&mut params, "userGesture", user_gesture);

        let browser = self.browser.clone();
        self.devtools_execute("Runtime.evaluate", Some(params), move |result| {
            let output = match result {
                Ok(payload) => runtime_result_to_string(&payload),
                Err(err) => {
                    debug!("Runtime.evaluate failed: {err}");
                    None
                },
            };
            invalidate_browser_surfaces(&browser);
            callback(output);
        });
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

    pub fn devtools_command_json<F>(
        &self,
        method: &str,
        params: Option<JsonValue>,
        callback: F,
    ) -> Result<(), String>
    where
        F: FnOnce(Result<JsonValue, String>) + 'static,
    {
        let params = match params {
            None => None,
            Some(JsonValue::Null) => None,
            Some(value) => Some(json_to_cef_dictionary(&value)?),
        };

        self.devtools_execute_checked(method, params, callback)
    }

    pub fn devtools_events_since(&self, last_id: u64, max: usize) -> (Vec<String>, u64) {
        let state = self.devtools_state.borrow();
        state.events_since(last_id, max)
    }

    pub fn latest_devtools_event_id(&self) -> u64 {
        let state = self.devtools_state.borrow();
        state.latest_event_id()
    }

    pub fn set_file_input_files<F>(
        &self,
        element_id: &str,
        paths: Vec<String>,
        callback: F,
    ) -> Result<(), String>
    where
        F: FnOnce(Result<String, String>) + 'static,
    {
        let browser = self.browser.clone();
        let state = self.devtools_state.clone();
        let callback: StringResultCallback = StdRc::new(RefCell::new(Some(Box::new(callback))));
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

        let callback_for_root = callback.clone();
        let browser_for_root = browser.clone();
        let state_for_root = state.clone();
        devtools_command_json_with(
            &browser_for_root,
            &state_for_root,
            "DOM.getDocument",
            Some(serde_json::json!({ "depth": 0 })),
            move |result| {
                let root_id = match result {
                    Ok(payload) => payload
                        .get("root")
                        .and_then(|root| root.get("nodeId"))
                        .and_then(JsonValue::as_u64),
                    Err(err) => {
                        finish_string_result_callback(&callback_for_root, Err(err));
                        return;
                    },
                };
                let Some(root_id) = root_id else {
                    finish_string_result_callback(
                        &callback_for_root,
                        Err(String::from("DOM.getDocument returned no root node id")),
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
                    move |result| {
                        let node_id = match result {
                            Ok(payload) => payload.get("nodeId").and_then(JsonValue::as_u64),
                            Err(err) => {
                                finish_string_result_callback(&callback, Err(err));
                                return;
                            },
                        };
                        let Some(node_id) = node_id.filter(|node_id| *node_id != 0) else {
                            finish_string_result_callback(
                                &callback,
                                Err(String::from("element not found")),
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
                            move |result| match result {
                                Ok(_) => {
                                    let callback_for_eval = callback_for_set.clone();
                                    let browser_for_eval = browser.clone();
                                    let state_for_eval = state.clone();
                                    if let Err(err) = runtime_evaluate_with(
                                        &browser_for_eval,
                                        &state_for_eval,
                                        &inspect_script,
                                        false,
                                        move |result| match result {
                                            Some(raw) => finish_string_result_callback(
                                                &callback_for_eval,
                                                Ok(raw),
                                            ),
                                            None => finish_string_result_callback(
                                                &callback_for_eval,
                                                Err(String::from(
                                                    "Runtime.evaluate returned no payload",
                                                )),
                                            ),
                                        },
                                    ) {
                                        finish_string_result_callback(&callback_for_set, Err(err));
                                    }
                                },
                                Err(err) => {
                                    finish_string_result_callback(&callback_for_set, Err(err))
                                },
                            },
                        );
                        if let Err(err) = set_files {
                            finish_string_result_callback(&callback, Err(err));
                        }
                    },
                );
                if let Err(err) = query {
                    finish_string_result_callback(&callback_for_root, Err(err));
                }
            },
        )
    }

    pub fn downloads(&self) -> Vec<AgentDownload> {
        self.automation_state.borrow().downloads()
    }

    pub fn poll_title(&mut self) -> Option<String> {
        let title = self.title_state.borrow().clone();
        let title = title?;

        if self.last_title.as_deref() == Some(&title) {
            return None;
        }

        self.last_title = Some(title.clone());
        Some(title)
    }

    pub fn poll_url(&mut self) -> Option<String> {
        let url = self.current_url()?;
        if self.last_url.as_deref() == Some(&url) {
            return None;
        }
        self.last_url = Some(url.clone());
        Some(url)
    }

    pub fn current_url(&self) -> Option<String> {
        let frame = self.browser.main_frame()?;
        let url = frame.url();
        let url = CefString::from(&url);
        let url = url.to_string();
        if url.is_empty() { None } else { Some(url) }
    }

    pub fn show_inspector(&mut self) -> bool {
        let Some(host) = self.browser.host() else {
            return false;
        };
        host.show_dev_tools(None, None, None, None);
        true
    }

    pub fn with_surfaces<R>(
        &self,
        func: impl FnOnce(Option<WebSurfaceRef>, Option<WebPopupSurfaceRef>) -> R,
    ) -> R {
        let paint_state = self.paint_state.borrow();
        let main = paint_state.main.as_ref().map(AcceleratedSurface::as_public_ref);
        let popup = paint_state.popup.surface.as_ref().map(|surface| WebPopupSurfaceRef {
            x: paint_state.popup.rect.x.max(0) as usize,
            y: paint_state.popup.rect.y.max(0) as usize,
            width: paint_state.popup.rect.width.max(0) as usize,
            height: paint_state.popup.rect.height.max(0) as usize,
            surface: surface.as_public_ref(),
        });

        func(main, popup)
    }

    fn enable_devtools_domains(&self) {
        self.devtools_fire("DOM.enable", None);
        self.devtools_fire("Network.enable", None);
        self.devtools_fire("Page.enable", None);
        self.devtools_fire("Runtime.enable", None);
        self.devtools_fire("Log.enable", None);
    }

    fn devtools_fire(&self, method: &str, params: Option<cef::DictionaryValue>) {
        let Some(host) = self.browser.host() else {
            return;
        };
        let method = CefString::from(method);
        let mut params = params;
        let id = {
            let mut state = self.devtools_state.borrow_mut();
            state.next_id()
        };
        let _ = host.execute_dev_tools_method(id, Some(&method), params.as_mut());
    }

    fn devtools_execute<F>(&self, method: &str, params: Option<cef::DictionaryValue>, callback: F)
    where
        F: FnOnce(Result<JsonValue, String>) + 'static,
    {
        let Some(host) = self.browser.host() else {
            callback(Err(String::from("DevTools host unavailable")));
            return;
        };

        let method = CefString::from(method);
        let mut params = params;
        let id = {
            let mut state = self.devtools_state.borrow_mut();
            let id = state.next_id();
            state.pending.insert(id, Box::new(callback));
            id
        };

        let ok = host.execute_dev_tools_method(id, Some(&method), params.as_mut());
        if ok == 0 {
            let callback = {
                let mut state = self.devtools_state.borrow_mut();
                state.pending.remove(&id)
            };
            if let Some(callback) = callback {
                callback(Err(String::from("DevTools method dispatch failed")));
            }
        }
    }

    fn devtools_execute_checked<F>(
        &self,
        method: &str,
        params: Option<cef::DictionaryValue>,
        callback: F,
    ) -> Result<(), String>
    where
        F: FnOnce(Result<JsonValue, String>) + 'static,
    {
        let Some(host) = self.browser.host() else {
            return Err(String::from("DevTools host unavailable"));
        };

        let method = CefString::from(method);
        let mut params = params;
        let id = {
            let mut state = self.devtools_state.borrow_mut();
            let id = state.next_id();
            state.pending.insert(id, Box::new(callback));
            id
        };

        let ok = host.execute_dev_tools_method(id, Some(&method), params.as_mut());
        if ok == 0 {
            let _ = {
                let mut state = self.devtools_state.borrow_mut();
                state.pending.remove(&id)
            };
            return Err(String::from("DevTools method dispatch failed"));
        }

        Ok(())
    }

    fn mouse_event(
        &self,
        window: &Window,
        position: PhysicalPosition<f64>,
        modifiers: objc2_app_kit::NSEventModifierFlags,
        button_flags: u32,
    ) -> Option<cef::MouseEvent> {
        let scale_factor = window.scale_factor.max(f64::MIN_POSITIVE);
        let x = (position.x / scale_factor).floor().max(0.0) as usize;
        let y = (position.y / scale_factor).floor().max(0.0) as usize;
        let layout = self.paint_state.borrow().layout;
        let (logical_x, logical_y) = layout.logical_point_for_visual(x, y)?;

        Some(cef::MouseEvent {
            x: logical_x as i32,
            y: logical_y as i32,
            modifiers: cef_mouse_event_flags(modifiers, button_flags),
        })
    }
}

fn should_run_frame_edit_inline(on_ui_thread: bool, handling_send_event: bool) -> bool {
    on_ui_thread && !handling_send_event
}

impl Drop for WebView {
    fn drop(&mut self) {
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
    callback: F,
) -> Result<(), String>
where
    F: FnOnce(Result<JsonValue, String>) + 'static,
{
    let params = match params {
        None => None,
        Some(JsonValue::Null) => None,
        Some(value) => Some(json_to_cef_dictionary(&value)?),
    };

    let Some(host) = browser.host() else {
        return Err(String::from("DevTools host unavailable"));
    };

    let method = CefString::from(method);
    let mut params = params;
    let id = {
        let mut state = state.borrow_mut();
        let id = state.next_id();
        state.pending.insert(id, Box::new(callback));
        id
    };

    let ok = host.execute_dev_tools_method(id, Some(&method), params.as_mut());
    if ok == 0 {
        let _ = {
            let mut state = state.borrow_mut();
            state.pending.remove(&id)
        };
        return Err(String::from("DevTools method dispatch failed"));
    }

    Ok(())
}

fn runtime_evaluate_with<F>(
    browser: &cef::Browser,
    state: &StdRc<RefCell<DevToolsState>>,
    script: &str,
    user_gesture: bool,
    callback: F,
) -> Result<(), String>
where
    F: FnOnce(Option<String>) + 'static,
{
    devtools_command_json_with(
        browser,
        state,
        "Runtime.evaluate",
        Some(serde_json::json!({
            "expression": script,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": user_gesture,
        })),
        move |result| {
            let output = match result {
                Ok(payload) => runtime_result_to_string(&payload),
                Err(err) => {
                    debug!("Runtime.evaluate failed: {err}");
                    None
                },
            };
            callback(output);
        },
    )
}

type StringResultCallback = StdRc<RefCell<Option<Box<dyn FnOnce(Result<String, String>)>>>>;

fn finish_string_result_callback(callback: &StringResultCallback, result: Result<String, String>) {
    if let Some(callback) = callback.borrow_mut().take() {
        callback(result);
    }
}

fn cef_mouse_button_flag(button: MouseButton) -> u32 {
    use cef::sys::cef_event_flags_t;

    match button {
        MouseButton::Left => cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0,
        MouseButton::Middle => cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0,
        MouseButton::Right => cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0,
        MouseButton::Back | MouseButton::Forward | MouseButton::Other(_) => 0,
    }
}

fn cef_mouse_event_flags(modifiers: objc2_app_kit::NSEventModifierFlags, button_flags: u32) -> u32 {
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

fn runtime_result_to_string(payload: &JsonValue) -> Option<String> {
    if payload.get("exceptionDetails").is_some() {
        return None;
    }

    let result = payload.get("result").unwrap_or(payload);
    if let Some(value) = result.get("value") {
        if let Some(text) = value.as_str() {
            return Some(text.to_string());
        }
        return Some(value.to_string());
    }

    result.get("description").and_then(|value| value.as_str()).map(|value| value.to_string())
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

fn ns_view(window: &Window) -> Result<*mut AnyObject, Box<dyn Error>> {
    match window.raw_window_handle() {
        RawWindowHandle::AppKit(handle) => Ok(handle.ns_view.as_ptr() as *mut AnyObject),
        _ => Err(std::io::Error::other("WebView requires an AppKit window").into()),
    }
}

fn cef_screen_rect(window: &Window, layout: BrowserViewportLayout) -> cef::Rect {
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

fn browser_view(browser: &cef::Browser) -> Option<*mut AnyObject> {
    let host = browser.host()?;
    let view = host.window_handle() as *mut AnyObject;
    if view.is_null() { None } else { Some(view) }
}

fn close_browser_resources(browser: &cef::Browser) {
    if let Some(host) = browser.host() {
        host.close_browser(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PaintState, WebFocusPolicy, WebViewDirtyNotifier, browser_device_scale_factor,
        browser_screen_point, browser_settings, cef_mouse_button_flag, cef_mouse_event_flags,
        handle_ime_composition_range_change, handle_text_selection_change,
        parse_web_editable_focus_message, scaled_browser_wheel_delta_y,
        should_invalidate_after_frame_edit, should_invalidate_after_key_input,
        should_run_frame_edit_inline,
    };
    #[cfg(not(feature = "passkey-webauthn"))]
    use super::{
        PermissionDecision, media_access_decision, permission_decision, permission_request_result,
        should_block_media_access_request, should_log_allowed_permission_request,
    };
    use cef::sys::cef_event_flags_t;
    #[cfg(not(feature = "passkey-webauthn"))]
    use cef::{
        MediaAccessPermissionTypes as MediaPermission, PermissionRequestResult,
        PermissionRequestTypes as Permission,
    };
    use objc2_app_kit::NSEventModifierFlags;
    use std::cell::RefCell;
    use winit::event::{ElementState, MouseButton};
    use winit::window::WindowId;

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
            PaintState::new(layout, cef::Rect { x: 0, y: 0, width: 900, height: 600 }, 2.0);
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
        assert_eq!(scaled_browser_wheel_delta_y(normal, 48.0), 48.0);

        let folded = crate::display::browser_layout::BrowserViewportLayout::new(
            &crate::display::SizeInfo::new(1100.0, 708.0, 1.0, 1.0, 0.0, 0.0, 0.0, false),
            1.0,
            crate::display::browser_layout::BrowserViewMode::MultiColumn,
            &crate::config::browser::MultiColumnBrowserConfig { target_width_px: 400 },
            None,
        );
        assert_eq!(folded.column_count(), 2);
        assert_eq!(scaled_browser_wheel_delta_y(folded, 48.0), 96.0);
    }

    #[test]
    fn browser_screen_point_uses_folded_visual_coordinates() {
        let folded = crate::display::browser_layout::BrowserViewportLayout::new(
            &crate::display::SizeInfo::new(1950.0, 600.0, 1.0, 1.0, 0.0, 0.0, 0.0, false),
            1.0,
            crate::display::browser_layout::BrowserViewMode::MultiColumn,
            &crate::config::browser::MultiColumnBrowserConfig::default(),
            None,
        );
        let screen_rect = cef::Rect { x: 500, y: 200, width: 1950, height: 600 };

        assert_eq!(folded.column_count(), 2);
        assert_eq!(browser_screen_point(folded, screen_rect, 17, 745), Some((1492, 345)));
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
        let permissions = Permission::LOCAL_NETWORK_ACCESS.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert_eq!(permission_request_result(permissions), PermissionRequestResult::ACCEPT);
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
