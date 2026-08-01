use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use cef::{ImplTask, Task, WrapTask, rc::Rc};
use log::{debug, error, info, warn};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use tempfile::TempDir;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use super::cef_host_protocol::{
    HostCommand, HostEvent, HostGeometry, PROTOCOL_VERSION, RequestId, SurfaceLeaseId, ViewId,
    read_message, write_message,
};
use super::cef_surface_transport::{
    SurfaceEndpoint, SurfaceFrame, SurfaceReceiveEvent, SurfaceReceiver, SurfaceSender,
};
use crate::event::{Event, EventType, WebCommand};
use crate::tabs::TabId;

const HOST_ARGUMENT: &str = "--tabor-cef-host";
const HOST_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HOST_RESTART_MIN_DELAY: Duration = Duration::from_millis(100);
const HOST_RESTART_MAX_DELAY: Duration = Duration::from_secs(5);
const MAX_QUEUED_VIEW_EVENTS: usize = 4096;

#[derive(Debug, Clone, Default)]
pub(crate) struct CefHostMetrics {
    pub pid: Option<u32>,
    pub generation: u64,
    pub starts: u64,
    pub crashes: u64,
    pub restarts: u64,
    pub active_views: u64,
    pub connected: bool,
    pub last_exit: Option<String>,
    pub last_error: Option<String>,
    pub memory_pressure_tests_started: u64,
    pub memory_pressure_tests_passed: u64,
    pub memory_pressure_tests_failed: u64,
    pub last_memory_pressure_request_id: Option<RequestId>,
    pub last_memory_pressure_error: Option<String>,
}

#[derive(Debug)]
pub(super) enum RemoteViewEvent {
    Host(HostEvent),
    Frame(SurfaceFrame),
    Connected { pid: u32, generation: u64 },
    Unavailable { error: String, crashes: u64 },
}

pub(super) struct RemoteViewInbox {
    queue: Mutex<VecDeque<RemoteViewEvent>>,
    proxy: EventLoopProxy<Event>,
    window_id: WindowId,
    tab_id: TabId,
}

impl RemoteViewInbox {
    pub(super) fn new(
        proxy: EventLoopProxy<Event>,
        window_id: WindowId,
        tab_id: TabId,
    ) -> Arc<Self> {
        Arc::new(Self { queue: Mutex::new(VecDeque::new()), proxy, window_id, tab_id })
    }

    pub(super) fn drain(&self) -> Vec<RemoteViewEvent> {
        self.queue.lock().expect("CEF host view inbox poisoned").drain(..).collect()
    }

    fn push(&self, event: RemoteViewEvent) -> Vec<SurfaceLeaseId> {
        let mut dropped_leases = Vec::new();
        let mut queue = self.queue.lock().expect("CEF host view inbox poisoned");

        if let RemoteViewEvent::Frame(frame) = &event {
            if let Some(index) = queue.iter().rposition(|queued| {
                matches!(
                    queued,
                    RemoteViewEvent::Frame(queued_frame)
                        if std::mem::discriminant(&queued_frame.element)
                            == std::mem::discriminant(&frame.element)
                )
            }) {
                if let Some(RemoteViewEvent::Frame(frame)) = queue.remove(index) {
                    dropped_leases.push(frame.lease_id);
                }
            }
        }

        queue.push_back(event);
        while queue.len() > MAX_QUEUED_VIEW_EVENTS {
            if let Some(RemoteViewEvent::Frame(frame)) = queue.pop_front() {
                dropped_leases.push(frame.lease_id);
            }
        }
        drop(queue);
        let _ = self.proxy.send_event(Event::for_tab(
            EventType::WebViewDirty,
            self.window_id,
            self.tab_id,
        ));
        dropped_leases
    }

    fn editable_focus(&self, editable: bool) {
        let _ = self.proxy.send_event(Event::for_tab(
            EventType::WebEditableFocus { editable },
            self.window_id,
            self.tab_id,
        ));
    }

    fn open_url(&self, url: String, new_tab: bool) {
        let _ = self.proxy.send_event(Event::for_tab(
            EventType::WebCommand(WebCommand::OpenUrl { url, new_tab }),
            self.window_id,
            self.tab_id,
        ));
    }
}

#[derive(Clone)]
pub(super) struct CefHostSupervisor {
    sender: mpsc::Sender<SupervisorMessage>,
    next_view_id: Arc<AtomicU64>,
    next_request_id: Arc<AtomicU64>,
}

