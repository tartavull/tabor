use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::ffi::CString;
use std::fmt::Write;
use std::fs;
use std::mem;
use std::path::PathBuf;
use std::rc::Rc as StdRc;

use cef::{
    CefString, Client, DevToolsMessageObserver, DisplayHandler, DownloadHandler,
    ImplBeforeDownloadCallback, ImplBrowser, ImplBrowserHost, ImplClient,
    ImplDevToolsMessageObserver, ImplDictionaryValue, ImplDisplayHandler, ImplDownloadHandler,
    ImplDownloadItem, ImplFrame, ImplListValue, ImplMediaAccessCallback, ImplPermissionHandler,
    ImplPermissionPromptCallback, ImplTask, PermissionHandler, PermissionRequestResult, Task,
    WrapClient, WrapDevToolsMessageObserver, WrapDisplayHandler, WrapDownloadHandler,
    WrapPermissionHandler, WrapTask, rc::Rc,
};
use log::debug;
use objc2::encode::{Encode, Encoding};
use objc2::ffi;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{MainThreadMarker, msg_send, sel};
use serde_json::{Map as JsonMap, Value as JsonValue};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseButton};
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::raw_window_handle::RawWindowHandle;

use crate::display::SizeInfo;
use crate::display::window::Window;
use crate::event::Event;
use crate::ipc::{AgentDownload, WebNetworkEntry};
use crate::tabs::TabId;
use tabor_terminal::grid::Dimensions;

use super::keycodes::macos_scancode_from_physical_key;
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
fn should_block_permission_request(requested_permissions: u32) -> bool {
    matches!(permission_decision(requested_permissions), PermissionDecision::Deny)
}

