use std::env;
use std::error::Error;
use std::sync::OnceLock;

use crate::ipc::{WebNetworkAction, WebNetworkEntry};

use super::{webview_cef, webview_webkit};

pub(crate) use super::webview_webkit::PendingPopup;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebViewBackend {
    WebKit,
    Cef,
}

static WEBVIEW_BACKEND: OnceLock<WebViewBackend> = OnceLock::new();

pub fn take_pending_popup(popup_id: usize) -> Option<PendingPopup> {
    if backend() == WebViewBackend::WebKit {
        webview_webkit::take_pending_popup(popup_id)
    } else {
        None
    }
}

fn backend() -> WebViewBackend {
    *WEBVIEW_BACKEND.get_or_init(select_backend)
}

fn select_backend() -> WebViewBackend {
    if let Ok(value) = env::var("TABOR_WEBVIEW_ENGINE") {
        match value.to_lowercase().as_str() {
            "webkit" | "wk" => return WebViewBackend::WebKit,
            "cef" | "chromium" | "chrome" => return WebViewBackend::Cef,
            _ => (),
        }
    }

    if webview_cef::is_available() {
        WebViewBackend::Cef
    } else {
        WebViewBackend::WebKit
    }
}

pub struct WebView {
    inner: WebViewInner,
}

enum WebViewInner {
    WebKit(webview_webkit::WebView),
    Cef(webview_cef::WebView),
}

impl WebView {
    pub fn new(
        window: &crate::display::window::Window,
        size_info: &crate::display::SizeInfo,
        tab_id: crate::tabs::TabId,
        url: &str,
        proxy: &winit::event_loop::EventLoopProxy<crate::event::Event>,
    ) -> Result<Self, Box<dyn Error>> {
        match backend() {
            WebViewBackend::WebKit => Ok(Self {
                inner: WebViewInner::WebKit(webview_webkit::WebView::new(
                    window, size_info, tab_id, url, proxy,
                )?),
            }),
            WebViewBackend::Cef => Ok(Self {
                inner: WebViewInner::Cef(webview_cef::WebView::new(
                    window, size_info, tab_id, url, proxy,
                )?),
            }),
        }
    }

    pub fn from_existing(
        window: &crate::display::window::Window,
        size_info: &crate::display::SizeInfo,
        tab_id: crate::tabs::TabId,
        view: objc2::rc::Retained<objc2::runtime::AnyObject>,
        delegate: objc2::rc::Retained<objc2::runtime::AnyObject>,
    ) -> Result<Self, Box<dyn Error>> {
        match backend() {
            WebViewBackend::WebKit => Ok(Self {
                inner: WebViewInner::WebKit(webview_webkit::WebView::from_existing(
                    window, size_info, tab_id, view, delegate,
                )?),
            }),
            WebViewBackend::Cef => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "CEF backend does not support existing WebView",
            )
            .into()),
        }
    }

    pub fn set_visible(&mut self, visible: bool) {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.set_visible(visible),
            WebViewInner::Cef(view) => view.set_visible(visible),
        }
    }

    pub fn update_frame(&mut self, window: &crate::display::window::Window, size_info: &crate::display::SizeInfo) {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.update_frame(window, size_info),
            WebViewInner::Cef(view) => view.update_frame(window, size_info),
        }
    }

    pub fn invalidate_cursor_rects(&self) {
        match &self.inner {
            WebViewInner::WebKit(view) => view.invalidate_cursor_rects(),
            WebViewInner::Cef(view) => view.invalidate_cursor_rects(),
        }
    }

    pub fn load_url(&mut self, url: &str) -> bool {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.load_url(url),
            WebViewInner::Cef(view) => view.load_url(url),
        }
    }

    pub fn reload(&mut self) {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.reload(),
            WebViewInner::Cef(view) => view.reload(),
        }
    }

    pub fn go_back(&mut self) {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.go_back(),
            WebViewInner::Cef(view) => view.go_back(),
        }
    }

    pub fn go_forward(&mut self) {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.go_forward(),
            WebViewInner::Cef(view) => view.go_forward(),
        }
    }

    pub fn handle_mouse_input(
        &mut self,
        window: &crate::display::window::Window,
        size_info: &crate::display::SizeInfo,
        position: winit::dpi::PhysicalPosition<f64>,
        state: winit::event::ElementState,
        button: winit::event::MouseButton,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        match &mut self.inner {
            WebViewInner::WebKit(view) => {
                view.handle_mouse_input(window, size_info, position, state, button, modifiers)
            }
            WebViewInner::Cef(view) => {
                view.handle_mouse_input(window, size_info, position, state, button, modifiers)
            }
        }
    }

    pub fn exec_js(&mut self, script: &str) {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.exec_js(script),
            WebViewInner::Cef(view) => view.exec_js(script),
        }
    }

    pub fn eval_js_string<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.eval_js_string(script, callback),
            WebViewInner::Cef(view) => view.eval_js_string(script, callback),
        }
    }

    pub fn snapshot_png<F>(&mut self, full: bool, callback: F)
    where
        F: FnOnce(Result<Vec<u8>, String>) + 'static,
    {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.snapshot_png(full, callback),
            WebViewInner::Cef(view) => view.snapshot_png(full, callback),
        }
    }

    pub fn pdf<F>(&mut self, callback: F)
    where
        F: FnOnce(Result<Vec<u8>, String>) + 'static,
    {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.pdf(callback),
            WebViewInner::Cef(view) => view.pdf(callback),
        }
    }

    pub fn poll_title(&mut self) -> Option<String> {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.poll_title(),
            WebViewInner::Cef(view) => view.poll_title(),
        }
    }

    pub fn poll_url(&mut self) -> Option<String> {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.poll_url(),
            WebViewInner::Cef(view) => view.poll_url(),
        }
    }

    pub fn current_url(&self) -> Option<String> {
        match &self.inner {
            WebViewInner::WebKit(view) => view.current_url(),
            WebViewInner::Cef(view) => view.current_url(),
        }
    }

    pub fn show_inspector(&mut self) -> bool {
        match &mut self.inner {
            WebViewInner::WebKit(view) => view.show_inspector(),
            WebViewInner::Cef(view) => view.show_inspector(),
        }
    }

    pub fn network_entries(&mut self, action: WebNetworkAction) -> Option<Vec<WebNetworkEntry>> {
        match &mut self.inner {
            WebViewInner::WebKit(_) => None,
            WebViewInner::Cef(view) => view.network_entries(action),
        }
    }
}
