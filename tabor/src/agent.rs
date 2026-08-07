use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use tabor_terminal::term::ClipboardType;

use crate::cli::{
    AgentAct, AgentClipboard, AgentClipboardCommand, AgentClipboardSet, AgentCommand, AgentEvents,
    AgentInspect, AgentOptions, AgentPdf, AgentRead, AgentScreenshot, AgentUpload, AgentUse,
    TerminalReadScopeArg,
};
use crate::clipboard::Clipboard;
use crate::ipc::{
    AGENT_APP_OPERATION_TIMEOUT, AGENT_APP_UPLOAD_TIMEOUT, AgentActResult, AgentAction,
    AgentDownload, AgentElementDetail, AgentEvent, AgentObservation, IpcConnection, IpcRequest,
    IpcTabGroup, IpcTabId, IpcTabState, IpcTerminalObservation, IpcTerminalRead,
    IpcTerminalReadScope, SocketReply, TerminalKeyInput, WebKeyModifiers, WebKeyState,
    agent_app_act_timeout, resolve_socket_path,
};
#[cfg(target_os = "macos")]
use crate::macos;

const CONTROLLER_START_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROLLER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CONTROLLER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROLLER_DISPATCH_MARGIN: Duration = Duration::from_secs(5);
const CONTROLLER_REPLY_MARGIN: Duration = Duration::from_secs(5);
const INTERNAL_SERVE_ARG: &str = "__tabor_agent_serve";

fn ipc_terminal_read_scope(scope: TerminalReadScopeArg) -> IpcTerminalReadScope {
    match scope {
        TerminalReadScopeArg::Viewport => IpcTerminalReadScope::Viewport,
        TerminalReadScopeArg::Buffer => IpcTerminalReadScope::Buffer,
        TerminalReadScopeArg::Selection => IpcTerminalReadScope::Selection,
    }
}

#[derive(Serialize, Deserialize)]
struct ControllerState {
    tabor_socket: PathBuf,
    control_socket: PathBuf,
    state_file: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControllerRequest {
    Ping,
    App,
    UseActive,
    UseTab { tab_id: IpcTabId },
    Observe,
    Read { scope: IpcTerminalReadScope, max_lines: Option<usize> },
    Inspect { element_id: String },
    Screenshot { path: Option<PathBuf>, full_page: bool, element_id: Option<String> },
    Events { since: Option<u64>, max: usize, kinds: Vec<String> },
    Pdf { path: Option<PathBuf> },
    Upload { element_id: String, paths: Vec<PathBuf> },
    Downloads,
    Act { actions: Vec<AgentAction>, observe: bool },
    ClipboardGet,
    ClipboardSet { text: String },
    Close,
}

impl ControllerRequest {
    fn app_operation_timeout(&self) -> Result<Option<Duration>, IoError> {
        match self {
            Self::Ping | Self::ClipboardGet | Self::ClipboardSet { .. } | Self::Close => Ok(None),
            Self::Upload { .. } => Ok(Some(AGENT_APP_UPLOAD_TIMEOUT)),
            Self::Act { actions, .. } => {
                agent_app_act_timeout(actions).map(Some).ok_or_else(|| {
                    IoError::new(ErrorKind::InvalidInput, "agent action timeout is too large")
                })
            },
            _ => Ok(Some(AGENT_APP_OPERATION_TIMEOUT)),
        }
    }

    fn controller_timeout(&self) -> Result<Duration, IoError> {
        self.app_operation_timeout()?
            .unwrap_or(Duration::ZERO)
            .checked_add(CONTROLLER_DISPATCH_MARGIN)
            .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "controller timeout is too large"))
    }

    fn controller_reply_timeout(&self) -> Result<Duration, IoError> {
        self.controller_timeout()?
            .checked_add(CONTROLLER_REPLY_MARGIN)
            .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "controller timeout is too large"))
    }
}

#[derive(Serialize, Deserialize)]
struct ControllerEnvelope {
    expires_at_unix_millis: u64,
    request: ControllerRequest,
}

impl ControllerEnvelope {
    fn new(request: ControllerRequest, valid_for: Duration) -> Result<Self, IoError> {
        let valid_for_millis = u64::try_from(valid_for.as_millis()).map_err(|_| {
            IoError::new(ErrorKind::InvalidInput, "controller timeout is too large")
        })?;
        let expires_at_unix_millis = unix_time_millis()?
            .checked_add(valid_for_millis)
            .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "controller deadline overflow"))?;
        Ok(Self { expires_at_unix_millis, request })
    }

    fn remaining(&self, maximum: Duration) -> Result<Duration, IoError> {
        let remaining_millis = self
            .expires_at_unix_millis
            .checked_sub(unix_time_millis()?)
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| {
                IoError::new(
                    ErrorKind::TimedOut,
                    "controller request expired before it could be dispatched",
                )
            })?;
        Ok(Duration::from_millis(remaining_millis).min(maximum))
    }
}

