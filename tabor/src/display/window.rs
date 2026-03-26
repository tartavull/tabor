#[cfg(not(any(target_os = "macos", windows)))]
use winit::platform::startup_notify::{
    self, EventLoopExtStartupNotify, WindowAttributesExtStartupNotify,
};
#[cfg(not(any(target_os = "macos", windows)))]
use winit::window::ActivationToken;

#[cfg(all(not(feature = "x11"), not(any(target_os = "macos", windows))))]
use winit::platform::wayland::WindowAttributesExtWayland;

#[rustfmt::skip]
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use {
    std::io::Cursor,
    winit::platform::x11::{WindowAttributesExtX11, ActiveEventLoopExtX11},
    glutin::platform::x11::X11VisualInfo,
    winit::window::Icon,
    png::Decoder,
};

#[cfg(target_os = "macos")]
use std::cell::RefCell;
use std::fmt::{self, Display, Formatter};

#[cfg(target_os = "macos")]
use {
    objc2::rc::Retained,
    objc2::runtime::AnyObject,
    objc2::{MainThreadMarker, MainThreadOnly, msg_send, sel},
    objc2_app_kit::{
        NSBackingStoreType, NSButton, NSColor, NSColorSpace, NSScreen, NSView,
        NSWindow as AppKitWindow, NSWindowAnimationBehavior, NSWindowButton,
        NSWindowCollectionBehavior, NSWindowOrderingMode, NSWindowStyleMask,
    },
    objc2_foundation::{NSEdgeInsets, NSPoint, NSRect, NSSize, NSString},
    winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS, WindowExtMacOS},
};

use bitflags::bitflags;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
#[cfg(windows)]
use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Fullscreen;
use winit::window::{
    CursorIcon, ImePurpose, Theme, UserAttentionType, Window as WinitWindow, WindowAttributes,
    WindowId,
};

use tabor_terminal::index::Point;

use crate::cli::WindowOptions;
use crate::config::UiConfig;
use crate::config::window::{Decorations, Identity, WindowConfig};
use crate::display::SizeInfo;
#[cfg(target_os = "macos")]
use crate::display::color::Rgb;
#[cfg(target_os = "macos")]
use crate::ipc::{
    IpcWindowDebugButton, IpcWindowDebugInsets, IpcWindowDebugRect, IpcWindowDebugState,
};

/// Window icon for `_NET_WM_ICON` property.
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
const WINDOW_ICON: &[u8] = include_bytes!("../../extra/logo/compat/tabor-term.png");

/// This should match the definition of IDI_ICON from `tabor.rc`.
#[cfg(windows)]
const IDI_ICON: u16 = 0x101;

#[cfg(target_os = "macos")]
const MACOS_TRAFFIC_LIGHT_MARGIN_X: f64 = 12.0;
#[cfg(target_os = "macos")]
const MACOS_TRAFFIC_LIGHT_MARGIN_Y: f64 = 8.0;
#[cfg(target_os = "macos")]
pub(crate) const MACOS_FULLSCREEN_WINDOW_CONTROL_REFERENCE_BAND_PX: f64 = 37.0;
#[cfg(target_os = "macos")]
const MACOS_FULLSCREEN_WINDOW_CONTROL_MARGIN_X_PX: f64 = 12.0;
#[cfg(target_os = "macos")]
const MACOS_FULLSCREEN_WINDOW_CONTROL_SPACING_PX: f64 = 8.0;
#[cfg(target_os = "macos")]
const MACOS_FULLSCREEN_WINDOW_CONTROL_SIZE_PX: f64 = 12.0;

/// Window errors.
#[derive(Debug)]
pub enum Error {
    /// Error creating the window.
    WindowCreation(winit::error::OsError),

    /// Error dealing with fonts.
    Font(crossfont::Error),
}

/// Result of fallible operations concerning a Window.
type Result<T> = std::result::Result<T, Error>;

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::WindowCreation(err) => err.source(),
            Error::Font(err) => err.source(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::WindowCreation(err) => write!(f, "Error creating GL context; {err}"),
            Error::Font(err) => err.fmt(f),
        }
    }
}

impl From<winit::error::OsError> for Error {
    fn from(val: winit::error::OsError) -> Self {
        Error::WindowCreation(val)
    }
}

impl From<crossfont::Error> for Error {
    fn from(val: crossfont::Error) -> Self {
        Error::Font(val)
    }
}

/// A window which can be used for displaying the terminal.
///
/// Wraps the underlying windowing library to provide a stable API in Tabor.
pub struct Window {
    /// Flag tracking that we have a frame we can draw.
    pub has_frame: bool,

    /// Cached scale factor for quickly scaling pixel sizes.
    pub scale_factor: f64,

    /// Flag indicating whether redraw was requested.
    pub requested_redraw: bool,

    /// Hold the window when terminal exits.
    pub hold: bool,

    #[cfg(target_os = "macos")]
    macos_notch_ears: RefCell<MacosNotchEarWindows>,

    #[cfg(target_os = "macos")]
    macos_window_controls: RefCell<MacosWindowControlsState>,

    #[cfg(target_os = "macos")]
    macos_real_ear_fullscreen_enabled: bool,

    window: WinitWindow,

    /// Current window title.
    title: String,

