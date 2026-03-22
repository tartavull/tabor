#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;
use url::Url;

const START_TIMEOUT: Duration = Duration::from_secs(12);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

enum HarnessLaunchMode {
    BackgroundBinary,
    ForegroundAppBundle { _debug_notch_ears: bool },
}

struct TempAppBundle {
    bundle_dir: PathBuf,
    executable: PathBuf,
}

const CEF_HELPER_NAMES: [&str; 5] = [
    "Tabor Helper",
    "Tabor Helper (Renderer)",
    "Tabor Helper (GPU)",
    "Tabor Helper (Plugin)",
    "Tabor Helper (Alerts)",
];

struct TaborHarness {
    client_bin: PathBuf,
    bundle_dir: PathBuf,
    socket: PathBuf,
    _tmp: TempDir,
    log_path: PathBuf,
    child: Child,
    kill_path: Option<PathBuf>,
}

impl TaborHarness {
    fn start() -> Self {
        Self::start_with_mode_and_env(HarnessLaunchMode::BackgroundBinary, &[])
    }

    fn start_fake_media() -> Self {
        Self::start_with_mode_and_env(
            HarnessLaunchMode::BackgroundBinary,
            &[("TABOR_CEF_FAKE_MEDIA", "1"), ("TABOR_CEF_LOG_PATH", "1")],
        )
    }

    fn start_foreground_app_bundle(debug_notch_ears: bool) -> Self {
        Self::start_with_mode_and_env(
            HarnessLaunchMode::ForegroundAppBundle { _debug_notch_ears: debug_notch_ears },
            &[],
        )
    }

    fn start_with_mode_and_env(mode: HarnessLaunchMode, extra_env: &[(&str, &str)]) -> Self {
        let built_bin = PathBuf::from(env!("CARGO_BIN_EXE_tabor"));
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let bundle = create_temp_app_bundle(&tmp, &built_bin);
        let client_bin = bundle.executable.clone();
        let socket = tmp.path().join("tabor.sock");
        let log_path = tmp.path().join("tabor.log");

        let stdout = File::create(&log_path).expect("failed to create harness log file");
        let stderr = stdout.try_clone().expect("failed to clone harness log file");

        let kill_path = Some(bundle.executable.clone());
        let mut command = match mode {
            HarnessLaunchMode::BackgroundBinary => Command::new(&bundle.executable),
            HarnessLaunchMode::ForegroundAppBundle { .. } => {
                let mut command = Command::new("open");
                command.arg("-n").arg(&bundle.bundle_dir).arg("--args");
                command
            },
        };
        command
            .arg("--socket")
            .arg(&socket)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        for (key, value) in extra_env {
            command.env(key, value);
        }

        match mode {
            HarnessLaunchMode::BackgroundBinary => {
                command.env("TABOR_BACKGROUND", "1");
                command.env("TABOR_WEBVIEW_ENGINE", "cef");
                command.env("RUST_BACKTRACE", "1");
            },
            HarnessLaunchMode::ForegroundAppBundle { _debug_notch_ears: _ } => (),
        }

        let child = command.spawn().expect("failed to spawn tabor");

        let harness = Self {
            client_bin,
            bundle_dir: bundle.bundle_dir,
            socket,
            _tmp: tmp,
            log_path: log_path.clone(),
            child,
            kill_path,
        };

        let start = Instant::now();
        while start.elapsed() < START_TIMEOUT {
            if harness.socket.exists() && harness.run_checked(["msg", "ping"]).is_ok() {
                if matches!(mode, HarnessLaunchMode::ForegroundAppBundle { .. }) {
                    harness.activate_bundle();
                }
                return harness;
            }
            thread::sleep(POLL_INTERVAL);
        }

        let log = std::fs::read_to_string(log_path).unwrap_or_else(|_| String::new());
        panic!("failed to start tabor harness; log:\n{log}");
    }

