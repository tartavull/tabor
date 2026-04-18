//! The display subsystem including window management, font rasterization, and
//! GPU drawing.

use std::cmp;
use std::fmt::{self, Formatter};
use std::mem::{self, ManuallyDrop};
use std::num::NonZeroU32;
use std::ops::Deref;
use std::time::{Duration, Instant};

use glutin::config::GetGlConfig;
use glutin::context::{NotCurrentContext, PossiblyCurrentContext};
use glutin::display::GetGlDisplay;
use glutin::error::ErrorKind;
use glutin::prelude::*;
use glutin::surface::{Surface, SwapInterval, WindowSurface};

use log::{debug, info};
use parking_lot::MutexGuard;
use serde::{Deserialize, Serialize};
use winit::dpi::PhysicalSize;
use winit::keyboard::ModifiersState;
use winit::raw_window_handle::RawWindowHandle;
use winit::window::CursorIcon;

use crossfont::{Rasterize, Rasterizer, Size as FontSize};
use unicode_width::UnicodeWidthChar;

use tabor_terminal::event::{EventListener, OnResize, WindowSize};
use tabor_terminal::grid::Dimensions as TermDimensions;
use tabor_terminal::index::{Column, Direction, Line, Point};
use tabor_terminal::selection::Selection;
use tabor_terminal::term::cell::Flags;
use tabor_terminal::term::{
    self, LineDamageBounds, MIN_COLUMNS, MIN_SCREEN_LINES, ResizeAnchor, Term, TermDamage, TermMode,
};
use tabor_terminal::vte::ansi::{CursorShape, NamedColor};

use crate::config::UiConfig;
use crate::config::debug::RendererPreference;
use crate::config::font::Font;
#[cfg(target_os = "macos")]
use crate::config::window::Decorations;
use crate::config::window::Dimensions;
#[cfg(not(windows))]
use crate::config::window::StartupMode;
use crate::display::auxiliary_regions::{AuxiliaryTopRegion, EarAwareTopRegions};
use crate::display::bell::VisualBell;
use crate::display::browser_layout::BrowserViewportLayout;
use crate::display::color::{List, Rgb};
use crate::display::content::{
    HintMatches, RenderableContent, RenderableContentContext, RenderableCursor,
};
use crate::display::cursor::IntoRects;
use crate::display::damage::{DamageTracker, damage_y_to_viewport_y};
use crate::display::hint::{HintMatch, HintState};
use crate::display::meter::Meter;
#[cfg(target_os = "macos")]
use crate::display::tab_panel::{PanelDimensions, TabPanel, compute_panel_dimensions};
use crate::display::terminal_layout::{TerminalViewMode, TerminalViewportLayout};
use crate::display::window::Window;
use crate::event::{CommandFooterMessage, CommandState, Event, EventType, Mouse, SearchState};
#[cfg(target_os = "macos")]
use crate::macos::image_view::ImageViewState;
#[cfg(target_os = "macos")]
use crate::macos::webview::{WebPopupSurfaceRef, WebView};
use crate::message_bar::{MessageBuffer, MessageType};
use crate::renderer::images::ImageSlice;
#[cfg(target_os = "macos")]
use crate::renderer::images::SurfaceSlot;
use crate::renderer::rects::{RenderLine, RenderLines, RenderRect};
use crate::renderer::{self, GlyphCache, Renderer, platform};
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::string::{ShortenDirection, StrShortener};

#[cfg(not(target_os = "macos"))]
#[derive(Default, Clone, Copy)]
struct PanelDimensions {
    columns: usize,
    width: f32,
}

pub mod auxiliary_regions;

pub mod browser_layout;
pub mod color;
pub mod content;
pub mod cursor;
pub mod hint;
pub mod terminal_layout;
pub mod window;

#[cfg(target_os = "macos")]
mod tab_panel;
#[cfg(target_os = "macos")]
pub(crate) use tab_panel::{TabPanelEditOutcome, TabPanelEditTarget};

#[cfg(target_os = "macos")]
#[derive(Clone)]
pub(crate) struct MacOsWebDraw<'a> {
    web_view: Option<&'a WebView>,
    browser_layout: BrowserViewportLayout,
    force_notch_ears: bool,
}