    is_x11: bool,
    current_mouse_cursor: CursorIcon,
    mouse_visible: bool,
    ime_inhibitor: ImeInhibitor,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct MacosNotchEarWindows {
    left: Option<Retained<AppKitWindow>>,
    right: Option<Retained<AppKitWindow>>,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct MacosWindowControlFrames {
    close_frame: NSRect,
    mini_frame: NSRect,
    zoom_frame: NSRect,
}

#[cfg(target_os = "macos")]
struct MacosWindowControlOverlays {
    close: Retained<NSButton>,
    mini: Retained<NSButton>,
    zoom: Retained<NSButton>,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct MacosWindowControlsState {
    original_frames: Option<MacosWindowControlFrames>,
    fullscreen_overlays: Option<MacosWindowControlOverlays>,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct MacosNotchGeometry {
    safe_area_insets: NSEdgeInsets,
    auxiliary_top_left_area: NSRect,
    auxiliary_top_right_area: NSRect,
}

#[cfg(target_os = "macos")]
impl MacosNotchEarWindows {
    fn active_window_numbers(&self) -> (Option<i64>, Option<i64>) {
        (
            self.left.as_ref().map(|window| window.windowNumber() as i64),
            self.right.as_ref().map(|window| window.windowNumber() as i64),
        )
    }

    fn is_active(&self) -> bool {
        self.left.is_some() || self.right.is_some()
    }
}

impl Window {
    /// Create a new window.
    ///
    /// This creates a window and fully initializes a window.
    pub fn new(
        event_loop: &ActiveEventLoop,
        config: &UiConfig,
        identity: &Identity,
        options: &mut WindowOptions,
        #[rustfmt::skip]
        #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
        x11_visual: Option<X11VisualInfo>,
    ) -> Result<Window> {
        let identity = identity.clone();
        let mut window_attributes = Window::get_platform_window(
            &identity,
            &config.window,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            x11_visual,
        );

        if let Some(position) = config.window.position {
            window_attributes = window_attributes
                .with_position(PhysicalPosition::<i32>::from((position.x, position.y)));
        }

        #[cfg(not(any(target_os = "macos", windows)))]
        if let Some(token) = options
            .activation_token
            .take()
            .map(ActivationToken::from_raw)
            .or_else(|| event_loop.read_token_from_env())
        {
            log::debug!("Activating window with token: {token:?}");
            window_attributes = window_attributes.with_activation_token(token);

            // Remove the token from the env.
            startup_notify::reset_activation_token_env();
        }

        // On X11, embed the window inside another if the parent ID has been set.
        #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
        if let Some(parent_window_id) = event_loop.is_x11().then_some(config.window.embed).flatten()
        {
            window_attributes = window_attributes.with_embed_parent_window(parent_window_id);
        }

        #[cfg(target_os = "macos")]
        let startup_fullscreen: Option<Fullscreen> = None;
        #[cfg(not(target_os = "macos"))]
        let startup_fullscreen = config.window.fullscreen();
        #[cfg(target_os = "macos")]
        let macos_real_ear_fullscreen_enabled = macos_real_ear_fullscreen_enabled(&config.window);

        window_attributes = window_attributes
            .with_title(&identity.title)
            .with_theme(config.window.theme())
            .with_visible(false)
            .with_transparent(true)
            .with_blur(config.window.blur)
            .with_maximized(config.window.maximized())
            .with_fullscreen(startup_fullscreen)
            .with_window_level(config.window.level.into());

        let window = event_loop.create_window(window_attributes)?;

        // Text cursor.
        let current_mouse_cursor = CursorIcon::Text;
        window.set_cursor(current_mouse_cursor);

        // Enable IME.
        window.set_ime_allowed(true);
        window.set_ime_purpose(ImePurpose::Terminal);

        // Set initial transparency hint.
        window.set_transparent(config.window_opacity() < 1.);

        #[cfg(target_os = "macos")]
        {
            use_srgb_color_space(&window);
            configure_macos_fullscreen_behavior(&window, macos_real_ear_fullscreen_enabled);
        }

        let scale_factor = window.scale_factor();
        log::info!("Window scale factor: {scale_factor}");
        let is_x11 = matches!(window.window_handle().unwrap().as_raw(), RawWindowHandle::Xlib(_));

        Ok(Self {
            hold: options.terminal_options.hold,
            requested_redraw: false,
            #[cfg(target_os = "macos")]
            macos_notch_ears: RefCell::new(MacosNotchEarWindows::default()),
            #[cfg(target_os = "macos")]
            macos_window_controls: RefCell::new(MacosWindowControlsState::default()),
            #[cfg(target_os = "macos")]
            macos_real_ear_fullscreen_enabled,
            title: identity.title,
            current_mouse_cursor,
            mouse_visible: true,
            has_frame: true,
            scale_factor,
            window,
            is_x11,
            ime_inhibitor: Default::default(),
        })
    }

    #[inline]
    pub fn raw_window_handle(&self) -> RawWindowHandle {
        self.window.window_handle().unwrap().as_raw()
    }

    #[inline]
    pub fn request_inner_size(&self, size: PhysicalSize<u32>) {
        let _ = self.window.request_inner_size(size);
    }

    #[inline]
    pub fn inner_size(&self) -> PhysicalSize<u32> {
        self.window.inner_size()
    }

    #[inline]
    pub fn set_visible(&self, visibility: bool) {
        self.window.set_visible(visibility);
    }

    #[cfg(target_os = "macos")]
    #[inline]
    pub fn focus_window(&self) {
        self.window.focus_window();
    }

    #[cfg(target_os = "macos")]
    pub fn focus_content_view(&self) {
        let _mtm = MainThreadMarker::new().expect("focus_content_view requires main thread");

        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => handle.ns_view.cast::<AnyObject>(),
            _ => return,
        };

        let _ = self.focus_native_view(view.as_ptr());
    }

    #[cfg(target_os = "macos")]
    pub fn focus_native_view(&self, view: *mut AnyObject) -> bool {
        let _mtm = MainThreadMarker::new().expect("focus_native_view requires main thread");
        let Some(view) = (unsafe { view.cast::<NSView>().as_ref() }) else {
            return false;
        };
        let Some(window) = view.window() else {
            return false;
        };
        let target_view = view as *const NSView as *const AnyObject;
        if let Some(first_responder) = window.firstResponder() {
            if std::ptr::eq(Retained::as_ptr(&first_responder).cast::<AnyObject>(), target_view) {
                return true;
            }
        }

        restore_first_responder_with_retry(
            || window.makeFirstResponder(None),
            || window.makeFirstResponder(Some(view)),
        )
    }

    /// Set the window title.
    #[inline]
    pub fn set_title(&mut self, title: String) {
        self.title = title;
        self.window.set_title(&self.title);
    }

    /// Get the window title.
    #[inline]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[inline]
    pub fn request_redraw(&mut self) {
        if !self.requested_redraw {
            self.requested_redraw = true;
            self.window.request_redraw();
        }
    }

    #[inline]
    pub fn set_mouse_cursor(&mut self, cursor: CursorIcon) {
        if cursor != self.current_mouse_cursor {
            self.current_mouse_cursor = cursor;
            self.window.set_cursor(cursor);
        }
    }

    /// Set mouse cursor visible.
    pub fn set_mouse_visible(&mut self, visible: bool) {
        if visible != self.mouse_visible {
            self.mouse_visible = visible;
            self.window.set_cursor_visible(visible);
        }
    }

    #[inline]
    pub fn mouse_visible(&self) -> bool {
        self.mouse_visible
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn get_platform_window(
        identity: &Identity,
        window_config: &WindowConfig,
        #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))] x11_visual: Option<
            X11VisualInfo,
        >,
    ) -> WindowAttributes {
        #[cfg(feature = "x11")]
        let icon = {
            let mut decoder = Decoder::new(Cursor::new(WINDOW_ICON));
            decoder.set_transformations(png::Transformations::normalize_to_color8());
            let mut reader = decoder.read_info().expect("invalid embedded icon");
            let mut buf = vec![0; reader.output_buffer_size()];
            let _ = reader.next_frame(&mut buf);
            Icon::from_rgba(buf, reader.info().width, reader.info().height)
                .expect("invalid embedded icon format")
        };

        let builder = WinitWindow::default_attributes()
            .with_name(&identity.class.general, &identity.class.instance)
            .with_decorations(window_config.decorations != Decorations::None);

        #[cfg(feature = "x11")]
        let builder = builder.with_window_icon(Some(icon));

        #[cfg(feature = "x11")]
        let builder = match x11_visual {
            Some(visual) => builder.with_x11_visual(visual.visual_id() as u32),
            None => builder,
        };

        builder
    }

    #[cfg(windows)]
    pub fn get_platform_window(_: &Identity, window_config: &WindowConfig) -> WindowAttributes {
        let icon = winit::window::Icon::from_resource(IDI_ICON, None);

        WinitWindow::default_attributes()
            .with_decorations(window_config.decorations != Decorations::None)
            .with_window_icon(icon.as_ref().ok().cloned())
            .with_taskbar_icon(icon.ok())
    }

    #[cfg(target_os = "macos")]
    pub fn get_platform_window(_: &Identity, window_config: &WindowConfig) -> WindowAttributes {
        let window =
            WinitWindow::default_attributes().with_option_as_alt(window_config.option_as_alt());

        match window_config.decorations {
            Decorations::Full => {
                if window_config.tab_panel.enabled {
                    window
                        .with_title_hidden(true)
                        .with_titlebar_transparent(true)
                        .with_fullsize_content_view(true)
                } else {
                    window
                }
            },
            Decorations::Transparent => window
                .with_title_hidden(true)
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true),
            Decorations::Buttonless => window
                .with_title_hidden(true)
                .with_titlebar_buttons_hidden(true)
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true),
            Decorations::None => window.with_titlebar_hidden(true),
        }
    }

    pub fn set_urgent(&self, is_urgent: bool) {
        let attention = if is_urgent { Some(UserAttentionType::Critical) } else { None };

        self.window.request_user_attention(attention);
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn set_transparent(&self, transparent: bool) {
        self.window.set_transparent(transparent);
    }

    pub fn set_blur(&self, blur: bool) {
        self.window.set_blur(blur);
    }

    pub fn set_maximized(&self, maximized: bool) {
        self.window.set_maximized(maximized);
    }

    pub fn set_minimized(&self, minimized: bool) {
        self.window.set_minimized(minimized);
    }

    #[cfg(target_os = "macos")]
    pub fn request_close(&self) {
        let Some(_mtm) = MainThreadMarker::new() else {
            return;
        };
        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return,
        };
        let Some(window) = view.window() else {
            return;
        };
        window.performClose(None);
    }

    pub fn set_resize_increments(&self, increments: PhysicalSize<f32>) {
        self.window.set_resize_increments(Some(increments));
    }

    /// Toggle the window's fullscreen state.
    pub fn toggle_fullscreen(&self) {
        #[cfg(target_os = "macos")]
        {
            let (native_fullscreen, simple_fullscreen, _) = self.macos_fullscreen_flags();
            if native_fullscreen {
                self.set_fullscreen(false);
                return;
            }

            if simple_fullscreen {
                self.set_simple_fullscreen(false);
                return;
            }

            self.set_preferred_fullscreen(true);
        }

        #[cfg(not(target_os = "macos"))]
        self.set_fullscreen(self.window.fullscreen().is_none());
    }

    /// Toggle the window's maximized state.
    pub fn toggle_maximized(&self) {
        self.set_maximized(!self.window.is_maximized());
    }

    /// Inform windowing system about presenting to the window.
    ///
    /// Should be called right before presenting to the window with e.g. `eglSwapBuffers`.
    pub fn pre_present_notify(&self) {
        self.window.pre_present_notify();
    }

    pub fn set_theme(&self, theme: Option<Theme>) {
        self.window.set_theme(theme);
    }

    #[cfg(target_os = "macos")]
    pub fn toggle_simple_fullscreen(&self) {
        self.set_simple_fullscreen(!self.window.simple_fullscreen());
    }

    #[cfg(target_os = "macos")]
    pub fn set_option_as_alt(&self, option_as_alt: OptionAsAlt) {
        self.window.set_option_as_alt(option_as_alt);
    }

    #[cfg(target_os = "macos")]
    pub fn set_preferred_fullscreen(&self, fullscreen: bool) {
        if !fullscreen {
            let (native_fullscreen, simple_fullscreen, _) = self.macos_fullscreen_flags();
            if simple_fullscreen {
                self.set_simple_fullscreen(false);
            }
            if native_fullscreen {
                self.set_fullscreen(false);
            }
            return;
        }

        if self.macos_should_use_real_ear_fullscreen() {
            self.set_simple_fullscreen(true);
        } else {
            self.set_fullscreen(true);
        }
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        #[cfg(target_os = "macos")]
        {
            let (native_fullscreen, simple_fullscreen, _) = self.macos_fullscreen_flags();
            if simple_fullscreen {
                self.set_simple_fullscreen(false);
            }
            if fullscreen {
                self.restore_macos_window_controls();
            }
            if !fullscreen {
                self.clear_macos_notch_ear_windows();
            }
            if native_fullscreen == fullscreen {
                return;
            }

            if fullscreen {
                self.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
            } else {
                self.window.set_fullscreen(None);
            }
        }

        #[cfg(not(target_os = "macos"))]
        if fullscreen {
            self.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            self.window.set_fullscreen(None);
        }
    }

    pub fn current_monitor(&self) -> Option<MonitorHandle> {
        self.window.current_monitor()
    }

    #[cfg(target_os = "macos")]
    pub fn set_simple_fullscreen(&self, simple_fullscreen: bool) {
        self.restore_macos_window_controls();
        if !simple_fullscreen {
            self.clear_macos_notch_ear_windows();
        }
        self.window.set_simple_fullscreen(simple_fullscreen);
    }

    #[cfg(target_os = "macos")]
    pub fn macos_fullscreen_flags(&self) -> (bool, bool, bool) {
        let _mtm = MainThreadMarker::new().expect("macOS fullscreen check must run on main thread");
        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => panic!("macOS window should always use an AppKit handle"),
        };
        let window = view.window().expect("NSView should always be attached to NSWindow");

        let native_fullscreen = window.styleMask().contains(NSWindowStyleMask::FullScreen);
        let simple_fullscreen = self.window.simple_fullscreen();
        let winit_fullscreen = self.window.fullscreen().is_some();

        (native_fullscreen, simple_fullscreen, winit_fullscreen)
    }

    #[cfg(target_os = "macos")]
    pub fn macos_real_ear_fullscreen_active(&self) -> bool {
        let (_native_fullscreen, simple_fullscreen, _) = self.macos_fullscreen_flags();
        simple_fullscreen && self.macos_should_use_real_ear_fullscreen()
    }

    #[cfg(target_os = "macos")]
    pub fn macos_real_ear_top_padding_px(&self) -> f32 {
        if !self.macos_real_ear_fullscreen_active() {
            return 0.0;
        }

        self.macos_notch_geometry()
            .map(|geometry| (geometry.safe_area_insets.top * self.scale_factor) as f32)
            .unwrap_or(0.0)
    }

    #[cfg(target_os = "macos")]
    pub fn macos_fullscreen_window_controls_band_height_px(&self, padding_y_px: f32) -> f32 {
        macos_fullscreen_window_controls_band_height_px(
            self.scale_factor,
            padding_y_px,
            self.macos_real_ear_top_padding_px(),
        )
    }

    #[cfg(target_os = "macos")]
    pub fn macos_fullscreen_window_controls_extra_top_padding_px(&self, padding_y_px: f32) -> f32 {
        macos_fullscreen_window_controls_extra_top_padding_px(
            self.scale_factor,
            padding_y_px,
            self.macos_real_ear_top_padding_px(),
        )
    }

    #[cfg(target_os = "macos")]
    fn macos_should_use_real_ear_fullscreen(&self) -> bool {
        self.macos_real_ear_fullscreen_enabled
            && self
                .macos_notch_geometry()
                .is_some_and(|geometry| macos_has_auxiliary_notch_regions(&geometry))
    }

    #[cfg(target_os = "macos")]
    fn macos_notch_geometry(&self) -> Option<MacosNotchGeometry> {
        let _mtm = MainThreadMarker::new()?;
        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return None,
        };
        let window = view.window()?;
        let screen = window.screen()?;
        Some(MacosNotchGeometry {
            safe_area_insets: screen.safeAreaInsets(),
            auxiliary_top_left_area: screen.auxiliaryTopLeftArea(),
            auxiliary_top_right_area: screen.auxiliaryTopRightArea(),
        })
    }

