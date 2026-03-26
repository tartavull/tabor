use std::cell::{Cell, RefCell};
use std::env;
use std::error::Error;
use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::debug;

use cef::{
    self, App, BrowserProcessHandler, CefString, ImplApp, ImplBrowser, ImplBrowserProcessHandler,
    ImplCommandLine, ImplDomnode, ImplFrame, ImplListValue, ImplProcessMessage,
    ImplRenderProcessHandler, LogSeverity, RenderProcessHandler, Settings, WrapApp,
    WrapBrowserProcessHandler, WrapRenderProcessHandler, args::Args, rc::Rc,
};

#[cfg(feature = "passkey-webauthn")]
const DISABLE_FEATURES: &str = "CalculateNativeWinOcclusion";

#[cfg(not(feature = "passkey-webauthn"))]
const DISABLE_FEATURES: &str = "CalculateNativeWinOcclusion,WebAuthentication,WebAuthenticationAPI,ContentWebAuthenticationAPI,WebBluetooth,ContentWebBluetooth,WebBluetoothNewPermissionsBackend";

#[cfg(not(feature = "passkey-webauthn"))]
const DISABLE_BLINK_FEATURES: &str = "WebAuthentication,WebBluetooth";

pub(super) const WEB_EDITABLE_FOCUS_MESSAGE_NAME: &str = "tabor.web_editable_focus";
pub(super) const WEB_EDITABLE_FOCUS_EDITABLE_ARG_INDEX: usize = 0;

type MessagePumpNotifier = Arc<dyn Fn(Duration) + Send + Sync + 'static>;

const METRICS_NONE: u64 = u64::MAX;
const MAX_MESSAGE_PUMP_DELAY_MS: u64 = 10_000;

static MESSAGE_PUMP_NOTIFIER: OnceLock<MessagePumpNotifier> = OnceLock::new();
static CEF_PUMP_SCHEDULED: AtomicU64 = AtomicU64::new(0);
static CEF_PUMP_EXECUTED: AtomicU64 = AtomicU64::new(0);
static CEF_PUMP_COALESCED: AtomicU64 = AtomicU64::new(0);
static CEF_PUMP_LAST_REQUESTED_DELAY_MS: AtomicU64 = AtomicU64::new(METRICS_NONE);
static CEF_PUMP_LAST_EFFECTIVE_DELAY_MS: AtomicU64 = AtomicU64::new(METRICS_NONE);
static CEF_PUMP_HIDDEN_THROTTLE_ACTIVE: AtomicBool = AtomicBool::new(false);
static CEF_PUMP_LAST_RUN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CefPumpMetrics {
    pub scheduled: u64,
    pub executed: u64,
    pub coalesced: u64,
    pub last_requested_delay_ms: Option<u64>,
    pub last_effective_delay_ms: Option<u64>,
    pub last_run_ms_ago: Option<u64>,
    pub hidden_throttle_active: bool,
}

fn last_run_lock() -> &'static Mutex<Option<Instant>> {
    CEF_PUMP_LAST_RUN.get_or_init(|| Mutex::new(None))
}

fn decode_metric(value: u64) -> Option<u64> {
    if value == METRICS_NONE { None } else { Some(value) }
}

fn schedule_message_pump_work(delay_ms: i64) {
    let normalized_delay_ms = delay_ms.max(0) as u64;
    let clamped_delay_ms = normalized_delay_ms.min(MAX_MESSAGE_PUMP_DELAY_MS);
    CEF_PUMP_SCHEDULED.fetch_add(1, Ordering::Relaxed);
    CEF_PUMP_LAST_REQUESTED_DELAY_MS.store(clamped_delay_ms, Ordering::Relaxed);

    if let Some(notifier) = MESSAGE_PUMP_NOTIFIER.get() {
        notifier(Duration::from_millis(clamped_delay_ms));
    }
}

fn send_web_editable_focus_message(
    browser: Option<&mut cef::Browser>,
    frame: Option<&mut cef::Frame>,
    editable: bool,
) {
    let Some(mut message) =
        cef::process_message_create(Some(&CefString::from(WEB_EDITABLE_FOCUS_MESSAGE_NAME)))
    else {
        return;
    };
    let Some(args) = message.argument_list() else {
        return;
    };
    if args.set_size(WEB_EDITABLE_FOCUS_EDITABLE_ARG_INDEX + 1) == 0 {
        return;
    }
    if args.set_bool(WEB_EDITABLE_FOCUS_EDITABLE_ARG_INDEX, if editable { 1 } else { 0 }) == 0 {
        return;
    }

    if let Some(frame) = frame {
        frame.send_process_message(cef::ProcessId::BROWSER, Some(&mut message));
        return;
    }

    if let Some(browser) = browser {
        if let Some(frame) = browser.main_frame() {
            frame.send_process_message(cef::ProcessId::BROWSER, Some(&mut message));
        }
    }
}