fn unix_time_millis() -> Result<u64, IoError> {
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| IoError::other("system clock is before the Unix epoch"))?
        .as_millis();
    u64::try_from(millis)
        .map_err(|_| IoError::new(ErrorKind::InvalidData, "system clock is out of range"))
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControllerReply {
    Attached { tabor_socket: PathBuf, control_socket: PathBuf },
    App { groups: Vec<IpcTabGroup>, selected_tab_id: Option<IpcTabId> },
    Use { tab: Box<IpcTabState> },
    Observation { observation: AgentObservation },
    TerminalObservation { observation: IpcTerminalObservation },
    TerminalRead { read: IpcTerminalRead },
    Element { element: AgentElementDetail },
    Screenshot { path: PathBuf, width: u32, height: u32 },
    Events { last_event_id: u64, events: Vec<AgentEvent> },
    Pdf { path: PathBuf },
    Upload { element: AgentElementDetail },
    Downloads { downloads: Vec<AgentDownload> },
    Act { result: AgentActResult },
    Clipboard { text: String },
    Closed,
    Pong,
    Error { error: String },
}

enum ClientOutcome {
    Continue,
    Close,
}

struct TaborConnection {
    socket: PathBuf,
    connection: Option<IpcConnection>,
}

impl TaborConnection {
    fn connect(socket: PathBuf) -> Result<Self, IoError> {
        let connection = IpcConnection::connect(Some(socket.clone()))?;
        Ok(Self { socket, connection: Some(connection) })
    }

    fn transaction<'a>(
        &'a mut self,
        timeout: Duration,
        client: &'a UnixStream,
    ) -> Result<TaborTransaction<'a>, IoError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "agent timeout is too large"))?;
        Ok(TaborTransaction { connection: self, client, deadline })
    }
}

struct TaborTransaction<'a> {
    connection: &'a mut TaborConnection,
    client: &'a UnixStream,
    deadline: Instant,
}

impl TaborTransaction<'_> {
    fn remaining(&self) -> Result<Duration, IoError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|timeout| !timeout.is_zero())
            .ok_or_else(|| {
                IoError::new(ErrorKind::TimedOut, "Tabor controller operation timed out")
            })
    }

    fn send_message(&mut self, request: &IpcRequest) -> Result<Option<SocketReply>, IoError> {
        if self.connection.connection.is_none() {
            self.connection.connection =
                Some(IpcConnection::connect(Some(self.connection.socket.clone()))?);
        }

        if peer_disconnected(self.client)? {
            return Err(IoError::new(
                ErrorKind::BrokenPipe,
                "controller client disconnected before Tabor request dispatch",
            ));
        }

        let timeout = self.remaining()?;
        if ipc_request_operation_timeout(request)?
            .is_some_and(|operation_timeout| timeout <= operation_timeout)
        {
            return Err(IoError::new(
                ErrorKind::TimedOut,
                "controller request no longer has enough time to start the Tabor operation",
            ));
        }

        let connection = self.connection.connection.as_mut().expect("connection initialized");
        let result = connection
            .set_timeout(Some(timeout))
            .and_then(|()| connection.send_message(request))
            .map_err(normalize_ipc_timeout);

        if !matches!(result, Ok(Some(_))) {
            self.connection.connection = None;
        }

        result
    }
}

fn ipc_request_operation_timeout(request: &IpcRequest) -> Result<Option<Duration>, IoError> {
    match request {
        IpcRequest::AgentAct { actions, .. } => agent_app_act_timeout(actions)
            .map(Some)
            .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "agent timeout is too large")),
        IpcRequest::AgentUpload { .. } => Ok(Some(AGENT_APP_UPLOAD_TIMEOUT)),
        IpcRequest::ListTabs
        | IpcRequest::GetTabState { .. }
        | IpcRequest::AgentObserve { .. }
        | IpcRequest::AgentInspect { .. }
        | IpcRequest::AgentScreenshot { .. }
        | IpcRequest::AgentEvents { .. }
        | IpcRequest::AgentPdf { .. }
        | IpcRequest::AgentDownloads { .. }
        | IpcRequest::TerminalObserve { .. }
        | IpcRequest::TerminalRead { .. }
        | IpcRequest::TerminalScreenshot { .. }
        | IpcRequest::TerminalKey { .. }
        | IpcRequest::SendInput { .. } => Ok(Some(AGENT_APP_OPERATION_TIMEOUT)),
        _ => Ok(None),
    }
}

fn normalize_ipc_timeout(error: IoError) -> IoError {
    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
        IoError::new(ErrorKind::TimedOut, format!("Tabor IPC transaction timed out: {error}"))
    } else {
        error
    }
}

