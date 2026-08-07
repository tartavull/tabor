//! Tabor - The GPU Enhanced Terminal.

#![warn(rust_2018_idioms, future_incompatible)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
#![cfg_attr(clippy, deny(warnings))]
// With the default subsystem, 'console', windows creates an additional console
// window for the program.
// This is silently ignored on non-windows systems.
// See https://msdn.microsoft.com/en-us/library/4cc7ya5b.aspx for more details.
#![windows_subsystem = "windows"]

#[cfg(not(any(feature = "x11", feature = "wayland", target_os = "macos", windows)))]
compile_error!(r#"at least one of the "x11"/"wayland" features must be enabled"#);

use std::error::Error;
#[cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::{env, fs};

use log::info;
#[cfg(windows)]
use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole};
use winit::event_loop::EventLoop;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};

use tabor_terminal::tty;

#[cfg(unix)]
mod agent;
mod cli;
mod clipboard;
mod config;
mod daemon;
mod display;
mod event;
mod input;
#[cfg(unix)]
mod ipc;
#[cfg(target_os = "macos")]
mod lifecycle;
mod logging;
#[cfg(target_os = "macos")]
mod macos;
mod message_bar;
mod migrate;
#[cfg(windows)]
mod panic;
mod renderer;
mod scheduler;
mod string;
mod tab_panel;
#[cfg(target_os = "macos")]
mod tab_panel_icons;
mod tabs;
#[cfg(target_os = "macos")]
mod text_edit;
mod web_url;
mod window_context;
mod window_kind;
#[cfg(unix)]
mod workspace;

mod gl {
    #![allow(clippy::all, unsafe_op_in_unsafe_fn)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

#[cfg(unix)]
use crate::cli::WindowOptions;
#[cfg(unix)]
use crate::cli::{
    MessageOptions, MsgCloseTab, MsgCreateGroup, MsgCreateTab, MsgDispatchAction, MsgGetTabState,
    MsgInspector, MsgInspectorAttach, MsgInspectorDetach, MsgInspectorPoll, MsgInspectorSend,
    MsgMoveTab, MsgOpenInspector, MsgOpenUrl, MsgReloadWeb, MsgRunCommandBar, MsgSelectTab,
    MsgSendInput, MsgSetGroupName, MsgSetTabPanel, MsgSetTabTitle, MsgSetWebUrl, MsgTerminalRead,
    MsgTerminalScreenshot, MsgTerminalTarget, TabIdArg, TerminalReadScopeArg, parse_tab_id,
};
use crate::cli::{Options, Subcommands};
use crate::config::UiConfig;
use crate::config::monitor::ConfigMonitor;
#[cfg(unix)]
use crate::config::ui_config::Program;
use crate::event::{Event, Processor};
#[cfg(target_os = "macos")]
use crate::macos::locale;
#[cfg(unix)]
use crate::window_kind::WindowKind;
#[cfg(unix)]
use base64::Engine;
#[cfg(unix)]
use base64::engine::general_purpose::STANDARD as BASE64;

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    panic::attach_handler();

    // When linked with the windows subsystem windows won't automatically attach
    // to the console of the parent process, so we do it explicitly. This fails
    // silently if the parent has no console.
    #[cfg(windows)]
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }

    #[cfg(target_os = "macos")]
    if macos::cef_host::maybe_run_from_argv(std::env::args_os().skip(1))? {
        return Ok(());
    }

    #[cfg(unix)]
    if agent::maybe_run_internal_from_argv()? {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    if args_indicate_cef_subprocess(std::env::args_os().skip(1)) {
        let Some(exit_code) = macos::cef::maybe_execute_subprocess()? else {
            return Err("CEF subprocess bootstrap returned without an exit code".into());
        };
        std::process::exit(exit_code);
    }

    let options = Options::new();
    #[cfg(target_os = "macos")]
    let record_lifecycle = options.subcommands.is_none();
    #[cfg(target_os = "macos")]
    if record_lifecycle {
        lifecycle::install_panic_hook();
        lifecycle::record_process_start();
    }

    let result = dispatch_options(options);
    #[cfg(target_os = "macos")]
    if record_lifecycle {
        match &result {
            Ok(()) => lifecycle::record_process_return("ok"),
            Err(err) => lifecycle::record_process_return(&format!("error: {err}")),
        }
    }

    result
}