    fn run_output<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        Command::new(&self.client_bin)
            .env("TABOR_SOCKET", &self.socket)
            .args(args)
            .output()
            .expect("failed to run tabor command")
    }

    fn run_checked<I, S>(&self, args: I) -> Result<String, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = self.run_output(args);
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    fn run_ok<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.run_checked(args).unwrap_or_else(|stderr| panic!("tabor command failed: {stderr}"))
    }

    fn run_json<I, S>(&self, args: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = self.run_output(args);
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if !output.status.success() {
            panic!(
                "tabor command failed while expecting json (status={:?}): stderr={stderr}; stdout={stdout}; harness_log_tail:\n{}",
                output.status.code(),
                self.log_tail()
            );
        }

        if stdout.is_empty() {
            let child_alive = self.child_alive();
            let child_sample =
                if child_alive { self.sample_child() } else { String::from("<child not alive>") };

            panic!(
                "empty json output from tabor: stderr={stderr}; child_alive={child_alive}; child_sample:\n{child_sample}\nharness_log_tail:\n{}",
                self.log_tail()
            );
        }

        serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!(
            "invalid json output from tabor: {stdout}; parse_error={err}; stderr={stderr}; harness_log_tail:\n{}",
            self.log_tail()
        )
    })
    }

    fn log_tail(&self) -> String {
        const LOG_LINES: usize = 120;

        match std::fs::read_to_string(&self.log_path) {
            Ok(content) => {
                let mut lines: Vec<&str> = content.lines().rev().take(LOG_LINES).collect();
                lines.reverse();
                if lines.is_empty() { String::from("<empty>") } else { lines.join("\n") }
            },
            Err(err) => format!("<unavailable: {err}>"),
        }
    }

    fn child_alive(&self) -> bool {
        Command::new("kill")
            .arg("-0")
            .arg(self.pid().to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn sample_child(&self) -> String {
        match Command::new("sample").arg(self.pid().to_string()).arg("1").arg("1").output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.trim().is_empty() {
                    stdout.lines().take(160).collect::<Vec<_>>().join("\n")
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    if stderr.is_empty() { String::from("<empty sample output>") } else { stderr }
                }
            },
            Err(err) => format!("<sample unavailable: {err}>"),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn tmp_path(&self, name: &str) -> PathBuf {
        self._tmp.path().join(name)
    }

    fn open_file_with_app_bundle(&self, path: &Path) {
        let output =
            Command::new("open").arg("-a").arg(&self.bundle_dir).arg(path).output().unwrap_or_else(
                |err| {
                    panic!(
                        "failed to open {} with {}: {err}",
                        path.display(),
                        self.bundle_dir.display()
                    )
                },
            );
        assert!(
            output.status.success(),
            "open -a {} {} failed: {}",
            self.bundle_dir.display(),
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn activate_bundle(&self) {
        let output =
            Command::new("open").arg("-a").arg(&self.bundle_dir).output().unwrap_or_else(|err| {
                panic!("failed to activate app bundle {}: {err}", self.bundle_dir.display())
            });
        assert!(
            output.status.success(),
            "failed to activate bundle {}: {}",
            self.bundle_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn send_raw_request(&self, request: Value) -> Value {
        let request = request.to_string();
        self.run_json(["msg", "send", request.as_str()])
    }
}

impl Drop for TaborHarness {
    fn drop(&mut self) {
        if let Some(kill_path) = &self.kill_path {
            let _ = Command::new("pkill").arg("-f").arg(kill_path).status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn create_temp_app_bundle(tmp: &TempDir, bin: &Path) -> TempAppBundle {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let bundle_dir = tmp.path().join("Tabor.app");
    let contents = bundle_dir.join("Contents");
    let macos = contents.join("MacOS");
    std::fs::create_dir_all(&macos).expect("failed to create temp app bundle");

    let info_plist_src = repo_root.join("extra/osx/Tabor.Info.plist");
    let info_plist_dst = contents.join("Info.plist");
    std::fs::copy(&info_plist_src, &info_plist_dst).unwrap_or_else(|err| {
        panic!(
            "failed to copy Info.plist from {} to {}: {err}",
            info_plist_src.display(),
            info_plist_dst.display()
        )
    });
    let bundle_id = format!("com.pinkbot.tabor.test.{}", sanitized_bundle_id_suffix(tmp.path()));
    let plist_status = Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(format!("Set :CFBundleIdentifier {bundle_id}"))
        .arg(&info_plist_dst)
        .status()
        .unwrap_or_else(|err| {
            panic!(
                "failed to update temp app bundle identifier in {}: {err}",
                info_plist_dst.display()
            )
        });
    assert!(
        plist_status.success(),
        "failed to set temp app bundle identifier in {}",
        info_plist_dst.display()
    );

    let bundled_bin = macos.join("tabor");
    std::fs::copy(bin, &bundled_bin).unwrap_or_else(|err| {
        panic!(
            "failed to copy test binary from {} to {}: {err}",
            bin.display(),
            bundled_bin.display()
        )
    });
    let mut permissions = std::fs::metadata(&bundled_bin)
        .unwrap_or_else(|err| {
            panic!("failed to stat copied test binary {}: {err}", bundled_bin.display())
        })
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&bundled_bin, permissions).unwrap_or_else(|err| {
        panic!("failed to mark copied test binary executable at {}: {err}", bundled_bin.display())
    });

    for script in ["scripts/bundle-macos-deps.sh", "scripts/create-macos-cef-helpers.sh"] {
        let status = Command::new(repo_root.join(script))
            .current_dir(&repo_root)
            .arg(&bundle_dir)
            .status()
            .unwrap_or_else(|err| {
                panic!("failed to run {script} for {}: {err}", bundle_dir.display())
            });
        assert!(
            status.success(),
            "{script} failed for {} with status {status}",
            bundle_dir.display()
        );
    }

    let sign_status = Command::new(repo_root.join("scripts/sign-macos-app.sh"))
        .current_dir(&repo_root)
        .arg(&bundle_dir)
        .status()
        .unwrap_or_else(|err| {
            panic!("failed to run scripts/sign-macos-app.sh for {}: {err}", bundle_dir.display())
        });
    assert!(
        sign_status.success(),
        "scripts/sign-macos-app.sh failed for {} with status {sign_status}",
        bundle_dir.display()
    );

    let verify_status = Command::new("codesign")
        .arg("--verify")
        .arg("--deep")
        .arg("--strict")
        .arg(&bundle_dir)
        .status()
        .unwrap_or_else(|err| {
            panic!("failed to verify temp app bundle {}: {err}", bundle_dir.display())
        });
    assert!(
        verify_status.success(),
        "codesign verification failed for {} with status {verify_status}",
        bundle_dir.display()
    );

    let describe_status =
        Command::new("codesign").arg("-dvv").arg(&bundle_dir).status().unwrap_or_else(|err| {
            panic!("failed to inspect temp app bundle {}: {err}", bundle_dir.display())
        });
    assert!(
        describe_status.success(),
        "codesign inspection failed for {} with status {describe_status}",
        bundle_dir.display()
    );

    TempAppBundle { bundle_dir, executable: bundled_bin }
}

fn sanitized_bundle_id_suffix(path: &Path) -> String {
    let raw = path.file_name().and_then(|name| name.to_str()).unwrap_or("bundle");
    let sanitized =
        raw.chars().map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' }).collect::<String>();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() { String::from("bundle") } else { sanitized.to_string() }
}

fn helper_info_plist(bundle_dir: &Path, helper_name: &str) -> PathBuf {
    bundle_dir
        .join("Contents")
        .join("Frameworks")
        .join(format!("{helper_name}.app"))
        .join("Contents")
        .join("Info.plist")
}

fn plist_string(path: &Path, key_path: &str) -> Option<String> {
    let output = Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(format!("Print :{key_path}"))
        .arg(path)
        .output()
        .unwrap_or_else(|err| {
            panic!("failed to read plist key {key_path} from {}: {err}", path.display())
        });
    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

struct PopupServer {
    port: u16,
    hits: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PopupServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind popup server");
        listener.set_nonblocking(true).expect("failed to set popup server nonblocking mode");

        let port = listener.local_addr().expect("failed to read popup server addr").port();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_hits = Arc::clone(&hits);
        let thread_stop = Arc::clone(&stop);
        let opener = opener_html().to_string();
        let icon = popup_icon();

        let handle = thread::spawn(move || {
            loop {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }

                match listener.accept() {
                    Ok((mut stream, _)) => {
                        handle_popup_connection(&mut stream, &thread_hits, &opener, &icon);
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    },
                    Err(_) => break,
                }
            }
        });

        Self { port, hits, stop, handle: Some(handle) }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn saw_path(&self, needle: &str) -> bool {
        self.hits.lock().expect("failed to lock hit list").iter().any(|path| path.contains(needle))
    }
}

impl Drop for PopupServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct ClipboardFixtureServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ClipboardFixtureServer {
    fn start() -> Self {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind clipboard fixture server");
        listener
            .set_nonblocking(true)
            .expect("failed to set clipboard fixture server nonblocking mode");

        let port =
            listener.local_addr().expect("failed to read clipboard fixture server addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let fixture_html = include_str!("fixtures/agent-fixture.html").to_string();

        let handle = thread::spawn(move || {
            loop {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }

                match listener.accept() {
                    Ok((mut stream, _)) => {
                        handle_clipboard_fixture_connection(&mut stream, fixture_html.as_bytes());
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    },
                    Err(_) => break,
                }
            }
        });

        Self { port, stop, handle: Some(handle) }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for ClipboardFixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct MediaFixtureServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MediaFixtureServer {
    fn start() -> Self {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind media fixture server");
        listener
            .set_nonblocking(true)
            .expect("failed to set media fixture server nonblocking mode");

        let port = listener.local_addr().expect("failed to read media fixture server addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let fixture_html = include_str!("fixtures/media-fixture.html").to_string();

        let handle = thread::spawn(move || {
            loop {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }

                match listener.accept() {
                    Ok((mut stream, _)) => {
                        handle_media_fixture_connection(&mut stream, fixture_html.as_bytes());
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    },
                    Err(_) => break,
                }
            }
        });

        Self { port, stop, handle: Some(handle) }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for MediaFixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WindowDebugRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WindowDebugInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WindowDebugState {
    native_fullscreen: bool,
    simple_fullscreen: bool,
    winit_fullscreen: bool,
    #[serde(default)]
    real_ear_fullscreen_active: bool,
    is_miniaturized: bool,
    notch_ears_active: bool,
    scale_factor: f64,
    #[serde(default)]
    is_key_window: bool,
    #[serde(default)]
    first_responder_class: Option<String>,
    #[serde(default)]
    content_view_class: Option<String>,
    window_number: Option<i64>,
    left_ear_window_number: Option<i64>,
    right_ear_window_number: Option<i64>,
    screen_frame_points: WindowDebugRect,
    content_frame_screen_points: WindowDebugRect,
    safe_area_insets_points: WindowDebugInsets,
    auxiliary_top_left_screen_points: WindowDebugRect,
    auxiliary_top_right_screen_points: WindowDebugRect,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WindowDebugSnapshot {
    png_base64: String,
    width: u32,
    height: u32,
    snapshot_screen_points: WindowDebugRect,
    state: WindowDebugState,
}

#[test]
fn temp_app_bundle_helpers_inherit_browser_usage_strings() {
    let built_bin = PathBuf::from(env!("CARGO_BIN_EXE_tabor"));
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let bundle = create_temp_app_bundle(&tmp, &built_bin);
    let main_info = bundle.bundle_dir.join("Contents").join("Info.plist");

    let expected_camera = plist_string(&main_info, "NSCameraUsageDescription")
        .unwrap_or_else(|| panic!("missing NSCameraUsageDescription in {}", main_info.display()));
    let expected_microphone = plist_string(&main_info, "NSMicrophoneUsageDescription")
        .unwrap_or_else(|| {
            panic!("missing NSMicrophoneUsageDescription in {}", main_info.display())
        });
    let expected_distribution = plist_string(&main_info, "TABORDistributionChannel")
        .unwrap_or_else(|| panic!("missing TABORDistributionChannel in {}", main_info.display()));
    let expected_passkey_usage = plist_string(
        &main_info,
        "NSWebBrowserPublicKeyCredentialUsageDescription",
    )
    .unwrap_or_else(|| {
        panic!("missing NSWebBrowserPublicKeyCredentialUsageDescription in {}", main_info.display())
    });

    for helper_name in CEF_HELPER_NAMES {
        let helper_info = helper_info_plist(&bundle.bundle_dir, helper_name);
        assert_eq!(
            plist_string(&helper_info, "NSCameraUsageDescription").as_deref(),
            Some(expected_camera.as_str()),
            "helper camera usage mismatch for {}",
            helper_info.display()
        );
        assert_eq!(
            plist_string(&helper_info, "NSMicrophoneUsageDescription").as_deref(),
            Some(expected_microphone.as_str()),
            "helper microphone usage mismatch for {}",
            helper_info.display()
        );
        assert_eq!(
            plist_string(&helper_info, "TABORDistributionChannel").as_deref(),
            Some(expected_distribution.as_str()),
            "helper distribution marker mismatch for {}",
            helper_info.display()
        );
        assert_eq!(
            plist_string(&helper_info, "NSWebBrowserPublicKeyCredentialUsageDescription")
                .as_deref(),
            Some(expected_passkey_usage.as_str()),
            "helper passkey usage mismatch for {}",
            helper_info.display()
        );
        assert_eq!(
            plist_string(&helper_info, "NSSupportsAutomaticGraphicsSwitching").as_deref(),
            Some("true"),
            "helper graphics switching flag mismatch for {}",
            helper_info.display()
        );
        assert_eq!(
            plist_string(&helper_info, "LSEnvironment:MallocNanoZone").as_deref(),
            Some("0"),
            "helper MallocNanoZone mismatch for {}",
            helper_info.display()
        );
    }
}

#[test]
fn fake_media_webrtc_probe_succeeds_in_signed_bundle() {
    let server = MediaFixtureServer::start();
    let harness = TaborHarness::start_fake_media();
    let fixture = server.url("/fixture.html");

    let initial_metrics = runtime_metrics(&harness);
    let (baseline_live, baseline_created, baseline_dropped) = webview_counts(&initial_metrics);
    let baseline_startup_failures =
        webview_metric(&initial_metrics, "accelerated_startup_failures");
    let baseline_cpu_paints = webview_metric(&initial_metrics, "unexpected_cpu_paints");

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    let tab_id_arg = tab_id_arg(&reply);
    let settled_layout = wait_for_tab_acceleration_settled(
        &harness,
        tab_id_arg.as_str(),
        Duration::from_secs(8),
    )
    .unwrap_or_else(|| {
        let latest_state = harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
        panic!(
            "timed out waiting for fake media tab acceleration to settle; last_state={latest_state}; harness_log_tail:\n{}",
            harness.log_tail(),
        )
    });
    assert_eq!(
        browser_layout_acceleration_state(&settled_layout),
        Some("ready"),
        "fake media tab acceleration failed: {settled_layout}"
    );

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);
    wait_for_agent_observation(&harness, "Media Fixture", Duration::from_secs(6))
        .unwrap_or_else(|| panic!("timed out waiting for media fixture title"));

    let inspector_session = attach_inspector(&harness, tab_id_arg.as_str());
    let mut inspector_command_id = 1_i64;
    let probe = inspector_eval_json(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        "(async () => JSON.stringify(await window.__taborMediaProbe.run()))()",
    );

    assert_eq!(probe.get("secureContext").and_then(Value::as_bool), Some(true));
    assert_eq!(probe.get("enumerateOk").and_then(Value::as_bool), Some(true));
    assert_eq!(probe.get("gumOk").and_then(Value::as_bool), Some(true));

    let device_kinds = probe
        .get("deviceKinds")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing deviceKinds in probe result: {probe}"))
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        device_kinds.contains(&"audioinput"),
        "expected audioinput device in fake media probe: {probe}"
    );
    assert!(
        device_kinds.contains(&"videoinput"),
        "expected videoinput device in fake media probe: {probe}"
    );

    let track_kinds = probe
        .get("trackKinds")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing trackKinds in probe result: {probe}"))
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(track_kinds.contains(&"audio"), "expected audio track in fake media probe: {probe}");
    assert!(track_kinds.contains(&"video"), "expected video track in fake media probe: {probe}");

    let settled_metrics = runtime_metrics(&harness);
    assert_eq!(
        webview_metric(&settled_metrics, "accelerated_startup_failures"),
        baseline_startup_failures,
        "accelerated startup failures changed during fake media probe: {settled_metrics}"
    );
    assert_eq!(
        webview_metric(&settled_metrics, "unexpected_cpu_paints"),
        baseline_cpu_paints,
        "unexpected CPU paints changed during fake media probe: {settled_metrics}"
    );
    assert!(
        webview_metric(&settled_metrics, "live_accelerated_surfaces") >= 1,
        "expected at least one live accelerated surface during fake media probe: {settled_metrics}"
    );

    let _ = inspector_eval_json(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        "Promise.resolve().then(() => { window.__taborMediaProbe.stop(); return JSON.stringify({\"stopped\":true}); })",
    );
    harness.run_ok(["msg", "close-tab"]);

    let expected = (baseline_live, baseline_created + 1, baseline_dropped + 1);
    wait_for_webview_counts(&harness, expected, Duration::from_secs(4)).unwrap_or_else(|| {
        panic!("timed out waiting for fake media webview teardown: {expected:?}")
    });
}

#[test]
fn agent_fixture_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);

    let observation =
        wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
            .unwrap_or_else(|| panic!("timed out waiting for initial agent observation"));
    let email_id = find_observed_element_id(&observation, "Email");
    let notes_id = find_observed_element_id(&observation, "Notes");
    let checkbox_id = find_observed_element_id(&observation, "check-me");

    let actions = json!([
        { "type": "fill", "id": email_id, "text": "test@example.com" },
        { "type": "fill", "id": notes_id, "text": "hello" },
        { "type": "click", "id": checkbox_id }
    ])
    .to_string();
    let act = harness.run_json(["agent", "act", actions.as_str()]);
    assert_eq!(act.get("type").and_then(Value::as_str), Some("act"));
    assert!(agent_action_results_all_ok(&act), "agent act failed: {act}");

    let email = harness.run_json(["agent", "inspect", email_id.as_str()]);
    assert_eq!(agent_detail_value(&email), Some("test@example.com"));

    let notes = harness.run_json(["agent", "inspect", notes_id.as_str()]);
    assert_eq!(agent_detail_value(&notes), Some("hello"));

    let checkbox = harness.run_json(["agent", "inspect", checkbox_id.as_str()]);
    assert_eq!(agent_detail_checked(&checkbox), Some(true));

    let second = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(second.get("type").and_then(Value::as_str), Some("tab_created"));

    let app = harness.run_json(["agent", "app"]);
    assert!(flatten_tabs(&app).len() >= 3, "expected agent app to list all tabs: {app}");
}

#[test]
fn web_popup_smoke() {
    let server = PopupServer::start();
    let harness = TaborHarness::start();

    let opener_url = server.url("/opener.html");
    let reply = harness.run_json(["msg", "create-tab", "--web", opener_url.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));
    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);

    let success_titles = ["popup-sent", "popup-ok"];
    let failure_titles = ["popup-no-opener", "popup-error", "popup-blocked", "popup-timeout"];

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_success = false;
    let mut click_attempts = 0usize;
    let mut last_click: Option<Instant> = None;

    while Instant::now() < deadline {
        let tabs = harness.run_json(["msg", "list-tabs"]);
        let titles = tab_titles(&tabs);

        if titles.iter().any(|title| success_titles.contains(&title.as_str())) {
            saw_success = true;
            break;
        }

        let ready = titles.iter().any(|title| title == "popup-ready");
        let should_click = match last_click {
            Some(when) => when.elapsed() >= Duration::from_millis(300),
            None => true,
        };
        if ready && should_click {
            click_popup_opener(&harness);
            click_attempts += 1;
            last_click = Some(Instant::now());
            thread::sleep(POLL_INTERVAL);
            continue;
        }

        if let Some(failure) =
            titles.iter().find(|title| failure_titles.contains(&title.as_str())).map(String::as_str)
        {
            panic!("popup smoke failed with title: {failure}");
        }

        thread::sleep(POLL_INTERVAL);
    }

    assert!(click_attempts > 0, "popup smoke never reached opener-ready state");
    assert!(saw_success, "popup smoke timed out waiting for popup success title");

    let favicon_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < favicon_deadline {
        if server.saw_path("/popup-icon.png") {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }

    panic!("popup smoke failed: favicon request missing");
}

#[test]
fn accelerated_web_multi_column_stays_gpu_backed_after_resize_and_scroll() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    harness.run_ok(["msg", "config", "browser.multi_column.target_width_px=150"]);

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    let ready_layout =
        wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
            layout.get("acceleration").and_then(|value| value.get("state")).and_then(Value::as_str)
                == Some("ready")
                && layout
                    .get("acceleration")
                    .and_then(|value| value.get("frame_delivery_mode"))
                    .and_then(Value::as_str)
                    == Some("cef_internal")
                && layout
                    .get("acceleration")
                    .and_then(|value| value.get("main_surface_width"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0
                && layout
                    .get("acceleration")
                    .and_then(|value| value.get("main_surface_height"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0
        })
        .unwrap_or_else(|| panic!("timed out waiting for accelerated browser layout"));
    assert_eq!(ready_layout.get("mode").and_then(Value::as_str), Some("normal"));

    let baseline = runtime_metrics(&harness);
    let baseline_accelerated_frames = webview_metric(&baseline, "accelerated_frames");
    let baseline_external_begin_frames = webview_metric(&baseline, "external_begin_frames");
    let baseline_unexpected_cpu_paints = webview_metric(&baseline, "unexpected_cpu_paints");
    assert_eq!(
        webview_frame_delivery_mode(&baseline),
        "cef_internal",
        "unexpected webview frame delivery mode: {baseline}"
    );
    assert_eq!(
        baseline_external_begin_frames, 0,
        "external begin frames should be unused in cef_internal mode: {baseline}"
    );

    let toggle_reply =
        harness.run_json(["msg", "dispatch-action", "--action", "ToggleMultiColumnTerminal"]);
    assert_eq!(toggle_reply.get("type").and_then(Value::as_str), Some("ok"));
    let _ = harness.run_ok(["msg", "set-tab-panel", "--width", "480"]);

    let multi_column_layout =
        wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
            layout.get("mode").and_then(Value::as_str) == Some("multi_column")
                && layout.get("column_count").and_then(Value::as_u64).unwrap_or(0) >= 2
                && layout
                    .get("acceleration")
                    .and_then(|value| value.get("state"))
                    .and_then(Value::as_str)
                    == Some("ready")
                && layout
                    .get("acceleration")
                    .and_then(|value| value.get("frame_delivery_mode"))
                    .and_then(Value::as_str)
                    == Some("cef_internal")
        })
        .unwrap_or_else(|| panic!("timed out waiting for multi-column accelerated browser layout"));
    assert!(
        multi_column_layout.get("column_count").and_then(Value::as_u64).unwrap_or(0) >= 2,
        "expected at least two browser columns after toggle: {multi_column_layout}"
    );

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);
    wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
        .unwrap_or_else(|| panic!("timed out waiting for browser observation before scroll"));

    let scroll = json!([{ "type": "scroll", "dy": 960 }]).to_string();
    let scroll_reply = harness.run_json(["agent", "act", scroll.as_str()]);
    assert!(agent_action_results_all_ok(&scroll_reply), "agent scroll failed: {scroll_reply}");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut settled_metrics = None;
    while Instant::now() < deadline {
        let metrics = runtime_metrics(&harness);
        let accelerated_frames = webview_metric(&metrics, "accelerated_frames");
        let external_begin_frames = webview_metric(&metrics, "external_begin_frames");
        let unexpected_cpu_paints = webview_metric(&metrics, "unexpected_cpu_paints");
        let live_accelerated_surfaces = webview_metric(&metrics, "live_accelerated_surfaces");

        if accelerated_frames > baseline_accelerated_frames
            && external_begin_frames == baseline_external_begin_frames
            && unexpected_cpu_paints == baseline_unexpected_cpu_paints
            && live_accelerated_surfaces >= 1
            && webview_frame_delivery_mode(&metrics) == "cef_internal"
        {
            settled_metrics = Some(metrics);
            break;
        }

        thread::sleep(POLL_INTERVAL);
    }

    let final_metrics = settled_metrics.unwrap_or_else(|| {
        let metrics = runtime_metrics(&harness);
        panic!("accelerated browser metrics did not advance after resize+scroll: {metrics}");
    });
    assert_eq!(
        webview_metric(&final_metrics, "unexpected_cpu_paints"),
        baseline_unexpected_cpu_paints,
        "unexpected CPU paint callbacks were observed: {final_metrics}"
    );
    assert_eq!(
        webview_metric(&final_metrics, "external_begin_frames"),
        baseline_external_begin_frames,
        "external begin frames advanced unexpectedly: {final_metrics}"
    );
    assert_eq!(
        webview_frame_delivery_mode(&final_metrics),
        "cef_internal",
        "unexpected final webview frame delivery mode: {final_metrics}"
    );

    let final_layout =
        wait_for_active_browser_layout_where(&harness, Duration::from_secs(4), |layout| {
            layout.get("mode").and_then(Value::as_str) == Some("multi_column")
                && layout
                    .get("acceleration")
                    .and_then(|value| value.get("state"))
                    .and_then(Value::as_str)
                    == Some("ready")
        })
        .unwrap_or_else(|| panic!("timed out waiting for final accelerated multi-column layout"));
    assert!(
        final_layout
            .get("acceleration")
            .and_then(|value| value.get("main_surface_width"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
            && final_layout
                .get("acceleration")
                .and_then(|value| value.get("main_surface_height"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0,
        "accelerated surface dimensions disappeared after resize+scroll: {final_layout}"
    );
}

#[test]
fn native_click_on_editable_switches_web_tab_into_insert_mode() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));
    let tab_id_arg = tab_id_arg(&reply);
    ensure_active_web_tab(&harness, 4);
    let layout = wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
        layout.get("mode").and_then(Value::as_str) == Some("normal")
            && layout
                .get("acceleration")
                .and_then(|value| value.get("state"))
                .and_then(Value::as_str)
                == Some("ready")
    })
    .unwrap_or_else(|| panic!("timed out waiting for initial browser layout"));

    let initial_state = harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
    assert_eq!(
        tab_web_mode(&initial_state),
        Some("normal"),
        "expected web tab to start in normal mode: {initial_state}"
    );

    let inspector_session = attach_inspector(&harness, tab_id_arg.as_str());
    let mut inspector_command_id = 1_i64;
    let scrolled_email = inspector_eval_string(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => {
            const el = document.getElementById("email-input");
            if (!el) throw new Error("email-input missing");
            el.scrollIntoView({ block: "center", inline: "center" });
            return "ok";
        })()"#,
    );
    assert_eq!(scrolled_email, "ok", "failed to scroll email input into view");
    thread::sleep(Duration::from_millis(100));
    let (logical_x, logical_y) = inspector_eval_point(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => {
            const el = document.getElementById("email-input");
            if (!el) throw new Error("email-input missing");
            const rect = el.getBoundingClientRect();
            return JSON.stringify({
                x: Math.round(rect.left + rect.width / 2),
                y: Math.round(rect.top + rect.height / 2)
            });
        })()"#,
    );
    let (visual_x, visual_y) = browser_visual_point(&layout, logical_x, logical_y);

    native_window_click(&harness, visual_x, visual_y);

    let active_id = inspector_eval_string(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => document.activeElement ? document.activeElement.id || "" : "")()"#,
    );
    assert_eq!(active_id, "email-input", "native click focused the wrong element");

    let mode_state =
        wait_for_tab_web_mode_value(&harness, tab_id_arg.as_str(), "insert", Duration::from_secs(4))
            .unwrap_or_else(|| {
                let latest_state =
                    harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
                panic!(
                    "timed out waiting for insert mode after clicking an editable element; last_state={latest_state}; harness_log_tail:\n{}",
                    harness.log_tail(),
                )
            });
    assert_eq!(
        tab_web_mode(&mode_state),
        Some("insert"),
        "expected native click on editable element to switch into insert mode: {mode_state}"
    );

    let scrolled_button = inspector_eval_string(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => {
            const el = document.getElementById("frame-launch");
            if (!el) throw new Error("frame-launch missing");
            el.scrollIntoView({ block: "center", inline: "center" });
            return "ok";
        })()"#,
    );
    assert_eq!(scrolled_button, "ok", "failed to scroll frame-launch into view");
    thread::sleep(Duration::from_millis(100));
    let (button_x, button_y) = inspector_eval_point(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => {
            const el = document.getElementById("frame-launch");
            if (!el) throw new Error("frame-launch missing");
            const rect = el.getBoundingClientRect();
            return JSON.stringify({
                x: Math.round(rect.left + rect.width / 2),
                y: Math.round(rect.top + rect.height / 2)
            });
        })()"#,
    );
    let (button_visual_x, button_visual_y) = browser_visual_point(&layout, button_x, button_y);
    native_window_click(&harness, button_visual_x, button_visual_y);

    let blurred_active_id = inspector_eval_string(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => document.activeElement ? document.activeElement.id || "" : "")()"#,
    );
    assert_ne!(blurred_active_id, "email-input", "native click left focus on the editable element");

    let normal_state =
        wait_for_tab_web_mode_value(&harness, tab_id_arg.as_str(), "normal", Duration::from_secs(4))
            .unwrap_or_else(|| {
                let latest_state =
                    harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
                panic!(
                    "timed out waiting for normal mode after clicking a non-editable element; last_state={latest_state}; harness_log_tail:\n{}",
                    harness.log_tail(),
                )
            });
    assert_eq!(
        tab_web_mode(&normal_state),
        Some("normal"),
        "expected native click on non-editable element to leave insert mode: {normal_state}"
    );

    let _ = harness.run_json([
        "msg",
        "inspector",
        "detach",
        "--session-id",
        inspector_session.as_str(),
    ]);
}