pub fn run(options: AgentOptions) -> Result<(), Box<dyn Error>> {
    match options.command {
        AgentCommand::Attach => {
            let reply = ensure_attached(options.socket)?;
            print_reply(&reply)?;
        },
        AgentCommand::App => {
            let reply = send_request(options.socket, ControllerRequest::App)?;
            print_reply(&reply)?;
        },
        AgentCommand::Use(AgentUse { active, tab_id }) => {
            let request = if active {
                ControllerRequest::UseActive
            } else {
                let tab_id = tab_id.expect("tab id");
                ControllerRequest::UseTab {
                    tab_id: IpcTabId { index: tab_id.index, generation: tab_id.generation },
                }
            };
            let reply = send_request(options.socket, request)?;
            print_reply(&reply)?;
        },
        AgentCommand::Observe => {
            let reply = send_request(options.socket, ControllerRequest::Observe)?;
            print_reply(&reply)?;
        },
        AgentCommand::Read(AgentRead { scope, max_lines }) => {
            let reply = send_request(
                options.socket,
                ControllerRequest::Read { scope: ipc_terminal_read_scope(scope), max_lines },
            )?;
            print_reply(&reply)?;
        },
        AgentCommand::Inspect(AgentInspect { element_id }) => {
            let reply = send_request(options.socket, ControllerRequest::Inspect { element_id })?;
            print_reply(&reply)?;
        },
        AgentCommand::Screenshot(AgentScreenshot { path, full_page, element_id }) => {
            let reply = send_request(
                options.socket,
                ControllerRequest::Screenshot { path, full_page, element_id },
            )?;
            print_reply(&reply)?;
        },
        AgentCommand::Events(AgentEvents { since, max, kinds }) => {
            let reply =
                send_request(options.socket, ControllerRequest::Events { since, max, kinds })?;
            print_reply(&reply)?;
        },
        AgentCommand::Pdf(AgentPdf { path }) => {
            let reply = send_request(options.socket, ControllerRequest::Pdf { path })?;
            print_reply(&reply)?;
        },
        AgentCommand::Upload(AgentUpload { element_id, paths }) => {
            let reply =
                send_request(options.socket, ControllerRequest::Upload { element_id, paths })?;
            print_reply(&reply)?;
        },
        AgentCommand::Downloads => {
            let reply = send_request(options.socket, ControllerRequest::Downloads)?;
            print_reply(&reply)?;
        },
        AgentCommand::Act(AgentAct { actions_json, observe }) => {
            let actions: Vec<AgentAction> = serde_json::from_str(&actions_json)?;
            let reply = send_request(options.socket, ControllerRequest::Act { actions, observe })?;
            print_reply(&reply)?;
        },
        AgentCommand::Clipboard(AgentClipboard { command }) => {
            let request = match command {
                AgentClipboardCommand::Get => ControllerRequest::ClipboardGet,
                AgentClipboardCommand::Set(AgentClipboardSet { text }) => {
                    ControllerRequest::ClipboardSet { text }
                },
            };
            let reply = send_request(options.socket, request)?;
            print_reply(&reply)?;
        },
        AgentCommand::Close => {
            let reply = send_request(options.socket, ControllerRequest::Close)?;
            print_reply(&reply)?;
        },
    }

    Ok(())
}

pub fn maybe_run_internal_from_argv() -> Result<bool, Box<dyn Error>> {
    let mut args = std::env::args_os();
    let _ = args.next();
    let Some(command) = args.next() else {
        return Ok(false);
    };
    if command != INTERNAL_SERVE_ARG {
        return Ok(false);
    }

    let options = parse_internal_serve_args(args)?;
    serve(options)?;
    Ok(true)
}

fn ensure_attached(socket: Option<PathBuf>) -> Result<ControllerReply, Box<dyn Error>> {
    let tabor_socket = resolve_socket_path(socket)?;
    let state = controller_state_for(&tabor_socket);
    let startup_lock = open_startup_lock(&state.state_file)?;
    lock_startup(&startup_lock)?;

    if state.control_socket.exists() {
        match ping_controller(&state) {
            Ok(Some(ControllerReply::Pong)) => {
                persist_controller_state(&state)?;
                return Ok(ControllerReply::Attached {
                    tabor_socket: state.tabor_socket,
                    control_socket: state.control_socket,
                });
            },
            Ok(Some(_)) => return Err("unexpected controller ping reply".into()),
            Ok(None) => remove_file_if_exists(&state.control_socket)?,
            Err(err) => {
                return Err(format!("tabor agent controller did not respond: {err}").into());
            },
        }
    }

    remove_file_if_exists(&state.state_file)?;

    let current_exe = std::env::current_exe()?;
    let args = [
        OsString::from(INTERNAL_SERVE_ARG),
        OsString::from("--socket"),
        state.tabor_socket.clone().into_os_string(),
        OsString::from("--control-socket"),
        state.control_socket.clone().into_os_string(),
        OsString::from("--state-file"),
        state.state_file.clone().into_os_string(),
    ];
    crate::daemon::spawn_daemon_from_dir(current_exe, args, None)?;

    let deadline = Instant::now() + CONTROLLER_START_TIMEOUT;
    while Instant::now() < deadline {
        match ping_controller(&state) {
            Ok(Some(ControllerReply::Pong)) => {
                persist_controller_state(&state)?;
                return Ok(ControllerReply::Attached {
                    tabor_socket: state.tabor_socket,
                    control_socket: state.control_socket,
                });
            },
            Ok(Some(_)) => return Err("unexpected controller ping reply".into()),
            Ok(None) => {},
            Err(err) => {
                return Err(format!("new tabor agent controller did not respond: {err}").into());
            },
        }
        std::thread::sleep(CONTROLLER_POLL_INTERVAL);
    }

    Err("timed out waiting for tabor agent controller".into())
}

