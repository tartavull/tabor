#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT_DIR/target/debug/tabor"

cargo build -p tabor

export TABOR_WEBVIEW_ENGINE=cef

LOG_FILE="$ROOT_DIR/agent-browser-test.log"
TABOR_PID=""
SOCKET=""
TEMP_DIR="${TMPDIR:-/tmp}"
USE_OPEN=false
APP_DIR=""
SKIP_OPEN="${TABOR_NO_OPEN:-}"

if [[ "$(uname -s)" == "Darwin" && -z "$SKIP_OPEN" ]]; then
  APP_TEMPLATE="$ROOT_DIR/extra/osx/Tabor.app"
  APP_DIR="$ROOT_DIR/target/debug/osx/Tabor.app"
  APP_BIN="$APP_DIR/Contents/MacOS/tabor"
  APP_FRAMEWORKS="$APP_DIR/Contents/Frameworks"
  if [[ -d "$APP_TEMPLATE" ]]; then
    mkdir -p "$(dirname "$APP_DIR")"
    if [[ -d "$APP_DIR" ]]; then
      chmod -R u+w "$APP_DIR" >/dev/null 2>&1 || true
    fi
    rm -rf "$APP_DIR"
    cp -a "$APP_TEMPLATE" "$APP_DIR"
    mkdir -p "$APP_DIR/Contents/MacOS"
    cp -f "$BIN" "$APP_BIN"
    chmod +x "$APP_BIN"

    # Bundle CEF so the app can open web tabs when launched outside a shell.
    mkdir -p "$APP_FRAMEWORKS"
    cef_root="${TABOR_CEF_PATH:-${CEF_PATH:-}}"
    cef_framework_dir="${TABOR_CEF_FRAMEWORK_DIR:-}"
    if [[ -z "$cef_framework_dir" && -n "$cef_root" ]]; then
      if [[ -d "$cef_root/Release/Chromium Embedded Framework.framework" ]]; then
        cef_framework_dir="$cef_root/Release/Chromium Embedded Framework.framework"
      elif [[ -d "$cef_root/Debug/Chromium Embedded Framework.framework" ]]; then
        cef_framework_dir="$cef_root/Debug/Chromium Embedded Framework.framework"
      elif [[ -d "$cef_root/Chromium Embedded Framework.framework" ]]; then
        cef_framework_dir="$cef_root/Chromium Embedded Framework.framework"
      fi
    fi
    if [[ -n "$cef_framework_dir" && -d "$cef_framework_dir" ]]; then
      rm -rf "$APP_FRAMEWORKS/Chromium Embedded Framework.framework"
      cp -a "$cef_framework_dir" "$APP_FRAMEWORKS/Chromium Embedded Framework.framework"

      for lib in libEGL.dylib libGLESv2.dylib; do
        src="$cef_framework_dir/Libraries/$lib"
        if [[ -f "$src" ]]; then
          cp -f "$src" "$APP_FRAMEWORKS/$lib"
          ln -sf "../Frameworks/$lib" "$APP_DIR/Contents/MacOS/$lib"
        fi
      done
    fi

    USE_OPEN=true
  fi
fi

if $USE_OPEN; then
  shopt -s nullglob
  existing_sockets=("$TEMP_DIR"/Tabor-*.sock)
  shopt -u nullglob
  if ! open -n -g -a "$APP_DIR" >/dev/null 2>&1; then
    USE_OPEN=false
  fi
fi

if ! $USE_OPEN; then
  TABOR_BACKGROUND=1 "$BIN" >"$LOG_FILE" 2>&1 &
  TABOR_PID=$!
fi

