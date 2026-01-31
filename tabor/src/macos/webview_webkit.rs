use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::ptr;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use block2::RcBlock;
use log::debug;
use objc2::encode::{Encode, Encoding};
use objc2::ffi::NSInteger;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::runtime::Bool;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, class, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
use objc2_foundation::{NSNumber, NSPoint, NSString};
use serde_json::Value as JsonValue;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton};
use winit::event_loop::EventLoopProxy;
use winit::raw_window_handle::RawWindowHandle;
use winit::window::WindowId;

use tabor_terminal::grid::Dimensions;

use crate::display::SizeInfo;
use crate::display::window::Window;
use crate::event::{Event, EventType};
use crate::tabs::TabId;
use libc::{c_char, c_void};

#[link(name = "WebKit", kind = "framework")]
unsafe extern "C" {}

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

const NS_BITMAP_IMAGE_FILE_TYPE_PNG: NSInteger = 4;

pub struct WebView {
    view: Retained<AnyObject>,
    last_title: Option<String>,
    last_url: Option<String>,
    _delegate: Retained<AnyObject>,
}

pub(crate) struct PendingPopup {
    pub(crate) view: Retained<AnyObject>,
    pub(crate) delegate: Retained<AnyObject>,
    pub(crate) url: Option<String>,
}

struct WebViewDelegateIvars {
    proxy: EventLoopProxy<Event>,
    window_id: WindowId,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = WebViewDelegateIvars]
    struct WebViewDelegate;

    impl WebViewDelegate {
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_webview(
            &self,
            webview: *mut AnyObject,
            config: *mut AnyObject,
            navigation_action: *mut AnyObject,
            _window_features: *mut AnyObject,
        ) -> *mut AnyObject {
            let Some(config) = (unsafe { config.as_ref() }) else {
                return ptr::null_mut();
            };
            if let Err(err) = configure_webview_config(config) {
                debug!("Failed to configure popup WebView: {err}");
                return ptr::null_mut();
            }

            let frame: CGRect = unsafe { msg_send![webview, frame] };
            let view: *mut AnyObject = unsafe { msg_send![class!(WKWebView), alloc] };
            let view: *mut AnyObject =
                unsafe { msg_send![view, initWithFrame: frame, configuration: config] };
            let Some(view) = (unsafe { Retained::from_raw(view) }) else {
                return ptr::null_mut();
            };

            if let Err(err) = apply_safari_user_agent(&view) {
                debug!("Failed to apply Safari user agent: {err}");
                return ptr::null_mut();
            }

            let delegate = WebViewDelegate::new(self.ivars().proxy.clone(), self.ivars().window_id);
            let delegate = unsafe { Retained::cast_unchecked(delegate) };
            set_webview_delegate(&view, &delegate);

            unsafe {
                let _: () = msg_send![&*view, setHidden: true];
            }

            let url = navigation_action_url(navigation_action);
            let Some(popup_view) = (unsafe {
                Retained::retain(Retained::as_ptr(&view).cast_mut())
            })
            else {
                return ptr::null_mut();
            };

            let popup_id = register_pending_popup(PendingPopup {
                view: popup_view,
                delegate,
                url,
            });

            let event = Event::new(
                EventType::WebPopup { popup_id },
                self.ivars().window_id,
            );
            let _ = self.ivars().proxy.send_event(event);

            Retained::autorelease_return(view)
        }

        #[unsafe(method(webViewDidClose:))]
        fn web_view_did_close(&self, webview: *mut AnyObject) {
            let Some(webview) = (unsafe { webview.as_ref() }) else {
                return;
            };
            let Some(tab_id) = take_webview_tab_id(webview) else {
                return;
            };

            let event = Event::new(EventType::CloseTab(tab_id), self.ivars().window_id);
            let _ = self.ivars().proxy.send_event(event);
        }
    }
);

static NEXT_POPUP_ID: AtomicUsize = AtomicUsize::new(1);
struct MouseMonitor {
    _monitor: Retained<AnyObject>,
    _block: RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent>,
}

