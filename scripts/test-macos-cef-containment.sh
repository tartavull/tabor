#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This containment harness requires macOS." >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
run_id="r$(date +%Y%m%d%H%M%S)$$"
bundle_id="com.pinkbot.tabor.test.cef-containment.${run_id}"
app_path="/Applications/Tabor CEF Containment Test ${run_id}.app"
executable_name="TaborCefContainmentTest_${run_id}"
main_binary="$app_path/Contents/MacOS/$executable_name"
web_host_app="$app_path/Contents/Frameworks/Tabor Web Host.app"
web_host_binary="$web_host_app/Contents/MacOS/Tabor Web Host"
artifact_dir="$repo_root/target/cef-containment/$run_id"
build_target_dir="${TABOR_CONTAINMENT_TARGET_DIR:-$repo_root/target/cef-containment-build}"
run_root="$(mktemp -d "/tmp/tabor-cef-containment-${run_id}.XXXXXX")"
state_root="$run_root/state"
socket="$state_root/tabor.sock"
log_path="$artifact_dir/tabor.log"
test_pid=""
open_pid=""
protected_pid="${TABOR_PROTECTED_PID:-}"
protected_command=""
pinned_cef_version="$(tr -d '\n' < "$repo_root/cef-version.txt")"
expected_cef_version="${pinned_cef_version%%+*}.0"

if [[ -z "$protected_pid" ]]; then
  protected_pid="$(ps -axo pid=,command= | awk '$2 ~ /^\/Applications\/Tabor\.app\/Contents\/MacOS\// && !found { print $1; found = 1 }')"
fi

assert_protected_unchanged() {
  if [[ -z "$protected_pid" ]]; then
    return
  fi
  kill -0 "$protected_pid"
  local command
  command="$(ps -p "$protected_pid" -o command=)"
  [[ "$command" == "$protected_command" ]]
}

validate_test_pid() {
  [[ -n "$test_pid" ]]
  [[ "$test_pid" != "$protected_pid" ]]
  local command
  command="$(ps -p "$test_pid" -o command=)"
  [[ "$command" == "$main_binary"* ]]
}

stop_test_process() {
  if [[ -n "$test_pid" ]] && kill -0 "$test_pid" 2>/dev/null; then
    validate_test_pid
    kill -TERM "$test_pid"
    for _ in {1..100}; do
      kill -0 "$test_pid" 2>/dev/null || break
      sleep 0.05
    done
    if kill -0 "$test_pid" 2>/dev/null; then
      validate_test_pid
      kill -KILL "$test_pid"
    fi
  fi
  test_pid=""
  if [[ -n "$open_pid" ]]; then
    wait "$open_pid" 2>/dev/null || true
  fi
  open_pid=""
}

cleanup() {
  stop_test_process
  assert_protected_unchanged
  while IFS= read -r host_log; do
    cp -f "$host_log" "$artifact_dir/$(basename "$host_log")"
  done < <(find "$run_root" -type f -name 'tabor-cef-host-*.log' 2>/dev/null | sort)
  if [[ "$app_path" == "/Applications/Tabor CEF Containment Test ${run_id}.app" ]]; then
    rm -rf -- "$app_path"
  fi
  if [[ "$run_root" == /tmp/tabor-cef-containment-${run_id}.* ]]; then
    rm -rf -- "$run_root"
  fi
}
trap cleanup EXIT