impl CefHostSupervisor {
    pub(super) fn shared() -> Result<Arc<Self>, Box<dyn Error>> {
        static SUPERVISOR: OnceLock<Result<Arc<CefHostSupervisor>, String>> = OnceLock::new();
        match SUPERVISOR.get_or_init(|| {
            let helper = super::cef::web_host_subprocess_path()
                .ok_or_else(|| String::from("signed Tabor Web Host helper is missing"))?;
            let socket_dir = tempfile::Builder::new()
                .prefix("tabor-cef-host-")
                .tempdir_in(super::runtime_tmp_dir())
                .map_err(|err| format!("create CEF host runtime directory: {err}"))?;
            fs::set_permissions(socket_dir.path(), fs::Permissions::from_mode(0o700))
                .map_err(|err| format!("secure CEF host runtime directory: {err}"))?;
            let (sender, receiver) = mpsc::channel();
            let surface_receiver = SurfaceReceiver::bind()?;
            let surface_endpoint = surface_receiver.endpoint();
            let surface_event_sender = sender.clone();
            surface_receiver.spawn(move |event| {
                let message = match event {
                    SurfaceReceiveEvent::Frame { generation, frame } => {
                        SupervisorMessage::SurfaceFrame { generation, frame }
                    },
                    SurfaceReceiveEvent::Rejected { generation, view_id, lease_id, error } => {
                        SupervisorMessage::SurfaceRejected { generation, view_id, lease_id, error }
                    },
                };
                let _ = surface_event_sender.send(message);
            })?;
            let event_sender = sender.clone();
            let metrics = host_metrics_cell();
            thread::Builder::new()
                .name(String::from("tabor-cef-supervisor"))
                .spawn(move || {
                    supervisor_loop(
                        helper,
                        socket_dir,
                        surface_endpoint,
                        receiver,
                        event_sender,
                        metrics,
                    )
                })
                .map_err(|err| format!("start CEF host supervisor: {err}"))?;
            Ok(Arc::new(Self {
                sender,
                next_view_id: Arc::new(AtomicU64::new(1)),
                next_request_id: Arc::new(AtomicU64::new(1)),
            }))
        }) {
            Ok(supervisor) => Ok(supervisor.clone()),
            Err(err) => Err(io::Error::other(err.clone()).into()),
        }
    }