cef::wrap_browser_process_handler! {
    struct TaborBrowserProcessHandler {}

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            schedule_message_pump_work(delay_ms);
        }
    }
}

cef::wrap_render_process_handler! {
    struct TaborRenderProcessHandler {}

    impl RenderProcessHandler {
        fn on_focused_node_changed(
            &self,
            browser: Option<&mut cef::Browser>,
            frame: Option<&mut cef::Frame>,
            node: Option<&mut cef::Domnode>,
        ) {
            let editable = node.map(|node| node.is_editable() != 0).unwrap_or(false);
            send_web_editable_focus_message(browser, frame, editable);
        }
    }
}

cef::wrap_app! {
    struct TaborCefApp {}

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&cef::CefString>,
            command_line: Option<&mut cef::CommandLine>,
        ) {
            let Some(command_line) = command_line else {
                return;
            };
            let use_mock = cef::CefString::from("use-mock-keychain");
            command_line.append_switch(Some(&use_mock));

            let password_store = cef::CefString::from("password-store");
            let basic = cef::CefString::from("basic");
            command_line.append_switch_with_value(Some(&password_store), Some(&basic));

            let disable_renderer_bg = cef::CefString::from("disable-renderer-backgrounding");
            command_line.append_switch(Some(&disable_renderer_bg));

            let disable_timer_throttle =
                cef::CefString::from("disable-background-timer-throttling");
            command_line.append_switch(Some(&disable_timer_throttle));

            let disable_occluded =
                cef::CefString::from("disable-backgrounding-occluded-windows");
            command_line.append_switch(Some(&disable_occluded));

            if fake_media_enabled() {
                let fake_device = cef::CefString::from("use-fake-device-for-media-stream");
                command_line.append_switch(Some(&fake_device));

                let fake_ui = cef::CefString::from("use-fake-ui-for-media-stream");
                command_line.append_switch(Some(&fake_ui));
            }

            let disable_features = cef::CefString::from("disable-features");
            let disable_features_value = cef::CefString::from(DISABLE_FEATURES);
            command_line.append_switch_with_value(Some(&disable_features), Some(&disable_features_value));

            #[cfg(not(feature = "passkey-webauthn"))]
            {
                let disable_blink_features = cef::CefString::from("disable-blink-features");
                let disable_blink_features_value =
                    cef::CefString::from(DISABLE_BLINK_FEATURES);
                command_line.append_switch_with_value(
                    Some(&disable_blink_features),
                    Some(&disable_blink_features_value),
                );

                let disable_webauthn = cef::CefString::from("disable-webauthn");
                command_line.append_switch(Some(&disable_webauthn));
            }
        }

        fn browser_process_handler(&self) -> Option<cef::BrowserProcessHandler> {
            Some(TaborBrowserProcessHandler::new())
        }

        fn render_process_handler(&self) -> Option<cef::RenderProcessHandler> {
            Some(TaborRenderProcessHandler::new())
        }
    }
}

struct CefRuntime {
    _args: Args,
    _framework_dir: PathBuf,
    _app: cef::App,
    _sandbox: Option<cef::sandbox::Sandbox>,
}

thread_local! {
    static CEF_RUNTIME: RefCell<Option<CefRuntime>> = const { RefCell::new(None) };
    static CEF_LIBRARY_LOADED: Cell<bool> = const { Cell::new(false) };
}

static CEF_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn env_flag_enabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| value != OsStr::new("0"))
}

fn fake_media_enabled() -> bool {
    env_flag_enabled(env::var_os("TABOR_CEF_FAKE_MEDIA").as_deref())
}

fn configured_cache_root() -> PathBuf {
    env::var("TABOR_CEF_CACHE_PATH").map(PathBuf::from).unwrap_or_else(|_| super::cef_cache_dir())
}

fn cef_no_sandbox_setting() -> i32 {
    if super::distribution_channel().is_mac_app_store() { 0 } else { 1 }
}

