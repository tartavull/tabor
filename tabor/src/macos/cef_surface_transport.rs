use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::io;
use std::mem;
use std::ptr::NonNull;
use std::sync::Arc;
use std::thread;

use log::{debug, warn};

use super::cef_host_protocol::{
    HostRect, HostSurfaceElement, HostSurfaceFormat, SurfaceLeaseId, ViewId,
};

type KernReturn = c_int;
type MachMsgBits = u32;
type MachMsgId = c_int;
type MachMsgOption = c_int;
type MachMsgReturn = c_int;
type MachMsgSize = u32;
type MachMsgTimeout = u32;
type MachPort = u32;
type MachPortRight = c_int;

const KERN_SUCCESS: KernReturn = 0;
const MACH_PORT_NULL: MachPort = 0;
const MACH_PORT_RIGHT_RECEIVE: MachPortRight = 1;
const MACH_MSGH_BITS_COMPLEX: MachMsgBits = 0x8000_0000;
const MACH_MSG_TYPE_MOVE_SEND: u8 = 17;
const MACH_MSG_TYPE_COPY_SEND: MachMsgBits = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const MACH_SEND_MSG: MachMsgOption = 0x0000_0001;
const MACH_RCV_MSG: MachMsgOption = 0x0000_0002;
const MACH_SEND_TIMEOUT: MachMsgOption = 0x0000_0010;
const MACH_MSG_TIMEOUT_NONE: MachMsgTimeout = 0;
const SURFACE_MESSAGE_ID: MachMsgId = 0x5441_4252;
const SURFACE_MESSAGE_VERSION: u32 = 1;
const RECEIVE_BUFFER_BYTES: usize = 4096;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct MachMsgHeader {
    bits: MachMsgBits,
    size: MachMsgSize,
    remote_port: MachPort,
    local_port: MachPort,
    voucher_port: MachPort,
    id: MachMsgId,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct MachMsgBody {
    descriptor_count: u32,
}

// This matches `mach_msg_port_descriptor_t`, which Darwin packs to four-byte alignment.
#[repr(C, packed(4))]
#[derive(Debug, Clone, Copy, Default)]
struct MachMsgPortDescriptor {
    name: MachPort,
    pad1: MachMsgSize,
    pad2: u16,
    disposition: u8,
    descriptor_type: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SurfaceMachMessage {
    header: MachMsgHeader,
    body: MachMsgBody,
    surface: MachMsgPortDescriptor,
    auth_a: u64,
    auth_b: u64,
    generation: u64,
    view_id: ViewId,
    lease_id: SurfaceLeaseId,
    width: u32,
    height: u32,
    popup_x: i32,
    popup_y: i32,
    popup_width: i32,
    popup_height: i32,
    version: u32,
    format: u32,
    element: u32,
    popup_present: u32,
}

#[repr(C, align(8))]
struct ReceiveBuffer {
    bytes: [u8; RECEIVE_BUFFER_BYTES],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreateMachPort(buffer: *mut c_void) -> MachPort;
    fn IOSurfaceLookupFromMachPort(port: MachPort) -> *mut c_void;
}

unsafe extern "C" {
    static bootstrap_port: MachPort;
    static mach_task_self_: MachPort;

    fn bootstrap_register(
        bootstrap_port: MachPort,
        service_name: *const c_char,
        service_port: MachPort,
    ) -> KernReturn;
    fn bootstrap_look_up(
        bootstrap_port: MachPort,
        service_name: *const c_char,
        service_port: *mut MachPort,
    ) -> KernReturn;
    fn mach_error_string(error: KernReturn) -> *const c_char;
    fn mach_msg(
        message: *mut MachMsgHeader,
        option: MachMsgOption,
        send_size: MachMsgSize,
        receive_limit: MachMsgSize,
        receive_name: MachPort,
        timeout: MachMsgTimeout,
        notify: MachPort,
    ) -> MachMsgReturn;
    fn mach_msg_destroy(message: *mut MachMsgHeader);
    fn mach_port_allocate(task: MachPort, right: MachPortRight, name: *mut MachPort) -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    fn mach_port_insert_right(
        task: MachPort,
        name: MachPort,
        right: MachPort,
        right_type: u32,
    ) -> KernReturn;
    fn mach_port_mod_refs(
        task: MachPort,
        name: MachPort,
        right: MachPortRight,
        delta: c_int,
    ) -> KernReturn;
}

#[derive(Clone)]
pub(super) struct SurfaceEndpoint {
    service_name: String,
    auth: [u64; 2],
}

impl SurfaceEndpoint {
    pub(super) fn service_name(&self) -> &str {
        &self.service_name
    }

    pub(super) fn auth(&self) -> [u64; 2] {
        self.auth
    }
}

pub(super) struct SurfaceReceiver {
    receive_port: MachPort,
    endpoint: SurfaceEndpoint,
}

impl SurfaceReceiver {
    pub(super) fn bind() -> Result<Self, String> {
        let mut random = [0_u8; 32];
        if unsafe { libc::getentropy(random.as_mut_ptr().cast(), random.len()) } != 0 {
            return Err(format!(
                "generate CEF surface endpoint identity: {}",
                io::Error::last_os_error()
            ));
        }

        let mut random_name = String::with_capacity(32);
        for byte in &random[..16] {
            write!(&mut random_name, "{byte:02x}").expect("write to String");
        }
        let service_name =
            format!("com.pinkbot.tabor.cef-surface.{}.{}", std::process::id(), random_name);
        let service_name_c = CString::new(service_name.as_str())
            .map_err(|_| String::from("CEF surface service name contains a null byte"))?;
        let auth = [
            u64::from_ne_bytes(random[16..24].try_into().expect("eight-byte auth word")),
            u64::from_ne_bytes(random[24..32].try_into().expect("eight-byte auth word")),
        ];

        let task = task_self();
        let mut receive_port = MACH_PORT_NULL;
        check_mach(
            unsafe { mach_port_allocate(task, MACH_PORT_RIGHT_RECEIVE, &mut receive_port) },
            "allocate CEF surface receive port",
        )?;
        if let Err(error) = check_mach(
            unsafe {
                mach_port_insert_right(task, receive_port, receive_port, MACH_MSG_TYPE_MAKE_SEND)
            },
            "create CEF surface send right",
        ) {
            release_receive_port(receive_port);
            return Err(error);
        }
        if let Err(error) = check_mach(
            unsafe { bootstrap_register(bootstrap_port, service_name_c.as_ptr(), receive_port) },
            "register CEF surface service",
        ) {
            release_receive_port(receive_port);
            return Err(error);
        }

        Ok(Self { receive_port, endpoint: SurfaceEndpoint { service_name, auth } })
    }

    pub(super) fn endpoint(&self) -> SurfaceEndpoint {
        self.endpoint.clone()
    }

    pub(super) fn spawn<F>(self, handler: F) -> Result<(), String>
    where
        F: Fn(SurfaceReceiveEvent) + Send + 'static,
    {
        thread::Builder::new()
            .name(String::from("tabor-cef-surface-receiver"))
            .spawn(move || self.receive_loop(handler))
            .map(|_| ())
            .map_err(|error| format!("start CEF surface receiver: {error}"))
    }

    fn receive_loop<F>(&self, handler: F)
    where
        F: Fn(SurfaceReceiveEvent),
    {
        loop {
            match self.receive_one() {
                Ok(Some(event)) => handler(event),
                Ok(None) => (),
                Err(error) => {
                    warn!("CEF surface receiver stopped: {error}");
                    return;
                },
            }
        }
    }

    fn receive_one(&self) -> Result<Option<SurfaceReceiveEvent>, String> {
        let mut buffer = ReceiveBuffer { bytes: [0; RECEIVE_BUFFER_BYTES] };
        let header = buffer.bytes.as_mut_ptr().cast::<MachMsgHeader>();
        let result = unsafe {
            mach_msg(
                header,
                MACH_RCV_MSG,
                0,
                RECEIVE_BUFFER_BYTES as MachMsgSize,
                self.receive_port,
                MACH_MSG_TIMEOUT_NONE,
                MACH_PORT_NULL,
            )
        };
        check_mach(result, "receive CEF surface")?;

        let message = unsafe { &*header.cast::<SurfaceMachMessage>() };
        if message.header.size as usize != mem::size_of::<SurfaceMachMessage>()
            || message.header.id != SURFACE_MESSAGE_ID
            || message.header.bits & MACH_MSGH_BITS_COMPLEX == 0
            || message.body.descriptor_count != 1
            || message.surface.descriptor_type != MACH_MSG_PORT_DESCRIPTOR
            || message.surface.disposition != MACH_MSG_TYPE_MOVE_SEND
        {
            unsafe { mach_msg_destroy(header) };
            debug!("Discarded malformed CEF surface message");
            return Ok(None);
        }

        let surface_port = MachSendRight(message.surface.name);
        if message.version != SURFACE_MESSAGE_VERSION
            || [message.auth_a, message.auth_b] != self.endpoint.auth
        {
            debug!("Discarded unauthenticated CEF surface message");
            return Ok(None);
        }

        let Some(format) = decode_format(message.format) else {
            return Ok(Some(SurfaceReceiveEvent::Rejected {
                generation: message.generation,
                view_id: message.view_id,
                lease_id: message.lease_id,
                error: format!("CEF surface message has invalid format {}", message.format),
            }));
        };
        let Some(element) = decode_element(message.element) else {
            return Ok(Some(SurfaceReceiveEvent::Rejected {
                generation: message.generation,
                view_id: message.view_id,
                lease_id: message.lease_id,
                error: format!("CEF surface message has invalid element {}", message.element),
            }));
        };
        if message.width == 0 || message.height == 0 {
            return Ok(Some(SurfaceReceiveEvent::Rejected {
                generation: message.generation,
                view_id: message.view_id,
                lease_id: message.lease_id,
                error: String::from("CEF surface message has empty dimensions"),
            }));
        }

        let io_surface = NonNull::new(unsafe { IOSurfaceLookupFromMachPort(surface_port.0) });
        let Some(io_surface) = io_surface else {
            return Ok(Some(SurfaceReceiveEvent::Rejected {
                generation: message.generation,
                view_id: message.view_id,
                lease_id: message.lease_id,
                error: String::from("IOSurface Mach capability is no longer available"),
            }));
        };
        let popup_rect = (message.popup_present != 0).then_some(HostRect {
            x: message.popup_x,
            y: message.popup_y,
            width: message.popup_width,
            height: message.popup_height,
        });

        Ok(Some(SurfaceReceiveEvent::Frame {
            generation: message.generation,
            frame: SurfaceFrame {
                view_id: message.view_id,
                lease_id: message.lease_id,
                element,
                surface: ReceivedIoSurface(io_surface),
                width: message.width as usize,
                height: message.height as usize,
                format,
                popup_rect,
            },
        }))
    }
}

impl Drop for SurfaceReceiver {
    fn drop(&mut self) {
        release_receive_port(self.receive_port);
    }
}

pub(super) enum SurfaceReceiveEvent {
    Frame { generation: u64, frame: SurfaceFrame },
    Rejected { generation: u64, view_id: ViewId, lease_id: SurfaceLeaseId, error: String },
}

#[derive(Debug)]
pub(super) struct SurfaceFrame {
    pub(super) view_id: ViewId,
    pub(super) lease_id: SurfaceLeaseId,
    pub(super) element: HostSurfaceElement,
    pub(super) surface: ReceivedIoSurface,
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) format: HostSurfaceFormat,
    pub(super) popup_rect: Option<HostRect>,
}

#[derive(Debug)]
pub(super) struct ReceivedIoSurface(NonNull<c_void>);

// IOSurface Mach-port lookup returns a retained, thread-safe IOSurface object. Ownership moves
// through the supervisor queue and is released after the main thread replaces or drops the frame.
unsafe impl Send for ReceivedIoSurface {}

impl ReceivedIoSurface {
    pub(super) fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for ReceivedIoSurface {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0.as_ptr().cast()) };
    }
}