    pub(super) fn allocate_view_id(&self) -> ViewId {
        self.next_view_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn allocate_request_id(&self) -> RequestId {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn register(
        &self,
        view_id: ViewId,
        url: String,
        geometry: HostGeometry,
        inbox: &Arc<RemoteViewInbox>,
    ) -> Result<(), String> {
        self.sender
            .send(SupervisorMessage::Register {
                view_id,
                url,
                geometry,
                inbox: Arc::downgrade(inbox),
            })
            .map_err(|_| String::from("CEF host supervisor stopped"))
    }

    pub(super) fn unregister(&self, view_id: ViewId) {
        let _ = self.sender.send(SupervisorMessage::Unregister { view_id });
    }

    pub(super) fn send(&self, command: HostCommand) -> bool {
        self.sender.send(SupervisorMessage::Command(command)).is_ok()
    }
}

pub(crate) fn metrics() -> CefHostMetrics {
    host_metrics_cell().lock().expect("CEF host metrics poisoned").clone()
}

pub(crate) fn crash_for_test() -> Result<(), String> {
    ensure_test_bundle("CEF host crash injection")?;
    let supervisor = CefHostSupervisor::shared().map_err(|err| err.to_string())?;
    if supervisor.send(HostCommand::CrashForTest) {
        Ok(())
    } else {
        Err(String::from("CEF host supervisor stopped"))
    }
}

pub(crate) fn simulate_memory_pressure_for_test(view_id: ViewId) -> Result<RequestId, String> {
    ensure_test_bundle("CEF memory-pressure injection")?;
    let supervisor = CefHostSupervisor::shared().map_err(|err| err.to_string())?;
    let request_id = supervisor.allocate_request_id();
    {
        let metrics_cell = host_metrics_cell();
        let mut metrics = metrics_cell.lock().expect("CEF host metrics poisoned");
        metrics.memory_pressure_tests_started =
            metrics.memory_pressure_tests_started.saturating_add(1);
        metrics.last_memory_pressure_request_id = Some(request_id);
        metrics.last_memory_pressure_error = None;
    }
    if supervisor.send(HostCommand::SimulateMemoryPressureForTest { view_id, request_id }) {
        Ok(request_id)
    } else {
        let metrics_cell = host_metrics_cell();
        let mut metrics = metrics_cell.lock().expect("CEF host metrics poisoned");
        metrics.memory_pressure_tests_failed =
            metrics.memory_pressure_tests_failed.saturating_add(1);
        metrics.last_memory_pressure_error = Some(String::from("CEF host supervisor stopped"));
        Err(String::from("CEF host supervisor stopped"))
    }
}

fn ensure_test_bundle(action: &str) -> Result<(), String> {
    if super::bundle_identifier().starts_with("com.pinkbot.tabor.test.") {
        Ok(())
    } else {
        Err(format!("{action} requires a com.pinkbot.tabor.test.* bundle"))
    }
}

fn host_metrics_cell() -> Arc<Mutex<CefHostMetrics>> {
    static METRICS: OnceLock<Arc<Mutex<CefHostMetrics>>> = OnceLock::new();
    METRICS.get_or_init(|| Arc::new(Mutex::new(CefHostMetrics::default()))).clone()
}

enum SupervisorMessage {
    Register { view_id: ViewId, url: String, geometry: HostGeometry, inbox: Weak<RemoteViewInbox> },
    Unregister { view_id: ViewId },
    Command(HostCommand),
    HostEvent { generation: u64, event: HostEvent },
    SurfaceFrame { generation: u64, frame: SurfaceFrame },
    SurfaceRejected { generation: u64, view_id: ViewId, lease_id: SurfaceLeaseId, error: String },
    HostDisconnected { generation: u64, error: String },
}

struct ViewRegistration {
    url: String,
    geometry: HostGeometry,
    visible: bool,
    focused: bool,
    editable: bool,
    inbox: Weak<RemoteViewInbox>,
}

struct ConnectedHost {
    generation: u64,
    child: Child,
    writer: UnixStream,
    pid: u32,
}

fn supervisor_loop(
    helper: PathBuf,
    socket_dir: TempDir,
    surface_endpoint: SurfaceEndpoint,
    receiver: mpsc::Receiver<SupervisorMessage>,
    sender: mpsc::Sender<SupervisorMessage>,
    metrics: Arc<Mutex<CefHostMetrics>>,
) {
    let socket_path = socket_dir.path().join("host.sock");
    let mut registrations = HashMap::<ViewId, ViewRegistration>::new();
    let mut connected: Option<ConnectedHost> = None;
    let mut generation = 0_u64;
    let mut restart_delay = HOST_RESTART_MIN_DELAY;
    let mut disconnected_since: Option<Instant> = None;

    loop {
        if connected.is_none() && !registrations.is_empty() {
            if disconnected_since.is_some_and(|at| at.elapsed() < restart_delay) {
                let wait = restart_delay.saturating_sub(
                    disconnected_since.expect("checked restart deadline").elapsed(),
                );
                match receiver.recv_timeout(wait) {
                    Ok(message) => {
                        handle_client_message(
                            message,
                            &mut registrations,
                            &mut connected,
                            &metrics,
                        );
                        continue;
                    },
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                }
            }

            generation = generation.saturating_add(1);
            match start_host(&helper, &socket_path, &surface_endpoint, generation, sender.clone()) {
                Ok(mut host) => {
                    let ready = wait_for_ready(
                        generation,
                        &receiver,
                        &mut registrations,
                        &mut host,
                        &metrics,
                    );
                    match ready {
                        Ok(()) => {
                            restart_delay = HOST_RESTART_MIN_DELAY;
                            disconnected_since = None;
                            replay_views(&mut host, &registrations);
                            connected = Some(host);
                        },
                        Err(err) => {
                            stop_child(&mut host.child);
                            record_disconnect(&metrics, None, err.clone(), false);
                            notify_unavailable(&registrations, &metrics, err);
                            disconnected_since = Some(Instant::now());
                            restart_delay = next_restart_delay(restart_delay);
                        },
                    }
                },
                Err(err) => {
                    record_disconnect(&metrics, None, err.clone(), false);
                    notify_unavailable(&registrations, &metrics, err);
                    disconnected_since = Some(Instant::now());
                    restart_delay = next_restart_delay(restart_delay);
                },
            }
            continue;
        }

        let message = match receiver.recv() {
            Ok(message) => message,
            Err(_) => break,
        };

        match message {
            SupervisorMessage::HostEvent { generation: event_generation, event }
                if connected.as_ref().is_some_and(|host| host.generation == event_generation) =>
            {
                handle_host_event(event, &mut registrations, connected.as_mut(), &metrics);
            },
            SupervisorMessage::SurfaceFrame { generation: event_generation, frame }
                if connected.as_ref().is_some_and(|host| host.generation == event_generation) =>
            {
                handle_surface_frame(frame, &registrations, connected.as_mut());
            },
            SupervisorMessage::SurfaceRejected {
                generation: event_generation,
                view_id,
                lease_id,
                error,
            } if connected.as_ref().is_some_and(|host| host.generation == event_generation) => {
                handle_surface_rejected(
                    view_id,
                    lease_id,
                    error,
                    &registrations,
                    connected.as_mut(),
                );
            },
            SupervisorMessage::HostDisconnected { generation: event_generation, error }
                if connected.as_ref().is_some_and(|host| host.generation == event_generation) =>
            {
                let mut host = connected.take().expect("matched connected CEF host");
                let status = host.child.try_wait().ok().flatten();
                stop_child(&mut host.child);
                let crashed = !registrations.is_empty();
                record_disconnect(&metrics, status, error.clone(), crashed);
                notify_unavailable(&registrations, &metrics, error);
                disconnected_since = Some(Instant::now());
                restart_delay = next_restart_delay(restart_delay);
            },
            other => {
                handle_client_message(other, &mut registrations, &mut connected, &metrics);
            },
        }
    }

    if let Some(mut host) = connected {
        let _ = write_message(&mut host.writer, &HostCommand::Shutdown);
        stop_child(&mut host.child);
    }
    let _ = fs::remove_file(socket_path);
}

fn handle_client_message(
    message: SupervisorMessage,
    registrations: &mut HashMap<ViewId, ViewRegistration>,
    connected: &mut Option<ConnectedHost>,
    metrics: &Arc<Mutex<CefHostMetrics>>,
) {
    match message {
        SupervisorMessage::Register { view_id, url, geometry, inbox } => {
            registrations.insert(
                view_id,
                ViewRegistration {
                    url: url.clone(),
                    geometry: geometry.clone(),
                    visible: true,
                    focused: false,
                    editable: false,
                    inbox,
                },
            );
            metrics.lock().expect("CEF host metrics poisoned").active_views =
                registrations.len() as u64;
            if let Some(host) = connected {
                let command = HostCommand::Create { view_id, url, geometry };
                if let Err(err) = write_message(&mut host.writer, &command) {
                    warn!("Failed to create view in CEF host: {err}");
                }
            }
        },
        SupervisorMessage::Unregister { view_id } => {
            registrations.remove(&view_id);
            metrics.lock().expect("CEF host metrics poisoned").active_views =
                registrations.len() as u64;
            if let Some(host) = connected {
                let _ = write_message(&mut host.writer, &HostCommand::Destroy { view_id });
            }
        },
        SupervisorMessage::Command(command) => {
            update_recovery_state(registrations, &command);
            if let Some(host) = connected {
                if let Err(err) = write_message(&mut host.writer, &command) {
                    warn!("Failed to send CEF host command: {err}");
                }
            }
        },
        SupervisorMessage::HostEvent { .. }
        | SupervisorMessage::SurfaceFrame { .. }
        | SupervisorMessage::SurfaceRejected { .. }
        | SupervisorMessage::HostDisconnected { .. } => (),
    }
}

fn update_recovery_state(
    registrations: &mut HashMap<ViewId, ViewRegistration>,
    command: &HostCommand,
) {
    let Some(view_id) = command.view_id() else {
        return;
    };
    let Some(registration) = registrations.get_mut(&view_id) else {
        return;
    };
    match command {
        HostCommand::SetVisible { visible, .. } => registration.visible = *visible,
        HostCommand::SetFocus { focused, .. } => registration.focused = *focused,
        HostCommand::SyncEditableFocus { editable, .. } => registration.editable = *editable,
        HostCommand::UpdateGeometry { geometry, .. } => registration.geometry = geometry.clone(),
        HostCommand::LoadUrl { url, .. } => registration.url = url.clone(),
        _ => (),
    }
}

fn start_host(
    helper: &Path,
    socket_path: &Path,
    surface_endpoint: &SurfaceEndpoint,
    generation: u64,
    sender: mpsc::Sender<SupervisorMessage>,
) -> Result<ConnectedHost, String> {
    let _ = fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)
        .map_err(|err| format!("bind CEF host socket {}: {err}", socket_path.display()))?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("secure CEF host socket {}: {err}", socket_path.display()))?;
    listener.set_nonblocking(true).map_err(|err| format!("configure CEF host socket: {err}"))?;