const CEF_HELPER_NAMES: [&str; 5] = [
    "Tabor Helper",
    "Tabor Helper (Renderer)",
    "Tabor Helper (GPU)",
    "Tabor Helper (Plugin)",
    "Tabor Helper (Alerts)",
];

struct BundlePaths {
    current_bundle: PathBuf,
    main_bundle: PathBuf,
}

fn ensure_application_selector_contract() -> Result<(), Box<dyn Error>> {
    super::ensure_cef_application()
}

pub fn maybe_execute_subprocess() -> Result<Option<i32>, Box<dyn Error>> {
    let Some(framework_dir) = framework_dir() else {
        return Ok(None);
    };

    ensure_application_selector_contract()?;

    load_library(&framework_dir)?;
    ensure_cef_sidecar_libs(&framework_dir)?;

    let args = Args::new();
    let sandbox = maybe_initialize_sandbox(&args);
    let mut app = TaborCefApp::new();
    let exit_code =
        cef::execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());
    let _ = sandbox;

    if exit_code >= 0 {
        return Ok(Some(exit_code));
    }

    Ok(None)
}

pub fn ensure_initialized() -> Result<(), Box<dyn Error>> {
    if is_initialized() {
        return Ok(());
    }

    let framework_dir = framework_dir().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "CEF framework not found")
    })?;

    ensure_application_selector_contract()?;
    load_library(&framework_dir)?;
    ensure_cef_sidecar_libs(&framework_dir)?;

    let args = Args::new();
    let sandbox = maybe_initialize_sandbox(&args);
    let mut app = TaborCefApp::new();
    let mut settings = Settings {
        no_sandbox: cef_no_sandbox_setting(),
        external_message_pump: 1,
        windowless_rendering_enabled: 1,
        remote_debugging_port: remote_debugging_port(),
        ..Settings::default()
    };

    let cache_root = configured_cache_root();
    settings.cache_path = CefString::from(cache_root.to_string_lossy().as_ref());
    settings.root_cache_path = settings.cache_path.clone();
    if let Ok(path) = env::var("TABOR_CEF_LOG_PATH") {
        let log_path = if path.is_empty() || path == "1" {
            super::logs_dir().join(format!("tabor-cef-{}.log", std::process::id()))
        } else {
            PathBuf::from(path)
        };
        settings.log_file = CefString::from(log_path.to_string_lossy().as_ref());
        settings.log_severity = LogSeverity::INFO;
    }

    let framework_dir_str = framework_dir.to_string_lossy();
    settings.framework_dir_path = CefString::from(framework_dir_str.as_ref());

    let resources_dir = framework_dir.join("Resources");
    if resources_dir.exists() {
        settings.resources_dir_path = CefString::from(resources_dir.to_string_lossy().as_ref());
    }

    let bundle_paths = bundle_paths();
    let subprocess_path = env::var("TABOR_CEF_SUBPROCESS_PATH")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            bundle_paths.as_ref().and_then(|paths| helper_subprocess_path(&paths.main_bundle))
        })
        .or_else(|| env::current_exe().ok());
    if let Some(subprocess_path) = subprocess_path {
        settings.browser_subprocess_path =
            CefString::from(subprocess_path.to_string_lossy().as_ref());
    }
    if let Some(bundle_path) = bundle_paths.as_ref().map(|paths| &paths.main_bundle) {
        settings.main_bundle_path = CefString::from(bundle_path.to_string_lossy().as_ref());
    }

    let ok = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );

    if ok != 1 {
        return Err(std::io::Error::other("CEF initialization failed").into());
    }

    CEF_RUNTIME.with(|cell| {
        *cell.borrow_mut() = Some(CefRuntime {
            _args: args,
            _framework_dir: framework_dir,
            _app: app,
            _sandbox: sandbox,
        });
    });
    CEF_INITIALIZED.store(true, Ordering::Relaxed);

    Ok(())
}

pub fn do_message_loop_work() {
    if is_initialized() {
        CEF_PUMP_EXECUTED.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last_run) = last_run_lock().lock() {
            *last_run = Some(Instant::now());
        }
        cef::do_message_loop_work();
    }
}

pub fn register_message_pump_notifier<F>(notifier: F)
where
    F: Fn(Duration) + Send + Sync + 'static,
{
    let _ = MESSAGE_PUMP_NOTIFIER.set(Arc::new(notifier));
}

