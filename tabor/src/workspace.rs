use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{BufReader, Error as IoError, ErrorKind, Read};
#[cfg(not(windows))]
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tabor_terminal::event::{VoidListener, WindowSize};
use tabor_terminal::grid::Dimensions;
use tabor_terminal::sync::FairMutex;
use tabor_terminal::term::test::TermSize;
use tabor_terminal::term::{ResizeAnchor, Term, TermSnapshot};
use tabor_terminal::tty;
use tabor_terminal::vte::ansi;

use crate::cli::{WorkspaceCommand, WorkspaceOptions};
use crate::event::EventProxy;
#[cfg(target_os = "macos")]
use crate::macos;

pub(crate) const WORKSPACE_PROTOCOL_VERSION: u32 = 1;

const PERSISTENCE_POLL_INTERVAL: Duration = Duration::from_millis(200);
const PREVIEW_IDLE_DEBOUNCE: Duration = Duration::from_secs(2);
const PREVIEW_MAX_FLUSH_INTERVAL: Duration = Duration::from_secs(10);
const MAX_PERSISTED_PREVIEW_LINES: usize = 50;
const TERMINAL_PREVIEW_VERSION: u32 = 1;
const JOURNAL_RECORD_OUTPUT: u8 = 1;
const JOURNAL_RECORD_RESIZE: u8 = 2;
const STATE_FILE_NAME: &str = "workspace-state.json";
const LEGACY_STATE_FILE_NAME: &str = "workspace-broker-state.json";
const PREVIEW_FILE_NAME: &str = "preview.json";