#[test]
fn native_click_on_iframe_editable_switches_web_tab_into_insert_mode() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));
    let tab_id_arg = tab_id_arg(&reply);
    ensure_active_web_tab(&harness, 4);
    let layout = wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
        layout.get("mode").and_then(Value::as_str) == Some("normal")
            && layout
                .get("acceleration")
                .and_then(|value| value.get("state"))
                .and_then(Value::as_str)
                == Some("ready")
    })
    .unwrap_or_else(|| panic!("timed out waiting for initial browser layout"));

    let initial_state = harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
    assert_eq!(
        tab_web_mode(&initial_state),
        Some("normal"),
        "expected web tab to start in normal mode: {initial_state}"
    );

    let inspector_session = attach_inspector(&harness, tab_id_arg.as_str());
    let mut inspector_command_id = 1_i64;
    let scrolled = inspector_eval_string(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => {
            const frame = document.getElementById("editable-frame");
            if (!frame) throw new Error("editable-frame missing");
            frame.scrollIntoView({ block: "center", inline: "center" });
            return "ok";
        })()"#,
    );
    assert_eq!(scrolled, "ok", "failed to scroll iframe editor into view");
    thread::sleep(Duration::from_millis(100));

    let (logical_x, logical_y) = inspector_eval_point(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => {
            const frame = document.getElementById("editable-frame");
            if (!frame) throw new Error("editable-frame missing");
            const doc = frame.contentDocument;
            if (!doc) throw new Error("editable-frame document missing");
            const el = doc.getElementById("iframe-editor");
            if (!el) throw new Error("iframe-editor missing");
            const frameRect = frame.getBoundingClientRect();
            const rect = el.getBoundingClientRect();
            return JSON.stringify({
                x: Math.round(frameRect.left + rect.left + rect.width / 2),
                y: Math.round(frameRect.top + rect.top + rect.height / 2)
            });
        })()"#,
    );
    let (visual_x, visual_y) = browser_visual_point(&layout, logical_x, logical_y);
    native_window_click(&harness, visual_x, visual_y);

    let mode_state =
        wait_for_tab_web_mode_value(&harness, tab_id_arg.as_str(), "insert", Duration::from_secs(4))
            .unwrap_or_else(|| {
                let outer_active_id = inspector_eval_string(
                    &harness,
                    inspector_session.as_str(),
                    &mut inspector_command_id,
                    r#"(() => document.activeElement ? document.activeElement.id || document.activeElement.tagName || "" : "")()"#,
                );
                let inner_active_id = inspector_eval_string(
                    &harness,
                    inspector_session.as_str(),
                    &mut inspector_command_id,
                    r#"(() => {
                        const frame = document.getElementById("editable-frame");
                        if (!frame || !frame.contentDocument) return "";
                        const active = frame.contentDocument.activeElement;
                        return active ? active.id || active.tagName || "" : "";
                    })()"#,
                );
                let latest_state =
                    harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
                panic!(
                    "timed out waiting for insert mode after clicking an iframe editable element; outer_active={outer_active_id}; inner_active={inner_active_id}; last_state={latest_state}; harness_log_tail:\n{}",
                    harness.log_tail()
                )
            });
    assert_eq!(
        tab_web_mode(&mode_state),
        Some("insert"),
        "expected native click on iframe editable element to switch into insert mode: {mode_state}"
    );

    let _ = harness.run_json([
        "msg",
        "inspector",
        "detach",
        "--session-id",
        inspector_session.as_str(),
    ]);
}

#[test]
fn macos_native_click_on_editable_renders_visible_caret() {
    let harness = TaborHarness::start_foreground_app_bundle(false);
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));
    let tab_id_arg = tab_id_arg(&reply);
    ensure_active_web_tab(&harness, 4);

    let inspector_session = attach_inspector(&harness, tab_id_arg.as_str());
    let mut inspector_command_id = 1_i64;
    let initial_layout =
        wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
            layout.get("mode").and_then(Value::as_str) == Some("normal")
                && layout
                    .get("acceleration")
                    .and_then(|value| value.get("state"))
                    .and_then(Value::as_str)
                    == Some("ready")
        })
        .unwrap_or_else(|| panic!("timed out waiting for initial browser layout"));

    native_click_caret_probe(
        &harness,
        &initial_layout,
        inspector_session.as_str(),
        &mut inspector_command_id,
    );
    assert_caret_probe_focused(
        &harness,
        tab_id_arg.as_str(),
        &initial_layout,
        inspector_session.as_str(),
        &mut inspector_command_id,
        "initial focus",
    );
    assert_visible_caret_probe(
        &harness,
        &initial_layout,
        inspector_session.as_str(),
        &mut inspector_command_id,
        "initial focus",
    );

    let _ = harness.run_json([
        "msg",
        "inspector",
        "detach",
        "--session-id",
        inspector_session.as_str(),
    ]);
}

#[test]
fn macos_close_active_web_tab_restores_focusable_content_view() {
    let harness = TaborHarness::start_foreground_app_bundle(false);
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));
    let tab_id_arg = tab_id_arg(&reply);
    ensure_active_web_tab(&harness, 4);

    let inspector_session = attach_inspector(&harness, tab_id_arg.as_str());
    let mut inspector_command_id = 1_i64;
    let layout = wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
        layout.get("mode").and_then(Value::as_str) == Some("normal")
            && layout
                .get("acceleration")
                .and_then(|value| value.get("state"))
                .and_then(Value::as_str)
                == Some("ready")
    })
    .unwrap_or_else(|| panic!("timed out waiting for initial browser layout"));

    native_click_caret_probe(
        &harness,
        &layout,
        inspector_session.as_str(),
        &mut inspector_command_id,
    );
    assert_caret_probe_focused(
        &harness,
        tab_id_arg.as_str(),
        &layout,
        inspector_session.as_str(),
        &mut inspector_command_id,
        "focus handoff",
    );

    let focused_state = wait_for_window_debug_state_where(&harness, Duration::from_secs(4), |state| {
        state.is_key_window
            && state.first_responder_class.is_some()
            && state.content_view_class.is_some()
    })
    .unwrap_or_else(|| {
        let latest_state = window_debug_state(&harness);
        panic!(
            "timed out waiting for responder debug state after focusing editable content: {latest_state:?}; harness_log_tail:\n{}",
            harness.log_tail()
        )
    });
    assert_window_responder_classes_stable(&focused_state, "after focusing editable content");

    let _ = harness.run_json([
        "msg",
        "inspector",
        "detach",
        "--session-id",
        inspector_session.as_str(),
    ]);

    harness.run_ok(["msg", "close-tab"]);

    let restored_state =
        wait_for_window_debug_state_where(&harness, Duration::from_secs(4), |state| {
            let Some(first_responder_class) = state.first_responder_class.as_deref() else {
                return false;
            };
            let Some(content_view_class) = state.content_view_class.as_deref() else {
                return false;
            };
            state.is_key_window && first_responder_class == content_view_class
        })
        .unwrap_or_else(|| {
            let latest_state = window_debug_state(&harness);
            panic!(
                "timed out waiting for content view to regain first responder after closing web tab: {latest_state:?}; harness_log_tail:\n{}",
                harness.log_tail()
            )
        });
    assert_window_responder_classes_stable(&restored_state, "after closing active web tab");
    assert_eq!(
        restored_state.first_responder_class.as_deref(),
        restored_state.content_view_class.as_deref(),
        "expected the content view to regain first responder after closing the active web tab: {restored_state:?}"
    );
}

#[test]
fn native_multi_column_click_focuses_lower_input() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let _ = harness.run_ok(["msg", "config", "browser.multi_column.target_width_px=150"]);

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));
    let tab_id_arg = tab_id_arg(&reply);
    ensure_active_web_tab(&harness, 4);
    let _ = wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
        layout.get("mode").and_then(Value::as_str) == Some("normal")
            && layout
                .get("acceleration")
                .and_then(|value| value.get("state"))
                .and_then(Value::as_str)
                == Some("ready")
    })
    .unwrap_or_else(|| panic!("timed out waiting for initial browser layout"));

    let toggle_reply =
        harness.run_json(["msg", "dispatch-action", "--action", "ToggleMultiColumnTerminal"]);
    assert_eq!(toggle_reply.get("type").and_then(Value::as_str), Some("ok"));

    let _ = wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
        layout.get("mode").and_then(Value::as_str) == Some("multi_column")
            && layout.get("column_count").and_then(Value::as_u64).unwrap_or(0) >= 2
    })
    .unwrap_or_else(|| panic!("timed out waiting for multi-column browser layout"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);
    let _ = wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
        .unwrap_or_else(|| {
            panic!("timed out waiting for browser observation before multi-column click")
        });

    let scroll = json!([{ "type": "scroll", "dy": 960 }]).to_string();
    let scroll_reply = harness.run_json(["agent", "act", scroll.as_str()]);
    assert!(agent_action_results_all_ok(&scroll_reply), "agent scroll failed: {scroll_reply}");

    let inspector_session = attach_inspector(&harness, tab_id_arg.as_str());
    let mut inspector_command_id = 1_i64;
    let (logical_x, logical_y) = inspector_eval_point(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => {
            const el = document.getElementById("lower-input");
            if (!el) throw new Error("lower-input missing");
            const rect = el.getBoundingClientRect();
            return JSON.stringify({
                x: Math.round(rect.left + rect.width / 2),
                y: Math.round(rect.top + rect.height / 2)
            });
        })()"#,
    );

    let layout = wait_for_active_browser_layout_where(&harness, Duration::from_secs(8), |layout| {
        if layout.get("mode").and_then(Value::as_str) != Some("multi_column") {
            return false;
        }
        let viewport_height = layout
            .get("viewport")
            .and_then(|value| value.get("height"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if viewport_height <= 0 {
            return false;
        }
        let column_index = logical_y.div_euclid(viewport_height) as usize;
        layout
            .get("columns")
            .and_then(Value::as_array)
            .and_then(|columns| columns.get(column_index))
            .is_some()
    })
    .unwrap_or_else(|| {
        let latest_state = harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
        panic!(
            "timed out waiting for multi-column browser layout to cover lower input; logical_point=({logical_x}, {logical_y}); last_state={latest_state}; harness_log_tail:\n{}",
            harness.log_tail()
        )
    });

    let viewport_height = layout
        .get("viewport")
        .and_then(|value| value.get("height"))
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing browser viewport height: {layout}"));
    assert!(
        logical_y >= viewport_height,
        "expected lower input to render in a folded column below the first viewport: center_y={logical_y}, viewport_height={viewport_height}, layout={layout}"
    );

    let (visual_x, visual_y) = browser_visual_point(&layout, logical_x, logical_y);
    native_window_click(&harness, visual_x, visual_y);
    let active_id = inspector_eval_string(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => document.activeElement ? document.activeElement.id || "" : "")()"#,
    );
    assert_eq!(active_id, "lower-input", "native folded click focused the wrong element");

    let input_text = "native multi column click";
    let type_actions = json!([{ "type": "type", "text": input_text }]).to_string();
    let type_reply = harness.run_json(["agent", "act", type_actions.as_str()]);
    assert!(agent_action_results_all_ok(&type_reply), "agent type failed: {type_reply}");
    let typed_value = inspector_eval_string(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        r#"(() => document.getElementById("lower-input")?.value || "")()"#,
    );
    assert_eq!(typed_value, input_text);

    let _ = harness.run_json([
        "msg",
        "inspector",
        "detach",
        "--session-id",
        inspector_session.as_str(),
    ]);
}