pub fn record_message_pump_schedule(
    delay: Duration,
    coalesced: bool,
    hidden_throttle_active: bool,
) {
    let delay_ms = delay.as_millis().min(MAX_MESSAGE_PUMP_DELAY_MS as u128) as u64;
    CEF_PUMP_LAST_EFFECTIVE_DELAY_MS.store(delay_ms, Ordering::Relaxed);
    CEF_PUMP_HIDDEN_THROTTLE_ACTIVE.store(hidden_throttle_active, Ordering::Relaxed);
    if coalesced {
        CEF_PUMP_COALESCED.fetch_add(1, Ordering::Relaxed);
    }
}

fn maybe_initialize_sandbox(args: &Args) -> Option<cef::sandbox::Sandbox> {
    if !super::distribution_channel().is_mac_app_store() {
        return None;
    }

    let mut sandbox = cef::sandbox::Sandbox::new();
    sandbox.initialize(args.as_main_args());
    Some(sandbox)
}

pub fn cef_pump_metrics() -> CefPumpMetrics {
    let last_run_ms_ago = if let Ok(last_run) = last_run_lock().lock() {
        last_run.map(|instant| Instant::now().saturating_duration_since(instant).as_millis() as u64)
    } else {
        None
    };

    CefPumpMetrics {
        scheduled: CEF_PUMP_SCHEDULED.load(Ordering::Relaxed),
        executed: CEF_PUMP_EXECUTED.load(Ordering::Relaxed),
        coalesced: CEF_PUMP_COALESCED.load(Ordering::Relaxed),
        last_requested_delay_ms: decode_metric(
            CEF_PUMP_LAST_REQUESTED_DELAY_MS.load(Ordering::Relaxed),
        ),
        last_effective_delay_ms: decode_metric(
            CEF_PUMP_LAST_EFFECTIVE_DELAY_MS.load(Ordering::Relaxed),
        ),
        last_run_ms_ago,
        hidden_throttle_active: CEF_PUMP_HIDDEN_THROTTLE_ACTIVE.load(Ordering::Relaxed),
    }
}

pub fn shutdown() {
    if is_initialized() {
        cef::shutdown();
        CEF_RUNTIME.with(|cell| {
            *cell.borrow_mut() = None;
        });
        CEF_INITIALIZED.store(false, Ordering::Relaxed);
        CEF_PUMP_LAST_REQUESTED_DELAY_MS.store(METRICS_NONE, Ordering::Relaxed);
        CEF_PUMP_LAST_EFFECTIVE_DELAY_MS.store(METRICS_NONE, Ordering::Relaxed);
        CEF_PUMP_HIDDEN_THROTTLE_ACTIVE.store(false, Ordering::Relaxed);
        if let Ok(mut last_run) = last_run_lock().lock() {
            *last_run = None;
        }
    }
}

pub fn is_initialized_global() -> bool {
    CEF_INITIALIZED.load(Ordering::Relaxed)
}

pub(crate) fn framework_dir() -> Option<PathBuf> {
    if let Ok(path) = env::var("TABOR_CEF_FRAMEWORK_DIR") {
        if let Some(framework) = resolve_framework_dir(Path::new(&path)) {
            return Some(framework);
        }
    }

    if let Ok(path) = env::var("TABOR_CEF_PATH").or_else(|_| env::var("CEF_PATH")) {
        if let Some(framework) = resolve_framework_dir(Path::new(&path)) {
            return Some(framework);
        }
    }

    // When running from a macOS `.app`, prefer a bundled framework so web tabs work even when
    // launched outside a shell environment (Finder/Spotlight/open).
    if let Some(bundle_root) = main_bundle_path() {
        let frameworks_dir = bundle_root.join("Contents").join("Frameworks");
        if let Some(framework) = resolve_framework_dir(&frameworks_dir) {
            return Some(framework);
        }
    }

    let arch_tag = if env::consts::ARCH == "aarch64" { "macosarm64" } else { "macosx64" };

    let vendor_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("vendor").join("cef");
    if !vendor_root.exists() {
        return None;
    }

    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&vendor_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.contains(arch_tag) {
                continue;
            }
            if let Some(framework) = resolve_framework_dir(&path) {
                candidates.push(framework);
            }
        }
    }

    candidates.sort();
    candidates.pop()
}