    let log_path = super::logs_dir().join(format!("tabor-cef-host-{generation}.log"));
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| format!("open CEF host log {}: {err}", log_path.display()))?;
    let stderr = stdout.try_clone().map_err(|err| format!("clone CEF host log handle: {err}"))?;
    let mut child = Command::new(helper)
        .arg(HOST_ARGUMENT)
        .arg(socket_path)
        .arg(surface_endpoint.service_name())
        .arg(surface_endpoint.auth()[0].to_string())
        .arg(surface_endpoint.auth()[1].to_string())
        .arg(generation.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|err| format!("spawn signed CEF host {}: {err}", helper.display()))?;
    let pid = child.id();

    let deadline = Instant::now() + HOST_CONNECT_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!("CEF host exited before connecting: {status}"));
                }
                if Instant::now() >= deadline {
                    stop_child(&mut child);
                    return Err(String::from("CEF host timed out while connecting"));
                }
                thread::sleep(Duration::from_millis(10));
            },
            Err(err) => return Err(format!("accept CEF host connection: {err}")),
        }
    };
    stream.set_nonblocking(false).map_err(|err| format!("configure CEF host stream: {err}"))?;
    let reader = stream.try_clone().map_err(|err| format!("clone CEF host stream: {err}"))?;
    thread::Builder::new()
        .name(format!("tabor-cef-host-reader-{generation}"))
        .spawn(move || read_host_events(reader, generation, sender))
        .map_err(|err| format!("start CEF host reader: {err}"))?;

    Ok(ConnectedHost { generation, child, writer: stream, pid })
}

fn wait_for_ready(
    generation: u64,
    receiver: &mpsc::Receiver<SupervisorMessage>,
    registrations: &mut HashMap<ViewId, ViewRegistration>,
    host: &mut ConnectedHost,
    metrics: &Arc<Mutex<CefHostMetrics>>,
) -> Result<(), String> {
    let deadline = Instant::now() + HOST_CONNECT_TIMEOUT;
    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(String::from("CEF host timed out during handshake"));
        }
        match receiver.recv_timeout(timeout) {
            Ok(SupervisorMessage::HostEvent {
                generation: event_generation,
                event: HostEvent::Ready { protocol_version, pid, cef_version },
            }) if event_generation == generation => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(format!(
                        "CEF host protocol mismatch: host {protocol_version}, Tabor {PROTOCOL_VERSION}"
                    ));
                }
                if pid != host.pid {
                    return Err(format!(
                        "CEF host PID mismatch: spawned {}, connected {pid}",
                        host.pid
                    ));
                }
                info!("CEF host {pid} ready with {cef_version}");
                let mut metrics = metrics.lock().expect("CEF host metrics poisoned");
                metrics.pid = Some(pid);
                metrics.generation = generation;
                metrics.starts = metrics.starts.saturating_add(1);
                metrics.restarts = metrics.starts.saturating_sub(1);
                metrics.connected = true;
                metrics.last_error = None;
                for registration in registrations.values() {
                    if let Some(inbox) = registration.inbox.upgrade() {
                        inbox.push(RemoteViewEvent::Connected { pid, generation });
                    }
                }
                return Ok(());
            },
            Ok(SupervisorMessage::HostDisconnected { generation: event_generation, error })
                if event_generation == generation =>
            {
                return Err(error);
            },
            Ok(message) => {
                let mut disconnected = None;
                handle_client_message(message, registrations, &mut disconnected, metrics);
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(String::from("CEF host timed out during handshake"));
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(String::from("CEF host supervisor channel closed"));
            },
        }
    }
}

