use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::fmt::Write;
use std::mem;
use std::sync::Once;

use objc2::encode::Encoding;
use objc2::ffi;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_foundation::NSString;
use winit::event_loop::EventLoopProxy;

use crate::event::{Event, EventType};

thread_local! {
    static OPEN_DOCUMENTS_PROXY: RefCell<Option<EventLoopProxy<Event>>> = const { RefCell::new(None) };
    static OPEN_URL_EVENT_HANDLER: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
}

const AE_INTERNET_EVENT_CLASS: u32 = u32::from_be_bytes(*b"GURL");
const AE_GET_URL_EVENT_ID: u32 = u32::from_be_bytes(*b"GURL");
const AE_KEY_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

fn dispatch_open_urls(urls: Vec<String>) {
    if urls.is_empty() {
        return;
    }

    OPEN_DOCUMENTS_PROXY.with(|cell| {
        if let Some(proxy) = cell.borrow().as_ref() {
            let _ = proxy.send_event(Event::new(EventType::OpenUrls(urls), None));
        }
    });
}

unsafe extern "C-unwind" fn handle_get_url_event(
    _this: &AnyObject,
    _sel: Sel,
    event: &AnyObject,
    _reply: &AnyObject,
) {
    let descriptor: Option<Retained<AnyObject>> =
        unsafe { msg_send![event, paramDescriptorForKeyword: AE_KEY_DIRECT_OBJECT] };
    let Some(descriptor) = descriptor else {
        return;
    };

    let url: Option<Retained<NSString>> = unsafe { msg_send![&*descriptor, stringValue] };
    let Some(url) = url else {
        return;
    };

    dispatch_open_urls(vec![url.to_string()]);
}

fn register_get_url_handler() {
    let class_name = c"NSAppleEventManager";
    let Some(manager_class) = AnyClass::get(class_name) else {
        return;
    };

    let manager: Option<Retained<AnyObject>> =
        unsafe { msg_send![manager_class, sharedAppleEventManager] };
    let Some(manager) = manager else {
        return;
    };

    let handler = open_url_event_handler();

    unsafe {
        let _: () = msg_send![
            &*manager,
            setEventHandler: &*handler,
            andSelector: sel!(handleGetURLEvent:withReplyEvent:),
            forEventClass: AE_INTERNET_EVENT_CLASS,
            andEventID: AE_GET_URL_EVENT_ID,
        ];
    }
}

fn open_url_event_handler() -> Retained<AnyObject> {
    OPEN_URL_EVENT_HANDLER.with(|cell| {
        if let Some(handler) = cell.borrow().as_ref() {
            return handler.clone();
        }

        let class = open_url_event_handler_class();
        let handler: Option<Retained<AnyObject>> = unsafe { msg_send![class, new] };
        let handler = handler.expect("failed to allocate open URL event handler");

        *cell.borrow_mut() = Some(handler.clone());
        handler
    })
}

fn open_url_event_handler_class() -> &'static AnyClass {
    static REGISTER: Once = Once::new();
    static mut CLASS: *const AnyClass = std::ptr::null();

    REGISTER.call_once(|| {
        let name = open_url_handler_class_name();
        let cls = if let Some(existing) = AnyClass::get(name) {
            existing
        } else {
            let superclass_name = c"NSObject";
            let superclass = AnyClass::get(superclass_name).expect("NSObject class unavailable");

            let super_ptr = superclass as *const AnyClass;
            let cls = unsafe { ffi::objc_allocateClassPair(super_ptr, name.as_ptr(), 0) };
            let cls = std::ptr::NonNull::new(cls)
                .expect("failed to allocate open URL event handler class");

            unsafe {
                add_method_raw(
                    cls.as_ptr(),
                    sel!(handleGetURLEvent:withReplyEvent:),
                    mem::transmute::<
                        unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, &AnyObject),
                        Imp,
                    >(handle_get_url_event),
                    Encoding::Void,
                    &[Encoding::Object, Encoding::Object],
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

fn open_url_handler_class_name() -> &'static CStr {
    c"TaborOpenUrlEventHandler"
}

unsafe fn add_method_raw(
    cls: *mut AnyClass,
    selector: Sel,
    imp: Imp,
    ret: Encoding,
    args: &[Encoding],
) {
    let encoding = method_type_encoding(ret, args);
    let success = unsafe { ffi::class_addMethod(cls, selector, imp, encoding.as_ptr()) };
    assert!(success.as_bool(), "failed to add open URL method");
}

fn method_type_encoding(ret: Encoding, args: &[Encoding]) -> CString {
    let mut types = format!("{ret}{}{}", Encoding::Object, Encoding::Sel);
    for enc in args {
        let _ = write!(&mut types, "{enc}");
    }
    CString::new(types).expect("method type encoding")
}

pub(crate) fn register_open_documents_handler(proxy: EventLoopProxy<Event>) {
    let _mtm = MainThreadMarker::new().expect("open document handler must be on the main thread");

    OPEN_DOCUMENTS_PROXY.with(|cell| {
        *cell.borrow_mut() = Some(proxy);
    });

    register_get_url_handler();
}