fn resolve_framework_dir(path: &Path) -> Option<PathBuf> {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "Chromium Embedded Framework.framework")
    {
        return path.canonicalize().ok();
    }

    let release = path.join("Release").join("Chromium Embedded Framework.framework");
    if release.exists() {
        return release.canonicalize().ok();
    }

    let debug = path.join("Debug").join("Chromium Embedded Framework.framework");
    if debug.exists() {
        return debug.canonicalize().ok();
    }

    // Some CEF distributions include a framework at the root as well as in Release/Debug. Prefer
    // Release/Debug above, since those contain sidecar libraries in `.../Libraries/`.
    let direct = path.join("Chromium Embedded Framework.framework");
    if direct.exists() {
        return direct.canonicalize().ok();
    }

    None
}

fn load_library(framework_dir: &Path) -> Result<(), Box<dyn Error>> {
    let framework_lib = framework_dir.join("Chromium Embedded Framework");
    let framework_lib = framework_lib.canonicalize()?;
    let Ok(path) = CString::new(framework_lib.as_os_str().as_bytes()) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CEF framework path contains null byte",
        )
        .into());
    };

    let loaded = CEF_LIBRARY_LOADED.with(|flag| {
        if flag.get() {
            return true;
        }
        let ptr = path.as_ptr();
        let result = unsafe { cef::load_library(Some(&*ptr)) } == 1;
        if result {
            flag.set(true);
        }
        result
    });

    if !loaded {
        return Err(std::io::Error::other("Failed to load CEF framework").into());
    }

    let api_hash = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let api_hash = if api_hash.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(api_hash) }.to_str().unwrap_or("")
    };
    if api_hash.is_empty() {
        return Err(
            std::io::Error::other("CEF API hash mismatch; check CEF framework version").into()
        );
    }

    debug!("CEF framework loaded from {}", framework_lib.display());
    Ok(())
}

fn ensure_cef_sidecar_libs(framework_dir: &Path) -> Result<(), Box<dyn Error>> {
    let exe_path = env::current_exe().ok().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "Failed to resolve executable path")
    })?;
    let exe_dir = exe_path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "Failed to resolve executable path")
    })?;

    let libs = ["libGLESv2.dylib", "libEGL.dylib"];
    let bundle_paths = bundle_paths();

    if let Some(paths) = bundle_paths.as_ref() {
        let frameworks_dir = paths.current_bundle.join("Contents").join("Frameworks");

        for lib in libs {
            let bundled = frameworks_dir.join(lib);
            if !bundled.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "Missing bundled CEF sidecar library {} in {}",
                        lib,
                        frameworks_dir.display()
                    ),
                )
                .into());
            }

            let link_path = exe_dir.join(lib);
            let metadata = fs::symlink_metadata(&link_path).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "Missing bundled sidecar link {} -> ../Frameworks/{}",
                        link_path.display(),
                        lib
                    ),
                )
            })?;
            if !metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Invalid bundled sidecar link {}: expected symlink to ../Frameworks/{}",
                        link_path.display(),
                        lib
                    ),
                )
                .into());
            }

            let expected = PathBuf::from("..").join("Frameworks").join(lib);
            let target = fs::read_link(&link_path)?;
            if target != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Invalid bundled sidecar link {} -> {} (expected {})",
                        link_path.display(),
                        target.display(),
                        expected.display()
                    ),
                )
                .into());
            }
        }

        return Ok(());
    }

    fs::create_dir_all(exe_dir)?;

    let src_dir = find_cef_sidecar_dir(framework_dir, &libs).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Missing CEF sidecar libraries ({}). Check CEF distribution near {}",
                libs.join(", "),
                framework_dir.display()
            ),
        )
    })?;

    for lib in libs {
        let dst = exe_dir.join(lib);
        if dst.exists() {
            continue;
        }

        let src = src_dir.join(lib);
        if !src.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Missing CEF sidecar library {} in {}", lib, src_dir.display()),
            )
            .into());
        }

        fs::copy(&src, &dst)?;
    }

    Ok(())
}

fn find_cef_sidecar_dir(framework_dir: &Path, libs: &[&str]) -> Option<PathBuf> {
    let candidates = [
        framework_dir.parent().map(|path| path.to_path_buf()),
        Some(framework_dir.join("Libraries")),
        framework_dir.parent().and_then(|path| path.parent()).map(|path| path.to_path_buf()),
        Some(framework_dir.to_path_buf()),
    ];

    candidates
        .into_iter()
        .flatten()
        .find(|candidate| libs.iter().all(|lib| candidate.join(lib).exists()))
}

pub fn is_initialized() -> bool {
    CEF_RUNTIME.with(|cell| cell.borrow().is_some())
}