thread_local! {
    static PENDING_POPUPS: RefCell<HashMap<usize, PendingPopup>> = RefCell::new(HashMap::new());
    static WEBVIEW_TAB_IDS: RefCell<HashMap<usize, TabId>> = RefCell::new(HashMap::new());
    static MOUSE_MONITOR: RefCell<Option<MouseMonitor>> = RefCell::new(None);
    static LAST_MOUSE_EVENT: RefCell<Option<Retained<NSEvent>>> = RefCell::new(None);
}

impl WebViewDelegate {
    fn new(proxy: EventLoopProxy<Event>, window_id: WindowId) -> Retained<Self> {
        let mtm =
            MainThreadMarker::new().expect("WebView delegate must be created on the main thread");
        let this = WebViewDelegate::alloc(mtm).set_ivars(WebViewDelegateIvars { proxy, window_id });
        unsafe { msg_send![super(this), init] }
    }
}

fn webview_key(view: &AnyObject) -> usize {
    view as *const AnyObject as usize
}

fn register_pending_popup(popup: PendingPopup) -> usize {
    let popup_id = NEXT_POPUP_ID.fetch_add(1, Ordering::Relaxed);
    PENDING_POPUPS.with(|cell| {
        cell.borrow_mut().insert(popup_id, popup);
    });
    popup_id
}

pub(crate) fn take_pending_popup(popup_id: usize) -> Option<PendingPopup> {
    PENDING_POPUPS.with(|cell| cell.borrow_mut().remove(&popup_id))
}

fn register_webview_tab(view: &AnyObject, tab_id: TabId) {
    let key = webview_key(view);
    WEBVIEW_TAB_IDS.with(|cell| {
        cell.borrow_mut().insert(key, tab_id);
    });
}

fn unregister_webview_tab(view: &AnyObject) {
    let key = webview_key(view);
    WEBVIEW_TAB_IDS.with(|cell| {
        cell.borrow_mut().remove(&key);
    });
}

fn take_webview_tab_id(view: &AnyObject) -> Option<TabId> {
    let key = webview_key(view);
    WEBVIEW_TAB_IDS.with(|cell| cell.borrow_mut().remove(&key))
}

fn set_webview_delegate(view: &AnyObject, delegate: &AnyObject) {
    unsafe {
        let _: () = msg_send![view, setUIDelegate: delegate];
        let _: () = msg_send![view, setNavigationDelegate: delegate];
    }
}

fn safari_user_agent(view: &AnyObject) -> Result<String, Box<dyn Error>> {
    let key = NSString::from_str("userAgent");
    let value: *mut AnyObject = unsafe { msg_send![view, valueForKey: &*key] };
    if value.is_null() {
        return Err(
            std::io::Error::new(std::io::ErrorKind::Other, "WKWebView has no userAgent").into()
        );
    }

    let base_agent = unsafe { &*(value as *const NSString) }.to_string();
    if base_agent.contains("Safari/") {
        return Ok(base_agent);
    }

    let webkit_version = base_agent
        .split_whitespace()
        .find_map(|token| token.strip_prefix("AppleWebKit/"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "WKWebView userAgent missing AppleWebKit",
            )
        })?;

    let safari_version = safari_version_from_bundle()?;
    Ok(format!("{base_agent} Version/{safari_version} Safari/{webkit_version}"))
}

fn safari_version_from_bundle() -> Result<String, Box<dyn Error>> {
    let paths = ["/Applications/Safari.app", "/System/Applications/Safari.app"];
    let mut bundle: *mut AnyObject = ptr::null_mut();
    for path in paths {
        let ns_path = NSString::from_str(path);
        let candidate: *mut AnyObject =
            unsafe { msg_send![class!(NSBundle), bundleWithPath: &*ns_path] };
        if !candidate.is_null() {
            bundle = candidate;
            break;
        }
    }

    if bundle.is_null() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "Safari.app not found").into());
    }

    let key = NSString::from_str("CFBundleShortVersionString");
    let value: *mut AnyObject = unsafe { msg_send![bundle, objectForInfoDictionaryKey: &*key] };
    if value.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Safari bundle missing CFBundleShortVersionString",
        )
        .into());
    }

    Ok(unsafe { &*(value as *const NSString) }.to_string())
}

