use std::error::Error;

use serde_json::Value as JsonValue;

use super::webview_cef;
use crate::ipc::AgentDownload;

pub struct WebView {
    inner: webview_cef::WebView,
}

impl WebView {
    pub fn new(
        window: &crate::display::window::Window,
        size_info: &crate::display::SizeInfo,
        tab_id: crate::tabs::TabId,
        url: &str,
        proxy: &winit::event_loop::EventLoopProxy<crate::event::Event>,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self { inner: webview_cef::WebView::new(window, size_info, tab_id, url, proxy)? })
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.inner.set_visible(visible);
    }

    pub fn set_focus(&mut self, focus: bool) {
        self.inner.set_focus(focus);
    }

    pub fn update_frame(
        &mut self,
        window: &crate::display::window::Window,
        size_info: &crate::display::SizeInfo,
    ) {
        self.inner.update_frame(window, size_info);
    }

    pub fn load_url(&mut self, url: &str) -> bool {
        self.inner.load_url(url)
    }

    pub fn reload(&mut self) {
        self.inner.reload();
    }

    pub fn go_back(&mut self) {
        self.inner.go_back();
    }

    pub fn go_forward(&mut self) {
        self.inner.go_forward();
    }

    pub fn handle_key_input(
        &mut self,
        window: &crate::display::window::Window,
        key: &winit::event::KeyEvent,
        text: &str,
        modifiers: winit::keyboard::ModifiersState,
    ) -> bool {
        self.inner.handle_key_input(window, key, text, modifiers)
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
        self.inner.handle_mouse_input(window, size_info, position, state, button, modifiers)
    }

    pub fn exec_js(&mut self, script: &str) {
        self.inner.exec_js(script);
    }

    pub fn eval_js_string<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        self.inner.eval_js_string(script, callback);
    }

    pub fn eval_js_string_with_user_gesture<F>(&mut self, script: &str, callback: F)
    where
        F: FnOnce(Option<String>) + 'static,
    {
        self.inner.eval_js_string_with_user_gesture(script, callback);
    }

    pub fn devtools_command_json<F>(
        &mut self,
        method: &str,
        params: Option<JsonValue>,
        callback: F,
    ) -> Result<(), String>
    where
        F: FnOnce(Result<JsonValue, String>) + 'static,
    {
        self.inner.devtools_command_json(method, params, callback)
    }

    pub fn devtools_events_since(&self, last_id: u64, max: usize) -> (Vec<String>, u64) {
        self.inner.devtools_events_since(last_id, max)
    }

    pub fn latest_devtools_event_id(&self) -> u64 {
        self.inner.latest_devtools_event_id()
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
        self.inner.set_file_input_files(element_id, paths, callback)
    }

    pub fn downloads(&self) -> Vec<AgentDownload> {
        self.inner.downloads()
    }

    pub fn poll_title(&mut self) -> Option<String> {
        self.inner.poll_title()
    }

    pub fn poll_url(&mut self) -> Option<String> {
        self.inner.poll_url()
    }

    pub fn current_url(&self) -> Option<String> {
        self.inner.current_url()
    }

    pub fn show_inspector(&mut self) -> bool {
        self.inner.show_inspector()
    }
}
