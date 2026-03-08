use std::error::Error;
use std::ffi::c_void;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseButton};
use winit::keyboard::ModifiersState;

use super::webview_cef;
use crate::display::browser_layout::BrowserViewportLayout;
use crate::ipc::AgentDownload;

pub struct WebView {
    inner: webview_cef::WebView,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAccelerationState {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebFrameDeliveryMode {
    #[default]
    CefInternal,
}

#[derive(Debug, Copy, Clone)]
pub struct WebSurfaceRef {
    pub io_surface: *mut c_void,
    pub width: usize,
    pub height: usize,
    pub format: cef::ColorType,
}

#[derive(Debug, Copy, Clone)]
pub struct WebPopupSurfaceRef {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub surface: WebSurfaceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebAccelerationInfo {
    pub state: WebAccelerationState,
    pub frame_delivery_mode: WebFrameDeliveryMode,
    pub main_surface_width: Option<usize>,
    pub main_surface_height: Option<usize>,
    pub popup_surface_width: Option<usize>,
    pub popup_surface_height: Option<usize>,
}

impl WebView {
    pub fn new(
        window: &crate::display::window::Window,
        size_info: &crate::display::SizeInfo,
        layout: BrowserViewportLayout,
        tab_id: crate::tabs::TabId,
        url: &str,
        proxy: &winit::event_loop::EventLoopProxy<crate::event::Event>,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            inner: webview_cef::WebView::new(window, size_info, layout, tab_id, url, proxy)?,
        })
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
        layout: BrowserViewportLayout,
    ) {
        self.inner.update_frame(window, size_info, layout);
    }

    pub fn acceleration_info(&self) -> WebAccelerationInfo {
        self.inner.acceleration_info()
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
        key: &KeyEvent,
        text: &str,
        modifiers: ModifiersState,
    ) -> bool {
        self.inner.handle_key_input(window, key, text, modifiers)
    }

    pub fn handle_mouse_input(
        &mut self,
        window: &crate::display::window::Window,
        position: PhysicalPosition<f64>,
        state: ElementState,
        button: MouseButton,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        self.inner.handle_mouse_input(window, position, state, button, modifiers)
    }

    pub fn handle_mouse_move(
        &mut self,
        window: &crate::display::window::Window,
        position: PhysicalPosition<f64>,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        self.inner.handle_mouse_move(window, position, modifiers)
    }

    pub fn handle_mouse_leave(&mut self) {
        self.inner.handle_mouse_leave();
    }

    pub fn handle_mouse_wheel(
        &mut self,
        window: &crate::display::window::Window,
        position: PhysicalPosition<f64>,
        delta_x: f64,
        delta_y: f64,
        modifiers: objc2_app_kit::NSEventModifierFlags,
    ) -> bool {
        self.inner.handle_mouse_wheel(window, position, delta_x, delta_y, modifiers)
    }

    pub fn handle_ime_commit(&mut self, text: &str) {
        self.inner.handle_ime_commit(text);
    }

    pub fn handle_ime_preedit(&mut self, text: &str, cursor_offset: Option<(usize, usize)>) {
        self.inner.handle_ime_preedit(text, cursor_offset);
    }

    pub fn cancel_ime_composition(&mut self) {
        self.inner.cancel_ime_composition();
    }

    pub fn exec_js(&mut self, script: &str) {
        self.inner.exec_js(script);
    }

    pub fn copy_selection(&mut self) {
        self.inner.copy_selection();
    }

    pub fn cut_selection(&mut self) {
        self.inner.cut_selection();
    }

    pub fn paste(&mut self) {
        self.inner.paste();
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

    pub fn with_surfaces<R>(
        &self,
        func: impl FnOnce(Option<WebSurfaceRef>, Option<WebPopupSurfaceRef>) -> R,
    ) -> R {
        self.inner.with_surfaces(func)
    }
}