#[test]
fn agent_wait_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);
    let _ = wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
        .unwrap_or_else(|| panic!("timed out waiting for initial agent observation"));

    let scroll = json!([{ "type": "scroll", "dy": 320 }]).to_string();
    let scroll_reply = harness.run_json(["agent", "act", scroll.as_str()]);
    assert!(agent_action_results_all_ok(&scroll_reply), "agent scroll failed: {scroll_reply}");

    let observation = harness.run_json(["agent", "observe"]);
    let fetch_id = find_observed_element_id(&observation, "Fetch data");
    let actions = json!([
        { "type": "click", "id": fetch_id },
        { "type": "wait", "text": "Fetch output: error", "timeout_ms": 5000 }
    ])
    .to_string();
    let act = harness.run_json(["agent", "act", actions.as_str()]);
    assert_eq!(act.get("type").and_then(Value::as_str), Some("act"));
    assert!(agent_action_results_all_ok(&act), "agent act failed: {act}");
}

#[test]
fn agent_artifacts_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);

    let first_observation =
        wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
            .unwrap_or_else(|| panic!("timed out waiting for initial agent observation"));
    let email_id = find_observed_element_id(&first_observation, "Email");

    let scroll = json!([{ "type": "scroll", "dy": 320 }]).to_string();
    let scroll_reply = harness.run_json(["agent", "act", scroll.as_str()]);
    assert!(agent_action_results_all_ok(&scroll_reply), "agent scroll failed: {scroll_reply}");

    let second_observation = harness.run_json(["agent", "observe"]);
    let file_input_id = find_observed_element_id(&second_observation, "file-input");
    let download_id = find_observed_element_id(&second_observation, "Download file");

    let screenshot_path = harness.tmp_path("agent-screenshot.png");
    let screenshot_path_str = screenshot_path.to_str().expect("screenshot path is not valid utf-8");
    let screenshot = harness.run_json([
        "agent",
        "screenshot",
        "--path",
        screenshot_path_str,
        "--element-id",
        email_id.as_str(),
    ]);
    assert_eq!(screenshot.get("type").and_then(Value::as_str), Some("screenshot"));
    assert!(screenshot_path.exists(), "screenshot path missing: {screenshot}");
    let screenshot_width = screenshot
        .get("width")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing screenshot width: {screenshot}"));
    let screenshot_height = screenshot
        .get("height")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing screenshot height: {screenshot}"));
    assert!(screenshot_width > 0 && screenshot_height > 0, "invalid screenshot size: {screenshot}");
    let screenshot_image = image::load_from_memory(
        &std::fs::read(&screenshot_path).expect("failed to read screenshot"),
    )
    .expect("failed to decode screenshot");
    assert_eq!(u64::from(screenshot_image.width()), screenshot_width);
    assert_eq!(u64::from(screenshot_image.height()), screenshot_height);

    let pdf_path = harness.tmp_path("agent-page.pdf");
    let pdf_path_str = pdf_path.to_str().expect("pdf path is not valid utf-8");
    let pdf = harness.run_json(["agent", "pdf", "--path", pdf_path_str]);
    assert_eq!(pdf.get("type").and_then(Value::as_str), Some("pdf"));
    let pdf_bytes = std::fs::read(&pdf_path).expect("failed to read generated pdf");
    assert!(pdf_bytes.starts_with(b"%PDF"), "generated PDF missing header");

    let upload_path = harness.tmp_path("agent-upload.txt");
    std::fs::write(&upload_path, "uploaded from e2e").expect("failed to write upload fixture");
    let upload_path_str = upload_path.to_str().expect("upload path is not valid utf-8");
    let upload = harness.run_json(["agent", "upload", file_input_id.as_str(), upload_path_str]);
    assert_eq!(upload.get("type").and_then(Value::as_str), Some("upload"));

    let upload_wait =
        json!([{ "type": "wait", "text": "Files: agent-upload.txt", "timeout_ms": 5000 }])
            .to_string();
    let upload_wait_reply = harness.run_json(["agent", "act", upload_wait.as_str()]);
    assert!(
        agent_action_results_all_ok(&upload_wait_reply),
        "agent upload wait failed: {upload_wait_reply}"
    );

    let download_actions = json!([{ "type": "click", "id": download_id }]).to_string();
    let download_click = harness.run_json(["agent", "act", download_actions.as_str()]);
    assert!(
        agent_action_results_all_ok(&download_click),
        "agent download click failed: {download_click}"
    );

    let download = wait_for_agent_download(&harness, "agent-download.txt", Duration::from_secs(6))
        .unwrap_or_else(|| panic!("timed out waiting for download"));
    let download_path = download
        .get("full_path")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("download missing full_path: {download}"));
    assert!(Path::new(download_path).exists(), "downloaded file missing: {download}");
}

#[test]
fn macos_opened_pdf_document_appears_as_web_tab_without_download() {
    let harness = TaborHarness::start_foreground_app_bundle(false);
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);
    let _ = wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
        .unwrap_or_else(|| panic!("timed out waiting for initial agent observation"));

    let pdf_path = harness.tmp_path("opened-document.pdf");
    let pdf_path_str = pdf_path.to_str().expect("pdf path is not valid utf-8");
    let pdf = harness.run_json(["agent", "pdf", "--path", pdf_path_str]);
    assert_eq!(pdf.get("type").and_then(Value::as_str), Some("pdf"));
    let pdf_bytes = std::fs::read(&pdf_path).expect("failed to read generated pdf");
    assert!(pdf_bytes.starts_with(b"%PDF"), "generated PDF missing header");

    let expected_pdf_path = pdf_path.canonicalize().unwrap_or_else(|err| {
        panic!("failed to canonicalize generated pdf path {}: {err}", pdf_path.display())
    });
    let expected_url = Url::from_file_path(&expected_pdf_path)
        .expect("failed to build file URL for generated pdf")
        .to_string();
    harness.open_file_with_app_bundle(&pdf_path);

    let active_pdf_tab =
        wait_for_active_web_url_value(&harness, expected_url.as_str(), Duration::from_secs(8))
            .unwrap_or_else(|| panic!("timed out waiting for active PDF tab at {expected_url}"));
    let pdf_tab_id = tab_id_pair(&active_pdf_tab)
        .unwrap_or_else(|| panic!("missing tab id for active PDF tab: {active_pdf_tab}"));
    let pdf_tab_id_arg = format!("{}:{}", pdf_tab_id.0, pdf_tab_id.1);
    let _ = wait_for_tab_acceleration_settled(
        &harness,
        pdf_tab_id_arg.as_str(),
        Duration::from_secs(8),
    )
    .unwrap_or_else(|| panic!("timed out waiting for PDF tab acceleration to settle"));

    harness.run_json(["agent", "use", "--active"]);
    let downloads = harness.run_json(["agent", "downloads"]);
    let download_entries = downloads
        .get("downloads")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing downloads array: {downloads}"));
    assert!(
        download_entries.is_empty(),
        "expected opened PDF tab to avoid tracked downloads: {downloads}"
    );
}

#[test]
fn agent_events_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);

    let baseline = harness.run_json(["agent", "events", "--max", "1"]);
    let since = baseline.get("last_event_id").and_then(Value::as_u64).unwrap_or(0).to_string();

    let _ = wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
        .unwrap_or_else(|| panic!("timed out waiting for initial agent observation"));

    let scroll = json!([{ "type": "scroll", "dy": 320 }]).to_string();
    let scroll_reply = harness.run_json(["agent", "act", scroll.as_str()]);
    assert!(agent_action_results_all_ok(&scroll_reply), "agent scroll failed: {scroll_reply}");

    let second_observation = harness.run_json(["agent", "observe"]);
    let fetch_id = find_observed_element_id(&second_observation, "Fetch data");
    let console_id = find_observed_element_id(&second_observation, "Console log");

    let actions = json!([
        { "type": "click", "id": console_id },
        { "type": "click", "id": fetch_id },
        { "type": "wait", "text": "Fetch output: error", "timeout_ms": 5000 }
    ])
    .to_string();
    let act = harness.run_json(["agent", "act", actions.as_str()]);
    assert!(agent_action_results_all_ok(&act), "agent event setup failed: {act}");

    let events = wait_for_agent_events(
        &harness,
        since.as_str(),
        &["console", "network"],
        Duration::from_secs(6),
    )
    .unwrap_or_else(|| panic!("timed out waiting for agent events"));
    let returned_events = events
        .get("events")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing agent events array: {events}"));
    assert!(
        returned_events
            .iter()
            .any(|event| event.get("kind").and_then(Value::as_str) == Some("console")),
        "missing console event: {events}"
    );
    assert!(
        returned_events
            .iter()
            .any(|event| event.get("kind").and_then(Value::as_str) == Some("network")),
        "missing network event: {events}"
    );
}

#[test]
fn browser_clipboard_api_smoke() {
    let server = ClipboardFixtureServer::start();
    let harness = TaborHarness::start();
    let fixture = server.url("/fixture.html");

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));
    let tab_id_arg = tab_id_arg(&reply);
    let inspector_session = attach_inspector(&harness, tab_id_arg.as_str());
    let mut inspector_command_id = 1_i64;

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);

    let observation =
        wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
            .unwrap_or_else(|| panic!("timed out waiting for initial agent observation"));
    let email_id = find_observed_element_id(&observation, "Email");
    let notes_id = find_observed_element_id(&observation, "Notes");
    let focus_notes_id = find_observed_element_id(&observation, "Focus Notes");
    let page_copy_id = find_observed_element_id(&observation, "Page Copy");
    let page_paste_id = find_observed_element_id(&observation, "Page Paste");
    let clipboard_status_id = find_observed_element_id(&observation, "Clipboard API status");

    let source_text = "web clipboard copy";
    let paste_text = "web clipboard paste";
    let notes_prefix = "prefix:";
    let setup_actions = json!([
        { "type": "fill", "id": email_id, "text": source_text },
        { "type": "fill", "id": notes_id, "text": notes_prefix }
    ])
    .to_string();
    let setup = harness.run_json(["agent", "act", setup_actions.as_str()]);
    assert!(agent_action_results_all_ok(&setup), "clipboard setup failed: {setup}");

    trusted_click(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        page_copy_id.as_str(),
    );
    let copy_wait = json!([
        { "type": "wait", "text": format!("Clipboard API write: {source_text}"), "timeout_ms": 5000 }
    ])
    .to_string();
    let copy_reply = harness.run_json(["agent", "act", copy_wait.as_str()]);
    if !agent_action_results_all_ok(&copy_reply) {
        let status = harness.run_json(["agent", "inspect", clipboard_status_id.as_str()]);
        panic!("page clipboard write failed: {copy_reply}; status={status}");
    }
    let copied = wait_for_clipboard_text(&harness, source_text, Duration::from_secs(4))
        .unwrap_or_else(|| panic!("timed out waiting for copied clipboard text"));
    assert_eq!(copied, source_text);

    let clipboard_seed = harness.run_json(["agent", "clipboard", "set", paste_text]);
    assert_eq!(clipboard_seed.get("type").and_then(Value::as_str), Some("clipboard"));

    trusted_click(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        focus_notes_id.as_str(),
    );
    trusted_click(
        &harness,
        inspector_session.as_str(),
        &mut inspector_command_id,
        page_paste_id.as_str(),
    );
    let paste_wait = json!([
        { "type": "wait", "text": format!("Clipboard API read: {paste_text}"), "timeout_ms": 5000 }
    ])
    .to_string();
    let paste_reply = harness.run_json(["agent", "act", paste_wait.as_str()]);
    if !agent_action_results_all_ok(&paste_reply) {
        let status = harness.run_json(["agent", "inspect", clipboard_status_id.as_str()]);
        panic!("page clipboard read failed: {paste_reply}; status={status}");
    }
    let expected_notes = format!("{notes_prefix}{paste_text}");
    let notes_value = wait_for_agent_value(
        &harness,
        notes_id.as_str(),
        expected_notes.as_str(),
        Duration::from_secs(4),
    )
    .unwrap_or_else(|| panic!("timed out waiting for pasted notes value"));
    assert_eq!(notes_value, expected_notes);

    let _ = harness.run_json([
        "msg",
        "inspector",
        "detach",
        "--session-id",
        inspector_session.as_str(),
    ]);
}