fn replay_views(host: &mut ConnectedHost, registrations: &HashMap<ViewId, ViewRegistration>) {
    for (&view_id, registration) in registrations {
        let commands = [
            HostCommand::Create {
                view_id,
                url: registration.url.clone(),
                geometry: registration.geometry.clone(),
            },
            HostCommand::SetVisible { view_id, visible: registration.visible },
            HostCommand::SetFocus { view_id, focused: registration.focused },
            HostCommand::SyncEditableFocus { view_id, editable: registration.editable },
        ];
        for command in commands {
            if let Err(err) = write_message(&mut host.writer, &command) {
                warn!("Failed to restore web view {view_id} in CEF host: {err}");
                return;
            }
        }
    }
}

fn read_host_events(
    mut reader: UnixStream,
    generation: u64,
    sender: mpsc::Sender<SupervisorMessage>,
) {
    loop {
        match read_message::<HostEvent>(&mut reader) {
            Ok(event) => {
                if sender.send(SupervisorMessage::HostEvent { generation, event }).is_err() {
                    return;
                }
            },
            Err(err) => {
                let _ = sender.send(SupervisorMessage::HostDisconnected {
                    generation,
                    error: format!("CEF host connection closed: {err}"),
                });
                return;
            },
        }
    }
}

fn handle_host_event(
    event: HostEvent,
    registrations: &mut HashMap<ViewId, ViewRegistration>,
    mut connected: Option<&mut ConnectedHost>,
    metrics: &Arc<Mutex<CefHostMetrics>>,
) {
    let Some(view_id) = event.view_id() else {
        return;
    };
    let Some(registration) = registrations.get_mut(&view_id) else { return };

    if let HostEvent::Url { url, .. } = &event {
        registration.url = url.clone();
    }
    if let HostEvent::TestResult { request_id, result, .. } = &event {
        let mut metrics = metrics.lock().expect("CEF host metrics poisoned");
        metrics.last_memory_pressure_request_id = Some(*request_id);
        match result {
            Ok(()) => {
                metrics.memory_pressure_tests_passed =
                    metrics.memory_pressure_tests_passed.saturating_add(1);
                metrics.last_memory_pressure_error = None;
            },
            Err(error) => {
                metrics.memory_pressure_tests_failed =
                    metrics.memory_pressure_tests_failed.saturating_add(1);
                metrics.last_memory_pressure_error = Some(error.clone());
            },
        }
    }
    let Some(inbox) = registration.inbox.upgrade() else { return };
    match event {
        HostEvent::EditableFocus { editable, .. } => inbox.editable_focus(editable),
        HostEvent::OpenUrl { url, new_tab, .. } => inbox.open_url(url, new_tab),
        event => {
            let dropped_leases = inbox.push(RemoteViewEvent::Host(event));
            if let Some(host) = connected.as_mut() {
                for lease_id in dropped_leases {
                    let _ = write_message(
                        &mut host.writer,
                        &HostCommand::SurfaceAcquired { view_id, lease_id },
                    );
                }
            }
        },
    }
}

fn handle_surface_frame(
    frame: SurfaceFrame,
    registrations: &HashMap<ViewId, ViewRegistration>,
    mut connected: Option<&mut ConnectedHost>,
) {
    let view_id = frame.view_id;
    let lease_id = frame.lease_id;
    let Some(registration) = registrations.get(&view_id) else {
        acknowledge_surface(&mut connected, view_id, lease_id);
        return;
    };
    let Some(inbox) = registration.inbox.upgrade() else {
        acknowledge_surface(&mut connected, view_id, lease_id);
        return;
    };
    let dropped_leases = inbox.push(RemoteViewEvent::Frame(frame));
    for dropped_lease_id in dropped_leases {
        acknowledge_surface(&mut connected, view_id, dropped_lease_id);
    }
}

fn handle_surface_rejected(
    view_id: ViewId,
    lease_id: SurfaceLeaseId,
    error: String,
    registrations: &HashMap<ViewId, ViewRegistration>,
    mut connected: Option<&mut ConnectedHost>,
) {
    acknowledge_surface(&mut connected, view_id, lease_id);
    let Some(inbox) = registrations.get(&view_id).and_then(|entry| entry.inbox.upgrade()) else {
        return;
    };
    let dropped_leases =
        inbox.push(RemoteViewEvent::Host(HostEvent::AccelerationFailed { view_id, reason: error }));
    for dropped_lease_id in dropped_leases {
        acknowledge_surface(&mut connected, view_id, dropped_lease_id);
    }
}