#[derive(Clone)]
pub(super) struct SurfaceSender {
    inner: Arc<SurfaceSenderInner>,
}

pub(super) struct SurfaceSendRequest {
    pub(super) view_id: ViewId,
    pub(super) lease_id: SurfaceLeaseId,
    pub(super) element: HostSurfaceElement,
    pub(super) io_surface: *mut c_void,
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) format: HostSurfaceFormat,
    pub(super) popup_rect: Option<HostRect>,
}

struct SurfaceSenderInner {
    destination: MachSendRight,
    auth: [u64; 2],
    generation: u64,
}

impl SurfaceSender {
    pub(super) fn connect(
        service_name: &str,
        auth: [u64; 2],
        generation: u64,
    ) -> Result<Self, String> {
        let service_name = CString::new(service_name)
            .map_err(|_| String::from("CEF surface service name contains a null byte"))?;
        let mut destination = MACH_PORT_NULL;
        check_mach(
            unsafe { bootstrap_look_up(bootstrap_port, service_name.as_ptr(), &mut destination) },
            "look up CEF surface service",
        )?;
        if destination == MACH_PORT_NULL {
            return Err(String::from("CEF surface service returned a null Mach port"));
        }
        Ok(Self {
            inner: Arc::new(SurfaceSenderInner {
                destination: MachSendRight(destination),
                auth,
                generation,
            }),
        })
    }

