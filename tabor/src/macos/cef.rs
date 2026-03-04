use std::cell::{Cell, RefCell};
use std::env;
use std::error::Error;
use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use log::debug;

use objc2::runtime::Bool;
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::NSApplication;

use cef::{
    self, App, CefString, ImplApp, ImplCommandLine, LogSeverity, Settings, WrapApp, args::Args,
    rc::Rc,
};

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

            let disable_features = cef::CefString::from("disable-features");
            #[cfg(feature = "passkey-webauthn")]
            let disable_features_value = cef::CefString::from("CalculateNativeWinOcclusion");
            #[cfg(not(feature = "passkey-webauthn"))]
            let disable_features_value =
                cef::CefString::from("CalculateNativeWinOcclusion,WebAuthentication");
            command_line.append_switch_with_value(Some(&disable_features), Some(&disable_features_value));

            #[cfg(not(feature = "passkey-webauthn"))]
            {
                let disable_webauthn = cef::CefString::from("disable-webauthn");
                command_line.append_switch(Some(&disable_webauthn));
            }
        }
    }
}

struct CefRuntime {
    _args: Args,
    _framework_dir: PathBuf,
    _app: cef::App,
}

thread_local! {
    static CEF_RUNTIME: RefCell<Option<CefRuntime>> = const { RefCell::new(None) };
    static CEF_LIBRARY_LOADED: Cell<bool> = const { Cell::new(false) };
}

static CEF_INITIALIZED: AtomicBool = AtomicBool::new(false);

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
    let mtm = MainThreadMarker::new().ok_or_else(|| {
        io::Error::other("CEF application contract check must run on the main thread")
    })?;

    let app = NSApplication::sharedApplication(mtm);
    let responds_is: Bool =
        unsafe { msg_send![&*app, respondsToSelector: sel!(isHandlingSendEvent)] };
    let responds_set: Bool =
        unsafe { msg_send![&*app, respondsToSelector: sel!(setHandlingSendEvent:)] };

    if responds_is.as_bool() && responds_set.as_bool() {
        Ok(())
    } else {
        Err(io::Error::other(
            "CEF macOS app contract violated: NSApplication must implement isHandlingSendEvent/setHandlingSendEvent:",
        )
        .into())
    }
}

pub fn maybe_execute_subprocess() -> Result<Option<i32>, Box<dyn Error>> {
    let Some(framework_dir) = framework_dir() else {
        return Ok(None);
    };

    ensure_application_selector_contract()?;

    load_library(&framework_dir)?;
    ensure_cef_sidecar_libs(&framework_dir)?;

    let args = Args::new();
    let mut app = TaborCefApp::new();
    let exit_code =
        cef::execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());

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
    let mut app = TaborCefApp::new();
    let mut settings = Settings {
        no_sandbox: 1,
        external_message_pump: 1,
        remote_debugging_port: remote_debugging_port(),
        ..Settings::default()
    };

    let cache_root = env::var("TABOR_CEF_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir().join(format!("tabor-cef-{}", std::process::id())));
    settings.cache_path = CefString::from(cache_root.to_string_lossy().as_ref());
    settings.root_cache_path = settings.cache_path.clone();
    if let Ok(path) = env::var("TABOR_CEF_LOG_PATH") {
        let log_path = if path.is_empty() || path == "1" {
            env::temp_dir().join(format!("tabor-cef-{}.log", std::process::id()))
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
        *cell.borrow_mut() =
            Some(CefRuntime { _args: args, _framework_dir: framework_dir, _app: app });
    });
    CEF_INITIALIZED.store(true, Ordering::Relaxed);

    Ok(())
}

pub fn do_message_loop_work() {
    if is_initialized() {
        cef::do_message_loop_work();
    }
}

pub fn shutdown() {
    if is_initialized() {
        cef::shutdown();
        CEF_RUNTIME.with(|cell| {
            *cell.borrow_mut() = None;
        });
        CEF_INITIALIZED.store(false, Ordering::Relaxed);
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
