use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::fmt::Write;
use std::mem;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "passkey-webauthn")]
use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::encode::{Encode, Encoding};
use objc2::ffi;
#[cfg(feature = "passkey-webauthn")]
use objc2::ffi::NSInteger;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, ProtocolObject, Sel};
use objc2::{msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{
    NSActivityOptions, NSDictionary, NSObjectProtocol, NSProcessInfo, NSString, NSUserDefaults,
    ns_string,
};

#[cfg(feature = "passkey-webauthn")]
#[link(name = "AuthenticationServices", kind = "framework")]
unsafe extern "C" {}

pub mod cef;
pub mod favicon;
pub(crate) mod keycodes;
pub mod locale;
pub mod open_documents;
pub mod proc;
pub mod web_commands;
pub mod web_cursor;
pub mod webview;
mod webview_cef;

pub(crate) use open_documents::register_open_documents_handler;

static WEBVIEW_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "passkey-webauthn")]
static PASSKEY_AUTH_REQUESTED: AtomicBool = AtomicBool::new(false);
thread_local! {
    #[cfg(feature = "passkey-webauthn")]
    static PASSKEY_AUTH_BLOCK: RefCell<Option<RcBlock<dyn Fn(NSInteger)>>> = RefCell::new(None);
    static APP_NAP_ACTIVITY: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        RefCell::new(None);
}

static CEF_HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);
const CEF_APPLICATION_CLASS_NAME: &[u8] = b"TaborCefApplication\0";

pub fn ensure_cef_application() {
    let mtm = MainThreadMarker::new().expect("CEF application setup must run on main thread");
    let class = cef_application_class();

    let app: *mut AnyObject = unsafe { msg_send![class, sharedApplication] };
    let Some(app) = (unsafe { Retained::from_raw(app) }) else {
        panic!("failed to initialize NSApplication singleton for CEF");
    };

    let responds_is: Bool =
        unsafe { msg_send![&*app, respondsToSelector: sel!(isHandlingSendEvent)] };
    let responds_set: Bool =
        unsafe { msg_send![&*app, respondsToSelector: sel!(setHandlingSendEvent:)] };

    assert!(
        responds_is.as_bool() && responds_set.as_bool(),
        "CEF application contract not satisfied: NSApplication missing isHandlingSendEvent/setHandlingSendEvent:"
    );

    let _ = mtm;
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

fn cef_application_class() -> &'static AnyClass {
    static REGISTER: Once = Once::new();
    static mut CLASS: *const AnyClass = std::ptr::null();

    REGISTER.call_once(|| {
        let name = CStr::from_bytes_with_nul(CEF_APPLICATION_CLASS_NAME)
            .expect("static CEF application class name");
        let cls = if let Some(existing) = AnyClass::get(name) {
            existing
        } else {
            let superclass_name =
                CStr::from_bytes_with_nul(b"NSApplication\0").expect("static NSApplication class");
            let superclass =
                AnyClass::get(superclass_name).expect("NSApplication class unavailable");

            let super_ptr = superclass as *const AnyClass;
            let cls = unsafe { ffi::objc_allocateClassPair(super_ptr, name.as_ptr(), 0) };
            let cls = std::ptr::NonNull::new(cls).expect("failed to allocate CEF app class");

            unsafe {
                add_cef_method_raw(
                    cls.as_ptr(),
                    sel!(isHandlingSendEvent),
                    mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel) -> Bool, Imp>(
                        cef_app_is_handling_send_event,
                    ),
                    Bool::ENCODING,
                    &[],
                );
                add_cef_method_raw(
                    cls.as_ptr(),
                    sel!(setHandlingSendEvent:),
                    mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel, Bool), Imp>(
                        cef_app_set_handling_send_event,
                    ),
                    Encoding::Void,
                    &[Bool::ENCODING],
                );

                ffi::objc_registerClassPair(cls.as_ptr());
                cls.as_ref()
            }
        };

        unsafe {
            CLASS = cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

unsafe fn add_cef_method_raw(
    cls: *mut AnyClass,
    selector: Sel,
    imp: Imp,
    ret: Encoding,
    args: &[Encoding],
) {
    let encoding = method_type_encoding(ret, args);
    let success = unsafe { ffi::class_addMethod(cls, selector, imp, encoding.as_ptr()) };
    assert!(success.as_bool(), "failed to add CEF application method");
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

    if prev == 1 {
        set_autofill_override(false);
    }
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