#[test]
fn macos_fullscreen_notch_ears_red_window_snapshot() {
    let harness = TaborHarness::start_foreground_app_bundle(false);
    let initial_state = window_debug_state(&harness);

    if rect_is_empty(&initial_state.auxiliary_top_left_screen_points)
        && rect_is_empty(&initial_state.auxiliary_top_right_screen_points)
    {
        eprintln!("skipping fullscreen notch test on a display without auxiliary notch regions");
        return;
    }

    let toggle_reply = harness.run_json(["msg", "dispatch-action", "--action", "ToggleFullscreen"]);
    assert_eq!(toggle_reply.get("type").and_then(Value::as_str), Some("ok"));
    let _ = window_debug_snapshot(&harness);

    let fullscreen_state = wait_for_fullscreen_window_state(&harness, Duration::from_secs(12))
        .unwrap_or_else(|state| {
            panic!(
                "timed out waiting for stable fullscreen state; last_state={state:?}; harness_log_tail:\n{}",
                harness.log_tail(),
            )
        });
    assert!(
        fullscreen_state.simple_fullscreen,
        "expected real-ear fullscreen to use simple fullscreen on a notched display: {fullscreen_state:?}"
    );
    assert!(
        !fullscreen_state.native_fullscreen,
        "real-ear fullscreen should avoid native AppKit fullscreen: {fullscreen_state:?}"
    );
    assert!(
        fullscreen_state.real_ear_fullscreen_active,
        "expected fullscreen debug state to report real-ear fullscreen: {fullscreen_state:?}"
    );
    assert_rect_close(
        &fullscreen_state.content_frame_screen_points,
        &fullscreen_state.screen_frame_points,
        1.0,
        "real-ear fullscreen content frame should span the fullscreen window",
    );
    let snapshot = window_debug_snapshot(&harness);
    assert!(snapshot.state.simple_fullscreen, "window snapshot did not report simple fullscreen");
    assert!(
        snapshot.state.real_ear_fullscreen_active,
        "window snapshot did not report active real-ear fullscreen"
    );

    let png_bytes =
        BASE64.decode(snapshot.png_base64.as_bytes()).expect("failed to decode snapshot PNG");
    let image =
        image::load_from_memory(&png_bytes).expect("failed to decode window snapshot").to_rgba8();
    assert_eq!(image.width(), snapshot.width);
    assert_eq!(image.height(), snapshot.height);

    let aux_rects = [
        &fullscreen_state.auxiliary_top_left_screen_points,
        &fullscreen_state.auxiliary_top_right_screen_points,
    ];

    for (index, rect) in aux_rects.iter().enumerate() {
        if rect_is_empty(rect) {
            continue;
        }

        let local = screen_rect_to_snapshot_pixels(
            rect,
            &snapshot.snapshot_screen_points,
            snapshot.state.scale_factor,
        );
        if !rect_within_snapshot(&local, snapshot.width, snapshot.height) {
            write_notch_failure_artifacts(
                &harness,
                &snapshot,
                &fullscreen_state,
                format!("auxiliary rect {index} mapped outside snapshot bounds: {local:?}"),
            );
        }

        let red_ratio = sampled_red_ratio(&image, local);
        if red_ratio < 0.95 {
            write_notch_failure_artifacts(
                &harness,
                &snapshot,
                &fullscreen_state,
                format!("auxiliary rect {index} was not red enough: ratio={red_ratio:.3}"),
            );
        }
    }
}

#[test]
fn macos_standard_zoom_button_enters_real_ear_fullscreen() {
    let harness = TaborHarness::start_foreground_app_bundle(false);
    let initial_state = window_debug_state(&harness);

    if rect_is_empty(&initial_state.auxiliary_top_left_screen_points)
        && rect_is_empty(&initial_state.auxiliary_top_right_screen_points)
    {
        eprintln!(
            "skipping fullscreen zoom-button test on a display without auxiliary notch regions"
        );
        return;
    }

    let zoom_reply = harness.run_json([
        "msg",
        "send",
        r#"{"type":"window_debug_press_standard_button","button":"zoom"}"#,
    ]);
    assert_eq!(zoom_reply.get("type").and_then(Value::as_str), Some("ok"));

    let fullscreen_state = wait_for_fullscreen_window_state(&harness, Duration::from_secs(12))
        .unwrap_or_else(|state| {
            panic!(
                "timed out waiting for real-ear fullscreen after zoom button press; last_state={state:?}; harness_log_tail:\n{}",
                harness.log_tail(),
            )
        });
    assert!(
        fullscreen_state.simple_fullscreen,
        "expected standard zoom button to enter simple real-ear fullscreen: {fullscreen_state:?}"
    );
    assert!(
        !fullscreen_state.native_fullscreen,
        "standard zoom button should avoid native AppKit fullscreen on notched displays: {fullscreen_state:?}"
    );
    assert!(
        fullscreen_state.real_ear_fullscreen_active,
        "expected standard zoom button path to report active real-ear fullscreen: {fullscreen_state:?}"
    );
    assert_rect_close(
        &fullscreen_state.content_frame_screen_points,
        &fullscreen_state.screen_frame_points,
        1.0,
        "real-ear fullscreen content frame should span the fullscreen window after a zoom button press",
    );
}

#[test]
fn macos_real_ear_fullscreen_resizes_active_web_viewport() {
    let harness = TaborHarness::start_foreground_app_bundle(false);
    let initial_state = window_debug_state(&harness);

    if rect_is_empty(&initial_state.auxiliary_top_left_screen_points)
        && rect_is_empty(&initial_state.auxiliary_top_right_screen_points)
    {
        eprintln!(
            "skipping fullscreen web-resize test on a display without auxiliary notch regions"
        );
        return;
    }

    let fixture = fixture_url();
    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    let tab_id_arg = tab_id_arg(&reply);

    let initial_tab_state =
        harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
    let initial_layout = initial_tab_state
        .get("tab")
        .and_then(|tab| tab.get("browser_layout"))
        .unwrap_or_else(|| panic!("missing initial browser_layout: {initial_tab_state}"));
    let initial_width =
        initial_layout.get("logical_width").and_then(Value::as_u64).unwrap_or_else(|| {
            panic!("missing initial browser_layout.logical_width: {initial_tab_state}")
        });
    let initial_height =
        initial_layout.get("logical_height").and_then(Value::as_u64).unwrap_or_else(|| {
            panic!("missing initial browser_layout.logical_height: {initial_tab_state}")
        });

    let zoom_reply = harness.run_json([
        "msg",
        "send",
        r#"{"type":"window_debug_press_standard_button","button":"zoom"}"#,
    ]);
    assert_eq!(zoom_reply.get("type").and_then(Value::as_str), Some("ok"));

    let fullscreen_state = wait_for_fullscreen_window_state(&harness, Duration::from_secs(12))
        .unwrap_or_else(|state| {
            panic!(
                "timed out waiting for real-ear fullscreen before resize check; last_state={state:?}; harness_log_tail:\n{}",
                harness.log_tail(),
            )
        });
    assert!(
        fullscreen_state.simple_fullscreen && fullscreen_state.real_ear_fullscreen_active,
        "expected real-ear fullscreen to be active before resize check: {fullscreen_state:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(12);
    let mut final_tab_state = initial_tab_state;
    let mut final_width = initial_width;
    let mut final_height = initial_height;
    while Instant::now() < deadline {
        final_tab_state =
            harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg.as_str()]);
        let layout = final_tab_state
            .get("tab")
            .and_then(|tab| tab.get("browser_layout"))
            .unwrap_or_else(|| panic!("missing fullscreen browser_layout: {final_tab_state}"));
        final_width = layout.get("logical_width").and_then(Value::as_u64).unwrap_or_else(|| {
            panic!("missing fullscreen browser_layout.logical_width: {final_tab_state}")
        });
        final_height = layout.get("logical_height").and_then(Value::as_u64).unwrap_or_else(|| {
            panic!("missing fullscreen browser_layout.logical_height: {final_tab_state}")
        });

        if final_width > initial_width && final_height > initial_height {
            break;
        }

        thread::sleep(POLL_INTERVAL);
    }

    assert!(
        final_width > initial_width,
        "expected fullscreen browser width to grow beyond {initial_width}, got {final_width}; tab_state={final_tab_state}; harness_log_tail:\n{}",
        harness.log_tail(),
    );
    assert!(
        final_height > initial_height,
        "expected fullscreen browser height to grow beyond {initial_height}, got {final_height}; tab_state={final_tab_state}; harness_log_tail:\n{}",
        harness.log_tail(),
    );
}

#[test]
fn macos_toggle_fullscreen_action_exits_real_ear_fullscreen() {
    let harness = TaborHarness::start_foreground_app_bundle(false);
    let initial_state = window_debug_state(&harness);

    if rect_is_empty(&initial_state.auxiliary_top_left_screen_points)
        && rect_is_empty(&initial_state.auxiliary_top_right_screen_points)
    {
        eprintln!(
            "skipping fullscreen zoom-button test on a display without auxiliary notch regions"
        );
        return;
    }

    let toggle_reply = harness.run_json(["msg", "dispatch-action", "--action", "ToggleFullscreen"]);
    assert_eq!(toggle_reply.get("type").and_then(Value::as_str), Some("ok"));
    let _ = window_debug_snapshot(&harness);

    let fullscreen_state = wait_for_fullscreen_window_state(&harness, Duration::from_secs(12))
        .unwrap_or_else(|state| {
            panic!(
                "timed out waiting for real-ear fullscreen before exit test; last_state={state:?}; harness_log_tail:\n{}",
                harness.log_tail(),
            )
        });
    assert!(
        fullscreen_state.simple_fullscreen && fullscreen_state.real_ear_fullscreen_active,
        "expected real-ear fullscreen to be active before exit test: {fullscreen_state:?}"
    );

    let exit_reply = harness.run_json(["msg", "dispatch-action", "--action", "ToggleFullscreen"]);
    assert_eq!(exit_reply.get("type").and_then(Value::as_str), Some("ok"));

    let windowed_state = wait_for_windowed_window_state(&harness, Duration::from_secs(12))
        .unwrap_or_else(|state| {
            panic!(
                "timed out waiting for windowed state after fullscreen exit; last_state={state:?}; harness_log_tail:\n{}",
                harness.log_tail(),
            )
        });
    assert!(
        !windowed_state.native_fullscreen
            && !windowed_state.simple_fullscreen
            && !windowed_state.winit_fullscreen,
        "zoom button should exit fullscreen completely: {windowed_state:?}"
    );
}

#[test]
fn macos_standard_minimize_button_exits_real_ear_fullscreen_and_miniaturizes() {
    let harness = TaborHarness::start_foreground_app_bundle(false);
    let initial_state = window_debug_state(&harness);

    if rect_is_empty(&initial_state.auxiliary_top_left_screen_points)
        && rect_is_empty(&initial_state.auxiliary_top_right_screen_points)
    {
        eprintln!(
            "skipping fullscreen minimize-button test on a display without auxiliary notch regions"
        );
        return;
    }

    let enter_reply = harness.run_json(["msg", "dispatch-action", "--action", "ToggleFullscreen"]);
    assert_eq!(enter_reply.get("type").and_then(Value::as_str), Some("ok"));

    let fullscreen_state = wait_for_fullscreen_window_state(&harness, Duration::from_secs(12))
        .unwrap_or_else(|state| {
            panic!(
                "timed out waiting for real-ear fullscreen before minimize test; last_state={state:?}; harness_log_tail:\n{}",
                harness.log_tail(),
            )
        });
    assert!(
        fullscreen_state.simple_fullscreen && fullscreen_state.real_ear_fullscreen_active,
        "expected real-ear fullscreen to be active before minimize test: {fullscreen_state:?}"
    );

    let minimize_reply = harness.run_json([
        "msg",
        "send",
        r#"{"type":"window_debug_press_standard_button","button":"minimize"}"#,
    ]);
    assert_eq!(minimize_reply.get("type").and_then(Value::as_str), Some("ok"));

    let miniaturized_state =
        wait_for_miniaturized_window_state(&harness, Duration::from_secs(12)).unwrap_or_else(
            |state| {
                panic!(
                    "timed out waiting for miniaturized state after fullscreen minimize; last_state={state:?}; harness_log_tail:\n{}",
                    harness.log_tail(),
                )
            },
        );
    assert!(
        miniaturized_state.is_miniaturized,
        "expected miniaturized state after fullscreen minimize: {miniaturized_state:?}"
    );
    assert!(
        !miniaturized_state.native_fullscreen
            && !miniaturized_state.simple_fullscreen
            && !miniaturized_state.winit_fullscreen,
        "minimize should leave fullscreen completely before miniaturizing: {miniaturized_state:?}"
    );
}

#[test]
fn close_active_web_tab_refreshes_terminal_program_name() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let initial_tabs = harness.run_json(["msg", "list-tabs"]);
    let terminal_id =
        first_terminal_tab_id(&initial_tabs).expect("missing initial terminal tab in list-tabs");
    let initial_program = wait_for_active_terminal_program_name(&harness, Duration::from_secs(6))
        .expect("active terminal never reported a program name");

    harness.run_ok(["msg", "send-input", "sleep 0.5; sleep 10\n"]);

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    thread::sleep(Duration::from_millis(900));

    harness.run_ok(["msg", "close-tab"]);

    let program_name =
        wait_for_active_terminal_program_name_value(&harness, "sleep", Duration::from_secs(6));
    let final_state = harness.run_json(["msg", "list-tabs"]);
    let tabs = flatten_tabs(&final_state);
    assert_eq!(tabs.len(), 1, "expected one tab after closing active web tab: {final_state}");

    let remaining = tabs[0];
    assert_eq!(
        tab_id_pair(remaining),
        Some(terminal_id),
        "expected original terminal tab to remain active: {final_state}"
    );
    assert_eq!(
        remaining.get("kind").and_then(Value::as_str),
        Some("terminal"),
        "expected remaining tab to be terminal: {final_state}"
    );
    assert_eq!(
        remaining.get("is_active").and_then(Value::as_bool),
        Some(true),
        "expected remaining terminal tab to be active: {final_state}"
    );

    assert_eq!(
        program_name.as_deref(),
        Some("sleep"),
        "expected close-tab handoff to refresh terminal program name (initial: {initial_program}): {final_state}"
    );
}

#[test]
fn close_active_web_tab_preserves_terminal_key_input_stream() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let initial_tabs = harness.run_json(["msg", "list-tabs"]);
    let terminal_id =
        first_terminal_tab_id(&initial_tabs).expect("missing initial terminal tab in list-tabs");
    let tab_id_arg = format!("{}:{}", terminal_id.0, terminal_id.1);

    let capture_path = harness.tmp_path("terminal-key-capture.txt");
    let capture_path_str = capture_path.to_str().expect("capture path is not valid utf-8");
    let command = format!("IFS= read -r line; printf '%s' \"$line\" > {}\n", capture_path_str);
    harness.run_ok(["msg", "send-input", command.as_str(), "--tab-id", tab_id_arg.as_str()]);
    thread::sleep(Duration::from_millis(200));

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    thread::sleep(Duration::from_millis(900));
    harness.run_ok(["msg", "close-tab"]);

    let sentinel = "alpha123beta456gamma789delta";
    for ch in sentinel.chars() {
        let key = ch.to_string();
        send_terminal_key(&harness, terminal_id, key.as_str(), Some(key.as_str()));
    }
    send_terminal_key(&harness, terminal_id, "enter", None);

    let captured =
        wait_for_file_content(&capture_path, Duration::from_secs(6)).unwrap_or_else(|| {
            panic!("timed out waiting for capture file: {}", capture_path.display())
        });
    assert_eq!(
        captured, sentinel,
        "terminal_key stream mismatch after opening and closing a web tab"
    );
}

