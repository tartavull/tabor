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
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("failed to spawn tabor");

        let harness = Self { bin, socket, _tmp: tmp, child };

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
        let stdout = self.run_ok(args);
        serde_json::from_str(&stdout)
            .unwrap_or_else(|_| panic!("invalid json output from tabor: {stdout}"))
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
fn agent_browser_fixture_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();

    harness.run_ok(["agent-browser", "open", fixture.as_str()]);
    harness.run_ok(["agent-browser", "wait", "--load", "domcontentloaded"]);

    harness.run_ok(["agent-browser", "fill", "#email-input", "test@example.com"]);
    assert_eq!(
        harness.run_ok(["agent-browser", "get", "value", "#email-input"]),
        "test@example.com"
    );

    harness.run_ok(["agent-browser", "type", "#notes", "hello"]);
    assert_eq!(harness.run_ok(["agent-browser", "get", "value", "#notes"]), "hello");

    harness.run_ok(["agent-browser", "check", "#check-me"]);
    assert_eq!(harness.run_ok(["agent-browser", "is", "checked", "#check-me"]), "true");

    harness.run_ok(["agent-browser", "uncheck", "#check-me"]);
    assert_eq!(harness.run_ok(["agent-browser", "is", "checked", "#check-me"]), "false");

    harness.run_ok(["agent-browser", "tab", "new", fixture.as_str()]);
    harness.run_ok(["agent-browser", "tab", "list"]);
    harness.run_ok(["agent-browser", "tab", "close"]);
}

#[test]
fn web_popup_smoke() {
    let server = PopupServer::start();
    let harness = TaborHarness::start();

    let opener_url = server.url("/opener.html");
    let reply = harness.run_json(["msg", "create-tab", "--web", opener_url.as_str()]);
    assert_eq!(reply.get("type").and_then(Value::as_str), Some("tab_created"));

    let success_titles = ["popup-sent", "popup-ok"];
    let failure_titles = ["popup-no-opener", "popup-error", "popup-blocked", "popup-timeout"];

    let deadline = Instant::now() + Duration::from_secs(10);
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
fn browser_clipboard_shortcut_smoke() {
    let harness = TaborHarness::start();
    let fixture = fixture_url();
    let expected = "copy-fragment";

    harness.run_ok(["agent-browser", "open", fixture.as_str()]);
    harness.run_ok(["agent-browser", "wait", "--load", "domcontentloaded"]);

    let selected = harness.run_ok([
        "agent-browser",
        "eval",
        "(() => {\n  const source = document.querySelector('#email-input');\n  const selected = 'copy-fragment';\n  source.value = 'start ' + selected + ' end';\n  source.focus();\n  const start = 6;\n  source.setSelectionRange(start, start + selected.length);\n  return source.value.slice(source.selectionStart, source.selectionEnd);\n})()",
    ]);
    assert_eq!(selected, expected);

    harness.run_ok(["agent-browser", "press", "Meta+c"]);
    harness.run_ok([
        "agent-browser",
        "eval",
        "(() => {\n  const destination = document.querySelector('#notes');\n  destination.value = '';\n  destination.focus();\n  destination.setSelectionRange(0, 0);\n  return destination.id;\n})()",
    ]);
    harness.run_ok(["agent-browser", "press", "Meta+v"]);

    let destination = harness.run_ok(["agent-browser", "get", "value", "#notes"]);
    assert_eq!(destination, expected, "browser-tab clipboard shortcut path failed");
}

fn fixture_url() -> String {
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent-browser.html");
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

fn click_popup_opener(harness: &TaborHarness) {
    let payload = json!({
        "type": "web_mouse",
        "tab_id": null,
        "action": "click",
        "x": 48.0,
        "y": 48.0,
        "button": "left",
    })
    .to_string();
    harness.run_ok(["msg", "send", payload.as_str()]);
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
