use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::error::Error;
use std::ffi::CStr;
use std::ffi::CString;
use std::fmt::Write;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[cfg(feature = "passkey-webauthn")]
use block2::RcBlock;
use objc2::encode::{Encode, Encoding};
use objc2::ffi;
#[cfg(feature = "passkey-webauthn")]
use objc2::ffi::NSInteger;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, ProtocolObject, Sel};
use objc2::{MainThreadMarker, Message};
use objc2::{msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSButton, NSTextField, NSView, NSWindow,
};
use objc2_foundation::{
    NSActivityOptions, NSDictionary, NSObjectProtocol, NSProcessInfo, NSString, NSUserDefaults,
    ns_string,
};
use plist::Value;

#[cfg(feature = "passkey-webauthn")]
#[link(name = "AuthenticationServices", kind = "framework")]
unsafe extern "C" {}

pub mod cef;
pub mod favicon;
pub mod image_view;
pub(crate) mod keycodes;
pub mod locale;
pub mod open_documents;
pub mod open_url;
pub mod pdf_view;
pub mod proc;
pub mod web_commands;
pub mod web_cursor;
pub mod webview;
mod webview_cef;

pub(crate) use open_documents::register_open_documents_handler;
use webview::WebFrameDeliveryMode;

const DEFAULT_BUNDLE_IDENTIFIER: &str = "com.pinkbot.tabor";
const DISTRIBUTION_CHANNEL_KEY: &str = "TABORDistributionChannel";
const DISTRIBUTION_CHANNEL_ENV: &str = "TABOR_DISTRIBUTION_CHANNEL";
const BUNDLE_IDENTIFIER_ENV: &str = "TABOR_BUNDLE_IDENTIFIER";
const CONTAINER_DATA_DIR_ENV: &str = "TABOR_CONTAINER_DATA_DIR";
const APP_SUPPORT_DIR_NAME: &str = "Tabor";
const ALLOW_UNSUPPORTED_GUI_LAUNCH_ENV: &str = "TABOR_ALLOW_UNSUPPORTED_GUI_LAUNCH";

static WEBVIEW_COUNT: AtomicUsize = AtomicUsize::new(0);
static WEBVIEW_CREATED_TOTAL: AtomicU64 = AtomicU64::new(0);
static WEBVIEW_DROPPED_TOTAL: AtomicU64 = AtomicU64::new(0);
static WEBVIEW_ACCELERATED_FRAMES_TOTAL: AtomicU64 = AtomicU64::new(0);
static WEBVIEW_EXTERNAL_BEGIN_FRAMES_TOTAL: AtomicU64 = AtomicU64::new(0);
static WEBVIEW_ACCELERATED_STARTUP_FAILURES_TOTAL: AtomicU64 = AtomicU64::new(0);
static WEBVIEW_UNEXPECTED_CPU_PAINTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static WEBVIEW_LIVE_ACCELERATED_SURFACES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "passkey-webauthn")]
static PASSKEY_AUTH_REQUESTED: AtomicBool = AtomicBool::new(false);
thread_local! {
    #[cfg(feature = "passkey-webauthn")]
    static PASSKEY_AUTH_BLOCK: RefCell<Option<RcBlock<dyn Fn(NSInteger)>>> = RefCell::new(None);
    static APP_NAP_ACTIVITY: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        RefCell::new(None);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WebViewMetrics {
    pub live: usize,
    pub created: u64,
    pub dropped: u64,
    pub accelerated_frames: u64,
    pub frame_delivery_mode: WebFrameDeliveryMode,
    pub external_begin_frames: u64,
    pub accelerated_startup_failures: u64,
    pub unexpected_cpu_paints: u64,
    pub live_accelerated_surfaces: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DistributionChannel {
    #[default]
    Direct,
    MacAppStore,
}

impl DistributionChannel {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "direct" => Some(Self::Direct),
            "mac_app_store" => Some(Self::MacAppStore),
            _ => None,
        }
    }

    pub(crate) fn is_mac_app_store(self) -> bool {
        matches!(self, Self::MacAppStore)
    }
}