fn apply_safari_user_agent(view: &AnyObject) -> Result<(), Box<dyn Error>> {
    let agent = safari_user_agent(view)?;
    let selector = sel!(setCustomUserAgent:);
    let responds: Bool = unsafe { msg_send![view, respondsToSelector: selector] };
    if !responds.as_bool() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "WKWebView does not support setCustomUserAgent",
        )
        .into());
    }

    let agent = NSString::from_str(&agent);
    unsafe {
        let _: () = msg_send![view, setCustomUserAgent: &*agent];
    }

    Ok(())
}

fn enable_web_inspector_view(view: &AnyObject) -> Result<(), Box<dyn Error>> {
    let selector = sel!(setInspectable:);
    let responds: Bool = unsafe { msg_send![view, respondsToSelector: selector] };
    if !responds.as_bool() {
        return Ok(());
    }

    unsafe {
        let _: () = msg_send![view, setInspectable: true];
    }

    Ok(())
}

fn navigation_action_url(navigation_action: *mut AnyObject) -> Option<String> {
    let request: *mut AnyObject = unsafe { msg_send![navigation_action, request] };
    if request.is_null() {
        return None;
    }

    let url: *mut AnyObject = unsafe { msg_send![request, URL] };
    if url.is_null() {
        return None;
    }

    let absolute: *mut AnyObject = unsafe { msg_send![url, absoluteString] };
    if absolute.is_null() {
        return None;
    }

    Some(unsafe { &*(absolute as *const NSString) }.to_string())
}

fn configure_webview_config(config: &AnyObject) -> Result<(), Box<dyn Error>> {
    enable_web_authentication(config)?;
    enable_web_inspector(config)?;
    enable_web_popups(config)?;
    Ok(())
}

fn install_mouse_monitor() -> Result<(), Box<dyn Error>> {
    MOUSE_MONITOR.with(|cell| {
        if cell.borrow().is_some() {
            return Ok(());
        }

        let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
            let event_ptr = event.as_ptr();
            let retained = unsafe { Retained::retain(event_ptr) };
            LAST_MOUSE_EVENT.with(|last| {
                *last.borrow_mut() = retained;
            });
            event_ptr
        });

        let mask = NSEventMask::LeftMouseDown
            | NSEventMask::LeftMouseUp
            | NSEventMask::RightMouseDown
            | NSEventMask::RightMouseUp
            | NSEventMask::OtherMouseDown
            | NSEventMask::OtherMouseUp;
        let monitor =
            unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Failed to install NSEvent mouse monitor",
                    )
                })?;
        *cell.borrow_mut() = Some(MouseMonitor { _monitor: monitor, _block: block });
        Ok(())
    })
}

fn take_last_mouse_event() -> Option<Retained<NSEvent>> {
    LAST_MOUSE_EVENT.with(|cell| cell.borrow_mut().take())
}