#[test]
fn repeated_web_tab_close_releases_webview_resources() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();
    let harness_pid = harness.pid();

    let baseline = runtime_metrics(&harness);
    let (baseline_live, baseline_created, baseline_dropped) = webview_counts(&baseline);
    let baseline_close_count = web_close_count(&baseline);

    let cycles = 8_u64;
    let mut steady_children = None;

    for iteration in 0..cycles {
        let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
        assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

        thread::sleep(Duration::from_millis(200));
        harness.run_ok(["msg", "close-tab"]);

        let expected =
            (baseline_live, baseline_created + iteration + 1, baseline_dropped + iteration + 1);
        let settled = wait_for_webview_counts(&harness, expected, Duration::from_secs(4))
            .unwrap_or_else(|| {
                panic!("timed out waiting for expected webview counts: {expected:?}")
            });
        let actual = webview_counts(&settled);
        assert_eq!(
            actual, expected,
            "unexpected webview metrics after close cycle {iteration}: {settled}"
        );

        if iteration == 0 {
            steady_children = Some(child_process_count(harness_pid));
        }
    }

    let final_metrics = runtime_metrics(&harness);
    let (final_live, final_created, final_dropped) = webview_counts(&final_metrics);
    assert_eq!(
        final_live, baseline_live,
        "webview live count changed after close cycles: {final_metrics}"
    );
    assert_eq!(
        final_created,
        baseline_created + cycles,
        "webview created count mismatch after close cycles: {final_metrics}"
    );
    assert_eq!(
        final_dropped,
        baseline_dropped + cycles,
        "webview dropped count mismatch after close cycles: {final_metrics}"
    );

    let final_close_count = web_close_count(&final_metrics);
    assert!(
        final_close_count >= baseline_close_count + cycles,
        "web close counter did not advance as expected: {final_metrics}"
    );

    let steady_children = steady_children.expect("steady child process baseline missing");
    let child_cap = steady_children + 2;
    let settled_children =
        wait_for_child_process_count_max(harness_pid, child_cap, Duration::from_secs(4))
            .unwrap_or_else(|| {
                let current = child_process_count(harness_pid);
                panic!(
                    "CEF subprocesses kept accumulating (steady={steady_children}, cap={child_cap}, current={current})"
                )
            });
    assert!(
        settled_children <= child_cap,
        "child process count exceeded cap after close cycles: {settled_children} > {child_cap}"
    );
}

#[test]
fn reopen_web_tab_after_idle_shutdown_window_keeps_instance_alive() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let idle_wait_secs = env_u64("TABOR_REPRO_IDLE_WAIT_SECS", 2);
    let switch_active = env_u64("TABOR_REPRO_SWITCH_ACTIVE", 1) != 0;

    let first_url = format!("{fixture}#first");
    let first_reply = harness.run_json(["msg", "create-tab", "--web", first_url.as_str()]);
    assert_eq!(first_reply.get("type").and_then(Value::as_str), Some("tab_created"));

    if switch_active {
        harness.run_ok(["msg", "select-tab", "--previous"]);
        harness.run_ok(["msg", "select-tab", "--next"]);
        harness.run_ok(["msg", "select-tab", "--next"]);
        harness.run_ok(["msg", "select-tab", "--previous"]);
    }

    ensure_active_web_tab(&harness, 5);
    harness.run_ok(["msg", "close-tab"]);
    harness.run_ok(["msg", "ping"]);
    thread::sleep(Duration::from_millis(120));
    assert_only_terminal_tab_remains(&harness);

    thread::sleep(Duration::from_secs(idle_wait_secs));

    let second_url = format!("{fixture}#second");
    let second_reply = harness.run_json(["msg", "create-tab", "--web", second_url.as_str()]);
    assert_eq!(second_reply.get("type").and_then(Value::as_str), Some("tab_created"));
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|value| value.parse::<u64>().ok()).unwrap_or(default)
}

#[derive(Debug, Clone, Copy)]
struct SnapshotPixelRect {
    x0: i64,
    y0: i64,
    x1: i64,
    y1: i64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct LogicalRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn window_debug_state(harness: &TaborHarness) -> WindowDebugState {
    let reply = harness.send_raw_request(json!({ "type": "window_debug_state" }));
    let Some(state) = reply.get("state") else {
        panic!("missing window debug state payload: {reply}");
    };
    serde_json::from_value(state.clone())
        .unwrap_or_else(|err| panic!("invalid window debug state reply: {err}; reply={reply}"))
}

fn window_debug_snapshot(harness: &TaborHarness) -> WindowDebugSnapshot {
    let reply = harness.send_raw_request(json!({
        "type": "window_debug_snapshot",
        "highlight_notch_ears": true
    }));
    let Some(snapshot) = reply.get("snapshot") else {
        panic!("missing window debug snapshot payload: {reply}");
    };
    serde_json::from_value(snapshot.clone())
        .unwrap_or_else(|err| panic!("invalid window debug snapshot reply: {err}; reply={reply}"))
}

fn wait_for_window_debug_state_where<F>(
    harness: &TaborHarness,
    timeout: Duration,
    mut predicate: F,
) -> Option<WindowDebugState>
where
    F: FnMut(&WindowDebugState) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let state = window_debug_state(harness);
        if predicate(&state) {
            return Some(state);
        }
        thread::sleep(POLL_INTERVAL);
    }
    None
}

fn assert_window_responder_classes_stable(state: &WindowDebugState, label: &str) {
    let first_responder_class = state.first_responder_class.as_deref().unwrap_or_else(|| {
        panic!("missing first_responder_class during {label}: {state:?}");
    });
    let content_view_class = state.content_view_class.as_deref().unwrap_or_else(|| {
        panic!("missing content_view_class during {label}: {state:?}");
    });

    assert!(
        !first_responder_class.starts_with("TaborNoFirstResponder_"),
        "first responder leaked the no-first-responder override during {label}: {state:?}"
    );
    assert!(
        !content_view_class.starts_with("TaborNoFirstResponder_"),
        "content view leaked the no-first-responder override during {label}: {state:?}"
    );
}

fn browser_visual_point(layout: &Value, logical_x: i64, logical_y: i64) -> (f64, f64) {
    let layout_logical_width = layout
        .get("logical_width")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing browser logical_width: {layout}"));
    let layout_logical_height = layout
        .get("logical_height")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing browser logical_height: {layout}"));
    let surface_width = layout
        .get("acceleration")
        .and_then(|value| value.get("main_surface_width"))
        .and_then(Value::as_i64)
        .filter(|width| *width > 0)
        .unwrap_or(layout_logical_width);
    let surface_height = layout
        .get("acceleration")
        .and_then(|value| value.get("main_surface_height"))
        .and_then(Value::as_i64)
        .filter(|height| *height > 0)
        .unwrap_or(layout_logical_height);
    let logical_x = if logical_x <= layout_logical_width || surface_width == layout_logical_width {
        logical_x
    } else {
        ((logical_x as f64) * (layout_logical_width as f64) / (surface_width as f64)).round() as i64
    };
    let logical_y = if logical_y <= layout_logical_height || surface_height == layout_logical_height
    {
        logical_y
    } else {
        ((logical_y as f64) * (layout_logical_height as f64) / (surface_height as f64)).round()
            as i64
    };
    let viewport =
        layout.get("viewport").unwrap_or_else(|| panic!("missing browser viewport: {layout}"));
    let viewport_y = viewport
        .get("y")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing browser viewport.y: {layout}"));
    let viewport_height = viewport
        .get("height")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing browser viewport.height: {layout}"));
    assert!(viewport_height > 0, "invalid browser viewport height: {layout}");

    let column_index = logical_y.div_euclid(viewport_height) as usize;
    let column_y = logical_y.rem_euclid(viewport_height);
    let column = layout
        .get("columns")
        .and_then(Value::as_array)
        .and_then(|columns| columns.get(column_index))
        .unwrap_or_else(|| {
            panic!(
                "missing browser column for logical point ({logical_x}, {logical_y}) in layout: {layout}"
            )
        });
    let column_x = column
        .get("x")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing browser column.x: {layout}"));

    ((column_x + logical_x) as f64, (viewport_y + column_y) as f64)
}

fn native_window_click(harness: &TaborHarness, x: f64, y: f64) {
    let scale_factor = window_debug_state(harness).scale_factor;
    let physical_x = x * scale_factor;
    let physical_y = y * scale_factor;
    let reply = harness.send_raw_request(json!({
        "type": "window_debug_mouse_drag",
        "x0": physical_x,
        "y0": physical_y,
        "x1": physical_x,
        "y1": physical_y,
        "steps": 1
    }));
    assert_eq!(
        reply.get("type").and_then(Value::as_str),
        Some("ok"),
        "native window click failed: {reply}"
    );
}

fn native_click_caret_probe(
    harness: &TaborHarness,
    layout: &Value,
    inspector_session: &str,
    inspector_command_id: &mut i64,
) {
    let scrolled = inspector_eval_string(
        harness,
        inspector_session,
        inspector_command_id,
        r#"(() => {
            const el = document.getElementById("caret-input");
            if (!el) throw new Error("caret-input missing");
            el.scrollIntoView({ block: "center", inline: "center" });
            return "ok";
        })()"#,
    );
    assert_eq!(scrolled, "ok", "failed to scroll caret probe into view");
    thread::sleep(Duration::from_millis(100));

    let (logical_x, logical_y) = inspector_eval_point(
        harness,
        inspector_session,
        inspector_command_id,
        r#"(() => {
            const el = document.getElementById("caret-input");
            if (!el) throw new Error("caret-input missing");
            const rect = el.getBoundingClientRect();
            return JSON.stringify({
                x: Math.round(rect.left + rect.width / 2),
                y: Math.round(rect.top + rect.height / 2)
            });
        })()"#,
    );
    let (visual_x, visual_y) = browser_visual_point(layout, logical_x, logical_y);
    native_window_click(harness, visual_x, visual_y);
}

fn assert_caret_probe_focused(
    harness: &TaborHarness,
    tab_id_arg: &str,
    layout: &Value,
    inspector_session: &str,
    inspector_command_id: &mut i64,
    label: &str,
) {
    let mode_state =
        wait_for_tab_web_mode_value(harness, tab_id_arg, "insert", Duration::from_secs(4))
            .unwrap_or_else(|| {
                let latest_state = harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg]);
                panic!(
                    "timed out waiting for insert mode during {label}; last_state={latest_state}; layout={layout}; harness_log_tail:\n{}",
                    harness.log_tail(),
                )
            });
    assert_eq!(
        tab_web_mode(&mode_state),
        Some("insert"),
        "expected caret probe click to switch into insert mode during {label}: {mode_state}"
    );

    let caret_state = inspector_eval_json(
        harness,
        inspector_session,
        inspector_command_id,
        r#"(() => {
            const el = document.getElementById("caret-input");
            if (!el) throw new Error("caret-input missing");
            return JSON.stringify({
                active: document.activeElement === el,
                selectionStart: el.selectionStart,
                selectionEnd: el.selectionEnd,
                valueLength: el.value.length
            });
        })()"#,
    );
    assert_eq!(
        caret_state.get("active").and_then(Value::as_bool),
        Some(true),
        "caret probe input was not active during {label}: {caret_state}"
    );
    assert_eq!(
        caret_state.get("selectionStart").and_then(Value::as_u64),
        Some(0),
        "caret probe selectionStart drifted during {label}: {caret_state}"
    );
    assert_eq!(
        caret_state.get("selectionEnd").and_then(Value::as_u64),
        Some(0),
        "caret probe selectionEnd drifted during {label}: {caret_state}"
    );
    assert_eq!(
        caret_state.get("valueLength").and_then(Value::as_u64),
        Some(0),
        "caret probe value changed during {label}: {caret_state}"
    );
}

fn assert_visible_caret_probe(
    harness: &TaborHarness,
    layout: &Value,
    inspector_session: &str,
    inspector_command_id: &mut i64,
    label: &str,
) {
    let caret_rect = inspector_eval_rect(
        harness,
        inspector_session,
        inspector_command_id,
        r#"(() => {
            const el = document.getElementById("caret-input");
            if (!el) throw new Error("caret-input missing");
            const style = getComputedStyle(el);
            const rect = el.getBoundingClientRect();
            const paddingLeft = parseFloat(style.paddingLeft) || 0;
            const paddingTop = parseFloat(style.paddingTop) || 0;
            const paddingBottom = parseFloat(style.paddingBottom) || 0;
            return JSON.stringify({
                x: rect.left + paddingLeft - 1,
                y: rect.top + paddingTop,
                width: 8,
                height: Math.max(8, rect.height - paddingTop - paddingBottom)
            });
        })()"#,
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last_ratio = 0.0_f64;
    let mut last_column_ratio = 0.0_f64;
    let mut last_snapshot = None;

    while Instant::now() < deadline {
        let snapshot = window_debug_snapshot(harness);
        let image = decode_snapshot_rgba(&snapshot);
        let local =
            logical_rect_to_snapshot_pixels(layout, snapshot.state.scale_factor, caret_rect);
        if rect_within_snapshot(&local, snapshot.width, snapshot.height) {
            let ratio = sampled_red_ratio(&image, local);
            let column_ratio = max_red_column_ratio(&image, local);
            if column_ratio >= 0.55 {
                return;
            }
            last_ratio = ratio;
            last_column_ratio = column_ratio;
        }
        last_snapshot = Some(snapshot);
        thread::sleep(POLL_INTERVAL);
    }

    let snapshot = last_snapshot.unwrap_or_else(|| window_debug_snapshot(harness));
    let image = decode_snapshot_rgba(&snapshot);
    let local = logical_rect_to_snapshot_pixels(layout, snapshot.state.scale_factor, caret_rect);
    let (final_ratio, final_column_ratio) =
        if rect_within_snapshot(&local, snapshot.width, snapshot.height) {
            (sampled_red_ratio(&image, local), max_red_column_ratio(&image, local))
        } else {
            (0.0, 0.0)
        };
    let artifact = PathBuf::from("/tmp/tabor-caret-probe-window-snapshot.png");
    let png_bytes = BASE64
        .decode(snapshot.png_base64.as_bytes())
        .expect("failed to decode caret probe snapshot");
    std::fs::write(&artifact, png_bytes).unwrap_or_else(|err| {
        panic!("failed to write caret probe snapshot {}: {err}", artifact.display())
    });

    panic!(
        "red caret did not appear during {label}; last_ratio={last_ratio:.3}; last_column_ratio={last_column_ratio:.3}; final_ratio={final_ratio:.3}; final_column_ratio={final_column_ratio:.3}; caret_rect={caret_rect:?}; snapshot_rect={local:?}; snapshot={}; layout={layout}; harness_log_tail:\n{}",
        artifact.display(),
        harness.log_tail(),
    );
}

fn max_red_column_ratio(image: &image::RgbaImage, rect: SnapshotPixelRect) -> f64 {
    let height = (rect.y1 - rect.y0).max(0) as u64;
    if height == 0 {
        return 0.0;
    }

    let mut best = 0u64;
    for x in rect.x0..rect.x1 {
        let mut red = 0u64;
        for y in rect.y0..rect.y1 {
            let pixel = image.get_pixel(x as u32, y as u32);
            if pixel[0] >= 245 && pixel[1] <= 10 && pixel[2] <= 10 {
                red += 1;
            }
        }
        best = best.max(red);
    }

    best as f64 / height as f64
}

fn decode_snapshot_rgba(snapshot: &WindowDebugSnapshot) -> image::RgbaImage {
    let png_bytes =
        BASE64.decode(snapshot.png_base64.as_bytes()).expect("failed to decode snapshot PNG");
    image::load_from_memory(&png_bytes).expect("failed to decode window snapshot").to_rgba8()
}