    pub(super) fn send(&self, frame: SurfaceSendRequest) -> Result<(), String> {
        let width = u32::try_from(frame.width).map_err(|_| {
            format!("CEF surface width {} exceeds Mach protocol limits", frame.width)
        })?;
        let height = u32::try_from(frame.height).map_err(|_| {
            format!("CEF surface height {} exceeds Mach protocol limits", frame.height)
        })?;
        let surface_port = unsafe { IOSurfaceCreateMachPort(frame.io_surface) };
        if surface_port == MACH_PORT_NULL {
            return Err(String::from("create Mach port for CEF IOSurface"));
        }
        let surface_port = MachSendRight(surface_port);
        let popup = frame.popup_rect.unwrap_or_default();
        let mut message = SurfaceMachMessage {
            header: MachMsgHeader {
                bits: MACH_MSGH_BITS_COMPLEX | MACH_MSG_TYPE_COPY_SEND,
                size: mem::size_of::<SurfaceMachMessage>() as MachMsgSize,
                remote_port: self.inner.destination.0,
                local_port: MACH_PORT_NULL,
                voucher_port: MACH_PORT_NULL,
                id: SURFACE_MESSAGE_ID,
            },
            body: MachMsgBody { descriptor_count: 1 },
            surface: MachMsgPortDescriptor {
                name: surface_port.0,
                pad1: 0,
                pad2: 0,
                disposition: MACH_MSG_TYPE_MOVE_SEND,
                descriptor_type: MACH_MSG_PORT_DESCRIPTOR,
            },
            auth_a: self.inner.auth[0],
            auth_b: self.inner.auth[1],
            generation: self.inner.generation,
            view_id: frame.view_id,
            lease_id: frame.lease_id,
            width,
            height,
            popup_x: popup.x,
            popup_y: popup.y,
            popup_width: popup.width,
            popup_height: popup.height,
            version: SURFACE_MESSAGE_VERSION,
            format: encode_format(frame.format),
            element: encode_element(frame.element),
            popup_present: u32::from(frame.popup_rect.is_some()),
        };
        let result = unsafe {
            mach_msg(
                &mut message.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                message.header.size,
                0,
                MACH_PORT_NULL,
                0,
                MACH_PORT_NULL,
            )
        };
        check_mach(result, "send CEF IOSurface Mach capability")?;
        mem::forget(surface_port);
        Ok(())
    }
}