impl WebView {
    pub fn new(
        window: &Window,
        size_info: &SizeInfo,
        tab_id: TabId,
        url: &str,
        proxy: &EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let _mtm = MainThreadMarker::new().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "WebView must be created on main thread")
        })?;

        super::register_webview();
        install_mouse_monitor()?;
        let result = (|| {
            let parent = ns_view(window)?;
            let config: *mut AnyObject = unsafe { msg_send![class!(WKWebViewConfiguration), new] };
            let config = unsafe { Retained::from_raw(config) }.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to allocate WKWebViewConfiguration",
                )
            })?;
            configure_webview_config(&*config)?;
            let store: *mut AnyObject =
                unsafe { msg_send![class!(WKWebsiteDataStore), defaultDataStore] };
            unsafe {
                let _: () = msg_send![&*config, setWebsiteDataStore: store];
            }

            let frame = webview_frame(window, size_info);
            let view: *mut AnyObject = unsafe { msg_send![class!(WKWebView), alloc] };
            let view: *mut AnyObject =
                unsafe { msg_send![view, initWithFrame: frame, configuration: &*config] };
            let view = unsafe { Retained::from_raw(view) }.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "Failed to init WKWebView")
            })?;

            unsafe {
                let _: () = msg_send![parent, addSubview: &*view];
            }

            let delegate = WebViewDelegate::new(proxy.clone(), window.id());
            let delegate = unsafe { Retained::cast_unchecked(delegate) };
            set_webview_delegate(&view, &delegate);
            register_webview_tab(&view, tab_id);
            apply_safari_user_agent(&view)?;
            enable_web_inspector_view(&view)?;

            let mut web_view = Self { view, last_title: None, last_url: None, _delegate: delegate };
            let initial_url = if url.is_empty() { "about:blank" } else { url };
            web_view.load_url(initial_url);
            Ok(web_view)
        })();

        if result.is_err() {
            super::unregister_webview();
        }

        result
    }

    pub fn from_existing(
        window: &Window,
        size_info: &SizeInfo,
        tab_id: TabId,
        view: Retained<AnyObject>,
        delegate: Retained<AnyObject>,
    ) -> Result<Self, Box<dyn Error>> {
        let _mtm = MainThreadMarker::new().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "WebView must be created on main thread")
        })?;

        super::register_webview();
        install_mouse_monitor()?;
        let result = (|| {
            let parent = ns_view(window)?;
            let config: *mut AnyObject = unsafe { msg_send![&*view, configuration] };
            let config = unsafe { config.as_ref() }.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "WKWebView has no configuration")
            })?;
            configure_webview_config(config)?;

            unsafe {
                let _: () = msg_send![parent, addSubview: &*view];
            }

            let frame = webview_frame(window, size_info);
            unsafe {
                let _: () = msg_send![&*view, setFrame: frame];
                let _: () = msg_send![&*view, setHidden: true];
            }

            set_webview_delegate(&view, &delegate);
            register_webview_tab(&view, tab_id);
            apply_safari_user_agent(&view)?;
            enable_web_inspector_view(&view)?;

            Ok(Self { view, last_title: None, last_url: None, _delegate: delegate })
        })();

        if result.is_err() {
            super::unregister_webview();
        }

        result
    }

    pub fn set_visible(&mut self, visible: bool) {
        unsafe {
            let _: () = msg_send![&*self.view, setHidden: !visible];
        }
        if visible {
            self.invalidate_cursor_rects();
        }
    }

    pub fn update_frame(&mut self, window: &Window, size_info: &SizeInfo) {
        let frame = webview_frame(window, size_info);
        unsafe {
            let _: () = msg_send![&*self.view, setFrame: frame];
        }
        self.invalidate_cursor_rects();
    }

    pub fn invalidate_cursor_rects(&self) {
        let window: *mut AnyObject = unsafe { msg_send![&*self.view, window] };
        if window.is_null() {
            return;
        }
        unsafe {
            let _: () = msg_send![window, invalidateCursorRectsForView: &*self.view];
        }
    }

    pub fn load_url(&mut self, url: &str) -> bool {
        self.last_title = None;
        self.last_url = None;
        let url = NSString::from_str(url);
        let ns_url: *mut AnyObject = unsafe { msg_send![class!(NSURL), URLWithString: &*url] };
        if ns_url.is_null() {
            return false;
        }

        let scheme: *mut AnyObject = unsafe { msg_send![ns_url, scheme] };
        if !scheme.is_null() {
            let scheme = unsafe { &*(scheme as *const NSString) }.to_string();
            if scheme == "file" {
                let access_url: *mut AnyObject =
                    unsafe { msg_send![ns_url, URLByDeletingLastPathComponent] };
                let access_url = if access_url.is_null() { ns_url } else { access_url };
                let _: *mut AnyObject = unsafe {
                    msg_send![&*self.view, loadFileURL: ns_url, allowingReadAccessToURL: access_url]
                };
                return true;
            }
        }

        let request: *mut AnyObject =
            unsafe { msg_send![class!(NSURLRequest), requestWithURL: ns_url] };
        let _: *mut AnyObject = unsafe { msg_send![&*self.view, loadRequest: request] };
        true
    }

    pub fn reload(&mut self) {
        unsafe {
            let _: () = msg_send![&*self.view, reload];
        }
    }

    pub fn go_back(&mut self) {
        unsafe {
            let _: () = msg_send![&*self.view, goBack];
        }
    }

    pub fn go_forward(&mut self) {
        unsafe {
            let _: () = msg_send![&*self.view, goForward];
        }
    }

    pub fn handle_mouse_input(
        &mut self,
        window: &Window,
        size_info: &SizeInfo,
        position: PhysicalPosition<f64>,
        state: ElementState,
        button: MouseButton,
        modifiers: NSEventModifierFlags,
    ) -> bool {
        let mtm = MainThreadMarker::new().expect("WebView input requires main thread");

        let scale_factor = window.scale_factor as f64;
        let origin_x = f64::from(size_info.padding_x()) / scale_factor;
        let origin_y = f64::from(size_info.padding_y()) / scale_factor;
        let width =
            f64::from(size_info.width() - size_info.padding_x() - size_info.padding_right())
                / scale_factor;
        let height =
            f64::from(size_info.cell_height() * size_info.screen_lines() as f32) / scale_factor;

        let view_point = NSPoint::new(position.x / scale_factor, position.y / scale_factor);
        let local_x = view_point.x - origin_x;
        let local_y = view_point.y - origin_y;
        if local_x < 0.0 || local_y < 0.0 || local_x >= width || local_y >= height {
            return false;
        }

        let ns_view = match window.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => handle.ns_view.as_ptr() as *mut AnyObject,
            _ => return false,
        };
        if ns_view.is_null() {
            return false;
        }

        let window_point: NSPoint = unsafe {
            msg_send![ns_view, convertPoint: view_point, toView: ptr::null::<AnyObject>()]
        };
        let ns_window: *mut AnyObject = unsafe { msg_send![ns_view, window] };
        if ns_window.is_null() {
            return false;
        }
        let window_number: NSInteger = unsafe { msg_send![ns_window, windowNumber] };

        let event_type = match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => NSEventType::LeftMouseDown,
            (MouseButton::Left, ElementState::Released) => NSEventType::LeftMouseUp,
            (MouseButton::Right, ElementState::Pressed) => NSEventType::RightMouseDown,
            (MouseButton::Right, ElementState::Released) => NSEventType::RightMouseUp,
            (MouseButton::Middle, ElementState::Pressed)
            | (MouseButton::Back, ElementState::Pressed)
            | (MouseButton::Forward, ElementState::Pressed)
            | (MouseButton::Other(_), ElementState::Pressed) => NSEventType::OtherMouseDown,
            (MouseButton::Middle, ElementState::Released)
            | (MouseButton::Back, ElementState::Released)
            | (MouseButton::Forward, ElementState::Released)
            | (MouseButton::Other(_), ElementState::Released) => NSEventType::OtherMouseUp,
        };

        // Prefer the OS event so WebKit sees a trusted user gesture (needed for WebAuthn).
        let event = take_last_mouse_event()
            .filter(|event| event.r#type() == event_type && event.windowNumber() == window_number)
            .or_else(|| {
                NSApplication::sharedApplication(mtm)
                    .currentEvent()
                    .filter(|event| {
                        event.r#type() == event_type && event.windowNumber() == window_number
                    })
            })
            .unwrap_or_else(|| {
                NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                    event_type,
                    window_point,
                    modifiers,
                    0.0,
                    window_number,
                    None,
                    0,
                    1,
                    0.0,
                )
                .expect("Failed to synthesize NSEvent for WebView input")
            });

        unsafe {
            let _: Bool = msg_send![ns_window, makeFirstResponder: &*self.view];
        }

        unsafe {
            match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => {
                    let _: () = msg_send![&*self.view, mouseDown: &*event];
                },
                (MouseButton::Left, ElementState::Released) => {
                    let _: () = msg_send![&*self.view, mouseUp: &*event];
                },
                (MouseButton::Right, ElementState::Pressed) => {
                    let _: () = msg_send![&*self.view, rightMouseDown: &*event];
                },
                (MouseButton::Right, ElementState::Released) => {
                    let _: () = msg_send![&*self.view, rightMouseUp: &*event];
                },
                (MouseButton::Middle, ElementState::Pressed)
                | (MouseButton::Back, ElementState::Pressed)
                | (MouseButton::Forward, ElementState::Pressed)
                | (MouseButton::Other(_), ElementState::Pressed) => {
                    let _: () = msg_send![&*self.view, otherMouseDown: &*event];
                },
                (MouseButton::Middle, ElementState::Released)
                | (MouseButton::Back, ElementState::Released)
                | (MouseButton::Forward, ElementState::Released)
                | (MouseButton::Other(_), ElementState::Released) => {
                    let _: () = msg_send![&*self.view, otherMouseUp: &*event];
                },
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
        let _mtm = MainThreadMarker::new().expect("WebView JS requires main thread");
        let script = NSString::from_str(script);
        let callback = Rc::new(RefCell::new(Some(callback)));
        let block = RcBlock::new({
            let callback = Rc::clone(&callback);
            move |result: *mut AnyObject, error: *mut AnyObject| {
                let Some(callback) = callback.borrow_mut().take() else {
                    return;
                };

                if !error.is_null() {
                    let error_desc: *mut AnyObject = unsafe { msg_send![error, description] };
                    if !error_desc.is_null() {
                        let error_str = unsafe { &*(error_desc as *const NSString) }.to_string();
                        debug!("WebView JS error: {error_str}");
                    }
                    callback(None);
                    return;
                }

                if result.is_null() {
                    callback(None);
                    return;
                }

                let desc: *mut AnyObject = unsafe { msg_send![result, description] };
                if desc.is_null() {
                    callback(None);
                    return;
                }

                let output = unsafe { &*(desc as *const NSString) }.to_string();
                callback(Some(output));
            }
        });

        unsafe {
            let _: () =
                msg_send![&*self.view, evaluateJavaScript: &*script, completionHandler: &*block];
        }
    }

    pub fn snapshot_png<F>(&mut self, full: bool, callback: F)
    where
        F: FnOnce(Result<Vec<u8>, String>) + 'static,
    {
        let view = self.view.clone();
        if !responds_to_selector(&view, sel!(takeSnapshotWithConfiguration:completionHandler:)) {
            callback(Err(String::from("WebView snapshot is unavailable")));
            return;
        }

        if full {
            let view = view.clone();
            let callback = Rc::new(RefCell::new(Some(callback)));
            self.eval_js_string(
                "JSON.stringify({width: document.documentElement.scrollWidth, height: document.documentElement.scrollHeight})",
                move |result| {
                    let Some(callback) = callback.borrow_mut().take() else {
                        return;
                    };
                    let rect = result
                        .and_then(|value| serde_json::from_str::<JsonValue>(&value).ok())
                        .and_then(|value| {
                            let width = value.get("width").and_then(|v| v.as_f64())?;
                            let height = value.get("height").and_then(|v| v.as_f64())?;
                            Some(CGRect {
                                origin: CGPoint { x: 0.0, y: 0.0 },
                                size: CGSize {
                                    width: width as CGFloat,
                                    height: height as CGFloat,
                                },
                            })
                        });
                    snapshot_view(view, rect, callback);
                },
            );
            return;
        }

        snapshot_view(view, None, callback);
    }

    pub fn pdf<F>(&mut self, callback: F)
    where
        F: FnOnce(Result<Vec<u8>, String>) + 'static,
    {
        let view = self.view.clone();
        if !responds_to_selector(&view, sel!(createPDFWithConfiguration:completionHandler:)) {
            callback(Err(String::from("WebView PDF export is unavailable")));
            return;
        }

        let config: *mut AnyObject = unsafe { msg_send![class!(WKPDFConfiguration), new] };
        let config = unsafe { Retained::from_raw(config) }
            .ok_or_else(|| String::from("Failed to allocate WKPDFConfiguration"));
        let Ok(config) = config else {
            callback(Err(String::from("Failed to allocate WKPDFConfiguration")));
            return;
        };

        let callback = Rc::new(RefCell::new(Some(callback)));
        let block = RcBlock::new({
            let callback = Rc::clone(&callback);
            move |data: *mut AnyObject, error: *mut AnyObject| {
                let Some(callback) = callback.borrow_mut().take() else {
                    return;
                };
                if !error.is_null() {
                    let error_desc: *mut AnyObject = unsafe { msg_send![error, description] };
                    let message = if error_desc.is_null() {
                        String::from("Failed to render PDF")
                    } else {
                        unsafe { &*(error_desc as *const NSString) }.to_string()
                    };
                    callback(Err(message));
                    return;
                }
                if data.is_null() {
                    callback(Err(String::from("PDF data unavailable")));
                    return;
                }
                let bytes: *const c_void = unsafe { msg_send![data, bytes] };
                let bytes = bytes as *const u8;
                let length: usize = unsafe { msg_send![data, length] };
                if bytes.is_null() || length == 0 {
                    callback(Err(String::from("PDF data unavailable")));
                    return;
                }
                let slice = unsafe { std::slice::from_raw_parts(bytes, length) };
                callback(Ok(slice.to_vec()));
            }
        });

        unsafe {
            let _: () =
                msg_send![&*view, createPDFWithConfiguration: &*config, completionHandler: &*block];
        }
    }

    pub fn poll_title(&mut self) -> Option<String> {
        let title: *mut AnyObject = unsafe { msg_send![&*self.view, title] };
        if title.is_null() {
            return None;
        }

        let title = unsafe { &*(title as *const NSString) }.to_string();
        if self.last_title.as_deref() == Some(&title) {
            return None;
        }

        self.last_title = Some(title.clone());
        Some(title)
    }

    pub fn poll_url(&mut self) -> Option<String> {
        let url: *mut AnyObject = unsafe { msg_send![&*self.view, URL] };
        if url.is_null() {
            return None;
        }

        let absolute: *mut AnyObject = unsafe { msg_send![url, absoluteString] };
        if absolute.is_null() {
            return None;
        }

        let url = unsafe { &*(absolute as *const NSString) }.to_string();
        if self.last_url.as_deref() == Some(&url) {
            return None;
        }

        self.last_url = Some(url.clone());
        Some(url)
    }

    pub fn current_url(&self) -> Option<String> {
        let url: *mut AnyObject = unsafe { msg_send![&*self.view, URL] };
        if url.is_null() {
            return None;
        }

        let absolute: *mut AnyObject = unsafe { msg_send![url, absoluteString] };
        if absolute.is_null() {
            return None;
        }

        Some(unsafe { &*(absolute as *const NSString) }.to_string())
    }

    pub fn show_inspector(&mut self) -> bool {
        let inspector: *mut AnyObject = unsafe { msg_send![&*self.view, _inspector] };
        if inspector.is_null() {
            return false;
        }

        unsafe {
            let _: () = msg_send![inspector, show];
        }

        true
    }
}

