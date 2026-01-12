#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
#[path = "../macos/web_cursor.rs"]
mod web_cursor;

#[cfg(target_os = "macos")]
mod smoke {
    use std::cell::RefCell;
    use std::error::Error;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use libc::{c_char, c_void};
    use objc2::encode::{Encode, Encoding};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send, MainThreadMarker};
    use objc2_foundation::NSString;
    use winit::application::ApplicationHandler;
    use winit::dpi::PhysicalSize;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{CursorIcon, Window, WindowAttributes};

    use crate::web_cursor;

    const TIMEOUT: Duration = Duration::from_secs(10);
    const DATA_URL: &str = concat!(
        "data:text/html;base64,",
        "PCFkb2N0eXBlIGh0bWw+CjxodG1sPgo8aGVhZD4KPG1ldGEgY2hhcnNldD0idXRmLTgiPgo8c3R5bGU+",
        "CiAgYm9keSB7IG1hcmdpbjogMDsgfQogICNsaW5rIHsgcG9zaXRpb246IGFic29sdXRlOyBsZWZ0OiAy",
        "MHB4OyB0b3A6IDIwcHg7IHdpZHRoOiAxMjBweDsgaGVpZ2h0OiAyNHB4OyB9CiAgI2lucHV0IHsgcG9z",
        "aXRpb246IGFic29sdXRlOyBsZWZ0OiAyMHB4OyB0b3A6IDYwcHg7IHdpZHRoOiAyMDBweDsgaGVpZ2h0",
        "OiAyNHB4OyB9CiAgI3BsYWluIHsgcG9zaXRpb246IGFic29sdXRlOyBsZWZ0OiAyMHB4OyB0b3A6IDEw",
        "MHB4OyB3aWR0aDogMTIwcHg7IGhlaWdodDogMjRweDsgfQo8L3N0eWxlPgo8L2hlYWQ+Cjxib2R5Pgog",
        "IDxhIGlkPSJsaW5rIiBocmVmPSJodHRwczovL2V4YW1wbGUuY29tIj5MaW5rPC9hPgogIDxpbnB1dCBp",
        "ZD0iaW5wdXQiIHR5cGU9InRleHQiIHZhbHVlPSJUZXh0IiAvPgogIDxkaXYgaWQ9InBsYWluIj5QbGFp",
        "bjwvZGl2Pgo8L2JvZHk+CjwvaHRtbD4K"
    );

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
        const ENCODING: Encoding =
            Encoding::Struct("CGPoint", &[CGFloat::ENCODING, CGFloat::ENCODING]);
    }

    #[repr(C)]
    struct CGSize {
        width: CGFloat,
        height: CGFloat,
    }

    // SAFETY: The struct is `repr(C)`, and the encoding is correct.
    unsafe impl Encode for CGSize {
        const ENCODING: Encoding =
            Encoding::Struct("CGSize", &[CGFloat::ENCODING, CGFloat::ENCODING]);
    }

    #[repr(C)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    // SAFETY: The struct is `repr(C)`, and the encoding is correct.
    unsafe impl Encode for CGRect {
        const ENCODING: Encoding =
            Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
    }

    struct WebViewSmoke {
        view: Retained<AnyObject>,
    }

    impl WebViewSmoke {
        fn new(window: &Window, url: &str) -> Result<Self, Box<dyn Error>> {
            let _mtm = MainThreadMarker::new().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "WebView must be created on main thread",
                )
            })?;

            let parent = ns_view(window)?;
            let config: *mut AnyObject = unsafe { msg_send![class!(WKWebViewConfiguration), new] };
            let config = unsafe { Retained::from_raw(config) }.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to allocate WKWebViewConfiguration",
                )
            })?;
            enable_web_authentication(&*config)?;
            let store: *mut AnyObject =
                unsafe { msg_send![class!(WKWebsiteDataStore), defaultDataStore] };
            unsafe {
                let _: () = msg_send![&*config, setWebsiteDataStore: store];
            }

            let frame = webview_frame(window);
            let view: *mut AnyObject = unsafe { msg_send![class!(WKWebView), alloc] };
            let view: *mut AnyObject =
                unsafe { msg_send![view, initWithFrame: frame, configuration: &*config] };
            let view = unsafe { Retained::from_raw(view) }.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "Failed to init WKWebView")
            })?;

            unsafe {
                let _: () = msg_send![parent, addSubview: &*view];
            }

            let mut web_view = Self { view };
            if !web_view.load_url(url) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid URL").into());
            }
            Ok(web_view)
        }

        fn update_frame(&mut self, window: &Window) {
            let frame = webview_frame(window);
            unsafe {
                let _: () = msg_send![&*self.view, setFrame: frame];
            }
        }

        fn load_url(&mut self, url: &str) -> bool {
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

        fn eval_js_string<F>(&mut self, script: &str, callback: F)
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

                    if !error.is_null() || result.is_null() {
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

        fn is_loading(&self) -> bool {
            let loading: Bool = unsafe { msg_send![&*self.view, isLoading] };
            loading.as_bool()
        }
    }

    impl Drop for WebViewSmoke {
        fn drop(&mut self) {
            unsafe {
                let _: () = msg_send![&*self.view, removeFromSuperview];
            }
        }
    }

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

    fn ns_view(window: &Window) -> Result<*mut AnyObject, Box<dyn Error>> {
        match window.window_handle()?.as_raw() {
            RawWindowHandle::AppKit(handle) => Ok(handle.ns_view.as_ptr() as *mut AnyObject),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "WebView requires an AppKit window",
            )
            .into()),
        }
    }

    fn webview_frame(window: &Window) -> CGRect {
        let size = window.inner_size();
        let scale_factor = window.scale_factor();
        let width = (size.width as f64 / scale_factor) as CGFloat;
        let height = (size.height as f64 / scale_factor) as CGFloat;

        CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize { width, height },
        }
    }

    struct CursorProbe {
        name: &'static str,
        script: String,
        expected: CursorIcon,
        result: Rc<RefCell<Option<Option<String>>>>,
        started: bool,
        done: bool,
    }

    impl CursorProbe {
        fn new(name: &'static str, script: String, expected: CursorIcon) -> Self {
            Self {
                name,
                script,
                expected,
                result: Rc::new(RefCell::new(None)),
                started: false,
                done: false,
            }
        }
    }

    struct App {
        window: Option<Window>,
        web_view: Option<WebViewSmoke>,
        probes: Vec<CursorProbe>,
        started_at: Instant,
        result: Option<bool>,
    }

    impl App {
        fn new() -> Self {
            Self {
                window: None,
                web_view: None,
                probes: Vec::new(),
                started_at: Instant::now(),
                result: None,
            }
        }

        fn finish(&mut self, event_loop: &ActiveEventLoop, ok: bool) {
            self.result = Some(ok);
            event_loop.exit();
        }
    }

    impl ApplicationHandler<()> for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }

            let attributes = WindowAttributes::default()
                .with_title("Tabor Web Cursor Smoke")
                .with_inner_size(PhysicalSize::new(600, 400));
            let window = match event_loop.create_window(attributes) {
                Ok(window) => window,
                Err(_) => {
                    self.finish(event_loop, false);
                    return;
                },
            };

            let web_view = match WebViewSmoke::new(&window, DATA_URL) {
                Ok(web_view) => web_view,
                Err(_) => {
                    self.finish(event_loop, false);
                    return;
                },
            };

            let probe_script = |x: f64, y: f64| {
                format!(
                    "{};{}",
                    web_cursor::WEB_CURSOR_BOOTSTRAP,
                    web_cursor::web_cursor_script(x, y)
                )
            };
            self.probes = vec![
                CursorProbe::new("link", probe_script(30.0, 30.0), CursorIcon::Pointer),
                CursorProbe::new("input", probe_script(30.0, 70.0), CursorIcon::Text),
                CursorProbe::new("plain", probe_script(30.0, 110.0), CursorIcon::Default),
            ];
            self.started_at = Instant::now();
            self.window = Some(window);
            self.web_view = Some(web_view);
            event_loop.set_control_flow(ControlFlow::Poll);
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _window_id: winit::window::WindowId,
            event: WindowEvent,
        ) {
            match event {
                WindowEvent::CloseRequested => self.finish(event_loop, false),
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                    if let (Some(window), Some(web_view)) =
                        (self.window.as_ref(), self.web_view.as_mut())
                    {
                        web_view.update_frame(window);
                    }
                },
                _ => (),
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self.result.is_some() {
                return;
            }

            if self.started_at.elapsed() > TIMEOUT {
                self.finish(event_loop, false);
                return;
            }

            let Some(web_view) = self.web_view.as_mut() else {
                return;
            };

            if web_view.is_loading() {
                return;
            }

            for probe in &mut self.probes {
                if !probe.started {
                    let result = Rc::clone(&probe.result);
                    web_view.eval_js_string(&probe.script, move |value| {
                        *result.borrow_mut() = Some(value);
                    });
                    probe.started = true;
                }
            }

            let mut all_done = true;
            for probe in &mut self.probes {
                if probe.done {
                    continue;
                }
                all_done = false;

                let outcome = probe.result.borrow_mut().take();
                let Some(outcome) = outcome else {
                    continue;
                };

                let Some(value) = outcome else {
                    eprintln!("cursor probe {} returned empty result", probe.name);
                    self.finish(event_loop, false);
                    return;
                };

                let cursor = web_cursor::web_cursor_from_css(&value)
                    .unwrap_or(CursorIcon::Default);
                if cursor != probe.expected {
                    eprintln!(
                        "cursor probe {} expected {:?}, got {:?} ({})",
                        probe.name, probe.expected, cursor, value
                    );
                    self.finish(event_loop, false);
                    return;
                }
                probe.done = true;
            }

            if all_done {
                self.finish(event_loop, true);
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let event_loop = EventLoop::new()?;
        let mut app = App::new();
        event_loop.run_app(&mut app)?;

        match app.result {
            Some(true) => Ok(()),
            _ => Err(
                std::io::Error::new(std::io::ErrorKind::Other, "WebView cursor smoke failed")
                    .into(),
            ),
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(err) = smoke::run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
