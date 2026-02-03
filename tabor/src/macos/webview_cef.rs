use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc as StdRc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use cef::{
    rc::Rc,
    CefString, Client, DevToolsMessageObserver, DisplayHandler, ImplBrowser, ImplBrowserHost,
    ImplClient, ImplDevToolsMessageObserver, ImplDictionaryValue, ImplDisplayHandler, ImplFrame,
    WrapClient, WrapDevToolsMessageObserver, WrapDisplayHandler,
};
use log::debug;
use objc2::encode::{Encode, Encoding};
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker};
use serde_json::Value as JsonValue;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton};
use winit::raw_window_handle::RawWindowHandle;

use crate::display::window::Window;
use crate::display::SizeInfo;
use crate::event::Event;
use crate::ipc::{WebNetworkAction, WebNetworkEntry};
use crate::tabs::TabId;
use tabor_terminal::grid::Dimensions;

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

struct DevToolsState {
    next_message_id: i32,
    pending: HashMap<i32, DevToolsCallback>,
    entries: Vec<WebNetworkEntry>,
    index: HashMap<String, usize>,
}

impl DevToolsState {
    fn new() -> Self {
        Self {
            next_message_id: 1,
            pending: HashMap::new(),
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn next_id(&mut self) -> i32 {
        let id = self.next_message_id;
        self.next_message_id += 1;
        id
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
            }
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
                    }
                };

                let entry = &mut self.entries[index];
                entry.status = status.or(entry.status);
                entry.resource_type = resource_type.or_else(|| entry.resource_type.clone());
                entry.end_time = timestamp.or(entry.end_time);
                if let Some(url) = url {
                    entry.url = url;
                }
            }
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
            }
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
            }
            _ => {}
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
            self.state
                .borrow_mut()
                .update_network_state(&method, params.as_ref());
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

cef::wrap_client! {
    struct TaborClient {
        display_handler: cef::DisplayHandler,
    }

    impl Client {
        fn display_handler(&self) -> Option<cef::DisplayHandler> {
            Some(self.display_handler.clone())
        }
    }
}