fn enable_web_inspector(config: &AnyObject) -> Result<(), Box<dyn Error>> {
    let prefs: *mut AnyObject = unsafe { msg_send![config, preferences] };
    if prefs.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "WKWebViewConfiguration has no preferences",
        )
        .into());
    }

    let enabled = NSNumber::numberWithBool(true);
    let key = NSString::from_str("developerExtrasEnabled");
    unsafe {
        let _: () = msg_send![prefs, setValue: &*enabled, forKey: &*key];
    }

    Ok(())
}

fn enable_web_popups(config: &AnyObject) -> Result<(), Box<dyn Error>> {
    let prefs: *mut AnyObject = unsafe { msg_send![config, preferences] };
    if prefs.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "WKWebViewConfiguration has no preferences",
        )
        .into());
    }

    let enabled = NSNumber::numberWithBool(true);
    let key = NSString::from_str("javaScriptCanOpenWindowsAutomatically");
    unsafe {
        let _: () = msg_send![prefs, setValue: &*enabled, forKey: &*key];
    }

    Ok(())
}

// WebAuthn/passkeys are guarded by WebKit preferences; enable them explicitly.
fn enable_web_authentication(config: &AnyObject) -> Result<(), Box<dyn Error>> {
    type WebAuthGet = unsafe extern "C" fn(*mut AnyObject) -> Bool;
    type WebAuthSet = unsafe extern "C" fn(*mut AnyObject, Bool);

    let get_ptr = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            b"_WKPreferencesGetWebAuthenticationEnabled\0".as_ptr() as *const c_char,
        )
    };
    let set_ptr = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            b"_WKPreferencesSetWebAuthenticationEnabled\0".as_ptr() as *const c_char,
        )
    };

    if get_ptr.is_null() || set_ptr.is_null() {
        return Ok(());
    }

    let get = unsafe { std::mem::transmute::<*mut c_void, WebAuthGet>(get_ptr) };
    let set = unsafe { std::mem::transmute::<*mut c_void, WebAuthSet>(set_ptr) };

    let prefs: *mut AnyObject = unsafe { msg_send![config, preferences] };
    if prefs.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "WKWebViewConfiguration has no preferences",
        )
        .into());
    }

    unsafe {
        set(prefs, Bool::YES);
    }

    let enabled = unsafe { get(prefs) };
    if !enabled.as_bool() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to enable WebAuthentication support",
        )
        .into());
    }

    Ok(())
}