    #[cfg(target_os = "macos")]
    pub fn macos_window_debug_state(&self) -> Option<IpcWindowDebugState> {
        let _mtm = MainThreadMarker::new()?;

        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return None,
        };

        let window = view.window()?;
        let screen = window.screen()?;
        let content_frame =
            window.convertRectToScreen(view.convertRect_toView(view.bounds(), None));
        let (native_fullscreen, simple_fullscreen, winit_fullscreen) =
            self.macos_fullscreen_flags();
        let real_ear_fullscreen_active = self.macos_real_ear_fullscreen_active();
        let notch_ears = self.macos_notch_ears.borrow();
        let (left_ear_window_number, right_ear_window_number) = notch_ears.active_window_numbers();

        Some(IpcWindowDebugState {
            native_fullscreen,
            simple_fullscreen,
            winit_fullscreen,
            real_ear_fullscreen_active,
            is_miniaturized: window.isMiniaturized(),
            notch_ears_active: notch_ears.is_active() || real_ear_fullscreen_active,
            scale_factor: self.scale_factor,
            is_key_window: window.isKeyWindow(),
            first_responder_class: window
                .firstResponder()
                .map(|responder| ns_object_class_name(Retained::as_ptr(&responder).cast())),
            content_view_class: window
                .contentView()
                .map(|content_view| ns_object_class_name(Retained::as_ptr(&content_view).cast())),
            window_number: Some(window.windowNumber() as i64),
            left_ear_window_number,
            right_ear_window_number,
            screen_frame_points: ns_rect_to_ipc(screen.frame()),
            content_frame_screen_points: ns_rect_to_ipc(content_frame),
            safe_area_insets_points: ns_edge_insets_to_ipc(screen.safeAreaInsets()),
            auxiliary_top_left_screen_points: ns_rect_to_ipc(screen.auxiliaryTopLeftArea()),
            auxiliary_top_right_screen_points: ns_rect_to_ipc(screen.auxiliaryTopRightArea()),
        })
    }

    #[cfg(target_os = "macos")]
    pub fn sync_macos_notch_ear_windows(&self, background_color: Rgb, highlight_notch_ears: bool) {
        let Some(_mtm) = MainThreadMarker::new() else {
            return;
        };

        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return,
        };
        let Some(window) = view.window() else {
            self.clear_macos_notch_ear_windows();
            return;
        };
        let Some(screen) = window.screen() else {
            self.clear_macos_notch_ear_windows();
            return;
        };
        let state = match self.macos_window_debug_state() {
            Some(state) => state,
            None => {
                self.clear_macos_notch_ear_windows();
                return;
            },
        };
        let left_rect = screen.auxiliaryTopLeftArea();
        let right_rect = screen.auxiliaryTopRightArea();

        if !state.native_fullscreen
            || ((left_rect.size.width <= 0.0 || left_rect.size.height <= 0.0)
                && (right_rect.size.width <= 0.0 || right_rect.size.height <= 0.0))
        {
            self.clear_macos_notch_ear_windows();
            return;
        }

        let color = if highlight_notch_ears {
            ns_color_from_rgb(Rgb::new(255, 0, 0))
        } else {
            ns_color_from_rgb(background_color)
        };

        let mut ears = self.macos_notch_ears.borrow_mut();
        Self::macos_sync_notch_ear_window(
            &window,
            Some(&screen),
            &mut ears.left,
            left_rect,
            &color,
        );
        Self::macos_sync_notch_ear_window(
            &window,
            Some(&screen),
            &mut ears.right,
            right_rect,
            &color,
        );
    }

    #[cfg(target_os = "macos")]
    pub fn clear_macos_notch_ear_windows(&self) {
        let mut ears = self.macos_notch_ears.borrow_mut();
        Self::macos_clear_notch_ear_window(&mut ears.left);
        Self::macos_clear_notch_ear_window(&mut ears.right);
    }

    #[cfg(target_os = "macos")]
    pub fn macos_press_standard_window_button(
        &self,
        button: IpcWindowDebugButton,
    ) -> std::result::Result<(), String> {
        self.macos_click_standard_window_button(button)
    }

    #[cfg(target_os = "macos")]
    fn macos_click_standard_window_button(
        &self,
        button: IpcWindowDebugButton,
    ) -> std::result::Result<(), String> {
        let _mtm = MainThreadMarker::new()
            .ok_or_else(|| String::from("macOS button click requires main thread"))?;
        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return Err(String::from("macOS standard buttons require AppKit window")),
        };
        let window = view
            .window()
            .ok_or_else(|| String::from("NSView should always be attached to NSWindow"))?;
        let button_kind = match button {
            IpcWindowDebugButton::Close => NSWindowButton::CloseButton,
            IpcWindowDebugButton::Minimize => NSWindowButton::MiniaturizeButton,
            IpcWindowDebugButton::Zoom => NSWindowButton::ZoomButton,
        };
        if let Some(button) = window.standardWindowButton(button_kind) {
            unsafe {
                button.performClick(None);
            }
            return Ok(());
        }

        match button {
            IpcWindowDebugButton::Close => unsafe {
                let _: () = msg_send![&*window, winitPerformClose: Option::<&objc2::runtime::AnyObject>::None];
            },
            IpcWindowDebugButton::Minimize => unsafe {
                let _: () = msg_send![&*window, winitPerformMiniaturize: Option::<&objc2::runtime::AnyObject>::None];
            },
            IpcWindowDebugButton::Zoom => unsafe {
                let _: () = msg_send![&*window, toggleFullScreen: Option::<&objc2::runtime::AnyObject>::None];
            },
        }

        Ok(())
    }

    /// Set IME inhibitor state and disable IME while any are present.
    ///
    /// IME is re-enabled once all inhibitors are unset.
    pub fn set_ime_inhibitor(&mut self, inhibitor: ImeInhibitor, inhibit: bool) {
        if self.ime_inhibitor.contains(inhibitor) != inhibit {
            self.ime_inhibitor.set(inhibitor, inhibit);
            self.window.set_ime_allowed(self.ime_inhibitor.is_empty());
        }
    }

    /// Adjust the IME editor position according to the new location of the cursor.
    pub fn update_ime_position(&self, point: Point<usize>, size: &SizeInfo) {
        // NOTE: X11 doesn't support cursor area, so we need to offset manually to not obscure
        // the text.
        let offset = if self.is_x11 { 1 } else { 0 };
        let nspot_x = f64::from(size.padding_x() + point.column.0 as f32 * size.cell_width());
        let nspot_y =
            f64::from(size.padding_y() + (point.line + offset) as f32 * size.cell_height());

        // NOTE: some compositors don't like excluding too much and try to render popup at the
        // bottom right corner of the provided area, so exclude just the full-width char to not
        // obscure the cursor and not render popup at the end of the window.
        let width = size.cell_width() as f64 * 2.;
        let height = size.cell_height as f64;

        self.window.set_ime_cursor_area(
            PhysicalPosition::new(nspot_x, nspot_y),
            PhysicalSize::new(width, height),
        );
    }

    /// Disable macOS window shadows.
    ///
    /// This prevents rendering artifacts from showing up when the window is transparent.
    #[cfg(target_os = "macos")]
    pub fn set_has_shadow(&self, has_shadows: bool) {
        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => {
                assert!(MainThreadMarker::new().is_some());
                unsafe { handle.ns_view.cast::<NSView>().as_ref() }
            },
            _ => return,
        };

        view.window().unwrap().setHasShadow(has_shadows);
    }

    /// Position macOS window controls inside the left panel.
    #[cfg(target_os = "macos")]
    pub fn layout_macos_window_controls(
        &self,
        panel_width_px: f32,
        padding_y_px: f32,
    ) -> Option<f32> {
        let _mtm = MainThreadMarker::new()?;
        self.clear_macos_fullscreen_window_control_overlays();

        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return None,
        };

        let window = view.window()?;

        let close = window.standardWindowButton(NSWindowButton::CloseButton)?;
        let mini = window.standardWindowButton(NSWindowButton::MiniaturizeButton)?;
        let zoom = window.standardWindowButton(NSWindowButton::ZoomButton)?;
        let button_container = unsafe { close.superview()? };

        {
            let mut controls = self.macos_window_controls.borrow_mut();
            controls.original_frames.get_or_insert(MacosWindowControlFrames {
                close_frame: close.frame(),
                mini_frame: mini.frame(),
                zoom_frame: zoom.frame(),
            });
        }

        if panel_width_px <= 0.0 {
            self.restore_macos_window_controls();
            return None;
        }

        let scale_factor = self.scale_factor;

        let close_frame = close.frame();
        let mini_frame = mini.frame();
        let zoom_frame = zoom.frame();

        let button_height =
            close_frame.size.height.max(mini_frame.size.height).max(zoom_frame.size.height);
        let left_margin = MACOS_TRAFFIC_LIGHT_MARGIN_X;
        let top_margin = (f64::from(padding_y_px) / scale_factor).max(MACOS_TRAFFIC_LIGHT_MARGIN_Y);
        let top_inset_points = button_height + top_margin;
        Self::position_macos_window_controls(
            &button_container,
            &close,
            &mini,
            &zoom,
            left_margin,
            top_margin,
        );
        button_container.layoutSubtreeIfNeeded();

        Some((top_inset_points * scale_factor) as f32)
    }

    #[cfg(target_os = "macos")]
    pub fn layout_macos_fullscreen_window_controls(
        &self,
        panel_width_px: f32,
        band_height_px: f32,
    ) -> bool {
        let mtm = match MainThreadMarker::new() {
            Some(mtm) => mtm,
            None => return false,
        };

        if panel_width_px <= 0.0 || band_height_px <= 0.0 {
            self.clear_macos_fullscreen_window_control_overlays();
            return false;
        }

        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return false,
        };

        let Some(window) = view.window() else {
            return false;
        };
        let Some(button_container) = (unsafe { view.superview() }) else {
            return false;
        };
        let overlay_frames = Self::macos_fullscreen_window_control_overlay_frames(
            view,
            &button_container,
            band_height_px,
            self.scale_factor,
        );
        let Some([close_frame, mini_frame, zoom_frame]) = overlay_frames else {
            self.clear_macos_fullscreen_window_control_overlays();
            return false;
        };

        {
            let mut controls = self.macos_window_controls.borrow_mut();
            let recreate = match controls.fullscreen_overlays.as_ref() {
                Some(overlays) => !Self::macos_window_control_overlays_match_container(
                    overlays,
                    &button_container,
                ),
                None => true,
            };
            if recreate {
                if let Some(overlays) = controls.fullscreen_overlays.take() {
                    Self::remove_macos_window_control_overlays(overlays);
                }
                controls.fullscreen_overlays = Some(Self::create_macos_window_control_overlays(
                    mtm,
                    &window,
                    &button_container,
                ));
            }

            let overlays =
                controls.fullscreen_overlays.as_ref().expect("fullscreen overlays just created");
            overlays.close.setFrame(close_frame);
            overlays.mini.setFrame(mini_frame);
            overlays.zoom.setFrame(zoom_frame);
        }

        button_container.layoutSubtreeIfNeeded();

        true
    }

    #[cfg(target_os = "macos")]
    pub fn restore_macos_window_controls(&self) {
        let _mtm = match MainThreadMarker::new() {
            Some(mtm) => mtm,
            None => return,
        };
        self.clear_macos_fullscreen_window_control_overlays();

        let Some(frames) = self.macos_window_controls.borrow().original_frames else {
            return;
        };

        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return,
        };

        let Some(window) = view.window() else {
            return;
        };

        let Some(close) = window.standardWindowButton(NSWindowButton::CloseButton) else {
            return;
        };
        let Some(mini) = window.standardWindowButton(NSWindowButton::MiniaturizeButton) else {
            return;
        };
        let Some(zoom) = window.standardWindowButton(NSWindowButton::ZoomButton) else {
            return;
        };
        let Some(button_container) = (unsafe { close.superview() }) else {
            return;
        };

        close.setFrame(frames.close_frame);
        mini.setFrame(frames.mini_frame);
        zoom.setFrame(frames.zoom_frame);
        button_container.layoutSubtreeIfNeeded();
    }

    #[cfg(target_os = "macos")]
    pub fn set_macos_background_color(&self, color: Rgb) {
        let _mtm = match MainThreadMarker::new() {
            Some(mtm) => mtm,
            None => return,
        };

        let view = match self.raw_window_handle() {
            RawWindowHandle::AppKit(handle) => unsafe { handle.ns_view.cast::<NSView>().as_ref() },
            _ => return,
        };

        let window = match view.window() {
            Some(window) => window,
            None => return,
        };

        let ns_color = ns_color_from_rgb(color);
        window.setBackgroundColor(Some(&ns_color));
    }

    #[cfg(target_os = "macos")]
    fn macos_sync_notch_ear_window(
        parent_window: &AppKitWindow,
        screen: Option<&NSScreen>,
        slot: &mut Option<Retained<AppKitWindow>>,
        rect: NSRect,
        color: &Retained<NSColor>,
    ) {
        if rect.size.width <= 0.0 || rect.size.height <= 0.0 {
            Self::macos_clear_notch_ear_window(slot);
            return;
        }

        let ear_window = slot.get_or_insert_with(|| {
            let mtm = MainThreadMarker::new().expect("ear window creation requires main thread");
            let ear = unsafe {
                AppKitWindow::initWithContentRect_styleMask_backing_defer_screen(
                    AppKitWindow::alloc(mtm),
                    rect,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                    screen,
                )
            };
            ear.setCollectionBehavior(
                NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
            ear.setAnimationBehavior(NSWindowAnimationBehavior::None);
            ear.setHasShadow(false);
            ear.setOpaque(true);
            ear.setIgnoresMouseEvents(true);
            ear.setMovable(false);
            ear.setMovableByWindowBackground(false);
            ear.setCanHide(false);
            ear.setExcludedFromWindowsMenu(true);
            unsafe {
                ear.setReleasedWhenClosed(false);
            }
            ear
        });

        ear_window.setLevel(parent_window.level());
        ear_window.setBackgroundColor(Some(color));
        ear_window.setFrame_display(rect, false);
        ear_window
            .orderWindow_relativeTo(NSWindowOrderingMode::Below, parent_window.windowNumber());
    }

    #[cfg(target_os = "macos")]
    fn macos_clear_notch_ear_window(slot: &mut Option<Retained<AppKitWindow>>) {
        if let Some(window) = slot.take() {
            window.orderOut(None);
        }
    }

    #[cfg(target_os = "macos")]
    fn position_macos_window_controls(
        button_container: &NSView,
        close: &NSButton,
        mini: &NSButton,
        zoom: &NSButton,
        left_margin: f64,
        top_margin: f64,
    ) {
        let close_frame = close.frame();
        let mini_frame = mini.frame();
        let zoom_frame = zoom.frame();
        let mini_dx = mini_frame.origin.x - close_frame.origin.x;
        let zoom_dx = zoom_frame.origin.x - close_frame.origin.x;

        close.setFrameOrigin(NSPoint::new(
            left_margin,
            Self::macos_window_control_origin_y(
                button_container,
                top_margin,
                close_frame.size.height,
            ),
        ));
        mini.setFrameOrigin(NSPoint::new(
            left_margin + mini_dx,
            Self::macos_window_control_origin_y(
                button_container,
                top_margin,
                mini_frame.size.height,
            ),
        ));
        zoom.setFrameOrigin(NSPoint::new(
            left_margin + zoom_dx,
            Self::macos_window_control_origin_y(
                button_container,
                top_margin,
                zoom_frame.size.height,
            ),
        ));
    }

    #[cfg(target_os = "macos")]
    fn macos_window_control_origin_y(
        button_container: &NSView,
        top_margin: f64,
        button_height: f64,
    ) -> f64 {
        if button_container.isFlipped() {
            top_margin.max(0.0)
        } else {
            (button_container.bounds().size.height - top_margin - button_height).max(0.0)
        }
    }

    #[cfg(target_os = "macos")]
    fn create_macos_window_control_overlays(
        mtm: MainThreadMarker,
        window: &AppKitWindow,
        button_container: &NSView,
    ) -> MacosWindowControlOverlays {
        let close = Self::new_macos_window_control_overlay(
            mtm,
            window,
            button_container,
            sel!(winitPerformClose:),
        );
        let mini = Self::new_macos_window_control_overlay(
            mtm,
            window,
            button_container,
            sel!(winitPerformMiniaturize:),
        );
        let zoom = Self::new_macos_window_control_overlay(
            mtm,
            window,
            button_container,
            sel!(toggleFullScreen:),
        );
        MacosWindowControlOverlays { close, mini, zoom }
    }

    #[cfg(target_os = "macos")]
    fn new_macos_window_control_overlay(
        mtm: MainThreadMarker,
        window: &AppKitWindow,
        button_container: &NSView,
        action: objc2::runtime::Sel,
    ) -> Retained<NSButton> {
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str(""),
                Some(window),
                Some(action),
                mtm,
            )
        };
        button.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)));
        button.setBordered(false);
        button.setTransparent(true);
        button.setRefusesFirstResponder(true);
        button.setAlphaValue(0.01);
        button.setEnabled(true);
        button_container.addSubview(&button);
        button
    }

    #[cfg(target_os = "macos")]
    fn clear_macos_fullscreen_window_control_overlays(&self) {
        let Some(_mtm) = MainThreadMarker::new() else {
            return;
        };

        let overlays = self.macos_window_controls.borrow_mut().fullscreen_overlays.take();
        if let Some(overlays) = overlays {
            Self::remove_macos_window_control_overlays(overlays);
        }
    }

    #[cfg(target_os = "macos")]
    fn remove_macos_window_control_overlays(overlays: MacosWindowControlOverlays) {
        overlays.close.removeFromSuperview();
        overlays.mini.removeFromSuperview();
        overlays.zoom.removeFromSuperview();
    }

    #[cfg(target_os = "macos")]
    fn macos_window_control_overlays_match_container(
        overlays: &MacosWindowControlOverlays,
        button_container: &NSView,
    ) -> bool {
        let Some(superview) = (unsafe { overlays.close.superview() }) else {
            return false;
        };

        std::ptr::eq((&*superview) as *const NSView, button_container as *const NSView)
    }

    #[cfg(target_os = "macos")]
    fn macos_fullscreen_window_control_overlay_frames(
        view: &NSView,
        button_container: &NSView,
        band_height_px: f32,
        scale_factor: f64,
    ) -> Option<[NSRect; 3]> {
        let band_height_px = f64::from(band_height_px).max(0.0);
        if band_height_px <= 0.0 || scale_factor <= 0.0 {
            return None;
        }

        let size_px = Self::macos_fullscreen_window_control_metric_px(
            band_height_px,
            MACOS_FULLSCREEN_WINDOW_CONTROL_SIZE_PX,
        )
        .min((band_height_px - 4.0).max(MACOS_FULLSCREEN_WINDOW_CONTROL_SIZE_PX));
        let padding_px = Self::macos_fullscreen_window_control_hit_padding_px(size_px);
        let top_px = ((band_height_px - size_px) / 2.0).max(0.0) - padding_px;
        let width_px = size_px + padding_px * 2.0;
        let height_px = size_px + padding_px * 2.0;
        let spacing_px = Self::macos_fullscreen_window_control_metric_px(
            band_height_px,
            MACOS_FULLSCREEN_WINDOW_CONTROL_SPACING_PX,
        );
        let mut x_px = Self::macos_fullscreen_window_control_metric_px(
            band_height_px,
            MACOS_FULLSCREEN_WINDOW_CONTROL_MARGIN_X_PX,
        ) - padding_px;

        Some([
            Self::macos_convert_overlay_frame_to_button_container(
                view,
                button_container,
                x_px / scale_factor,
                top_px / scale_factor,
                width_px / scale_factor,
                height_px / scale_factor,
            ),
            {
                x_px += size_px + spacing_px;
                Self::macos_convert_overlay_frame_to_button_container(
                    view,
                    button_container,
                    x_px / scale_factor,
                    top_px / scale_factor,
                    width_px / scale_factor,
                    height_px / scale_factor,
                )
            },
            {
                x_px += size_px + spacing_px;
                Self::macos_convert_overlay_frame_to_button_container(
                    view,
                    button_container,
                    x_px / scale_factor,
                    top_px / scale_factor,
                    width_px / scale_factor,
                    height_px / scale_factor,
                )
            },
        ])
    }

    #[cfg(target_os = "macos")]
    fn macos_convert_overlay_frame_to_button_container(
        view: &NSView,
        button_container: &NSView,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> NSRect {
        let view_rect = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
        button_container.convertRect_fromView(view_rect, Some(view))
    }

    #[cfg(target_os = "macos")]
    fn macos_fullscreen_window_control_metric_px(band_height_px: f64, reference_px: f64) -> f64 {
        (band_height_px * (reference_px / MACOS_FULLSCREEN_WINDOW_CONTROL_REFERENCE_BAND_PX))
            .round()
            .max(reference_px)
    }

    #[cfg(target_os = "macos")]
    fn macos_fullscreen_window_control_hit_padding_px(size_px: f64) -> f64 {
        (size_px * 0.2).round().max(3.0)
    }
}