pub struct WebView {
    browser: cef::Browser,
    last_title: Option<String>,
    last_url: Option<String>,
    title_state: StdRc<RefCell<Option<String>>>,
    devtools_state: StdRc<RefCell<DevToolsState>>,
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
        let _mtm = MainThreadMarker::new().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "WebView must be created on main thread")
        })?;

        crate::macos::cef::ensure_initialized()?;
        super::register_webview();

        let result = (|| {
            let parent = ns_view(window)?;
            let frame = webview_frame(window, size_info);
            let bounds = cef_rect(window, size_info);
            let window_info = cef::WindowInfo::default().set_as_child(parent.cast(), &bounds);

            let title_state = StdRc::new(RefCell::new(None));
            let display_handler = TaborDisplayHandler::new(title_state.clone());
            let mut client = TaborClient::new(display_handler);

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
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "Failed to create CEF browser")
            })?;

            if let Some(view) = browser_view(&browser) {
                unsafe {
                    let _: () = msg_send![view, setFrame: frame];
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

        let scale_factor = window.scale_factor as f64;
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

        let event = cef::MouseEvent {
            x: local_x as i32,
            y: local_y as i32,
            modifiers: 0,
        };

        let button_type = match button {
            MouseButton::Left => cef::MouseButtonType::LEFT,
            MouseButton::Right => cef::MouseButtonType::RIGHT,
            MouseButton::Middle => cef::MouseButtonType::MIDDLE,
            MouseButton::Back | MouseButton::Forward | MouseButton::Other(_) => {
                cef::MouseButtonType::MIDDLE
            }
        };

        let Some(host) = self.browser.host() else {
            return false;
        };

        match state {
            ElementState::Pressed => {
                host.send_mouse_click_event(Some(&event), button_type, 0, 1);
            }
            ElementState::Released => {
                host.send_mouse_click_event(Some(&event), button_type, 1, 1);
            }
        }

        true
    }

    pub fn exec_js(&mut self, script: &str) {
        self.eval_js_string(script, |_| {});
    }

    pub fn eval_js_string<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        let mut params = match cef::dictionary_value_create() {
            Some(params) => params,
            None => {
                callback(None);
                return;
            }
        };
        dict_set_string(&mut params, "expression", script);
        dict_set_bool(&mut params, "returnByValue", true);
        dict_set_bool(&mut params, "awaitPromise", true);

        self.devtools_execute("Runtime.evaluate", Some(params), move |result| {
            let output = match result {
                Ok(payload) => runtime_result_to_string(&payload),
                Err(err) => {
                    debug!("Runtime.evaluate failed: {err}");
                    None
                }
            };
            callback(output);
        });
    }

    pub fn snapshot_png<F>(&mut self, full: bool, callback: F)
    where
        F: FnOnce(Result<Vec<u8>, String>) + 'static,
    {
        let mut params = match cef::dictionary_value_create() {
            Some(params) => params,
            None => {
                callback(Err(String::from("Failed to build screenshot params")));
                return;
            }
        };
        dict_set_string(&mut params, "format", "png");
        if full {
            dict_set_bool(&mut params, "captureBeyondViewport", true);
        }

        self.devtools_execute("Page.captureScreenshot", Some(params), move |result| {
            match result {
                Ok(payload) => {
                    let Some(data) = payload.get("data").and_then(|v| v.as_str()) else {
                        callback(Err(String::from("Screenshot data missing")));
                        return;
                    };
                    match BASE64.decode(data) {
                        Ok(bytes) => callback(Ok(bytes)),
                        Err(err) => callback(Err(err.to_string())),
                    }
                }
                Err(err) => callback(Err(err)),
            }
        });
    }

    pub fn pdf<F>(&mut self, callback: F)
    where
        F: FnOnce(Result<Vec<u8>, String>) + 'static,
    {
        let mut params = match cef::dictionary_value_create() {
            Some(params) => params,
            None => {
                callback(Err(String::from("Failed to build PDF params")));
                return;
            }
        };
        dict_set_bool(&mut params, "printBackground", true);

        self.devtools_execute("Page.printToPDF", Some(params), move |result| {
            match result {
                Ok(payload) => {
                    let Some(data) = payload.get("data").and_then(|v| v.as_str()) else {
                        callback(Err(String::from("PDF data missing")));
                        return;
                    };
                    match BASE64.decode(data) {
                        Ok(bytes) => callback(Ok(bytes)),
                        Err(err) => callback(Err(err.to_string())),
                    }
                }
                Err(err) => callback(Err(err)),
            }
        });
    }

    pub fn poll_title(&mut self) -> Option<String> {
        let title = self.title_state.borrow().clone();
        let Some(title) = title else {
            return None;
        };

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
        if url.is_empty() {
            None
        } else {
            Some(url)
        }
    }

    pub fn show_inspector(&mut self) -> bool {
        let Some(host) = self.browser.host() else {
            return false;
        };
        host.show_dev_tools(None, None, None, None);
        true
    }

    pub fn network_entries(&mut self, action: WebNetworkAction) -> Option<Vec<WebNetworkEntry>> {
        let mut state = self.devtools_state.borrow_mut();
        match action {
            WebNetworkAction::Clear => {
                state.entries.clear();
                state.index.clear();
                Some(Vec::new())
            }
            WebNetworkAction::List { filter } => {
                let entries = if let Some(filter) = filter {
                    state
                        .entries
                        .iter()
                        .filter(|entry| entry.url.contains(&filter))
                        .cloned()
                        .collect::<Vec<_>>()
                } else {
                    state.entries.clone()
                };
                Some(entries)
            }
        }
    }

    fn enable_devtools_domains(&self) {
        self.devtools_fire("Network.enable", None);
        self.devtools_fire("Page.enable", None);
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
}

impl Drop for WebView {
    fn drop(&mut self) {
        if let Some(host) = self.browser.host() {
            host.close_browser(1);
        }
        super::unregister_webview();
    }
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

    result
        .get("description")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
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

fn ns_view(window: &Window) -> Result<*mut AnyObject, Box<dyn Error>> {
    match window.raw_window_handle() {
        RawWindowHandle::AppKit(handle) => Ok(handle.ns_view.as_ptr() as *mut AnyObject),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "WebView requires an AppKit window",
        )
        .into()),
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
    let scale_factor = window.scale_factor as f64;
    let x = (f64::from(size_info.padding_x()) / scale_factor) as i32;
    let y = (f64::from(size_info.padding_y()) / scale_factor) as i32;
    let width =
        (f64::from(size_info.width() - size_info.padding_x() - size_info.padding_right())
            / scale_factor) as i32;
    let height =
        (f64::from(size_info.cell_height() * size_info.screen_lines() as f32) / scale_factor) as i32;
    cef::Rect { x, y, width, height }
}

fn browser_view(browser: &cef::Browser) -> Option<*mut AnyObject> {
    let host = browser.host()?;
    let view = host.window_handle() as *mut AnyObject;
    if view.is_null() { None } else { Some(view) }
}

pub fn is_available() -> bool {
    crate::macos::cef::is_available()
}
