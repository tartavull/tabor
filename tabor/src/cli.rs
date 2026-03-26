use std::cmp::max;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::rc::Rc;

use clap::{ArgAction, ArgGroup, Args, Parser, Subcommand, ValueHint};
use log::{LevelFilter, error};
use serde::{Deserialize, Serialize};
use tabor_config::SerdeReplace;
use toml::Value;

use tabor_terminal::tty::Options as PtyOptions;
#[cfg(unix)]
use tabor_terminal::vi_mode::ViMotion;

use crate::config::UiConfig;
use crate::config::window::{Class, Identity};
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::window_kind::WindowKind;

/// CLI options for the main Tabor executable.
#[derive(Parser, Default, Debug)]
#[clap(author, about, version = env!("VERSION"))]
pub struct Options {
    /// Print all events to STDOUT.
    #[clap(long)]
    pub print_events: bool,

    /// Generates ref test.
    #[clap(long, conflicts_with("daemon"))]
    pub ref_test: bool,

    /// X11 window ID to embed Tabor within (decimal or hexadecimal with "0x" prefix).
    #[clap(long)]
    pub embed: Option<String>,

    /// Specify alternative configuration file [default:
    /// $XDG_CONFIG_HOME/tabor/tabor.toml].
    #[cfg(not(any(target_os = "macos", windows)))]
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub config_file: Option<PathBuf>,

    /// Specify alternative configuration file [default: %APPDATA%\tabor\tabor.toml].
    #[cfg(windows)]
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub config_file: Option<PathBuf>,

    /// Specify alternative configuration file [default: $HOME/.config/tabor/tabor.toml].
    #[cfg(target_os = "macos")]
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub config_file: Option<PathBuf>,

    /// Path for IPC socket creation.
    #[cfg(unix)]
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub socket: Option<PathBuf>,

    /// Reduces the level of verbosity (the min level is -qq).
    #[clap(short, conflicts_with("verbose"), action = ArgAction::Count)]
    quiet: u8,

    /// Increases the level of verbosity (the max level is -vvv).
    #[clap(short, conflicts_with("quiet"), action = ArgAction::Count)]
    verbose: u8,

    /// Do not spawn an initial window.
    #[clap(long)]
    pub daemon: bool,

    /// CLI options for config overrides.
    #[clap(skip)]
    pub config_options: ParsedOptions,

    /// Options which can be passed via IPC.
    #[clap(flatten)]
    pub window_options: WindowOptions,

    /// Subcommand passed to the CLI.
    #[clap(subcommand)]
    pub subcommands: Option<Subcommands>,
}

impl Options {
    pub fn new() -> Self {
        let mut options = Self::parse();

        // Parse CLI config overrides.
        options.config_options = options.window_options.config_overrides();

        options
    }

    /// Override configuration file with options from the CLI.
    pub fn override_config(&mut self, config: &mut UiConfig) {
        #[cfg(unix)]
        if self.socket.is_some() {
            config.ipc_socket = Some(true);
        }

        config.window.embed = self.embed.as_ref().and_then(|embed| parse_hex_or_decimal(embed));
        config.debug.print_events |= self.print_events;
        config.debug.log_level = max(config.debug.log_level, self.log_level());
        config.debug.ref_test |= self.ref_test;

        if config.debug.print_events {
            config.debug.log_level = max(config.debug.log_level, LevelFilter::Info);
        }

        // Replace CLI options.
        self.config_options.override_config(config);
    }

    /// Logging filter level.
    pub fn log_level(&self) -> LevelFilter {
        match (self.quiet, self.verbose) {
            // Force at least `Info` level for `--print-events`.
            (_, 0) if self.print_events => LevelFilter::Info,

            // Default.
            (0, 0) => LevelFilter::Warn,

            // Verbose.
            (_, 1) => LevelFilter::Info,
            (_, 2) => LevelFilter::Debug,
            (0, _) => LevelFilter::Trace,

            // Quiet.
            (1, _) => LevelFilter::Error,
            (..) => LevelFilter::Off,
        }
    }
}