if [[ -n "$protected_pid" ]]; then
  protected_command="$(ps -p "$protected_pid" -o command=)"
  [[ "$protected_command" == /Applications/Tabor.app/Contents/MacOS/* ]]
fi
assert_protected_unchanged

cef_root="${CEF_PATH:-}"
if [[ -z "$cef_root" ]]; then
  echo "Set CEF_PATH to the repo-pinned CEF 151 runtime." >&2
  exit 1
fi

cef_framework=""
for candidate in \
  "$cef_root/Release/Chromium Embedded Framework.framework" \
  "$cef_root/Chromium Embedded Framework.framework"; do
  if [[ -d "$candidate" ]]; then
    cef_framework="$candidate"
    break
  fi
done
if [[ -z "$cef_framework" ]]; then
  echo "CEF framework not found below $cef_root" >&2
  exit 1
fi
cef_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
  "$cef_framework/Resources/Info.plist")"
if [[ "$cef_version" != "$expected_cef_version" ]]; then
  echo "Expected CEF $expected_cef_version, found $cef_version at $cef_framework" >&2
  exit 1
fi

mkdir -p "$artifact_dir" "$state_root" "$app_path/Contents/MacOS" \
  "$app_path/Contents/Resources"
cd "$repo_root"
CEF_PATH="$cef_root" CARGO_TARGET_DIR="$build_target_dir" \
  cargo build -p tabor --bin tabor
cp extra/osx/Tabor.Info.plist "$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $bundle_id" \
  "$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleExecutable $executable_name" \
  "$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleName Tabor CEF Containment Test" \
  "$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName Tabor CEF Containment Test" \
  "$app_path/Contents/Info.plist"
cp "$build_target_dir/debug/tabor" "$main_binary"
chmod 755 "$main_binary"
CEF_PATH="$cef_root" scripts/bundle-macos-deps.sh "$app_path"
scripts/create-macos-cef-helpers.sh "$app_path"
scripts/sign-macos-app.sh "$app_path"

codesign --verify --deep --strict "$app_path" 2>&1 | tee "$artifact_dir/codesign-verify.txt"
codesign -dvv "$app_path" 2>&1 | tee "$artifact_dir/codesign-details.txt"
codesign --verify --deep --strict "$web_host_app" 2>&1 \
  | tee "$artifact_dir/web-host-codesign-verify.txt"
codesign -dvv "$web_host_app" 2>&1 | tee "$artifact_dir/web-host-codesign-details.txt"
grep -q '^TeamIdentifier=7A5AR5N85X$' "$artifact_dir/codesign-details.txt"
grep -q '^TeamIdentifier=7A5AR5N85X$' "$artifact_dir/web-host-codesign-details.txt"
! grep -qiE '^(Signature=adhoc|TeamIdentifier=not set)$' \
  "$artifact_dir/codesign-details.txt" "$artifact_dir/web-host-codesign-details.txt"

msg() {
  "$main_binary" msg --socket "$socket" "$@"
}

runtime_metrics() {
  msg send '{"type":"runtime_metrics"}'
}

observe_request() {
  local tab_json="$1"
  local request
  request="$(jq -cn --argjson tab "$tab_json" '{type:"agent_observe",tab_id:$tab}')"
  msg send "$request"
}

wait_for_observation() {
  local tab_json="$1"
  local observation=""
  for _ in {1..200}; do
    observation="$(observe_request "$tab_json" 2>/dev/null || true)"
    if jq -e --arg run_id "$run_id" '
      .type == "agent_observation"
      and (.observation.url | contains($run_id))
      and .observation.ready_state == "complete"
      and any(.observation.elements[]?; .name == "Recovered")
    ' <<<"$observation" >/dev/null 2>&1; then
      printf '%s\n' "$observation"
      return
    fi
    sleep 0.1
  done
  echo "Web tab did not recover: $observation" >&2
  return 1
}

wait_for_acceleration_ready() {
  local tab_id="$1"
  local tab_state=""
  for _ in {1..200}; do
    tab_state="$(msg get-tab-state --tab-id "$tab_id")"
    if jq -e '
      .tab.browser_layout.acceleration.state == "ready"
      and .tab.browser_layout.acceleration.frame_delivery_mode == "cef_host_ipc"
      and .tab.browser_layout.acceleration.main_surface_width > 0
      and .tab.browser_layout.acceleration.main_surface_height > 0
    ' <<<"$tab_state" >/dev/null 2>&1; then
      printf '%s\n' "$tab_state"
      return
    fi
    sleep 0.1
  done
  echo "Web acceleration did not recover: $tab_state" >&2
  return 1
}

open -W -n -a "$app_path" \
  --stdout "$log_path" --stderr "$log_path" \
  --env "TABOR_TEST_STATE_ROOT=$state_root" \
  --env "TABOR_CEF_CACHE_PATH=$state_root/cef" \
  --env "TABOR_CEF_LOG_PATH=$artifact_dir/cef.log" \
  --env TABOR_WEBVIEW_ENGINE=cef \
  --env RUST_BACKTRACE=1 \
  --args --socket "$socket" &
open_pid=$!

for _ in {1..300}; do
  [[ -S "$socket" ]] && msg ping >/dev/null 2>&1 && break
  kill -0 "$open_pid" 2>/dev/null || break
  sleep 0.1
done
[[ -S "$socket" ]]
msg ping >/dev/null
test_pid="$(lsof -t -- "$socket" | sort -u | awk 'NR == 1 { print }')"
validate_test_pid
assert_protected_unchanged

fixture_url="data:text/html,<title>${run_id}</title><button>Recovered</button>"
create_reply="$(msg create-tab --web "$fixture_url")"
tab_json="$(jq -c '.tab_id' <<<"$create_reply")"
tab_id="$(jq -r '.tab_id | "\(.index):\(.generation)"' <<<"$create_reply")"

initial_metrics=""
for _ in {1..200}; do
  initial_metrics="$(runtime_metrics)"
  if jq -e '
    .metrics.cef_host.connected == true
    and .metrics.cef_host.active_views >= 1
    and .metrics.cef_host.pid != null
    and .metrics.webview.frame_delivery_mode == "cef_host_ipc"
    and .metrics.cef_pump.executed == 0
  ' <<<"$initial_metrics" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
jq -e '
  .metrics.cef_host.connected == true
  and .metrics.cef_host.active_views >= 1
  and .metrics.cef_host.pid != null
  and .metrics.webview.frame_delivery_mode == "cef_host_ipc"
  and .metrics.cef_pump.executed == 0
' <<<"$initial_metrics" >/dev/null
initial_observation="$(wait_for_observation "$tab_json")"
initial_tab_state="$(wait_for_acceleration_ready "$tab_id")"
initial_metrics="$(runtime_metrics)"
jq -e '
  .metrics.webview.accelerated_frames >= 1
  and .metrics.webview.live_accelerated_surfaces >= 1
' <<<"$initial_metrics" >/dev/null
printf '%s\n' "$initial_metrics" >"$artifact_dir/metrics-initial.json"
printf '%s\n' "$initial_observation" >"$artifact_dir/observation-initial.json"
printf '%s\n' "$initial_tab_state" >"$artifact_dir/tab-state-initial.json"

initial_host_pid="$(jq -r '.metrics.cef_host.pid' <<<"$initial_metrics")"
initial_generation="$(jq -r '.metrics.cef_host.generation' <<<"$initial_metrics")"
initial_crashes="$(jq -r '.metrics.cef_host.crashes' <<<"$initial_metrics")"
initial_pressure_passed="$(jq -r '.metrics.cef_host.memory_pressure_tests_passed' \
  <<<"$initial_metrics")"
initial_pressure_failed="$(jq -r '.metrics.cef_host.memory_pressure_tests_failed' \
  <<<"$initial_metrics")"
initial_accelerated_frames="$(jq -r '.metrics.webview.accelerated_frames' <<<"$initial_metrics")"

pressure_request="$(jq -cn --argjson tab "$tab_json" \
  '{type:"window_debug_cef_memory_pressure",tab_id:$tab}')"
pressure_reply="$(msg send "$pressure_request")"
jq -e '.type == "ok"' <<<"$pressure_reply" >/dev/null

pressure_metrics=""
for _ in {1..200}; do
  pressure_metrics="$(runtime_metrics)"
  if jq -e \
    --argjson pid "$initial_host_pid" \
    --argjson generation "$initial_generation" \
    --argjson passed "$((initial_pressure_passed + 1))" \
    --argjson failed "$initial_pressure_failed" '
      .metrics.cef_host.connected == true
      and .metrics.cef_host.pid == $pid
      and .metrics.cef_host.generation == $generation
      and .metrics.cef_host.memory_pressure_tests_passed == $passed
      and .metrics.cef_host.memory_pressure_tests_failed == $failed
      and .metrics.cef_host.last_memory_pressure_error == null
    ' <<<"$pressure_metrics" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
sleep 1
pressure_metrics="$(runtime_metrics)"
jq -e \
  --argjson pid "$initial_host_pid" \
  --argjson generation "$initial_generation" \
  --argjson passed "$((initial_pressure_passed + 1))" \
  --argjson failed "$initial_pressure_failed" '
    .metrics.cef_host.connected == true
    and .metrics.cef_host.pid == $pid
    and .metrics.cef_host.generation == $generation
    and .metrics.cef_host.memory_pressure_tests_passed == $passed
    and .metrics.cef_host.memory_pressure_tests_failed == $failed
    and .metrics.cef_host.last_memory_pressure_error == null
  ' <<<"$pressure_metrics" >/dev/null
kill -0 "$test_pid"
pressure_observation="$(wait_for_observation "$tab_json")"
pressure_tab_state="$(wait_for_acceleration_ready "$tab_id")"
pressure_metrics="$(runtime_metrics)"
jq -e '.metrics.webview.live_accelerated_surfaces >= 1' \
  <<<"$pressure_metrics" >/dev/null
printf '%s\n' "$pressure_metrics" >"$artifact_dir/metrics-after-pressure.json"
printf '%s\n' "$pressure_observation" >"$artifact_dir/observation-after-pressure.json"
printf '%s\n' "$pressure_tab_state" >"$artifact_dir/tab-state-after-pressure.json"

crash_reply="$(msg send '{"type":"window_debug_cef_host_crash"}')"
jq -e '.type == "ok"' <<<"$crash_reply" >/dev/null

recovered_metrics=""
for _ in {1..300}; do
  kill -0 "$test_pid"
  recovered_metrics="$(runtime_metrics 2>/dev/null || true)"
  if jq -e \
    --argjson old_pid "$initial_host_pid" \
    --argjson old_generation "$initial_generation" \
    --argjson crashes "$((initial_crashes + 1))" '
      .metrics.cef_host.connected == true
      and .metrics.cef_host.pid != null
      and .metrics.cef_host.pid != $old_pid
      and .metrics.cef_host.generation > $old_generation
      and .metrics.cef_host.crashes >= $crashes
      and .metrics.cef_host.restarts >= 1
    ' <<<"$recovered_metrics" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
jq -e \
  --argjson old_pid "$initial_host_pid" \
  --argjson old_generation "$initial_generation" \
  --argjson crashes "$((initial_crashes + 1))" '
    .metrics.cef_host.connected == true
    and .metrics.cef_host.pid != null
    and .metrics.cef_host.pid != $old_pid
    and .metrics.cef_host.generation > $old_generation
    and .metrics.cef_host.crashes >= $crashes
    and .metrics.cef_host.restarts >= 1
  ' <<<"$recovered_metrics" >/dev/null

recovered_host_pid="$(jq -r '.metrics.cef_host.pid' <<<"$recovered_metrics")"
recovered_host_command="$(ps -p "$recovered_host_pid" -o command=)"
[[ "$recovered_host_command" == "$web_host_binary"* ]]
printf '%s\n' "$recovered_metrics" >"$artifact_dir/metrics-after-restart.json"
recovered_observation="$(wait_for_observation "$tab_json")"
tab_state="$(wait_for_acceleration_ready "$tab_id")"
recovered_metrics="$(runtime_metrics)"
jq -e --argjson initial_frames "$initial_accelerated_frames" '
  .metrics.webview.accelerated_frames > $initial_frames
  and .metrics.webview.live_accelerated_surfaces >= 1
' <<<"$recovered_metrics" >/dev/null
jq -e --arg run_id "$run_id" '.tab.kind.web.url | contains($run_id)' \
  <<<"$tab_state" >/dev/null
msg ping >/dev/null
kill -0 "$test_pid"
validate_test_pid
assert_protected_unchanged

printf '%s\n' "$recovered_metrics" >"$artifact_dir/metrics-after-restart.json"
printf '%s\n' "$recovered_observation" >"$artifact_dir/observation-after-restart.json"
printf '%s\n' "$tab_state" >"$artifact_dir/tab-state-after-restart.json"
printf '%s\n' \
  "main_pid=$test_pid" \
  "initial_host_pid=$initial_host_pid" \
  "recovered_host_pid=$recovered_host_pid" \
  "initial_generation=$initial_generation" \
  "recovered_generation=$(jq -r '.metrics.cef_host.generation' <<<"$recovered_metrics")" \
  "memory_pressure=pass" \
  "host_crash_contained=pass" \
  "url_recovered=pass" \
  "artifacts=$artifact_dir" | tee "$artifact_dir/result.txt"

stop_test_process
assert_protected_unchanged
