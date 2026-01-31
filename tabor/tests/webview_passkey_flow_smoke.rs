#![cfg(target_os = "macos")]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires user interaction for passkey creation"]
fn webview_passkey_flow_smoke() {
    let exe = env!("CARGO_BIN_EXE_webview_passkey_flow_smoke");
    let mut child = Command::new(exe)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn webview_passkey_flow_smoke");

    let deadline = Instant::now() + Duration::from_secs(140);
    loop {
        if let Some(status) = child.try_wait().expect("failed to poll webview_passkey_flow_smoke") {
            assert!(status.success(), "webview_passkey_flow_smoke exited with {status}");
            return;
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("webview_passkey_flow_smoke timed out");
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