fn acknowledge_surface(
    host: &mut Option<&mut ConnectedHost>,
    view_id: ViewId,
    lease_id: SurfaceLeaseId,
) {
    if let Some(host) = host {
        let _ =
            write_message(&mut host.writer, &HostCommand::SurfaceAcquired { view_id, lease_id });
    }
}

fn notify_unavailable(
    registrations: &HashMap<ViewId, ViewRegistration>,
    metrics: &Arc<Mutex<CefHostMetrics>>,
    error: String,
) {
    let crashes = metrics.lock().expect("CEF host metrics poisoned").crashes;
    for registration in registrations.values() {
        if let Some(inbox) = registration.inbox.upgrade() {
            inbox.push(RemoteViewEvent::Unavailable { error: error.clone(), crashes });
        }
    }
}

fn record_disconnect(
    metrics: &Arc<Mutex<CefHostMetrics>>,
    status: Option<ExitStatus>,
    error: String,
    crashed: bool,
) {
    let mut metrics = metrics.lock().expect("CEF host metrics poisoned");
    metrics.pid = None;
    metrics.connected = false;
    metrics.last_exit = status.map(|status| status.to_string());
    metrics.last_error = Some(error);
    if crashed {
        metrics.crashes = metrics.crashes.saturating_add(1);
    }
}

fn next_restart_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(HOST_RESTART_MAX_DELAY)
}

fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[derive(Clone)]
pub(super) struct HostEventSender {
    stream: Arc<Mutex<UnixStream>>,
}

impl HostEventSender {
    fn new(stream: UnixStream) -> Self {
        Self { stream: Arc::new(Mutex::new(stream)) }
    }

    pub(super) fn send(&self, event: HostEvent) -> bool {
        let result = write_message(
            &mut *self.stream.lock().expect("CEF host event stream poisoned"),
            &event,
        );
        if let Err(err) = result {
            debug!("Failed to send CEF host event: {err}");
            false
        } else {
            true
        }
    }
}

struct ChildHostState {
    views: HashMap<ViewId, super::webview_cef::WebView>,
    sender: HostEventSender,
    surface_sender: SurfaceSender,
    parent_window: Retained<NSWindow>,
}

thread_local! {
    static CHILD_HOST_STATE: RefCell<Option<ChildHostState>> = const { RefCell::new(None) };
}

static CHILD_COMMAND_QUEUE: OnceLock<Mutex<VecDeque<HostCommand>>> = OnceLock::new();

cef::wrap_task! {
    struct ProcessHostCommandsTask {}

    impl Task {
        fn execute(&self) {
            process_child_commands();
        }
    }
}

pub(crate) fn maybe_run_from_argv<I, S>(args: I) -> Result<bool, Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(false);
    };
    if first.as_ref() != std::ffi::OsStr::new(HOST_ARGUMENT) {
        return Ok(false);
    }
    let socket_path =
        args.next().ok_or_else(|| io::Error::other("CEF host socket path is missing"))?;
    let surface_service = args
        .next()
        .ok_or_else(|| io::Error::other("CEF surface service name is missing"))?
        .as_ref()
        .to_str()
        .ok_or_else(|| io::Error::other("CEF surface service name is not UTF-8"))?
        .to_owned();
    let auth_a = parse_host_u64_argument(args.next(), "CEF surface auth word A")?;
    let auth_b = parse_host_u64_argument(args.next(), "CEF surface auth word B")?;
    let generation = parse_host_u64_argument(args.next(), "CEF host generation")?;
    if args.next().is_some() {
        return Err(io::Error::other("unexpected CEF host arguments").into());
    }
    run_child_host(
        Path::new(socket_path.as_ref()),
        &surface_service,
        [auth_a, auth_b],
        generation,
    )?;
    Ok(true)
}

fn parse_host_u64_argument<S: AsRef<std::ffi::OsStr>>(
    value: Option<S>,
    name: &str,
) -> Result<u64, Box<dyn Error>> {
    value
        .ok_or_else(|| io::Error::other(format!("{name} is missing")))?
        .as_ref()
        .to_str()
        .ok_or_else(|| io::Error::other(format!("{name} is not UTF-8")))?
        .parse()
        .map_err(|error| io::Error::other(format!("invalid {name}: {error}")).into())
}

fn run_child_host(
    socket_path: &Path,
    surface_service: &str,
    surface_auth: [u64; 2],
    generation: u64,
) -> Result<(), Box<dyn Error>> {
    super::enforce_signed_app_launch()?;
    let stream = UnixStream::connect(socket_path)?;
    let reader = stream.try_clone()?;
    let sender = HostEventSender::new(stream);
    let surface_sender = SurfaceSender::connect(surface_service, surface_auth, generation)?;

    super::cef::ensure_initialized_for_host()?;
    let parent_window = create_host_parent_window()?;
    CHILD_HOST_STATE.with(|state| {
        *state.borrow_mut() = Some(ChildHostState {
            views: HashMap::new(),
            sender: sender.clone(),
            surface_sender,
            parent_window,
        });
    });

    sender.send(HostEvent::Ready {
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        cef_version: String::from(super::cef::CEF_RUNTIME_VERSION),
    });
    thread::Builder::new()
        .name(String::from("tabor-cef-host-commands"))
        .spawn(move || read_child_commands(reader))?;

    cef::run_message_loop();
    CHILD_HOST_STATE.with(|state| state.borrow_mut().take());
    super::cef::shutdown();
    Ok(())
}