fn logical_rect_to_snapshot_pixels(
    layout: &Value,
    scale_factor: f64,
    rect: LogicalRect,
) -> SnapshotPixelRect {
    let (x0, y0) = browser_visual_point(layout, rect.x.floor() as i64, rect.y.floor() as i64);
    let (x1, y1) = browser_visual_point(
        layout,
        (rect.x + rect.width).ceil() as i64,
        (rect.y + rect.height).ceil() as i64,
    );

    SnapshotPixelRect {
        x0: (x0 * scale_factor).floor() as i64,
        y0: (y0 * scale_factor).floor() as i64,
        x1: (x1 * scale_factor).ceil() as i64,
        y1: (y1 * scale_factor).ceil() as i64,
    }
}

#[allow(clippy::result_large_err)]
fn wait_for_fullscreen_window_state(
    harness: &TaborHarness,
    timeout: Duration,
) -> Result<WindowDebugState, WindowDebugState> {
    let deadline = Instant::now() + timeout;
    let mut last_state = window_debug_state(harness);
    let mut stable_count = 0usize;
    let mut last_signature = None;

    while Instant::now() < deadline {
        let state = window_debug_state(harness);
        last_state = state.clone();
        let has_auxiliary_rects = !rect_is_empty(&state.auxiliary_top_left_screen_points)
            || !rect_is_empty(&state.auxiliary_top_right_screen_points);
        let fullscreen_active =
            state.native_fullscreen || state.simple_fullscreen || state.winit_fullscreen;

        if fullscreen_active && has_auxiliary_rects {
            let signature = window_state_signature(&state);
            if last_signature == Some(signature) {
                stable_count += 1;
            } else {
                stable_count = 1;
                last_signature = Some(signature);
            }

            if stable_count >= 3 {
                return Ok(state);
            }
        } else {
            stable_count = 0;
            last_signature = None;
            let _ = window_debug_snapshot(harness);
        }

        thread::sleep(POLL_INTERVAL);
    }

    Err(last_state)
}

#[allow(clippy::result_large_err)]
fn wait_for_windowed_window_state(
    harness: &TaborHarness,
    timeout: Duration,
) -> Result<WindowDebugState, WindowDebugState> {
    let deadline = Instant::now() + timeout;
    let mut last_state = window_debug_state(harness);
    let mut stable_count = 0usize;
    let mut last_signature = None;

    while Instant::now() < deadline {
        let state = window_debug_state(harness);
        last_state = state.clone();
        let fullscreen_active =
            state.native_fullscreen || state.simple_fullscreen || state.winit_fullscreen;

        if !fullscreen_active {
            let signature = window_state_signature(&state);
            if last_signature == Some(signature) {
                stable_count += 1;
            } else {
                stable_count = 1;
                last_signature = Some(signature);
            }

            if stable_count >= 3 {
                return Ok(state);
            }
        } else {
            stable_count = 0;
            last_signature = None;
            let _ = window_debug_snapshot(harness);
        }

        thread::sleep(POLL_INTERVAL);
    }

    Err(last_state)
}

#[allow(clippy::result_large_err)]
fn wait_for_miniaturized_window_state(
    harness: &TaborHarness,
    timeout: Duration,
) -> Result<WindowDebugState, WindowDebugState> {
    let deadline = Instant::now() + timeout;
    let mut last_state = window_debug_state(harness);

    while Instant::now() < deadline {
        let state = window_debug_state(harness);
        last_state = state.clone();

        if state.is_miniaturized {
            return Ok(state);
        }

        thread::sleep(POLL_INTERVAL);
    }

    Err(last_state)
}

fn rect_is_empty(rect: &WindowDebugRect) -> bool {
    rect.width <= 0.0 || rect.height <= 0.0
}

fn assert_rect_close(
    actual: &WindowDebugRect,
    expected: &WindowDebugRect,
    tolerance: f64,
    label: &str,
) {
    let deltas = [
        (actual.x - expected.x).abs(),
        (actual.y - expected.y).abs(),
        (actual.width - expected.width).abs(),
        (actual.height - expected.height).abs(),
    ];
    if deltas.into_iter().any(|delta| delta > tolerance) {
        panic!("{label}: actual={actual:?} expected={expected:?} tolerance={tolerance}");
    }
}

fn window_state_signature(state: &WindowDebugState) -> (i64, i64, i64, i64, i64, i64, i64, i64) {
    let content = &state.content_frame_screen_points;
    let left = &state.auxiliary_top_left_screen_points;
    let right = &state.auxiliary_top_right_screen_points;
    (
        (content.x * 1000.0).round() as i64,
        (content.y * 1000.0).round() as i64,
        (content.width * 1000.0).round() as i64,
        (content.height * 1000.0).round() as i64,
        (left.width * 1000.0).round() as i64,
        (left.height * 1000.0).round() as i64,
        (right.width * 1000.0).round() as i64,
        (right.height * 1000.0).round() as i64,
    )
}

fn screen_rect_to_snapshot_pixels(
    rect: &WindowDebugRect,
    snapshot_screen_points: &WindowDebugRect,
    scale: f64,
) -> SnapshotPixelRect {
    let local_x = (rect.x - snapshot_screen_points.x) * scale;
    let local_y =
        (snapshot_screen_points.height - (rect.y - snapshot_screen_points.y) - rect.height) * scale;
    let local_width = rect.width * scale;
    let local_height = rect.height * scale;

    SnapshotPixelRect {
        x0: local_x.floor() as i64,
        y0: local_y.floor() as i64,
        x1: (local_x + local_width).ceil() as i64,
        y1: (local_y + local_height).ceil() as i64,
    }
}

fn rect_within_snapshot(rect: &SnapshotPixelRect, width: u32, height: u32) -> bool {
    rect.x0 >= 0
        && rect.y0 >= 0
        && rect.x1 > rect.x0
        && rect.y1 > rect.y0
        && rect.x1 <= i64::from(width)
        && rect.y1 <= i64::from(height)
}

fn sampled_red_ratio(image: &image::RgbaImage, rect: SnapshotPixelRect) -> f64 {
    let mut total = 0u64;
    let mut red = 0u64;

    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let pixel = image.get_pixel(x as u32, y as u32);
            total += 1;
            if pixel[0] >= 245 && pixel[1] <= 10 && pixel[2] <= 10 {
                red += 1;
            }
        }
    }

    if total == 0 { 0.0 } else { red as f64 / total as f64 }
}

fn write_notch_failure_artifacts(
    harness: &TaborHarness,
    snapshot: &WindowDebugSnapshot,
    state: &WindowDebugState,
    message: String,
) -> ! {
    let png_path = harness.tmp_path("fullscreen-notch-window-snapshot.png");
    let json_path = harness.tmp_path("fullscreen-notch-window-state.json");
    let png_bytes =
        BASE64.decode(snapshot.png_base64.as_bytes()).expect("failed to decode failure snapshot");
    std::fs::write(&png_path, png_bytes).unwrap_or_else(|err| {
        panic!("failed to write snapshot artifact {}: {err}", png_path.display())
    });
    std::fs::write(
        &json_path,
        serde_json::to_vec_pretty(&json!({
            "state": state,
            "snapshot": snapshot,
        }))
        .expect("failed to serialize snapshot artifact json"),
    )
    .unwrap_or_else(|err| panic!("failed to write state artifact {}: {err}", json_path.display()));

    panic!("{message}; snapshot={}; state={}", png_path.display(), json_path.display());
}

fn flatten_tabs(response: &Value) -> Vec<&Value> {
    let mut tabs = Vec::new();

    if let Some(groups) = response.get("groups").and_then(Value::as_array) {
        for group in groups {
            if let Some(group_tabs) = group.get("tabs").and_then(Value::as_array) {
                tabs.extend(group_tabs.iter());
            }
        }
    }

    tabs
}

fn tab_id_pair(tab: &Value) -> Option<(u64, u64)> {
    let tab_id = tab.get("tab_id")?;
    let index = tab_id.get("index")?.as_u64()?;
    let generation = tab_id.get("generation")?.as_u64()?;
    Some((index, generation))
}

fn first_terminal_tab_id(response: &Value) -> Option<(u64, u64)> {
    flatten_tabs(response)
        .into_iter()
        .find_map(|tab| if tab_kind_is(tab, "terminal") { tab_id_pair(tab) } else { None })
}