pub(crate) fn distribution_channel() -> DistributionChannel {
    env::var(DISTRIBUTION_CHANNEL_ENV)
        .ok()
        .and_then(|value| DistributionChannel::from_str(value.trim()))
        .or_else(|| {
            info_plist_string(DISTRIBUTION_CHANNEL_KEY)
                .as_deref()
                .and_then(DistributionChannel::from_str)
        })
        .unwrap_or_default()
}

pub(crate) fn bundle_identifier() -> String {
    env::var(BUNDLE_IDENTIFIER_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| info_plist_string("CFBundleIdentifier"))
        .unwrap_or_else(|| String::from(DEFAULT_BUNDLE_IDENTIFIER))
}

pub(crate) fn container_data_dir() -> PathBuf {
    if let Ok(path) = env::var(CONTAINER_DATA_DIR_ENV) {
        return ensure_directory(PathBuf::from(path), "app container override");
    }

    let home_dir = home::home_dir().unwrap_or_else(|| {
        panic!("unable to resolve home directory for bundle {}", bundle_identifier())
    });
    ensure_directory(
        home_dir.join("Library").join("Containers").join(bundle_identifier()).join("Data"),
        "app container data directory",
    )
}
fn direct_runtime_tmp_root() -> PathBuf {
    darwin_user_temp_dir().unwrap_or_else(env::temp_dir)
}

fn darwin_user_temp_dir() -> Option<PathBuf> {
    unsafe {
        let len = libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0);
        if len == 0 {
            return None;
        }

        let mut buffer = vec![0u8; len as usize];
        let written =
            libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buffer.as_mut_ptr().cast(), len);
        if written == 0 {
            return None;
        }

        let path = CStr::from_ptr(buffer.as_ptr().cast());
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
    }
}

pub(crate) fn runtime_tmp_dir() -> PathBuf {
    if let Some(path) = test_bundle_root() {
        return ensure_directory(path.join("tmp"), "test bundle runtime temporary directory");
    }

    if distribution_channel().is_mac_app_store() {
        ensure_directory(container_data_dir().join("tmp"), "runtime temporary directory")
    } else {
        ensure_directory(direct_runtime_tmp_root(), "runtime temporary directory")
    }
}

pub(crate) fn logs_dir() -> PathBuf {
    if let Some(path) = test_bundle_root() {
        return ensure_directory(path.join("logs"), "test bundle log directory");
    }

    if distribution_channel().is_mac_app_store() {
        ensure_directory(
            container_data_dir().join("Library").join("Logs").join("Tabor"),
            "log directory",
        )
    } else {
        ensure_directory(direct_runtime_tmp_root(), "log directory")
    }
}

pub(crate) fn direct_app_support_dir() -> PathBuf {
    if let Some(path) = test_bundle_root() {
        return ensure_directory(path.join("app-support"), "test bundle app support directory");
    }

    let home_dir = home::home_dir().unwrap_or_else(|| {
        panic!("unable to resolve home directory for bundle {}", bundle_identifier())
    });
    ensure_directory(
        home_dir.join("Library").join("Application Support").join(APP_SUPPORT_DIR_NAME),
        "app support directory",
    )
}

pub(crate) fn cef_cache_dir() -> PathBuf {
    if distribution_channel().is_mac_app_store() {
        ensure_directory(
            container_data_dir().join("Library").join("Caches").join("Tabor").join("cef"),
            "CEF cache directory",
        )
    } else {
        ensure_directory(direct_app_support_dir().join("cef"), "CEF cache directory")
    }
}

fn test_bundle_root() -> Option<PathBuf> {
    let bundle_id = bundle_identifier();
    if !bundle_id.starts_with("com.pinkbot.tabor.test.") {
        return None;
    }

    if let Some(path) = env::var_os("TABOR_TEST_STATE_ROOT") {
        return Some(PathBuf::from(path));
    }

    let mut hasher = DefaultHasher::new();
    bundle_id.hash(&mut hasher);
    Some(env::temp_dir().join(format!("tabor-test-{:016x}", hasher.finish())))
}

