#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod probe {
    use std::cell::RefCell;
    use std::error::Error;
    use std::ptr;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use objc2::encode::{Encode, Encoding};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{MainThreadMarker, class, msg_send, sel};
    use objc2_foundation::NSString;
    use serde::Deserialize;
    use serde_json;
    use winit::application::ApplicationHandler;
    use winit::dpi::PhysicalSize;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowAttributes};

    pub const DEFAULT_URL: &str = "https://mail.google.com/";
    pub const DEFAULT_NEEDLE: &str = "this browser version is no longer supported";
    const EVAL_INTERVAL: Duration = Duration::from_millis(500);

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

    struct ProbeState {
        pending: bool,
        last_eval: Instant,
        last_result: Option<ProbeResult>,
    }

    impl Default for ProbeState {
        fn default() -> Self {
            Self { pending: false, last_eval: Instant::now(), last_result: None }
        }
    }

    #[derive(Clone, Debug, Deserialize)]
    struct ProbeResult {
        #[serde(default)]
        banner: bool,
        #[serde(default)]
        ua: String,
        #[serde(default)]
        vendor: String,
        #[serde(default)]
        platform: String,
        #[serde(default)]
        app_version: String,
        #[serde(default)]
        ready_state: String,
        #[serde(default)]
        location: String,
    }

    struct WebViewProbe {
        view: Retained<AnyObject>,
    }

    impl WebViewProbe {
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

            apply_safari_user_agent(&view)?;

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
            let block = block2::RcBlock::new({
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
                let _: () = msg_send![&*self.view, evaluateJavaScript: &*script, completionHandler: &*block];
            }
        }
    }

    impl Drop for WebViewProbe {
        fn drop(&mut self) {
            unsafe {
                let _: () = msg_send![&*self.view, removeFromSuperview];
            }
        }
    }

    fn webview_user_agent(view: &AnyObject) -> Option<String> {
        let key = NSString::from_str("userAgent");
        let value: *mut AnyObject = unsafe { msg_send![view, valueForKey: &*key] };
        if value.is_null() {
            return None;
        }

        Some(unsafe { &*(value as *const NSString) }.to_string())
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
            return Err(
                std::io::Error::new(std::io::ErrorKind::Other, "Safari.app not found").into()
            );
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

    fn safari_user_agent(view: &AnyObject) -> Result<String, Box<dyn Error>> {
        let base_agent = webview_user_agent(view).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "WKWebView has no userAgent")
        })?;
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

    struct App {
        window: Option<Window>,
        web_view: Option<WebViewProbe>,
        started_at: Instant,
        timeout: Duration,
        url: String,
        needle: String,
        state: Rc<RefCell<ProbeState>>,
        last_report: Option<ProbeResult>,
        result: Option<bool>,
    }

    impl App {
        fn new(url: String, timeout: Duration, needle: String) -> Self {
            Self {
                window: None,
                web_view: None,
                started_at: Instant::now(),
                timeout,
                url,
                needle,
                state: Rc::new(RefCell::new(ProbeState::default())),
                last_report: None,
                result: None,
            }
        }

        fn finish(&mut self, event_loop: &ActiveEventLoop, ok: bool) {
            if let Some(report) = self.last_report.take() {
                let status = if ok { "found" } else { "not found" };
                println!("Banner: {status}");
                println!("URL: {}", report.location);
                println!("ReadyState: {}", report.ready_state);
                println!("User-Agent: {}", report.ua);
                println!("Vendor: {}", report.vendor);
                println!("Platform: {}", report.platform);
                println!("AppVersion: {}", report.app_version);
            }
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
                .with_title("Tabor WebView Banner Probe")
                .with_inner_size(PhysicalSize::new(900, 700));
            let window = match event_loop.create_window(attributes) {
                Ok(window) => window,
                Err(_) => {
                    self.finish(event_loop, false);
                    return;
                },
            };

            let web_view = match WebViewProbe::new(&window, &self.url) {
                Ok(web_view) => web_view,
                Err(_) => {
                    self.finish(event_loop, false);
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

            if self.started_at.elapsed() > self.timeout {
                self.finish(event_loop, false);
                return;
            }

            let Some(web_view) = self.web_view.as_mut() else {
                return;
            };

            let should_eval = {
                let state = self.state.borrow();
                !state.pending && state.last_eval.elapsed() >= EVAL_INTERVAL
            };

            if should_eval {
                let state = Rc::clone(&self.state);
                let needle = self.needle.to_ascii_lowercase();
                let script = probe_script(&needle);
                {
                    let mut state = state.borrow_mut();
                    state.pending = true;
                    state.last_eval = Instant::now();
                }
                web_view.eval_js_string(&script, move |result| {
                    let mut state = state.borrow_mut();
                    state.pending = false;
                    let Some(result) = result else {
                        return;
                    };
                    let parsed: Result<ProbeResult, _> = serde_json::from_str(&result);
                    if let Ok(parsed) = parsed {
                        state.last_result = Some(parsed);
                    }
                });
            }

            let result = {
                let mut state = self.state.borrow_mut();
                state.last_result.take()
            };
            if let Some(result) = result {
                let banner = result.banner;
                self.last_report = Some(result);
                if banner {
                    self.finish(event_loop, true);
                }
            }
        }
    }

    fn probe_script(needle: &str) -> String {
        let needle = serde_json::to_string(needle).expect("serialize probe needle");
        format!(
            r#"(() => {{
  const needle = {needle};
  const text = (document.body && document.body.innerText || "").toLowerCase();
  const banner = text.includes(needle);
  return JSON.stringify({{
    banner,
    ua: navigator.userAgent || "",
    vendor: navigator.vendor || "",
    platform: navigator.platform || "",
    app_version: navigator.appVersion || "",
    ready_state: document.readyState || "",
    location: location.href || "",
  }});
}})()"#
        )
    }

    pub fn run(url: &str, needle: &str, timeout: Duration) -> Result<(), Box<dyn Error>> {
        let event_loop = EventLoop::new()?;
        let mut app = App::new(url.to_string(), timeout, needle.to_string());
        event_loop.run_app(&mut app)?;

        match app.result {
            Some(true) => Ok(()),
            _ => Err(std::io::Error::new(std::io::ErrorKind::Other, "Banner not found").into()),
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    use std::time::Duration;

    let mut url = probe::DEFAULT_URL.to_string();
    let mut needle = probe::DEFAULT_NEEDLE.to_string();
    let mut timeout = Duration::from_secs(20);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => {
                if let Some(value) = args.next() {
                    url = value;
                }
            },
            "--needle" => {
                if let Some(value) = args.next() {
                    needle = value;
                }
            },
            "--timeout" => {
                if let Some(value) = args.next() {
                    if let Ok(seconds) = value.parse::<u64>() {
                        timeout = Duration::from_secs(seconds);
                    }
                }
            },
            _ => (),
        }
    }

    if let Err(err) = probe::run(&url, &needle, timeout) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
