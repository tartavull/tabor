#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod smoke {
    use std::error::Error;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration, Instant};

    use libc::{c_char, c_void};
    use objc2::encode::{Encode, Encoding};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{MainThreadMarker, class, msg_send};
    use objc2_foundation::{NSDictionary, NSString, NSUserDefaults, ns_string};
    use winit::application::ApplicationHandler;
    use winit::dpi::PhysicalSize;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowAttributes};

    const TITLE_AVAILABLE: &str = "PASSKEY_AVAILABLE";
    const TITLE_UNAVAILABLE: &str = "PASSKEY_UNAVAILABLE";
    const TITLE_UNSUPPORTED: &str = "PASSKEY_UNSUPPORTED";
    const TITLE_UNSUPPORTED_PREFIX: &str = "PASSKEY_UNSUPPORTED:";
    const TITLE_ERROR: &str = "PASSKEY_ERROR";
    const TITLE_ERROR_PREFIX: &str = "PASSKEY_ERROR:";
    const TIMEOUT: Duration = Duration::from_secs(20);

    const HTML_BODY: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>PASSKEY_LOADING</title>
<body>
<pre id="status">PASSKEY_LOADING</pre>
<script>
(function () {
  const statusEl = document.getElementById("status");
  const setStatus = (status, detail) => {
    const text = detail ? `${status}:${detail}` : status;
    document.title = text;
    if (statusEl) {
      statusEl.textContent = text;
    }
  };
  const unsupported = (reason) => setStatus("PASSKEY_UNSUPPORTED", reason);
  const reportError = (err) => {
    const name = err && err.name ? err.name : String(err);
    setStatus("PASSKEY_ERROR", name);
  };

  const secure = window.isSecureContext === true;
  const hasCreds = !!(navigator && navigator.credentials);
  const hasPK = typeof PublicKeyCredential !== "undefined";
  if (!secure) {
    unsupported("insecure-context");
    return;
  }
  if (!hasCreds) {
    unsupported("missing-credentials");
    return;
  }
  if (!hasPK) {
    unsupported("missing-publickey");
    return;
  }
  if (typeof PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable !== "function") {
    unsupported("missing-uvpaa");
    return;
  }

  PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable()
    .then((available) => {
      setStatus(available ? "PASSKEY_AVAILABLE" : "PASSKEY_UNAVAILABLE");
    })
    .catch(reportError);
})();
</script>
"#;

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

    struct AutofillOverride;

    impl AutofillOverride {
        fn enable() -> Self {
            unsafe {
                NSUserDefaults::standardUserDefaults().registerDefaults(&NSDictionary::<
                    NSString,
                    AnyObject,
                >::from_slices(
                    &[ns_string!("NSAutoFillHeuristicControllerEnabled")],
                    &[ns_string!("NO")],
                ));
            }

            NSUserDefaults::standardUserDefaults()
                .setBool_forKey(true, ns_string!("NSAutoFillHeuristicControllerEnabled"));
            Self
        }
    }

    impl Drop for AutofillOverride {
        fn drop(&mut self) {
            NSUserDefaults::standardUserDefaults()
                .removeObjectForKey(ns_string!("NSAutoFillHeuristicControllerEnabled"));
        }
    }

    struct WebViewSmoke {
        view: Retained<AnyObject>,
        last_title: Option<String>,
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
            Self::enable_web_authentication(&*config)?;
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

            let mut web_view = Self { view, last_title: None };
            if !web_view.load_url(url) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid URL").into());
            }
            Ok(web_view)
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

        fn update_frame(&mut self, window: &Window) {
            let frame = webview_frame(window);
            unsafe {
                let _: () = msg_send![&*self.view, setFrame: frame];
            }
        }

        fn load_url(&mut self, url: &str) -> bool {
            self.last_title = None;
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

        fn poll_title(&mut self) -> Option<String> {
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
    }

    impl Drop for WebViewSmoke {
        fn drop(&mut self) {
            unsafe {
                let _: () = msg_send![&*self.view, removeFromSuperview];
            }
        }
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

        CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width, height } }
    }

    fn start_server() -> Result<u16, Box<dyn Error>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            HTML_BODY.len(),
            HTML_BODY
        );

        thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let Ok(mut stream) = stream else {
                    continue;
                };

                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(response.as_bytes());
            }
        });

        Ok(port)
    }

    struct App {
        window: Option<Window>,
        web_view: Option<WebViewSmoke>,
        started_at: Instant,
        result: Option<Result<(), String>>,
        _autofill: Option<AutofillOverride>,
    }

    impl App {
        fn new() -> Self {
            Self {
                window: None,
                web_view: None,
                started_at: Instant::now(),
                result: None,
                _autofill: None,
            }
        }

        fn finish(&mut self, event_loop: &ActiveEventLoop, result: Result<(), String>) {
            self.result = Some(result);
            event_loop.exit();
        }
    }

    impl ApplicationHandler<()> for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }

            let attributes = WindowAttributes::default()
                .with_title("Tabor Web Passkey Smoke")
                .with_inner_size(PhysicalSize::new(800, 600));
            let window = match event_loop.create_window(attributes) {
                Ok(window) => window,
                Err(_) => {
                    self.finish(event_loop, Err(String::from("Failed to create window")));
                    return;
                },
            };

            let port = match start_server() {
                Ok(port) => port,
                Err(_) => {
                    self.finish(event_loop, Err(String::from("Failed to start HTTP server")));
                    return;
                },
            };

            self._autofill = Some(AutofillOverride::enable());

            let url = format!("http://localhost:{}/", port);
            let web_view = match WebViewSmoke::new(&window, &url) {
                Ok(web_view) => web_view,
                Err(_) => {
                    self.finish(event_loop, Err(String::from("Failed to create WKWebView")));
                    return;
                },
            };

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
                WindowEvent::CloseRequested => {
                    self.finish(event_loop, Err(String::from("Window closed")));
                },
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
                self.finish(event_loop, Err(String::from("Timed out waiting for passkey status")));
                return;
            }

            let Some(web_view) = self.web_view.as_mut() else {
                return;
            };

            let Some(title) = web_view.poll_title() else {
                return;
            };

            if title == TITLE_AVAILABLE {
                self.finish(event_loop, Ok(()));
                return;
            }
            if title == TITLE_UNAVAILABLE {
                self.finish(
                    event_loop,
                    Err(String::from("Passkey platform authenticator unavailable")),
                );
                return;
            }
            if let Some(reason) = title.strip_prefix(TITLE_UNSUPPORTED_PREFIX) {
                self.finish(event_loop, Err(format!("WebAuthn unsupported: {reason}")));
                return;
            }
            if title == TITLE_UNSUPPORTED {
                self.finish(event_loop, Err(String::from("WebAuthn unsupported")));
                return;
            }
            if let Some(reason) = title.strip_prefix(TITLE_ERROR_PREFIX) {
                self.finish(event_loop, Err(format!("Passkey error: {reason}")));
                return;
            }
            if title == TITLE_ERROR {
                self.finish(event_loop, Err(String::from("Passkey error")));
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let event_loop = EventLoop::new()?;
        let mut app = App::new();
        event_loop.run_app(&mut app)?;

        match app.result {
            Some(Ok(())) => Ok(()),
            Some(Err(message)) => {
                Err(std::io::Error::new(std::io::ErrorKind::Other, message).into())
            },
            None => {
                Err(std::io::Error::new(std::io::ErrorKind::Other, "WebView passkey smoke failed")
                    .into())
            },
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
