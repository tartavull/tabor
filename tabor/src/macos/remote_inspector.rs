use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use block2::RcBlock;
use libc::{c_char, c_void, free, size_t};
use parking_lot::{Condvar, Mutex};

use crate::tabs::TabId;

const WIR_XPC_MACH_PORT_NAME: &str = "com.apple.webinspector";

const WIR_MESSAGE_NAME_KEY: &str = "WIRMessageNameKey";
const WIR_TARGET_IDENTIFIER_KEY: &str = "WIRTargetIdentifierKey";
const WIR_CONNECTION_IDENTIFIER_KEY: &str = "WIRConnectionIdentifierKey";
const WIR_SENDER_KEY: &str = "WIRSenderKey";
const WIR_SOCKET_DATA_KEY: &str = "WIRSocketDataKey";
const WIR_RAW_DATA_KEY: &str = "WIRRawDataKey";
const WIR_MESSAGE_DATA_TYPE_KEY: &str = "WIRMessageDataTypeKey";
const WIR_MESSAGE_DATA_TYPE_CHUNK_SUPPORTED_KEY: &str = "WIRMessageDataTypeChunkSupportedKey";
const WIR_LISTING_KEY: &str = "WIRListingKey";
const WIR_TYPE_KEY: &str = "WIRTypeKey";
const WIR_URL_KEY: &str = "WIRURLKey";
const WIR_TITLE_KEY: &str = "WIRTitleKey";
const WIR_OVERRIDE_NAME_KEY: &str = "WIROverrideNameKey";
const WIR_HOST_APPLICATION_IDENTIFIER_KEY: &str = "WIRHostApplicationIdentifierKey";

const WIR_APPLICATION_GET_LISTING_MESSAGE: &str = "WIRApplicationGetListingMessage";
const WIR_LISTING_MESSAGE: &str = "WIRListingMessage";
const WIR_SOCKET_SETUP_MESSAGE: &str = "WIRSocketSetupMessage";
const WIR_SOCKET_DATA_MESSAGE: &str = "WIRSocketDataMessage";
const WIR_RAW_DATA_MESSAGE: &str = "WIRRawDataMessage";
const WIR_WEB_PAGE_CLOSE_MESSAGE: &str = "WIRWebPageCloseMessage";
const WIR_PERMISSION_DENIED: &str = "WIRPermissionDenied";

