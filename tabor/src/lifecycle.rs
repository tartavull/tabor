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

fn previous_unfinished_process() -> Option<u64> {
    let mut completed_after = Vec::new();
    for event in read_lifecycle_events()?.into_iter().rev() {
        let event_name = event.get("event")?.as_str()?;
        let pid = event.get("pid")?.as_u64()?;
        if matches!(event_name, "process_return" | "panic") {
            completed_after.push(pid);
            continue;
        }
        if event_name == "process_start" {
            return (!completed_after.contains(&pid)).then_some(pid);
        }
    }
    None
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

    #[test]
    fn process_start_marks_previous_process_without_completion() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tempdir = tempdir().expect("tempdir");
        let _home = EnvVarGuard::set("HOME", tempdir.path());
        let path = lifecycle_log_path().expect("lifecycle log path");
        fs::create_dir_all(path.parent().expect("lifecycle log parent"))
            .expect("create lifecycle log parent");
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({
                    "unix_time_ms": 1_u64,
                    "pid": 4242_u64,
                    "event": "process_start",
                    "detail": "",
                })
            ),
        )
        .expect("seed lifecycle log");

        record_process_start();

        let events = read_events();
        assert!(
            events.iter().any(|event| {
                event["event"] == "previous_process_unfinished"
                    && event["previous_pid"].as_u64() == Some(4242)
            }),
            "missing previous_process_unfinished event in {events:#?}"
        );
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