fn send_request(
    socket: Option<PathBuf>,
    request: ControllerRequest,
) -> Result<ControllerReply, Box<dyn Error>> {
    let state = match load_state(socket.clone()) {
        Ok(state) => state,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let _ = ensure_attached(socket.clone())?;
            load_state(socket)?
        },
        Err(err) => return Err(err.into()),
    };

    let reply = send_controller_request(&state, request)?;
    match &reply {
        ControllerReply::Error { error } => Err(error.clone().into()),
        _ => Ok(reply),
    }
}

fn ping_controller(state: &ControllerState) -> Result<Option<ControllerReply>, Box<dyn Error>> {
    let envelope = ControllerEnvelope::new(ControllerRequest::Ping, CONTROLLER_REQUEST_TIMEOUT)?;
    let stream = match UnixStream::connect(&state.control_socket) {
        Ok(stream) => stream,
        Err(err) if matches!(err.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
            return Ok(None);
        },
        Err(err) => return Err(err.into()),
    };
    send_controller_request_on_stream(stream, envelope, CONTROLLER_REQUEST_TIMEOUT).map(Some)
}

fn send_controller_request(
    state: &ControllerState,
    request: ControllerRequest,
) -> Result<ControllerReply, Box<dyn Error>> {
    let controller_timeout = request.controller_timeout()?;
    let reply_timeout = request.controller_reply_timeout()?;
    let envelope = ControllerEnvelope::new(request, controller_timeout)?;
    let stream = UnixStream::connect(&state.control_socket)?;
    send_controller_request_on_stream(stream, envelope, reply_timeout)
}

fn send_controller_request_on_stream(
    mut stream: UnixStream,
    envelope: ControllerEnvelope,
    timeout: Duration,
) -> Result<ControllerReply, Box<dyn Error>> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let json = serde_json::to_string(&envelope)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err("empty controller reply".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn serve(options: InternalServeOptions) -> Result<(), Box<dyn Error>> {
    let listener = UnixListener::bind(&options.control_socket)?;
    let socket_identity = socket_identity(&options.control_socket)?;
    let result = serve_loop(&options, listener);

    let cleanup_result = cleanup_controller_files(&options, socket_identity);

    result.and(cleanup_result)
}

fn serve_loop(
    options: &InternalServeOptions,
    listener: UnixListener,
) -> Result<(), Box<dyn Error>> {
    let mut tabor = TaborConnection::connect(options.socket.clone())?;
    let state = ControllerState {
        tabor_socket: options.socket.clone(),
        control_socket: options.control_socket.clone(),
        state_file: options.state_file.clone(),
    };
    persist_controller_state(&state)?;

    let mut selected_tab_id = None;
    loop {
        let (stream, _) = listener.accept()?;
        if matches!(serve_client(&mut tabor, &mut selected_tab_id, stream)?, ClientOutcome::Close) {
            break;
        }
    }

    Ok(())
}

fn serve_client(
    tabor: &mut TaborConnection,
    selected_tab_id: &mut Option<IpcTabId>,
    mut stream: UnixStream,
) -> Result<ClientOutcome, serde_json::Error> {
    if stream.set_read_timeout(Some(CONTROLLER_REQUEST_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(CONTROLLER_REQUEST_TIMEOUT)).is_err()
    {
        return Ok(ClientOutcome::Continue);
    }

    let envelope = match read_request(&stream) {
        Ok(envelope) => envelope,
        Err(err) => {
            let reply = serde_json::to_string(&ControllerReply::Error { error: err.to_string() })?;
            let _ = write_reply(&mut stream, &reply);
            return Ok(ClientOutcome::Continue);
        },
    };
    let reply = match dispatch_controller_request(tabor, selected_tab_id, &stream, envelope) {
        Ok(reply) => reply,
        Err(err) => ControllerReply::Error { error: err.to_string() },
    };
    let should_exit = matches!(reply, ControllerReply::Closed);
    let reply = serde_json::to_string(&reply)?;
    let _ = write_reply(&mut stream, &reply);
    Ok(if should_exit { ClientOutcome::Close } else { ClientOutcome::Continue })
}

fn dispatch_controller_request(
    tabor: &mut TaborConnection,
    selected_tab_id: &mut Option<IpcTabId>,
    stream: &UnixStream,
    envelope: ControllerEnvelope,
) -> Result<ControllerReply, Box<dyn Error>> {
    let app_timeout = envelope.request.app_operation_timeout()?;
    let remaining = envelope.remaining(envelope.request.controller_timeout()?)?;
    if app_timeout.is_some_and(|app_timeout| remaining <= app_timeout) {
        return Err(IoError::new(
            ErrorKind::TimedOut,
            "controller request expired while waiting to be dispatched",
        )
        .into());
    }
    if peer_disconnected(stream)? {
        return Err(IoError::new(
            ErrorKind::BrokenPipe,
            "controller client disconnected before request dispatch",
        )
        .into());
    }
    handle_request(tabor, selected_tab_id, stream, envelope.request, remaining)
}

fn peer_disconnected(stream: &UnixStream) -> Result<bool, IoError> {
    let mut byte = 0_u8;
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    if result > 0 {
        return Ok(false);
    }

    let error = IoError::last_os_error();
    if error.kind() == ErrorKind::WouldBlock { Ok(false) } else { Err(error) }
}

fn persist_controller_state(state: &ControllerState) -> Result<(), Box<dyn Error>> {
    let contents = serde_json::to_vec(state)?;
    match fs::read(&state.state_file) {
        Ok(existing) if existing == contents => return Ok(()),
        Ok(_) => {},
        Err(err) if err.kind() == ErrorKind::NotFound => {},
        Err(err) => return Err(err.into()),
    }

    let temp_file = state.state_file.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp_file, contents)?;
    fs::rename(temp_file, &state.state_file)?;
    Ok(())
}

fn socket_identity(path: &Path) -> Result<(u64, u64), IoError> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

fn cleanup_controller_files(
    options: &InternalServeOptions,
    identity: (u64, u64),
) -> Result<(), Box<dyn Error>> {
    let startup_lock = open_startup_lock(&options.state_file)?;
    lock_startup(&startup_lock)?;
    if !matches!(
        socket_identity(&options.control_socket),
        Ok(current_identity) if current_identity == identity
    ) {
        return Ok(());
    }
    remove_file_if_exists(&options.control_socket)?;
    remove_file_if_exists(&options.state_file)?;
    Ok(())
}

fn open_startup_lock(state_file: &Path) -> Result<File, IoError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_file.with_extension("lock"))
}