cleanup() {
  if [[ -n "$TABOR_PID" ]]; then
    kill "$TABOR_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if $USE_OPEN; then
  declare -A existing
  for sock in "${existing_sockets[@]:-}"; do
    existing["$sock"]=1
  done
  for _ in {1..80}; do
    shopt -s nullglob
    for sock in "$TEMP_DIR"/Tabor-*.sock; do
      if [[ -z "${existing[$sock]:-}" ]]; then
        SOCKET="$sock"
        break
      fi
    done
    shopt -u nullglob
    if [[ -S "$SOCKET" ]]; then
      break
    fi
    sleep 0.2
  done
  if [[ -S "$SOCKET" && "$SOCKET" =~ Tabor-([0-9]+)\.sock$ ]]; then
    TABOR_PID="${BASH_REMATCH[1]}"
  fi
else
  SOCKET="$TEMP_DIR/Tabor-$TABOR_PID.sock"
  for _ in {1..80}; do
    if [[ -S "$SOCKET" ]]; then
      break
    fi
    sleep 0.2
  done
fi

if [[ ! -S "$SOCKET" ]]; then
  if $USE_OPEN; then
    echo "Failed to locate Tabor socket in $TEMP_DIR. Check /tmp/Tabor-*.log for details." >&2
  else
    echo "Failed to locate Tabor socket $SOCKET. See $LOG_FILE" >&2
  fi
  exit 1
fi

export TABOR_SOCKET="$SOCKET"
wait_for_ipc() {
  python3 - <<'PY'
import json
import os
import socket
import sys
import time

path = os.environ.get("TABOR_SOCKET")
if not path:
    sys.exit(2)

deadline = time.time() + 8
payload = json.dumps({"type": "ping"}).encode()

while time.time() < deadline:
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(1.5)
        sock.connect(path)
        sock.sendall(payload)
        sock.shutdown(socket.SHUT_WR)
        data = sock.recv(4096)
        if data:
            sys.exit(0)
    except Exception:
        pass
    finally:
        try:
            sock.close()
        except Exception:
            pass
    time.sleep(0.2)
sys.exit(1)
PY
}

if ! wait_for_ipc; then
  if $USE_OPEN; then
    if [[ -n "$TABOR_PID" ]]; then
      kill "$TABOR_PID" 2>/dev/null || true
    fi
    if [[ -n "$SOCKET" ]]; then
      rm -f "$SOCKET" || true
    fi

    USE_OPEN=false
    TABOR_BACKGROUND=1 "$BIN" >"$LOG_FILE" 2>&1 &
    TABOR_PID=$!

    SOCKET="$TEMP_DIR/Tabor-$TABOR_PID.sock"
    for _ in {1..80}; do
      if [[ -S "$SOCKET" ]]; then
        break
      fi
      sleep 0.2
    done

    if [[ ! -S "$SOCKET" ]]; then
      echo "Failed to locate Tabor socket $SOCKET. See $LOG_FILE" >&2
      exit 1
    fi

    export TABOR_SOCKET="$SOCKET"
    if ! wait_for_ipc; then
      echo "Failed to ping Tabor IPC socket $SOCKET" >&2
      exit 1
    fi
  else
    echo "Failed to ping Tabor IPC socket $SOCKET" >&2
    exit 1
  fi
fi

if $USE_OPEN; then
  if ! timeout 5 "$BIN" agent-browser eval "1+1" >/dev/null 2>&1; then
    if [[ -n "$TABOR_PID" ]]; then
      kill "$TABOR_PID" 2>/dev/null || true
    fi
    if [[ -n "$SOCKET" ]]; then
      rm -f "$SOCKET" || true
    fi

    USE_OPEN=false
    TABOR_BACKGROUND=1 "$BIN" >"$LOG_FILE" 2>&1 &
    TABOR_PID=$!

    SOCKET="$TEMP_DIR/Tabor-$TABOR_PID.sock"
    for _ in {1..80}; do
      if [[ -S "$SOCKET" ]]; then
        break
      fi
      sleep 0.2
    done

    if [[ ! -S "$SOCKET" ]]; then
      echo "Failed to locate Tabor socket $SOCKET. See $LOG_FILE" >&2
      exit 1
    fi

    export TABOR_SOCKET="$SOCKET"
    if ! wait_for_ipc; then
      echo "Failed to ping Tabor IPC socket $SOCKET" >&2
      exit 1
    fi
  fi
fi

ab() {
  "$BIN" agent-browser "$@"
}

FILE_PATH="$ROOT_DIR/tabor/tests/fixtures/agent-browser.html"
FILE_URL="file://$FILE_PATH"

ab open "$FILE_URL"
ab wait --load domcontentloaded
ab snapshot -i
ab snapshot -i -c -d 2 -s "body"

ab console --clear >/dev/null
ab errors --clear >/dev/null

ab click "#console-btn"
sleep 0.2
ab console | grep -q "console: hello"

ab click "#error-btn"
sleep 0.5
ab errors | grep -q "fixture error"

ab fill "#email-input" "test@example.com"
[[ "$(ab get value "#email-input")" == "test@example.com" ]]

ab type "#notes" "hello"
[[ "$(ab get value "#notes")" == "hello" ]]

ab press Enter
ab keydown Shift
ab keyup Shift
ab hover "#btn-alert"

ab check "#check-me"
[[ "$(ab is checked "#check-me")" == "true" ]]
ab uncheck "#check-me"
[[ "$(ab is checked "#check-me")" == "false" ]]

ab select "#select-me" "b"
[[ "$(ab get value "#select-me")" == "b" ]]

ab drag "#drag-src" "#drag-dst"
[[ "$(ab get text "#drag-dst")" == "drag-data" ]]

UPLOAD_FILE="$ROOT_DIR/agent-browser-upload.txt"
echo "upload" > "$UPLOAD_FILE"
ab upload "#file-input" "$UPLOAD_FILE"
ab wait --text "Files: $(basename "$UPLOAD_FILE")"

ab scroll down 200
ab scrollintoview "#bottom-target"

ab find role button click --name "Console log"
ab find label "Email" fill "agent@test.com"
ab find placeholder "Email" type "x"
ab find alt "Logo" click
ab find title "Close" click
ab find testid "console-button" click
ab find first ".item" click
ab find last ".item" hover
ab find nth 2 ".item" hover

ab mouse move 10 10
ab mouse down left
ab mouse up left
ab mouse wheel 120

[[ "$(ab get attr "#logo" alt)" == "Logo" ]]
[[ "$(ab get count ".item")" == "3" ]]
[[ "$(ab is visible "#title")" == "true" ]]

ab get box "#title" >/dev/null
ab get styles "#title" >/dev/null

ab set viewport 800 600
ab set device "iPhone 14"
[[ "$(ab eval "navigator.userAgent")" == *"iPhone"* ]]

ab set geo 37.7749 -122.4194
[[ "$(ab eval "window.__geo=null; navigator.geolocation.getCurrentPosition(p => { window.__geo = p.coords.latitude; }); window.__geo")" == "37.7749" ]]

ab set offline on
[[ "$(ab eval "navigator.onLine")" == "false" ]]
ab set offline off

ab set headers '{"X-Test":"ok"}'
ab set credentials "user" "pass"

ab set media dark reduced-motion
[[ "$(ab eval "window.matchMedia('(prefers-color-scheme: dark)').matches")" == "true" ]]

ab network route "https://example.com/route-test" --body "hello world" --content-type "text/plain"
ab click "#fetch-btn"
ab wait --text "Fetch output: hello"
ab network requests --filter "route-test" | grep -q "route-test"
ab network unroute "https://example.com/route-test"
ab network requests --clear >/dev/null

ab cookies set foo bar

ab storage local set "foo" "bar"
[[ "$(ab storage local foo)" == "bar" ]]

ab tab new "$FILE_URL"
ab tab list >/dev/null
ab tab close

ab window new
ab tab close
ab open "$FILE_URL"
ab wait --load domcontentloaded

ab frame "#inner-frame"
[[ "$(ab get text "#frame-text")" == "Frame text" ]]
ab frame main

ab dialog accept "hello"
ab click "#btn-prompt"
ab wait --text "Prompt result: hello"

ab dialog dismiss
ab click "#btn-confirm"
ab wait --text "Confirm result: dismissed"

ab highlight "#title"

SCREENSHOT_PATH="$ROOT_DIR/agent-browser.png"
PDF_PATH="$ROOT_DIR/agent-browser.pdf"
TRACE_PATH="$ROOT_DIR/agent-browser-trace.json"
RECORD_PATH="$ROOT_DIR/agent-browser.webm"

ab screenshot "$SCREENSHOT_PATH"
ab pdf "$PDF_PATH"

ab trace start
ab click "#console-btn"
ab trace stop "$TRACE_PATH"

ab record start "$RECORD_PATH"
sleep 1
ab click "#console-btn"
sleep 1
ab record stop

[[ -s "$SCREENSHOT_PATH" ]]
[[ -s "$PDF_PATH" ]]
[[ -s "$TRACE_PATH" ]]
[[ -s "$RECORD_PATH" ]]

ab connect 9222
ab close

echo "agent-browser verification completed"