#[cfg(target_os = "macos")]
impl<'a> MacOsWebDraw<'a> {
    pub(crate) fn new(
        web_view: Option<&'a WebView>,
        browser_layout: BrowserViewportLayout,
        force_notch_ears: bool,
    ) -> Self {
        Self { web_view, browser_layout, force_notch_ears }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacosSemaphoreLayoutMode {
    Hidden,
    WindowedInset,
    FullscreenCustom,
}

mod bell;
mod damage;
mod meter;

/// Label for the forward terminal search bar.
const FORWARD_SEARCH_LABEL: &str = "Search: ";

/// Label for the backward terminal search bar.
const BACKWARD_SEARCH_LABEL: &str = "Backward Search: ";

/// The character used to shorten the visible text like uri preview or search regex.
const SHORTENER: char = '…';

/// Color which is used to highlight damaged rects when debugging.
const DAMAGE_RECT_COLOR: Rgb = Rgb::new(255, 0, 255);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalFooterBarMode {
    None,
    ViIndicator,
    CommandFeedback,
    Search,
    Command,
}

fn terminal_footer_bar_mode(
    command_active: bool,
    search_active: bool,
    command_feedback_active: bool,
    vi_mode: bool,
) -> TerminalFooterBarMode {
    if command_active {
        TerminalFooterBarMode::Command
    } else if search_active {
        TerminalFooterBarMode::Search
    } else if command_feedback_active {
        TerminalFooterBarMode::CommandFeedback
    } else if vi_mode {
        TerminalFooterBarMode::ViIndicator
    } else {
        TerminalFooterBarMode::None
    }
}

fn line_indicator_text(line: usize, total_lines: usize) -> String {
    format!("[{}/{}]", line, total_lines.saturating_sub(1))
}

fn vi_mode_line_indicator_line(
    layout: TerminalViewportLayout,
    size_info: &SizeInfo,
    cursor_point: Point,
) -> usize {
    let logical_bottom = layout.logical_size(size_info).bottommost_line().0;
    usize::try_from(logical_bottom - cursor_point.line.0)
        .expect("vi mode cursor should stay within the logical viewport")
}

fn footer_text_max_width(total_columns: usize, right_text: Option<&str>) -> usize {
    let reserved_columns = right_text
        .map(|text| {
            let text_width = text.chars().count();
            if text_width >= total_columns { text_width } else { text_width + 1 }
        })
        .unwrap_or(0);
    total_columns.saturating_sub(reserved_columns)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FooterBarViewportBand {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl FooterBarViewportBand {
    fn new(size_info: &SizeInfo<f32>, line: usize, offset_y: f32, height: f32) -> Self {
        let y = size_info.cell_height().mul_add(line as f32, size_info.padding_y()) + offset_y;
        let x = size_info.padding_x();
        let width = (size_info.width() - size_info.padding_x() - size_info.padding_right()).max(0.);

        Self { x, y, width, height }
    }

    fn damage_rect(self) -> (i32, i32, i32, i32) {
        (self.x as i32, self.y as i32, self.width as i32, self.height as i32)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DisplayUpdateOptions {
    pub(crate) web_status_bar: bool,
    pub(crate) terminal_view_mode: Option<TerminalViewMode>,
    pub(crate) exact_multi_column_count: Option<usize>,
}

#[derive(Debug)]
pub enum Error {
    /// Error with window management.
    Window(window::Error),

    /// Error dealing with fonts.
    Font(crossfont::Error),

    /// Error in renderer.
    Render(renderer::Error),

    /// Error during context operations.
    Context(glutin::error::Error),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Window(err) => err.source(),
            Error::Font(err) => err.source(),
            Error::Render(err) => err.source(),
            Error::Context(err) => err.source(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Window(err) => err.fmt(f),
            Error::Font(err) => err.fmt(f),
            Error::Render(err) => err.fmt(f),
            Error::Context(err) => err.fmt(f),
        }
    }
}

impl From<window::Error> for Error {
    fn from(val: window::Error) -> Self {
        Error::Window(val)
    }
}

impl From<crossfont::Error> for Error {
    fn from(val: crossfont::Error) -> Self {
        Error::Font(val)
    }
}

impl From<renderer::Error> for Error {
    fn from(val: renderer::Error) -> Self {
        Error::Render(val)
    }
}

impl From<glutin::error::Error> for Error {
    fn from(val: glutin::error::Error) -> Self {
        Error::Context(val)
    }
}

/// Terminal size info.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct SizeInfo<T = f32> {
    /// Terminal window width.
    width: T,

    /// Terminal window height.
    height: T,

    /// Width of individual cell.
    cell_width: T,

    /// Height of individual cell.
    cell_height: T,

    /// Horizontal window padding.
    padding_x: T,

    /// Right window padding.
    padding_right: T,

    /// Top window padding.
    padding_y: T,

    /// Bottom window padding.
    #[serde(default)]
    padding_bottom: T,

    /// Number of lines in the viewport.
    screen_lines: usize,

    /// Number of columns in the viewport.
    columns: usize,
}

impl From<SizeInfo<f32>> for SizeInfo<u32> {
    fn from(size_info: SizeInfo<f32>) -> Self {
        Self {
            width: size_info.width as u32,
            height: size_info.height as u32,
            cell_width: size_info.cell_width as u32,
            cell_height: size_info.cell_height as u32,
            padding_x: size_info.padding_x as u32,
            padding_right: size_info.padding_right as u32,
            padding_y: size_info.padding_y as u32,
            padding_bottom: size_info.padding_bottom as u32,
            screen_lines: size_info.screen_lines,
            columns: size_info.columns,
        }
    }
}

impl From<SizeInfo<f32>> for WindowSize {
    fn from(size_info: SizeInfo<f32>) -> Self {
        Self {
            num_cols: size_info.columns() as u16,
            num_lines: size_info.screen_lines() as u16,
            cell_width: size_info.cell_width() as u16,
            cell_height: size_info.cell_height() as u16,
        }
    }
}

impl<T: Clone + Copy> SizeInfo<T> {
    #[inline]
    pub fn width(&self) -> T {
        self.width
    }

    #[inline]
    pub fn height(&self) -> T {
        self.height
    }

    #[inline]
    pub fn cell_width(&self) -> T {
        self.cell_width
    }

    #[inline]
    pub fn cell_height(&self) -> T {
        self.cell_height
    }

    #[inline]
    pub fn padding_x(&self) -> T {
        self.padding_x
    }

    #[inline]
    pub fn padding_right(&self) -> T {
        self.padding_right
    }

    #[inline]
    pub fn padding_y(&self) -> T {
        self.padding_y
    }

    #[inline]
    pub fn padding_bottom(&self) -> T {
        self.padding_bottom
    }
}

impl SizeInfo<f32> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: f32,
        height: f32,
        cell_width: f32,
        cell_height: f32,
        padding_x: f32,
        padding_right: f32,
        padding_y: f32,
        dynamic_padding: bool,
    ) -> SizeInfo {
        Self::new_with_vertical_padding(
            width,
            height,
            cell_width,
            cell_height,
            padding_x,
            padding_right,
            padding_y,
            padding_y,
            dynamic_padding,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_vertical_padding(
        width: f32,
        height: f32,
        cell_width: f32,
        cell_height: f32,
        mut padding_x: f32,
        mut padding_right: f32,
        mut padding_y: f32,
        mut padding_bottom: f32,
        dynamic_padding: bool,
    ) -> SizeInfo {
        if dynamic_padding {
            padding_x = Self::dynamic_padding(padding_x.floor(), width, cell_width);
            padding_right = padding_x;
            let padding_top = padding_y.floor();
            let padding_bottom_floor = padding_bottom.floor();
            let extra = ((height - padding_top - padding_bottom_floor) % cell_height) / 2.;
            padding_y = padding_top + extra;
            padding_bottom = padding_bottom_floor + extra;
        }

        let lines = (height - padding_y - padding_bottom) / cell_height;
        let screen_lines = cmp::max(lines as usize, MIN_SCREEN_LINES);

        let columns = (width - padding_x - padding_right) / cell_width;
        let columns = cmp::max(columns as usize, MIN_COLUMNS);

        SizeInfo {
            width,
            height,
            cell_width,
            cell_height,
            padding_x: padding_x.floor(),
            padding_right: padding_right.floor(),
            padding_y: padding_y.floor(),
            padding_bottom: padding_bottom.floor(),
            screen_lines,
            columns,
        }
    }

    #[inline]
    pub fn viewport_height(&self) -> f32 {
        (self.height - self.padding_y - self.padding_bottom).max(0.)
    }

    #[inline]
    pub fn footer_offset(&self) -> f32 {
        let grid_bottom = self.padding_y + self.screen_lines as f32 * self.cell_height;
        (self.height - grid_bottom).max(0.)
    }

    #[inline]
    pub fn reserve_lines(&mut self, count: usize) {
        self.screen_lines = cmp::max(self.screen_lines.saturating_sub(count), MIN_SCREEN_LINES);
    }

    /// Check if coordinates are inside the terminal grid.
    ///
    /// The padding, message bar or search are not counted as part of the grid.
    #[inline]
    pub fn contains_point(&self, x: usize, y: usize) -> bool {
        x <= (self.padding_x + self.columns as f32 * self.cell_width) as usize
            && x > self.padding_x as usize
            && y <= (self.padding_y + self.screen_lines as f32 * self.cell_height) as usize
            && y > self.padding_y as usize
    }

    /// Calculate padding to spread it evenly around the terminal content.
    #[inline]
    fn dynamic_padding(padding: f32, dimension: f32, cell_dimension: f32) -> f32 {
        padding + ((dimension - 2. * padding) % cell_dimension) / 2.
    }
}

impl TermDimensions for SizeInfo {
    #[inline]
    fn columns(&self) -> usize {
        self.columns
    }

    #[inline]
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    #[inline]
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct DisplayUpdate {
    pub dirty: bool,

    dimensions: Option<PhysicalSize<u32>>,
    cursor_dirty: bool,
    font: Option<Font>,
}

impl DisplayUpdate {
    pub fn dimensions(&self) -> Option<PhysicalSize<u32>> {
        self.dimensions
    }

    pub fn font(&self) -> Option<&Font> {
        self.font.as_ref()
    }

    pub fn cursor_dirty(&self) -> bool {
        self.cursor_dirty
    }

    pub fn set_dimensions(&mut self, dimensions: PhysicalSize<u32>) {
        self.dimensions = Some(dimensions);
        self.dirty = true;
    }

    pub fn set_font(&mut self, font: Font) {
        self.font = Some(font);
        self.dirty = true;
    }

    pub fn set_cursor_dirty(&mut self) {
        self.cursor_dirty = true;
        self.dirty = true;
    }
}

/// The display wraps a window, font rasterizer, and GPU renderer.
pub struct Display {
    pub window: Window,

    pub size_info: SizeInfo,
    pub terminal_viewport: TerminalViewportLayout,
    active_status_lines: usize,

    #[cfg(target_os = "macos")]
    pub tab_panel: TabPanel,

    /// Hint highlighted by the mouse.
    pub highlighted_hint: Option<HintMatch>,
    /// Frames since hint highlight was created.
    highlighted_hint_age: usize,

    /// Hint highlighted by the vi mode cursor.
    pub vi_highlighted_hint: Option<HintMatch>,
    /// Frames since hint highlight was created.
    vi_highlighted_hint_age: usize,

    pub raw_window_handle: RawWindowHandle,

    /// UI cursor visibility for blinking.
    pub cursor_hidden: bool,

    pub visual_bell: VisualBell,

    /// Mapped RGB values for each terminal color.
    pub colors: List,

    /// State of the keyboard hints.
    pub hint_state: HintState,

    /// Unprocessed display updates.
    pub pending_update: DisplayUpdate,

    /// The renderer update that takes place only once before the actual rendering.
    pub pending_renderer_update: Option<RendererUpdate>,

    /// The ime on the given display.
    pub ime: Ime,

    /// The state of the timer for frame scheduling.
    pub frame_timer: FrameTimer,

    /// Damage tracker for the given display.
    pub damage_tracker: DamageTracker,

    /// Font size used by the window.
    pub font_size: FontSize,

    // Mouse point position when highlighting hints.
    hint_mouse_point: Option<Point>,

    renderer: ManuallyDrop<Renderer>,
    renderer_preference: Option<RendererPreference>,

    surface: ManuallyDrop<Surface<WindowSurface>>,

    context: ManuallyDrop<PossiblyCurrentContext>,

    glyph_cache: GlyphCache,
    meter: Meter,
}

impl Display {
    #[cfg(target_os = "macos")]
    fn macos_debug_notch_ears_enabled() -> bool {
        std::env::var_os("TABOR_DEBUG_NOTCH_EARS").is_some_and(|value| value != "0")
    }

    #[cfg(target_os = "macos")]
    fn macos_semaphore_layout_mode(
        tab_panel_enabled: bool,
        decorations: Decorations,
        is_fullscreen_or_simple_fullscreen: bool,
    ) -> MacosSemaphoreLayoutMode {
        if !tab_panel_enabled
            || !matches!(decorations, Decorations::Full | Decorations::Transparent)
        {
            MacosSemaphoreLayoutMode::Hidden
        } else if is_fullscreen_or_simple_fullscreen {
            MacosSemaphoreLayoutMode::FullscreenCustom
        } else {
            MacosSemaphoreLayoutMode::WindowedInset
        }
    }

    #[cfg(target_os = "macos")]
    fn sync_macos_tab_panel_semaphore_inset(
        window: &Window,
        tab_panel: &mut TabPanel,
        panel_dimensions: PanelDimensions,
        padding_y_px: f32,
        config: &UiConfig,
        highlight_notch_ears: bool,
    ) -> f32 {
        tab_panel.set_enabled(config.window.tab_panel.enabled);
        tab_panel.set_dimensions(panel_dimensions);

        let (native_fullscreen, simple_fullscreen, _) = window.macos_fullscreen_flags();
        let is_fullscreen_or_simple_fullscreen = native_fullscreen || simple_fullscreen;
        let layout_mode = Self::macos_semaphore_layout_mode(
            tab_panel.is_enabled(),
            config.window.decorations,
            is_fullscreen_or_simple_fullscreen,
        );
        let show_native_controls = layout_mode == MacosSemaphoreLayoutMode::WindowedInset;
        let show_fullscreen_controls = layout_mode == MacosSemaphoreLayoutMode::FullscreenCustom;

        let background_color = if show_native_controls {
            TabPanel::background_color(config)
        } else {
            config.colors.primary.background
        };
        window.set_macos_background_color(background_color);
        if native_fullscreen && !show_fullscreen_controls {
            window.sync_macos_notch_ear_windows(background_color, highlight_notch_ears);
        } else {
            window.clear_macos_notch_ear_windows();
        }

        let fullscreen_top_padding = if show_fullscreen_controls {
            window.macos_fullscreen_window_controls_extra_top_padding_px(padding_y_px)
        } else {
            0.0
        };

        if show_native_controls {
            let top_inset = window
                .layout_macos_window_controls(panel_dimensions.width, padding_y_px)
                .unwrap_or(0.0);
            tab_panel.set_native_window_controls_inset_px(top_inset);
        } else if show_fullscreen_controls {
            let band_height_px =
                window.macos_fullscreen_window_controls_band_height_px(padding_y_px);
            let _ = window
                .layout_macos_fullscreen_window_controls(panel_dimensions.width, band_height_px);
            tab_panel.set_fullscreen_window_controls_band_px(band_height_px);
        } else {
            window.restore_macos_window_controls();
            tab_panel.clear_window_controls();
        }

        fullscreen_top_padding
    }

    #[cfg(target_os = "macos")]
    fn sync_macos_tab_panel_semaphore_inset_for_draw(
        &mut self,
        config: &UiConfig,
        force_notch_ears: bool,
    ) {
        let scale_factor = self.window.scale_factor as f32;
        let padding = config.window.padding(scale_factor);
        let panel_dimensions = compute_panel_dimensions(
            config,
            self.size_info.cell_width(),
            self.size_info.width(),
            padding.0,
            scale_factor,
        );

        let _ = Self::sync_macos_tab_panel_semaphore_inset(
            &self.window,
            &mut self.tab_panel,
            panel_dimensions,
            padding.1,
            config,
            force_notch_ears || Self::macos_debug_notch_ears_enabled(),
        );
    }
    pub fn new(
        window: Window,
        gl_context: NotCurrentContext,
        config: &UiConfig,
        _tabbed: bool,
    ) -> Result<Display, Error> {
        let raw_window_handle = window.raw_window_handle();

        let scale_factor = window.scale_factor as f32;
        let rasterizer = Rasterizer::new()?;

        let font_size = config.font.size().scale(scale_factor);
        debug!("Loading \"{}\" font", &config.font.normal().family);
        let font = config.font.clone().with_size(font_size);
        let mut glyph_cache = GlyphCache::new(rasterizer, &font)?;

        let metrics = glyph_cache.font_metrics();
        let (cell_width, cell_height) = compute_cell_size(config, &metrics);

        // Resize the window to account for the user configured size.
        if let Some(dimensions) = config.window.dimensions() {
            let size = window_size(config, dimensions, cell_width, cell_height, scale_factor);
            window.request_inner_size(size);
        }

        // Create the GL surface to draw into.
        let surface = platform::create_gl_surface(
            &gl_context,
            window.inner_size(),
            window.raw_window_handle(),
        )?;

        // Make the context current.
        let context = gl_context.make_current(&surface)?;

        // Create renderer.
        let mut renderer = Renderer::new(&context, config.debug.renderer)?;

        // Load font common glyphs to accelerate rendering.
        debug!("Filling glyph cache with common glyphs");
        renderer.with_loader(|mut api| {
            glyph_cache.reset_glyph_cache(&mut api);
        });

        let padding = config.window.padding(window.scale_factor as f32);
        let viewport_size = window.inner_size();

        #[cfg(target_os = "macos")]
        let panel_dimensions = compute_panel_dimensions(
            config,
            cell_width,
            viewport_size.width as f32,
            padding.0,
            window.scale_factor as f32,
        );
        #[cfg(not(target_os = "macos"))]
        let panel_dimensions = PanelDimensions::default();
        #[cfg(target_os = "macos")]
        let mut tab_panel = TabPanel::new();
        #[cfg(target_os = "macos")]
        let fullscreen_top_padding = Self::sync_macos_tab_panel_semaphore_inset(
            &window,
            &mut tab_panel,
            panel_dimensions,
            padding.1,
            config,
            Self::macos_debug_notch_ears_enabled(),
        );
        #[cfg(not(target_os = "macos"))]
        let fullscreen_top_padding = 0.0;
        let panel_padding = panel_dimensions.width;
        let dynamic_padding = config.window.dynamic_padding
            && config.window.dimensions().is_none()
            && panel_dimensions.columns == 0;
        // Create new size with at least one column and row.
        let size_info = SizeInfo::new_with_vertical_padding(
            viewport_size.width as f32,
            viewport_size.height as f32,
            cell_width,
            cell_height,
            padding.0 + panel_padding,
            padding.0,
            padding.1 + fullscreen_top_padding,
            padding.1,
            dynamic_padding,
        );

        info!("Cell size: {cell_width} x {cell_height}");
        info!("Padding: {} x {}", size_info.padding_x(), size_info.padding_y());
        info!("Width: {}, Height: {}", size_info.width(), size_info.height());

        // Update OpenGL projection.
        renderer.resize(&size_info);

        // Clear screen.
        let background_color = config.colors.primary.background;
        renderer.clear(background_color, config.window_opacity());

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        window.set_has_shadow(config.window_opacity() >= 1.0);

        let is_wayland = matches!(raw_window_handle, RawWindowHandle::Wayland(_));

        // On Wayland we can safely ignore this call, since the window isn't visible until you
        // actually draw something into it and commit those changes.
        if !is_wayland {
            surface.swap_buffers(&context).expect("failed to swap buffers.");
            renderer.finish();
        }

        // Set resize increments for the newly created window.
        if config.window.resize_increments {
            window.set_resize_increments(PhysicalSize::new(cell_width, cell_height));
        }

        let hint_state = HintState::new(config.hints.alphabet());

        let mut damage_tracker = DamageTracker::new(size_info.screen_lines(), size_info.columns());
        damage_tracker.debug = config.debug.highlight_damage;
        // Show only after startup layout is complete, so macOS controls do not visibly jump.
        window.set_visible(true);

        // Always focus new windows, even if no Tabor window is currently focused.
        #[cfg(target_os = "macos")]
        window.focus_window();

        #[allow(clippy::single_match)]
        #[cfg(not(windows))]
        if !_tabbed {
            match config.window.startup_mode {
                #[cfg(target_os = "macos")]
                StartupMode::Fullscreen => window.set_preferred_fullscreen(true),
                #[cfg(target_os = "macos")]
                StartupMode::SimpleFullscreen => window.set_simple_fullscreen(true),
                StartupMode::Maximized if !is_wayland => window.set_maximized(true),
                _ => (),
            }
        }

        // Disable vsync.
        if let Err(err) = surface.set_swap_interval(&context, SwapInterval::DontWait) {
            info!("Failed to disable vsync: {err}");
        }

        Ok(Self {
            context: ManuallyDrop::new(context),
            visual_bell: VisualBell::from(&config.bell),
            renderer: ManuallyDrop::new(renderer),
            renderer_preference: config.debug.renderer,
            surface: ManuallyDrop::new(surface),
            colors: List::from(&config.colors),
            frame_timer: FrameTimer::new(),
            raw_window_handle,
            damage_tracker,
            #[cfg(target_os = "macos")]
            tab_panel,
            glyph_cache,
            hint_state,
            size_info,
            terminal_viewport: TerminalViewportLayout::normal(size_info),
            active_status_lines: 0,
            font_size,
            window,
            pending_renderer_update: Default::default(),
            vi_highlighted_hint_age: Default::default(),
            highlighted_hint_age: Default::default(),
            vi_highlighted_hint: Default::default(),
            highlighted_hint: Default::default(),
            hint_mouse_point: Default::default(),
            pending_update: Default::default(),
            cursor_hidden: Default::default(),
            meter: Default::default(),
            ime: Default::default(),
        })
    }

    pub fn terminal_viewport(&self) -> TerminalViewportLayout {
        self.terminal_viewport
    }

    pub fn size_info_for_status_lines(&self, status_lines: usize) -> SizeInfo {
        let mut size_info = self.size_info;
        match status_lines.cmp(&self.active_status_lines) {
            cmp::Ordering::Less => {
                size_info.screen_lines += self.active_status_lines - status_lines;
            },
            cmp::Ordering::Greater => {
                size_info.reserve_lines(status_lines - self.active_status_lines);
            },
            cmp::Ordering::Equal => (),
        }
        size_info
    }

    pub fn ear_aware_top_regions(&self, size_info: &SizeInfo) -> Option<EarAwareTopRegions> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = size_info;
            None
        }

        #[cfg(target_os = "macos")]
        {
            let state = self.window.macos_window_debug_state()?;
            if !state.real_ear_fullscreen_active {
                return None;
            }

            let reclaim_top_px =
                (size_info.padding_y() - size_info.padding_bottom()).max(0.) as usize;
            if reclaim_top_px == 0 {
                return None;
            }

            let scale_factor = state.scale_factor.max(f64::MIN_POSITIVE);
            let content_left = state.content_frame_screen_points.x * scale_factor;
            let content_width = state.content_frame_screen_points.width.max(0.0) * scale_factor;
            let left = auxiliary_top_region_in_content_space(
                content_left,
                content_width,
                state.auxiliary_top_left_screen_points,
                scale_factor,
            );
            let right = auxiliary_top_region_in_content_space(
                content_left,
                content_width,
                state.auxiliary_top_right_screen_points,
                scale_factor,
            );

            (left.is_some() || right.is_some()).then_some(EarAwareTopRegions {
                reclaim_top_px,
                left,
                right,
            })
        }
    }

    #[inline]
    pub fn gl_context(&self) -> &PossiblyCurrentContext {
        &self.context
    }

    pub fn make_not_current(&mut self) {
        if self.context.is_current() {
            self.context.make_not_current_in_place().expect("failed to disable context");
        }
    }

    pub fn make_current(&mut self) {
        let is_current = self.context.is_current();

        // Attempt to make the context current if it's not.
        let context_loss = if is_current {
            self.renderer.was_context_reset()
        } else {
            match self.context.make_current(&self.surface) {
                Err(err) if err.error_kind() == ErrorKind::ContextLost => {
                    info!("Context lost for window {:?}", self.window.id());
                    true
                },
                _ => false,
            }
        };

        if !context_loss {
            return;
        }

        let gl_display = self.context.display();
        let gl_config = self.context.config();
        let raw_window_handle = Some(self.window.raw_window_handle());
        let context = platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)
            .expect("failed to recreate context.");

        // Drop the old context and renderer.
        unsafe {
            ManuallyDrop::drop(&mut self.renderer);
            ManuallyDrop::drop(&mut self.context);
        }

        // Activate new context.
        let context = context.treat_as_possibly_current();
        self.context = ManuallyDrop::new(context);
        self.context.make_current(&self.surface).expect("failed to reativate context after reset.");

        // Recreate renderer.
        let renderer = Renderer::new(&self.context, self.renderer_preference)
            .expect("failed to recreate renderer after reset");
        self.renderer = ManuallyDrop::new(renderer);

        // Resize the renderer.
        self.renderer.resize(&self.size_info);

        self.reset_glyph_cache();
        self.damage_tracker.frame().mark_fully_damaged();

        debug!("Recovered window {:?} from gpu reset", self.window.id());
    }

    fn swap_buffers(&self) {
        #[allow(clippy::single_match)]
        let res = match (self.surface.deref(), &self.context.deref()) {
            #[cfg(not(any(target_os = "macos", windows)))]
            (Surface::Egl(surface), PossiblyCurrentContext::Egl(context))
                if matches!(self.raw_window_handle, RawWindowHandle::Wayland(_))
                    && !self.damage_tracker.debug =>
            {
                let damage = self.damage_tracker.shape_frame_damage(self.size_info.into());
                surface.swap_buffers_with_damage(context, &damage)
            },
            (surface, context) => surface.swap_buffers(context),
        };
        if let Err(err) = res {
            debug!("error calling swap_buffers: {err}");
        }
    }

    /// Update font size and cell dimensions.
    ///
    /// This will return a tuple of the cell width and height.
    fn update_font_size(
        glyph_cache: &mut GlyphCache,
        config: &UiConfig,
        font: &Font,
    ) -> (f32, f32) {
        let _ = glyph_cache.update_font_size(font);

        // Compute new cell sizes.
        compute_cell_size(config, &glyph_cache.font_metrics())
    }

    /// Reset glyph cache.
    fn reset_glyph_cache(&mut self) {
        let cache = &mut self.glyph_cache;
        self.renderer.with_loader(|mut api| {
            cache.reset_glyph_cache(&mut api);
        });
    }

    // XXX: this function must not call to any `OpenGL` related tasks. Renderer updates are
    // performed in [`Self::process_renderer_update`] right before drawing.
    //
    /// Process update events.
    pub fn handle_update<T>(
        &mut self,
        terminal: &mut Term<T>,
        pty_resize_handle: &mut dyn OnResize,
        search_state: &mut SearchState,
        options: DisplayUpdateOptions,
        config: &UiConfig,
    ) where
        T: EventListener,
    {
        let pending_update = mem::take(&mut self.pending_update);

        let (mut cell_width, mut cell_height) =
            (self.size_info.cell_width(), self.size_info.cell_height());

        if pending_update.font().is_some() || pending_update.cursor_dirty() {
            let renderer_update = self.pending_renderer_update.get_or_insert(Default::default());
            renderer_update.clear_font_cache = true
        }

        // Update font size and cell dimensions.
        if let Some(font) = pending_update.font() {
            let cell_dimensions = Self::update_font_size(&mut self.glyph_cache, config, font);
            cell_width = cell_dimensions.0;
            cell_height = cell_dimensions.1;

            info!("Cell size: {cell_width} x {cell_height}");

            // Mark entire terminal as damaged since glyph size could change without cell size
            // changes.
            self.damage_tracker.frame().mark_fully_damaged();
        }

        let (mut width, mut height) = (self.size_info.width(), self.size_info.height());
        if let Some(dimensions) = pending_update.dimensions() {
            width = dimensions.width as f32;
            height = dimensions.height as f32;
        }

        let padding = config.window.padding(self.window.scale_factor as f32);

        #[cfg(target_os = "macos")]
        let panel_dimensions = compute_panel_dimensions(
            config,
            cell_width,
            width,
            padding.0,
            self.window.scale_factor as f32,
        );
        #[cfg(not(target_os = "macos"))]
        let panel_dimensions = PanelDimensions::default();
        #[cfg(target_os = "macos")]
        let fullscreen_top_padding = Self::sync_macos_tab_panel_semaphore_inset(
            &self.window,
            &mut self.tab_panel,
            panel_dimensions,
            padding.1,
            config,
            Self::macos_debug_notch_ears_enabled(),
        );
        #[cfg(not(target_os = "macos"))]
        let fullscreen_top_padding = 0.0;
        let panel_padding = panel_dimensions.width;
        let dynamic_padding = config.window.dynamic_padding && panel_dimensions.columns == 0;
        let mut new_size = SizeInfo::new_with_vertical_padding(
            width,
            height,
            cell_width,
            cell_height,
            padding.0 + panel_padding,
            padding.0,
            padding.1 + fullscreen_top_padding,
            padding.1,
            dynamic_padding,
        );

        // Update number of column/lines in the viewport.
        //
        // Message bar is rendered inline with the footer bar, so it doesn't reserve extra lines.
        let status_lines = usize::from(options.web_status_bar);
        new_size.reserve_lines(status_lines);
        self.active_status_lines = status_lines;

        // Update resize increments.
        if config.window.resize_increments {
            self.window.set_resize_increments(PhysicalSize::new(cell_width, cell_height));
        }

        self.terminal_viewport = options.terminal_view_mode.map_or_else(
            || TerminalViewportLayout::normal(new_size),
            |mode| {
                TerminalViewportLayout::new(
                    &new_size,
                    mode,
                    &config.terminal.multi_column,
                    options.exact_multi_column_count,
                    self.ear_aware_top_regions(&new_size),
                )
            },
        );
        let logical_size = self.terminal_viewport.logical_size(&new_size);

        // Resize the active terminal when its logical dimensions have changed.
        if terminal.screen_lines() != logical_size.screen_lines()
            || terminal.columns() != logical_size.columns()
        {
            // Resize PTY.
            pty_resize_handle.on_resize(logical_size.into());

            // Resize terminal.
            terminal.resize_with_anchor(logical_size, ResizeAnchor::Top);
        }

        if self.size_info.screen_lines() != new_size.screen_lines
            || self.size_info.columns() != new_size.columns()
        {
            self.damage_tracker.resize(new_size.screen_lines(), new_size.columns());
        }

        // Check if dimensions have changed.
        if new_size != self.size_info {
            // Queue renderer update.
            let renderer_update = self.pending_renderer_update.get_or_insert(Default::default());
            renderer_update.resize = true;

            // Clear focused search match.
            search_state.clear_focused_match();
        }
        self.size_info = new_size;
    }

    // NOTE: Renderer updates are split off, since platforms like Wayland require resize and other
    // OpenGL operations to be performed right before rendering. Otherwise they could lock the
    // back buffer and render with the previous state. This also solves flickering during resizes.
    //
    /// Update the state of the renderer.
    pub fn process_renderer_update(&mut self) {
        let renderer_update = match self.pending_renderer_update.take() {
            Some(renderer_update) => renderer_update,
            _ => return,
        };

        // Resize renderer.
        if renderer_update.resize {
            let width = NonZeroU32::new(self.size_info.width() as u32).unwrap();
            let height = NonZeroU32::new(self.size_info.height() as u32).unwrap();
            self.surface.resize(&self.context, width, height);
        }

        // Ensure we're modifying the correct OpenGL context.
        self.make_current();

        if renderer_update.clear_font_cache {
            self.reset_glyph_cache();
        }

        self.renderer.resize(&self.size_info);

        info!("Padding: {} x {}", self.size_info.padding_x(), self.size_info.padding_y());
        info!("Width: {}, Height: {}", self.size_info.width(), self.size_info.height());
    }

    #[cfg(target_os = "macos")]
    pub fn window_snapshot_rgba(&mut self) -> (u32, u32, Vec<u8>) {
        self.make_current();
        let width = self.size_info.width() as u32;
        let height = self.size_info.height() as u32;
        let pixels = self.renderer.read_front_buffer_rgba(width, height);
        (width, height, pixels)
    }

    /// Draw the screen.
    ///
    /// A reference to Term whose state is being drawn must be provided.
    ///
    /// This call may block if vsync is enabled.
    #[allow(clippy::too_many_arguments)]
    pub fn draw<T: EventListener>(
        &mut self,
        mut terminal: MutexGuard<'_, Term<T>>,
        scheduler: &mut Scheduler,
        message_buffer: &MessageBuffer,
        config: &UiConfig,
        search_state: &mut SearchState,
        command_state: &CommandState,
        command_footer_message: Option<&CommandFooterMessage>,
        #[cfg(target_os = "macos")] force_notch_ears: bool,
    ) {
        #[cfg(target_os = "macos")]
        self.sync_macos_tab_panel_semaphore_inset_for_draw(config, force_notch_ears);

        let terminal_viewport = self.terminal_viewport.with_terminal_content(&terminal);

        let cursor_point = terminal.grid().cursor.point;
        let total_lines = terminal.grid().total_lines();
        let metrics = self.glyph_cache.font_metrics();
        let size_info = self.size_info;
        let command_active = command_state.is_active();
        let vi_mode = terminal.mode().contains(TermMode::VI);
        let search_active = search_state.regex().is_some();
        let command_feedback_active = command_footer_message.is_some();
        let footer_bar_mode = terminal_footer_bar_mode(
            command_active,
            search_active,
            command_feedback_active,
            vi_mode,
        );
        let message_visible =
            message_buffer.message().is_some() && footer_bar_mode == TerminalFooterBarMode::None;
        let vi_cursor_point = if vi_mode { Some(terminal.vi_mode_cursor.point) } else { None };

        let folded_terminal = terminal_viewport.is_multi_column();

        // Add damage from the terminal.
        if folded_terminal {
            self.damage_tracker.frame().mark_fully_damaged();
            self.damage_tracker.next_frame().mark_fully_damaged();
        } else {
            match terminal.damage() {
                TermDamage::Full => self.damage_tracker.frame().mark_fully_damaged(),
                TermDamage::Partial(damaged_lines) => {
                    for damage in damaged_lines {
                        self.damage_tracker.frame().damage_line(damage);
                    }
                },
            }
        }
        terminal.reset_damage();
        let search =
            search_state.dfas().map(|dfas| HintMatches::visible_regex_matches(&terminal, dfas));
        let focused_match = search_state.focused_match();

        let mut content = RenderableContent::new(
            config,
            &mut self.hint_state,
            &terminal,
            RenderableContentContext {
                colors: self.colors,
                size: size_info,
                layout: terminal_viewport,
                search,
                focused_match,
                cursor_hidden: self.cursor_hidden,
                ime_preedit_active: self.ime.preedit().is_some(),
            },
        );
        let selection_range = content.selection_range();
        let foreground_color = content.color(NamedColor::Foreground as usize);
        let background_color = content.color(NamedColor::Background as usize);
        let display_offset = content.display_offset();

        // Invalidate highlighted hints if grid has changed.
        self.validate_hint_highlights(display_offset);

        // Add damage from tabor's UI elements overlapping terminal.

        let requires_full_damage = folded_terminal
            || self.visual_bell.intensity() != 0.
            || self.hint_state.active()
            || search_active
            || command_active
            || command_feedback_active;
        if requires_full_damage {
            self.damage_tracker.frame().mark_fully_damaged();
            self.damage_tracker.next_frame().mark_fully_damaged();
        }

        let vi_cursor_viewport_point = vi_cursor_point
            .and_then(|cursor| term::point_to_viewport(display_offset, cursor))
            .and_then(|point| terminal_viewport.visual_point_for_logical_viewport(point));
        let vi_line_indicator = if footer_bar_mode == TerminalFooterBarMode::CommandFeedback {
            None
        } else {
            vi_cursor_point.map(|cursor| {
                line_indicator_text(
                    vi_mode_line_indicator_line(terminal_viewport, &size_info, cursor),
                    total_lines,
                )
            })
        };
        let footer_text_max_width =
            footer_text_max_width(size_info.columns(), vi_line_indicator.as_deref());
        self.damage_tracker.damage_vi_cursor(vi_cursor_viewport_point);
        if !folded_terminal {
            self.damage_tracker.damage_selection(selection_range, display_offset);
        }

        // Make sure this window's OpenGL context is active.
        self.make_current();

        self.renderer.clear(background_color, config.window_opacity());
        let mut lines = RenderLines::new();

        // Optimize loop hint comparator.
        let has_highlighted_hint =
            self.highlighted_hint.is_some() || self.vi_highlighted_hint.is_some();

        // Draw grid.
        {
            let _sampler = self.meter.sampler();

            // Ensure macOS hasn't reset our viewport.
            #[cfg(target_os = "macos")]
            self.renderer.set_viewport(&size_info);

            let glyph_cache = &mut self.glyph_cache;
            let highlighted_hint = &self.highlighted_hint;
            let vi_highlighted_hint = &self.vi_highlighted_hint;
            let damage_tracker = &mut self.damage_tracker;
            let needs_ear_aware_cell_viewport =
                terminal_viewport.strip_geometries().iter().any(|strip| strip.y_offset_px > 0);
            let mut cell_render_size = size_info;
            let cell_projection_offset = if needs_ear_aware_cell_viewport {
                cell_render_size.padding_y = 0.;
                (0., size_info.padding_y())
            } else {
                (0., 0.)
            };

            let cells = content.by_ref().map(|mut cell| {
                // Underline hints hovered by mouse or vi mode cursor.
                if has_highlighted_hint {
                    let point = cell.logical_point;
                    let hyperlink = cell.extra.as_ref().and_then(|extra| extra.hyperlink.as_ref());

                    let should_highlight = |hint: &Option<HintMatch>| {
                        hint.as_ref().is_some_and(|hint| hint.should_highlight(point, hyperlink))
                    };
                    if should_highlight(highlighted_hint) || should_highlight(vi_highlighted_hint) {
                        damage_tracker.frame().damage_point(cell.point);
                        cell.flags.insert(Flags::UNDERLINE);
                    }
                }

                // Update underline/strikeout.
                lines.update(&cell);

                cell
            });
            #[cfg(target_os = "macos")]
            if needs_ear_aware_cell_viewport {
                self.renderer.set_viewport(&cell_render_size);
            }
            self.renderer
                .set_text_projection_with_offset(&cell_render_size, cell_projection_offset);
            self.renderer.draw_cells(&cell_render_size, glyph_cache, cells);
            if needs_ear_aware_cell_viewport {
                #[cfg(target_os = "macos")]
                self.renderer.set_viewport(&size_info);
                self.renderer.set_text_projection(&size_info);
            }
        }
        let cursor = content.cursor();

        // Drop terminal as early as possible to free lock.
        drop(terminal);

        let mut rects = lines.rects(&metrics, &size_info);

        if vi_mode {
            // Vi mode reuses the footer bar as its status indicator.
        } else if search_active {
            // Show current display offset in vi-less search to indicate match position.
            self.draw_line_indicator(config, total_lines, None, display_offset);
        };

        // Draw cursor.
        rects.extend(cursor.rects(&size_info, config.cursor.thickness()));

        #[cfg(target_os = "macos")]
        if self.tab_panel.is_enabled() {
            self.tab_panel.push_rects(&size_info, config, &mut rects);
            self.damage_tracker.frame().add_viewport_rect(
                &size_info,
                0,
                0,
                self.tab_panel.width().round() as i32,
                size_info.height() as i32,
            );
        }

        // Push visual bell after url/underline/strikeout rects.
        let visual_bell_intensity = self.visual_bell.intensity();
        if visual_bell_intensity != 0. {
            let visual_bell_rect = RenderRect::new(
                0.,
                0.,
                size_info.width(),
                size_info.height(),
                config.bell.color,
                visual_bell_intensity as f32,
            );
            rects.push(visual_bell_rect);
        }

        // Handle IME positioning and command/search bar rendering.
        let footer_offset =
            if footer_bar_mode == TerminalFooterBarMode::None { 0. } else { self.footer_offset() };

        let ime_position = match footer_bar_mode {
            TerminalFooterBarMode::Command => {
                let command_text =
                    Self::format_command(command_state.text(), footer_text_max_width);

                self.draw_command_bar(config, &command_text, footer_offset);

                let line = size_info.screen_lines().saturating_sub(1);
                let column = Column(command_text.chars().count() - 1);

                if self.ime.preedit().is_none() {
                    let fg = config.colors.footer_bar_foreground();
                    let shape = CursorShape::Underline;
                    let cursor_width = NonZeroU32::new(1).unwrap();
                    let cursor =
                        RenderableCursor::new(Point::new(line, column), shape, fg, cursor_width);
                    let mut cursor_rects: Vec<_> =
                        cursor.rects(&size_info, config.cursor.thickness()).collect();
                    for rect in &mut cursor_rects {
                        rect.y += footer_offset;
                    }
                    rects.extend(cursor_rects);
                }

                Some(Point::new(line, column))
            },
            TerminalFooterBarMode::Search => {
                let regex = search_state.regex().expect("search footer mode requires regex");
                let search_label = match search_state.direction() {
                    Direction::Right => FORWARD_SEARCH_LABEL,
                    Direction::Left => BACKWARD_SEARCH_LABEL,
                };

                let search_text = Self::format_search(regex, search_label, footer_text_max_width);

                self.draw_search(config, &search_text, footer_offset);

                let line = size_info.screen_lines().saturating_sub(1);
                let column = Column(search_text.chars().count() - 1);

                if self.ime.preedit().is_none() {
                    let fg = config.colors.footer_bar_foreground();
                    let shape = CursorShape::Underline;
                    let cursor_width = NonZeroU32::new(1).unwrap();
                    let cursor =
                        RenderableCursor::new(Point::new(line, column), shape, fg, cursor_width);
                    let mut cursor_rects: Vec<_> =
                        cursor.rects(&size_info, config.cursor.thickness()).collect();
                    for rect in &mut cursor_rects {
                        rect.y += footer_offset;
                    }
                    rects.extend(cursor_rects);
                }

                Some(Point::new(line, column))
            },
            TerminalFooterBarMode::CommandFeedback => {
                let message =
                    command_footer_message.expect("command feedback footer mode requires message");
                self.draw_command_feedback(config, message, footer_offset);
                None
            },
            TerminalFooterBarMode::ViIndicator => {
                self.draw_command_bar(config, "", footer_offset);
                None
            },
            TerminalFooterBarMode::None => match vi_cursor_viewport_point {
                None => term::point_to_viewport(display_offset, cursor_point)
                    .and_then(|point| terminal_viewport.visual_point_for_logical_viewport(point)),
                point => point,
            },
        };

        // Handle IME.
        if self.ime.is_enabled() {
            if let Some(point) = ime_position {
                let (fg, bg) = if footer_bar_mode == TerminalFooterBarMode::None {
                    (foreground_color, background_color)
                } else {
                    (config.colors.footer_bar_foreground(), config.colors.footer_bar_background())
                };

                self.draw_ime_preview(
                    point,
                    fg,
                    bg,
                    &mut rects,
                    config,
                    (
                        footer_offset,
                        terminal_viewport.y_offset_px_for_visual_column(point.column.0) as f32,
                    ),
                );
            }
        }

        if let Some(indicator_text) = vi_line_indicator.as_deref() {
            self.draw_footer_line_indicator(config, indicator_text, footer_offset);
        }

        // Draw rectangles.
        self.renderer.draw_rects(&size_info, &metrics, rects);

        #[cfg(target_os = "macos")]
        self.tab_panel.draw_text(&size_info, config, &mut self.renderer, &mut self.glyph_cache);

        if message_visible {
            if let Some(message) = message_buffer.message() {
                let message_text = message.text(&size_info).into_iter().next().unwrap_or_default();
                let fg = config.colors.primary.background;
                let bg = match message.ty() {
                    MessageType::Error => config.colors.normal.red,
                    MessageType::Warning => config.colors.normal.yellow,
                };
                let line = size_info.screen_lines().saturating_sub(1);
                let message_offset = self.footer_offset();
                let band = FooterBarViewportBand::new(
                    &size_info,
                    line,
                    message_offset,
                    size_info.cell_height(),
                );
                let (x, y, width, height) = band.damage_rect();
                self.damage_tracker.frame().add_viewport_rect(&size_info, x, y, width, height);
                self.damage_tracker.next_frame().add_viewport_rect(&size_info, x, y, width, height);

                self.draw_footer_bar_line(&message_text, fg, bg, line, message_offset);
            }
        }

        self.draw_render_timer(config);

        // Draw hyperlink uri preview.
        if has_highlighted_hint {
            let cursor_point = vi_cursor_point.or(Some(cursor_point));
            self.draw_hyperlink_preview(config, terminal_viewport, cursor_point, display_offset);
        }

        // Notify winit that we're about to present.
        self.window.pre_present_notify();

        // Highlight damage for debugging.
        if self.damage_tracker.debug {
            let damage = self.damage_tracker.shape_frame_damage(self.size_info.into());
            let mut rects = Vec::with_capacity(damage.len());
            self.highlight_damage(&mut rects);
            self.renderer.draw_rects(&self.size_info, &metrics, rects);
        }

        // Clearing debug highlights from the previous frame requires full redraw.
        self.swap_buffers();

        if matches!(self.raw_window_handle, RawWindowHandle::Xcb(_) | RawWindowHandle::Xlib(_)) {
            // On X11 `swap_buffers` does not block for vsync. However the next OpenGl command
            // will block to synchronize (this is `glClear` in Tabor), which causes a
            // permanent one frame delay.
            self.renderer.finish();
        }

        // XXX: Request the new frame after swapping buffers, so the
        // time to finish OpenGL operations is accounted for in the timeout.
        if !matches!(self.raw_window_handle, RawWindowHandle::Wayland(_)) {
            self.request_frame(scheduler);
        }

        self.damage_tracker.swap_damage();
    }

    pub fn draw_web(
        &mut self,
        scheduler: &mut Scheduler,
        message_buffer: &MessageBuffer,
        config: &UiConfig,
        url: &str,
        command_state: &CommandState,
        #[cfg(target_os = "macos")] macos: MacOsWebDraw<'_>,
    ) {
        #[cfg(target_os = "macos")]
        self.sync_macos_tab_panel_semaphore_inset_for_draw(config, macos.force_notch_ears);

        let size_info = self.size_info;
        let metrics = self.glyph_cache.font_metrics();
        let background_color = config.colors.primary.background;
        let command_active = command_state.is_active();
        let message_visible = message_buffer.message().is_some() && !command_active;

        self.damage_tracker.frame().mark_fully_damaged();

        self.make_current();
        self.renderer.clear(background_color, config.window_opacity());

        #[cfg(target_os = "macos")]
        self.renderer.set_viewport(&size_info);

        #[cfg(target_os = "macos")]
        if let Some(web_view) = macos.web_view {
            web_view.with_surfaces(|surface, popup| {
                if let Some(surface) = surface {
                    let slices = browser_main_image_slices(
                        &macos.browser_layout,
                        self.window.scale_factor,
                        surface.width,
                        surface.height,
                    );
                    self.renderer.draw_web_surface_slices(
                        &size_info,
                        SurfaceSlot::Main,
                        surface.io_surface,
                        surface.width,
                        surface.height,
                        surface.format,
                        &slices,
                    );
                }

                if let Some(popup) = popup {
                    let slices = browser_popup_image_slices(
                        &macos.browser_layout,
                        self.window.scale_factor,
                        popup,
                    );
                    self.renderer.draw_web_surface_slices(
                        &size_info,
                        SurfaceSlot::Popup,
                        popup.surface.io_surface,
                        popup.surface.width,
                        popup.surface.height,
                        popup.surface.format,
                        &slices,
                    );
                }
            });
        }

        let mut rects = Vec::new();

        #[cfg(target_os = "macos")]
        if self.tab_panel.is_enabled() {
            self.tab_panel.push_rects(&size_info, config, &mut rects);
            self.damage_tracker.frame().add_viewport_rect(
                &size_info,
                0,
                0,
                self.tab_panel.width().round() as i32,
                size_info.height() as i32,
            );
        }

        let footer_offset = self.footer_offset();

        let ime_position = if command_active {
            let command_text = Self::format_command(command_state.text(), size_info.columns());
            self.draw_command_bar(config, &command_text, footer_offset);

            let line = size_info.screen_lines().saturating_sub(1);
            let column = Column(command_text.chars().count().saturating_sub(1));

            if self.ime.preedit().is_none() {
                let fg = config.colors.footer_bar_foreground();
                let shape = CursorShape::Underline;
                let cursor_width = NonZeroU32::new(1).unwrap();
                let cursor =
                    RenderableCursor::new(Point::new(line, column), shape, fg, cursor_width);
                let mut cursor_rects: Vec<_> =
                    cursor.rects(&size_info, config.cursor.thickness()).collect();
                for rect in &mut cursor_rects {
                    rect.y += footer_offset;
                }
                rects.extend(cursor_rects);
            }

            Some(Point::new(line, column))
        } else {
            None
        };

        if self.ime.is_enabled() {
            if let Some(point) = ime_position {
                let fg = config.colors.footer_bar_foreground();
                let bg = config.colors.footer_bar_background();
                self.draw_ime_preview(point, fg, bg, &mut rects, config, (footer_offset, 0.));
            }
        }

        self.renderer.draw_rects(&size_info, &metrics, rects);

        #[cfg(target_os = "macos")]
        self.tab_panel.draw_text(&size_info, config, &mut self.renderer, &mut self.glyph_cache);

        if message_visible {
            if let Some(message) = message_buffer.message() {
                let message_text = message.text(&size_info).into_iter().next().unwrap_or_default();
                let fg = config.colors.primary.background;
                let bg = match message.ty() {
                    MessageType::Error => config.colors.normal.red,
                    MessageType::Warning => config.colors.normal.yellow,
                };
                let line = size_info.screen_lines().saturating_sub(1);
                let band = FooterBarViewportBand::new(
                    &size_info,
                    line,
                    footer_offset,
                    size_info.cell_height(),
                );
                let (x, y, width, height) = band.damage_rect();
                self.damage_tracker.frame().add_viewport_rect(&size_info, x, y, width, height);
                self.damage_tracker.next_frame().add_viewport_rect(&size_info, x, y, width, height);

                self.draw_footer_bar_line(&message_text, fg, bg, line, footer_offset);
            }
        } else if !command_active {
            let url_text: String = StrShortener::new(
                url,
                size_info.columns(),
                ShortenDirection::Right,
                Some(SHORTENER),
            )
            .collect();
            let fg = config.colors.footer_bar_foreground();
            let bg = config.colors.footer_bar_background();
            let line = size_info.screen_lines().saturating_sub(1);
            self.draw_footer_bar_line(&url_text, fg, bg, line, footer_offset);
        }

        self.draw_render_timer(config);

        self.window.pre_present_notify();

        if self.damage_tracker.debug {
            let damage = self.damage_tracker.shape_frame_damage(self.size_info.into());
            let mut rects = Vec::with_capacity(damage.len());
            self.highlight_damage(&mut rects);
            self.renderer.draw_rects(&self.size_info, &metrics, rects);
        }

        self.swap_buffers();

        if matches!(self.raw_window_handle, RawWindowHandle::Xcb(_) | RawWindowHandle::Xlib(_)) {
            self.renderer.finish();
        }

        if !matches!(self.raw_window_handle, RawWindowHandle::Wayland(_)) {
            self.request_frame(scheduler);
        }

        self.damage_tracker.swap_damage();
    }

    #[cfg(target_os = "macos")]
    pub fn draw_image(
        &mut self,
        scheduler: &mut Scheduler,
        message_buffer: &MessageBuffer,
        config: &UiConfig,
        image_view: &ImageViewState,
        command_state: &CommandState,
        force_notch_ears: bool,
    ) {
        self.sync_macos_tab_panel_semaphore_inset_for_draw(config, force_notch_ears);

        let size_info = self.size_info;
        let metrics = self.glyph_cache.font_metrics();
        let background_color = config.colors.primary.background;
        let command_active = command_state.is_active();
        let message_visible = message_buffer.message().is_some() && !command_active;

        self.damage_tracker.frame().mark_fully_damaged();

        self.make_current();
        self.renderer.clear(background_color, config.window_opacity());
        self.renderer.set_viewport(&size_info);

        if let (Some(bitmap), Some(quad)) = (
            image_view.bitmap(),
            image_view.render_quad(PhysicalSize::new(
                size_info.width() as u32,
                size_info.height() as u32,
            )),
        ) {
            self.renderer.draw_image_bitmap(
                &size_info,
                bitmap.width as usize,
                bitmap.height as usize,
                &bitmap.rgba,
                quad,
            );
        }

        let mut rects = Vec::new();

        if self.tab_panel.is_enabled() {
            self.tab_panel.push_rects(&size_info, config, &mut rects);
            self.damage_tracker.frame().add_viewport_rect(
                &size_info,
                0,
                0,
                self.tab_panel.width().round() as i32,
                size_info.height() as i32,
            );
        }

        let footer_offset = self.footer_offset();

        let ime_position = if command_active {
            let command_text = Self::format_command(command_state.text(), size_info.columns());
            self.draw_command_bar(config, &command_text, footer_offset);

            let line = size_info.screen_lines().saturating_sub(1);
            let column = Column(command_text.chars().count().saturating_sub(1));

            if self.ime.preedit().is_none() {
                let fg = config.colors.footer_bar_foreground();
                let shape = CursorShape::Underline;
                let cursor_width = NonZeroU32::new(1).unwrap();
                let cursor =
                    RenderableCursor::new(Point::new(line, column), shape, fg, cursor_width);
                let mut cursor_rects: Vec<_> =
                    cursor.rects(&size_info, config.cursor.thickness()).collect();
                for rect in &mut cursor_rects {
                    rect.y += footer_offset;
                }
                rects.extend(cursor_rects);
            }

            Some(Point::new(line, column))
        } else {
            None
        };

        if self.ime.is_enabled() {
            if let Some(point) = ime_position {
                let fg = config.colors.footer_bar_foreground();
                let bg = config.colors.footer_bar_background();
                self.draw_ime_preview(point, fg, bg, &mut rects, config, (footer_offset, 0.));
            }
        }

        self.renderer.draw_rects(&size_info, &metrics, rects);
        self.tab_panel.draw_text(&size_info, config, &mut self.renderer, &mut self.glyph_cache);

        if message_visible {
            if let Some(message) = message_buffer.message() {
                let message_text = message.text(&size_info).into_iter().next().unwrap_or_default();
                let fg = config.colors.primary.background;
                let bg = match message.ty() {
                    MessageType::Error => config.colors.normal.red,
                    MessageType::Warning => config.colors.normal.yellow,
                };
                let line = size_info.screen_lines().saturating_sub(1);
                let band = FooterBarViewportBand::new(
                    &size_info,
                    line,
                    footer_offset,
                    size_info.cell_height(),
                );
                let (x, y, width, height) = band.damage_rect();
                self.damage_tracker.frame().add_viewport_rect(&size_info, x, y, width, height);
                self.damage_tracker.next_frame().add_viewport_rect(&size_info, x, y, width, height);
                self.draw_footer_bar_line(&message_text, fg, bg, line, footer_offset);
            }
        } else if !command_active {
            let source_text: String = StrShortener::new(
                &image_view.source,
                size_info.columns(),
                ShortenDirection::Right,
                Some(SHORTENER),
            )
            .collect();
            let fg = config.colors.footer_bar_foreground();
            let bg = config.colors.footer_bar_background();
            let line = size_info.screen_lines().saturating_sub(1);
            self.draw_footer_bar_line(&source_text, fg, bg, line, footer_offset);
        }

        self.draw_render_timer(config);
        self.window.pre_present_notify();

        if self.damage_tracker.debug {
            let damage = self.damage_tracker.shape_frame_damage(self.size_info.into());
            let mut rects = Vec::with_capacity(damage.len());
            self.highlight_damage(&mut rects);
            self.renderer.draw_rects(&self.size_info, &metrics, rects);
        }

        self.swap_buffers();

        if matches!(self.raw_window_handle, RawWindowHandle::Xcb(_) | RawWindowHandle::Xlib(_)) {
            self.renderer.finish();
        }

        if !matches!(self.raw_window_handle, RawWindowHandle::Wayland(_)) {
            self.request_frame(scheduler);
        }

        self.damage_tracker.swap_damage();
    }

    /// Update to a new configuration.
    pub fn update_config(&mut self, config: &UiConfig) {
        self.damage_tracker.debug = config.debug.highlight_damage;
        self.visual_bell.update_config(&config.bell);
        self.colors = List::from(&config.colors);
        #[cfg(target_os = "macos")]
        self.tab_panel.set_enabled(config.window.tab_panel.enabled);
    }

    #[cfg(target_os = "macos")]
    pub fn set_tab_panel_groups(
        &mut self,
        groups: Vec<crate::tab_panel::TabPanelGroup>,
        new_group_id: Option<usize>,
    ) -> bool {
        self.tab_panel.set_groups(groups, new_group_id)
    }

    /// Update the mouse/vi mode cursor hint highlighting.
    ///
    /// This will return whether the highlighted hints changed.
    pub fn update_highlighted_hints<T>(
        &mut self,
        term: &Term<T>,
        config: &UiConfig,
        mouse: &Mouse,
        modifiers: ModifiersState,
    ) -> bool {
        let terminal_viewport = self.terminal_viewport.with_terminal_content(term);

        // Update vi mode cursor hint.
        let vi_highlighted_hint = if term.mode().contains(TermMode::VI) {
            let mods = ModifiersState::all();
            let point = term.vi_mode_cursor.point;
            hint::highlighted_at(term, config, point, mods)
        } else {
            None
        };
        let mut dirty = vi_highlighted_hint != self.vi_highlighted_hint;
        self.vi_highlighted_hint = vi_highlighted_hint;
        self.vi_highlighted_hint_age = 0;

        // Force full redraw if the vi mode highlight was cleared.
        if dirty {
            self.damage_tracker.frame().mark_fully_damaged();
        }

        // Abort if mouse highlighting conditions are not met.
        if !self.window.mouse_visible()
            || !mouse.inside_text_area
            || !term.selection.as_ref().is_none_or(Selection::is_empty)
        {
            if self.highlighted_hint.take().is_some() {
                self.damage_tracker.frame().mark_fully_damaged();
                dirty = true;
            }
            return dirty;
        }

        // Find highlighted hint at mouse position.
        let point = mouse.point(&self.size_info, &terminal_viewport, term.grid().display_offset());
        let highlighted_hint = hint::highlighted_at(term, config, point, modifiers);

        // Update cursor shape.
        if highlighted_hint.is_some() {
            // If mouse changed the line, we should update the hyperlink preview, since the
            // highlighted hint could be disrupted by the old preview.
            dirty = self.hint_mouse_point.is_some_and(|p| p.line != point.line);
            self.hint_mouse_point = Some(point);
            self.window.set_mouse_cursor(CursorIcon::Pointer);
        } else if self.highlighted_hint.is_some() {
            self.hint_mouse_point = None;
            if term.mode().intersects(TermMode::MOUSE_MODE) && !term.mode().contains(TermMode::VI) {
                self.window.set_mouse_cursor(CursorIcon::Default);
            } else {
                self.window.set_mouse_cursor(CursorIcon::Text);
            }
        }

        let mouse_highlight_dirty = self.highlighted_hint != highlighted_hint;
        dirty |= mouse_highlight_dirty;
        self.highlighted_hint = highlighted_hint;
        self.highlighted_hint_age = 0;

        // Force full redraw if the mouse cursor highlight was changed.
        if mouse_highlight_dirty {
            self.damage_tracker.frame().mark_fully_damaged();
        }

        dirty
    }

    #[inline(never)]
    fn draw_ime_preview(
        &mut self,
        point: Point<usize>,
        fg: Rgb,
        bg: Rgb,
        rects: &mut Vec<RenderRect>,
        config: &UiConfig,
        offsets: (f32, f32),
    ) {
        let (offset_y, y_offset_px) = offsets;
        let mut ime_size_info = self.size_info;
        let total_offset_y = offset_y - y_offset_px;
        if total_offset_y != 0. {
            ime_size_info.padding_y += total_offset_y;
        }

        let preedit = match self.ime.preedit() {
            Some(preedit) => preedit,
            None => {
                // In case we don't have preedit, just set the popup point.
                self.window.update_ime_position(point, &ime_size_info);
                return;
            },
        };

        let num_cols = self.size_info.columns();

        // Get the visible preedit.
        let visible_text: String = match (preedit.cursor_byte_offset, preedit.cursor_end_offset) {
            (Some(byte_offset), Some(end_offset)) if end_offset.0 > num_cols => StrShortener::new(
                &preedit.text[byte_offset.0..],
                num_cols,
                ShortenDirection::Right,
                Some(SHORTENER),
            ),
            _ => {
                StrShortener::new(&preedit.text, num_cols, ShortenDirection::Left, Some(SHORTENER))
            },
        }
        .collect();

        let visible_len = visible_text.chars().count();

        let end = cmp::min(point.column.0 + visible_len, num_cols);
        let start = end.saturating_sub(visible_len);

        let start = Point::new(point.line, Column(start));
        let end = Point::new(point.line, Column(end - 1));

        let glyph_cache = &mut self.glyph_cache;
        let metrics = glyph_cache.font_metrics();

        if total_offset_y != 0. {
            self.renderer.set_text_projection_with_offset(&self.size_info, (0., total_offset_y));
        }

        self.renderer.draw_string(
            start,
            fg,
            bg,
            visible_text.chars(),
            &self.size_info,
            glyph_cache,
        );

        if total_offset_y != 0. {
            self.renderer.set_text_projection(&self.size_info);
        }

        // Damage preedit inside the terminal viewport.
        if point.line < self.size_info.screen_lines() {
            let damage = LineDamageBounds::new(start.line, 0, num_cols);
            self.damage_tracker.frame().damage_line(damage);
            self.damage_tracker.next_frame().damage_line(damage);
        }

        // Add underline for preedit text.
        let underline = RenderLine { start, end, y_offset_px: 0, color: fg };
        let mut underline_rects = underline.rects(Flags::UNDERLINE, &metrics, &self.size_info);
        for rect in &mut underline_rects {
            rect.y += total_offset_y;
        }
        rects.extend(underline_rects);

        let ime_popup_point = match preedit.cursor_end_offset {
            Some(cursor_end_offset) => {
                // Use hollow block when multiple characters are changed at once.
                let (shape, width) = if let Some(width) =
                    NonZeroU32::new((cursor_end_offset.0 - cursor_end_offset.1) as u32)
                {
                    (CursorShape::HollowBlock, width)
                } else {
                    (CursorShape::Beam, NonZeroU32::new(1).unwrap())
                };

                let cursor_column = Column(
                    (end.column.0 as isize - cursor_end_offset.0 as isize + 1).max(0) as usize,
                );
                let cursor_point = Point::new(point.line, cursor_column);
                let cursor = RenderableCursor::new(cursor_point, shape, fg, width);
                let mut cursor_rects: Vec<_> =
                    cursor.rects(&self.size_info, config.cursor.thickness()).collect();
                for rect in &mut cursor_rects {
                    rect.y += offset_y;
                }
                rects.extend(cursor_rects);
                cursor_point
            },
            _ => end,
        };

        self.window.update_ime_position(ime_popup_point, &ime_size_info);
    }

    /// Format search regex to account for the cursor and fullwidth characters.
    fn format_search(search_regex: &str, search_label: &str, max_width: usize) -> String {
        let label_len = search_label.len();

        // Skip `search_regex` formatting if only label is visible.
        if label_len > max_width {
            return search_label[..max_width].to_owned();
        }

        // The search string consists of `search_label` + `search_regex` + `cursor`.
        let mut bar_text = String::from(search_label);
        bar_text.extend(StrShortener::new(
            search_regex,
            max_width.wrapping_sub(label_len + 1),
            ShortenDirection::Left,
            Some(SHORTENER),
        ));

        // Add place for cursor.
        bar_text.push(' ');

        bar_text
    }

    /// Format command input to account for the cursor and fullwidth characters.
    fn format_command(command: &str, max_width: usize) -> String {
        if max_width == 0 {
            return String::new();
        }

        let mut bar_text: String = StrShortener::new(
            command,
            max_width.saturating_sub(1),
            ShortenDirection::Left,
            Some(SHORTENER),
        )
        .collect();

        // Add place for cursor.
        bar_text.push(' ');
        bar_text
    }

    fn format_footer_feedback(text: &str, max_width: usize) -> String {
        if max_width == 0 {
            return String::new();
        }

        StrShortener::new(text, max_width, ShortenDirection::Right, Some(SHORTENER)).collect()
    }

    /// Draw preview for the currently highlighted `Hyperlink`.
    #[inline(never)]
    fn draw_hyperlink_preview(
        &mut self,
        config: &UiConfig,
        layout: TerminalViewportLayout,
        cursor_point: Option<Point>,
        display_offset: usize,
    ) {
        let num_cols = self.size_info.columns();
        let uris: Vec<_> = self
            .highlighted_hint
            .iter()
            .chain(&self.vi_highlighted_hint)
            .filter_map(|hint| hint.hyperlink().map(|hyperlink| hyperlink.uri()))
            .map(|uri| StrShortener::new(uri, num_cols, ShortenDirection::Right, Some(SHORTENER)))
            .collect();

        if uris.is_empty() {
            return;
        }

        // The maximum amount of protected lines including the ones we'll show preview on.
        let max_protected_lines = uris.len() * 2;

        // Lines we shouldn't show preview on, because it'll obscure the highlighted hint.
        let mut protected_lines = Vec::with_capacity(max_protected_lines);
        if self.size_info.screen_lines() > max_protected_lines {
            // Prefer to show preview even when it'll likely obscure the highlighted hint, when
            // there's no place left for it.
            protected_lines.push(self.hint_mouse_point.map(|point| point.line));
            protected_lines.push(cursor_point.map(|point| point.line));
        }

        // Find the line in viewport we can draw preview on without obscuring protected lines.
        let viewport_bottom =
            layout.logical_size(&self.size_info).bottommost_line() - Line(display_offset as i32);
        let viewport_top =
            viewport_bottom - (layout.logical_size(&self.size_info).screen_lines() - 1);
        let uri_lines = (viewport_top.0..=viewport_bottom.0)
            .rev()
            .map(|line| Some(Line(line)))
            .filter_map(|line| {
                if protected_lines.contains(&line) {
                    None
                } else {
                    protected_lines.push(line);
                    line
                }
            })
            .take(uris.len())
            .filter_map(|line| {
                term::point_to_viewport(display_offset, Point::new(line, Column(0)))
                    .and_then(|point| layout.visual_point_for_logical_viewport(point))
            });

        let fg = config.colors.footer_bar_foreground();
        let bg = config.colors.footer_bar_background();
        for (uri, point) in uris.into_iter().zip(uri_lines) {
            // Damage the uri preview.
            let damage = LineDamageBounds::new(point.line, point.column.0, num_cols);
            self.damage_tracker.frame().damage_line(damage);

            // Damage the uri preview for the next frame as well.
            self.damage_tracker.next_frame().damage_line(damage);

            self.renderer.draw_string(point, fg, bg, uri, &self.size_info, &mut self.glyph_cache);
        }
    }

    fn footer_offset(&self) -> f32 {
        let size_info = self.size_info;
        size_info.footer_offset()
    }

    fn draw_footer_bar_background_with_height(
        &mut self,
        bg: Rgb,
        line: usize,
        offset_y: f32,
        height: f32,
    ) {
        let band = FooterBarViewportBand::new(&self.size_info, line, offset_y, height);
        let rect = RenderRect::new(band.x, band.y, band.width, band.height, bg, 1.);
        let metrics = self.glyph_cache.font_metrics();
        self.renderer.draw_rects(&self.size_info, &metrics, vec![rect]);
    }

    fn draw_footer_bar_line(&mut self, text: &str, fg: Rgb, bg: Rgb, line: usize, offset_y: f32) {
        self.draw_footer_bar_line_with_text_offset(text, fg, bg, line, offset_y, 0.);
    }

    fn draw_footer_bar_line_with_text_offset(
        &mut self,
        text: &str,
        fg: Rgb,
        bg: Rgb,
        line: usize,
        offset_y: f32,
        text_offset_y: f32,
    ) {
        let num_cols = self.size_info.columns();
        let text = format!("{text:<num_cols$}");
        let point = Point::new(line, Column(0));

        let extra_height = if text_offset_y < 0. { -text_offset_y } else { 0. };
        let background_offset =
            if text_offset_y < 0. { offset_y + text_offset_y } else { offset_y };
        let background_height = self.size_info.cell_height() + extra_height;
        let band =
            FooterBarViewportBand::new(&self.size_info, line, background_offset, background_height);
        let (x, y, width, height) = band.damage_rect();
        self.damage_tracker.frame().add_viewport_rect(&self.size_info, x, y, width, height);
        self.damage_tracker.next_frame().add_viewport_rect(&self.size_info, x, y, width, height);

        self.draw_footer_bar_background_with_height(bg, line, background_offset, background_height);

        let text_offset = offset_y + text_offset_y;
        self.renderer.set_text_projection_with_offset(&self.size_info, (0., text_offset));

        self.renderer.draw_string(
            point,
            fg,
            bg,
            text.chars(),
            &self.size_info,
            &mut self.glyph_cache,
        );

        self.renderer.set_text_projection(&self.size_info);
    }

    /// Draw current search regex.
    #[inline(never)]
    fn draw_search(&mut self, config: &UiConfig, text: &str, offset_y: f32) {
        let fg = config.colors.footer_bar_foreground();
        let bg = config.colors.footer_bar_background();
        let line = self.size_info.screen_lines().saturating_sub(1);

        self.draw_footer_bar_line(text, fg, bg, line, offset_y);
    }

    /// Draw current command input.
    #[inline(never)]
    fn draw_command_bar(&mut self, config: &UiConfig, text: &str, offset_y: f32) {
        let fg = config.colors.footer_bar_foreground();
        let bg = config.colors.footer_bar_background();
        let line = self.size_info.screen_lines().saturating_sub(1);

        self.draw_footer_bar_line(text, fg, bg, line, offset_y);
    }

    fn draw_command_feedback(
        &mut self,
        config: &UiConfig,
        message: &CommandFooterMessage,
        offset_y: f32,
    ) {
        let text = Self::format_footer_feedback(message.text(), self.size_info.columns());
        let fg = config.colors.primary.background;
        let bg = match message.ty() {
            MessageType::Error => config.colors.normal.red,
            MessageType::Warning => config.colors.normal.yellow,
        };
        let line = self.size_info.screen_lines().saturating_sub(1);

        self.draw_footer_bar_line(&text, fg, bg, line, offset_y);
    }

    fn draw_footer_right_aligned_text(
        &mut self,
        text: &str,
        fg: Rgb,
        bg: Rgb,
        line: usize,
        offset_y: f32,
    ) {
        let columns = self.size_info.columns();
        if columns == 0 {
            return;
        }

        let text: String =
            StrShortener::new(text, columns, ShortenDirection::Left, Some(SHORTENER)).collect();
        let column = Column(columns.saturating_sub(text.chars().count()));
        let point = Point::new(line, column);

        self.renderer.set_text_projection_with_offset(&self.size_info, (0., offset_y));
        self.renderer.draw_string(
            point,
            fg,
            bg,
            text.chars(),
            &self.size_info,
            &mut self.glyph_cache,
        );
        self.renderer.set_text_projection(&self.size_info);
    }

    fn draw_footer_line_indicator(&mut self, config: &UiConfig, text: &str, offset_y: f32) {
        let colors = &config.colors;
        let fg = colors.line_indicator.foreground.unwrap_or(colors.primary.background);
        let bg = colors.line_indicator.background.unwrap_or(colors.primary.foreground);
        let line = self.size_info.screen_lines().saturating_sub(1);

        self.draw_footer_right_aligned_text(text, fg, bg, line, offset_y);
    }

    /// Draw render timer.
    #[inline(never)]
    fn draw_render_timer(&mut self, config: &UiConfig) {
        if !config.debug.render_timer {
            return;
        }

        let timing = format!("{:.3} usec", self.meter.average());
        let point = Point::new(self.size_info.screen_lines().saturating_sub(2), Column(0));
        let fg = config.colors.primary.background;
        let bg = config.colors.normal.red;

        // Damage render timer for current and next frame.
        let damage = LineDamageBounds::new(point.line, point.column.0, timing.len());
        self.damage_tracker.frame().damage_line(damage);
        self.damage_tracker.next_frame().damage_line(damage);

        let glyph_cache = &mut self.glyph_cache;
        self.renderer.draw_string(point, fg, bg, timing.chars(), &self.size_info, glyph_cache);
    }

    /// Draw an indicator for the position of a line in history.
    #[inline(never)]
    fn draw_line_indicator(
        &mut self,
        config: &UiConfig,
        total_lines: usize,
        obstructed_column: Option<Column>,
        line: usize,
    ) {
        let columns = self.size_info.columns();
        let text = line_indicator_text(line, total_lines);
        let column = Column(self.size_info.columns().saturating_sub(text.len()));
        let point = Point::new(0, column);

        // Damage the line indicator for current and next frame.
        let damage = LineDamageBounds::new(point.line, point.column.0, columns - 1);
        self.damage_tracker.frame().damage_line(damage);
        self.damage_tracker.next_frame().damage_line(damage);

        let colors = &config.colors;
        let fg = colors.line_indicator.foreground.unwrap_or(colors.primary.background);
        let bg = colors.line_indicator.background.unwrap_or(colors.primary.foreground);

        // Do not render anything if it would obscure the vi mode cursor.
        if obstructed_column.is_none_or(|obstructed_column| obstructed_column < column) {
            let glyph_cache = &mut self.glyph_cache;
            self.renderer.draw_string(point, fg, bg, text.chars(), &self.size_info, glyph_cache);
        }
    }

    /// Highlight damaged rects.
    ///
    /// This function is for debug purposes only.
    fn highlight_damage(&self, render_rects: &mut Vec<RenderRect>) {
        for damage_rect in &self.damage_tracker.shape_frame_damage(self.size_info.into()) {
            let x = damage_rect.x as f32;
            let height = damage_rect.height as f32;
            let width = damage_rect.width as f32;
            let y = damage_y_to_viewport_y(&self.size_info, damage_rect) as f32;
            let render_rect = RenderRect::new(x, y, width, height, DAMAGE_RECT_COLOR, 0.5);

            render_rects.push(render_rect);
        }
    }

    /// Check whether a hint highlight needs to be cleared.
    fn validate_hint_highlights(&mut self, display_offset: usize) {
        if self.terminal_viewport.is_multi_column() {
            return;
        }

        let frame = self.damage_tracker.frame();
        let hints = [
            (&mut self.highlighted_hint, &mut self.highlighted_hint_age, true),
            (&mut self.vi_highlighted_hint, &mut self.vi_highlighted_hint_age, false),
        ];

        let num_lines = self.size_info.screen_lines();
        for (hint, hint_age, reset_mouse) in hints {
            let (start, end) = match hint {
                Some(hint) => (*hint.bounds().start(), *hint.bounds().end()),
                None => continue,
            };

            // Ignore hints that were created this frame.
            *hint_age += 1;
            if *hint_age == 1 {
                continue;
            }

            // Convert hint bounds to viewport coordinates.
            let start = term::point_to_viewport(display_offset, start)
                .filter(|point| point.line < num_lines)
                .unwrap_or_default();
            let end = term::point_to_viewport(display_offset, end)
                .filter(|point| point.line < num_lines)
                .unwrap_or_else(|| Point::new(num_lines - 1, self.size_info.last_column()));

            // Clear invalidated hints.
            if frame.intersects(start, end) {
                if reset_mouse {
                    self.window.set_mouse_cursor(CursorIcon::Default);
                }
                frame.mark_fully_damaged();
                *hint = None;
            }
        }
    }

    /// Request a new frame for a window on Wayland.
    pub(crate) fn request_frame(&mut self, scheduler: &mut Scheduler) {
        // Mark that we've used a frame.
        self.window.has_frame = false;

        // Get the display vblank interval.
        let monitor_vblank_interval = 1_000_000.
            / self
                .window
                .current_monitor()
                .and_then(|monitor| monitor.refresh_rate_millihertz())
                .unwrap_or(60_000) as f64;

        // Now convert it to micro seconds.
        let monitor_vblank_interval =
            Duration::from_micros((1000. * monitor_vblank_interval) as u64);

        let swap_timeout = self.frame_timer.compute_timeout(monitor_vblank_interval);

        let window_id = self.window.id();
        let timer_id = TimerId::new(Topic::Frame, window_id);
        let event = Event::new(EventType::Frame, window_id);

        scheduler.schedule(event, swap_timeout, false, timer_id);
    }
}

#[cfg(target_os = "macos")]
fn dip_to_physical(value: usize, scale_factor: f64) -> usize {
    ((value as f64) * scale_factor).round().max(0.0) as usize
}

#[cfg(target_os = "macos")]
fn auxiliary_top_region_in_content_space(
    content_left: f64,
    content_width: f64,
    region: crate::ipc::IpcWindowDebugRect,
    scale_factor: f64,
) -> Option<AuxiliaryTopRegion> {
    if region.width <= 0.0 || region.height <= 0.0 || content_width <= 0.0 {
        return None;
    }

    let start_x = (region.x.mul_add(scale_factor, -content_left)).max(0.0).floor();
    let end_x = ((region.x + region.width) * scale_factor - content_left).min(content_width).ceil();
    if end_x <= start_x {
        return None;
    }

    Some(AuxiliaryTopRegion { x: start_x as usize, width: (end_x - start_x) as usize })
}

#[cfg(target_os = "macos")]
fn browser_main_image_slices(
    layout: &BrowserViewportLayout,
    scale_factor: f64,
    image_width_px: usize,
    image_height_px: usize,
) -> Vec<ImageSlice> {
    let viewport_width_px = dip_to_physical(layout.logical_width(), scale_factor).max(1);
    let mut slices = Vec::with_capacity(layout.column_count());

    for column_index in 0..layout.column_count() {
        let Some(column_rect) = layout.column_rect(column_index) else {
            continue;
        };
        let Some(column_logical_y) = layout.column_logical_y(column_index) else {
            continue;
        };

        let column_height_px = dip_to_physical(column_rect.height, scale_factor).max(1);
        let src_y_px = dip_to_physical(column_logical_y, scale_factor);
        if src_y_px >= image_height_px {
            continue;
        }

        let src_height_px = column_height_px.min(image_height_px.saturating_sub(src_y_px));
        let dest_height_px = ((src_height_px as f64 / column_height_px as f64)
            * dip_to_physical(column_rect.height, scale_factor) as f64)
            .round() as usize;

        slices.push(ImageSlice {
            dest_x_px: dip_to_physical(column_rect.x, scale_factor),
            dest_y_px: dip_to_physical(column_rect.y, scale_factor),
            dest_width_px: dip_to_physical(column_rect.width, scale_factor),
            dest_height_px,
            src_x_px: 0,
            src_y_px,
            src_width_px: image_width_px.min(viewport_width_px),
            src_height_px,
        });
    }

    slices
}

#[cfg(target_os = "macos")]
fn browser_popup_image_slices(
    layout: &BrowserViewportLayout,
    scale_factor: f64,
    popup: WebPopupSurfaceRef,
) -> Vec<ImageSlice> {
    if popup.width == 0 || popup.height == 0 {
        return Vec::new();
    }

    let popup_scale_x = popup.surface.width as f64 / popup.width as f64;
    let popup_scale_y = popup.surface.height as f64 / popup.height as f64;
    let popup_left = popup.x;
    let popup_top = popup.y;
    let popup_bottom = popup.y.saturating_add(popup.height);

    let mut slices = Vec::new();

    for column_index in 0..layout.column_count() {
        let Some(column_rect) = layout.column_rect(column_index) else {
            continue;
        };
        let Some(column_top) = layout.column_logical_y(column_index) else {
            continue;
        };
        let column_bottom = column_top.saturating_add(column_rect.height);
        let slice_top = popup_top.max(column_top);
        let slice_bottom = popup_bottom.min(column_bottom);
        if slice_top >= slice_bottom {
            continue;
        }

        let logical_slice_height = slice_bottom.saturating_sub(slice_top);
        let src_y_px =
            ((slice_top.saturating_sub(popup_top) as f64) * popup_scale_y).round() as usize;
        let src_height_px = ((logical_slice_height as f64) * popup_scale_y).round() as usize;
        let src_width_px = ((popup.width as f64) * popup_scale_x).round() as usize;

        slices.push(ImageSlice {
            dest_x_px: dip_to_physical(column_rect.x.saturating_add(popup_left), scale_factor),
            dest_y_px: dip_to_physical(
                column_rect.y.saturating_add(slice_top.saturating_sub(column_top)),
                scale_factor,
            ),
            dest_width_px: dip_to_physical(popup.width, scale_factor),
            dest_height_px: dip_to_physical(logical_slice_height, scale_factor),
            src_x_px: 0,
            src_y_px,
            src_width_px: src_width_px.min(popup.surface.width),
            src_height_px: src_height_px.min(popup.surface.height.saturating_sub(src_y_px)),
        });
    }

    slices
}

impl Drop for Display {
    fn drop(&mut self) {
        // Switch OpenGL context before dropping, otherwise objects (like programs) from other
        // contexts might be deleted when dropping renderer.
        self.make_current();
        unsafe {
            ManuallyDrop::drop(&mut self.renderer);
            ManuallyDrop::drop(&mut self.context);
            ManuallyDrop::drop(&mut self.surface);
        }
    }
}

/// Input method state.
#[derive(Debug, Default)]
pub struct Ime {
    /// Whether the IME is enabled.
    enabled: bool,

    /// Current IME preedit.
    preedit: Option<Preedit>,
}

impl Ime {
    #[inline]
    pub fn set_enabled(&mut self, is_enabled: bool) {
        if is_enabled {
            self.enabled = is_enabled
        } else {
            // Clear state when disabling IME.
            *self = Default::default();
        }
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    pub fn set_preedit(&mut self, preedit: Option<Preedit>) {
        self.preedit = preedit;
    }

    #[inline]
    pub fn preedit(&self) -> Option<&Preedit> {
        self.preedit.as_ref()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Preedit {
    /// The preedit text.
    text: String,

    /// Byte offset for cursor start into the preedit text.
    ///
    /// `None` means that the cursor is invisible.
    cursor_byte_offset: Option<(usize, usize)>,

    /// The cursor offset from the end of the start of the preedit in char width.
    cursor_end_offset: Option<(usize, usize)>,
}

impl Preedit {
    pub fn new(text: String, cursor_byte_offset: Option<(usize, usize)>) -> Self {
        let cursor_end_offset = if let Some(byte_offset) = cursor_byte_offset {
            // Convert byte offset into char offset.
            let start_to_end_offset =
                text[byte_offset.0..].chars().fold(0, |acc, ch| acc + ch.width().unwrap_or(1));
            let end_to_end_offset =
                text[byte_offset.1..].chars().fold(0, |acc, ch| acc + ch.width().unwrap_or(1));

            Some((start_to_end_offset, end_to_end_offset))
        } else {
            None
        };

        Self { text, cursor_byte_offset, cursor_end_offset }
    }
}

/// Pending renderer updates.
///
/// All renderer updates are cached to be applied just before rendering, to avoid platform-specific
/// rendering issues.
#[derive(Debug, Default, Copy, Clone)]
pub struct RendererUpdate {
    /// Should resize the window.
    resize: bool,

    /// Clear font caches.
    clear_font_cache: bool,
}

/// The frame timer state.
pub struct FrameTimer {
    /// Base timestamp used to compute sync points.
    base: Instant,

    /// The last timestamp we synced to.
    last_synced_timestamp: Instant,

    /// The refresh rate we've used to compute sync timestamps.
    refresh_interval: Duration,
}

impl FrameTimer {
    pub fn new() -> Self {
        let now = Instant::now();
        Self { base: now, last_synced_timestamp: now, refresh_interval: Duration::ZERO }
    }

    /// Compute the delay that we should use to achieve the target frame
    /// rate.
    pub fn compute_timeout(&mut self, refresh_interval: Duration) -> Duration {
        let now = Instant::now();

        // Handle refresh rate change.
        if self.refresh_interval != refresh_interval {
            self.base = now;
            self.last_synced_timestamp = now;
            self.refresh_interval = refresh_interval;
            return refresh_interval;
        }

        let next_frame = self.last_synced_timestamp + self.refresh_interval;

        if next_frame < now {
            // Redraw immediately if we haven't drawn in over `refresh_interval` microseconds.
            let elapsed_micros = (now - self.base).as_micros() as u64;
            let refresh_micros = self.refresh_interval.as_micros() as u64;
            self.last_synced_timestamp =
                now - Duration::from_micros(elapsed_micros % refresh_micros);
            Duration::ZERO
        } else {
            // Redraw on the next `refresh_interval` clock tick.
            self.last_synced_timestamp = next_frame;
            next_frame - now
        }
    }
}

/// Calculate the cell dimensions based on font metrics.
///
/// This will return a tuple of the cell width and height.
#[inline]
fn compute_cell_size(config: &UiConfig, metrics: &crossfont::Metrics) -> (f32, f32) {
    let offset_x = f64::from(config.font.offset.x);
    let offset_y = f64::from(config.font.offset.y);
    (
        (metrics.average_advance + offset_x).floor().max(1.) as f32,
        (metrics.line_height + offset_y).floor().max(1.) as f32,
    )
}

/// Calculate the size of the window given padding, terminal dimensions and cell size.
fn window_size(
    config: &UiConfig,
    dimensions: Dimensions,
    cell_width: f32,
    cell_height: f32,
    scale_factor: f32,
) -> PhysicalSize<u32> {
    let padding = config.window.padding(scale_factor);

    let grid_width = cell_width * dimensions.columns.max(MIN_COLUMNS) as f32;
    let grid_height = cell_height * dimensions.lines.max(MIN_SCREEN_LINES) as f32;

    #[cfg(target_os = "macos")]
    let panel_width = if config.window.tab_panel.enabled {
        config.window.tab_panel.width as f32 * scale_factor
    } else {
        0.
    };
    #[cfg(not(target_os = "macos"))]
    let panel_width = 0.;

    let width = (padding.0 * 2. + grid_width + panel_width).floor();
    let height = (padding.1).mul_add(2., grid_height).floor();

    PhysicalSize::new(width as u32, height as u32)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::config::browser::MultiColumnBrowserConfig;
    use crate::config::terminal::MultiColumnTerminalConfig;
    use crate::display::browser_layout::BrowserViewMode;

    #[test]
    fn macos_window_controls_visibility_policy() {
        assert_eq!(
            Display::macos_semaphore_layout_mode(true, Decorations::Full, false),
            MacosSemaphoreLayoutMode::WindowedInset
        );
        assert_eq!(
            Display::macos_semaphore_layout_mode(true, Decorations::Transparent, false),
            MacosSemaphoreLayoutMode::WindowedInset
        );
        assert_eq!(
            Display::macos_semaphore_layout_mode(true, Decorations::Full, true),
            MacosSemaphoreLayoutMode::FullscreenCustom
        );
        assert_eq!(
            Display::macos_semaphore_layout_mode(true, Decorations::Transparent, true),
            MacosSemaphoreLayoutMode::FullscreenCustom
        );
        assert_eq!(
            Display::macos_semaphore_layout_mode(false, Decorations::Full, false),
            MacosSemaphoreLayoutMode::Hidden
        );
        assert_eq!(
            Display::macos_semaphore_layout_mode(true, Decorations::Buttonless, false),
            MacosSemaphoreLayoutMode::Hidden
        );
        assert_eq!(
            Display::macos_semaphore_layout_mode(true, Decorations::None, false),
            MacosSemaphoreLayoutMode::Hidden
        );
    }

    #[test]
    fn macos_fullscreen_roundtrip_uses_measured_inset() {
        let measured_inset = 26.0_f32;

        let windowed_layout = Display::macos_semaphore_layout_mode(true, Decorations::Full, false);
        let windowed_top_inset = match windowed_layout {
            MacosSemaphoreLayoutMode::WindowedInset => measured_inset,
            MacosSemaphoreLayoutMode::Hidden | MacosSemaphoreLayoutMode::FullscreenCustom => 0.0,
        };
        assert_eq!(windowed_top_inset, measured_inset);

        let fullscreen_layout = Display::macos_semaphore_layout_mode(true, Decorations::Full, true);
        let fullscreen_band_height = match fullscreen_layout {
            MacosSemaphoreLayoutMode::FullscreenCustom => {
                crate::display::window::macos_fullscreen_window_controls_band_height_px(
                    1.0, 4.0, 0.0,
                )
            },
            MacosSemaphoreLayoutMode::Hidden | MacosSemaphoreLayoutMode::WindowedInset => 0.0,
        };
        assert_eq!(fullscreen_band_height, 37.0);
    }

    #[test]
    fn macos_native_fullscreen_reserves_minimum_control_band() {
        assert_eq!(
            crate::display::window::macos_fullscreen_window_controls_extra_top_padding_px(
                1.0, 4.0, 0.0,
            ),
            33.0
        );
    }

    #[test]
    fn asymmetric_vertical_padding_preserves_footer_status_space() {
        let mut size =
            SizeInfo::new_with_vertical_padding(200., 100., 1., 1., 0., 0., 20., 4., false);
        size.reserve_lines(1);

        assert_eq!(size.viewport_height(), 76.);
        assert_eq!(size.screen_lines(), 75);
        assert_eq!(size.footer_offset(), 5.);
    }

    #[test]
    fn size_info_u32_conversion_preserves_columns() {
        let size = SizeInfo::new(300., 120., 10., 20., 40., 20., 12., false);
        let converted: SizeInfo<u32> = size.into();

        assert_eq!(converted.columns, size.columns());
        assert_eq!(converted.screen_lines, size.screen_lines());
    }

    #[test]
    fn footer_bar_band_uses_full_width_without_padding() {
        let size = SizeInfo::new(120., 40., 1., 1., 0., 0., 0., false);

        assert_eq!(
            FooterBarViewportBand::new(&size, 39, 0., 1.),
            FooterBarViewportBand { x: 0., y: 39., width: 120., height: 1. }
        );
    }

    #[test]
    fn footer_bar_band_starts_at_content_origin_and_excludes_right_padding() {
        let size = SizeInfo::new(300., 60., 1., 1., 120., 8., 0., false);

        assert_eq!(
            FooterBarViewportBand::new(&size, 59, 0., 1.),
            FooterBarViewportBand { x: 120., y: 59., width: 172., height: 1. }
        );
    }

    #[test]
    fn footer_bar_band_preserves_vertical_offset_and_damage_rect() {
        let size = SizeInfo::new(300., 120., 10., 20., 130., 8., 5., false);
        let band = FooterBarViewportBand::new(&size, 4, 6., 24.);

        assert_eq!(band, FooterBarViewportBand { x: 130., y: 91., width: 162., height: 24. });
        assert_eq!(band.damage_rect(), (130, 91, 162, 24));
    }

    #[test]
    fn terminal_footer_bar_mode_shows_vi_indicator_only_when_idle_in_vi_mode() {
        assert_eq!(
            terminal_footer_bar_mode(false, false, false, false),
            TerminalFooterBarMode::None
        );
        assert_eq!(
            terminal_footer_bar_mode(false, false, false, true),
            TerminalFooterBarMode::ViIndicator
        );
        assert_eq!(
            terminal_footer_bar_mode(false, true, false, true),
            TerminalFooterBarMode::Search
        );
        assert_eq!(
            terminal_footer_bar_mode(true, false, false, true),
            TerminalFooterBarMode::Command
        );
        assert_eq!(
            terminal_footer_bar_mode(false, false, true, true),
            TerminalFooterBarMode::CommandFeedback
        );
        assert_eq!(
            terminal_footer_bar_mode(false, true, true, true),
            TerminalFooterBarMode::Search
        );
        assert_eq!(
            terminal_footer_bar_mode(true, false, true, true),
            TerminalFooterBarMode::Command
        );
    }

    #[test]
    fn vi_mode_line_indicator_uses_logical_multi_column_height() {
        let size = SizeInfo::new(300., 4., 1., 1., 0., 0., 0., false);
        let layout = TerminalViewportLayout::new(
            &size,
            TerminalViewMode::MultiColumn,
            &MultiColumnTerminalConfig::default(),
            None,
            None,
        );

        assert_eq!(vi_mode_line_indicator_line(layout, &size, Point::new(Line(11), Column(0))), 0);
        assert_eq!(vi_mode_line_indicator_line(layout, &size, Point::new(Line(0), Column(0))), 11);
    }

    #[cfg(target_os = "macos")]
    fn browser_layout(width: usize, height: usize) -> BrowserViewportLayout {
        let size = SizeInfo::new(width as f32, height as f32, 1., 1., 0., 0., 0., false);
        BrowserViewportLayout::new(
            &size,
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
            None,
            None,
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_main_image_slices_follow_column_layout() {
        let layout = browser_layout(1950, 600);
        let slices = browser_main_image_slices(&layout, 1.0, layout.logical_width(), 1200);

        assert_eq!(slices.len(), 2);
        assert_eq!(
            slices[0],
            ImageSlice {
                dest_x_px: 0,
                dest_y_px: 0,
                dest_width_px: 975,
                dest_height_px: 600,
                src_x_px: 0,
                src_y_px: 0,
                src_width_px: 975,
                src_height_px: 600,
            }
        );
        assert_eq!(
            slices[1],
            ImageSlice {
                dest_x_px: 975,
                dest_y_px: 0,
                dest_width_px: 975,
                dest_height_px: 600,
                src_x_px: 0,
                src_y_px: 600,
                src_width_px: 975,
                src_height_px: 600,
            }
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_popup_image_slices_split_across_columns() {
        let layout = browser_layout(1950, 600);
        let popup = crate::macos::webview::WebPopupSurfaceRef {
            x: 40,
            y: 560,
            width: 100,
            height: 80,
            surface: crate::macos::webview::WebSurfaceRef {
                io_surface: std::ptr::null_mut(),
                width: 100,
                height: 80,
                format: cef::ColorType::BGRA_8888,
            },
        };
        let slices = browser_popup_image_slices(&layout, 1.0, popup);
        assert_eq!(slices.len(), 2);
        assert_eq!(
            slices[0],
            ImageSlice {
                dest_x_px: 40,
                dest_y_px: 560,
                dest_width_px: 100,
                dest_height_px: 40,
                src_x_px: 0,
                src_y_px: 0,
                src_width_px: 100,
                src_height_px: 40,
            }
        );
        assert_eq!(
            slices[1],
            ImageSlice {
                dest_x_px: 1015,
                dest_y_px: 0,
                dest_width_px: 100,
                dest_height_px: 40,
                src_x_px: 0,
                src_y_px: 40,
                src_width_px: 100,
                src_height_px: 40,
            }
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_main_image_slices_follow_mixed_height_columns() {
        let size = SizeInfo::new_with_vertical_padding(900., 680., 1., 1., 0., 0., 40., 0., false);
        let layout = BrowserViewportLayout::new(
            &size,
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
            Some(3),
            Some(EarAwareTopRegions {
                reclaim_top_px: 40,
                left: Some(AuxiliaryTopRegion { x: 0, width: 300 }),
                right: Some(AuxiliaryTopRegion { x: 600, width: 300 }),
            }),
        );
        let slices = browser_main_image_slices(&layout, 1.0, layout.logical_width(), 2000);
        assert_eq!(slices.len(), 3);
        assert_eq!(slices[0].src_y_px, 0);
        assert_eq!(slices[0].src_height_px, 680);
        assert_eq!(slices[0].dest_y_px, 0);
        assert_eq!(slices[0].dest_height_px, 680);
        assert_eq!(slices[1].src_y_px, 680);
        assert_eq!(slices[1].src_height_px, 640);
        assert_eq!(slices[1].dest_y_px, 40);
        assert_eq!(slices[1].dest_height_px, 640);
        assert_eq!(slices[2].src_y_px, 1320);
        assert_eq!(slices[2].src_height_px, 680);
        assert_eq!(slices[2].dest_y_px, 0);
        assert_eq!(slices[2].dest_height_px, 680);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_popup_image_slices_follow_mixed_height_columns() {
        let size = SizeInfo::new_with_vertical_padding(900., 680., 1., 1., 0., 0., 40., 0., false);
        let layout = BrowserViewportLayout::new(
            &size,
            1.0,
            BrowserViewMode::MultiColumn,
            &MultiColumnBrowserConfig::default(),
            Some(3),
            Some(EarAwareTopRegions {
                reclaim_top_px: 40,
                left: Some(AuxiliaryTopRegion { x: 0, width: 300 }),
                right: Some(AuxiliaryTopRegion { x: 600, width: 300 }),
            }),
        );
        let popup = crate::macos::webview::WebPopupSurfaceRef {
            x: 40,
            y: 1300,
            width: 100,
            height: 60,
            surface: crate::macos::webview::WebSurfaceRef {
                io_surface: std::ptr::null_mut(),
                width: 100,
                height: 60,
                format: cef::ColorType::BGRA_8888,
            },
        };
        let slices = browser_popup_image_slices(&layout, 1.0, popup);
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].dest_x_px, 340);
        assert_eq!(slices[0].dest_y_px, 660);
        assert_eq!(slices[0].dest_height_px, 20);
        assert_eq!(slices[0].src_y_px, 0);
        assert_eq!(slices[0].src_height_px, 20);
        assert_eq!(slices[1].dest_x_px, 640);
        assert_eq!(slices[1].dest_y_px, 0);
        assert_eq!(slices[1].dest_height_px, 40);
        assert_eq!(slices[1].src_y_px, 20);
        assert_eq!(slices[1].src_height_px, 40);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn auxiliary_top_regions_convert_screen_points_to_physical_pixels() {
        let region = auxiliary_top_region_in_content_space(
            0.0,
            3840.0,
            crate::ipc::IpcWindowDebugRect { x: 0.0, y: 1206.0, width: 856.0, height: 37.0 },
            2.0,
        );

        assert_eq!(region, Some(AuxiliaryTopRegion { x: 0, width: 1712 }));
    }
}