bitflags! {
    /// IME inhibition sources.
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct ImeInhibitor: u8 {
        const FOCUS = 1;
        const TOUCH = 1 << 1;
        const VI    = 1 << 2;
    }
}

#[cfg(target_os = "macos")]
fn macos_real_ear_fullscreen_enabled(window_config: &WindowConfig) -> bool {
    window_config.tab_panel.enabled
        && matches!(window_config.decorations, Decorations::Full | Decorations::Transparent)
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_fullscreen_window_controls_band_height_px(
    scale_factor: f64,
    padding_y_px: f32,
    fullscreen_top_padding_px: f32,
) -> f32 {
    let min_band_height = (MACOS_FULLSCREEN_WINDOW_CONTROL_REFERENCE_BAND_PX * scale_factor) as f32;
    (padding_y_px + fullscreen_top_padding_px).max(min_band_height)
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_fullscreen_window_controls_extra_top_padding_px(
    scale_factor: f64,
    padding_y_px: f32,
    fullscreen_top_padding_px: f32,
) -> f32 {
    (macos_fullscreen_window_controls_band_height_px(
        scale_factor,
        padding_y_px,
        fullscreen_top_padding_px,
    ) - padding_y_px)
        .max(0.0)
}

#[cfg(target_os = "macos")]
fn macos_has_auxiliary_notch_regions(geometry: &MacosNotchGeometry) -> bool {
    (geometry.auxiliary_top_left_area.size.width > 0.0
        && geometry.auxiliary_top_left_area.size.height > 0.0)
        || (geometry.auxiliary_top_right_area.size.width > 0.0
            && geometry.auxiliary_top_right_area.size.height > 0.0)
}

#[cfg(target_os = "macos")]
fn use_srgb_color_space(window: &WinitWindow) {
    let view = match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::AppKit(handle) => {
            assert!(MainThreadMarker::new().is_some());
            unsafe { handle.ns_view.cast::<NSView>().as_ref() }
        },
        _ => return,
    };

    view.window().unwrap().setColorSpace(Some(&NSColorSpace::sRGBColorSpace()));
}

#[cfg(target_os = "macos")]
fn configure_macos_fullscreen_behavior(window: &WinitWindow, prefer_simple_fullscreen: bool) {
    let view = match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::AppKit(handle) => {
            assert!(MainThreadMarker::new().is_some());
            unsafe { handle.ns_view.cast::<NSView>().as_ref() }
        },
        _ => return,
    };

    let window = view.window().unwrap();
    window.setCollectionBehavior(
        window.collectionBehavior()
            | NSWindowCollectionBehavior::FullScreenPrimary
            | NSWindowCollectionBehavior::FullScreenAllowsTiling,
    );
    let _: () =
        unsafe { msg_send![&*window, setWinitPreferSimpleFullscreen: prefer_simple_fullscreen] };
}