pub(crate) fn default_download_dir() -> PathBuf {
    let base_dir = home::home_dir()
        .map(|path| path.join("Downloads"))
        .unwrap_or_else(|| runtime_tmp_dir().join("Downloads"));
    ensure_directory(base_dir, "download directory")
}

pub(crate) fn should_show_download_dialog() -> bool {
    !bundle_identifier().starts_with("com.pinkbot.tabor.test.")
}

pub(crate) fn press_test_js_dialog_button(
    button_title: &str,
    prompt_text: Option<&str>,
) -> Result<(), String> {
    if !bundle_identifier().starts_with("com.pinkbot.tabor.test.") {
        return Err(String::from(
            "JavaScript dialog debug actions require a com.pinkbot.tabor.test.* bundle",
        ));
    }

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| String::from("JavaScript dialog debug actions require the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    if let Some(window) = app.modalWindow() {
        if press_js_dialog_button_in_window(&window, button_title, prompt_text)? {
            return Ok(());
        }
    }

    for window in app.windows() {
        if window.sheetParent().is_some()
            && press_js_dialog_button_in_window(&window, button_title, prompt_text)?
        {
            return Ok(());
        }
    }

    Err(format!("No active JavaScript dialog has a {button_title:?} button"))
}

fn press_js_dialog_button_in_window(
    window: &NSWindow,
    button_title: &str,
    prompt_text: Option<&str>,
) -> Result<bool, String> {
    let Some(content_view) = window.contentView() else {
        return Ok(false);
    };
    let Some(button) = find_button_with_title(&content_view, button_title) else {
        return Ok(false);
    };

    if let Some(prompt_text) = prompt_text {
        let field = find_editable_text_field(&content_view)
            .ok_or_else(|| String::from("Active JavaScript dialog has no editable prompt field"))?;
        field.setStringValue(&NSString::from_str(prompt_text));
    }

    // SAFETY: The button belongs to the active test-only NSAlert and nil is a valid sender.
    unsafe { button.performClick(None) };
    Ok(true)
}

fn find_button_with_title(view: &NSView, title: &str) -> Option<Retained<NSButton>> {
    let object: &AnyObject = view;
    if let Some(button) = object.downcast_ref::<NSButton>() {
        if button.title().to_string() == title {
            return Some(button.retain());
        }
    }

    for subview in view.subviews() {
        if let Some(button) = find_button_with_title(&subview, title) {
            return Some(button);
        }
    }
    None
}

fn find_editable_text_field(view: &NSView) -> Option<Retained<NSTextField>> {
    let object: &AnyObject = view;
    if let Some(field) = object.downcast_ref::<NSTextField>() {
        if field.isEditable() {
            return Some(field.retain());
        }
    }

    for subview in view.subviews() {
        if let Some(field) = find_editable_text_field(&subview) {
            return Some(field);
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::env;
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};

    pub(crate) fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    pub(crate) struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        pub(crate) fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var_os(key);
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }

        pub(crate) fn unset(key: &'static str) -> Self {
            let previous = env::var_os(key);
            unsafe { env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                unsafe { env::set_var(self.key, previous) };
            } else {
                unsafe { env::remove_var(self.key) };
            }
        }
    }
}

pub(crate) fn preferred_working_dir() -> PathBuf {
    if distribution_channel().is_mac_app_store() {
        container_data_dir()
    } else {
        home::home_dir().expect("unable to resolve home directory for Tabor")
    }
}

pub(crate) fn webview_metrics() -> WebViewMetrics {
    WebViewMetrics {
        live: WEBVIEW_COUNT.load(Ordering::SeqCst),
        created: WEBVIEW_CREATED_TOTAL.load(Ordering::SeqCst),
        dropped: WEBVIEW_DROPPED_TOTAL.load(Ordering::SeqCst),
        accelerated_frames: WEBVIEW_ACCELERATED_FRAMES_TOTAL.load(Ordering::SeqCst),
        frame_delivery_mode: WebFrameDeliveryMode::CefInternal,
        external_begin_frames: WEBVIEW_EXTERNAL_BEGIN_FRAMES_TOTAL.load(Ordering::SeqCst),
        accelerated_startup_failures: WEBVIEW_ACCELERATED_STARTUP_FAILURES_TOTAL
            .load(Ordering::SeqCst),
        unexpected_cpu_paints: WEBVIEW_UNEXPECTED_CPU_PAINTS_TOTAL.load(Ordering::SeqCst),
        live_accelerated_surfaces: WEBVIEW_LIVE_ACCELERATED_SURFACES.load(Ordering::SeqCst),
    }
}

