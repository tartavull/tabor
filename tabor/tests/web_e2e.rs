#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
use serde_json::{Value, json};
use tempfile::TempDir;
use url::Url;

const START_TIMEOUT: Duration = Duration::from_secs(12);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

struct TaborHarness {
    bin: PathBuf,
    socket: PathBuf,
    _tmp: TempDir,
    log_path: PathBuf,
    child: Child,
}

impl TaborHarness {
    fn start() -> Self {
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_tabor"));
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let socket = tmp.path().join("tabor.sock");
        let log_path = tmp.path().join("tabor.log");

        let stdout = File::create(&log_path).expect("failed to create harness log file");
        let stderr = stdout.try_clone().expect("failed to clone harness log file");

        let child = Command::new(&bin)
            .arg("--socket")
            .arg(&socket)
            .env("TABOR_BACKGROUND", "1")
            .env("TABOR_WEBVIEW_ENGINE", "cef")
            .env("RUST_BACKTRACE", "1")
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("failed to spawn tabor");

        let harness = Self { bin, socket, _tmp: tmp, log_path: log_path.clone(), child };

        let start = Instant::now();
        while start.elapsed() < START_TIMEOUT {
            if harness.socket.exists() && harness.run_checked(["msg", "ping"]).is_ok() {
                return harness;
            }
            thread::sleep(POLL_INTERVAL);
        }

        let log = std::fs::read_to_string(log_path).unwrap_or_else(|_| String::new());
        panic!("failed to start tabor harness in background; log:\n{log}");
    }

    fn run_output<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        Command::new(&self.bin)
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
}

impl Drop for TaborHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

#[test]
fn agent_fixture_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);

    let observation = harness.run_json(["agent", "observe"]);
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
fn agent_wait_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);
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
    let file_input_id = find_observed_element_id(&first_observation, "file-input");

    let scroll = json!([{ "type": "scroll", "dy": 320 }]).to_string();
    let scroll_reply = harness.run_json(["agent", "act", scroll.as_str()]);
    assert!(agent_action_results_all_ok(&scroll_reply), "agent scroll failed: {scroll_reply}");

    let second_observation = harness.run_json(["agent", "observe"]);
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
fn agent_events_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    let reply = harness.run_json(["msg", "create-tab", "--web", fixture.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    harness.run_json(["agent", "attach"]);
    harness.run_json(["agent", "use", "--active"]);

    let baseline = harness.run_json(["agent", "events", "--max", "1"]);
    let since = baseline.get("last_event_id").and_then(Value::as_u64).unwrap_or(0).to_string();

    let first_observation =
        wait_for_agent_observation(&harness, "Agent Browser Fixture", Duration::from_secs(6))
            .unwrap_or_else(|| panic!("timed out waiting for initial agent observation"));
    let fetch_id = find_observed_element_id(&first_observation, "Fetch data");

    let scroll = json!([{ "type": "scroll", "dy": 320 }]).to_string();
    let scroll_reply = harness.run_json(["agent", "act", scroll.as_str()]);
    assert!(agent_action_results_all_ok(&scroll_reply), "agent scroll failed: {scroll_reply}");

    let second_observation = harness.run_json(["agent", "observe"]);
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