fn bundle_paths() -> Option<BundlePaths> {
    let exe = env::current_exe().ok()?;
    bundle_paths_for_exe(&exe)
}

fn bundle_paths_for_exe(exe: &Path) -> Option<BundlePaths> {
    let contents_dir = exe.parent()?.parent()?;
    if contents_dir.file_name() != Some(OsStr::new("Contents")) {
        return None;
    }

    let current_bundle = contents_dir.parent()?.to_path_buf();
    if current_bundle.extension().and_then(|ext| ext.to_str()) != Some("app") {
        return None;
    }

    let main_bundle =
        helper_parent_main_bundle(&current_bundle).unwrap_or_else(|| current_bundle.clone());

    Some(BundlePaths { current_bundle, main_bundle })
}

fn helper_parent_main_bundle(current_bundle: &Path) -> Option<PathBuf> {
    let frameworks_dir = current_bundle.parent()?;
    if frameworks_dir.file_name() != Some(OsStr::new("Frameworks")) {
        return None;
    }

    let contents_dir = frameworks_dir.parent()?;
    if contents_dir.file_name() != Some(OsStr::new("Contents")) {
        return None;
    }

    let main_bundle = contents_dir.parent()?;
    if main_bundle.extension().and_then(|ext| ext.to_str()) != Some("app") {
        return None;
    }

    Some(main_bundle.to_path_buf())
}

fn main_bundle_path() -> Option<PathBuf> {
    bundle_paths().map(|paths| paths.main_bundle)
}

fn helper_subprocess_path(main_bundle: &Path) -> Option<PathBuf> {
    let frameworks_dir = main_bundle.join("Contents").join("Frameworks");
    CEF_HELPER_NAMES
        .iter()
        .map(|name| {
            frameworks_dir.join(format!("{name}.app")).join("Contents").join("MacOS").join(name)
        })
        .find(|path| path.exists())
}

fn remote_debugging_port() -> i32 {
    let port = env::var("TABOR_CDP_PORT")
        .or_else(|_| env::var("TABOR_CEF_REMOTE_DEBUGGING_PORT"))
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0);

    if port < 0 { 0 } else { port }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos::test_support::{EnvVarGuard, env_lock};

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn non_passkey_disable_features_cover_webauthn_and_bluetooth() {
        let parts = DISABLE_FEATURES.split(',').collect::<Vec<_>>();
        for required in [
            "WebAuthentication",
            "WebAuthenticationAPI",
            "ContentWebAuthenticationAPI",
            "WebBluetooth",
            "ContentWebBluetooth",
            "WebBluetoothNewPermissionsBackend",
        ] {
            assert!(parts.contains(&required), "missing disable feature: {required}");
        }
    }

    #[cfg(not(feature = "passkey-webauthn"))]
    #[test]
    fn non_passkey_disable_blink_features_cover_webauthn_and_bluetooth() {
        let parts = DISABLE_BLINK_FEATURES.split(',').collect::<Vec<_>>();
        assert!(parts.contains(&"WebAuthentication"));
        assert!(parts.contains(&"WebBluetooth"));
    }

    #[cfg(feature = "passkey-webauthn")]
    #[test]
    fn passkey_build_keeps_only_occlusion_feature_disable() {
        assert_eq!(DISABLE_FEATURES, "CalculateNativeWinOcclusion");
    }

    #[test]
    fn fake_media_env_zero_is_disabled() {
        assert!(!env_flag_enabled(Some(OsStr::new("0"))));
    }

    #[test]
    fn fake_media_env_nonzero_is_enabled() {
        assert!(env_flag_enabled(Some(OsStr::new("1"))));
    }

    #[test]
    fn mac_app_store_distribution_enables_cef_sandbox() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "mac_app_store");
        assert_eq!(cef_no_sandbox_setting(), 0);
    }

    #[test]
    fn configured_cache_root_prefers_explicit_env_override() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let custom_root = "/tmp/custom-cef-profile";
        let _cache_path = EnvVarGuard::set("TABOR_CEF_CACHE_PATH", custom_root);

        assert_eq!(configured_cache_root(), PathBuf::from(custom_root));
    }

    #[test]
    fn configured_cache_root_defaults_to_stable_direct_profile_dir() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let home_dir = temp_dir.path().join("home");
        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());
        let _cache_override = EnvVarGuard::unset("TABOR_CEF_CACHE_PATH");

        assert_eq!(
            configured_cache_root(),
            home_dir.join("Library").join("Application Support").join("Tabor").join("cef")
        );
    }
}