fn lock_startup(lock_file: &File) -> Result<(), IoError> {
    loop {
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }

        let err = IoError::last_os_error();
        if err.kind() != ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), IoError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn handle_request(
    tabor: &mut TaborConnection,
    selected_tab_id: &mut Option<IpcTabId>,
    client: &UnixStream,
    request: ControllerRequest,
    controller_timeout: Duration,
) -> Result<ControllerReply, Box<dyn Error>> {
    let mut transaction = tabor.transaction(controller_timeout, client)?;
    let tabor = &mut transaction;
    match request {
        ControllerRequest::Ping => Ok(ControllerReply::Pong),
        ControllerRequest::App => {
            let groups = list_tabs(tabor)?;
            Ok(ControllerReply::App { groups, selected_tab_id: *selected_tab_id })
        },
        ControllerRequest::UseActive => {
            let groups = list_tabs(tabor)?;
            let tab = groups
                .iter()
                .flat_map(|group| group.tabs.iter())
                .find(|tab| tab.is_active)
                .cloned()
                .ok_or_else(|| IoError::new(ErrorKind::NotFound, "No active tab"))?;
            *selected_tab_id = Some(tab.tab_id);
            Ok(ControllerReply::Use { tab: Box::new(tab) })
        },
        ControllerRequest::UseTab { tab_id } => {
            let reply = send_tabor_request(tabor, &IpcRequest::GetTabState { tab_id })?;
            let SocketReply::TabState { tab } = reply else {
                return Err(IoError::other("unexpected tab state reply").into());
            };
            *selected_tab_id = Some(tab_id);
            Ok(ControllerReply::Use { tab })
        },
        ControllerRequest::Observe => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            match &tab.kind {
                crate::ipc::IpcTabKind::Terminal => {
                    let reply = send_tabor_request(
                        tabor,
                        &IpcRequest::TerminalObserve { tab_id: Some(tab.tab_id) },
                    )?;
                    let SocketReply::TerminalObservation { observation } = reply else {
                        return Err(IoError::other("unexpected terminal observe reply").into());
                    };
                    Ok(ControllerReply::TerminalObservation { observation })
                },
                crate::ipc::IpcTabKind::Web { .. } => {
                    let reply = send_tabor_request(
                        tabor,
                        &IpcRequest::AgentObserve { tab_id: Some(tab.tab_id) },
                    )?;
                    let SocketReply::AgentObservation { observation } = reply else {
                        return Err(IoError::other("unexpected observe reply").into());
                    };
                    Ok(ControllerReply::Observation { observation })
                },
                _ => Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "Observe is only supported for terminal and web tabs",
                )
                .into()),
            }
        },
        ControllerRequest::Read { scope, max_lines } => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            if !matches!(&tab.kind, crate::ipc::IpcTabKind::Terminal) {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "Read is only supported for terminal tabs",
                )
                .into());
            }
            let reply = send_tabor_request(
                tabor,
                &IpcRequest::TerminalRead { tab_id: Some(tab.tab_id), scope, max_lines },
            )?;
            let SocketReply::TerminalRead { read } = reply else {
                return Err(IoError::other("unexpected terminal read reply").into());
            };
            Ok(ControllerReply::TerminalRead { read })
        },
        ControllerRequest::Inspect { element_id } => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            if !matches!(&tab.kind, crate::ipc::IpcTabKind::Web { .. }) {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "Inspect is only supported for web tabs",
                )
                .into());
            }
            let reply = send_tabor_request(
                tabor,
                &IpcRequest::AgentInspect { tab_id: Some(tab.tab_id), element_id },
            )?;
            let SocketReply::AgentElement { element } = reply else {
                return Err(IoError::other("unexpected inspect reply").into());
            };
            Ok(ControllerReply::Element { element })
        },
        ControllerRequest::Screenshot { path, full_page, element_id } => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            let screenshot = match &tab.kind {
                crate::ipc::IpcTabKind::Terminal => {
                    if full_page {
                        return Err(IoError::new(
                            ErrorKind::InvalidInput,
                            "full-page screenshots are only supported for web tabs",
                        )
                        .into());
                    }
                    if element_id.is_some() {
                        return Err(IoError::new(
                            ErrorKind::InvalidInput,
                            "element screenshots are only supported for web tabs",
                        )
                        .into());
                    }
                    let reply = send_tabor_request(
                        tabor,
                        &IpcRequest::TerminalScreenshot { tab_id: Some(tab.tab_id) },
                    )?;
                    let SocketReply::TerminalScreenshot { screenshot } = reply else {
                        return Err(IoError::other("unexpected terminal screenshot reply").into());
                    };
                    screenshot
                },
                crate::ipc::IpcTabKind::Web { .. } => {
                    let reply = send_tabor_request(
                        tabor,
                        &IpcRequest::AgentScreenshot { tab_id: Some(tab.tab_id), full_page },
                    )?;
                    let SocketReply::AgentScreenshot { screenshot } = reply else {
                        return Err(IoError::other("unexpected screenshot reply").into());
                    };
                    screenshot
                },
                _ => {
                    return Err(IoError::new(
                        ErrorKind::InvalidInput,
                        "Screenshot is only supported for terminal and web tabs",
                    )
                    .into());
                },
            };
            let mut png = BASE64.decode(screenshot.data_base64.as_bytes())?;
            let image = image::load_from_memory(&png)?;
            let (image_width, image_height) = image.dimensions();
            let mut width = if screenshot.width == 0 { image_width } else { screenshot.width };
            let mut height = if screenshot.height == 0 { image_height } else { screenshot.height };

            if let Some(element_id) = element_id {
                let element = inspect_element(tabor, tab.tab_id, element_id)?;
                let bbox =
                    element.bbox.ok_or_else(|| IoError::other("element has no bounding box"))?;
                let x = bbox.x.max(0) as u32;
                let y = bbox.y.max(0) as u32;
                let crop_width = bbox.width.max(1) as u32;
                let crop_height = bbox.height.max(1) as u32;
                let crop_x = x.min(image_width.saturating_sub(1));
                let crop_y = y.min(image_height.saturating_sub(1));
                let max_width = image_width.saturating_sub(crop_x).max(1);
                let max_height = image_height.saturating_sub(crop_y).max(1);
                width = crop_width.min(max_width);
                height = crop_height.min(max_height);
                let cropped = image.crop_imm(crop_x, crop_y, width, height);
                let mut cursor = std::io::Cursor::new(Vec::new());
                cropped.write_to(&mut cursor, image::ImageFormat::Png)?;
                png = cursor.into_inner();
            }

            let path = materialize_artifact(path, "screenshot", "png", &png)?;
            Ok(ControllerReply::Screenshot { path, width, height })
        },
        ControllerRequest::Events { since, max, kinds } => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            if !matches!(&tab.kind, crate::ipc::IpcTabKind::Web { .. }) {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "Events are only supported for web tabs",
                )
                .into());
            }
            let reply = send_tabor_request(
                tabor,
                &IpcRequest::AgentEvents {
                    tab_id: Some(tab.tab_id),
                    since,
                    max: Some(max),
                    kinds: (!kinds.is_empty()).then_some(kinds),
                },
            )?;
            let SocketReply::AgentEvents { last_event_id, events } = reply else {
                return Err(IoError::other("unexpected events reply").into());
            };
            Ok(ControllerReply::Events { last_event_id, events })
        },
        ControllerRequest::Pdf { path } => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            if !matches!(&tab.kind, crate::ipc::IpcTabKind::Web { .. }) {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "PDF export is only supported for web tabs",
                )
                .into());
            }
            let reply =
                send_tabor_request(tabor, &IpcRequest::AgentPdf { tab_id: Some(tab.tab_id) })?;
            let SocketReply::AgentPdf { pdf } = reply else {
                return Err(IoError::other("unexpected pdf reply").into());
            };
            let bytes = BASE64.decode(pdf.data_base64.as_bytes())?;
            let path = materialize_artifact(path, "page", "pdf", &bytes)?;
            Ok(ControllerReply::Pdf { path })
        },
        ControllerRequest::Upload { element_id, paths } => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            if !matches!(&tab.kind, crate::ipc::IpcTabKind::Web { .. }) {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "Upload is only supported for web tabs",
                )
                .into());
            }
            let reply = send_tabor_request(
                tabor,
                &IpcRequest::AgentUpload {
                    tab_id: Some(tab.tab_id),
                    element_id,
                    paths: paths
                        .into_iter()
                        .map(|path| path.to_string_lossy().to_string())
                        .collect(),
                },
            )?;
            let SocketReply::AgentElement { element } = reply else {
                return Err(IoError::other("unexpected upload reply").into());
            };
            Ok(ControllerReply::Upload { element })
        },
        ControllerRequest::Downloads => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            if !matches!(&tab.kind, crate::ipc::IpcTabKind::Web { .. }) {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "Downloads are only supported for web tabs",
                )
                .into());
            }
            let reply = send_tabor_request(
                tabor,
                &IpcRequest::AgentDownloads { tab_id: Some(tab.tab_id) },
            )?;
            let SocketReply::AgentDownloads { downloads } = reply else {
                return Err(IoError::other("unexpected downloads reply").into());
            };
            Ok(ControllerReply::Downloads { downloads })
        },
        ControllerRequest::Act { actions, observe } => {
            let tab = selected_tab_state(tabor, *selected_tab_id)?;
            match &tab.kind {
                crate::ipc::IpcTabKind::Terminal => {
                    let result = run_terminal_actions(tabor, tab.tab_id, actions, observe)?;
                    Ok(ControllerReply::Act { result })
                },
                crate::ipc::IpcTabKind::Web { .. } => {
                    let reply = send_tabor_request(
                        tabor,
                        &IpcRequest::AgentAct { tab_id: Some(tab.tab_id), actions, observe },
                    )?;
                    let SocketReply::AgentAct { result } = reply else {
                        return Err(IoError::other("unexpected act reply").into());
                    };
                    Ok(ControllerReply::Act { result })
                },
                _ => Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "Act is only supported for terminal and web tabs",
                )
                .into()),
            }
        },
        ControllerRequest::ClipboardGet => {
            let mut clipboard = Clipboard::default();
            let text = clipboard.load(ClipboardType::Clipboard);
            Ok(ControllerReply::Clipboard { text })
        },
        ControllerRequest::ClipboardSet { text } => {
            let mut clipboard = Clipboard::default();
            clipboard.store(ClipboardType::Clipboard, text.clone());
            Ok(ControllerReply::Clipboard { text })
        },
        ControllerRequest::Close => Ok(ControllerReply::Closed),
    }
}