#[cfg(not(feature = "passkey-webauthn"))]
fn log_blocked_permission_request(
    source: &str,
    requesting_origin: Option<&cef::CefString>,
    requested_permissions: u32,
) {
    let origin = requesting_origin
        .map(|origin| origin.to_string())
        .unwrap_or_else(|| String::from("<unknown>"));
    debug!(
        "Denied CEF permission request (source={source}, origin={origin}, mask=0x{requested_permissions:08x})"
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
            if should_block_permission_request(requested_permissions) {
                if let Some(callback) = callback {
                    callback.cancel();
                }
                log_blocked_permission_request(
                    "media_access",
                    requesting_origin,
                    requested_permissions,
                );
                return 1;
            }

            0
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut cef::Browser>,
            _prompt_id: u64,
            requesting_origin: Option<&cef::CefString>,
            requested_permissions: u32,
            callback: Option<&mut cef::PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            if should_block_permission_request(requested_permissions) {
                if let Some(callback) = callback {
                    callback.cont(PermissionRequestResult::DENY);
                }
                log_blocked_permission_request("prompt", requesting_origin, requested_permissions);
                return 1;
            }

            0
        }
    }
}
#[cfg(not(feature = "passkey-webauthn"))]
cef::wrap_client! {
    struct TaborClient {
        display_handler: cef::DisplayHandler,
        download_handler: cef::DownloadHandler,
        permission_handler: cef::PermissionHandler,
    }

    impl Client {
        fn display_handler(&self) -> Option<cef::DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn download_handler(&self) -> Option<cef::DownloadHandler> {
            Some(self.download_handler.clone())
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
        download_handler: cef::DownloadHandler,
    }

    impl Client {
        fn display_handler(&self) -> Option<cef::DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn download_handler(&self) -> Option<cef::DownloadHandler> {
            Some(self.download_handler.clone())
        }
    }
}

cef::wrap_task! {
    struct SendKeyTask {
        browser: cef::Browser,
        events: Vec<cef::KeyEvent>,
    }

    impl Task {
        fn execute(&self) {
            let Some(host) = self.browser.host() else {
                return;
            };
            for event in &self.events {
                host.send_key_event(Some(event));
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
    title_state: StdRc<RefCell<Option<String>>>,
    devtools_state: StdRc<RefCell<DevToolsState>>,
    automation_state: StdRc<RefCell<AutomationState>>,
    _devtools_observer: cef::DevToolsMessageObserver,
    _devtools_registration: Option<cef::Registration>,
    _client: cef::Client,
}

impl WebView {
    pub fn new(
        window: &Window,
        size_info: &SizeInfo,
        _tab_id: TabId,
        url: &str,
        _proxy: &winit::event_loop::EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let _mtm = MainThreadMarker::new()
            .ok_or_else(|| std::io::Error::other("WebView must be created on main thread"))?;

        crate::macos::cef::ensure_initialized()?;
        super::register_webview();

        let result = (|| {
            let parent = ns_view(window)?;
            let frame = webview_frame(window, size_info);
            let bounds = cef_rect(window, size_info);
            let window_info = cef::WindowInfo::default().set_as_child(parent.cast(), &bounds);

            let title_state = StdRc::new(RefCell::new(None));
            let automation_state = StdRc::new(RefCell::new(AutomationState::new()));
            let display_handler = TaborDisplayHandler::new(title_state.clone());
            let download_handler = TaborDownloadHandler::new(automation_state.clone());
            #[cfg(not(feature = "passkey-webauthn"))]
            let mut client = {
                let permission_handler = TaborPermissionHandler::new();
                TaborClient::new(display_handler, download_handler, permission_handler)
            };
            #[cfg(feature = "passkey-webauthn")]
            let mut client = TaborClient::new(display_handler, download_handler);

            let browser_settings = cef::BrowserSettings::default();
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

            if let Some(view) = browser_view(&browser) {
                unsafe {
                    let _: () = msg_send![view, setFrame: frame];
                    disable_cef_view_first_responder(view);
                    let _: () = msg_send![parent, addSubview: view];
                }
            }
            if let Some(host) = browser.host() {
                host.was_resized();
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
                title_state,
                devtools_state,
                automation_state,
                _devtools_observer: observer,
                _devtools_registration: registration,
                _client: client,
            };

            web_view.enable_devtools_domains();

            Ok(web_view)
        })();

        if result.is_err() {
            super::unregister_webview();
        }

        result
    }

    pub fn set_visible(&mut self, visible: bool) {
        if let Some(view) = browser_view(&self.browser) {
            unsafe {
                let _: () = msg_send![view, setHidden: !visible];
            }
        }
        if let Some(host) = self.browser.host() {
            host.was_hidden(if visible { 0 } else { 1 });
        }
    }

    pub fn set_focus(&mut self, focus: bool) {
        if let Some(host) = self.browser.host() {
            host.set_focus(if focus { 1 } else { 0 });
        }
    }

    pub fn update_frame(&mut self, window: &Window, size_info: &SizeInfo) {
        if let Some(view) = browser_view(&self.browser) {
            let frame = webview_frame(window, size_info);
            unsafe {
                let _: () = msg_send![view, setFrame: frame];
            }
        }
        if let Some(host) = self.browser.host() {
            host.was_resized();
        }
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
        size_info: &SizeInfo,
        position: PhysicalPosition<f64>,
        state: ElementState,
        button: MouseButton,
        _modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        let _mtm = MainThreadMarker::new().expect("WebView input requires main thread");

        let scale_factor = window.scale_factor;
        let origin_x = f64::from(size_info.padding_x()) / scale_factor;
        let origin_y = f64::from(size_info.padding_y()) / scale_factor;
        let width =
            f64::from(size_info.width() - size_info.padding_x() - size_info.padding_right())
                / scale_factor;
        let height =
            f64::from(size_info.cell_height() * size_info.screen_lines() as f32) / scale_factor;

        let local_x = position.x / scale_factor - origin_x;
        let local_y = position.y / scale_factor - origin_y;
        if local_x < 0.0 || local_y < 0.0 || local_x >= width || local_y >= height {
            return false;
        }

        let event = cef::MouseEvent { x: local_x as i32, y: local_y as i32, modifiers: 0 };

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

        match state {
            ElementState::Pressed => {
                host.send_mouse_click_event(Some(&event), button_type, 0, 1);
            },
            ElementState::Released => {
                host.send_mouse_click_event(Some(&event), button_type, 1, 1);
            },
        }

        true
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

        if cef::currently_on(cef::ThreadId::UI) == 1 {
            if let Some(host) = self.browser.host() {
                for event in &events {
                    host.send_key_event(Some(event));
                }
            }
        } else {
            let mut task = SendKeyTask::new(self.browser.clone(), events);
            let _ = cef::post_task(cef::ThreadId::UI, Some(&mut task));
        }

        windows_key_code != 0 || should_send_char
    }

    pub fn exec_js(&mut self, script: &str) {
        self.eval_js_string(script, |_| {});
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

        self.devtools_execute("Runtime.evaluate", Some(params), move |result| {
            let output = match result {
                Ok(payload) => runtime_result_to_string(&payload),
                Err(err) => {
                    debug!("Runtime.evaluate failed: {err}");
                    None
                },
            };
            callback(output);
        });
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

fn webview_frame(window: &Window, size_info: &SizeInfo) -> CGRect {
    let scale_factor = window.scale_factor;
    let x = (f64::from(size_info.padding_x()) / scale_factor) as CGFloat;
    let y = (f64::from(size_info.padding_y()) / scale_factor) as CGFloat;
    let width = (f64::from(size_info.width() - size_info.padding_x() - size_info.padding_right())
        / scale_factor) as CGFloat;
    let height = (f64::from(size_info.cell_height() * size_info.screen_lines() as f32)
        / scale_factor) as CGFloat;

    CGRect { origin: CGPoint { x, y }, size: CGSize { width, height } }
}

fn cef_rect(window: &Window, size_info: &SizeInfo) -> cef::Rect {
    let scale_factor = window.scale_factor;
    let x = (f64::from(size_info.padding_x()) / scale_factor) as i32;
    let y = (f64::from(size_info.padding_y()) / scale_factor) as i32;
    let width = (f64::from(size_info.width() - size_info.padding_x() - size_info.padding_right())
        / scale_factor) as i32;
    let height = (f64::from(size_info.cell_height() * size_info.screen_lines() as f32)
        / scale_factor) as i32;
    cef::Rect { x, y, width, height }
}

fn browser_view(browser: &cef::Browser) -> Option<*mut AnyObject> {
    let host = browser.host()?;
    let view = host.window_handle() as *mut AnyObject;
    if view.is_null() { None } else { Some(view) }
}

fn close_browser_resources(browser: &cef::Browser) {
    if let Some(view) = browser_view(browser) {
        unsafe {
            let _: () = msg_send![view, removeFromSuperview];
        }
    }

    if let Some(host) = browser.host() {
        host.close_browser(1);
    }
}

// Keep the embedded CEF view from stealing keyboard focus.
//
// Tabor routes keyboard events through winit even for web tabs (command bar, vi-like bindings,
// web automation), so the CEF view must not become the NSWindow first responder.
unsafe extern "C-unwind" fn cef_view_accepts_first_responder(_this: &AnyObject, _sel: Sel) -> Bool {
    Bool::NO
}

unsafe extern "C-unwind" fn cef_view_become_first_responder(_this: &AnyObject, _sel: Sel) -> Bool {
    Bool::NO
}

fn no_first_responder_subclass(superclass: &AnyClass) -> &'static AnyClass {
    let super_name = superclass.name().to_str().unwrap_or("Unknown");
    let name = CString::new(format!("TaborNoFirstResponder_{super_name}"))
        .expect("no-first-responder subclass name");

    if let Some(existing) = AnyClass::get(name.as_c_str()) {
        return existing;
    }

    let super_ptr = superclass as *const AnyClass;
    let cls = unsafe { ffi::objc_allocateClassPair(super_ptr, name.as_ptr(), 0) };
    let cls =
        std::ptr::NonNull::new(cls).expect("failed to allocate no-first-responder override class");

    unsafe {
        add_method_raw(
            cls.as_ptr(),
            sel!(acceptsFirstResponder),
            mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel) -> Bool, Imp>(
                cef_view_accepts_first_responder,
            ),
            Bool::ENCODING,
            &[],
        );
        add_method_raw(
            cls.as_ptr(),
            sel!(becomeFirstResponder),
            mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel) -> Bool, Imp>(
                cef_view_become_first_responder,
            ),
            Bool::ENCODING,
            &[],
        );

        ffi::objc_registerClassPair(cls.as_ptr());
        cls.as_ref()
    }
}

fn disable_cef_view_first_responder(view: *mut AnyObject) {
    if view.is_null() {
        return;
    }

    let obj = unsafe { &*view };
    let current_class = obj.class();
    if current_class.name().to_bytes().starts_with(b"TaborNoFirstResponder_") {
        return;
    }

    let subclass = no_first_responder_subclass(current_class);
    unsafe {
        let old_class = AnyObject::set_class(obj, subclass);
        debug_assert_eq!(old_class, current_class);
    }
}

unsafe fn add_method_raw(
    cls: *mut AnyClass,
    selector: Sel,
    imp: Imp,
    ret: Encoding,
    args: &[Encoding],
) {
    let encoding = method_type_encoding(ret, args);
    let success = unsafe { ffi::class_addMethod(cls, selector, imp, encoding.as_ptr()) };
    assert!(success.as_bool(), "failed to add no-first-responder override method");
}

fn method_type_encoding(ret: Encoding, args: &[Encoding]) -> CString {
    let mut types = format!("{ret}{}{}", Encoding::Object, Encoding::Sel);
    for enc in args {
        let _ = write!(&mut types, "{enc}");
    }
    CString::new(types).expect("method type encoding")
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "passkey-webauthn"))]
    use super::{PermissionDecision, permission_decision, should_block_permission_request};
    #[cfg(not(feature = "passkey-webauthn"))]
    use cef::PermissionRequestTypes as Permission;

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_allows_known_safe_permissions() {
        let permissions = Permission::CAMERA_STREAM.get_raw() | Permission::MIC_STREAM.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert!(!should_block_permission_request(permissions));
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_denies_blocked_permissions() {
        let permissions = Permission::AR_SESSION.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Deny);
        assert!(should_block_permission_request(permissions));
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_denies_unknown_permissions() {
        let unknown_bit = 1_u32 << 30;
        assert_eq!(permission_decision(unknown_bit), PermissionDecision::Deny);
        assert!(should_block_permission_request(unknown_bit));
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_denies_empty_permissions() {
        assert_eq!(permission_decision(0), PermissionDecision::Deny);
        assert!(should_block_permission_request(0));
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn permission_policy_allows_local_network_permission() {
        let permissions = Permission::LOCAL_NETWORK_ACCESS.get_raw();
        assert_eq!(permission_decision(permissions), PermissionDecision::Allow);
        assert!(!should_block_permission_request(permissions));
    }
}