impl Drop for WebView {
    fn drop(&mut self) {
        unregister_webview_tab(&self.view);
        unsafe {
            let _: () = msg_send![&*self.view, removeFromSuperview];
        }
        super::unregister_webview();
    }
}

fn ns_view(window: &Window) -> Result<*mut AnyObject, Box<dyn Error>> {
    match window.raw_window_handle() {
        RawWindowHandle::AppKit(handle) => Ok(handle.ns_view.as_ptr() as *mut AnyObject),
        _ => {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "WebView requires an AppKit window")
                .into())
        },
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

fn responds_to_selector(object: &AnyObject, selector: objc2::runtime::Sel) -> bool {
    let responds: Bool = unsafe { msg_send![object, respondsToSelector: selector] };
    responds.as_bool()
}

fn snapshot_view<F>(view: Retained<AnyObject>, rect: Option<CGRect>, callback: F)
where
    F: FnOnce(Result<Vec<u8>, String>) + 'static,
{
    let config: *mut AnyObject = unsafe { msg_send![class!(WKSnapshotConfiguration), new] };
    let config = unsafe { Retained::from_raw(config) }
        .ok_or_else(|| String::from("Failed to allocate WKSnapshotConfiguration"));
    let Ok(config) = config else {
        callback(Err(String::from("Failed to allocate WKSnapshotConfiguration")));
        return;
    };

    if let Some(rect) = rect {
        unsafe {
            let _: () = msg_send![&*config, setRect: rect];
        }
    }

    let callback = Rc::new(RefCell::new(Some(callback)));
    let block = RcBlock::new({
        let callback = Rc::clone(&callback);
        move |image: *mut AnyObject, error: *mut AnyObject| {
            let Some(callback) = callback.borrow_mut().take() else {
                return;
            };

            if !error.is_null() {
                let error_desc: *mut AnyObject = unsafe { msg_send![error, description] };
                let message = if error_desc.is_null() {
                    String::from("Failed to capture snapshot")
                } else {
                    unsafe { &*(error_desc as *const NSString) }.to_string()
                };
                callback(Err(message));
                return;
            }

            if image.is_null() {
                callback(Err(String::from("Snapshot data unavailable")));
                return;
            }

            callback(nsimage_to_png(image));
        }
    });

    unsafe {
        let _: () =
            msg_send![&*view, takeSnapshotWithConfiguration: &*config, completionHandler: &*block];
    }
}

fn nsimage_to_png(image: *mut AnyObject) -> Result<Vec<u8>, String> {
    let tiff: *mut AnyObject = unsafe { msg_send![image, TIFFRepresentation] };
    if tiff.is_null() {
        return Err(String::from("Snapshot data unavailable"));
    }

    let bitmap: *mut AnyObject =
        unsafe { msg_send![class!(NSBitmapImageRep), imageRepWithData: tiff] };
    if bitmap.is_null() {
        return Err(String::from("Snapshot data unavailable"));
    }

    let png_data: *mut AnyObject = unsafe {
        msg_send![bitmap, representationUsingType: NS_BITMAP_IMAGE_FILE_TYPE_PNG, properties: ptr::null::<AnyObject>()]
    };
    if png_data.is_null() {
        return Err(String::from("Snapshot data unavailable"));
    }

    let bytes: *const c_void = unsafe { msg_send![png_data, bytes] };
    let bytes = bytes as *const u8;
    let length: usize = unsafe { msg_send![png_data, length] };
    if bytes.is_null() || length == 0 {
        return Err(String::from("Snapshot data unavailable"));
    }

    let slice = unsafe { std::slice::from_raw_parts(bytes, length) };
    Ok(slice.to_vec())
}