fn selected_tab_state(
    tabor: &mut TaborTransaction<'_>,
    selected_tab_id: Option<IpcTabId>,
) -> Result<Box<IpcTabState>, Box<dyn Error>> {
    let tab_id =
        selected_tab_id.ok_or_else(|| IoError::new(ErrorKind::NotFound, "No selected tab"))?;
    let reply = send_tabor_request(tabor, &IpcRequest::GetTabState { tab_id })?;
    let SocketReply::TabState { tab } = reply else {
        return Err(IoError::other("unexpected tab state reply").into());
    };
    Ok(tab)
}

fn run_terminal_actions(
    tabor: &mut TaborTransaction<'_>,
    tab_id: IpcTabId,
    actions: Vec<AgentAction>,
    observe: bool,
) -> Result<AgentActResult, Box<dyn Error>> {
    let mut results = Vec::with_capacity(actions.len());

    for (index, action) in actions.into_iter().enumerate() {
        let outcome = dispatch_terminal_action(tabor, tab_id, action);
        match outcome {
            Ok(()) => results.push(crate::ipc::AgentActionReport { index, ok: true, error: None }),
            Err(err) => {
                results.push(crate::ipc::AgentActionReport {
                    index,
                    ok: false,
                    error: Some(err.to_string()),
                });
                return Ok(AgentActResult {
                    results,
                    observation: None,
                    terminal_observation: None,
                });
            },
        }
    }

    let terminal_observation = if observe {
        let reply =
            send_tabor_request(tabor, &IpcRequest::TerminalObserve { tab_id: Some(tab_id) })?;
        let SocketReply::TerminalObservation { observation } = reply else {
            return Err(IoError::other("unexpected terminal observe reply").into());
        };
        Some(observation)
    } else {
        None
    };

    Ok(AgentActResult { results, observation: None, terminal_observation })
}

