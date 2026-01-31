use std::cell::{Cell, RefCell};
use std::env;
use std::error::Error;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};

use log::debug;

use cef::{
    self, args::Args, rc::Rc, App, CefString, ImplApp, ImplCommandLine, LogSeverity, Settings,
    WrapApp,
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
            let disable_features_value = cef::CefString::from("CalculateNativeWinOcclusion");
            command_line.append_switch_with_value(Some(&disable_features), Some(&disable_features_value));
        }
    }
}

struct CefRuntime {
    _args: Args,
    _framework_dir: PathBuf,
    _app: cef::App,
}

thread_local! {
    static CEF_RUNTIME: RefCell<Option<CefRuntime>> = RefCell::new(None);
    static CEF_LIBRARY_LOADED: Cell<bool> = Cell::new(false);
}

static CEF_INITIALIZED: AtomicBool = AtomicBool::new(false);

pub fn is_available() -> bool {
    framework_dir().is_some()
}

pub fn maybe_execute_subprocess() -> Result<Option<i32>, Box<dyn Error>> {
    let Some(framework_dir) = framework_dir() else {
        return Ok(None);
    };

    load_library(&framework_dir)?;

    let args = Args::new();
    let mut app = TaborCefApp::new();
    let exit_code = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );

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

    load_library(&framework_dir)?;

    let args = Args::new();
    let mut app = TaborCefApp::new();
    let mut settings = Settings::default();
    settings.no_sandbox = 1;
    settings.external_message_pump = 1;
    settings.remote_debugging_port = remote_debugging_port();

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

    let subprocess_path = env::var("TABOR_CEF_SUBPROCESS_PATH")
        .ok()
        .map(PathBuf::from)
        .or_else(|| env::current_exe().ok());
    if let Some(subprocess_path) = subprocess_path {
        settings.browser_subprocess_path =
            CefString::from(subprocess_path.to_string_lossy().as_ref());
    }
    if let Some(bundle_path) = main_bundle_path() {
        settings.main_bundle_path = CefString::from(bundle_path.to_string_lossy().as_ref());
    }

    let ok = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );

    if ok != 1 {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "CEF initialization failed").into());
    }

    CEF_RUNTIME.with(|cell| {
        *cell.borrow_mut() = Some(CefRuntime {
            _args: args,
            _framework_dir: framework_dir,
            _app: app,
        });
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

    let arch_tag = if env::consts::ARCH == "aarch64" {
        "macosarm64"
    } else {
        "macosx64"
    };

    let vendor_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vendor")
        .join("cef");
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

    let direct = path.join("Chromium Embedded Framework.framework");
    if direct.exists() {
        return direct.canonicalize().ok();
    }

    let release = path
        .join("Release")
        .join("Chromium Embedded Framework.framework");
    if release.exists() {
        return release.canonicalize().ok();
    }

    let debug = path
        .join("Debug")
        .join("Chromium Embedded Framework.framework");
    if debug.exists() {
        return debug.canonicalize().ok();
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to load CEF framework",
        )
        .into());
    }

    let api_hash = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let api_hash = if api_hash.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(api_hash) }.to_str().unwrap_or("")
    };
    if api_hash.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "CEF API hash mismatch; check CEF framework version",
        )
        .into());
    }

    debug!("CEF framework loaded from {}", framework_lib.display());
    Ok(())
}

pub fn is_initialized() -> bool {
    CEF_RUNTIME.with(|cell| cell.borrow().is_some())
}

fn main_bundle_path() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let bundle = exe.parent()?.parent()?.parent()?.to_path_buf();
    if bundle.extension().and_then(|ext| ext.to_str()) == Some("app") {
        Some(bundle)
    } else {
        None
    }
}

fn remote_debugging_port() -> i32 {
    let port = env::var("TABOR_CDP_PORT")
        .or_else(|_| env::var("TABOR_CEF_REMOTE_DEBUGGING_PORT"))
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0);

    if port < 0 { 0 } else { port }
}