struct MachSendRight(MachPort);

impl Drop for MachSendRight {
    fn drop(&mut self) {
        if self.0 != MACH_PORT_NULL {
            unsafe {
                mach_port_deallocate(task_self(), self.0);
            }
        }
    }
}

fn task_self() -> MachPort {
    unsafe { mach_task_self_ }
}

fn release_receive_port(port: MachPort) {
    if port == MACH_PORT_NULL {
        return;
    }
    let task = task_self();
    unsafe {
        mach_port_mod_refs(task, port, MACH_PORT_RIGHT_RECEIVE, -1);
        mach_port_deallocate(task, port);
    }
}

fn check_mach(result: KernReturn, action: &str) -> Result<(), String> {
    if result == KERN_SUCCESS {
        return Ok(());
    }
    let description = unsafe {
        let value = mach_error_string(result);
        if value.is_null() {
            None
        } else {
            Some(CStr::from_ptr(value).to_string_lossy().into_owned())
        }
    };
    match description {
        Some(description) => Err(format!("{action}: {description} ({result})")),
        None => Err(format!("{action}: Mach error {result}")),
    }
}

fn encode_format(format: HostSurfaceFormat) -> u32 {
    match format {
        HostSurfaceFormat::Bgra8888 => 1,
        HostSurfaceFormat::Rgba8888 => 2,
    }
}

fn decode_format(format: u32) -> Option<HostSurfaceFormat> {
    match format {
        1 => Some(HostSurfaceFormat::Bgra8888),
        2 => Some(HostSurfaceFormat::Rgba8888),
        _ => None,
    }
}

fn encode_element(element: HostSurfaceElement) -> u32 {
    match element {
        HostSurfaceElement::View => 1,
        HostSurfaceElement::Popup => 2,
    }
}

fn decode_element(element: u32) -> Option<HostSurfaceElement> {
    match element {
        1 => Some(HostSurfaceElement::View),
        2 => Some(HostSurfaceElement::Popup),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mach_surface_message_matches_darwin_layout() {
        assert_eq!(mem::size_of::<MachMsgHeader>(), 24);
        assert_eq!(mem::size_of::<MachMsgPortDescriptor>(), 12);
        assert_eq!(mem::offset_of!(SurfaceMachMessage, body), 24);
        assert_eq!(mem::offset_of!(SurfaceMachMessage, surface), 28);
        assert_eq!(mem::offset_of!(SurfaceMachMessage, auth_a), 40);
        assert_eq!(mem::size_of::<SurfaceMachMessage>() % 8, 0);
    }

    #[test]
    fn surface_wire_enums_reject_unknown_values() {
        assert!(decode_format(0).is_none());
        assert!(decode_element(0).is_none());
        assert!(matches!(decode_format(1), Some(HostSurfaceFormat::Bgra8888)));
        assert!(matches!(decode_element(2), Some(HostSurfaceElement::Popup)));
    }
}