fn dispatch_terminal_action(
    tabor: &mut TaborTransaction<'_>,
    tab_id: IpcTabId,
    action: AgentAction,
) -> Result<(), Box<dyn Error>> {
    match action {
        AgentAction::Type { text } | AgentAction::Paste { text } => expect_ok(send_tabor_request(
            tabor,
            &IpcRequest::SendInput { tab_id: Some(tab_id), text },
        )?),
        AgentAction::Press { key, modifiers } => {
            dispatch_terminal_key(tabor, tab_id, key.clone(), modifiers, WebKeyState::Down)?;
            dispatch_terminal_key(tabor, tab_id, key, modifiers, WebKeyState::Up)
        },
        AgentAction::KeyDown { key, modifiers } => {
            dispatch_terminal_key(tabor, tab_id, key, modifiers, WebKeyState::Down)
        },
        AgentAction::KeyUp { key, modifiers } => {
            dispatch_terminal_key(tabor, tab_id, key, modifiers, WebKeyState::Up)
        },
        AgentAction::Wait { ms: Some(ms), .. } => {
            std::thread::sleep(Duration::from_millis(ms));
            Ok(())
        },
        AgentAction::Wait { .. } => Err(IoError::new(
            ErrorKind::InvalidInput,
            "Terminal wait only supports explicit ms delays",
        )
        .into()),
        other => Err(IoError::new(
            ErrorKind::InvalidInput,
            format!("Terminal tabs do not support action {other:?}"),
        )
        .into()),
    }
}