fn dispatch_options(options: Options) -> Result<(), Box<dyn Error>> {
    match options.subcommands {
        #[cfg(unix)]
        Some(Subcommands::Msg(options)) => msg(options)?,
        #[cfg(unix)]
        Some(Subcommands::Agent(options)) => agent::run(options)?,
        #[cfg(unix)]
        Some(Subcommands::Workspace(options)) => workspace::run(options)?,
        Some(Subcommands::Migrate(options)) => migrate::migrate(options),
        None => {
            #[cfg(target_os = "macos")]
            {
                macos::enforce_signed_app_launch()?;
                macos::enforce_supported_gui_launch_context()?;
            }
            tabor(options)?
        },
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn args_indicate_cef_subprocess<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return false;
    };
    let first = first.as_ref();

    if first == OsStr::new("--type") {
        return args.next().is_some_and(|arg| !arg.as_ref().is_empty());
    }

    first
        .to_str()
        .and_then(|value| value.strip_prefix("--type="))
        .is_some_and(|value| !value.is_empty())
}

/// `msg` subcommand entrypoint.
#[cfg(unix)]
#[allow(unused_mut)]
fn msg(mut options: MessageOptions) -> Result<(), Box<dyn Error>> {
    fn ipc_tab_id(tab_id: TabIdArg) -> ipc::IpcTabId {
        ipc::IpcTabId { index: tab_id.index, generation: tab_id.generation }
    }

    fn source_tab_id_from_env() -> Result<Option<ipc::IpcTabId>, Box<dyn Error>> {
        match env::var(ipc::TABOR_TAB_ID_ENV) {
            Ok(value) => parse_tab_id(&value)
                .map(ipc_tab_id)
                .map(Some)
                .map_err(|err| format!("invalid {}: {}", ipc::TABOR_TAB_ID_ENV, err).into()),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(err) => Err(format!("invalid {}: {}", ipc::TABOR_TAB_ID_ENV, err).into()),
        }
    }

    fn ipc_terminal_read_scope(scope: TerminalReadScopeArg) -> ipc::IpcTerminalReadScope {
        match scope {
            TerminalReadScopeArg::Viewport => ipc::IpcTerminalReadScope::Viewport,
            TerminalReadScopeArg::Buffer => ipc::IpcTerminalReadScope::Buffer,
            TerminalReadScopeArg::Selection => ipc::IpcTerminalReadScope::Selection,
        }
    }

    fn write_terminal_screenshot(
        reply: Option<ipc::SocketReply>,
        path: PathBuf,
    ) -> Result<(), Box<dyn Error>> {
        match reply {
            Some(ipc::SocketReply::TerminalScreenshot { screenshot }) => {
                let png = BASE64.decode(screenshot.data_base64.as_bytes())?;
                fs::write(&path, png)?;
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "type": "terminal_screenshot",
                        "path": path,
                        "width": screenshot.width,
                        "height": screenshot.height,
                    }))?
                );
                Ok(())
            },
            Some(ipc::SocketReply::Error { error }) => Err(error.message.into()),
            Some(other) => {
                Err(format!("unexpected reply for terminal screenshot: {other:?}").into())
            },
            None => Err("missing reply for terminal screenshot".into()),
        }
    }

    fn print_reply(reply: Option<ipc::SocketReply>) -> Result<(), Box<dyn Error>> {
        if let Some(reply) = reply {
            println!("{}", serde_json::to_string(&reply)?);
            if let ipc::SocketReply::Error { error } = reply {
                return Err(error.message.into());
            }
        }
        Ok(())
    }

    fn send_request(
        socket: &Option<PathBuf>,
        request: ipc::IpcRequest,
    ) -> Result<(), Box<dyn Error>> {
        let reply = ipc::send_message(socket.clone(), request)?;
        print_reply(reply)
    }

    let socket = options.socket.clone();

    match options.message {
        crate::cli::MessageCommand::Config(config) => {
            let reply = ipc::send_message(socket.clone(), ipc::IpcRequest::SetConfig(config))?;
            if let Some(ipc::SocketReply::Error { error }) = reply {
                return Err(error.message.into());
            }
        },
        crate::cli::MessageCommand::GetConfig(config) => {
            let reply = ipc::send_message(socket.clone(), ipc::IpcRequest::GetConfig(config))?;
            match reply {
                Some(ipc::SocketReply::Config { config }) => {
                    println!("{}", serde_json::to_string(&config)?);
                },
                Some(ipc::SocketReply::Error { error }) => {
                    return Err(error.message.into());
                },
                _ => (),
            }
        },
        crate::cli::MessageCommand::Ping => {
            send_request(&socket, ipc::IpcRequest::Ping)?;
        },
        crate::cli::MessageCommand::GetCapabilities => {
            send_request(&socket, ipc::IpcRequest::GetCapabilities)?;
        },
        crate::cli::MessageCommand::ListTabs => {
            send_request(&socket, ipc::IpcRequest::ListTabs)?;
        },
        crate::cli::MessageCommand::GetTabState(MsgGetTabState { tab_id }) => {
            send_request(&socket, ipc::IpcRequest::GetTabState { tab_id: ipc_tab_id(tab_id) })?;
        },
        crate::cli::MessageCommand::CreateTab(MsgCreateTab {
            web,
            group_id,
            group_name,
            terminal_options,
            window_identity,
        }) => {
            let mut tab_options = WindowOptions::default();
            tab_options.terminal_options = terminal_options;
            tab_options.window_identity = window_identity;
            tab_options.window_kind = match web {
                Some(url) => WindowKind::Web { url },
                None => WindowKind::Terminal,
            };
            send_request(
                &socket,
                ipc::IpcRequest::CreateTab { options: tab_options, group_id, group_name },
            )?;
        },
        crate::cli::MessageCommand::CreateGroup(MsgCreateGroup { name }) => {
            send_request(&socket, ipc::IpcRequest::CreateGroup { name })?;
        },
        crate::cli::MessageCommand::CloseTab(MsgCloseTab { tab_id }) => {
            send_request(&socket, ipc::IpcRequest::CloseTab { tab_id: tab_id.map(ipc_tab_id) })?;
        },
        crate::cli::MessageCommand::SelectTab(MsgSelectTab {
            active,
            next,
            previous,
            last,
            index,
            tab_id,
        }) => {
            let selection = if active {
                ipc::TabSelection::Active
            } else if next {
                ipc::TabSelection::Next
            } else if previous {
                ipc::TabSelection::Previous
            } else if last {
                ipc::TabSelection::Last
            } else if let Some(index) = index {
                ipc::TabSelection::ByIndex { index }
            } else {
                ipc::TabSelection::ById { tab_id: ipc_tab_id(tab_id.expect("tab id")) }
            };
            send_request(&socket, ipc::IpcRequest::SelectTab { selection })?;
        },
        crate::cli::MessageCommand::MoveTab(MsgMoveTab {
            tab_id,
            target_group_id,
            target_index,
        }) => {
            send_request(
                &socket,
                ipc::IpcRequest::MoveTab {
                    tab_id: ipc_tab_id(tab_id),
                    target_group_id,
                    target_index,
                },
            )?;
        },
        crate::cli::MessageCommand::SetTabTitle(MsgSetTabTitle { tab_id, title, clear }) => {
            let title = if clear { None } else { title };
            send_request(
                &socket,
                ipc::IpcRequest::SetTabTitle { tab_id: tab_id.map(ipc_tab_id), title },
            )?;
        },
        crate::cli::MessageCommand::SetGroupName(MsgSetGroupName { group_id, name, clear }) => {
            let name = if clear { None } else { name };
            send_request(&socket, ipc::IpcRequest::SetGroupName { group_id, name })?;
        },
        crate::cli::MessageCommand::RestoreClosedTab => {
            send_request(&socket, ipc::IpcRequest::RestoreClosedTab)?;
        },
        crate::cli::MessageCommand::OpenUrl(MsgOpenUrl { url, new_tab, tab_id }) => {
            let target = if new_tab {
                match source_tab_id_from_env()? {
                    Some(source_tab_id) => ipc::UrlTarget::NewTabInSourceGroup { source_tab_id },
                    None => ipc::UrlTarget::NewTab,
                }
            } else if let Some(tab_id) = tab_id {
                ipc::UrlTarget::TabId { tab_id: ipc_tab_id(tab_id) }
            } else {
                ipc::UrlTarget::Current
            };
            send_request(&socket, ipc::IpcRequest::OpenUrl { url, target })?;
        },
        crate::cli::MessageCommand::SetWebUrl(MsgSetWebUrl { url, tab_id }) => {
            send_request(
                &socket,
                ipc::IpcRequest::SetWebUrl { tab_id: tab_id.map(ipc_tab_id), url },
            )?;
        },
        crate::cli::MessageCommand::ReloadWeb(MsgReloadWeb { tab_id }) => {
            send_request(&socket, ipc::IpcRequest::ReloadWeb { tab_id: tab_id.map(ipc_tab_id) })?;
        },
        crate::cli::MessageCommand::OpenInspector(MsgOpenInspector { tab_id }) => {
            send_request(
                &socket,
                ipc::IpcRequest::OpenInspector { tab_id: tab_id.map(ipc_tab_id) },
            )?;
        },
        crate::cli::MessageCommand::TerminalObserve(MsgTerminalTarget { tab_id }) => {
            send_request(
                &socket,
                ipc::IpcRequest::TerminalObserve { tab_id: tab_id.map(ipc_tab_id) },
            )?;
        },
        crate::cli::MessageCommand::TerminalRead(MsgTerminalRead { tab_id, scope, max_lines }) => {
            send_request(
                &socket,
                ipc::IpcRequest::TerminalRead {
                    tab_id: tab_id.map(ipc_tab_id),
                    scope: ipc_terminal_read_scope(scope),
                    max_lines,
                },
            )?;
        },
        crate::cli::MessageCommand::TerminalScreenshot(MsgTerminalScreenshot { tab_id, path }) => {
            let reply = ipc::send_message(
                socket.clone(),
                ipc::IpcRequest::TerminalScreenshot { tab_id: tab_id.map(ipc_tab_id) },
            )?;
            if let Some(path) = path {
                write_terminal_screenshot(reply, path)?;
            } else {
                print_reply(reply)?;
            }
        },
        crate::cli::MessageCommand::GetTabPanel => {
            send_request(&socket, ipc::IpcRequest::GetTabPanel)?;
        },
        crate::cli::MessageCommand::SetTabPanel(MsgSetTabPanel { enable, disable, width }) => {
            let enabled = if enable {
                Some(true)
            } else if disable {
                Some(false)
            } else {
                None
            };
            send_request(&socket, ipc::IpcRequest::SetTabPanel { enabled, width })?;
        },
        crate::cli::MessageCommand::DispatchAction(MsgDispatchAction {
            tab_id,
            action,
            vi_motion,
            vi_action,
            search_action,
            mouse_action,
            esc,
            command,
        }) => {
            let action = if let Some(name) = action {
                ipc::IpcAction::Action { name }
            } else if let Some(motion) = vi_motion {
                ipc::IpcAction::ViMotion { motion }
            } else if let Some(action) = vi_action {
                ipc::IpcAction::ViAction { action }
            } else if let Some(action) = search_action {
                ipc::IpcAction::SearchAction { action }
            } else if let Some(action) = mouse_action {
                ipc::IpcAction::MouseAction { action }
            } else if let Some(sequence) = esc {
                ipc::IpcAction::Esc { sequence }
            } else if let Some(command) = command {
                let (program, args) = command.split_first().expect("command");
                let program = if args.is_empty() {
                    Program::Just(program.clone())
                } else {
                    Program::WithArgs { program: program.clone(), args: args.to_vec() }
                };
                ipc::IpcAction::Command { program }
            } else {
                return Err("No action provided".into());
            };
            send_request(
                &socket,
                ipc::IpcRequest::DispatchAction { tab_id: tab_id.map(ipc_tab_id), action },
            )?;
        },
        crate::cli::MessageCommand::SendInput(MsgSendInput { text, tab_id }) => {
            send_request(
                &socket,
                ipc::IpcRequest::SendInput { tab_id: tab_id.map(ipc_tab_id), text },
            )?;
        },
        crate::cli::MessageCommand::RunCommandBar(MsgRunCommandBar { input, tab_id }) => {
            send_request(
                &socket,
                ipc::IpcRequest::RunCommandBar { tab_id: tab_id.map(ipc_tab_id), input },
            )?;
        },
        crate::cli::MessageCommand::Inspector { command } => match command {
            MsgInspector::ListTargets => {
                send_request(&socket, ipc::IpcRequest::ListInspectorTargets)?;
            },
            MsgInspector::Attach(MsgInspectorAttach { tab_id, target_id }) => {
                send_request(
                    &socket,
                    ipc::IpcRequest::AttachInspector { tab_id: tab_id.map(ipc_tab_id), target_id },
                )?;
            },
            MsgInspector::Detach(MsgInspectorDetach { session_id }) => {
                send_request(&socket, ipc::IpcRequest::DetachInspector { session_id })?;
            },
            MsgInspector::Send(MsgInspectorSend { session_id, message }) => {
                send_request(
                    &socket,
                    ipc::IpcRequest::SendInspectorMessage { session_id, message },
                )?;
            },
            MsgInspector::Poll(MsgInspectorPoll { session_id, max }) => {
                send_request(&socket, ipc::IpcRequest::PollInspectorMessages { session_id, max })?;
            },
        },
        crate::cli::MessageCommand::Send { json } => {
            let reply = ipc::send_raw_message(socket, &json)?;
            if let Some(reply) = reply {
                println!("{}", serde_json::to_string(&reply)?);
            }
        },
        crate::cli::MessageCommand::ListRequests => {
            println!("Available IPC request types:");
            for entry in ipc::ipc_request_help() {
                println!("{:<24} {}", entry.name, entry.summary);
            }
            println!("\nSee docs/ipc.md for full request schemas and examples.");
        },
    }

    Ok(())
}

