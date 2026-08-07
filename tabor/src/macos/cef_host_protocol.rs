use std::io::{self, Read, Write};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::display::browser_layout::BrowserViewportLayout;
use crate::ipc::{AgentDownload, IpcError};

pub const PROTOCOL_VERSION: u32 = 5;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub type ViewId = u64;
pub type RequestId = u64;
pub type SurfaceLeaseId = u64;

pub fn unix_deadline_after(timeout: Duration) -> io::Result<u64> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| io::Error::other("system clock is before the Unix epoch"))?;
    let now_millis = u64::try_from(now.as_millis())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "system clock is out of range"))?;
    let timeout_millis = u64::try_from(timeout.as_millis())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "timeout is too large"))?;
    now_millis
        .checked_add(timeout_millis)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "deadline overflow"))
}

pub fn remaining_until_unix_deadline(expires_at_unix_millis: u64) -> io::Result<Duration> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| io::Error::other("system clock is before the Unix epoch"))?;
    let now_millis = u64::try_from(now.as_millis())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "system clock is out of range"))?;
    expires_at_unix_millis
        .checked_sub(now_millis)
        .filter(|remaining| *remaining > 0)
        .map(Duration::from_millis)
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "CEF host request expired"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostGeometry {
    pub layout: BrowserViewportLayout,
    pub screen_rect: HostRect,
    pub scale_factor: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct HostRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSurfaceElement {
    View,
    Popup,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSurfaceFormat {
    Bgra8888,
    Rgba8888,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct HostMouseEvent {
    pub x: i32,
    pub y: i32,
    pub modifiers: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyEventKind {
    KeyDown,
    KeyUp,
    Char,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HostKeyEvent {
    pub kind: HostKeyEventKind,
    pub modifiers: u32,
    pub windows_key_code: i32,
    pub native_key_code: i32,
    pub character: u16,
    pub unmodified_character: u16,
    pub focus_on_editable_field: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostFrameEditCommand {
    Copy,
    Cut,
    Paste,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostJsDialogKind {
    Alert,
    Confirm,
    Prompt,
    BeforeUnloadReload,
    BeforeUnloadNavigate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HostCommand {
    Create {
        view_id: ViewId,
        url: String,
        geometry: HostGeometry,
    },
    Destroy {
        view_id: ViewId,
    },
    SetVisible {
        view_id: ViewId,
        visible: bool,
    },
    SetFocus {
        view_id: ViewId,
        focused: bool,
    },
    SyncEditableFocus {
        view_id: ViewId,
        editable: bool,
    },
    UpdateGeometry {
        view_id: ViewId,
        geometry: HostGeometry,
    },
    LoadUrl {
        view_id: ViewId,
        url: String,
    },
    Reload {
        view_id: ViewId,
    },
    GoBack {
        view_id: ViewId,
    },
    GoForward {
        view_id: ViewId,
    },
    MouseClick {
        view_id: ViewId,
        event: HostMouseEvent,
        button: HostMouseButton,
        mouse_up: bool,
        click_count: i32,
    },
    MouseMove {
        view_id: ViewId,
        event: HostMouseEvent,
        mouse_leave: bool,
    },
    MouseWheel {
        view_id: ViewId,
        event: HostMouseEvent,
        delta_x: i32,
        delta_y: i32,
    },
    ImeCommit {
        view_id: ViewId,
        text: String,
    },
    ImePreedit {
        view_id: ViewId,
        text: String,
        cursor_offset: Option<(usize, usize)>,
    },
    ImeCancel {
        view_id: ViewId,
    },
    KeyEvents {
        view_id: ViewId,
        events: Vec<HostKeyEvent>,
        invalidate_after: bool,
    },
    Evaluate {
        view_id: ViewId,
        request_id: RequestId,
        script: String,
        user_gesture: bool,
        expires_at_unix_millis: u64,
    },
    AgentEvaluate {
        view_id: ViewId,
        request_id: RequestId,
        script: String,
        user_gesture: bool,
        expires_at_unix_millis: u64,
    },
    FrameEdit {
        view_id: ViewId,
        command: HostFrameEditCommand,
    },
    DevTools {
        view_id: ViewId,
        request_id: RequestId,
        method: String,
        params: Option<JsonValue>,
        expires_at_unix_millis: u64,
    },
    RenewAgentEventCapture {
        view_id: ViewId,
    },
    RetainInspectorSession {
        view_id: ViewId,
    },
    ReleaseInspectorSession {
        view_id: ViewId,
    },
    SetFileInputFiles {
        view_id: ViewId,
        request_id: RequestId,
        element_id: String,
        paths: Vec<String>,
        expires_at_unix_millis: u64,
    },
    ShowInspector {
        view_id: ViewId,
    },
    SurfaceAcquired {
        view_id: ViewId,
        lease_id: SurfaceLeaseId,
    },
    JsDialogResult {
        view_id: ViewId,
        dialog_id: u64,
        accepted: bool,
        prompt_text: Option<String>,
    },
    SimulateMemoryPressureForTest {
        view_id: ViewId,
        request_id: RequestId,
        expires_at_unix_millis: u64,
    },
    CrashForTest,
    Shutdown,
}

impl HostCommand {
    pub fn view_id(&self) -> Option<ViewId> {
        match self {
            Self::Create { view_id, .. }
            | Self::Destroy { view_id }
            | Self::SetVisible { view_id, .. }
            | Self::SetFocus { view_id, .. }
            | Self::SyncEditableFocus { view_id, .. }
            | Self::UpdateGeometry { view_id, .. }
            | Self::LoadUrl { view_id, .. }
            | Self::Reload { view_id }
            | Self::GoBack { view_id }
            | Self::GoForward { view_id }
            | Self::MouseClick { view_id, .. }
            | Self::MouseMove { view_id, .. }
            | Self::MouseWheel { view_id, .. }
            | Self::ImeCommit { view_id, .. }
            | Self::ImePreedit { view_id, .. }
            | Self::ImeCancel { view_id }
            | Self::KeyEvents { view_id, .. }
            | Self::Evaluate { view_id, .. }
            | Self::AgentEvaluate { view_id, .. }
            | Self::FrameEdit { view_id, .. }
            | Self::DevTools { view_id, .. }
            | Self::RenewAgentEventCapture { view_id }
            | Self::RetainInspectorSession { view_id }
            | Self::ReleaseInspectorSession { view_id }
            | Self::SetFileInputFiles { view_id, .. }
            | Self::ShowInspector { view_id }
            | Self::SurfaceAcquired { view_id, .. }
            | Self::JsDialogResult { view_id, .. }
            | Self::SimulateMemoryPressureForTest { view_id, .. } => Some(*view_id),
            Self::CrashForTest | Self::Shutdown => None,
        }
    }

    pub fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Evaluate { request_id, .. }
            | Self::AgentEvaluate { request_id, .. }
            | Self::DevTools { request_id, .. }
            | Self::SetFileInputFiles { request_id, .. }
            | Self::SimulateMemoryPressureForTest { request_id, .. } => Some(*request_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HostEvent {
    Ready {
        protocol_version: u32,
        pid: u32,
        cef_version: String,
    },
    ViewReady {
        view_id: ViewId,
    },
    ViewFailed {
        view_id: ViewId,
        error: String,
    },
    PopupClosed {
        view_id: ViewId,
    },
    AccelerationFailed {
        view_id: ViewId,
        reason: String,
    },
    Title {
        view_id: ViewId,
        title: String,
    },
    Url {
        view_id: ViewId,
        url: String,
    },
    EditableFocus {
        view_id: ViewId,
        editable: bool,
    },
    OpenUrl {
        view_id: ViewId,
        url: String,
        new_tab: bool,
    },
    Downloads {
        view_id: ViewId,
        downloads: Vec<AgentDownload>,
    },
    EvaluateResult {
        view_id: ViewId,
        request_id: RequestId,
        result: Result<Option<String>, IpcError>,
    },
    DevToolsResult {
        view_id: ViewId,
        request_id: RequestId,
        result: Result<JsonValue, String>,
    },
    DevToolsEvent {
        view_id: ViewId,
        id: u64,
        payload: String,
    },
    FileInputResult {
        view_id: ViewId,
        request_id: RequestId,
        result: Result<String, IpcError>,
    },
    JsDialog {
        view_id: ViewId,
        dialog_id: u64,
        kind: HostJsDialogKind,
        origin_url: Option<String>,
        message_text: String,
        default_prompt_text: Option<String>,
    },
    JsDialogClosed {
        view_id: ViewId,
        dialog_id: u64,
    },
    TestResult {
        view_id: ViewId,
        request_id: RequestId,
        result: Result<(), String>,
    },
}

impl HostEvent {
    pub fn view_id(&self) -> Option<ViewId> {
        match self {
            Self::Ready { .. } => None,
            Self::ViewReady { view_id }
            | Self::ViewFailed { view_id, .. }
            | Self::PopupClosed { view_id }
            | Self::AccelerationFailed { view_id, .. }
            | Self::Title { view_id, .. }
            | Self::Url { view_id, .. }
            | Self::EditableFocus { view_id, .. }
            | Self::OpenUrl { view_id, .. }
            | Self::Downloads { view_id, .. }
            | Self::EvaluateResult { view_id, .. }
            | Self::DevToolsResult { view_id, .. }
            | Self::DevToolsEvent { view_id, .. }
            | Self::FileInputResult { view_id, .. }
            | Self::JsDialog { view_id, .. }
            | Self::JsDialogClosed { view_id, .. }
            | Self::TestResult { view_id, .. } => Some(*view_id),
        }
    }
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(message).map_err(io::Error::other)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "CEF host frame is too large"))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "CEF host frame is too large"));
    }
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "CEF host frame is too large"));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::{HostCommand, HostEvent, read_message, write_message};
    use crate::ipc::{IpcError, IpcErrorCode};

    #[test]
    fn protocol_round_trip_preserves_embedded_newlines() {
        let message = HostCommand::AgentEvaluate {
            view_id: 7,
            request_id: 11,
            script: String::from("one\ntwo"),
            user_gesture: true,
            expires_at_unix_millis: 42_000,
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).expect("serialize protocol message");
        let decoded: HostCommand =
            read_message(&mut bytes.as_slice()).expect("deserialize protocol message");
        match decoded {
            HostCommand::AgentEvaluate { script, user_gesture, expires_at_unix_millis, .. } => {
                assert_eq!(script, "one\ntwo");
                assert!(user_gesture);
                assert_eq!(expires_at_unix_millis, 42_000);
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn file_input_result_round_trip_preserves_ipc_error() {
        let event = HostEvent::FileInputResult {
            view_id: 7,
            request_id: 12,
            result: Err(IpcError::new(IpcErrorCode::Timeout, "upload timed out")),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &event).expect("serialize file input result");
        let decoded: HostEvent =
            read_message(&mut bytes.as_slice()).expect("deserialize file input result");
        match decoded {
            HostEvent::FileInputResult { result: Err(error), .. } => {
                assert_eq!(error.code, IpcErrorCode::Timeout);
                assert_eq!(error.message, "upload timed out");
            },
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn popup_closed_event_round_trips_with_view_identity() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &HostEvent::PopupClosed { view_id: 29 })
            .expect("serialize popup close");
        let decoded: HostEvent =
            read_message(&mut bytes.as_slice()).expect("deserialize popup close");
        assert_eq!(decoded.view_id(), Some(29));
        assert!(matches!(decoded, HostEvent::PopupClosed { view_id: 29 }));
    }
}