fn create_host_parent_window() -> Result<Retained<NSWindow>, Box<dyn Error>> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("CEF host must start on the main thread"))?;
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    Ok(window)
}

fn read_child_commands(mut reader: UnixStream) {
    loop {
        let command = match read_message::<HostCommand>(&mut reader) {
            Ok(command) => command,
            Err(err) => {
                debug!("CEF host parent connection closed: {err}");
                HostCommand::Shutdown
            },
        };
        CHILD_COMMAND_QUEUE
            .get_or_init(|| Mutex::new(VecDeque::new()))
            .lock()
            .expect("CEF host command queue poisoned")
            .push_back(command.clone());
        let mut task = ProcessHostCommandsTask::new();
        if cef::post_task(cef::ThreadId::UI, Some(&mut task)) == 0 {
            error!("Failed to post CEF host command to UI thread");
            return;
        }
        if matches!(command, HostCommand::Shutdown) {
            return;
        }
    }
}

fn process_child_commands() {
    let commands = CHILD_COMMAND_QUEUE
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .expect("CEF host command queue poisoned")
        .drain(..)
        .collect::<Vec<_>>();
    for command in commands {
        process_child_command(command);
    }
}

fn process_child_command(command: HostCommand) {
    CHILD_HOST_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(state) = state.as_mut() else {
            return;
        };
        match command {
            HostCommand::Create { view_id, url, geometry } => {
                let parent_view = state
                    .parent_window
                    .contentView()
                    .map(|view| Retained::as_ptr(&view).cast_mut().cast())
                    .unwrap_or(std::ptr::null_mut());
                match super::webview_cef::WebView::new_host(
                    parent_view,
                    geometry,
                    view_id,
                    &url,
                    state.sender.clone(),
                    state.surface_sender.clone(),
                ) {
                    Ok(view) => {
                        state.views.insert(view_id, view);
                        state.sender.send(HostEvent::ViewReady { view_id });
                    },
                    Err(err) => {
                        state
                            .sender
                            .send(HostEvent::ViewFailed { view_id, error: err.to_string() });
                    },
                }
            },
            HostCommand::Destroy { view_id } => {
                state.views.remove(&view_id);
            },
            HostCommand::SetVisible { view_id, visible } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.set_visible(visible);
                }
            },
            HostCommand::SetFocus { view_id, focused } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.set_focus(focused);
                }
            },
            HostCommand::SyncEditableFocus { view_id, editable } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.sync_editable_focus(editable);
                }
            },
            HostCommand::UpdateGeometry { view_id, geometry } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.update_host_geometry(geometry);
                }
            },
            HostCommand::LoadUrl { view_id, url } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.load_url(&url);
                }
            },
            HostCommand::Reload { view_id } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.reload();
                }
            },
            HostCommand::GoBack { view_id } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.go_back();
                }
            },
            HostCommand::GoForward { view_id } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.go_forward();
                }
            },
            HostCommand::MouseClick { view_id, event, button, mouse_up, click_count } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.host_mouse_click(event, button, mouse_up, click_count);
                }
            },
            HostCommand::MouseMove { view_id, event, mouse_leave } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.host_mouse_move(event, mouse_leave);
                }
            },
            HostCommand::MouseWheel { view_id, event, delta_x, delta_y } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.host_mouse_wheel(event, delta_x, delta_y);
                }
            },
            HostCommand::ImeCommit { view_id, text } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.handle_ime_commit(&text);
                }
            },
            HostCommand::ImePreedit { view_id, text, cursor_offset } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.handle_ime_preedit(&text, cursor_offset);
                }
            },
            HostCommand::ImeCancel { view_id } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.cancel_ime_composition();
                }
            },
            HostCommand::KeyEvents { view_id, events, invalidate_after } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.host_key_events(events, invalidate_after);
                }
            },
            HostCommand::Evaluate { view_id, request_id, script, user_gesture } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    let sender = state.sender.clone();
                    view.eval_js_string_impl_for_host(&script, user_gesture, move |result| {
                        sender.send(HostEvent::EvaluateResult { view_id, request_id, result });
                    });
                }
            },
            HostCommand::FrameEdit { view_id, command } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.host_frame_edit(command);
                }
            },
            HostCommand::DevTools { view_id, request_id, method, params } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    let sender = state.sender.clone();
                    if let Err(err) = view.devtools_command_json(&method, params, move |result| {
                        sender.send(HostEvent::DevToolsResult { view_id, request_id, result });
                    }) {
                        state.sender.send(HostEvent::DevToolsResult {
                            view_id,
                            request_id,
                            result: Err(err),
                        });
                    }
                }
            },
            HostCommand::RenewAgentEventCapture { view_id } => {
                if let Some(view) = state.views.get(&view_id) {
                    view.renew_agent_event_capture();
                }
            },
            HostCommand::RetainInspectorSession { view_id } => {
                if let Some(view) = state.views.get(&view_id) {
                    view.retain_inspector_session();
                }
            },
            HostCommand::ReleaseInspectorSession { view_id } => {
                if let Some(view) = state.views.get(&view_id) {
                    view.release_inspector_session();
                }
            },
            HostCommand::SetFileInputFiles { view_id, request_id, element_id, paths } => {
                if let Some(view) = state.views.get(&view_id) {
                    let sender = state.sender.clone();
                    if let Err(err) = view.set_file_input_files(&element_id, paths, move |result| {
                        sender.send(HostEvent::FileInputResult { view_id, request_id, result });
                    }) {
                        state.sender.send(HostEvent::FileInputResult {
                            view_id,
                            request_id,
                            result: Err(err),
                        });
                    }
                }
            },
            HostCommand::ShowInspector { view_id } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.show_inspector();
                }
            },
            HostCommand::SurfaceAcquired { view_id, lease_id } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.release_host_surface_lease(lease_id);
                }
            },
            HostCommand::JsDialogResult { view_id, dialog_id, accepted, prompt_text } => {
                if let Some(view) = state.views.get_mut(&view_id) {
                    view.complete_host_js_dialog(dialog_id, accepted, prompt_text.as_deref());
                }
            },
            HostCommand::SimulateMemoryPressureForTest { view_id, request_id } => {
                if !super::bundle_identifier().starts_with("com.pinkbot.tabor.test.") {
                    state.sender.send(HostEvent::TestResult {
                        view_id,
                        request_id,
                        result: Err(String::from("memory-pressure injection requires test bundle")),
                    });
                } else if let Some(view) = state.views.get(&view_id) {
                    let sender = state.sender.clone();
                    let result = view.devtools_command_json(
                        "Memory.simulatePressureNotification",
                        Some(serde_json::json!({ "level": "critical" })),
                        move |result| {
                            sender.send(HostEvent::TestResult {
                                view_id,
                                request_id,
                                result: result.map(|_| ()),
                            });
                        },
                    );
                    if let Err(err) = result {
                        state.sender.send(HostEvent::TestResult {
                            view_id,
                            request_id,
                            result: Err(err),
                        });
                    }
                } else {
                    state.sender.send(HostEvent::TestResult {
                        view_id,
                        request_id,
                        result: Err(String::from("web view not found")),
                    });
                }
            },
            HostCommand::CrashForTest => {
                if super::bundle_identifier().starts_with("com.pinkbot.tabor.test.") {
                    unsafe { libc::raise(libc::SIGTRAP) };
                }
            },
            HostCommand::Shutdown => cef::quit_message_loop(),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{RemoteViewInbox, ViewRegistration, next_restart_delay, update_recovery_state};
    use crate::display::browser_layout::{BrowserViewportLayout, BrowserViewportRect};
    use crate::macos::cef_host_protocol::{HostCommand, HostGeometry, HostRect};
    use std::collections::HashMap;
    use std::sync::Weak;
    use std::time::Duration;

    #[test]
    fn restart_backoff_is_bounded() {
        let mut delay = Duration::from_millis(100);
        for _ in 0..20 {
            delay = next_restart_delay(delay);
        }
        assert_eq!(delay, Duration::from_secs(5));
    }

    #[test]
    fn recovery_state_tracks_latest_tab_state() {
        let view_id = 17;
        let geometry = HostGeometry {
            layout: BrowserViewportLayout::normal(
                BrowserViewportRect { x: 0, y: 0, width: 800, height: 600 },
                800,
            ),
            screen_rect: HostRect { x: 10, y: 20, width: 800, height: 600 },
            scale_factor: 2.0,
        };
        let mut registrations = HashMap::from([(
            view_id,
            ViewRegistration {
                url: String::from("https://before.example"),
                geometry: geometry.clone(),
                visible: true,
                focused: false,
                editable: false,
                inbox: Weak::<RemoteViewInbox>::new(),
            },
        )]);
        let updated_geometry = HostGeometry {
            screen_rect: HostRect { x: 30, y: 40, width: 900, height: 700 },
            ..geometry
        };

        for command in [
            HostCommand::LoadUrl { view_id, url: String::from("https://after.example/recovered") },
            HostCommand::UpdateGeometry { view_id, geometry: updated_geometry.clone() },
            HostCommand::SetVisible { view_id, visible: false },
            HostCommand::SetFocus { view_id, focused: true },
            HostCommand::SyncEditableFocus { view_id, editable: true },
        ] {
            update_recovery_state(&mut registrations, &command);
        }

        let registration = registrations.get(&view_id).expect("view registration");
        assert_eq!(registration.url, "https://after.example/recovered");
        assert_eq!(registration.geometry.screen_rect.x, updated_geometry.screen_rect.x);
        assert_eq!(registration.geometry.screen_rect.width, updated_geometry.screen_rect.width);
        assert!(!registration.visible);
        assert!(registration.focused);
        assert!(registration.editable);
    }
}