#[cfg(target_os = "macos")]
fn ns_color_from_rgb(color: Rgb) -> Retained<NSColor> {
    let red = f64::from(color.r) / 255.0;
    let green = f64::from(color.g) / 255.0;
    let blue = f64::from(color.b) / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(red, green, blue, 1.0)
}

#[cfg(target_os = "macos")]
fn ns_rect_to_ipc(rect: NSRect) -> IpcWindowDebugRect {
    IpcWindowDebugRect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

#[cfg(target_os = "macos")]
fn ns_edge_insets_to_ipc(insets: NSEdgeInsets) -> IpcWindowDebugInsets {
    IpcWindowDebugInsets {
        top: insets.top,
        left: insets.left,
        bottom: insets.bottom,
        right: insets.right,
    }
}

#[cfg(target_os = "macos")]
fn ns_object_class_name(object: *const AnyObject) -> String {
    let Some(object) = (unsafe { object.as_ref() }) else {
        return String::from("Unknown");
    };
    object.class().name().to_string_lossy().into_owned()
}

#[cfg(target_os = "macos")]
fn restore_first_responder_with_retry<F, G>(mut clear_first_responder: F, mut focus_view: G) -> bool
where
    F: FnMut() -> bool,
    G: FnMut() -> bool,
{
    if focus_view() {
        return true;
    }

    let _ = clear_first_responder();
    focus_view()
}

#[cfg(all(target_os = "macos", test))]
mod tests {
    use super::restore_first_responder_with_retry;

    #[test]
    fn restore_first_responder_returns_immediately_on_success() {
        let mut clear_calls = 0;
        let mut focus_calls = 0;

        let restored = restore_first_responder_with_retry(
            || {
                clear_calls += 1;
                true
            },
            || {
                focus_calls += 1;
                true
            },
        );

        assert!(restored);
        assert_eq!(focus_calls, 1);
        assert_eq!(clear_calls, 0);
    }

    #[test]
    fn restore_first_responder_retries_after_clearing_stale_responder() {
        let mut clear_calls = 0;
        let mut focus_calls = 0;

        let restored = restore_first_responder_with_retry(
            || {
                clear_calls += 1;
                true
            },
            || {
                focus_calls += 1;
                focus_calls == 2
            },
        );

        assert!(restored);
        assert_eq!(focus_calls, 2);
        assert_eq!(clear_calls, 1);
    }
}
