#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This regression harness requires macOS." >&2
  exit 1
fi

: "${TABOR_PROTECTED_PID:?set TABOR_PROTECTED_PID to the canonical Tabor PID}"
: "${TABOR_PROTECTED_SOCKET:?set TABOR_PROTECTED_SOCKET to the canonical Tabor socket}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
run_id="r$(date +%Y%m%d%H%M%S)$$"
bundle_id="com.pinkbot.tabor.test.jsdialog.${run_id}"
app_path="/Applications/Tabor JS Dialog Test ${run_id}.app"
executable_name="TaborJsDialogTest_${run_id}"
main_binary="$app_path/Contents/MacOS/$executable_name"
artifact_dir="$repo_root/target/js-dialog-regression/$run_id"
run_root="$(mktemp -d "/tmp/tabor-js-dialog-${run_id}.XXXXXX")"
diagnostic_marker="$artifact_dir/diagnostic-marker"
test_pid=""
open_pid=""
socket=""
state_root=""

assert_canonical_unchanged() {
  kill -0 "$TABOR_PROTECTED_PID"
  [[ -S "$TABOR_PROTECTED_SOCKET" ]]
  local command
  command="$(ps -p "$TABOR_PROTECTED_PID" -o command=)"
  [[ "$command" == /Applications/Tabor.app/* ]]
  lsof -a -p "$TABOR_PROTECTED_PID" -U "$TABOR_PROTECTED_SOCKET" >/dev/null
}

validate_test_pid() {
  [[ -n "$test_pid" ]]
  [[ "$test_pid" != "$TABOR_PROTECTED_PID" ]]
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
  assert_canonical_unchanged
  if [[ "$app_path" == "/Applications/Tabor JS Dialog Test ${run_id}.app" ]]; then
    rm -rf -- "$app_path"
  fi
  if [[ "$run_root" == /tmp/tabor-js-dialog-${run_id}.* ]]; then
    rm -rf -- "$run_root"
  fi
}
trap cleanup EXIT

assert_canonical_unchanged
mkdir -p "$artifact_dir" "$app_path/Contents/MacOS" "$app_path/Contents/Resources"
touch "$diagnostic_marker"

cd "$repo_root"
cargo build -p tabor --bin tabor
cp extra/osx/Tabor.Info.plist "$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $bundle_id" "$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleExecutable $executable_name" "$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleName Tabor JS Dialog Test" "$app_path/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName Tabor JS Dialog Test" "$app_path/Contents/Info.plist"
cp target/debug/tabor "$main_binary"
chmod 755 "$main_binary"
scripts/bundle-macos-deps.sh "$app_path"
scripts/create-macos-cef-helpers.sh "$app_path"
scripts/sign-macos-app.sh "$app_path"
codesign --verify --deep --strict "$app_path" 2>&1 | tee "$artifact_dir/codesign-verify.txt"
codesign -dvv "$app_path" 2>&1 | tee "$artifact_dir/codesign-details.txt"
grep -q '^TeamIdentifier=7A5AR5N85X$' "$artifact_dir/codesign-details.txt"
! grep -qiE '^(Signature=adhoc|TeamIdentifier=not set)$' "$artifact_dir/codesign-details.txt"

msg() {
  "$main_binary" msg --socket "$socket" "$@"
}

launch_test() {
  local mode="$1"
  state_root="$run_root/$mode-state"
  socket="$state_root/tabor.sock"
  local log="$artifact_dir/$mode.log"
  mkdir -p "$state_root"
  local args=(
    open -W -n -a "$app_path"
    --stdout "$log" --stderr "$log"
    --env "TABOR_TEST_STATE_ROOT=$state_root"
    --env TABOR_WEBVIEW_ENGINE=cef
    --env RUST_BACKTRACE=1
  )
  if [[ "$mode" == "red" ]]; then
    args+=(--env TABOR_TEST_USE_DEFAULT_JS_DIALOG=1)
  fi
  args+=(--args --socket "$socket")
  "${args[@]}" &
  open_pid=$!

  for _ in {1..200}; do
    [[ -S "$socket" ]] && msg ping >/dev/null 2>&1 && break
    kill -0 "$open_pid" 2>/dev/null || break
    sleep 0.1
  done
  [[ -S "$socket" ]]
  msg ping >/dev/null
  test_pid="$(lsof -t -- "$socket" | sort -u | head -n 1)"
  validate_test_pid
  assert_canonical_unchanged
  printf 'mode=%s\npid=%s\nsocket=%s\n' "$mode" "$test_pid" "$socket" \
    >"$artifact_dir/$mode-process.txt"
}

send_command() {
  local session="$1"
  local id="$2"
  local method="$3"
  local params="$4"
  local message
  message="$(jq -cn --argjson id "$id" --arg method "$method" --argjson params "$params" \
    '{id:$id,method:$method,params:$params}')"
  msg inspector send --session-id "$session" --message "$message" >/dev/null
}

send_eval() {
  local session="$1"
  local id="$2"
  local expression="$3"
  local params
  params="$(jq -cn --arg expression "$expression" \
    '{expression:$expression,returnByValue:true,awaitPromise:true}')"
  send_command "$session" "$id" Runtime.evaluate "$params"
}

wait_event() {
  local session="$1"
  local method="$2"
  local polled payload
  for _ in {1..100}; do
    polled="$(msg inspector poll --session-id "$session" --max 64)"
    payload="$(jq -r --arg method "$method" \
      '.messages[].payload | fromjson | select(.method == $method) | @json' \
      <<<"$polled" | head -n 1)"
    if [[ -n "$payload" ]]; then
      printf '%s\n' "$payload"
      return 0
    fi
    sleep 0.05
  done
  return 1
}

wait_response() {
  local session="$1"
  local id="$2"
  local polled payload
  for _ in {1..100}; do
    polled="$(msg inspector poll --session-id "$session" --max 64)"
    payload="$(jq -r --argjson id "$id" \
      '.messages[].payload | fromjson | select(.id == $id) | @json' \
      <<<"$polled" | head -n 1)"
    if [[ -n "$payload" ]]; then
      printf '%s\n' "$payload"
      return 0
    fi
    sleep 0.05
  done
  return 1
}

attach_web_tab() {
  local reply tab session
  reply="$(msg create-tab --web about:blank)"
  tab="$(jq -r '.tab_id | "\(.index):\(.generation)"' <<<"$reply")"
  reply="$(msg inspector attach --tab-id "$tab")"
  session="$(jq -r '.session.session_id' <<<"$reply")"
  send_command "$session" 1 Page.enable '{}'
  wait_response "$session" 1 >/dev/null
  send_command "$session" 2 Runtime.enable '{}'
  wait_response "$session" 2 >/dev/null
  printf '%s %s\n' "$tab" "$session"
}

press_dialog() {
  local button="$1"
  local prompt_text="${2:-}"
  local request
  if [[ -n "$prompt_text" ]]; then
    request="$(jq -cn --arg button "$button" --arg prompt "$prompt_text" \
      '{type:"window_debug_press_js_dialog_button",button:$button,prompt_text:$prompt}')"
  else
    request="$(jq -cn --arg button "$button" \
      '{type:"window_debug_press_js_dialog_button",button:$button}')"
  fi
  msg send "$request"
}

launch_test red
read -r red_tab red_session < <(attach_web_tab)
red_crashed=0
for iteration in {1..40}; do
  id=$((100 + iteration))
  send_eval "$red_session" "$id" 'confirm("Confirm me")'
  event="$(wait_event "$red_session" Page.javascriptDialogOpening)"
  jq -e '.params.type == "confirm" and .params.message == "Confirm me"' <<<"$event" >/dev/null
  set +e
  press_dialog dismiss >"$artifact_dir/red-button-$iteration.txt" 2>&1
  button_status=$?
  set -e
  sleep 0.2
  if ! kill -0 "$test_pid" 2>/dev/null; then
    red_crashed=1
    printf 'iteration=%s\nbutton_status=%s\n' "$iteration" "$button_status" \
      >"$artifact_dir/red-result.txt"
    break
  fi
  response="$(wait_response "$red_session" "$id")"
  jq -e '.result.result.value == false' <<<"$response" >/dev/null
done
[[ "$red_crashed" -eq 1 ]]
set +e
wait "$open_pid"
red_open_status=$?
set -e
open_pid=""
test_pid=""
printf 'launch_services_wait_status=%s\n' "$red_open_status" >>"$artifact_dir/red-result.txt"

red_report=""
for _ in {1..100}; do
  red_report="$(find "$HOME/Library/Logs/DiagnosticReports" -type f \
    -name "${executable_name}*.ips" -newer "$diagnostic_marker" -print 2>/dev/null \
    | sort | tail -n 1)"
  [[ -n "$red_report" ]] && break
  sleep 0.1
done
[[ -n "$red_report" ]]
cp "$red_report" "$artifact_dir/red-crash.ips"
rg -n 'objc_retain|didEndAlert|NSWindowEndWindowModalSession|sendAction|NSButtonCell' \
  "$artifact_dir/red-crash.ips" >"$artifact_dir/red-stack.txt"
grep -q 'didEndAlert' "$artifact_dir/red-stack.txt"

rm -rf -- "$state_root"
launch_test green
read -r green_tab green_session < <(attach_web_tab)

send_eval "$green_session" 10 'alert("Alert me"); "alert accepted"'
event="$(wait_event "$green_session" Page.javascriptDialogOpening)"
jq -e '.params.type == "alert"' <<<"$event" >/dev/null
press_dialog accept >/dev/null
response="$(wait_response "$green_session" 10)"
jq -e '.result.result.value == "alert accepted"' <<<"$response" >/dev/null

send_eval "$green_session" 20 'confirm("Confirm accept")'
wait_event "$green_session" Page.javascriptDialogOpening >/dev/null
press_dialog accept >/dev/null
response="$(wait_response "$green_session" 20)"
jq -e '.result.result.value == true' <<<"$response" >/dev/null

send_eval "$green_session" 21 'confirm("Confirm dismiss")'
wait_event "$green_session" Page.javascriptDialogOpening >/dev/null
press_dialog dismiss >/dev/null
response="$(wait_response "$green_session" 21)"
jq -e '.result.result.value == false' <<<"$response" >/dev/null

send_eval "$green_session" 30 'prompt("Prompt accept", "default value")'
wait_event "$green_session" Page.javascriptDialogOpening >/dev/null
press_dialog accept 'edited value' >/dev/null
response="$(wait_response "$green_session" 30)"
jq -e '.result.result.value == "edited value"' <<<"$response" >/dev/null

send_eval "$green_session" 31 'prompt("Prompt dismiss", "default value")'
wait_event "$green_session" Page.javascriptDialogOpening >/dev/null
press_dialog dismiss >/dev/null
response="$(wait_response "$green_session" 31)"
jq -e '.result.result.type == "object" and .result.result.subtype == "null"' \
  <<<"$response" >/dev/null

send_eval "$green_session" 40 'confirm("Reset me")'
wait_event "$green_session" Page.javascriptDialogOpening >/dev/null
msg set-web-url 'about:blank#reset' --tab-id "$green_tab" >/dev/null
sleep 0.2
reset_reply="$(press_dialog dismiss)"
jq -e '.type == "error"' <<<"$reset_reply" >/dev/null
msg ping >/dev/null

read -r teardown_tab teardown_session < <(attach_web_tab)
send_eval "$teardown_session" 50 'prompt("Teardown me", "default value")'
wait_event "$teardown_session" Page.javascriptDialogOpening >/dev/null
msg close-tab --tab-id "$teardown_tab" >/dev/null
sleep 0.2
teardown_reply="$(press_dialog dismiss)"
jq -e '.type == "error"' <<<"$teardown_reply" >/dev/null
msg ping >/dev/null

printf '%s\n' \
  'alert_accept=pass' \
  'confirm_accept=pass' \
  'confirm_dismiss=pass' \
  'prompt_accept=pass' \
  'prompt_dismiss=pass' \
  'reset=pass' \
  'teardown=pass' >"$artifact_dir/green-result.txt"

stop_test_process
rm -rf -- "$state_root"
assert_canonical_unchanged
printf 'artifacts=%s\n' "$artifact_dir"
