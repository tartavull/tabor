use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::fmt::Write;
use std::mem;
use std::sync::Once;

use objc2::encode::{Encode, Encoding};
use objc2::ffi;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::{NSApplication, NSApplicationDelegateReply};
use objc2_foundation::{NSArray, NSString, NSURL};
use winit::event_loop::EventLoopProxy;

use crate::event::{Event, EventType};

thread_local! {
    static OPEN_DOCUMENTS_PROXY: RefCell<Option<EventLoopProxy<Event>>> = RefCell::new(None);
}

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

unsafe extern "C-unwind" fn application_open_urls(
    _this: &AnyObject,
    _sel: Sel,
    _application: &NSApplication,
    urls: &NSArray<NSURL>,
) {
    let urls = collect_urls_from_nsurls(urls);
    dispatch_open_urls(urls);
}

unsafe extern "C-unwind" fn application_open_file(
    _this: &AnyObject,
    _sel: Sel,
    _application: &NSApplication,
    filename: &NSString,
) -> Bool {
    let Some(url) = url_from_path(filename) else {
        return Bool::NO;
    };

    dispatch_open_urls(vec![url]);
    Bool::YES
}

unsafe extern "C-unwind" fn application_open_files(
    _this: &AnyObject,
    _sel: Sel,
    application: &NSApplication,
    filenames: &NSArray<NSString>,
) {
    let urls = collect_urls_from_paths(filenames);
    dispatch_open_urls(urls);

    application.replyToOpenOrPrint(NSApplicationDelegateReply::Success);
}

fn open_documents_delegate_class(superclass: &AnyClass) -> &'static AnyClass {
    static REGISTER: Once = Once::new();
    static mut CLASS: *const AnyClass = std::ptr::null();

    REGISTER.call_once(|| {
        let name = delegate_class_name();
        let cls = if let Some(existing) = AnyClass::get(name) {
            existing
        } else {
            let super_ptr = superclass as *const AnyClass;
            let cls = unsafe { ffi::objc_allocateClassPair(super_ptr, name.as_ptr(), 0) };
            let cls = std::ptr::NonNull::new(cls)
                .expect("failed to allocate open documents delegate class");

            unsafe {
                add_method_raw(
                    cls.as_ptr(),
                    sel!(application:openURLs:),
                    mem::transmute::<
                        unsafe extern "C-unwind" fn(
                            &AnyObject,
                            Sel,
                            &NSApplication,
                            &NSArray<NSURL>,
                        ),
                        Imp,
                    >(application_open_urls),
                    Encoding::Void,
                    &[Encoding::Object, Encoding::Object],
                );
                add_method_raw(
                    cls.as_ptr(),
                    sel!(application:openFile:),
                    mem::transmute::<
                        unsafe extern "C-unwind" fn(
                            &AnyObject,
                            Sel,
                            &NSApplication,
                            &NSString,
                        ) -> Bool,
                        Imp,
                    >(application_open_file),
                    Bool::ENCODING,
                    &[Encoding::Object, Encoding::Object],
                );
                add_method_raw(
                    cls.as_ptr(),
                    sel!(application:openFiles:),
                    mem::transmute::<
                        unsafe extern "C-unwind" fn(
                            &AnyObject,
                            Sel,
                            &NSApplication,
                            &NSArray<NSString>,
                        ),
                        Imp,
                    >(application_open_files),
                    Encoding::Void,
                    &[Encoding::Object, Encoding::Object],
                );

                ffi::objc_registerClassPair(cls.as_ptr());
            }

            unsafe { cls.as_ref() }
        };

        unsafe {
            CLASS = cls as *const AnyClass;
        }
    });

    unsafe { &*CLASS }
}

fn delegate_class_name() -> &'static CStr {
    CStr::from_bytes_with_nul(b"TaborOpenDocumentsDelegate\0").expect("static class name")
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
    assert!(success.as_bool(), "failed to add open documents method");
}

fn method_type_encoding(ret: Encoding, args: &[Encoding]) -> CString {
    let mut types = format!("{ret}{}{}", Encoding::Object, Encoding::Sel);
    for enc in args {
        let _ = write!(&mut types, "{enc}");
    }
    CString::new(types).expect("method type encoding")
}

fn url_from_path(path: &NSString) -> Option<String> {
    let url = NSURL::fileURLWithPath(path);
    url.absoluteString().map(|url| url.to_string())
}

fn collect_urls_from_paths(paths: &NSArray<NSString>) -> Vec<String> {
    let mut urls = Vec::new();
    let count = paths.count();
    for index in 0..count {
        let path = paths.objectAtIndex(index);
        if let Some(url) = url_from_path(&path) {
            urls.push(url);
        }
    }
    urls
}

fn collect_urls_from_nsurls(urls: &NSArray<NSURL>) -> Vec<String> {
    let mut out = Vec::new();
    let count = urls.count();
    for index in 0..count {
        let url = urls.objectAtIndex(index);
        if let Some(value) = url.absoluteString() {
            out.push(value.to_string());
        }
    }
    out
}

pub(crate) fn register_open_documents_handler(proxy: EventLoopProxy<Event>) {
    let mtm = MainThreadMarker::new().expect("open document handler must be on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app.delegate().expect("open document handler requires an application delegate");
    let delegate_obj = unsafe { &*(Retained::as_ptr(&delegate) as *const AnyObject) };

    OPEN_DOCUMENTS_PROXY.with(|cell| {
        *cell.borrow_mut() = Some(proxy);
    });

    let current_class = delegate_obj.class();
    if current_class.name() == delegate_class_name() {
        return;
    }

    let subclass = open_documents_delegate_class(current_class);
    unsafe {
        let old_class = AnyObject::set_class(delegate_obj, subclass);
        debug_assert_eq!(old_class, current_class);
    }
    app.setDelegate(Some(&*delegate));
}