static CEF_HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);
const SUPPORTED_GUI_PARENT_PROCESSES: &[&str] =
    &["launchd", "xpcproxy", "open", "loginwindow", "dock", "finder"];

pub(crate) fn cef_handling_send_event() -> bool {
    CEF_HANDLING_SEND_EVENT.load(Ordering::Relaxed)
}

pub fn ensure_cef_application() -> Result<(), Box<dyn Error>> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("CEF application setup must run on main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    install_cef_application_contract(app.class());

    let responds_is: Bool =
        unsafe { msg_send![&*app, respondsToSelector: sel!(isHandlingSendEvent)] };
    let responds_set: Bool =
        unsafe { msg_send![&*app, respondsToSelector: sel!(setHandlingSendEvent:)] };

    if responds_is.as_bool() && responds_set.as_bool() {
        Ok(())
    } else {
        Err(io::Error::other(
            "CEF application contract not satisfied: NSApplication missing isHandlingSendEvent/setHandlingSendEvent:",
        )
        .into())
    }
}

pub fn enforce_signed_app_launch() -> Result<(), Box<dyn Error>> {
    let exe_path = std::env::current_exe()?;
    let app_bundle = enclosing_app_bundle(&exe_path).ok_or_else(|| {
        format!(
            "Refusing to launch Tabor outside a signed macOS app bundle. Current executable: {}",
            exe_path.display()
        )
    })?;

    let mut verify = Command::new("codesign");
    verify.args(["--verify", "--deep", "--strict"]).arg(&app_bundle);
    let verify_status = verify.status()?;
    if !verify_status.success() {
        return Err(format!("codesign verification failed for {}", app_bundle.display()).into());
    }

    let inspect = Command::new("codesign").arg("-dvv").arg(&app_bundle).output()?;
    if !inspect.status.success() {
        return Err(format!("codesign inspection failed for {}", app_bundle.display()).into());
    }

    let inspect_text = String::from_utf8_lossy(&inspect.stderr);
    let team_identifier =
        inspect_text.lines().find_map(|line| line.strip_prefix("TeamIdentifier=")).unwrap_or("");
    if team_identifier.is_empty() || team_identifier == "not set" {
        return Err(format!(
            "Refusing to launch unsigned or ad-hoc-signed Tabor bundle {}",
            app_bundle.display()
        )
        .into());
    }

    Ok(())
}

pub fn enforce_supported_gui_launch_context() -> Result<(), Box<dyn Error>> {
    let parent_pid = unsafe { libc::getppid() };
    if parent_pid <= 1 {
        return Ok(());
    }

    let parent_path = proc::executable_path(parent_pid)?;
    let parent_name = parent_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("unable to resolve macOS GUI parent process name"))?;

    if is_supported_gui_parent_process(parent_name) {
        return Ok(());
    }

    if env::var_os(ALLOW_UNSUPPORTED_GUI_LAUNCH_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
        && bundle_identifier().starts_with("com.pinkbot.tabor.test.")
    {
        return Ok(());
    }

    let current_executable = env::current_exe()?;
    Err(format!(
        "Unsupported macOS GUI launch context: {} was executed directly by {}. Launch Tabor via LaunchServices instead, for example `open -a /Applications/Tabor.app`. Direct execution remains supported for CLI commands such as `tabor msg`, `tabor agent`, and `tabor workspace`.",
        current_executable.display(),
        parent_path.display(),
    )
    .into())
}

