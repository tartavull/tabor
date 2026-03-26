use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::io::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tabor_terminal::event::{Event as TerminalEvent, EventListener, Notify, OnResize, WindowSize};
use tabor_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use tabor_terminal::grid::Dimensions;
use tabor_terminal::sync::FairMutex;
use tabor_terminal::term::test::TermSize;
use tabor_terminal::term::{Term, TermSnapshot};
use tabor_terminal::tty;

use crate::cli::{WorkspaceCommand, WorkspaceOptions};
#[cfg(target_os = "macos")]
use crate::macos;

pub(crate) const WORKSPACE_PROTOCOL_VERSION: u32 = 1;
const BROKER_START_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const BROKER_IDLE_WAIT: Duration = Duration::from_millis(25);
const INTERNAL_SERVE_ARG: &str = "__tabor_workspace_serve";

#[derive(Serialize, Deserialize)]
struct BrokerRuntimeState {
    control_socket: PathBuf,
    persisted_state_file: PathBuf,
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedWorkspaceBrokerState {
    next_terminal_id: u64,
    terminals: Vec<PersistedTerminalState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedTerminalState {
    pub id: u64,
    pub launch_options: tty::Options,
    pub snapshot: TermSnapshot,
    pub revision: u64,
    pub title: Option<String>,
    pub program_name: String,
    pub working_directory: Option<PathBuf>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceBrokerTerminalStatus {
    pub id: u64,
    pub revision: u64,
    pub title: Option<String>,
    pub program_name: String,
    pub working_directory: Option<PathBuf>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceBrokerTerminalSnapshot {
    pub status: WorkspaceBrokerTerminalStatus,
    pub snapshot: TermSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceBrokerStatus {
    pub terminals: Vec<WorkspaceBrokerTerminalStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceTabLayout {
    pub persistent_id: String,
    pub custom_title: Option<String>,
    pub terminal_view_mode: crate::display::terminal_layout::TerminalViewMode,
    pub terminal_multi_column_count_override: Option<usize>,
    pub kind: WorkspaceTabKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkspaceTabKind {
    Terminal {
        broker_id: u64,
        launch_options: tty::Options,
    },
    Web {
        url: String,
        browser_view_mode: crate::display::browser_layout::BrowserViewMode,
        browser_multi_column_count_override: Option<usize>,
    },
    Image {
        source: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceGroupLayout {
    pub name: Option<String>,
    pub tabs: Vec<WorkspaceTabLayout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceLayout {
    pub protocol_version: u32,
    pub active_tab_id: Option<String>,
    pub groups: Vec<WorkspaceGroupLayout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SerializableWindowSize {
    num_lines: u16,
    num_cols: u16,
    cell_width: u16,
    cell_height: u16,
}

impl From<WindowSize> for SerializableWindowSize {
    fn from(value: WindowSize) -> Self {
        Self {
            num_lines: value.num_lines,
            num_cols: value.num_cols,
            cell_width: value.cell_width,
            cell_height: value.cell_height,
        }
    }
}

impl From<SerializableWindowSize> for WindowSize {
    fn from(value: SerializableWindowSize) -> Self {
        Self {
            num_lines: value.num_lines,
            num_cols: value.num_cols,
            cell_width: value.cell_width,
            cell_height: value.cell_height,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BrokerRequest {
    Ping,
    Status,
    CreateTerminal { launch_options: tty::Options, window_size: SerializableWindowSize },
    RestartTerminal { id: u64 },
    SendInput { id: u64, data: Vec<u8> },
    Resize { id: u64, window_size: SerializableWindowSize },
    Snapshot { id: u64 },
    CloseTerminal { id: u64 },
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BrokerReply {
    Pong,
    Status { status: WorkspaceBrokerStatus },
    Created { terminal: WorkspaceBrokerTerminalSnapshot },
    Restarted { terminal: WorkspaceBrokerTerminalSnapshot },
    Snapshot { terminal: WorkspaceBrokerTerminalSnapshot },
    Closed,
    Stopped,
    Error { error: String },
}

struct RuntimeTerminalState {
    id: u64,
    launch_options: tty::Options,
    terminal: Arc<FairMutex<Term<BrokerEventProxy>>>,
    notifier: Notifier,
    #[cfg(unix)]
    master_fd: RawFd,
    #[cfg(unix)]
    shell_pid: u32,
    revision: u64,
    title: Option<String>,
    program_name: String,
    working_directory: Option<PathBuf>,
    exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
struct BrokerEventRecord {
    terminal_id: u64,
    event: TerminalEvent,
}

#[derive(Clone)]
struct BrokerEventProxy {
    terminal_id: u64,
    sender: Sender<BrokerEventRecord>,
}

impl EventListener for BrokerEventProxy {
    fn send_event(&self, event: TerminalEvent) {
        let _ = self.sender.send(BrokerEventRecord { terminal_id: self.terminal_id, event });
    }
}

pub(crate) fn maybe_run_internal_from_argv() -> Result<bool, Box<dyn Error>> {
    let mut args = std::env::args_os();
    let _ = args.next();
    let Some(command) = args.next() else {
        return Ok(false);
    };
    if command != INTERNAL_SERVE_ARG {
        return Ok(false);
    }

    let mut control_socket = None;
    let mut persisted_state_file = None;
    while let Some(arg) = args.next() {
        if arg == "--control-socket" {
            control_socket = args.next().map(PathBuf::from);
        } else if arg == "--persisted-state-file" {
            persisted_state_file = args.next().map(PathBuf::from);
        }
    }

    let control_socket = control_socket
        .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "missing control socket"))?;
    let persisted_state_file = persisted_state_file
        .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "missing persisted state file"))?;
    serve(control_socket, persisted_state_file)?;
    Ok(true)
}

pub(crate) fn run(options: WorkspaceOptions) -> Result<(), Box<dyn Error>> {
    match options.command {
        WorkspaceCommand::Status => {
            let status = status()?;
            println!("{}", serde_json::to_string(&status)?);
        },
        WorkspaceCommand::Stop => {
            stop_workspace()?;
        },
        WorkspaceCommand::RestartTerminal { id } => {
            let terminal = restart_terminal(id)?;
            println!("{}", serde_json::to_string(&terminal)?);
        },
    }
    Ok(())
}

pub(crate) fn ensure_broker_running() -> Result<(), Box<dyn Error>> {
    let runtime = runtime_state_path();

    if let Ok(state) = load_runtime_state() {
        if matches!(
            send_request(&state.control_socket, &BrokerRequest::Ping),
            Ok(BrokerReply::Pong)
        ) {
            return Ok(());
        }
        let _ = fs::remove_file(&state.control_socket);
    }

    let control_socket = control_socket_path();
    let persisted_state_file = persisted_state_file();
    let _ = fs::remove_file(&runtime);
    let _ = fs::remove_file(&control_socket);

    let current_exe = std::env::current_exe()?;
    let args = vec![
        OsString::from(INTERNAL_SERVE_ARG),
        OsString::from("--control-socket"),
        control_socket.into_os_string(),
        OsString::from("--persisted-state-file"),
        persisted_state_file.into_os_string(),
    ];
    Command::new(current_exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .args(args)
        .spawn()?;

    let deadline = Instant::now() + BROKER_START_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(state) = load_runtime_state() {
            if matches!(
                send_request(&state.control_socket, &BrokerRequest::Ping),
                Ok(BrokerReply::Pong)
            ) {
                return Ok(());
            }
        }
        thread::sleep(BROKER_POLL_INTERVAL);
    }

    Err("timed out waiting for workspace broker".into())
}

pub(crate) fn broker_status() -> Result<WorkspaceBrokerStatus, Box<dyn Error>> {
    ensure_broker_running()?;
    let control_socket = load_runtime_state()?.control_socket;
    let reply = send_request(&control_socket, &BrokerRequest::Status)?;
    match reply {
        BrokerReply::Status { status } => Ok(status),
        BrokerReply::Error { error } => Err(error.into()),
        other => Err(format!("unexpected status reply: {other:?}").into()),
    }
}

pub(crate) fn broker_snapshot(id: u64) -> Result<WorkspaceBrokerTerminalSnapshot, Box<dyn Error>> {
    let control_socket = load_runtime_state()?.control_socket;
    let reply = send_request(&control_socket, &BrokerRequest::Snapshot { id })?;
    match reply {
        BrokerReply::Snapshot { terminal } => Ok(terminal),
        BrokerReply::Error { error } => Err(error.into()),
        other => Err(format!("unexpected snapshot reply: {other:?}").into()),
    }
}

pub(crate) fn restart_terminal(id: u64) -> Result<WorkspaceBrokerTerminalSnapshot, Box<dyn Error>> {
    ensure_broker_running()?;
    let control_socket = load_runtime_state()?.control_socket;
    let reply = send_request(&control_socket, &BrokerRequest::RestartTerminal { id })?;
    match reply {
        BrokerReply::Restarted { terminal } => Ok(terminal),
        BrokerReply::Error { error } => Err(error.into()),
        other => Err(format!("unexpected restart reply: {other:?}").into()),
    }
}

pub(crate) fn stop_workspace() -> Result<(), Box<dyn Error>> {
    if let Ok(state) = load_runtime_state() {
        let _ = send_request(&state.control_socket, &BrokerRequest::Stop);
        let _ = fs::remove_file(&state.control_socket);
        let _ = fs::remove_file(runtime_state_path());
    }
    let _ = fs::remove_file(workspace_layout_file());
    let _ = fs::remove_file(persisted_state_file());
    Ok(())
}

pub(crate) fn load_workspace_layout() -> Result<Option<WorkspaceLayout>, Box<dyn Error>> {
    let path = workspace_layout_file();
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn save_workspace_layout(layout: &WorkspaceLayout) -> Result<(), Box<dyn Error>> {
    write_json_atomic(&workspace_layout_file(), layout)
}

pub(crate) fn load_persisted_terminals()
-> Result<HashMap<u64, PersistedTerminalState>, Box<dyn Error>> {
    let state = load_persisted_state()?;
    Ok(state.terminals.into_iter().map(|terminal| (terminal.id, terminal)).collect())
}

pub(crate) fn save_persisted_terminals(
    terminals: impl IntoIterator<Item = PersistedTerminalState>,
) -> Result<(), Box<dyn Error>> {
    let mut state = load_persisted_state()?;
    let mut terminals = terminals.into_iter().collect::<Vec<_>>();
    terminals.sort_by_key(|terminal| terminal.id);
    state.terminals = terminals;
    write_json_atomic(&persisted_state_file(), &state)
}

pub(crate) fn allocate_terminal_id() -> Result<u64, Box<dyn Error>> {
    let mut state = load_persisted_state()?;
    let id = allocate_terminal_id_from_state(&mut state);
    write_json_atomic(&persisted_state_file(), &state)?;
    Ok(id)
}

pub(crate) fn send_terminal_input(id: u64, data: Vec<u8>) -> Result<(), Box<dyn Error>> {
    let control_socket = load_runtime_state()?.control_socket;
    let reply = send_request(&control_socket, &BrokerRequest::SendInput { id, data })?;
    match reply {
        BrokerReply::Closed => Ok(()),
        BrokerReply::Error { error } => Err(error.into()),
        other => Err(format!("unexpected send input reply: {other:?}").into()),
    }
}

pub(crate) fn resize_terminal(id: u64, window_size: WindowSize) -> Result<(), Box<dyn Error>> {
    let control_socket = load_runtime_state()?.control_socket;
    let reply = send_request(
        &control_socket,
        &BrokerRequest::Resize { id, window_size: window_size.into() },
    )?;
    match reply {
        BrokerReply::Closed => Ok(()),
        BrokerReply::Error { error } => Err(error.into()),
        other => Err(format!("unexpected resize reply: {other:?}").into()),
    }
}

pub(crate) fn close_terminal(id: u64) -> Result<(), Box<dyn Error>> {
    let control_socket = load_runtime_state()?.control_socket;
    let reply = send_request(&control_socket, &BrokerRequest::CloseTerminal { id })?;
    match reply {
        BrokerReply::Closed => Ok(()),
        BrokerReply::Error { error } => Err(error.into()),
        other => Err(format!("unexpected close reply: {other:?}").into()),
    }
}

pub(crate) fn status() -> Result<WorkspaceBrokerStatus, Box<dyn Error>> {
    if let Ok(state) = load_runtime_state() {
        if matches!(
            send_request(&state.control_socket, &BrokerRequest::Ping),
            Ok(BrokerReply::Pong)
        ) {
            return broker_status();
        }
    }

    let mut terminals = load_persisted_terminals()?
        .into_values()
        .map(|terminal| WorkspaceBrokerTerminalStatus {
            id: terminal.id,
            revision: terminal.revision,
            title: terminal.title,
            program_name: terminal.program_name,
            working_directory: terminal.working_directory,
            exit_code: terminal.exit_code,
        })
        .collect::<Vec<_>>();
    terminals.sort_by_key(|terminal| terminal.id);
    Ok(WorkspaceBrokerStatus { terminals })
}

fn serve(control_socket: PathBuf, persisted_state_path: PathBuf) -> Result<(), Box<dyn Error>> {
    tty::setup_env();

    ensure_parent_dir(&control_socket)?;
    ensure_parent_dir(&persisted_state_path)?;
    if control_socket.exists() {
        let _ = fs::remove_file(&control_socket);
    }

    let listener = UnixListener::bind(&control_socket)?;
    listener.set_nonblocking(true)?;

    let runtime_state = BrokerRuntimeState {
        control_socket: control_socket.clone(),
        persisted_state_file: persisted_state_path.clone(),
    };
    write_json_atomic(&runtime_state_path(), &runtime_state)?;

    let (event_tx, event_rx) = mpsc::channel();
    let mut persisted_state = load_persisted_state().unwrap_or_default();
    let mut terminals = BTreeMap::<u64, RuntimeTerminalState>::new();
    let mut should_exit = false;

    while !should_exit {
        should_exit |= process_terminal_events(&mut terminals, &event_rx, &mut persisted_state)?;

        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false)?;
                let request = read_request(&stream)?;
                let reply = handle_request(
                    request,
                    &event_tx,
                    &mut terminals,
                    &mut persisted_state,
                    &persisted_state_path,
                )?;
                should_exit |= matches!(reply, BrokerReply::Stopped);
                write_reply(&mut stream, &reply)?;
            },
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                thread::sleep(BROKER_IDLE_WAIT);
            },
            Err(err) => return Err(err.into()),
        }
    }

    for terminal in terminals.values_mut() {
        let _ = terminal.notifier.0.send(Msg::Shutdown);
    }
    let _ = fs::remove_file(control_socket);
    let _ = fs::remove_file(runtime_state_path());
    Ok(())
}

fn process_terminal_events(
    terminals: &mut BTreeMap<u64, RuntimeTerminalState>,
    rx: &Receiver<BrokerEventRecord>,
    persisted_state: &mut PersistedWorkspaceBrokerState,
) -> Result<bool, Box<dyn Error>> {
    let mut saw_stop = false;
    loop {
        match rx.try_recv() {
            Ok(record) => {
                let Some(terminal) = terminals.get_mut(&record.terminal_id) else {
                    continue;
                };
                match record.event {
                    TerminalEvent::ClipboardStore(_, _) | TerminalEvent::ClipboardLoad(_, _) => {},
                    TerminalEvent::ColorRequest(index, format) => {
                        let terminal_state = terminal.terminal.lock();
                        let color = terminal_state.colors()[index]
                            .unwrap_or(tabor_terminal::vte::ansi::Rgb { r: 0, g: 0, b: 0 });
                        terminal.notifier.notify(format(color).into_bytes());
                    },
                    TerminalEvent::PtyWrite(text) => {
                        terminal.notifier.notify(text.into_bytes());
                    },
                    TerminalEvent::TextAreaSizeRequest(format) => {
                        let terminal_state = terminal.terminal.lock();
                        let size = WindowSize {
                            num_cols: terminal_state.columns() as u16,
                            num_lines: terminal_state.screen_lines() as u16,
                            cell_width: 8,
                            cell_height: 16,
                        };
                        terminal.notifier.notify(format(size).into_bytes());
                    },
                    TerminalEvent::Title(title) => {
                        terminal.title = Some(title);
                    },
                    TerminalEvent::ResetTitle => {
                        terminal.title = None;
                    },
                    TerminalEvent::Exit => {
                        saw_stop = false;
                    },
                    TerminalEvent::ChildExit(code) => {
                        terminal.exit_code = Some(code);
                    },
                    TerminalEvent::Wakeup
                    | TerminalEvent::Bell
                    | TerminalEvent::MouseCursorDirty
                    | TerminalEvent::CursorBlinkingChange => {},
                }

                refresh_terminal_metadata(terminal);
                terminal.revision = terminal.revision.saturating_add(1);
                persist_terminal_state(terminal, persisted_state);
            },
            Err(TryRecvError::Empty) => return Ok(saw_stop),
            Err(TryRecvError::Disconnected) => return Ok(true),
        }
    }
}

fn handle_request(
    request: BrokerRequest,
    event_tx: &Sender<BrokerEventRecord>,
    terminals: &mut BTreeMap<u64, RuntimeTerminalState>,
    persisted_state: &mut PersistedWorkspaceBrokerState,
    persisted_state_path: &Path,
) -> Result<BrokerReply, Box<dyn Error>> {
    match request {
        BrokerRequest::Ping => Ok(BrokerReply::Pong),
        BrokerRequest::Status => Ok(BrokerReply::Status {
            status: WorkspaceBrokerStatus {
                terminals: terminals
                    .values_mut()
                    .map(|terminal| {
                        refresh_terminal_metadata(terminal);
                        runtime_status(terminal)
                    })
                    .collect(),
            },
        }),
        BrokerRequest::CreateTerminal { launch_options, window_size } => {
            let id = allocate_terminal_id_from_state(persisted_state);
            let terminal =
                spawn_terminal(id, launch_options, window_size.into(), event_tx.clone())?;
            let snapshot = runtime_snapshot(&terminal);
            persist_terminal_state(&terminal, persisted_state);
            terminals.insert(id, terminal);
            write_json_atomic(persisted_state_path, persisted_state)?;
            Ok(BrokerReply::Created { terminal: snapshot })
        },
        BrokerRequest::RestartTerminal { id } => {
            if terminals.contains_key(&id) {
                return Ok(BrokerReply::Error {
                    error: format!("terminal {id} is already running"),
                });
            }
            let Some(saved) = persisted_state.terminals.iter().find(|terminal| terminal.id == id)
            else {
                return Ok(BrokerReply::Error { error: format!("terminal {id} not found") });
            };
            let window_size = WindowSize {
                num_cols: saved.snapshot.grid.columns() as u16,
                num_lines: saved.snapshot.grid.screen_lines() as u16,
                cell_width: 8,
                cell_height: 16,
            };
            let terminal =
                spawn_terminal(id, saved.launch_options.clone(), window_size, event_tx.clone())?;
            let snapshot = runtime_snapshot(&terminal);
            persist_terminal_state(&terminal, persisted_state);
            terminals.insert(id, terminal);
            write_json_atomic(persisted_state_path, persisted_state)?;
            Ok(BrokerReply::Restarted { terminal: snapshot })
        },
        BrokerRequest::SendInput { id, data } => {
            let Some(terminal) = terminals.get(&id) else {
                return Ok(BrokerReply::Error { error: format!("terminal {id} not found") });
            };
            terminal.notifier.notify(Cow::Owned(data));
            Ok(BrokerReply::Closed)
        },
        BrokerRequest::Resize { id, window_size } => {
            let Some(terminal) = terminals.get_mut(&id) else {
                return Ok(BrokerReply::Error { error: format!("terminal {id} not found") });
            };
            terminal.notifier.on_resize(window_size.into());
            Ok(BrokerReply::Closed)
        },
        BrokerRequest::Snapshot { id } => {
            let Some(terminal) = terminals.get_mut(&id) else {
                return Ok(BrokerReply::Error { error: format!("terminal {id} not found") });
            };
            refresh_terminal_metadata(terminal);
            Ok(BrokerReply::Snapshot { terminal: runtime_snapshot(terminal) })
        },
        BrokerRequest::CloseTerminal { id } => {
            let Some(terminal) = terminals.remove(&id) else {
                return Ok(BrokerReply::Error { error: format!("terminal {id} not found") });
            };
            let _ = terminal.notifier.0.send(Msg::Shutdown);
            persisted_state.terminals.retain(|terminal| terminal.id != id);
            write_json_atomic(persisted_state_path, persisted_state)?;
            Ok(BrokerReply::Closed)
        },
        BrokerRequest::Stop => {
            for terminal in terminals.values_mut() {
                let _ = terminal.notifier.0.send(Msg::Shutdown);
            }
            terminals.clear();
            persisted_state.terminals.clear();
            write_json_atomic(persisted_state_path, persisted_state)?;
            Ok(BrokerReply::Stopped)
        },
    }
}

fn normalized_launch_options(mut launch_options: tty::Options) -> tty::Options {
    #[cfg(target_os = "macos")]
    if launch_options.working_directory.is_none() {
        launch_options.working_directory = Some(macos::preferred_working_dir());
    }

    launch_options
}

fn spawn_terminal(
    id: u64,
    launch_options: tty::Options,
    window_size: WindowSize,
    event_tx: Sender<BrokerEventRecord>,
) -> Result<RuntimeTerminalState, Box<dyn Error>> {
    let launch_options = normalized_launch_options(launch_options);
    let listener = BrokerEventProxy { terminal_id: id, sender: event_tx };
    let term_size = TermSize::new(window_size.num_cols as usize, window_size.num_lines as usize);
    let terminal =
        Arc::new(FairMutex::new(Term::new(Default::default(), &term_size, listener.clone())));

    let pty = tty::new(&launch_options, window_size, 0)?;
    #[cfg(unix)]
    let master_fd = std::os::fd::AsRawFd::as_raw_fd(pty.file());
    #[cfg(unix)]
    let shell_pid = pty.child().id();

    let event_loop = PtyEventLoop::new(
        Arc::clone(&terminal),
        listener,
        pty,
        launch_options.drain_on_exit,
        false,
    )?;
    let notifier = Notifier(event_loop.channel());
    let _io_thread = event_loop.spawn();

    let mut state = RuntimeTerminalState {
        id,
        launch_options,
        terminal,
        notifier,
        #[cfg(unix)]
        master_fd,
        #[cfg(unix)]
        shell_pid,
        revision: 1,
        title: None,
        program_name: String::new(),
        working_directory: None,
        exit_code: None,
    };
    refresh_terminal_metadata(&mut state);
    Ok(state)
}

fn runtime_snapshot(terminal: &RuntimeTerminalState) -> WorkspaceBrokerTerminalSnapshot {
    WorkspaceBrokerTerminalSnapshot {
        status: runtime_status(terminal),
        snapshot: terminal.terminal.lock().export_snapshot(),
    }
}

fn runtime_status(terminal: &RuntimeTerminalState) -> WorkspaceBrokerTerminalStatus {
    WorkspaceBrokerTerminalStatus {
        id: terminal.id,
        revision: terminal.revision,
        title: terminal.title.clone(),
        program_name: terminal.program_name.clone(),
        working_directory: terminal.working_directory.clone(),
        exit_code: terminal.exit_code,
    }
}

fn refresh_terminal_metadata(terminal: &mut RuntimeTerminalState) {
    #[cfg(unix)]
    {
        terminal.program_name =
            crate::daemon::foreground_process_name(terminal.master_fd, terminal.shell_pid)
                .unwrap_or_else(|_| String::from("shell"));
        terminal.working_directory =
            crate::daemon::foreground_process_path(terminal.master_fd, terminal.shell_pid).ok();
    }
}

fn persist_terminal_state(
    terminal: &RuntimeTerminalState,
    persisted_state: &mut PersistedWorkspaceBrokerState,
) {
    let persisted = PersistedTerminalState {
        id: terminal.id,
        launch_options: terminal.launch_options.clone(),
        snapshot: terminal.terminal.lock().export_snapshot(),
        revision: terminal.revision,
        title: terminal.title.clone(),
        program_name: terminal.program_name.clone(),
        working_directory: terminal.working_directory.clone(),
        exit_code: terminal.exit_code,
    };

    if let Some(existing) =
        persisted_state.terminals.iter_mut().find(|saved| saved.id == terminal.id)
    {
        *existing = persisted;
    } else {
        persisted_state.terminals.push(persisted);
    }
}

fn allocate_terminal_id_from_state(persisted_state: &mut PersistedWorkspaceBrokerState) -> u64 {
    let next = persisted_state.next_terminal_id.max(1);
    persisted_state.next_terminal_id = next.saturating_add(1);
    next
}

fn send_request(
    control_socket: &Path,
    request: &BrokerRequest,
) -> Result<BrokerReply, Box<dyn Error>> {
    let mut stream = UnixStream::connect(control_socket)?;
    let json = serde_json::to_string(request)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err("empty broker reply".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn read_request(stream: &UnixStream) -> Result<BrokerRequest, Box<dyn Error>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err("empty broker request".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn write_reply(stream: &mut UnixStream, reply: &BrokerReply) -> Result<(), Box<dyn Error>> {
    let json = serde_json::to_string(reply)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn load_runtime_state() -> Result<BrokerRuntimeState, Box<dyn Error>> {
    let bytes = fs::read(runtime_state_path())?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn load_persisted_state() -> Result<PersistedWorkspaceBrokerState, Box<dyn Error>> {
    let path = persisted_state_file();
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            Ok(PersistedWorkspaceBrokerState::default())
        },
        Err(err) => Err(err.into()),
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), Box<dyn Error>> {
    ensure_parent_dir(path)?;
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp_path, path)?;
    Ok(())
}

fn ensure_parent_dir(path: &Path) -> Result<(), Box<dyn Error>> {
    let parent =
        path.parent().ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    Ok(())
}

fn runtime_state_path() -> PathBuf {
    runtime_dir().join("tabor-workspace-broker.json")
}

fn control_socket_path() -> PathBuf {
    runtime_dir().join("tabor-workspace-broker.sock")
}

fn workspace_layout_file() -> PathBuf {
    persisted_dir().join("workspace-layout.json")
}

fn persisted_state_file() -> PathBuf {
    persisted_dir().join("workspace-broker-state.json")
}

#[cfg(target_os = "macos")]
fn test_bundle_workspace_root() -> Option<PathBuf> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let bundle_id = macos::bundle_identifier();
    if !bundle_id.starts_with("com.pinkbot.tabor.test.") {
        return None;
    }

    let mut hasher = DefaultHasher::new();
    bundle_id.hash(&mut hasher);
    let path = PathBuf::from("/tmp").join(format!("ttw-{:016x}", hasher.finish()));
    let _ = fs::create_dir_all(&path);
    Some(path)
}

fn runtime_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(path) = test_bundle_workspace_root() {
            let path = path.join("r");
            let _ = fs::create_dir_all(&path);
            return path;
        }
        macos::runtime_tmp_dir()
    }

    #[cfg(not(target_os = "macos"))]
    {
        let path = std::env::temp_dir().join("tabor");
        let _ = fs::create_dir_all(&path);
        path
    }
}

fn persisted_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(path) = test_bundle_workspace_root() {
            let path = path.join("p");
            let _ = fs::create_dir_all(&path);
            return path;
        }
        let base = if macos::distribution_channel().is_mac_app_store() {
            macos::container_data_dir().join("Library").join("Application Support").join("Tabor")
        } else {
            macos::direct_app_support_dir()
        };
        let path = base.join("workspace");
        let _ = fs::create_dir_all(&path);
        path
    }

    #[cfg(not(target_os = "macos"))]
    {
        let path = std::env::temp_dir().join("tabor-workspace");
        let _ = fs::create_dir_all(&path);
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    use crate::macos::test_support::{EnvVarGuard, env_lock};

    use tempfile::tempdir;

    #[test]
    fn create_terminal_reply_survives_large_snapshot_roundtrip() {
        let tempdir = tempdir().expect("tempdir");
        let persisted_state = tempdir.path().join("broker-state.json");
        let (client_stream, mut server_stream) = UnixStream::pair().expect("stream pair");

        let server_state = persisted_state.clone();
        let server = thread::spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            let mut persisted_state = PersistedWorkspaceBrokerState::default();
            let mut terminals = BTreeMap::new();
            let request = read_request(&server_stream).expect("read request");
            let reply = handle_request(
                request,
                &event_tx,
                &mut terminals,
                &mut persisted_state,
                &server_state,
            )
            .expect("handle request");
            write_reply(&mut server_stream, &reply).expect("write reply");
            for terminal in terminals.values_mut() {
                let _ = terminal.notifier.0.send(Msg::Shutdown);
            }
        });

        let request = BrokerRequest::CreateTerminal {
            launch_options: tty::Options {
                shell: Some(tty::Shell::new(
                    String::from("/bin/sh"),
                    vec![String::from("-lc"), String::from("sleep 60")],
                )),
                ..tty::Options::default()
            },
            window_size: WindowSize {
                num_lines: 49,
                num_cols: 109,
                cell_width: 10,
                cell_height: 24,
            }
            .into(),
        };
        let mut client_stream = client_stream;
        client_stream
            .write_all(serde_json::to_string(&request).expect("serialize request").as_bytes())
            .expect("write request");
        client_stream.write_all(b"\n").expect("write request terminator");
        client_stream.flush().expect("flush request");

        let mut line = String::new();
        BufReader::new(client_stream).read_line(&mut line).expect("read reply");
        let reply: BrokerReply = serde_json::from_str(&line).expect("parse reply");
        match reply {
            BrokerReply::Created { terminal } => {
                assert!(terminal.snapshot.grid.columns() > 0);
                assert!(terminal.snapshot.grid.screen_lines() > 0);
            },
            other => panic!("unexpected reply: {other:?}"),
        }

        server.join().expect("server thread");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn normalized_launch_options_uses_preferred_working_dir_when_missing() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let tempdir = tempdir().expect("tempdir");
        let home_dir = tempdir.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create home dir");

        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());

        let launch_options = normalized_launch_options(tty::Options::default());
        assert_eq!(launch_options.working_directory, Some(home_dir));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn normalized_launch_options_preserves_explicit_working_dir() {
        let explicit_dir = PathBuf::from("/tmp/tabor-explicit-cwd");
        let launch_options = normalized_launch_options(tty::Options {
            working_directory: Some(explicit_dir.clone()),
            ..tty::Options::default()
        });

        assert_eq!(launch_options.working_directory, Some(explicit_dir));
    }
}