fn dispatch_terminal_key(
    tabor: &mut TaborTransaction<'_>,
    tab_id: IpcTabId,
    key: String,
    modifiers: WebKeyModifiers,
    state: WebKeyState,
) -> Result<(), Box<dyn Error>> {
    expect_ok(send_tabor_request(
        tabor,
        &IpcRequest::TerminalKey {
            tab_id: Some(tab_id),
            input: TerminalKeyInput { key, text: None, modifiers, repeat: false, state },
        },
    )?)
}

fn expect_ok(reply: SocketReply) -> Result<(), Box<dyn Error>> {
    match reply {
        SocketReply::Ok => Ok(()),
        other => Err(IoError::other(format!("unexpected reply: {other:?}")).into()),
    }
}

fn list_tabs(tabor: &mut TaborTransaction<'_>) -> Result<Vec<IpcTabGroup>, Box<dyn Error>> {
    let reply = send_tabor_request(tabor, &IpcRequest::ListTabs)?;
    let SocketReply::TabList { groups } = reply else {
        return Err(IoError::other("unexpected tab list reply").into());
    };
    Ok(groups)
}

fn inspect_element(
    tabor: &mut TaborTransaction<'_>,
    tab_id: IpcTabId,
    element_id: String,
) -> Result<AgentElementDetail, Box<dyn Error>> {
    let reply =
        send_tabor_request(tabor, &IpcRequest::AgentInspect { tab_id: Some(tab_id), element_id })?;
    let SocketReply::AgentElement { element } = reply else {
        return Err(IoError::other("unexpected inspect reply").into());
    };
    Ok(element)
}

fn send_tabor_request(
    tabor: &mut TaborTransaction<'_>,
    request: &IpcRequest,
) -> Result<SocketReply, Box<dyn Error>> {
    let reply = tabor
        .send_message(request)?
        .ok_or_else(|| IoError::new(ErrorKind::UnexpectedEof, "missing Tabor IPC reply"))?;

    match reply {
        SocketReply::Error { error } => Err(error.message.into()),
        other => Ok(other),
    }
}

fn materialize_artifact(
    requested_path: Option<PathBuf>,
    prefix: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf, Box<dyn Error>> {
    let path = requested_path.unwrap_or_else(|| {
        let nanos =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos();
        #[cfg(target_os = "macos")]
        let base_dir = macos::runtime_tmp_dir();
        #[cfg(not(target_os = "macos"))]
        let base_dir = std::env::temp_dir();
        base_dir.join(format!("tabor-agent-{prefix}-{nanos}.{extension}"))
    });
    fs::write(&path, bytes)?;
    Ok(path)
}

fn read_request(stream: &UnixStream) -> Result<ControllerEnvelope, Box<dyn Error>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err(IoError::new(ErrorKind::UnexpectedEof, "empty controller request").into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn write_reply(stream: &mut UnixStream, reply: &str) -> Result<(), IoError> {
    stream.write_all(reply.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn load_state(socket: Option<PathBuf>) -> Result<ControllerState, IoError> {
    let state_file = controller_state_for(&resolve_socket_path(socket)?).state_file;
    let contents = fs::read_to_string(state_file)?;
    serde_json::from_str(&contents)
        .map_err(|err| IoError::new(ErrorKind::InvalidData, format!("invalid state file: {err}")))
}

fn print_reply(reply: &ControllerReply) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string(reply)?);
    Ok(())
}

fn controller_state_for(socket: &Path) -> ControllerState {
    let parent = socket.parent().unwrap_or_else(|| Path::new("/tmp"));
    let file_name = socket
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("tabor.sock");
    let state_file = parent.join(format!(".{file_name}.agent.json"));
    let control_socket = parent.join(format!(".{file_name}.agent.sock"));
    ControllerState { tabor_socket: socket.to_path_buf(), control_socket, state_file }
}

struct InternalServeOptions {
    socket: PathBuf,
    control_socket: PathBuf,
    state_file: PathBuf,
}

fn parse_internal_serve_args(
    mut args: impl Iterator<Item = OsString>,
) -> Result<InternalServeOptions, Box<dyn Error>> {
    let mut socket = None;
    let mut control_socket = None;
    let mut state_file = None;

    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--control-socket" => control_socket = args.next().map(PathBuf::from),
            "--state-file" => state_file = args.next().map(PathBuf::from),
            other => {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("unknown internal agent arg: {other}"),
                )
                .into());
            },
        }
    }

    let socket = socket.ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "missing --socket"))?;
    let control_socket = control_socket
        .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "missing --control-socket"))?;
    let state_file =
        state_file.ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "missing --state-file"))?;

    Ok(InternalServeOptions { socket, control_socket, state_file })
}