/// Temporary files stored for Tabor.
///
/// This stores temporary files to automate their destruction through its `Drop` implementation.
struct TemporaryFiles {
    #[cfg(unix)]
    socket_path: Option<PathBuf>,
    log_file: Option<PathBuf>,
}

impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        // Clean up the IPC socket file.
        #[cfg(unix)]
        if let Some(socket_path) = &self.socket_path {
            let _ = fs::remove_file(socket_path);
        }

        // Clean up logfile.
        if let Some(log_file) = &self.log_file {
            if fs::remove_file(log_file).is_ok() {
                let _ = writeln!(io::stdout(), "Deleted log file at \"{}\"", log_file.display());
            }
        }
    }
}

/// Run main Tabor entrypoint.
///
/// Creates a window, the terminal state, PTY, I/O event loop, input processor,
/// config change monitor, and runs the main display loop.
fn tabor(mut options: Options) -> Result<(), Box<dyn Error>> {
    // Setup winit event loop.
    let window_event_loop = EventLoop::<Event>::with_user_event().build()?;

    // Initialize the logger as soon as possible as to capture output from other subsystems.
    let log_file = logging::initialize(&options, window_event_loop.create_proxy())
        .expect("Unable to initialize logger");

    info!("Welcome to Tabor");
    info!("Version {}", env!("VERSION"));

    #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
    info!(
        "Running on {}",
        if matches!(
            window_event_loop.display_handle().unwrap().as_raw(),
            RawDisplayHandle::Wayland(_)
        ) {
            "Wayland"
        } else {
            "X11"
        }
    );
    #[cfg(not(any(feature = "x11", target_os = "macos", windows)))]
    info!("Running on Wayland");

    // Load configuration file.
    let config = config::load(&mut options);
    log_config_path(&config);

    // Update the log level from config.
    log::set_max_level(config.debug.log_level);

    // Set tty environment variables.
    tty::setup_env();

    // Set env vars from config.
    for (key, value) in config.env.iter() {
        unsafe { env::set_var(key, value) };
    }

    // GUI launches inherit LaunchServices cwd, so normalize to the preferred working directory.
    #[cfg(target_os = "macos")]
    env::set_current_dir(macos::preferred_working_dir()).unwrap();

    #[cfg(target_os = "macos")]
    macos::ensure_cef_application()?;

    #[cfg(target_os = "macos")]
    macos::register_open_documents_handler(window_event_loop.create_proxy());

    // Set macOS locale.
    #[cfg(target_os = "macos")]
    locale::set_locale_environment();

    #[cfg(target_os = "macos")]
    macos::disable_app_nap();

    #[cfg(target_os = "macos")]
    macos::set_background_activation();

    #[cfg(target_os = "macos")]
    macos::disable_autofill();

    // Create the IPC socket listener.
    #[cfg(unix)]
    let socket_path = if config.ipc_socket() {
        match ipc::spawn_ipc_socket(&options, window_event_loop.create_proxy()) {
            Ok(path) => Some(path),
            Err(err) if options.daemon => return Err(err.into()),
            Err(err) => {
                log::warn!("Unable to create socket: {err:?}");
                None
            },
        }
    } else {
        None
    };

    // Setup automatic RAII cleanup for our files.
    let log_cleanup = log_file.filter(|_| !config.debug.persistent_logging);
    let _files = TemporaryFiles {
        #[cfg(unix)]
        socket_path,
        log_file: log_cleanup,
    };

    // Event processor.
    let mut processor = Processor::new(config, options, &window_event_loop);

    // Start event loop and block until shutdown.
    let result = processor.run(window_event_loop);

    // `Processor` must be dropped before calling `FreeConsole`.
    //
    // This is needed for ConPTY backend. Otherwise a deadlock can occur.
    // The cause:
    //   - Drop for ConPTY will deadlock if the conout pipe has already been dropped
    //   - ConPTY is dropped when the last of processor and window context are dropped, because both
    //     of them own an Arc<ConPTY>
    //
    // The fix is to ensure that processor is dropped first. That way, when window context (i.e.
    // PTY) is dropped, it can ensure ConPTY is dropped before the conout pipe in the PTY drop
    // order.
    //
    // FIXME: Change PTY API to enforce the correct drop order with the typesystem.

    // Terminate the config monitor.
    if let Some(config_monitor) = processor.config_monitor.take() {
        config_monitor.shutdown();
    }

    // Drop processor before shutting down platform services.
    drop(processor);

    // Without explicitly detaching the console cmd won't redraw it's prompt.
    #[cfg(windows)]
    unsafe {
        FreeConsole();
    }

    info!("Goodbye");

    result
}

fn log_config_path(config: &UiConfig) {
    if config.config_paths.is_empty() {
        return;
    }

    let mut msg = String::from("Configuration files loaded from:");
    for path in &config.config_paths {
        let _ = write!(msg, "\n  {:?}", path.display());
    }

    info!("{msg}");
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::args_indicate_cef_subprocess;

    #[test]
    fn detects_type_equals_style_cef_arg() {
        assert!(args_indicate_cef_subprocess(["--type=renderer"]));
    }

    #[test]
    fn detects_split_type_cef_arg() {
        assert!(args_indicate_cef_subprocess(["--type", "gpu-process"]));
    }

    #[test]
    fn ignores_missing_type_value() {
        assert!(!args_indicate_cef_subprocess(["--type"]));
    }

    #[test]
    fn ignores_normal_cli_args() {
        assert!(!args_indicate_cef_subprocess(["msg", "list-tabs"]));
        assert!(!args_indicate_cef_subprocess(["--daemon"]));
        assert!(!args_indicate_cef_subprocess(["-e", "echo", "--type=renderer"]));
    }
}