fn enclosing_app_bundle(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

fn current_info_plist() -> Option<PathBuf> {
    let current_executable = env::current_exe().ok()?;
    enclosing_app_bundle(&current_executable)
        .map(|bundle| bundle.join("Contents").join("Info.plist"))
}

fn info_plist_string(key: &str) -> Option<String> {
    let info_plist = current_info_plist()?;
    let plist = Value::from_file(&info_plist).ok()?;
    let dict = plist.as_dictionary()?;
    dict.get(key)?.as_string().map(ToOwned::to_owned)
}

fn ensure_directory(path: PathBuf, label: &str) -> PathBuf {
    fs::create_dir_all(&path)
        .unwrap_or_else(|err| panic!("unable to create {label} {}: {err}", path.display()));
    path
}

unsafe extern "C-unwind" fn cef_app_is_handling_send_event(_this: &AnyObject, _sel: Sel) -> Bool {
    if CEF_HANDLING_SEND_EVENT.load(Ordering::Relaxed) { Bool::YES } else { Bool::NO }
}

unsafe extern "C-unwind" fn cef_app_set_handling_send_event(
    _this: &AnyObject,
    _sel: Sel,
    handling_send_event: Bool,
) {
    CEF_HANDLING_SEND_EVENT.store(handling_send_event.as_bool(), Ordering::Relaxed);
}

fn install_cef_application_contract(class: &AnyClass) {
    unsafe {
        add_cef_method_if_missing(
            class,
            sel!(isHandlingSendEvent),
            mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel) -> Bool, Imp>(
                cef_app_is_handling_send_event,
            ),
            Bool::ENCODING,
            &[],
        );
        add_cef_method_if_missing(
            class,
            sel!(setHandlingSendEvent:),
            mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel, Bool), Imp>(
                cef_app_set_handling_send_event,
            ),
            Encoding::Void,
            &[Bool::ENCODING],
        );
    }
}

unsafe fn add_cef_method_if_missing(
    class: &AnyClass,
    selector: Sel,
    imp: Imp,
    ret: Encoding,
    args: &[Encoding],
) {
    if class.instance_method(selector).is_some() {
        return;
    }

    let encoding = method_type_encoding(ret, args);
    let class_ptr = class as *const AnyClass as *mut AnyClass;
    let success = unsafe { ffi::class_addMethod(class_ptr, selector, imp, encoding.as_ptr()) };
    assert!(success.as_bool(), "failed to add CEF application method");
}

fn is_supported_gui_parent_process(parent_name: &str) -> bool {
    SUPPORTED_GUI_PARENT_PROCESSES
        .iter()
        .any(|candidate| parent_name.eq_ignore_ascii_case(candidate))
}

fn method_type_encoding(ret: Encoding, args: &[Encoding]) -> CString {
    let mut types = format!("{ret}{}{}", Encoding::Object, Encoding::Sel);
    for enc in args {
        let _ = write!(&mut types, "{enc}");
    }
    CString::new(types).expect("method type encoding")
}
pub fn disable_autofill() {
    unsafe {
        NSUserDefaults::standardUserDefaults().registerDefaults(
            &NSDictionary::<NSString, AnyObject>::from_slices(
                &[ns_string!("NSAutoFillHeuristicControllerEnabled")],
                &[ns_string!("NO")],
            ),
        );
    }
    NSUserDefaults::standardUserDefaults()
        .removeObjectForKey(ns_string!("NSAutoFillHeuristicControllerEnabled"));
}

pub fn disable_app_nap() {
    let _mtm = match MainThreadMarker::new() {
        Some(mtm) => mtm,
        None => return,
    };

    APP_NAP_ACTIVITY.with(|cell| {
        if cell.borrow().is_some() {
            return;
        }

        let process_info = NSProcessInfo::processInfo();
        let reason = NSString::from_str("Tabor background activity");
        let activity = process_info.beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
            &reason,
        );
        *cell.borrow_mut() = Some(activity);
    });
}