/// Parse the class CLI parameter.
fn parse_class(input: &str) -> Result<Class, String> {
    let (general, instance) = match input.split_once(',') {
        // Warn the user if they've passed too many values.
        Some((_, instance)) if instance.contains(',') => {
            return Err(String::from("Too many parameters"));
        },
        Some((general, instance)) => (general, instance),
        None => (input, input),
    };

    Ok(Class::new(general, instance))
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabIdArg {
    pub index: u32,
    pub generation: u32,
}

#[cfg(unix)]
fn parse_tab_id(input: &str) -> Result<TabIdArg, String> {
    let (index, generation) = input
        .split_once(':')
        .or_else(|| input.split_once(','))
        .ok_or_else(|| String::from("tab id must be <index>:<generation>"))?;
    let index = index.parse::<u32>().map_err(|_| String::from("tab id index must be a u32"))?;
    let generation =
        generation.parse::<u32>().map_err(|_| String::from("tab id generation must be a u32"))?;
    Ok(TabIdArg { index, generation })
}

#[cfg(unix)]
fn parse_vi_motion(input: &str) -> Result<ViMotion, String> {
    serde_json::from_str(&format!("\"{input}\"")).map_err(|err| err.to_string())
}

/// Convert to hex if possible, else decimal
fn parse_hex_or_decimal(input: &str) -> Option<u32> {
    input
        .strip_prefix("0x")
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .or_else(|| input.parse().ok())
}

#[cfg(not(windows))]
fn shell_escape_arg(input: &str) -> String {
    if input.is_empty() {
        return String::from("''");
    }

    let mut escaped = String::with_capacity(input.len() + 2);
    escaped.push('\'');
    for ch in input.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

#[cfg(windows)]
fn shell_escape_arg(input: &str) -> String {
    if input.is_empty() {
        return String::from("''");
    }

    let mut escaped = String::with_capacity(input.len() + 2);
    escaped.push('\'');
    for ch in input.chars() {
        if ch == '\'' {
            escaped.push_str("''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

/// Terminal specific cli options which can be passed to new windows via IPC.
#[derive(Serialize, Deserialize, Args, Default, Debug, Clone, PartialEq, Eq)]
pub struct TerminalOptions {
    /// Start the shell in the specified working directory.
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub working_directory: Option<PathBuf>,

    /// Remain open after child process exit.
    #[clap(long)]
    pub hold: bool,

    /// Command and args to execute in the default shell (must be last argument).
    #[clap(short = 'e', long, allow_hyphen_values = true, num_args = 1..)]
    command: Vec<String>,
}

impl TerminalOptions {
    #[cfg(not(windows))]
    pub(crate) fn command_input(&self) -> Option<String> {
        let (program, args) = self.command.split_first()?;
        let mut parts = Vec::with_capacity(self.command.len());
        parts.push(shell_escape_arg(program));
        for arg in args {
            parts.push(shell_escape_arg(arg));
        }
        Some(parts.join(" "))
    }

    #[cfg(windows)]
    pub(crate) fn command_input(&self) -> Option<String> {
        let (program, args) = self.command.split_first()?;
        let mut parts = Vec::with_capacity(self.command.len() + 1);
        let needs_call = program.chars().any(|ch| ch.is_whitespace() || ch == '\'');
        if needs_call {
            parts.push(String::from("&"));
            parts.push(shell_escape_arg(program));
        } else {
            parts.push(program.clone());
        }
        for arg in args {
            parts.push(shell_escape_arg(arg));
        }
        Some(parts.join(" "))
    }

    /// Override the [`PtyOptions`]'s fields with the [`TerminalOptions`].
    pub fn override_pty_config(&self, pty_config: &mut PtyOptions) {
        if let Some(working_directory) = &self.working_directory {
            if working_directory.is_dir() {
                pty_config.working_directory = Some(working_directory.to_owned());
            } else {
                error!("Invalid working directory: {working_directory:?}");
            }
        }

        pty_config.drain_on_exit |= self.hold;
    }
}

impl From<TerminalOptions> for PtyOptions {
    fn from(mut options: TerminalOptions) -> Self {
        PtyOptions {
            working_directory: options.working_directory.take(),
            shell: None,
            drain_on_exit: options.hold,
            env: HashMap::new(),
            #[cfg(target_os = "windows")]
            escape_args: false,
        }
    }
}

/// Window specific cli options which can be passed to new windows via IPC.
#[derive(Serialize, Deserialize, Args, Default, Debug, Clone, PartialEq, Eq)]
pub struct WindowIdentity {
    /// Defines the window title [default: Tabor].
    #[clap(short = 'T', short_alias('t'), long)]
    pub title: Option<String>,

    /// Defines window class/app_id on X11/Wayland [default: Tabor].
    #[clap(long, value_name = "general> | <general>,<instance", value_parser = parse_class)]
    pub class: Option<Class>,
}

impl WindowIdentity {
    /// Override the [`WindowIdentity`]'s fields with the [`WindowOptions`].
    pub fn override_identity_config(&self, identity: &mut Identity) {
        if let Some(title) = &self.title {
            identity.title.clone_from(title);
        }
        if let Some(class) = &self.class {
            identity.class.clone_from(class);
        }
    }
}

/// Available CLI subcommands.
#[derive(Subcommand, Debug)]
pub enum Subcommands {
    #[cfg(unix)]
    Msg(MessageOptions),
    #[cfg(unix)]
    Agent(AgentOptions),
    #[cfg(unix)]
    Workspace(WorkspaceOptions),
    Migrate(MigrateOptions),
}

/// Send a message to the Tabor socket.
#[cfg(unix)]
#[derive(Args, Debug)]
pub struct MessageOptions {
    /// IPC socket connection path override.
    #[clap(short, long, value_hint = ValueHint::FilePath)]
    pub socket: Option<PathBuf>,

    /// Message which should be sent.
    #[clap(subcommand)]
    pub message: MessageCommand,
}

/// Stateful agent control for an attached Tabor instance.
#[cfg(unix)]
#[derive(Args, Debug, Clone)]
pub struct AgentOptions {
    /// IPC socket connection path override.
    #[clap(short, long, value_hint = ValueHint::FilePath)]
    pub socket: Option<PathBuf>,

    /// Agent command.
    #[clap(subcommand)]
    pub command: AgentCommand,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone)]
pub struct WorkspaceOptions {
    #[clap(subcommand)]
    pub command: WorkspaceCommand,
}

#[cfg(unix)]
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceCommand {
    Status,
    Stop,
    RestartTerminal {
        #[clap(long)]
        id: u64,
    },
}

#[cfg(unix)]
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum AgentCommand {
    /// Attach to a running Tabor instance and start the local controller.
    Attach,

    /// List tabs and current agent state.
    App,

    /// Select the active tab or a specific tab for automation.
    Use(AgentUse),

    /// Observe the selected web tab.
    Observe,

    /// Inspect one observed element by id.
    Inspect(AgentInspect),

    /// Capture a screenshot from the selected web tab.
    Screenshot(AgentScreenshot),

    /// Read buffered console, network, and page events.
    Events(AgentEvents),

    /// Render the selected web tab as a PDF.
    Pdf(AgentPdf),

    /// Upload local files into a selected file input.
    Upload(AgentUpload),

    /// List tracked downloads for the selected web tab.
    Downloads,

    /// Execute batched actions encoded as JSON.
    Act(AgentAct),

    /// Read or update the system clipboard.
    Clipboard(AgentClipboard),

    /// Stop the local controller for this Tabor socket.
    Close,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[clap(group(
    ArgGroup::new("selection")
        .required(true)
        .args(&["active", "tab_id"])
))]
pub struct AgentUse {
    /// Select the active tab.
    #[clap(long)]
    pub active: bool,

    /// Tab id formatted as <index>:<generation>.
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AgentInspect {
    /// Element id returned by `tabor agent observe`.
    pub element_id: String,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AgentScreenshot {
    /// Write the PNG to this path. A temp file is used by default.
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub path: Option<PathBuf>,

    /// Capture the full page instead of the viewport.
    #[clap(long)]
    pub full_page: bool,

    /// Crop the viewport screenshot to the observed element.
    #[clap(long)]
    pub element_id: Option<String>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AgentEvents {
    /// Return events after this event id.
    #[clap(long)]
    pub since: Option<u64>,

    /// Maximum number of events to return.
    #[clap(long, default_value_t = 200)]
    pub max: usize,

    /// Restrict events to one or more kinds.
    #[clap(long = "kind")]
    pub kinds: Vec<String>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AgentPdf {
    /// Write the PDF to this path. A temp file is used by default.
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub path: Option<PathBuf>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AgentUpload {
    /// Element id returned by `tabor agent observe`.
    pub element_id: String,

    /// One or more local file paths to upload.
    #[clap(required = true, value_hint = ValueHint::FilePath)]
    pub paths: Vec<PathBuf>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AgentAct {
    /// JSON array of agent actions.
    pub actions_json: String,

    /// Include a post-action observation in the reply.
    #[clap(long, default_value_t = true, action = ArgAction::Set)]
    pub observe: bool,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AgentClipboard {
    #[clap(subcommand)]
    pub command: AgentClipboardCommand,
}

#[cfg(unix)]
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum AgentClipboardCommand {
    /// Read the system clipboard.
    Get,

    /// Replace the system clipboard contents.
    Set(AgentClipboardSet),
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AgentClipboardSet {
    pub text: String,
}

/// Available socket messages.
#[cfg(unix)]
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum MessageCommand {
    /// Update the Tabor configuration.
    Config(IpcConfig),

    /// Read runtime Tabor configuration.
    GetConfig(IpcGetConfig),

    /// Ping the IPC socket.
    Ping,

    /// List IPC capabilities.
    GetCapabilities,

    /// List all tabs.
    ListTabs,

    /// Get a single tab state.
    GetTabState(MsgGetTabState),

    /// Create a new tab.
    CreateTab(MsgCreateTab),

    /// Create a new tab group.
    CreateGroup(MsgCreateGroup),

    /// Close a tab (defaults to active).
    CloseTab(MsgCloseTab),

    /// Select a tab.
    SelectTab(MsgSelectTab),

    /// Move a tab within or across groups.
    MoveTab(MsgMoveTab),

    /// Set or clear a tab title.
    SetTabTitle(MsgSetTabTitle),

    /// Set or clear a tab group name.
    SetGroupName(MsgSetGroupName),

    /// Restore the most recently closed tab.
    RestoreClosedTab,

    /// Open a URL in a tab.
    OpenUrl(MsgOpenUrl),

    /// Set the URL for a web tab.
    SetWebUrl(MsgSetWebUrl),

    /// Reload a web tab.
    ReloadWeb(MsgReloadWeb),

    /// Open the Web Inspector for a web tab.
    OpenInspector(MsgOpenInspector),

    /// Get tab panel state.
    GetTabPanel,

    /// Set tab panel state.
    SetTabPanel(MsgSetTabPanel),

    /// Dispatch a configured action.
    DispatchAction(MsgDispatchAction),

    /// Send literal input to a tab.
    SendInput(MsgSendInput),

    /// Run a command in the command bar.
    RunCommandBar(MsgRunCommandBar),

    /// Web Inspector commands.
    Inspector {
        #[clap(subcommand)]
        command: MsgInspector,
    },

    /// Send raw JSON IPC message.
    Send {
        /// JSON payload to send.
        #[clap(value_name = "JSON")]
        json: String,
    },

    /// List available IPC request types.
    ListRequests,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgGetTabState {
    /// Tab id formatted as <index>:<generation>.
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: TabIdArg,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgCreateTab {
    /// Create a web tab with the provided URL.
    #[clap(long, value_name = "URL")]
    pub web: Option<String>,

    /// Target group id for the new tab.
    #[clap(long, value_name = "GROUP_ID", conflicts_with = "group_name")]
    pub group_id: Option<usize>,

    /// Target group name for the new tab.
    #[clap(long, value_name = "NAME", conflicts_with = "group_id")]
    pub group_name: Option<String>,

    #[clap(flatten)]
    pub terminal_options: TerminalOptions,

    #[clap(flatten)]
    pub window_identity: WindowIdentity,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgCreateGroup {
    /// Optional name for the new group.
    #[clap(long, value_name = "NAME")]
    pub name: Option<String>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgCloseTab {
    /// Tab id formatted as <index>:<generation>.
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[clap(group(
    ArgGroup::new("selection")
        .required(true)
        .args(&["active", "next", "previous", "last", "index", "tab_id"])
))]
pub struct MsgSelectTab {
    #[clap(long)]
    pub active: bool,

    #[clap(long)]
    pub next: bool,

    #[clap(long)]
    pub previous: bool,

    #[clap(long)]
    pub last: bool,

    #[clap(long)]
    pub index: Option<usize>,

    /// Tab id formatted as <index>:<generation>.
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[clap(group(
    ArgGroup::new("target")
        .required(true)
        .args(&["target_group_id", "target_index"])
))]
pub struct MsgMoveTab {
    /// Tab id formatted as <index>:<generation>.
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: TabIdArg,

    #[clap(long, value_name = "GROUP_ID")]
    pub target_group_id: Option<usize>,

    #[clap(long, value_name = "INDEX")]
    pub target_index: Option<usize>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[clap(group(
    ArgGroup::new("title_choice")
        .required(true)
        .args(&["title", "clear"])
))]
pub struct MsgSetTabTitle {
    /// Tab id formatted as <index>:<generation>.
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,

    #[clap(long)]
    pub title: Option<String>,

    #[clap(long, conflicts_with = "title")]
    pub clear: bool,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[clap(group(
    ArgGroup::new("group_name_choice")
        .required(true)
        .args(&["name", "clear"])
))]
pub struct MsgSetGroupName {
    #[clap(long, value_name = "GROUP_ID")]
    pub group_id: usize,

    #[clap(long)]
    pub name: Option<String>,

    #[clap(long, conflicts_with = "name")]
    pub clear: bool,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgOpenUrl {
    pub url: String,

    #[clap(long, conflicts_with = "tab_id")]
    pub new_tab: bool,

    /// Target tab id formatted as <index>:<generation>.
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgSetWebUrl {
    pub url: String,

    /// Tab id formatted as <index>:<generation> (defaults to active tab).
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgReloadWeb {
    /// Tab id formatted as <index>:<generation> (defaults to active tab).
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgOpenInspector {
    /// Tab id formatted as <index>:<generation> (defaults to active tab).
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[clap(group(
    ArgGroup::new("panel")
        .required(true)
        .args(&["enable", "disable", "width"])
))]
pub struct MsgSetTabPanel {
    #[clap(long, conflicts_with = "disable")]
    pub enable: bool,

    #[clap(long, conflicts_with = "enable")]
    pub disable: bool,

    #[clap(long)]
    pub width: Option<usize>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[clap(group(
    ArgGroup::new("action_choice")
        .required(true)
        .args(&[
            "action",
            "vi_motion",
            "vi_action",
            "search_action",
            "mouse_action",
            "esc",
            "command",
        ])
))]
pub struct MsgDispatchAction {
    /// Tab id formatted as <index>:<generation> (defaults to active tab).
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,

    #[clap(long)]
    pub action: Option<String>,

    #[clap(long, value_parser = parse_vi_motion)]
    pub vi_motion: Option<ViMotion>,

    #[clap(long)]
    pub vi_action: Option<String>,

    #[clap(long)]
    pub search_action: Option<String>,

    #[clap(long)]
    pub mouse_action: Option<String>,

    #[clap(long)]
    pub esc: Option<String>,

    #[clap(long, num_args = 1..)]
    pub command: Option<Vec<String>>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgSendInput {
    pub text: String,

    /// Tab id formatted as <index>:<generation> (defaults to active tab).
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgRunCommandBar {
    pub input: String,

    /// Tab id formatted as <index>:<generation> (defaults to active tab).
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,
}

#[cfg(unix)]
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum MsgInspector {
    /// List Web Inspector targets.
    ListTargets,

    /// Attach to a Web Inspector target.
    Attach(MsgInspectorAttach),

    /// Detach a Web Inspector session.
    Detach(MsgInspectorDetach),

    /// Send a Web Inspector message.
    Send(MsgInspectorSend),

    /// Poll for Web Inspector messages.
    Poll(MsgInspectorPoll),
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[clap(group(
    ArgGroup::new("inspector_target")
        .required(true)
        .args(&["tab_id", "target_id"])
))]
pub struct MsgInspectorAttach {
    /// Tab id formatted as <index>:<generation>.
    #[clap(long, value_parser = parse_tab_id, value_name = "INDEX:GEN")]
    pub tab_id: Option<TabIdArg>,

    #[clap(long)]
    pub target_id: Option<u64>,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgInspectorDetach {
    #[clap(long)]
    pub session_id: String,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgInspectorSend {
    #[clap(long)]
    pub session_id: String,

    #[clap(long)]
    pub message: String,
}

#[cfg(unix)]
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct MsgInspectorPoll {
    #[clap(long)]
    pub session_id: String,

    #[clap(long)]
    pub max: Option<usize>,
}

/// Migrate the configuration file.
#[derive(Args, Clone, Debug)]
pub struct MigrateOptions {
    /// Path to the configuration file.
    #[clap(short, long, value_hint = ValueHint::FilePath)]
    pub config_file: Option<PathBuf>,

    /// Only output TOML config to STDOUT.
    #[clap(short, long)]
    pub dry_run: bool,

    /// Do not recurse over imports.
    #[clap(short = 'i', long)]
    pub skip_imports: bool,

    /// Do not move renamed fields to their new location.
    #[clap(long)]
    pub skip_renames: bool,

    #[clap(short, long)]
    /// Do not output to STDOUT.
    pub silent: bool,
}

/// Subset of window options that can be passed via IPC.
#[derive(Serialize, Deserialize, Args, Default, Clone, Debug, PartialEq, Eq)]
pub struct WindowOptions {
    /// Terminal options which can be passed via IPC.
    #[clap(flatten)]
    pub terminal_options: TerminalOptions,

    #[clap(skip)]
    #[serde(default)]
    pub window_kind: WindowKind,

    #[clap(flatten)]
    /// Window options which could be passed via IPC.
    pub window_identity: WindowIdentity,

    #[clap(skip)]
    #[serde(default)]
    pub command_input: Option<String>,

    /// When set (internal only), close this tab after the new tab is created successfully.
    ///
    /// This is intentionally not serialized over IPC: it's used to implement "replace tab"
    /// semantics without closing the current tab if tab creation fails.
    #[clap(skip)]
    #[serde(skip)]
    pub close_tab_on_success: Option<crate::tabs::TabId>,

    #[clap(skip)]
    #[cfg(not(any(target_os = "macos", windows)))]
    /// `ActivationToken` that we pass to winit.
    pub activation_token: Option<String>,

    /// Override configuration file options [example: 'cursor.style="Beam"'].
    #[clap(short = 'o', long, num_args = 1..)]
    option: Vec<String>,
}

impl WindowOptions {
    /// Get the parsed set of CLI config overrides.
    pub fn config_overrides(&self) -> ParsedOptions {
        ParsedOptions::from_options(&self.option)
    }
}

/// Parameters to the `config` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcConfig {
    /// Configuration file options [example: 'cursor.style="Beam"'].
    #[clap(required = true, value_name = "CONFIG_OPTIONS")]
    pub options: Vec<String>,

    /// Window ID for the new config.
    ///
    /// Use `-1` to apply this change to all windows.
    #[clap(short, long, allow_hyphen_values = true, env = "TABOR_WINDOW_ID")]
    pub window_id: Option<i128>,

    /// Clear all runtime configuration changes.
    #[clap(short, long, conflicts_with = "options")]
    pub reset: bool,
}

/// Parameters to the `get-config` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcGetConfig {
    /// Window ID for the config request.
    ///
    /// Use `-1` to get the global config.
    #[clap(short, long, allow_hyphen_values = true, env = "TABOR_WINDOW_ID")]
    pub window_id: Option<i128>,
}

/// Parsed CLI config overrides.
#[derive(Debug, Default)]
pub struct ParsedOptions {
    config_options: Vec<(String, Value)>,
}

impl ParsedOptions {
    /// Parse CLI config overrides.
    pub fn from_options(options: &[String]) -> Self {
        let mut config_options = Vec::new();

        for option in options {
            let parsed = match toml::from_str(option) {
                Ok(parsed) => parsed,
                Err(err) => {
                    eprintln!("Ignoring invalid CLI option '{option}': {err}");
                    continue;
                },
            };
            config_options.push((option.clone(), parsed));
        }

        Self { config_options }
    }

    /// Apply CLI config overrides, removing broken ones.
    pub fn override_config(&mut self, config: &mut UiConfig) {
        let mut i = 0;
        while i < self.config_options.len() {
            let (option, parsed) = &self.config_options[i];
            match config.replace(parsed.clone()) {
                Err(err) => {
                    error!(
                        target: LOG_TARGET_IPC_CONFIG,
                        "Unable to override option '{option}': {err}"
                    );
                    self.config_options.swap_remove(i);
                },
                Ok(_) => i += 1,
            }
        }
    }

    /// Apply CLI config overrides to a CoW config.
    pub fn override_config_rc(&mut self, config: Rc<UiConfig>) -> Rc<UiConfig> {
        // Skip clone without write requirement.
        if self.config_options.is_empty() {
            return config;
        }

        // Override cloned config.
        let mut config = (*config).clone();
        self.override_config(&mut config);

        Rc::new(config)
    }
}

impl Deref for ParsedOptions {
    type Target = Vec<(String, Value)>;

    fn deref(&self) -> &Self::Target {
        &self.config_options
    }
}

impl DerefMut for ParsedOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config_options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::fs::File;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::io::Read;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use clap::CommandFactory;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use clap_complete::Shell;
    use toml::Table;

    #[test]
    fn dynamic_title_ignoring_options_by_default() {
        let mut config = UiConfig::default();
        let old_dynamic_title = config.window.dynamic_title;

        Options::default().override_config(&mut config);

        assert_eq!(old_dynamic_title, config.window.dynamic_title);
    }

    #[test]
    fn dynamic_title_not_overridden_by_config() {
        let mut config = UiConfig::default();

        config.window.identity.title = "foo".to_owned();
        Options::default().override_config(&mut config);

        assert!(config.window.dynamic_title);
    }

    #[test]
    fn valid_option_as_value() {
        // Test with a single field.
        let value: Value = toml::from_str("field=true").unwrap();

        let mut table = Table::new();
        table.insert(String::from("field"), Value::Boolean(true));

        assert_eq!(value, Value::Table(table));

        // Test with nested fields
        let value: Value = toml::from_str("parent.field=true").unwrap();

        let mut parent_table = Table::new();
        parent_table.insert(String::from("field"), Value::Boolean(true));
        let mut table = Table::new();
        table.insert(String::from("parent"), Value::Table(parent_table));

        assert_eq!(value, Value::Table(table));
    }

    #[test]
    fn invalid_option_as_value() {
        let value = toml::from_str::<Value>("}");
        assert!(value.is_err());
    }

    #[test]
    fn float_option_as_value() {
        let value: Value = toml::from_str("float=3.4").unwrap();

        let mut expected = Table::new();
        expected.insert(String::from("float"), Value::Float(3.4));

        assert_eq!(value, Value::Table(expected));
    }

    #[test]
    fn parse_instance_class() {
        let class = parse_class("one").unwrap();
        assert_eq!(class.general, "one");
        assert_eq!(class.instance, "one");
    }

    #[test]
    fn parse_general_class() {
        let class = parse_class("one,two").unwrap();
        assert_eq!(class.general, "one");
        assert_eq!(class.instance, "two");
    }

    #[test]
    fn parse_invalid_class() {
        let class = parse_class("one,two,three");
        assert!(class.is_err());
    }

    #[test]
    fn valid_decimal() {
        let value = parse_hex_or_decimal("10485773");
        assert_eq!(value, Some(10485773));
    }

    #[test]
    fn valid_hex_to_decimal() {
        let value = parse_hex_or_decimal("0xa0000d");
        assert_eq!(value, Some(10485773));
    }

    #[test]
    fn invalid_hex_to_decimal() {
        let value = parse_hex_or_decimal("0xa0xx0d");
        assert_eq!(value, None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn completions() {
        let mut clap = Options::command();

        for (shell, file) in
            &[(Shell::Bash, "tabor.bash"), (Shell::Fish, "tabor.fish"), (Shell::Zsh, "_tabor")]
        {
            if std::env::var("TABOR_GEN_COMPLETIONS").is_ok() {
                let mut file = File::create(format!("../extra/completions/{file}")).unwrap();
                clap_complete::generate(*shell, &mut clap, "tabor", &mut file);
                continue;
            }

            let mut generated = Vec::new();
            clap_complete::generate(*shell, &mut clap, "tabor", &mut generated);
            let generated = String::from_utf8_lossy(&generated);

            let mut completion = String::new();
            let mut file = File::open(format!("../extra/completions/{file}")).unwrap();
            file.read_to_string(&mut completion).unwrap();

            assert_eq!(generated, completion);
        }

        if std::env::var("TABOR_GEN_COMPLETIONS").is_ok() {}
    }
}
