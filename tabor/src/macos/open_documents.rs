use std::cell::RefCell;

use objc2::ffi::NSUInteger;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{class, define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationDelegateReply};
use objc2_foundation::{NSObjectProtocol, NSString};
use winit::event_loop::EventLoopProxy;

use crate::event::{Event, EventType};

struct OpenDocumentsDelegateIvars {
    proxy: EventLoopProxy<Event>,
    forward: Retained<ProtocolObject<dyn NSApplicationDelegate>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = OpenDocumentsDelegateIvars]
    struct OpenDocumentsDelegate;

    impl OpenDocumentsDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn application_did_finish_launching(&self, notification: *mut AnyObject) {
            let forward = &self.ivars().forward;
            unsafe {
                let _: () = msg_send![&**forward, applicationDidFinishLaunching: notification];
            }
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn application_will_terminate(&self, notification: *mut AnyObject) {
            let forward = &self.ivars().forward;
            unsafe {
                let _: () = msg_send![&**forward, applicationWillTerminate: notification];
            }
        }

        #[unsafe(method(application:openFiles:))]
        fn application_open_files(&self, app: *mut AnyObject, files: *mut AnyObject) {
            let urls = urls_from_file_list(files);
            self.send_open_urls(urls);
            unsafe {
                let _: () =
                    msg_send![app, replyToOpenOrPrint: NSApplicationDelegateReply::Success];
            }
        }

        #[unsafe(method(application:openFile:))]
        fn application_open_file(&self, _app: *mut AnyObject, filename: *mut AnyObject) -> bool {
            if filename.is_null() {
                return false.into();
            }

            let urls = url_from_file_string(filename);
            self.send_open_urls(urls);
            true.into()
        }

        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(&self, _app: *mut AnyObject, urls: *mut AnyObject) {
            let urls = urls_from_url_list(urls);
            self.send_open_urls(urls);
        }
    }
);

unsafe impl NSObjectProtocol for OpenDocumentsDelegate {}
unsafe impl NSApplicationDelegate for OpenDocumentsDelegate {}

thread_local! {
    static OPEN_DOCUMENTS_DELEGATE: RefCell<Option<Retained<OpenDocumentsDelegate>>> = RefCell::new(None);
}

impl OpenDocumentsDelegate {
    fn new(
        proxy: EventLoopProxy<Event>,
        forward: Retained<ProtocolObject<dyn NSApplicationDelegate>>,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = OpenDocumentsDelegate::alloc(mtm)
            .set_ivars(OpenDocumentsDelegateIvars { proxy, forward });
        unsafe { msg_send![super(this), init] }
    }

    fn send_open_urls(&self, urls: Vec<String>) {
        if urls.is_empty() {
            return;
        }

        let _ = self
            .ivars()
            .proxy
            .send_event(Event::new(EventType::OpenUrls(urls), None));
    }
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

        let url = unsafe { &*(absolute as *const NSString) }.to_string();
        entries.push(url);
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

pub(crate) fn register_open_documents_handler(proxy: EventLoopProxy<Event>) {
    let mtm = MainThreadMarker::new().expect("open document handler must be on the main thread");
    let app = NSApplication::sharedApplication(mtm);

    OPEN_DOCUMENTS_DELEGATE.with(|cell| {
        if cell.borrow().is_some() {
            return;
        }

        let Some(forward) = app.delegate() else {
            return;
        };

        let delegate = OpenDocumentsDelegate::new(proxy, forward, mtm);
        app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        *cell.borrow_mut() = Some(delegate);
    });
}