pub fn set_background_activation() {
    if std::env::var("TABOR_BACKGROUND").is_err() {
        return;
    }

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

pub(crate) fn register_webview() {
    let prev = WEBVIEW_COUNT.fetch_add(1, Ordering::SeqCst);
    WEBVIEW_CREATED_TOTAL.fetch_add(1, Ordering::SeqCst);
    if prev == 0 {
        set_autofill_override(true);
        #[cfg(feature = "passkey-webauthn")]
        request_passkey_authorization();
    }
}

pub(crate) fn unregister_webview() {
    let prev = WEBVIEW_COUNT
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
            if count == 0 { None } else { Some(count - 1) }
        })
        .expect("WebView autofill counter underflow");
    WEBVIEW_DROPPED_TOTAL.fetch_add(1, Ordering::SeqCst);

    if prev == 1 {
        set_autofill_override(false);
    }
}

pub(crate) fn record_accelerated_frame() {
    WEBVIEW_ACCELERATED_FRAMES_TOTAL.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn record_accelerated_startup_failure() {
    WEBVIEW_ACCELERATED_STARTUP_FAILURES_TOTAL.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn record_unexpected_cpu_paint() {
    WEBVIEW_UNEXPECTED_CPU_PAINTS_TOTAL.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn register_accelerated_surface() {
    WEBVIEW_LIVE_ACCELERATED_SURFACES.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn unregister_accelerated_surface() {
    WEBVIEW_LIVE_ACCELERATED_SURFACES
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| count.checked_sub(1))
        .expect("WebView accelerated surface counter underflow");
}

fn set_autofill_override(enabled: bool) {
    let defaults = NSUserDefaults::standardUserDefaults();
    if enabled {
        defaults.setBool_forKey(true, ns_string!("NSAutoFillHeuristicControllerEnabled"));
    } else {
        defaults.removeObjectForKey(ns_string!("NSAutoFillHeuristicControllerEnabled"));
    }
}

#[cfg(feature = "passkey-webauthn")]
fn request_passkey_authorization() {
    if PASSKEY_AUTH_REQUESTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let _mtm = match MainThreadMarker::new() {
        Some(mtm) => mtm,
        None => return,
    };

    let class_name =
        CStr::from_bytes_with_nul(b"ASAuthorizationWebBrowserPublicKeyCredentialManager\0")
            .expect("static CStr");
    let Some(manager_class) = AnyClass::get(class_name) else {
        return;
    };

    let manager: *mut AnyObject = unsafe { msg_send![manager_class, new] };
    let Some(manager) = (unsafe { Retained::from_raw(manager) }) else {
        return;
    };

    let request_sel = sel!(requestAuthorizationForPublicKeyCredentials:);
    let responds: Bool = unsafe { msg_send![&*manager, respondsToSelector: request_sel] };
    if !responds.as_bool() {
        return;
    }

    let mut state: NSInteger = 2;
    let state_sel = sel!(authorizationStateForPlatformCredentials);
    let responds_state: Bool = unsafe { msg_send![&*manager, respondsToSelector: state_sel] };
    if responds_state.as_bool() {
        state = unsafe { msg_send![&*manager, authorizationStateForPlatformCredentials] };
    }

    if state != 2 {
        return;
    }

    let block = RcBlock::new(|_state: NSInteger| {});
    PASSKEY_AUTH_BLOCK.with(|cell| {
        *cell.borrow_mut() = Some(block.clone());
    });

    unsafe {
        let _: () = msg_send![&*manager, requestAuthorizationForPublicKeyCredentials: &*block];
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::test_support::{EnvVarGuard, env_lock};
    use super::*;

    #[test]
    fn mac_app_store_path_helpers_use_container_override() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let container = temp_dir.path().join("container-data");

        let _distribution = EnvVarGuard::set(DISTRIBUTION_CHANNEL_ENV, "mac_app_store");
        let _container = EnvVarGuard::set(CONTAINER_DATA_DIR_ENV, &container.display().to_string());
        let _bundle_id = EnvVarGuard::set(BUNDLE_IDENTIFIER_ENV, "com.pinkbot.tabor.test");

        assert_eq!(distribution_channel(), DistributionChannel::MacAppStore);
        assert_eq!(bundle_identifier(), "com.pinkbot.tabor.test");
        assert_eq!(container_data_dir(), container);
        assert_eq!(preferred_working_dir(), container);
        assert_eq!(runtime_tmp_dir(), container.join("tmp"));
        assert_eq!(logs_dir(), container.join("Library").join("Logs").join("Tabor"));
        assert_eq!(
            cef_cache_dir(),
            container.join("Library").join("Caches").join("Tabor").join("cef")
        );
    }

    #[test]
    fn direct_path_helpers_use_application_support_for_cef_profile_storage() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let home_dir = temp_dir.path().join("home");

        let _distribution = EnvVarGuard::set(DISTRIBUTION_CHANNEL_ENV, "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());

        let app_support_dir = home_dir.join("Library").join("Application Support").join("Tabor");
        assert_eq!(distribution_channel(), DistributionChannel::Direct);
        assert_eq!(direct_app_support_dir(), app_support_dir);
        assert_eq!(cef_cache_dir(), app_support_dir.join("cef"));
    }

    #[test]
    fn downloads_default_to_downloads_root() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let home_dir = temp_dir.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create home dir");

        let _distribution = EnvVarGuard::set(DISTRIBUTION_CHANNEL_ENV, "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());
        let _bundle_id = EnvVarGuard::set(BUNDLE_IDENTIFIER_ENV, DEFAULT_BUNDLE_IDENTIFIER);

        assert_eq!(default_download_dir(), home_dir.join("Downloads"));
    }

    #[test]
    fn download_dialog_is_enabled_outside_test_bundles() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");

        let production_bundle = EnvVarGuard::set(BUNDLE_IDENTIFIER_ENV, DEFAULT_BUNDLE_IDENTIFIER);
        assert!(should_show_download_dialog());
        drop(production_bundle);

        let _test_bundle =
            EnvVarGuard::set(BUNDLE_IDENTIFIER_ENV, "com.pinkbot.tabor.test.web-e2e");
        assert!(!should_show_download_dialog());
    }

    #[test]
    fn direct_runtime_paths_ignore_tmpdir_override() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let home_dir = temp_dir.path().join("home");
        let fake_tmpdir = temp_dir.path().join("nix-shell.fake");
        std::fs::create_dir_all(&home_dir).expect("create home dir");
        std::fs::create_dir_all(&fake_tmpdir).expect("create tmpdir");

        let _distribution = EnvVarGuard::set(DISTRIBUTION_CHANNEL_ENV, "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());
        let _tmpdir = EnvVarGuard::set("TMPDIR", &fake_tmpdir.display().to_string());

        let canonical_tmp = darwin_user_temp_dir().expect("darwin user temp dir");
        assert_eq!(runtime_tmp_dir(), canonical_tmp);
        assert_eq!(logs_dir(), canonical_tmp);
        assert_ne!(runtime_tmp_dir(), fake_tmpdir);
    }

    #[test]
    fn direct_test_bundle_paths_use_per_bundle_temp_root() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let home_dir = temp_dir.path().join("home");

        let _distribution = EnvVarGuard::set(DISTRIBUTION_CHANNEL_ENV, "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());
        let _bundle_id = EnvVarGuard::set(BUNDLE_IDENTIFIER_ENV, "com.pinkbot.tabor.test.tmpcase");

        let root = test_bundle_root().expect("expected test bundle root");
        assert_eq!(runtime_tmp_dir(), root.join("tmp"));
        assert_eq!(logs_dir(), root.join("logs"));
        assert_eq!(direct_app_support_dir(), root.join("app-support"));
        assert_eq!(cef_cache_dir(), root.join("app-support").join("cef"));
    }

    #[test]
    fn gui_parent_allowlist_accepts_launchservices_processes() {
        assert!(is_supported_gui_parent_process("launchd"));
        assert!(is_supported_gui_parent_process("xpcproxy"));
        assert!(is_supported_gui_parent_process("Dock"));
    }

    #[test]
    fn gui_parent_allowlist_rejects_direct_exec_callers() {
        assert!(!is_supported_gui_parent_process("codex"));
        assert!(!is_supported_gui_parent_process("zsh"));
        assert!(!is_supported_gui_parent_process("python3"));
    }
}
