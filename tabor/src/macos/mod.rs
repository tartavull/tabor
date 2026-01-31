use std::cell::RefCell;
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use block2::RcBlock;
use objc2::ffi::NSInteger;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{
    NSDictionary, NSActivityOptions, NSProcessInfo, NSObjectProtocol, NSString, NSUserDefaults,
    ns_string,
};

#[link(name = "AuthenticationServices", kind = "framework")]
unsafe extern "C" {}

pub mod favicon;
pub mod cef;
pub mod locale;
pub mod open_documents;
pub mod proc;
pub mod remote_inspector;
pub mod web_commands;
pub mod web_cursor;
pub mod webview;
mod webview_cef;
mod webview_webkit;

pub(crate) use open_documents::register_open_documents_handler;

static WEBVIEW_COUNT: AtomicUsize = AtomicUsize::new(0);
static PASSKEY_AUTH_REQUESTED: AtomicBool = AtomicBool::new(false);
thread_local! {
    static PASSKEY_AUTH_BLOCK: RefCell<Option<RcBlock<dyn Fn(NSInteger)>>> = RefCell::new(None);
    static APP_NAP_ACTIVITY: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        RefCell::new(None);
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