static PERSISTENCE_WORKER: LazyLock<Mutex<Option<PersistenceWorker>>> =
    LazyLock::new(|| Mutex::new(None));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedTerminalState {
    pub id: u64,
    pub launch_options: tty::Options,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub program_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub clean_exit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<TermSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceTerminalStatus {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub program_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub clean_exit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceStatus {
    pub terminals: Vec<WorkspaceTerminalStatus>,
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
        #[serde(alias = "broker_id")]
        terminal_id: u64,
        #[serde(default)]
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
    Pdf {
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

#[derive(Clone)]
pub(crate) struct TerminalOutputObserver {
    output_dirty: Arc<AtomicBool>,
}

impl TerminalOutputObserver {
    pub(crate) fn observe(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        self.output_dirty.store(true, Ordering::Release);
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct PersistedWorkspaceState {
    next_terminal_id: u64,
    terminals: Vec<PersistedTerminalState>,
}

struct PersistenceWorker {
    sender: Sender<PersistenceCommand>,
    join: JoinHandle<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TerminalPreview {
    version: u32,
    #[serde(default)]
    lines: Vec<String>,
}

pub(crate) struct LiveTerminalRegistration {
    pub terminal_id: u64,
    pub terminal: Arc<FairMutex<Term<EventProxy>>>,
    pub launch_options: tty::Options,
    pub title: Option<String>,
    pub program_name: String,
    pub working_directory: Option<PathBuf>,
    #[cfg(not(windows))]
    pub master_fd: RawFd,
    #[cfg(not(windows))]
    pub shell_pid: u32,
}

struct LiveTerminalState {
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    metadata: PersistedTerminalState,
    #[cfg(not(windows))]
    master_fd: RawFd,
    #[cfg(not(windows))]
    shell_pid: u32,
    output_dirty: Arc<AtomicBool>,
    dirty: bool,
    last_activity: Instant,
    last_preview_flush: Instant,
}

enum PersistenceCommand {
    Register {
        terminal_id: u64,
        terminal: Arc<FairMutex<Term<EventProxy>>>,
        launch_options: tty::Options,
        title: Option<String>,
        program_name: String,
        working_directory: Option<PathBuf>,
        #[cfg(not(windows))]
        master_fd: RawFd,
        #[cfg(not(windows))]
        shell_pid: u32,
        output_dirty: Arc<AtomicBool>,
    },
    Resize {
        terminal_id: u64,
    },
    Metadata {
        terminal_id: u64,
        title: Option<String>,
        program_name: Option<String>,
        working_directory: Option<PathBuf>,
        exit_code: Option<i32>,
        clean_exit: Option<bool>,
    },
    Checkpoint {
        terminal_id: u64,
        force: bool,
    },
    Remove {
        terminal_id: u64,
    },
    Shutdown {
        clear_persisted: bool,
    },
}

#[derive(Debug)]
enum JournalRecord {
    Output { sequence: u64, bytes: Vec<u8> },
    Resize { sequence: u64, columns: u32, lines: u32 },
}

pub(crate) fn run(options: WorkspaceOptions) -> Result<(), Box<dyn Error>> {
    match options.command {
        WorkspaceCommand::Status => {
            println!("{}", serde_json::to_string(&status()?)?);
        },
        WorkspaceCommand::Stop => {
            stop_workspace()?;
        },
    }
    Ok(())
}

pub(crate) fn register_live_terminal(
    registration: LiveTerminalRegistration,
) -> Result<TerminalOutputObserver, Box<dyn Error>> {
    let LiveTerminalRegistration {
        terminal_id,
        terminal,
        launch_options,
        title,
        program_name,
        working_directory,
        #[cfg(not(windows))]
        master_fd,
        #[cfg(not(windows))]
        shell_pid,
    } = registration;

    let sender = ensure_persistence_worker()?;
    let output_dirty = Arc::new(AtomicBool::new(false));
    sender.send(PersistenceCommand::Register {
        terminal_id,
        terminal,
        launch_options,
        title,
        program_name,
        working_directory,
        #[cfg(not(windows))]
        master_fd,
        #[cfg(not(windows))]
        shell_pid,
        output_dirty: Arc::clone(&output_dirty),
    })?;
    Ok(TerminalOutputObserver { output_dirty })
}

pub(crate) fn record_terminal_resize(terminal_id: u64, _window_size: WindowSize) {
    let Some(sender) = persistence_sender() else {
        return;
    };

    let _ = sender.send(PersistenceCommand::Resize { terminal_id });
}

pub(crate) fn update_terminal_metadata(
    terminal_id: u64,
    title: Option<String>,
    program_name: Option<String>,
    working_directory: Option<PathBuf>,
    exit_code: Option<i32>,
    clean_exit: Option<bool>,
) {
    if let Some(sender) = persistence_sender() {
        let _ = sender.send(PersistenceCommand::Metadata {
            terminal_id,
            title,
            program_name,
            working_directory,
            exit_code,
            clean_exit,
        });
        return;
    }

    let _ = update_terminal_metadata_sync(
        terminal_id,
        title,
        program_name,
        working_directory,
        exit_code,
        clean_exit,
    );
}

pub(crate) fn checkpoint_terminal(terminal_id: u64, force: bool) {
    let Some(sender) = persistence_sender() else {
        return;
    };
    let _ = sender.send(PersistenceCommand::Checkpoint { terminal_id, force });
}

pub(crate) fn remove_terminal(terminal_id: u64) -> Result<(), Box<dyn Error>> {
    if let Some(sender) = persistence_sender() {
        sender.send(PersistenceCommand::Remove { terminal_id })?;
        return Ok(());
    }

    remove_terminal_sync(terminal_id)
}

pub(crate) fn shutdown_persistence() -> Result<(), Box<dyn Error>> {
    shutdown_persistence_worker(false)
}

pub(crate) fn stop_workspace() -> Result<(), Box<dyn Error>> {
    shutdown_persistence_worker(true)?;
    let _ = fs::remove_file(workspace_layout_file());
    let _ = fs::remove_file(current_state_file());
    let _ = fs::remove_file(legacy_state_file());
    let _ = fs::remove_dir_all(terminals_dir());
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

pub(crate) fn load_persisted_terminals_for_layout(
    layout: &WorkspaceLayout,
) -> Result<HashMap<u64, PersistedTerminalState>, Box<dyn Error>> {
    let terminal_ids = workspace_layout_terminal_ids(layout);
    let mut state = load_persisted_state()?;
    let stale_terminal_ids = state
        .terminals
        .iter()
        .map(|terminal| terminal.id)
        .filter(|terminal_id| !terminal_ids.contains(terminal_id))
        .collect::<Vec<_>>();

    if !stale_terminal_ids.is_empty() {
        state.terminals.retain(|terminal| terminal_ids.contains(&terminal.id));
        write_persisted_state(&state)?;
        for terminal_id in stale_terminal_ids {
            remove_terminal_files(terminal_id)?;
        }
    }
    remove_orphaned_terminal_directories(&terminal_ids)?;

    Ok(state.terminals.into_iter().map(|terminal| (terminal.id, terminal)).collect())
}

pub(crate) fn load_terminal_preview_lines(
    terminal_id: u64,
    state: &PersistedTerminalState,
) -> Result<Option<Vec<String>>, Box<dyn Error>> {
    if let Some(lines) = load_preview_lines_file(terminal_id)? {
        return Ok(Some(lines));
    }

    if let Some(snapshot) = load_legacy_terminal_snapshot(terminal_id, state)? {
        let lines = legacy_snapshot_to_preview_lines(snapshot);
        write_terminal_preview_lines(terminal_id, &lines)?;
        return Ok(Some(lines));
    }

    Ok(None)
}

fn load_legacy_terminal_snapshot(
    terminal_id: u64,
    state: &PersistedTerminalState,
) -> Result<Option<TermSnapshot>, Box<dyn Error>> {
    if let Ok(bytes) = fs::read(checkpoint_file(terminal_id)) {
        let mut snapshot: TermSnapshot = serde_json::from_slice(&bytes)?;
        replay_journal_into_snapshot(&mut snapshot, terminal_id)?;
        return Ok(Some(snapshot));
    }

    if let Some(mut snapshot) = state.snapshot.clone() {
        replay_journal_into_snapshot(&mut snapshot, terminal_id)?;
        return Ok(Some(snapshot));
    }

    Ok(None)
}

fn load_preview_lines_file(terminal_id: u64) -> Result<Option<Vec<String>>, Box<dyn Error>> {
    match fs::read(preview_file(terminal_id)) {
        Ok(bytes) => {
            let mut preview: TerminalPreview = serde_json::from_slice(&bytes)?;
            if preview.lines.len() > MAX_PERSISTED_PREVIEW_LINES {
                preview.lines.drain(..preview.lines.len() - MAX_PERSISTED_PREVIEW_LINES);
            }
            Ok((!preview.lines.is_empty()).then_some(preview.lines))
        },
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn legacy_snapshot_to_preview_lines(snapshot: TermSnapshot) -> Vec<String> {
    let mut terminal = Term::new(
        Default::default(),
        &TermSize::new(snapshot.grid.columns(), snapshot.grid.screen_lines()),
        VoidListener,
    );
    terminal.apply_snapshot(snapshot);
    terminal.export_preview_lines(MAX_PERSISTED_PREVIEW_LINES)
}

pub(crate) fn allocate_terminal_id() -> Result<u64, Box<dyn Error>> {
    let mut state = load_persisted_state()?;
    let reserved_terminal_ids = load_workspace_layout()
        .ok()
        .flatten()
        .map(|layout| workspace_layout_terminal_ids(&layout))
        .unwrap_or_default();
    let id = allocate_terminal_id_from_state(&mut state, &reserved_terminal_ids);
    write_persisted_state(&state)?;
    Ok(id)
}

pub(crate) fn status() -> Result<WorkspaceStatus, Box<dyn Error>> {
    let mut terminals = load_persisted_terminals()?
        .into_values()
        .map(|terminal| WorkspaceTerminalStatus {
            id: terminal.id,
            title: terminal.title,
            program_name: terminal.program_name,
            working_directory: terminal.working_directory,
            exit_code: terminal.exit_code,
            clean_exit: terminal.clean_exit,
        })
        .collect::<Vec<_>>();
    terminals.sort_by_key(|terminal| terminal.id);
    Ok(WorkspaceStatus { terminals })
}

fn ensure_persistence_worker() -> Result<Sender<PersistenceCommand>, Box<dyn Error>> {
    let mut worker = PERSISTENCE_WORKER.lock().expect("persistence worker lock poisoned");
    if let Some(worker) = worker.as_ref() {
        return Ok(worker.sender.clone());
    }

    let (sender, receiver) = mpsc::channel();
    let join = thread::spawn(move || persistence_loop(receiver));
    *worker = Some(PersistenceWorker { sender: sender.clone(), join });
    Ok(sender)
}

fn persistence_sender() -> Option<Sender<PersistenceCommand>> {
    let worker = PERSISTENCE_WORKER.lock().ok()?;
    let worker = worker.as_ref()?;
    Some(worker.sender.clone())
}

fn shutdown_persistence_worker(clear_persisted: bool) -> Result<(), Box<dyn Error>> {
    let mut worker = PERSISTENCE_WORKER.lock().expect("persistence worker lock poisoned");
    let Some(worker) = worker.take() else {
        if clear_persisted {
            let _ = fs::remove_dir_all(terminals_dir());
        }
        return Ok(());
    };

    worker.sender.send(PersistenceCommand::Shutdown { clear_persisted })?;
    worker.join.join().map_err(|_| "persistence worker panicked")?;
    Ok(())
}

fn persistence_loop(receiver: Receiver<PersistenceCommand>) {
    let mut state = load_persisted_state().unwrap_or_default();
    let mut live = HashMap::<u64, LiveTerminalState>::new();
    let mut state_dirty = false;
    let mut clear_persisted = false;

    loop {
        match receiver.recv_timeout(PERSISTENCE_POLL_INTERVAL) {
            Ok(command) => match command {
                PersistenceCommand::Register {
                    terminal_id,
                    terminal,
                    launch_options,
                    title,
                    program_name,
                    working_directory,
                    #[cfg(not(windows))]
                    master_fd,
                    #[cfg(not(windows))]
                    shell_pid,
                    output_dirty,
                } => {
                    let metadata = PersistedTerminalState {
                        id: terminal_id,
                        launch_options,
                        title,
                        program_name,
                        working_directory,
                        exit_code: None,
                        clean_exit: false,
                        snapshot: None,
                    };
                    live.insert(
                        terminal_id,
                        LiveTerminalState {
                            terminal,
                            metadata: metadata.clone(),
                            #[cfg(not(windows))]
                            master_fd,
                            #[cfg(not(windows))]
                            shell_pid,
                            output_dirty,
                            dirty: false,
                            last_activity: Instant::now(),
                            last_preview_flush: Instant::now(),
                        },
                    );
                    upsert_terminal_state(&mut state, metadata);
                    state_dirty = true;
                },
                PersistenceCommand::Resize { terminal_id, .. } => {
                    let Some(live_terminal) = live.get_mut(&terminal_id) else {
                        continue;
                    };
                    live_terminal.dirty = true;
                    live_terminal.last_activity = Instant::now();
                },
                PersistenceCommand::Metadata {
                    terminal_id,
                    title,
                    program_name,
                    working_directory,
                    exit_code,
                    clean_exit,
                } => {
                    if let Some(live_terminal) = live.get_mut(&terminal_id) {
                        apply_metadata_update(
                            &mut live_terminal.metadata,
                            title,
                            program_name,
                            working_directory,
                            exit_code,
                            clean_exit,
                        );
                        upsert_terminal_state(&mut state, live_terminal.metadata.clone());
                        state_dirty = true;
                    } else if apply_metadata_update_to_state(
                        &mut state,
                        terminal_id,
                        title,
                        program_name,
                        working_directory,
                        exit_code,
                        clean_exit,
                    ) {
                        state_dirty = true;
                    }
                },
                PersistenceCommand::Checkpoint { terminal_id, force } => {
                    if let Some(live_terminal) = live.get_mut(&terminal_id) {
                        if matches!(flush_terminal_preview(live_terminal, force), Ok(true)) {
                            upsert_terminal_state(&mut state, live_terminal.metadata.clone());
                            state_dirty = true;
                        }
                    }
                },
                PersistenceCommand::Remove { terminal_id } => {
                    live.remove(&terminal_id);
                    state.terminals.retain(|terminal| terminal.id != terminal_id);
                    let _ = remove_terminal_files(terminal_id);
                    state_dirty = true;
                },
                PersistenceCommand::Shutdown { clear_persisted: clear } => {
                    clear_persisted = clear;
                    for terminal in live.values_mut() {
                        let _ = flush_terminal_preview(terminal, true);
                        upsert_terminal_state(&mut state, terminal.metadata.clone());
                    }
                    break;
                },
            },
            Err(RecvTimeoutError::Timeout) => {},
            Err(RecvTimeoutError::Disconnected) => break,
        }

        for terminal in live.values_mut() {
            if terminal.output_dirty.swap(false, Ordering::Acquire) {
                terminal.dirty = true;
                terminal.last_activity = Instant::now();
            }
            if !terminal.dirty {
                continue;
            }
            if terminal.last_activity.elapsed() < PREVIEW_IDLE_DEBOUNCE
                && terminal.last_preview_flush.elapsed() < PREVIEW_MAX_FLUSH_INTERVAL
            {
                continue;
            }
            if matches!(flush_terminal_preview(terminal, false), Ok(true)) {
                upsert_terminal_state(&mut state, terminal.metadata.clone());
                state_dirty = true;
            }
        }

        if state_dirty {
            let _ = write_persisted_state(&state);
            state_dirty = false;
        }
    }

    if clear_persisted {
        let _ = fs::remove_file(current_state_file());
        let _ = fs::remove_file(legacy_state_file());
        let _ = fs::remove_dir_all(terminals_dir());
    } else {
        let _ = write_persisted_state(&state);
    }
}

fn flush_terminal_preview(
    terminal: &mut LiveTerminalState,
    force: bool,
) -> Result<bool, Box<dyn Error>> {
    if !terminal.dirty && !force {
        return Ok(false);
    }

    let previous_program_name = terminal.metadata.program_name.clone();
    let previous_working_directory = terminal.metadata.working_directory.clone();
    #[cfg(not(windows))]
    refresh_live_terminal_metadata(terminal);

    let preview_lines = if force {
        terminal.terminal.lock().export_preview_lines(MAX_PERSISTED_PREVIEW_LINES)
    } else {
        let Some(terminal_guard) = terminal.terminal.try_lock_unfair() else {
            return Err("terminal busy".into());
        };
        terminal_guard.export_preview_lines(MAX_PERSISTED_PREVIEW_LINES)
    };

    write_terminal_preview_lines(terminal.metadata.id, &preview_lines)?;
    terminal.dirty = false;
    terminal.last_preview_flush = Instant::now();
    terminal.metadata.snapshot = None;
    Ok(terminal.metadata.program_name != previous_program_name
        || terminal.metadata.working_directory != previous_working_directory)
}

#[cfg(not(windows))]
fn refresh_live_terminal_metadata(terminal: &mut LiveTerminalState) {
    terminal.metadata.program_name =
        crate::daemon::foreground_process_name(terminal.master_fd, terminal.shell_pid)
            .unwrap_or_else(|_| terminal.metadata.program_name.clone());
    terminal.metadata.working_directory =
        crate::daemon::foreground_process_path(terminal.master_fd, terminal.shell_pid)
            .ok()
            .or_else(|| terminal.metadata.working_directory.clone());
}

fn apply_metadata_update(
    terminal: &mut PersistedTerminalState,
    title: Option<String>,
    program_name: Option<String>,
    working_directory: Option<PathBuf>,
    exit_code: Option<i32>,
    clean_exit: Option<bool>,
) {
    if let Some(title) = title {
        terminal.title = Some(title);
    }
    if let Some(program_name) = program_name {
        terminal.program_name = program_name;
    }
    if let Some(working_directory) = working_directory {
        terminal.working_directory = Some(working_directory);
    }
    if let Some(exit_code) = exit_code {
        terminal.exit_code = Some(exit_code);
    }
    if let Some(clean_exit) = clean_exit {
        terminal.clean_exit = clean_exit;
    }
}

fn apply_metadata_update_to_state(
    state: &mut PersistedWorkspaceState,
    terminal_id: u64,
    title: Option<String>,
    program_name: Option<String>,
    working_directory: Option<PathBuf>,
    exit_code: Option<i32>,
    clean_exit: Option<bool>,
) -> bool {
    let Some(terminal) = state.terminals.iter_mut().find(|terminal| terminal.id == terminal_id)
    else {
        return false;
    };

    apply_metadata_update(terminal, title, program_name, working_directory, exit_code, clean_exit);
    true
}

fn upsert_terminal_state(state: &mut PersistedWorkspaceState, terminal: PersistedTerminalState) {
    let terminal_id = terminal.id;
    if let Some(existing) = state.terminals.iter_mut().find(|saved| saved.id == terminal_id) {
        *existing = terminal;
    } else {
        state.terminals.push(terminal);
    }
    state.terminals.sort_by_key(|terminal| terminal.id);
    state.next_terminal_id = state.next_terminal_id.max(terminal_id.saturating_add(1));
}

fn update_terminal_metadata_sync(
    terminal_id: u64,
    title: Option<String>,
    program_name: Option<String>,
    working_directory: Option<PathBuf>,
    exit_code: Option<i32>,
    clean_exit: Option<bool>,
) -> Result<(), Box<dyn Error>> {
    let mut state = load_persisted_state()?;
    if apply_metadata_update_to_state(
        &mut state,
        terminal_id,
        title,
        program_name,
        working_directory,
        exit_code,
        clean_exit,
    ) {
        write_persisted_state(&state)?;
    }
    Ok(())
}

fn remove_terminal_sync(terminal_id: u64) -> Result<(), Box<dyn Error>> {
    let mut state = load_persisted_state()?;
    let original_len = state.terminals.len();
    state.terminals.retain(|terminal| terminal.id != terminal_id);
    if state.terminals.len() != original_len {
        write_persisted_state(&state)?;
    }
    remove_terminal_files(terminal_id)
}

fn replay_journal_into_snapshot(
    snapshot: &mut TermSnapshot,
    terminal_id: u64,
) -> Result<(), Box<dyn Error>> {
    let mut terminal = Term::new(
        Default::default(),
        &TermSize::new(snapshot.grid.columns(), snapshot.grid.screen_lines()),
        VoidListener,
    );
    terminal.apply_snapshot(snapshot.clone());
    let mut parser: ansi::Processor = Default::default();
    let mut journal = load_journal_records(terminal_id)?;
    journal.sort_by_key(|record| match record {
        JournalRecord::Output { sequence, .. } | JournalRecord::Resize { sequence, .. } => {
            *sequence
        },
    });
    for record in journal {
        match record {
            JournalRecord::Output { bytes, .. } => parser.advance(&mut terminal, &bytes),
            JournalRecord::Resize { columns, lines, .. } => {
                terminal.resize_with_anchor(
                    TermSize::new(columns as usize, lines as usize),
                    ResizeAnchor::Top,
                );
            },
        }
    }
    *snapshot = terminal.export_snapshot();
    Ok(())
}

fn load_journal_records(terminal_id: u64) -> Result<Vec<JournalRecord>, Box<dyn Error>> {
    let path = journal_file(terminal_id);
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    loop {
        let mut tag = [0u8; 1];
        match reader.read_exact(&mut tag) {
            Ok(()) => {},
            Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(err.into()),
        }

        let sequence = read_u64(&mut reader)?;
        match tag[0] {
            JOURNAL_RECORD_OUTPUT => {
                let len = read_u32(&mut reader)? as usize;
                let mut bytes = vec![0; len];
                if let Err(err) = reader.read_exact(&mut bytes) {
                    if err.kind() == ErrorKind::UnexpectedEof {
                        break;
                    }
                    return Err(err.into());
                }
                records.push(JournalRecord::Output { sequence, bytes });
            },
            JOURNAL_RECORD_RESIZE => {
                let columns = read_u32(&mut reader)?;
                let lines = read_u32(&mut reader)?;
                records.push(JournalRecord::Resize { sequence, columns, lines });
            },
            _ => break,
        }
    }

    Ok(records)
}

fn write_terminal_preview_lines(
    terminal_id: u64,
    preview_lines: &[String],
) -> Result<(), Box<dyn Error>> {
    let path = preview_file(terminal_id);
    if preview_lines.is_empty() {
        let _ = fs::remove_file(&path);
    } else {
        let preview =
            TerminalPreview { version: TERMINAL_PREVIEW_VERSION, lines: preview_lines.to_vec() };
        write_json_atomic_compact(&path, &preview)?;
    }
    let _ = fs::remove_file(checkpoint_file(terminal_id));
    let _ = fs::remove_file(journal_file(terminal_id));
    Ok(())
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64, Box<dyn Error>> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32, Box<dyn Error>> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn load_persisted_state() -> Result<PersistedWorkspaceState, Box<dyn Error>> {
    for path in [current_state_file(), legacy_state_file()] {
        match fs::read(&path) {
            Ok(bytes) => return Ok(serde_json::from_slice(&bytes)?),
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(PersistedWorkspaceState::default())
}

fn write_persisted_state(state: &PersistedWorkspaceState) -> Result<(), Box<dyn Error>> {
    let mut serializable = state.clone();
    for terminal in &mut serializable.terminals {
        terminal.snapshot = None;
    }
    write_json_atomic(&current_state_file(), &serializable)?;
    let legacy = legacy_state_file();
    if legacy.exists() {
        let _ = fs::remove_file(legacy);
    }
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), Box<dyn Error>> {
    write_bytes_atomic_if_changed(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn write_json_atomic_compact<T: Serialize>(path: &Path, value: &T) -> Result<(), Box<dyn Error>> {
    write_bytes_atomic_if_changed(path, serde_json::to_vec(value)?)?;
    Ok(())
}

fn write_bytes_atomic_if_changed(path: &Path, bytes: Vec<u8>) -> Result<bool, Box<dyn Error>> {
    match fs::read(path) {
        Ok(existing) if existing == bytes => return Ok(false),
        Ok(_) => {},
        Err(err) if err.kind() == ErrorKind::NotFound => {},
        Err(err) => return Err(err.into()),
    }

    ensure_parent_dir(path)?;
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, bytes)?;
    fs::rename(tmp_path, path)?;
    Ok(true)
}

fn ensure_parent_dir(path: &Path) -> Result<(), Box<dyn Error>> {
    let parent =
        path.parent().ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    Ok(())
}

fn checkpoint_file(terminal_id: u64) -> PathBuf {
    terminal_dir(terminal_id).join("checkpoint.json")
}

fn journal_file(terminal_id: u64) -> PathBuf {
    terminal_dir(terminal_id).join("journal.bin")
}

fn preview_file(terminal_id: u64) -> PathBuf {
    terminal_dir(terminal_id).join(PREVIEW_FILE_NAME)
}

fn terminal_dir(terminal_id: u64) -> PathBuf {
    terminals_dir().join(terminal_id.to_string())
}

fn terminals_dir() -> PathBuf {
    persisted_dir().join("terminals")
}

fn remove_terminal_files(terminal_id: u64) -> Result<(), Box<dyn Error>> {
    let dir = terminal_dir(terminal_id);
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

fn remove_orphaned_terminal_directories(terminal_ids: &HashSet<u64>) -> Result<(), Box<dyn Error>> {
    let entries = match fs::read_dir(terminals_dir()) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    for entry in entries {
        let entry = entry?;
        let Some(terminal_id) = entry.file_name().to_str().and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if !terminal_ids.contains(&terminal_id) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn allocate_terminal_id_from_state(
    state: &mut PersistedWorkspaceState,
    reserved_terminal_ids: &HashSet<u64>,
) -> u64 {
    let used_terminal_ids = state
        .terminals
        .iter()
        .map(|terminal| terminal.id)
        .chain(reserved_terminal_ids.iter().copied())
        .collect::<HashSet<_>>();
    let mut next = state.next_terminal_id.max(1);
    while used_terminal_ids.contains(&next) {
        next = next.checked_add(1).expect("terminal id space exhausted");
    }
    state.next_terminal_id = next.saturating_add(1);
    next
}

fn workspace_layout_terminal_ids(layout: &WorkspaceLayout) -> HashSet<u64> {
    layout
        .groups
        .iter()
        .flat_map(|group| group.tabs.iter())
        .filter_map(|tab| match &tab.kind {
            WorkspaceTabKind::Terminal { terminal_id, .. } => Some(*terminal_id),
            WorkspaceTabKind::Web { .. }
            | WorkspaceTabKind::Image { .. }
            | WorkspaceTabKind::Pdf { .. } => None,
        })
        .collect()
}

fn workspace_layout_file() -> PathBuf {
    persisted_dir().join("workspace-layout.json")
}

fn current_state_file() -> PathBuf {
    persisted_dir().join(STATE_FILE_NAME)
}

fn legacy_state_file() -> PathBuf {
    persisted_dir().join(LEGACY_STATE_FILE_NAME)
}

#[cfg(target_os = "macos")]
fn test_bundle_workspace_root() -> Option<PathBuf> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let bundle_id = macos::bundle_identifier();
    if !bundle_id.starts_with("com.pinkbot.tabor.test.") {
        return None;
    }

    if let Some(path) = std::env::var_os("TABOR_TEST_STATE_ROOT") {
        let path = PathBuf::from(path).join("workspace");
        let _ = fs::create_dir_all(&path);
        return Some(path);
    }

    let mut hasher = DefaultHasher::new();
    bundle_id.hash(&mut hasher);
    let path = PathBuf::from("/tmp").join(format!("ttw-{:016x}", hasher.finish()));
    let _ = fs::create_dir_all(&path);
    Some(path)
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

    fn persisted_terminal_state(id: u64) -> PersistedTerminalState {
        PersistedTerminalState {
            id,
            launch_options: tty::Options::default(),
            title: Some(String::from("shell")),
            program_name: String::from("shell"),
            working_directory: None,
            exit_code: None,
            clean_exit: false,
            snapshot: None,
        }
    }

    #[test]
    fn terminal_output_observer_coalesces_activity() {
        let output_dirty = Arc::new(AtomicBool::new(false));
        let observer = TerminalOutputObserver { output_dirty: Arc::clone(&output_dirty) };

        observer.observe(b"first");
        observer.observe(b"second");

        assert!(output_dirty.swap(false, Ordering::Acquire));
        assert!(!output_dirty.load(Ordering::Acquire));
    }

    #[test]
    fn write_bytes_atomic_if_changed_skips_identical_content() {
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("state.json");

        assert!(
            write_bytes_atomic_if_changed(&path, b"first".to_vec()).expect("write initial content")
        );
        assert!(
            !write_bytes_atomic_if_changed(&path, b"first".to_vec())
                .expect("skip identical content")
        );
        assert!(
            write_bytes_atomic_if_changed(&path, b"second".to_vec())
                .expect("replace changed content")
        );
        assert_eq!(std::fs::read(path).expect("read content"), b"second");
    }

    #[test]
    fn workspace_tab_kind_deserializes_legacy_broker_id() {
        let launch_options =
            serde_json::to_value(tty::Options::default()).expect("serialize launch options");
        let tab: WorkspaceTabKind = serde_json::from_value(serde_json::json!({
            "kind": "terminal",
            "broker_id": 7,
            "launch_options": launch_options
        }))
        .expect("deserialize terminal layout");

        match tab {
            WorkspaceTabKind::Terminal { terminal_id, .. } => assert_eq!(terminal_id, 7),
            _ => panic!("expected terminal layout"),
        }
    }

    #[test]
    fn load_terminal_preview_lines_returns_inline_snapshot_preview_when_no_preview_exists() {
        let mut terminal = Term::new(Default::default(), &TermSize::new(8, 3), VoidListener);
        terminal.apply_preview_lines(&[
            String::from("alpha"),
            String::from("beta"),
            String::from("gamma"),
        ]);
        let snapshot = terminal.export_snapshot();
        let state = PersistedTerminalState {
            id: 77,
            launch_options: tty::Options::default(),
            title: Some(String::from("shell")),
            program_name: String::from("shell"),
            working_directory: None,
            exit_code: None,
            clean_exit: false,
            snapshot: Some(snapshot),
        };

        let restored = load_terminal_preview_lines(77, &state).expect("load preview");
        assert_eq!(
            restored,
            Some(vec![String::from("alpha"), String::from("beta"), String::from("gamma")])
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn load_terminal_preview_lines_migrates_legacy_checkpoint_files() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let tempdir = tempdir().expect("tempdir");
        let home_dir = tempdir.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create home dir");

        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());

        let mut terminal = Term::new(Default::default(), &TermSize::new(8, 3), VoidListener);
        terminal.apply_preview_lines(&[
            String::from("legacy"),
            String::from("preview"),
            String::from("lines"),
        ]);
        let snapshot = terminal.export_snapshot();

        std::fs::create_dir_all(terminal_dir(7)).expect("create terminal dir");
        std::fs::write(
            checkpoint_file(7),
            serde_json::to_vec(&snapshot).expect("serialize checkpoint snapshot"),
        )
        .expect("write legacy checkpoint");
        std::fs::write(journal_file(7), []).expect("write empty legacy journal");

        let state = PersistedTerminalState {
            id: 7,
            launch_options: tty::Options::default(),
            title: Some(String::from("shell")),
            program_name: String::from("shell"),
            working_directory: None,
            exit_code: None,
            clean_exit: false,
            snapshot: None,
        };

        let restored = load_terminal_preview_lines(7, &state).expect("load migrated preview");

        assert_eq!(
            restored,
            Some(vec![String::from("legacy"), String::from("preview"), String::from("lines"),])
        );
        assert_eq!(load_preview_lines_file(7).expect("load migrated preview file"), restored);
        assert!(!checkpoint_file(7).exists());
        assert!(!journal_file(7).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn write_terminal_preview_lines_roundtrips_and_cleans_legacy_files() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let tempdir = tempdir().expect("tempdir");
        let home_dir = tempdir.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create home dir");

        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());

        std::fs::create_dir_all(terminal_dir(7)).expect("create terminal dir");
        std::fs::write(checkpoint_file(7), br#"{"legacy":true}"#).expect("write legacy checkpoint");
        std::fs::write(journal_file(7), b"legacy").expect("write legacy journal");

        let preview_lines = vec![String::from("recent"), String::from("output")];
        write_terminal_preview_lines(7, &preview_lines).expect("write preview");

        assert_eq!(load_preview_lines_file(7).expect("load preview"), Some(preview_lines));
        assert!(!checkpoint_file(7).exists());
        assert!(!journal_file(7).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn allocate_terminal_id_skips_ids_reserved_by_workspace_layout() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let tempdir = tempdir().expect("tempdir");
        let home_dir = tempdir.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create home dir");

        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());

        write_persisted_state(&PersistedWorkspaceState {
            next_terminal_id: 50,
            terminals: Vec::new(),
        })
        .expect("write stale state");
        save_workspace_layout(&WorkspaceLayout {
            protocol_version: WORKSPACE_PROTOCOL_VERSION,
            active_tab_id: None,
            groups: vec![WorkspaceGroupLayout {
                name: None,
                tabs: vec![
                    WorkspaceTabLayout {
                        persistent_id: String::from("g0-t0"),
                        custom_title: None,
                        terminal_view_mode:
                            crate::display::terminal_layout::TerminalViewMode::Normal,
                        terminal_multi_column_count_override: None,
                        kind: WorkspaceTabKind::Terminal {
                            terminal_id: 50,
                            launch_options: tty::Options::default(),
                        },
                    },
                    WorkspaceTabLayout {
                        persistent_id: String::from("g0-t1"),
                        custom_title: None,
                        terminal_view_mode:
                            crate::display::terminal_layout::TerminalViewMode::Normal,
                        terminal_multi_column_count_override: None,
                        kind: WorkspaceTabKind::Terminal {
                            terminal_id: 51,
                            launch_options: tty::Options::default(),
                        },
                    },
                ],
            }],
        })
        .expect("write workspace layout");

        assert_eq!(allocate_terminal_id().expect("allocate terminal id"), 52);
        assert_eq!(load_persisted_state().expect("load state").next_terminal_id, 53);
    }

    #[test]
    fn allocate_terminal_id_skips_ids_already_in_state() {
        let mut state = PersistedWorkspaceState {
            next_terminal_id: 50,
            terminals: vec![persisted_terminal_state(50), persisted_terminal_state(51)],
        };

        assert_eq!(
            allocate_terminal_id_from_state(&mut state, &std::collections::HashSet::new()),
            52
        );
        assert_eq!(state.next_terminal_id, 53);
    }

    #[test]
    fn upsert_terminal_state_advances_next_terminal_id() {
        let mut state = PersistedWorkspaceState { next_terminal_id: 50, terminals: Vec::new() };

        upsert_terminal_state(&mut state, persisted_terminal_state(51));

        assert_eq!(state.next_terminal_id, 52);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn load_persisted_terminals_for_layout_prunes_stale_state_and_directories() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let tempdir = tempdir().expect("tempdir");
        let home_dir = tempdir.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create home dir");

        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());

        write_persisted_state(&PersistedWorkspaceState {
            next_terminal_id: 10,
            terminals: vec![persisted_terminal_state(7), persisted_terminal_state(8)],
        })
        .expect("write state");
        for terminal_id in [7, 8, 9] {
            std::fs::create_dir_all(terminal_dir(terminal_id)).expect("create terminal dir");
        }

        let layout = WorkspaceLayout {
            protocol_version: WORKSPACE_PROTOCOL_VERSION,
            active_tab_id: Some(String::from("g0-t0")),
            groups: vec![WorkspaceGroupLayout {
                name: None,
                tabs: vec![WorkspaceTabLayout {
                    persistent_id: String::from("g0-t0"),
                    custom_title: None,
                    terminal_view_mode: crate::display::terminal_layout::TerminalViewMode::Normal,
                    terminal_multi_column_count_override: None,
                    kind: WorkspaceTabKind::Terminal {
                        terminal_id: 7,
                        launch_options: tty::Options::default(),
                    },
                }],
            }],
        };

        let terminals =
            load_persisted_terminals_for_layout(&layout).expect("load reconciled terminals");

        assert_eq!(terminals.keys().copied().collect::<Vec<_>>(), vec![7]);
        assert_eq!(load_persisted_state().expect("load state").terminals.len(), 1);
        assert!(terminal_dir(7).exists());
        assert!(!terminal_dir(8).exists());
        assert!(!terminal_dir(9).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn write_terminal_preview_lines_removes_empty_preview_file() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let tempdir = tempdir().expect("tempdir");
        let home_dir = tempdir.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create home dir");

        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());

        write_terminal_preview_lines(11, &[String::from("recent")]).expect("seed preview");
        assert!(preview_file(11).exists());

        write_terminal_preview_lines(11, &[]).expect("remove preview");

        assert_eq!(load_preview_lines_file(11).expect("load preview"), None);
        assert!(!preview_file(11).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stop_workspace_removes_terminal_state() {
        let _env_guard = env_lock().lock().expect("environment lock poisoned");
        let tempdir = tempdir().expect("tempdir");
        let home_dir = tempdir.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create home dir");

        let _distribution = EnvVarGuard::set("TABOR_DISTRIBUTION_CHANNEL", "direct");
        let _home = EnvVarGuard::set("HOME", &home_dir.display().to_string());

        let path = persisted_dir();
        std::fs::create_dir_all(path.join("terminals/1")).expect("create terminals dir");
        std::fs::write(path.join(STATE_FILE_NAME), b"{}").expect("write state");
        std::fs::write(path.join("workspace-layout.json"), b"{}").expect("write layout");

        stop_workspace().expect("stop workspace");

        assert!(!path.join(STATE_FILE_NAME).exists());
        assert!(!path.join("workspace-layout.json").exists());
        assert!(!path.join("terminals").exists());
    }
}
