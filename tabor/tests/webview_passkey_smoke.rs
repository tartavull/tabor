#![cfg(target_os = "macos")]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn webview_passkey_smoke() {
    let exe = env!("CARGO_BIN_EXE_webview_passkey_smoke");
    let mut child = Command::new(exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn webview_passkey_smoke");
    let mut stdout = child.stdout.take().expect("failed to capture stdout");
    let mut stderr = child.stderr.take().expect("failed to capture stderr");

    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(status) = child.try_wait().expect("failed to poll webview_passkey_smoke") {
            let mut output = String::new();
            let mut errors = String::new();
            let _ = stdout.read_to_string(&mut output);
            let _ = stderr.read_to_string(&mut errors);
            let combined = format!("{output}{errors}");
            if status.success() {
                return;
            }
            if combined.contains("Passkey platform authenticator unavailable")
                || combined.contains("WebAuthn unsupported")
            {
                eprintln!("webview_passkey_smoke skipped: {}", combined.trim());
                return;
            }
            panic!("webview_passkey_smoke exited with {status}: {combined}");
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("webview_passkey_smoke timed out");
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