const LISTING_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorTarget {
    pub target_id: u64,
    pub target_type: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub override_name: Option<String>,
    pub host_app_identifier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorSession {
    pub session_id: String,
    pub target_id: u64,
    pub tab_id: TabId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorMessage {
    pub session_id: String,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorTabInfo {
    pub tab_id: TabId,
    pub url: Option<String>,
    pub title: Option<String>,
    pub override_name: Option<String>,
}

#[derive(Debug)]
pub enum InspectorError {
    PermissionDenied,
    ConnectionFailed(String),
    Timeout,
    NotFound(String),
    Ambiguous(String),
    InvalidMessage(String),
}

impl InspectorError {
    fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidMessage(message.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageDataType {
    Full,
    Chunk,
    FinalChunk,
}

impl MessageDataType {
    fn parse(value: Option<String>, numeric: Option<u64>) -> Self {
        if let Some(value) = value {
            match value.to_ascii_lowercase().as_str() {
                "chunk" => return Self::Chunk,
                "finalchunk" | "final_chunk" => return Self::FinalChunk,
                "full" => return Self::Full,
                _ => (),
            }
        }
        if let Some(value) = numeric {
            return match value {
                1 => Self::Chunk,
                2 => Self::FinalChunk,
                _ => Self::Full,
            };
        }
        Self::Full
    }
}

struct InspectorSessionState {
    target_id: u64,
    pending_messages: VecDeque<String>,
    pending_chunk: Vec<u8>,
}

#[derive(Default)]
struct RemoteInspectorState {
    permission_denied: bool,
    connection_error: Option<String>,
    listing_generation: u64,
    targets: Vec<InspectorTarget>,
    sessions: HashMap<String, InspectorSessionState>,
}

pub struct RemoteInspectorClient {
    inner: Arc<RemoteInspectorInner>,
}

struct RemoteInspectorInner {
    connection: xpc::xpc_connection_t,
    queue: xpc::dispatch_queue_t,
    handler: *mut block2::Block<dyn Fn(xpc::xpc_object_t)>,
    sender: String,
    state: Mutex<RemoteInspectorState>,
    listing_cv: Condvar,
    connection_seq: AtomicU64,
}

impl RemoteInspectorClient {
    pub fn connect() -> Result<Self, InspectorError> {
        let queue_label =
            CString::new("tabor.remote_inspector").map_err(|_| InspectorError::invalid("Bad label"))?;
        let queue = unsafe { xpc::dispatch_queue_create(queue_label.as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            return Err(InspectorError::ConnectionFailed(String::from("Failed to create dispatch queue")));
        }

        let mach_name =
            CString::new(WIR_XPC_MACH_PORT_NAME).map_err(|_| InspectorError::invalid("Bad mach name"))?;
        let connection = unsafe { xpc::xpc_connection_create_mach_service(mach_name.as_ptr(), queue, 0) };
        if connection.is_null() {
            return Err(InspectorError::ConnectionFailed(String::from("Failed to connect to webinspectord")));
        }

        let sender = format!("PID:{}", std::process::id());
        let inner = Arc::new_cyclic(|weak: &Weak<RemoteInspectorInner>| {
            let handler = RcBlock::new({
                let weak = weak.clone();
                move |message: xpc::xpc_object_t| {
                    if let Some(inner) = Weak::upgrade(&weak) {
                        inner.handle_message(message);
                    }
                }
            });
            let handler_ptr = RcBlock::into_raw(handler);

            unsafe {
                xpc::xpc_connection_set_event_handler(connection, &*handler_ptr);
                xpc::xpc_connection_resume(connection);
            }

            RemoteInspectorInner {
                connection,
                queue,
                handler: handler_ptr,
                sender,
                state: Mutex::new(RemoteInspectorState::default()),
                listing_cv: Condvar::new(),
                connection_seq: AtomicU64::new(1),
            }
        });

        Ok(Self { inner })
    }

    pub fn list_targets(&self) -> Result<Vec<InspectorTarget>, InspectorError> {
        self.send_listing_request()?;
        let deadline = Instant::now() + LISTING_TIMEOUT;
        let mut state = self.inner.state.lock();
        let generation = state.listing_generation;

        loop {
            if state.permission_denied {
                return Err(InspectorError::PermissionDenied);
            }
            if let Some(error) = state.connection_error.take() {
                return Err(InspectorError::ConnectionFailed(error));
            }
            if state.listing_generation != generation {
                return Ok(state.targets.clone());
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(InspectorError::Timeout);
            }
            self.inner.listing_cv.wait_for(&mut state, deadline - now);
        }
    }

    pub fn attach(&self, tab_id: TabId, target_id: u64) -> Result<InspectorSession, InspectorError> {
        let connection_id = self.next_connection_id();
        let session_id = format!("{}-{connection_id}", self.inner.sender);
        let mut state = self.inner.state.lock();
        if state.permission_denied {
            return Err(InspectorError::PermissionDenied);
        }
        if let Some(error) = state.connection_error.clone() {
            return Err(InspectorError::ConnectionFailed(error));
        }
        state.sessions.insert(
            session_id.clone(),
            InspectorSessionState {
                target_id,
                pending_messages: VecDeque::new(),
                pending_chunk: Vec::new(),
            },
        );
        drop(state);

        self.send_socket_setup(target_id, &session_id)?;
        Ok(InspectorSession {
            session_id,
            target_id,
            tab_id,
        })
    }

    pub fn detach(&self, session_id: &str) -> Result<(), InspectorError> {
        let mut state = self.inner.state.lock();
        let Some(session) = state.sessions.remove(session_id) else {
            return Err(InspectorError::not_found("Inspector session not found"));
        };
        drop(state);

        self.send_web_page_close(session.target_id, session_id)?;
        Ok(())
    }

    pub fn send_message(&self, session_id: &str, message: &str) -> Result<(), InspectorError> {
        let state = self.inner.state.lock();
        let Some(session) = state.sessions.get(session_id) else {
            return Err(InspectorError::not_found("Inspector session not found"));
        };
        let target_id = session.target_id;
        drop(state);

        self.send_socket_data(target_id, session_id, message.as_bytes())
    }

    pub fn poll_messages(
        &self,
        session_id: &str,
        max: Option<usize>,
    ) -> Result<Vec<InspectorMessage>, InspectorError> {
        let mut state = self.inner.state.lock();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return Err(InspectorError::not_found("Inspector session not found"));
        };

        let take = max.unwrap_or(session.pending_messages.len());
        let mut messages = Vec::with_capacity(take);
        for _ in 0..take {
            let Some(payload) = session.pending_messages.pop_front() else {
                break;
            };
            messages.push(InspectorMessage {
                session_id: session_id.to_string(),
                payload,
            });
        }

        Ok(messages)
    }

    pub fn has_session(&self, session_id: &str) -> bool {
        let state = self.inner.state.lock();
        state.sessions.contains_key(session_id)
    }

    fn next_connection_id(&self) -> u64 {
        self.inner.connection_seq.fetch_add(1, Ordering::Relaxed)
    }

    fn send_listing_request(&self) -> Result<(), InspectorError> {
        let mut message = xpc::XpcDictionary::new();
        message.set_string(WIR_MESSAGE_NAME_KEY, WIR_APPLICATION_GET_LISTING_MESSAGE);
        message.set_string(WIR_SENDER_KEY, &self.inner.sender);
        self.send_message_raw(message)
    }

    fn send_socket_setup(&self, target_id: u64, session_id: &str) -> Result<(), InspectorError> {
        let mut message = xpc::XpcDictionary::new();
        message.set_string(WIR_MESSAGE_NAME_KEY, WIR_SOCKET_SETUP_MESSAGE);
        message.set_uint64(WIR_TARGET_IDENTIFIER_KEY, target_id);
        message.set_string(WIR_CONNECTION_IDENTIFIER_KEY, session_id);
        message.set_string(WIR_SENDER_KEY, &self.inner.sender);
        message.set_bool(WIR_MESSAGE_DATA_TYPE_CHUNK_SUPPORTED_KEY, true);
        self.send_message_raw(message)
    }

    fn send_socket_data(
        &self,
        target_id: u64,
        session_id: &str,
        payload: &[u8],
    ) -> Result<(), InspectorError> {
        let mut message = xpc::XpcDictionary::new();
        message.set_string(WIR_MESSAGE_NAME_KEY, WIR_SOCKET_DATA_MESSAGE);
        message.set_uint64(WIR_TARGET_IDENTIFIER_KEY, target_id);
        message.set_string(WIR_CONNECTION_IDENTIFIER_KEY, session_id);
        message.set_string(WIR_SENDER_KEY, &self.inner.sender);
        message.set_data(WIR_SOCKET_DATA_KEY, payload);
        self.send_message_raw(message)
    }

    fn send_web_page_close(&self, target_id: u64, session_id: &str) -> Result<(), InspectorError> {
        let mut message = xpc::XpcDictionary::new();
        message.set_string(WIR_MESSAGE_NAME_KEY, WIR_WEB_PAGE_CLOSE_MESSAGE);
        message.set_uint64(WIR_TARGET_IDENTIFIER_KEY, target_id);
        message.set_string(WIR_CONNECTION_IDENTIFIER_KEY, session_id);
        message.set_string(WIR_SENDER_KEY, &self.inner.sender);
        self.send_message_raw(message)
    }

    fn send_message_raw(&self, message: xpc::XpcDictionary) -> Result<(), InspectorError> {
        if message.is_null() {
            return Err(InspectorError::invalid("Failed to build XPC message"));
        }
        unsafe {
            xpc::xpc_connection_send_message(self.inner.connection, message.as_raw());
        }
        Ok(())
    }
}

impl RemoteInspectorInner {
    fn handle_message(self: Arc<Self>, message: xpc::xpc_object_t) {
        if xpc::is_error(message) {
            self.handle_error(message);
            return;
        }

        if !xpc::is_dictionary(message) {
            return;
        }

        let name = xpc::dictionary_string(message, WIR_MESSAGE_NAME_KEY);
        if name.as_deref() == Some(WIR_PERMISSION_DENIED) {
            let mut state = self.state.lock();
            state.permission_denied = true;
            self.listing_cv.notify_all();
            return;
        }

        let message_name = name.as_deref().unwrap_or_default();
        match message_name {
            WIR_LISTING_MESSAGE => self.handle_listing(message),
            WIR_RAW_DATA_MESSAGE => self.handle_raw_data(message),
            _ => {
                if xpc::dictionary_contains_key(message, WIR_LISTING_KEY) {
                    self.handle_listing(message);
                } else if xpc::dictionary_contains_key(message, WIR_RAW_DATA_KEY) {
                    self.handle_raw_data(message);
                }
            },
        }
    }

    fn handle_error(&self, message: xpc::xpc_object_t) {
        let mut state = self.state.lock();
        state.connection_error = xpc::copy_description(message)
            .filter(|value| !value.is_empty())
            .or_else(|| Some(String::from("XPC connection error")));
        self.listing_cv.notify_all();
    }

    fn handle_listing(&self, message: xpc::xpc_object_t) {
        let listing = xpc::dictionary_value(message, WIR_LISTING_KEY).unwrap_or(message);
        let targets = xpc::collect_targets(listing);

        let mut state = self.state.lock();
        state.targets = targets;
        state.listing_generation = state.listing_generation.wrapping_add(1);
        self.listing_cv.notify_all();
    }

    fn handle_raw_data(&self, message: xpc::xpc_object_t) {
        let Some(session_id) = xpc::dictionary_string(message, WIR_CONNECTION_IDENTIFIER_KEY) else {
            return;
        };
        let Some(payload) = xpc::dictionary_data(message, WIR_RAW_DATA_KEY) else {
            return;
        };

        let data_type =
            MessageDataType::parse(xpc::dictionary_string(message, WIR_MESSAGE_DATA_TYPE_KEY), xpc::dictionary_uint64(message, WIR_MESSAGE_DATA_TYPE_KEY));

        let mut state = self.state.lock();
        let Some(session) = state.sessions.get_mut(&session_id) else {
            return;
        };

        match data_type {
            MessageDataType::Full => {
                if let Ok(text) = String::from_utf8(payload) {
                    session.pending_messages.push_back(text);
                }
            },
            MessageDataType::Chunk => {
                session.pending_chunk.extend_from_slice(&payload);
            },
            MessageDataType::FinalChunk => {
                session.pending_chunk.extend_from_slice(&payload);
                let chunk = std::mem::take(&mut session.pending_chunk);
                if let Ok(text) = String::from_utf8(chunk) {
                    session.pending_messages.push_back(text);
                }
            },
        }
    }
}

impl Drop for RemoteInspectorInner {
    fn drop(&mut self) {
        unsafe {
            xpc::xpc_connection_cancel(self.connection);
            xpc::xpc_release(self.connection);
            if !self.queue.is_null() {
                xpc::dispatch_release(self.queue.cast());
            }
            if !self.handler.is_null() {
                let _ = RcBlock::from_raw(self.handler);
            }
        }
    }
}

pub fn match_target_for_tab(
    targets: &[InspectorTarget],
    tab: &InspectorTabInfo,
    pid: u32,
) -> Result<u64, InspectorError> {
    let pid_prefix = format!("PID:{pid}");
    let mut candidates: Vec<(u64, usize)> = Vec::new();

    for target in targets {
        if let Some(score) = match_score(tab, target, &pid_prefix) {
            candidates.push((target.target_id, score));
        }
    }

    if candidates.is_empty() {
        return Err(InspectorError::not_found("No matching inspector target found"));
    }

    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    let best_score = candidates[0].1;
    let best: Vec<u64> = candidates
        .iter()
        .filter(|(_, score)| *score == best_score)
        .map(|(target_id, _)| *target_id)
        .collect();

    if best.len() > 1 {
        return Err(InspectorError::Ambiguous(String::from("Multiple inspector targets matched")));
    }

    Ok(best[0])
}

pub fn match_tab_for_target(
    target: &InspectorTarget,
    tabs: &[InspectorTabInfo],
    pid: u32,
) -> Option<TabId> {
    let pid_prefix = format!("PID:{pid}");
    let mut candidates: Vec<(TabId, usize)> = Vec::new();

    for tab in tabs {
        if let Some(score) = match_score(tab, target, &pid_prefix) {
            candidates.push((tab.tab_id, score));
        }
    }

    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    let best = candidates.first()?;
    if candidates.len() > 1 && candidates[1].1 == best.1 {
        return None;
    }
    Some(best.0)
}

fn match_score(
    tab: &InspectorTabInfo,
    target: &InspectorTarget,
    pid_prefix: &str,
) -> Option<usize> {
    if target.host_app_identifier.as_deref() != Some(pid_prefix) {
        return None;
    }
    if let Some(target_type) = target.target_type.as_deref() {
        if target_type != "WIRTypeWebPage" {
            return None;
        }
    }

    let normalized_url = tab.url.as_deref().filter(|value| !value.is_empty()).map(normalize_url_key);
    let title = tab.title.as_deref().filter(|value| !value.is_empty());
    let override_name = tab.override_name.as_deref().filter(|value| !value.is_empty());

    if let (Some(tab_url), Some(target_url)) = (normalized_url.as_deref(), target.url.as_deref()) {
        if normalize_url_key(target_url) == tab_url {
            return Some(3);
        }
    }
    if let (Some(override_name), Some(target_name)) =
        (override_name, target.override_name.as_deref())
    {
        if target_name == override_name {
            return Some(2);
        }
    }
    if let (Some(title), Some(target_title)) = (title, target.title.as_deref()) {
        if target_title == title {
            return Some(1);
        }
    }

    None
}

fn normalize_url_key(input: &str) -> String {
    input.trim_end_matches('/').to_ascii_lowercase()
}

mod xpc {
    use super::*;
    use block2::StackBlock;
    use objc2::encode::{Encoding, RefEncode};
    use std::ptr;

    #[repr(C)]
    pub struct XpcObject {
        _private: [u8; 0],
    }

    // SAFETY: `xpc_object_t` is an opaque pointer; treat it as a void pointer for encoding.
    unsafe impl RefEncode for XpcObject {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Void);
    }

    #[allow(non_camel_case_types)]
    pub type xpc_object_t = *mut XpcObject;
    #[allow(non_camel_case_types)]
    pub type xpc_connection_t = xpc_object_t;
    #[allow(non_camel_case_types)]
    pub type xpc_type_t = *const c_void;
    #[allow(non_camel_case_types)]
    pub type dispatch_queue_t = *mut c_void;

    #[link(name = "System", kind = "framework")]
    unsafe extern "C" {
        pub fn xpc_connection_create_mach_service(
            name: *const c_char,
            queue: dispatch_queue_t,
            flags: u64,
        ) -> xpc_connection_t;
        pub fn xpc_connection_set_event_handler(
            connection: xpc_connection_t,
            handler: &block2::Block<dyn Fn(xpc_object_t)>,
        );
        pub fn xpc_connection_resume(connection: xpc_connection_t);
        pub fn xpc_connection_cancel(connection: xpc_connection_t);
        pub fn xpc_connection_send_message(connection: xpc_connection_t, message: xpc_object_t);

        pub fn xpc_dictionary_create(
            keys: *const *const c_char,
            values: *const xpc_object_t,
            count: size_t,
        ) -> xpc_object_t;
        pub fn xpc_dictionary_set_string(
            dict: xpc_object_t,
            key: *const c_char,
            value: *const c_char,
        );
        pub fn xpc_dictionary_set_uint64(
            dict: xpc_object_t,
            key: *const c_char,
            value: u64,
        );
        pub fn xpc_dictionary_set_bool(
            dict: xpc_object_t,
            key: *const c_char,
            value: bool,
        );
        pub fn xpc_dictionary_set_data(
            dict: xpc_object_t,
            key: *const c_char,
            value: *const c_void,
            length: size_t,
        );
        pub fn xpc_dictionary_get_value(dict: xpc_object_t, key: *const c_char) -> xpc_object_t;
        pub fn xpc_dictionary_get_string(dict: xpc_object_t, key: *const c_char) -> *const c_char;
        pub fn xpc_dictionary_get_uint64(dict: xpc_object_t, key: *const c_char) -> u64;
        pub fn xpc_dictionary_apply(
            dict: xpc_object_t,
            applier: &block2::Block<dyn Fn(*const c_void, xpc_object_t) -> u8>,
        ) -> bool;

        pub fn xpc_array_get_count(array: xpc_object_t) -> size_t;
        pub fn xpc_array_get_value(array: xpc_object_t, index: size_t) -> xpc_object_t;

        pub fn xpc_get_type(object: xpc_object_t) -> xpc_type_t;
        pub fn xpc_type_get_name(xtype: xpc_type_t) -> *const c_char;
        pub fn xpc_data_get_length(data: xpc_object_t) -> size_t;
        pub fn xpc_data_get_bytes_ptr(data: xpc_object_t) -> *const c_void;
        pub fn xpc_copy_description(object: xpc_object_t) -> *mut c_char;
        pub fn xpc_release(object: xpc_object_t);

        pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> dispatch_queue_t;
        pub fn dispatch_release(object: *const c_void);
    }

    pub struct XpcDictionary {
        raw: xpc_object_t,
    }

    impl XpcDictionary {
        pub fn new() -> Self {
            let raw = unsafe { xpc_dictionary_create(ptr::null(), ptr::null(), 0) };
            Self { raw }
        }

        pub fn is_null(&self) -> bool {
            self.raw.is_null()
        }

        pub fn set_string(&mut self, key: &str, value: &str) {
            let key = CString::new(key).expect("xpc key");
            let value = CString::new(value).expect("xpc value");
            unsafe {
                xpc_dictionary_set_string(self.raw, key.as_ptr(), value.as_ptr());
            }
        }

        pub fn set_uint64(&mut self, key: &str, value: u64) {
            let key = CString::new(key).expect("xpc key");
            unsafe {
                xpc_dictionary_set_uint64(self.raw, key.as_ptr(), value);
            }
        }

        pub fn set_bool(&mut self, key: &str, value: bool) {
            let key = CString::new(key).expect("xpc key");
            unsafe {
                xpc_dictionary_set_bool(self.raw, key.as_ptr(), value);
            }
        }

        pub fn set_data(&mut self, key: &str, value: &[u8]) {
            let key = CString::new(key).expect("xpc key");
            unsafe {
                xpc_dictionary_set_data(self.raw, key.as_ptr(), value.as_ptr().cast(), value.len());
            }
        }

        pub fn as_raw(&self) -> xpc_object_t {
            self.raw
        }
    }

    impl Drop for XpcDictionary {
        fn drop(&mut self) {
            unsafe {
                if !self.raw.is_null() {
                    xpc_release(self.raw);
                }
            }
        }
    }

    fn type_matches(object: xpc_object_t, expected: &str) -> bool {
        let xtype = unsafe { xpc_get_type(object) };
        if xtype.is_null() {
            return false;
        }
        let name = unsafe { xpc_type_get_name(xtype) };
        if name.is_null() {
            return false;
        }
        let name = unsafe { CStr::from_ptr(name) };
        name.to_bytes() == expected.as_bytes()
    }

    pub fn is_dictionary(object: xpc_object_t) -> bool {
        type_matches(object, "dictionary")
    }

    pub fn is_array(object: xpc_object_t) -> bool {
        type_matches(object, "array")
    }

    pub fn is_data(object: xpc_object_t) -> bool {
        type_matches(object, "data")
    }

    pub fn is_error(object: xpc_object_t) -> bool {
        type_matches(object, "error")
    }

    pub fn copy_description(object: xpc_object_t) -> Option<String> {
        let ptr = unsafe { xpc_copy_description(object) };
        if ptr.is_null() {
            return None;
        }
        let description = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
        unsafe {
            free(ptr.cast());
        }
        Some(description)
    }

    pub fn dictionary_contains_key(dict: xpc_object_t, key: &str) -> bool {
        !dictionary_value(dict, key).is_none()
    }

    pub fn dictionary_value(dict: xpc_object_t, key: &str) -> Option<xpc_object_t> {
        if !is_dictionary(dict) {
            return None;
        }
        let key = CString::new(key).ok()?;
        let value = unsafe { xpc_dictionary_get_value(dict, key.as_ptr()) };
        if value.is_null() {
            None
        } else {
            Some(value)
        }
    }

    pub fn dictionary_string(dict: xpc_object_t, key: &str) -> Option<String> {
        if !is_dictionary(dict) {
            return None;
        }
        let key = CString::new(key).ok()?;
        let value = unsafe { xpc_dictionary_get_string(dict, key.as_ptr()) };
        if value.is_null() {
            return None;
        }
        let cstr = unsafe { CStr::from_ptr(value) };
        Some(cstr.to_string_lossy().into_owned())
    }

    pub fn dictionary_uint64(dict: xpc_object_t, key: &str) -> Option<u64> {
        if !is_dictionary(dict) {
            return None;
        }
        let key = CString::new(key).ok()?;
        let value = unsafe { xpc_dictionary_get_uint64(dict, key.as_ptr()) };
        if value == 0 {
            return None;
        }
        Some(value)
    }

    pub fn dictionary_data(dict: xpc_object_t, key: &str) -> Option<Vec<u8>> {
        let value = dictionary_value(dict, key)?;
        if !is_data(value) {
            return None;
        }
        let len = unsafe { xpc_data_get_length(value) };
        let ptr = unsafe { xpc_data_get_bytes_ptr(value) };
        if ptr.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
        Some(bytes.to_vec())
    }

    pub fn collect_targets(root: xpc_object_t) -> Vec<InspectorTarget> {
        let mut targets = Vec::new();
        collect_targets_inner(root, &mut targets);
        targets
    }

    fn collect_targets_inner(root: xpc_object_t, targets: &mut Vec<InspectorTarget>) {
        if is_dictionary(root) {
            if let Some(target_id) = dictionary_uint64(root, WIR_TARGET_IDENTIFIER_KEY) {
                let target = InspectorTarget {
                    target_id,
                    target_type: dictionary_string(root, WIR_TYPE_KEY),
                    url: dictionary_string(root, WIR_URL_KEY),
                    title: dictionary_string(root, WIR_TITLE_KEY),
                    override_name: dictionary_string(root, WIR_OVERRIDE_NAME_KEY),
                    host_app_identifier: dictionary_string(root, WIR_HOST_APPLICATION_IDENTIFIER_KEY),
                };
                targets.push(target);
            }

            let targets_ptr = targets as *mut Vec<InspectorTarget>;
            let applier =
                StackBlock::new(move |_key: *const c_void, value: xpc_object_t| -> u8 {
                    // SAFETY: xpc_dictionary_apply invokes the block synchronously.
                    unsafe {
                        collect_targets_inner(value, &mut *targets_ptr);
                    }
                    1
                });
            let _ = unsafe { xpc_dictionary_apply(root, &applier) };
        } else if is_array(root) {
            let count = unsafe { xpc_array_get_count(root) };
            for index in 0..count {
                let value = unsafe { xpc_array_get_value(root, index) };
                if !value.is_null() {
                    collect_targets_inner(value, targets);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_chunks_are_reassembled() {
        let mut state = RemoteInspectorState::default();
        state.sessions.insert(
            String::from("sess-1"),
            InspectorSessionState {
                target_id: 10,
                pending_messages: VecDeque::new(),
                pending_chunk: Vec::new(),
            },
        );

        {
            let session = state.sessions.get_mut("sess-1").unwrap();
            session.pending_chunk.extend_from_slice(b"{\"id\":1,");
        }
        {
            let session = state.sessions.get_mut("sess-1").unwrap();
            session.pending_chunk.extend_from_slice(b"\"result\":{}}");
            let payload = std::mem::take(&mut session.pending_chunk);
            session
                .pending_messages
                .push_back(String::from_utf8(payload).expect("utf8"));
        }

        let session = state.sessions.get_mut("sess-1").unwrap();
        let payload = session.pending_messages.pop_front().unwrap();
        assert_eq!(payload, "{\"id\":1,\"result\":{}}");
    }

    #[test]
    fn match_target_prefers_url() {
        let targets = vec![
            InspectorTarget {
                target_id: 1,
                target_type: Some(String::from("WIRTypeWebPage")),
                url: Some(String::from("https://example.com/")),
                title: Some(String::from("Example")),
                override_name: None,
                host_app_identifier: Some(String::from("PID:123")),
            },
            InspectorTarget {
                target_id: 2,
                target_type: Some(String::from("WIRTypeWebPage")),
                url: Some(String::from("https://example.com/docs")),
                title: Some(String::from("Docs")),
                override_name: None,
                host_app_identifier: Some(String::from("PID:123")),
            },
        ];

        let tab = InspectorTabInfo {
            tab_id: TabId::new(1, 1),
            url: Some(String::from("https://example.com")),
            title: Some(String::from("Example")),
            override_name: None,
        };

        let matched = match_target_for_tab(&targets, &tab, 123).expect("match");
        assert_eq!(matched, 1);
    }

    #[test]
    fn match_tab_for_target_avoids_ambiguous() {
        let target = InspectorTarget {
            target_id: 1,
            target_type: Some(String::from("WIRTypeWebPage")),
            url: Some(String::from("https://example.com")),
            title: Some(String::from("Example")),
            override_name: None,
            host_app_identifier: Some(String::from("PID:99")),
        };
        let tabs = vec![
            InspectorTabInfo {
                tab_id: TabId::new(1, 1),
                url: Some(String::from("https://example.com")),
                title: Some(String::from("Example")),
                override_name: None,
            },
            InspectorTabInfo {
                tab_id: TabId::new(2, 1),
                url: Some(String::from("https://example.com")),
                title: Some(String::from("Example")),
                override_name: None,
            },
        ];

        let matched = match_tab_for_target(&target, &tabs, 99);
        assert!(matched.is_none());
    }

    #[test]
    fn message_data_type_parses() {
        assert!(matches!(
            MessageDataType::parse(Some(String::from("chunk")), None),
            MessageDataType::Chunk
        ));
        assert!(matches!(
            MessageDataType::parse(Some(String::from("finalchunk")), None),
            MessageDataType::FinalChunk
        ));
        assert!(matches!(
            MessageDataType::parse(None, Some(2)),
            MessageDataType::FinalChunk
        ));
    }
}
