#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod smoke {
    use std::cell::RefCell;
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use libc::{c_char, c_void};
    use objc2::encode::{Encode, Encoding};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{MainThreadMarker, class, msg_send};
    use objc2_foundation::NSString;
    use winit::application::ApplicationHandler;
    use winit::dpi::PhysicalSize;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowAttributes};

    const TIMEOUT: Duration = Duration::from_secs(10);
    const PDF_DETECT_SCRIPT: &str = r#"(function() {
  const ct = (document.contentType || "").toLowerCase();
  if (ct.includes("pdf")) { return ct; }
  const embed = document.querySelector('embed[type*="pdf"]');
  if (embed && embed.type) { return embed.type.toLowerCase(); }
  const obj = document.querySelector('object[type*="pdf"]');
  if (obj && obj.type) { return obj.type.toLowerCase(); }
  return "";
})()"#;

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
        fn new(window: &Window, pdf_path: &Path) -> Result<Self, Box<dyn Error>> {
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
            web_view.load_file(pdf_path)?;
            Ok(web_view)
        }

        fn update_frame(&mut self, window: &Window) {
            let frame = webview_frame(window);
            unsafe {
                let _: () = msg_send![&*self.view, setFrame: frame];
            }
        }

        fn load_file(&mut self, path: &Path) -> Result<(), Box<dyn Error>> {
            let path = path.to_str().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "PDF path is not utf-8")
            })?;
            let ns_path = NSString::from_str(path);
            let ns_url: *mut AnyObject =
                unsafe { msg_send![class!(NSURL), fileURLWithPath: &*ns_path] };
            if ns_url.is_null() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to create file URL",
                )
                .into());
            }

            let access_url: *mut AnyObject =
                unsafe { msg_send![ns_url, URLByDeletingLastPathComponent] };
            let access_url = if access_url.is_null() { ns_url } else { access_url };
            unsafe {
                let _: *mut AnyObject = msg_send![
                    &*self.view,
                    loadFileURL: ns_url,
                    allowingReadAccessToURL: access_url
                ];
            }
            Ok(())
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
                let _: () = msg_send![&*self.view, evaluateJavaScript: &*script, completionHandler: &*block];
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

        CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width, height } }
    }

    fn build_pdf_bytes() -> Vec<u8> {
        let stream = "BT\n/F1 24 Tf\n72 100 Td\n(Hello PDF) Tj\nET\n";
        let objects = vec![
            String::from("1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n"),
            String::from("2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n"),
            String::from(
                "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 144] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n",
            ),
            format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
                stream.len(),
                stream
            ),
            String::from(
                "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
            ),
        ];

        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::with_capacity(objects.len());
        for obj in &objects {
            offsets.push(pdf.len());
            pdf.push_str(obj);
        }

        let xref_offset = pdf.len();
        let size = objects.len() + 1;
        pdf.push_str(&format!("xref\n0 {size}\n"));
        pdf.push_str("0000000000 65535 f \n");
        for offset in offsets {
            pdf.push_str(&format!("{:010} 00000 n \n", offset));
        }
        pdf.push_str(&format!("trailer\n<< /Size {size} /Root 1 0 R >>\n"));
        pdf.push_str(&format!("startxref\n{xref_offset}\n%%EOF\n"));
        pdf.into_bytes()
    }

    fn write_temp_pdf() -> Result<PathBuf, Box<dyn Error>> {
        let mut path = std::env::temp_dir();
        path.push(format!("tabor_pdf_smoke_{}.pdf", std::process::id()));
        std::fs::write(&path, build_pdf_bytes())?;
        Ok(path)
    }

    struct App {
        window: Option<Window>,
        web_view: Option<WebViewSmoke>,
        started_at: Instant,
        result: Option<bool>,
        probe_started: bool,
        probe_result: Rc<RefCell<Option<Option<String>>>>,
    }

    impl App {
        fn new() -> Self {
            Self {
                window: None,
                web_view: None,
                started_at: Instant::now(),
                result: None,
                probe_started: false,
                probe_result: Rc::new(RefCell::new(None)),
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
                .with_title("Tabor Web PDF Smoke")
                .with_inner_size(PhysicalSize::new(600, 400));
            let window = match event_loop.create_window(attributes) {
                Ok(window) => window,
                Err(_) => {
                    self.finish(event_loop, false);
                    return;
                },
            };

            let pdf_path = match write_temp_pdf() {
                Ok(path) => path,
                Err(err) => {
                    eprintln!("failed to write temp pdf: {err}");
                    self.finish(event_loop, false);
                    return;
                },
            };

            let web_view = match WebViewSmoke::new(&window, &pdf_path) {
                Ok(web_view) => web_view,
                Err(err) => {
                    eprintln!("failed to create pdf webview: {err}");
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

            if !self.probe_started {
                let result = Rc::clone(&self.probe_result);
                web_view.eval_js_string(PDF_DETECT_SCRIPT, move |value| {
                    *result.borrow_mut() = Some(value);
                });
                self.probe_started = true;
            }

            let outcome = self.probe_result.borrow_mut().take();
            let Some(outcome) = outcome else {
                return;
            };

            let Some(value) = outcome else {
                eprintln!("pdf probe returned empty result");
                self.finish(event_loop, false);
                return;
            };

            let value = value.trim().to_lowercase();
            if value.is_empty() {
                self.probe_started = false;
                return;
            }

            if !value.contains("pdf") {
                eprintln!("expected pdf content type, got {value}");
                self.finish(event_loop, false);
                return;
            }

            self.finish(event_loop, true);
        }
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let event_loop = EventLoop::new()?;
        let mut app = App::new();
        event_loop.run_app(&mut app)?;

        match app.result {
            Some(true) => Ok(()),
            _ => {
                Err(std::io::Error::new(std::io::ErrorKind::Other, "WebView PDF smoke failed")
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
