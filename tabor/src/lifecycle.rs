use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

pub(crate) fn install_panic_hook() {
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        record_event("panic", panic_info.to_string());
        previous_hook(panic_info);
    }));
}

pub(crate) fn record_process_start() {
    if let Some(previous_pid) = previous_unfinished_process() {
        record_event_with_fields("previous_process_unfinished", "", |record| {
            record.insert("previous_pid".to_string(), Value::from(previous_pid));
        });
    }
    record_event("process_start", "");
}

pub(crate) fn record_process_return(detail: &str) {
    record_event("process_return", detail);
}

pub(crate) fn record_exit_requested(reason: &str) {
    record_event("exit_requested", reason);
}

pub(crate) fn record_event_loop_exiting() {
    if !current_process_has_exit_request() {
        record_event("unexpected_event_loop_exit", "");
    }
    record_event("event_loop_exiting", "");
}

#[derive(Clone, Copy)]
struct PreviousProcessLifecycle {
    pid: u64,
    process_returned_cleanly: Option<bool>,
    panicked: bool,
    unexpected_event_loop_exit: bool,
}

impl PreviousProcessLifecycle {
    fn should_restore_workspace(self) -> bool {
        if process_is_running(self.pid) {
            return false;
        }

        self.panicked
            || self.unexpected_event_loop_exit
            || !self.process_returned_cleanly.unwrap_or(false)
    }

    fn is_unfinished(self) -> bool {
        !process_is_running(self.pid) && self.process_returned_cleanly.is_none() && !self.panicked
    }
}

fn record_event(event: &str, detail: impl AsRef<str>) {
    record_event_with_fields(event, detail, |_| {});
}

fn record_event_with_fields(
    event: &str,
    detail: impl AsRef<str>,
    extend: impl FnOnce(&mut Map<String, Value>),
) {
    let Some(path) = lifecycle_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let unix_time_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let mut record = Map::new();
    record.insert("unix_time_ms".to_string(), Value::from(unix_time_ms as u64));
    record.insert("pid".to_string(), Value::from(process::id()));
    record.insert("event".to_string(), Value::from(event));
    record.insert("detail".to_string(), Value::from(detail.as_ref()));
    extend(&mut record);
    let record = Value::Object(record);

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{record}");
    }
}

pub(crate) fn should_restore_workspace_after_start() -> bool {
    let Some(events) = read_lifecycle_events() else {
        return false;
    };

    latest_previous_process_lifecycle(&events)
        .is_some_and(PreviousProcessLifecycle::should_restore_workspace)
}

fn latest_previous_process_lifecycle(events: &[Value]) -> Option<PreviousProcessLifecycle> {
    let current_pid = u64::from(process::id());
    let cutoff = events
        .iter()
        .rposition(|event| {
            event.get("pid").and_then(Value::as_u64) == Some(current_pid)
                && event.get("event").and_then(Value::as_str) == Some("process_start")
        })
        .unwrap_or(events.len());
    let prior_events = &events[..cutoff];

    let (start_index, pid) = prior_events.iter().enumerate().rev().find_map(|(index, event)| {
        (event.get("event").and_then(Value::as_str) == Some("process_start"))
            .then(|| event.get("pid").and_then(Value::as_u64).map(|pid| (index, pid)))
            .flatten()
    })?;

    let mut lifecycle = PreviousProcessLifecycle {
        pid,
        process_returned_cleanly: None,
        panicked: false,
        unexpected_event_loop_exit: false,
    };

    for event in &prior_events[start_index + 1..] {
        if event.get("pid").and_then(Value::as_u64) != Some(pid) {
            continue;
        }

        match event.get("event").and_then(Value::as_str) {
            Some("process_return") => {
                lifecycle.process_returned_cleanly =
                    Some(event.get("detail").and_then(Value::as_str) == Some("ok"));
            },
            Some("panic") => lifecycle.panicked = true,
            Some("unexpected_event_loop_exit") => lifecycle.unexpected_event_loop_exit = true,
            _ => {},
        }
    }

    Some(lifecycle)
}

fn process_is_running(pid: u64) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }

        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }

        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn previous_unfinished_process() -> Option<u64> {
    let events = read_lifecycle_events()?;
    let lifecycle = latest_previous_process_lifecycle(&events)?;
    lifecycle.is_unfinished().then_some(lifecycle.pid)
}

fn current_process_has_exit_request() -> bool {
    let Some(events) = read_lifecycle_events() else {
        return false;
    };
    let current_pid = u64::from(process::id());
    for event in events.into_iter().rev() {
        let Some(pid) = event.get("pid").and_then(Value::as_u64) else {
            continue;
        };
        if pid != current_pid {
            continue;
        }
        let Some(event_name) = event.get("event").and_then(Value::as_str) else {
            continue;
        };
        if event_name == "exit_requested" {
            return true;
        }
        if event_name == "process_start" {
            return false;
        }
    }
    false
}

fn read_lifecycle_events() -> Option<Vec<Value>> {
    let path = lifecycle_log_path()?;
    let content = fs::read_to_string(path).ok()?;
    Some(content.lines().filter_map(|line| serde_json::from_str(line).ok()).collect())
}