fn wait_for_active_terminal_program_name(
    harness: &TaborHarness,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let tabs = harness.run_json(["msg", "list-tabs"]);
        let Some(active) = active_tab(&tabs) else {
            thread::sleep(POLL_INTERVAL);
            continue;
        };

        if tab_kind_is(active, "terminal") {
            if let Some(name) = active.get("program_name").and_then(Value::as_str) {
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }

        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn wait_for_active_terminal_program_name_value(
    harness: &TaborHarness,
    expected: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let tabs = harness.run_json(["msg", "list-tabs"]);
        let Some(active) = active_tab(&tabs) else {
            thread::sleep(POLL_INTERVAL);
            continue;
        };

        if tab_kind_is(active, "terminal") {
            if let Some(name) = active.get("program_name").and_then(Value::as_str) {
                if name == expected {
                    return Some(name.to_string());
                }
            }
        }

        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn active_tab(response: &Value) -> Option<&Value> {
    flatten_tabs(response)
        .into_iter()
        .find(|tab| tab.get("is_active").and_then(Value::as_bool) == Some(true))
}

fn tab_web_url(tab: &Value) -> Option<&str> {
    tab.get("kind")
        .and_then(|value| value.get("web"))
        .and_then(|value| value.get("url"))
        .and_then(Value::as_str)
}

fn wait_for_active_web_url_value(
    harness: &TaborHarness,
    expected: &str,
    timeout: Duration,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let tabs = harness.run_json(["msg", "list-tabs"]);
        if let Some(active) = active_tab(&tabs)
            .filter(|tab| tab_kind_is(tab, "web") && tab_web_url(tab) == Some(expected))
        {
            return Some(active.clone());
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn wait_for_active_browser_layout_where<F>(
    harness: &TaborHarness,
    timeout: Duration,
    predicate: F,
) -> Option<Value>
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let tabs = harness.run_json(["msg", "list-tabs"]);
        if let Some(layout) = active_tab(&tabs)
            .filter(|tab| tab_kind_is(tab, "web"))
            .and_then(|tab| tab.get("browser_layout"))
            .filter(|layout| predicate(layout))
        {
            return Some(layout.clone());
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn tab_browser_layout(state: &Value) -> Option<&Value> {
    state.get("tab").and_then(|tab| tab.get("browser_layout"))
}

fn browser_layout_acceleration_state(layout: &Value) -> Option<&str> {
    layout.get("acceleration").and_then(|value| value.get("state")).and_then(Value::as_str)
}

fn wait_for_tab_browser_layout_where<F>(
    harness: &TaborHarness,
    tab_id_arg: &str,
    timeout: Duration,
    predicate: F,
) -> Option<Value>
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let state = harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg]);
        if let Some(layout) = tab_browser_layout(&state).filter(|layout| predicate(layout)) {
            return Some(layout.clone());
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn wait_for_tab_acceleration_settled(
    harness: &TaborHarness,
    tab_id_arg: &str,
    timeout: Duration,
) -> Option<Value> {
    wait_for_tab_browser_layout_where(harness, tab_id_arg, timeout, |layout| {
        matches!(browser_layout_acceleration_state(layout), Some("ready" | "failed"))
    })
}

fn wait_for_tab_web_mode_value(
    harness: &TaborHarness,
    tab_id_arg: &str,
    expected_mode: &str,
    timeout: Duration,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let state = harness.run_json(["msg", "get-tab-state", "--tab-id", tab_id_arg]);
        if tab_web_mode(&state) == Some(expected_mode) {
            return Some(state);
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn ensure_active_web_tab(harness: &TaborHarness, max_switches: usize) {
    for _ in 0..max_switches {
        let tabs = harness.run_json(["msg", "list-tabs"]);
        if let Some(active) = active_tab(&tabs) {
            if tab_kind_is(active, "web") {
                return;
            }
        }

        harness.run_ok(["msg", "select-tab", "--next"]);
        thread::sleep(Duration::from_millis(40));
    }

    let tabs = harness.run_json(["msg", "list-tabs"]);
    panic!("failed to activate a web tab before close: {tabs}");
}

fn assert_only_terminal_tab_remains(harness: &TaborHarness) {
    let tabs = harness.run_json(["msg", "list-tabs"]);
    let flat_tabs = flatten_tabs(&tabs);
    assert_eq!(flat_tabs.len(), 1, "expected one tab after closing web batch: {tabs}");
    assert!(
        tab_kind_is(flat_tabs[0], "terminal"),
        "expected remaining tab to be terminal after closing web batch: {tabs}"
    );
}

fn tab_kind_is(tab: &Value, kind: &str) -> bool {
    match tab.get("kind") {
        Some(Value::String(current_kind)) => current_kind == kind,
        Some(Value::Object(tab_kind)) => tab_kind.contains_key(kind),
        _ => false,
    }
}

fn tab_web_mode(state: &Value) -> Option<&str> {
    state.get("tab").and_then(|tab| tab.get("web_mode")).and_then(Value::as_str)
}

fn send_terminal_key(harness: &TaborHarness, tab_id: (u64, u64), key: &str, text: Option<&str>) {
    let payload = json!({
        "type": "terminal_key",
        "tab_id": {
            "index": tab_id.0,
            "generation": tab_id.1
        },
        "input": {
            "key": key,
            "text": text,
            "modifiers": {
                "shift": false,
                "control": false,
                "alt": false,
                "super_key": false
            },
            "repeat": false,
            "state": "down"
        }
    });
    let message = payload.to_string();
    harness.run_ok(["msg", "send", message.as_str()]);
}

fn runtime_metrics(harness: &TaborHarness) -> Value {
    let payload = json!({"type": "runtime_metrics"}).to_string();
    harness.run_json(["msg", "send", payload.as_str()])
}

fn webview_counts(metrics_response: &Value) -> (u64, u64, u64) {
    let metrics = metrics_response
        .get("metrics")
        .unwrap_or_else(|| panic!("missing metrics field: {metrics_response}"));
    let webview = metrics
        .get("webview")
        .unwrap_or_else(|| panic!("missing webview field: {metrics_response}"));

    let live = webview
        .get("live")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing webview.live: {metrics_response}"));
    let created = webview
        .get("created")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing webview.created: {metrics_response}"));
    let dropped = webview
        .get("dropped")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing webview.dropped: {metrics_response}"));

    (live, created, dropped)
}

fn webview_metric(metrics_response: &Value, key: &str) -> u64 {
    metrics_response
        .get("metrics")
        .and_then(|metrics| metrics.get("webview"))
        .and_then(|webview| webview.get(key))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing webview.{key}: {metrics_response}"))
}

fn webview_frame_delivery_mode(metrics_response: &Value) -> &str {
    metrics_response
        .get("metrics")
        .and_then(|metrics| metrics.get("webview"))
        .and_then(|webview| webview.get("frame_delivery_mode"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing webview.frame_delivery_mode: {metrics_response}"))
}

fn web_close_count(metrics_response: &Value) -> u64 {
    metrics_response
        .get("metrics")
        .and_then(|metrics| metrics.get("web_close"))
        .and_then(|close| close.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing web_close.count: {metrics_response}"))
}

fn wait_for_webview_counts(
    harness: &TaborHarness,
    expected: (u64, u64, u64),
    timeout: Duration,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let metrics = runtime_metrics(harness);
        if webview_counts(&metrics) == expected {
            return Some(metrics);
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn child_process_count(parent_pid: u32) -> usize {
    let output = Command::new("pgrep")
        .arg("-P")
        .arg(parent_pid.to_string())
        .output()
        .unwrap_or_else(|err| panic!("failed to run pgrep for pid {parent_pid}: {err}"));

    if !output.status.success() {
        return 0;
    }

    String::from_utf8_lossy(&output.stdout).lines().filter(|line| !line.trim().is_empty()).count()
}

fn wait_for_child_process_count_max(
    parent_pid: u32,
    max_count: usize,
    timeout: Duration,
) -> Option<usize> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let count = child_process_count(parent_pid);
        if count <= max_count {
            return Some(count);
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn wait_for_file_content(path: &Path, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path) {
            return Some(content);
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn wait_for_agent_observation(
    harness: &TaborHarness,
    title: &str,
    timeout: Duration,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let observation = harness.run_json(["agent", "observe"]);
        if observation
            .get("observation")
            .and_then(|value| value.get("title"))
            .and_then(Value::as_str)
            == Some(title)
        {
            return Some(observation);
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn wait_for_agent_download(
    harness: &TaborHarness,
    suggested_name: &str,
    timeout: Duration,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let downloads = harness.run_json(["agent", "downloads"]);
        if let Some(download) =
            downloads.get("downloads").and_then(Value::as_array).and_then(|entries| {
                entries.iter().find(|entry| {
                    entry.get("suggested_name").and_then(Value::as_str) == Some(suggested_name)
                        && entry.get("state").and_then(Value::as_str) == Some("complete")
                })
            })
        {
            return Some(download.clone());
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn wait_for_agent_events(
    harness: &TaborHarness,
    since: &str,
    kinds: &[&str],
    timeout: Duration,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    let expected_kinds = kinds.iter().map(|kind| kind.to_string()).collect::<Vec<_>>();
    while Instant::now() < deadline {
        let mut args = vec![
            String::from("agent"),
            String::from("events"),
            String::from("--since"),
            since.to_string(),
            String::from("--max"),
            String::from("64"),
        ];
        for kind in kinds {
            args.push(String::from("--kind"));
            args.push((*kind).to_string());
        }
        let events = harness.run_json(args);
        let returned_kinds = events
            .get("events")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        entry.get("kind").and_then(Value::as_str).map(ToOwned::to_owned)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if expected_kinds.iter().all(|kind| returned_kinds.iter().any(|value| value == kind)) {
            return Some(events);
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn tab_id_arg(response: &Value) -> String {
    let tab_id = response.get("tab_id").unwrap_or_else(|| panic!("missing tab_id: {response}"));
    let index = tab_id
        .get("index")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing tab_id.index: {response}"));
    let generation = tab_id
        .get("generation")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing tab_id.generation: {response}"));
    format!("{index}:{generation}")
}

fn attach_inspector(harness: &TaborHarness, tab_id_arg: &str) -> String {
    let reply = harness.run_json(["msg", "inspector", "attach", "--tab-id", tab_id_arg]);
    reply
        .get("session")
        .and_then(|value| value.get("session_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| panic!("missing inspector session_id: {reply}"))
}

fn inspector_eval_string(
    harness: &TaborHarness,
    session_id: &str,
    command_id: &mut i64,
    expression: &str,
) -> String {
    let response = inspector_command(
        harness,
        session_id,
        *command_id,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true
        }),
    );
    *command_id += 1;
    response
        .get("result")
        .and_then(|value| value.get("result"))
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| panic!("missing Runtime.evaluate string result: {response}"))
}

fn inspector_eval_json(
    harness: &TaborHarness,
    session_id: &str,
    command_id: &mut i64,
    expression: &str,
) -> Value {
    let raw = inspector_eval_string(harness, session_id, command_id, expression);
    serde_json::from_str(&raw).unwrap_or_else(|err| {
        panic!("invalid Runtime.evaluate json result: {err}; raw={raw}; expression={expression}")
    })
}

fn inspector_eval_point(
    harness: &TaborHarness,
    session_id: &str,
    command_id: &mut i64,
    expression: &str,
) -> (i64, i64) {
    let point = inspector_eval_json(harness, session_id, command_id, expression);
    let x = point
        .get("x")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing point.x: {point}"));
    let y = point
        .get("y")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing point.y: {point}"));
    (x, y)
}

fn inspector_eval_rect(
    harness: &TaborHarness,
    session_id: &str,
    command_id: &mut i64,
    expression: &str,
) -> LogicalRect {
    let rect = inspector_eval_json(harness, session_id, command_id, expression);
    serde_json::from_value(rect)
        .unwrap_or_else(|err| panic!("invalid Runtime.evaluate rect result: {err}"))
}

fn trusted_click(harness: &TaborHarness, session_id: &str, command_id: &mut i64, element_id: &str) {
    let detail = harness.run_json(["agent", "inspect", element_id]);
    let center = detail
        .get("element")
        .and_then(|value| value.get("center"))
        .unwrap_or_else(|| panic!("missing element center: {detail}"));
    let x = center
        .get("x")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing center.x: {detail}"));
    let y = center
        .get("y")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing center.y: {detail}"));

    inspector_command(
        harness,
        session_id,
        *command_id,
        "Input.dispatchMouseEvent",
        json!({"type":"mouseMoved","x":x,"y":y,"button":"none","buttons":0}),
    );
    *command_id += 1;
    inspector_command(
        harness,
        session_id,
        *command_id,
        "Input.dispatchMouseEvent",
        json!({"type":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1}),
    );
    *command_id += 1;
    inspector_command(
        harness,
        session_id,
        *command_id,
        "Input.dispatchMouseEvent",
        json!({"type":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}),
    );
    *command_id += 1;
}

fn inspector_command(
    harness: &TaborHarness,
    session_id: &str,
    id: i64,
    method: &str,
    params: Value,
) -> Value {
    let message = json!({ "id": id, "method": method, "params": params }).to_string();
    let _ = harness.run_json([
        "msg",
        "inspector",
        "send",
        "--session-id",
        session_id,
        "--message",
        message.as_str(),
    ]);

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let polled = harness.run_json([
            "msg",
            "inspector",
            "poll",
            "--session-id",
            session_id,
            "--max",
            "32",
        ]);
        let messages = polled
            .get("messages")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("missing inspector messages: {polled}"));
        for message in messages {
            let payload = message
                .get("payload")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("missing inspector payload: {polled}"));
            let payload: Value = serde_json::from_str(payload)
                .unwrap_or_else(|err| panic!("invalid inspector payload: {err}; raw={payload}"));
            if payload.get("id").and_then(Value::as_i64) == Some(id) {
                if payload.get("error").is_some() {
                    panic!("inspector command failed: {payload}");
                }
                return payload;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }

    panic!("timed out waiting for inspector response id={id} method={method}");
}

fn wait_for_clipboard_text(
    harness: &TaborHarness,
    expected: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let clipboard = harness.run_json(["agent", "clipboard", "get"]);
        let text = clipboard.get("text").and_then(Value::as_str);
        if text == Some(expected) {
            return text.map(ToOwned::to_owned);
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn wait_for_agent_value(
    harness: &TaborHarness,
    element_id: &str,
    expected: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let detail = harness.run_json(["agent", "inspect", element_id]);
        let value = agent_detail_value(&detail);
        if value == Some(expected) {
            return value.map(ToOwned::to_owned);
        }
        thread::sleep(POLL_INTERVAL);
    }

    None
}

fn fixture_url() -> String {
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent-fixture.html");
    assert!(fixture_path.exists(), "fixture missing: {}", fixture_path.display());
    Url::from_file_path(&fixture_path).expect("invalid fixture file path").to_string()
}

fn tab_titles(response: &Value) -> Vec<String> {
    let mut titles = Vec::new();

    if let Some(groups) = response.get("groups").and_then(Value::as_array) {
        for group in groups {
            if let Some(tabs) = group.get("tabs").and_then(Value::as_array) {
                for tab in tabs {
                    if let Some(title) = tab.get("title").and_then(Value::as_str) {
                        titles.push(title.to_string());
                    }
                }
            }
        }
    }

    titles
}

fn find_observed_element_id(response: &Value, needle: &str) -> String {
    response
        .get("observation")
        .and_then(|value| value.get("elements"))
        .and_then(Value::as_array)
        .and_then(|elements| {
            elements.iter().find_map(|element| {
                let name = element.get("name").and_then(Value::as_str)?;
                if name == needle {
                    element.get("id").and_then(Value::as_str).map(ToOwned::to_owned)
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| panic!("missing observed element {needle}: {response}"))
}

fn agent_action_results_all_ok(response: &Value) -> bool {
    response
        .get("result")
        .and_then(|value| value.get("results"))
        .and_then(Value::as_array)
        .is_some_and(|results| {
            !results.is_empty()
                && results
                    .iter()
                    .all(|result| result.get("ok").and_then(Value::as_bool) == Some(true))
        })
}

fn agent_detail_value(response: &Value) -> Option<&str> {
    response.get("element").and_then(|value| value.get("value")).and_then(Value::as_str)
}

fn agent_detail_checked(response: &Value) -> Option<bool> {
    response.get("element").and_then(|value| value.get("checked")).and_then(Value::as_bool)
}

fn click_popup_opener(harness: &TaborHarness) {
    let observation = harness.run_json(["agent", "observe"]);
    let button_id = find_observed_element_id(&observation, "Open popup");
    let actions = json!([{ "type": "click", "id": button_id }]).to_string();
    let reply = harness.run_json(["agent", "act", actions.as_str()]);
    assert!(agent_action_results_all_ok(&reply), "popup click failed: {reply}");
}

fn opener_html() -> &'static str {
    r#"<!doctype html>
<title>popup-opener</title>
<style>
  html, body { margin: 0; padding: 0; }
  #open-popup {
    position: fixed;
    left: 0;
    top: 0;
    width: 180px;
    height: 120px;
    border: 0;
    font: 16px/1 sans-serif;
    background: #0b74de;
    color: #fff;
  }
</style>
<button id="open-popup" type="button">Open popup</button>
<script>
let got = false;
window.addEventListener("message", (event) => {
  if (event.data === "popup-ok") {
    got = true;
    document.title = "popup-ok";
  }
});
function openPopup() {
  const popup = window.open("", "_blank", "width=400,height=400");
  if (!popup) {
    document.title = "popup-blocked";
    return;
  }
  const iconUrl = window.location.origin + "/popup-icon.png";
  const popupHtml = [
    '<!doctype html>',
    '<title>popup</title>',
    '<link rel="icon" href="' + iconUrl + '">',
    '<script>',
    'try {',
    '  fetch("' + iconUrl + '", { cache: "no-store" }).catch(() => {});',
    '  if (!window.opener) {',
    '    document.title = "popup-no-opener";',
    '  } else {',
    '    window.opener.postMessage("popup-ok", "*");',
    '    document.title = "popup-sent";',
    '  }',
    '} catch (err) {',
    '  document.title = "popup-error";',
    '}',
    '</' + 'script>',
  ].join('');

  popup.document.open();
  popup.document.write(popupHtml);
  popup.document.close();
  setTimeout(() => {
    if (!got && document.title !== "popup-ok") {
      document.title = "popup-timeout";
    }
  }, 2000);
}
window.onload = () => {
  document.title = "popup-ready";
  const button = document.getElementById("open-popup");
  button.addEventListener("click", () => {
    document.title = "popup-clicked";
    openPopup();
  });
};
</script>
"#
}

fn popup_icon() -> Vec<u8> {
    BASE64
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR4nGNgYAAAAAMAASsJTYQAAAAASUVORK5CYII=")
        .expect("invalid icon base64")
}

fn handle_popup_connection(
    stream: &mut TcpStream,
    hits: &Arc<Mutex<Vec<String>>>,
    opener: &str,
    icon: &[u8],
) {
    let mut buf = [0u8; 4096];
    let read = match stream.read(&mut buf) {
        Ok(size) => size,
        Err(_) => return,
    };

    if read == 0 {
        return;
    }

    let request = String::from_utf8_lossy(&buf[..read]);
    let path =
        request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");

    hits.lock().expect("failed to lock path hits").push(path.to_string());

    let (status_line, content_type, body): (&str, &str, Vec<u8>) = match path {
        "/opener.html" => {
            ("HTTP/1.1 200 OK", "text/html; charset=utf-8", opener.as_bytes().to_vec())
        },
        "/popup-icon.png" => ("HTTP/1.1 200 OK", "image/png", icon.to_vec()),
        _ => ("HTTP/1.1 404 Not Found", "text/plain; charset=utf-8", b"not found".to_vec()),
    };

    let header = format!(
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn handle_clipboard_fixture_connection(stream: &mut TcpStream, fixture_html: &[u8]) {
    let mut buf = [0u8; 4096];
    let read = match stream.read(&mut buf) {
        Ok(size) => size,
        Err(_) => return,
    };

    if read == 0 {
        return;
    }

    let request = String::from_utf8_lossy(&buf[..read]);
    let path =
        request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");

    let (status_line, content_type, body): (&str, &str, &[u8]) = match path {
        "/" | "/fixture.html" => ("HTTP/1.1 200 OK", "text/html; charset=utf-8", fixture_html),
        _ => ("HTTP/1.1 404 Not Found", "text/plain; charset=utf-8", b"not found"),
    };

    let header = format!(
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn handle_media_fixture_connection(stream: &mut TcpStream, fixture_html: &[u8]) {
    let mut buf = [0u8; 4096];
    let read = match stream.read(&mut buf) {
        Ok(size) => size,
        Err(_) => return,
    };

    if read == 0 {
        return;
    }

    let request = String::from_utf8_lossy(&buf[..read]);
    let path =
        request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");

    let (status_line, content_type, body): (&str, &str, &[u8]) = match path {
        "/" | "/fixture.html" => ("HTTP/1.1 200 OK", "text/html; charset=utf-8", fixture_html),
        _ => ("HTTP/1.1 404 Not Found", "text/plain; charset=utf-8", b"not found"),
    };

    let header = format!(
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}
