use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::fmt::Write;
use std::mem;
use std::sync::Once;

use objc2::encode::{Encode, Encoding};
use objc2::ffi;
use objc2::ffi::NSUInteger;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, ProtocolObject, Sel};
use objc2::{MainThreadMarker, class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationDelegateReply};
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

fn is_file_url(url: &str) -> bool {
    url.starts_with("file:")
}

fn filter_file_urls(urls: Vec<String>) -> Vec<String> {
    urls.into_iter().filter(|url| is_file_url(url)).collect()
}

fn urls_from_url_list(urls: *mut AnyObject) -> Vec<String> {
    if urls.is_null() {
        return Vec::new();
    }

    let count: NSUInteger = unsafe { msg_send![urls, count] };
    let mut entries = Vec::new();
    for index in 0..count {
        let item: *mut AnyObject = unsafe { msg_send![urls, objectAtIndex: index] };
        if item.is_null() {
            continue;
        }

        let absolute: *mut AnyObject = unsafe { msg_send![item, absoluteString] };
        if absolute.is_null() {
            continue;
        }

        entries.push(unsafe { &*(absolute as *const NSString) }.to_string());
    }

    entries
}

fn urls_from_file_list(files: *mut AnyObject) -> Vec<String> {
    if files.is_null() {
        return Vec::new();
    }

    let count: NSUInteger = unsafe { msg_send![files, count] };
    let mut entries = Vec::new();
    for index in 0..count {
        let item: *mut AnyObject = unsafe { msg_send![files, objectAtIndex: index] };
        if item.is_null() {
            continue;
        }

        entries.extend(url_from_file_string(item));
    }

    entries
}

fn url_from_file_string(filename: *mut AnyObject) -> Vec<String> {
    if filename.is_null() {
        return Vec::new();
    }

    let path = unsafe { &*(filename as *const NSString) };
    let ns_url: *mut AnyObject = unsafe { msg_send![class!(NSURL), fileURLWithPath: path] };
    if ns_url.is_null() {
        return Vec::new();
    }

    let absolute: *mut AnyObject = unsafe { msg_send![ns_url, absoluteString] };
    if absolute.is_null() {
        return Vec::new();
    }

    vec![unsafe { &*(absolute as *const NSString) }.to_string()]
}

unsafe extern "C-unwind" fn handle_open_files(
    _this: &AnyObject,
    _sel: Sel,
    app: *mut AnyObject,
    files: *mut AnyObject,
) {
    dispatch_open_urls(urls_from_file_list(files));
    unsafe {
        let _: () = msg_send![app, replyToOpenOrPrint: NSApplicationDelegateReply::Success];
    }
}

unsafe extern "C-unwind" fn handle_open_file(
    _this: &AnyObject,
    _sel: Sel,
    _app: *mut AnyObject,
    filename: *mut AnyObject,
) -> Bool {
    if filename.is_null() {
        return Bool::NO;
    }

    dispatch_open_urls(url_from_file_string(filename));
    Bool::YES
}

unsafe extern "C-unwind" fn handle_open_urls(
    _this: &AnyObject,
    _sel: Sel,
    _app: *mut AnyObject,
    urls: *mut AnyObject,
) {
    dispatch_open_urls(filter_file_urls(urls_from_url_list(urls)));
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
    let mtm = MainThreadMarker::new().expect("open document handler must be on the main thread");

    OPEN_DOCUMENTS_PROXY.with(|cell| {
        *cell.borrow_mut() = Some(proxy);
    });

    let app = NSApplication::sharedApplication(mtm);
    register_delegate_open_document_methods(app.delegate());

    register_get_url_handler();
}

fn register_delegate_open_document_methods(
    delegate: Option<Retained<ProtocolObject<dyn NSApplicationDelegate>>>,
) {
    let Some(delegate) = delegate else {
        return;
    };

    let delegate_obj = Retained::as_ptr(&delegate).cast::<AnyObject>();
    let cls = unsafe { &*delegate_obj }.class();
    add_method_if_missing(
        cls,
        sel!(application:openFiles:),
        unsafe {
            mem::transmute::<
                unsafe extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject, *mut AnyObject),
                Imp,
            >(handle_open_files)
        },
        Encoding::Void,
        &[Encoding::Object, Encoding::Object],
    );
    add_method_if_missing(
        cls,
        sel!(application:openFile:),
        unsafe {
            mem::transmute::<
                unsafe extern "C-unwind" fn(
                    &AnyObject,
                    Sel,
                    *mut AnyObject,
                    *mut AnyObject,
                ) -> Bool,
                Imp,
            >(handle_open_file)
        },
        Bool::ENCODING,
        &[Encoding::Object, Encoding::Object],
    );
    add_method_if_missing(
        cls,
        sel!(application:openURLs:),
        unsafe {
            mem::transmute::<
                unsafe extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject, *mut AnyObject),
                Imp,
            >(handle_open_urls)
        },
        Encoding::Void,
        &[Encoding::Object, Encoding::Object],
    );
}

fn add_method_if_missing(
    cls: &AnyClass,
    selector: Sel,
    imp: Imp,
    ret: Encoding,
    args: &[Encoding],
) {
    if cls.instance_method(selector).is_some() {
        return;
    }

    unsafe { add_method_raw(cls as *const AnyClass as *mut AnyClass, selector, imp, ret, args) }
}

#[cfg(test)]
mod tests {
    use super::{filter_file_urls, is_file_url};

    #[test]
    fn file_url_filter_keeps_only_file_urls() {
        let urls = vec![
            String::from("file:///tmp/doc.pdf"),
            String::from("https://example.com"),
            String::from("file:///tmp/other.txt"),
        ];

        assert_eq!(
            filter_file_urls(urls),
            vec![String::from("file:///tmp/doc.pdf"), String::from("file:///tmp/other.txt")]
        );
    }

    #[test]
    fn is_file_url_requires_file_scheme() {
        assert!(is_file_url("file:///tmp/doc.pdf"));
        assert!(!is_file_url("https://example.com/doc.pdf"));
        assert!(!is_file_url("about:blank"));
    }
}