fn lifecycle_log_path() -> Option<PathBuf> {
    Some(
        PathBuf::from(env::var_os("HOME")?)
            .join("Library")
            .join("Application Support")
            .join("Tabor")
            .join("lifecycle.jsonl"),
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{LazyLock, Mutex};

    use serde_json::Value;
    use tempfile::tempdir;

    use super::*;

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let original = env::var_os(key);
            unsafe { env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(value) => env::set_var(self.key, value),
                    None => env::remove_var(self.key),
                }
            }
        }
    }

    fn read_events() -> Vec<Value> {
        let path = lifecycle_log_path().expect("lifecycle log path");
        let content = fs::read_to_string(path).expect("read lifecycle log");
        content
            .lines()
            .map(|line| serde_json::from_str(line).expect("parse lifecycle event"))
            .collect()
    }

    fn dead_pid() -> u64 {
        i32::MAX as u64 + 1
    }

    fn write_events(events: &[Value]) {
        let path = lifecycle_log_path().expect("lifecycle log path");
        fs::create_dir_all(path.parent().expect("lifecycle log parent"))
            .expect("create lifecycle log parent");
        let content = events.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        fs::write(&path, format!("{content}\n")).expect("write lifecycle events");
    }

    fn lifecycle_event(pid: u64, event: &str, detail: &str) -> Value {
        serde_json::json!({
            "unix_time_ms": 1_u64,
            "pid": pid,
            "event": event,
            "detail": detail,
        })
    }

    #[test]
    fn process_start_marks_previous_process_without_completion() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tempdir = tempdir().expect("tempdir");
        let _home = EnvVarGuard::set("HOME", tempdir.path());
        let previous_pid = dead_pid();
        write_events(&[lifecycle_event(previous_pid, "process_start", "")]);

        record_process_start();

        let events = read_events();
        assert!(
            events.iter().any(|event| {
                event["event"] == "previous_process_unfinished"
                    && event["previous_pid"].as_u64() == Some(previous_pid)
            }),
            "missing previous_process_unfinished event in {events:#?}"
        );
    }

    #[test]
    fn workspace_restore_skips_clean_previous_process() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tempdir = tempdir().expect("tempdir");
        let _home = EnvVarGuard::set("HOME", tempdir.path());
        let previous_pid = dead_pid();
        write_events(&[
            lifecycle_event(previous_pid, "process_start", ""),
            lifecycle_event(previous_pid, "exit_requested", "last_window_closed"),
            lifecycle_event(previous_pid, "event_loop_exiting", ""),
            lifecycle_event(previous_pid, "process_return", "ok"),
        ]);

        record_process_start();

        assert!(!should_restore_workspace_after_start());
    }

    #[test]
    fn workspace_restore_detects_dead_unfinished_previous_process() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tempdir = tempdir().expect("tempdir");
        let _home = EnvVarGuard::set("HOME", tempdir.path());
        let previous_pid = dead_pid();
        write_events(&[lifecycle_event(previous_pid, "process_start", "")]);

        record_process_start();

        assert!(should_restore_workspace_after_start());
    }

    #[test]
    fn workspace_restore_detects_previous_panic() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tempdir = tempdir().expect("tempdir");
        let _home = EnvVarGuard::set("HOME", tempdir.path());
        let previous_pid = dead_pid();
        write_events(&[
            lifecycle_event(previous_pid, "process_start", ""),
            lifecycle_event(previous_pid, "panic", "boom"),
        ]);

        record_process_start();

        assert!(should_restore_workspace_after_start());
    }

    #[test]
    fn workspace_restore_detects_unexpected_event_loop_exit() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tempdir = tempdir().expect("tempdir");
        let _home = EnvVarGuard::set("HOME", tempdir.path());
        let previous_pid = dead_pid();
        write_events(&[
            lifecycle_event(previous_pid, "process_start", ""),
            lifecycle_event(previous_pid, "unexpected_event_loop_exit", ""),
            lifecycle_event(previous_pid, "event_loop_exiting", ""),
            lifecycle_event(previous_pid, "process_return", "ok"),
        ]);

        record_process_start();

        assert!(should_restore_workspace_after_start());
    }

    #[test]
    fn workspace_restore_skips_previous_process_that_is_still_running() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tempdir = tempdir().expect("tempdir");
        let _home = EnvVarGuard::set("HOME", tempdir.path());
        let previous_pid = u64::from(process::id());
        write_events(&[lifecycle_event(previous_pid, "process_start", "")]);

        record_process_start();

        assert!(!should_restore_workspace_after_start());
        assert!(read_events().iter().all(|event| event["event"] != "previous_process_unfinished"));
    }

    #[test]
    fn event_loop_exit_without_exit_request_is_marked_unexpected() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tempdir = tempdir().expect("tempdir");
        let _home = EnvVarGuard::set("HOME", tempdir.path());

        record_process_start();
        record_event_loop_exiting();

        let events = read_events();
        assert!(
            events.iter().any(|event| event["event"] == "unexpected_event_loop_exit"),
            "missing unexpected_event_loop_exit event in {events:#?}"
        );
    }
}
