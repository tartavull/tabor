use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;
use std::ptr;
use std::ptr::NonNull;

use block2::RcBlock;
use log::debug;
use objc2::encode::{Encode, Encoding};
use objc2::ffi::NSInteger;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::runtime::Bool;
use objc2::{class, msg_send, sel, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
use objc2_foundation::{NSNumber, NSPoint, NSString};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton};
use winit::raw_window_handle::RawWindowHandle;

use tabor_terminal::grid::Dimensions;

use crate::display::SizeInfo;
use crate::display::window::Window;
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

pub struct WebView {
    view: Retained<AnyObject>,
    last_title: Option<String>,
    last_url: Option<String>,
}

struct MouseMonitor {
    _monitor: Retained<AnyObject>,
    _block: RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent>,
}

thread_local! {
    static MOUSE_MONITOR: RefCell<Option<MouseMonitor>> = RefCell::new(None);
    static LAST_MOUSE_EVENT: RefCell<Option<Retained<NSEvent>>> = RefCell::new(None);
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
        let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }
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
    pub fn new(window: &Window, size_info: &SizeInfo, url: &str) -> Result<Self, Box<dyn Error>> {
        let _mtm = MainThreadMarker::new().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "WebView must be created on main thread",
            )
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
            enable_web_authentication(&*config)?;
            enable_web_inspector(&*config)?;
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

            let mut web_view = Self { view, last_title: None, last_url: None };
            let initial_url = if url.is_empty() { "about:blank" } else { url };
            web_view.load_url(initial_url);
            Ok(web_view)
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
        let width = f64::from(size_info.width() - size_info.padding_x() - size_info.padding_right())
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
        let selectors = [
            sel!(showWebInspector),
            sel!(showInspector),
            sel!(_showInspector),
            sel!(_showWebInspector),
            sel!(toggleWebInspector),
            sel!(toggleInspector),
            sel!(_toggleInspector),
        ];

        for selector in selectors {
            let responds: Bool = unsafe { msg_send![&*self.view, respondsToSelector: selector] };
            if responds.as_bool() {
                unsafe {
                    let _: *mut AnyObject = msg_send![&*self.view, performSelector: selector];
                }
                return true;
            }
        }

        false
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
        unsafe {
            let _: () = msg_send![&*self.view, removeFromSuperview];
        }
        super::unregister_webview();
    }
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
    let width =
        (f64::from(size_info.width() - size_info.padding_x() - size_info.padding_right())
            / scale_factor) as CGFloat;
    let height =
        (f64::from(size_info.cell_height() * size_info.screen_lines() as f32) / scale_factor)
            as CGFloat;

    CGRect {
        origin: CGPoint { x, y },
        size: CGSize { width, height },
    }
}
