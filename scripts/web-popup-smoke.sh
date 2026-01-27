#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${TABOR_BIN:-}" ]]; then
  if [[ "$(uname)" == "Darwin" ]]; then
    TABOR_BIN="./scripts/run.sh"
  elif [[ -x "./target/debug/tabor" ]]; then
    TABOR_BIN="./target/debug/tabor"
  else
    TABOR_BIN="tabor"
  fi
fi
PYTHON_BIN="${PYTHON_BIN:-python3}"

if ! command -v "$TABOR_BIN" >/dev/null 2>&1; then
  echo "tabor binary not found: $TABOR_BIN" >&2
  exit 1
fi

if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
  echo "python3 not found: $PYTHON_BIN" >&2
  exit 1
fi

if [[ "$(uname)" != "Darwin" ]]; then
  echo "web popup smoke test only supports macOS" >&2
  exit 1
fi

socket_dir=$(mktemp -d "${TMPDIR:-/tmp}/tabor-popup-smoke.XXXXXX")
socket_path="${socket_dir}/tabor.sock"
site_dir="${socket_dir}/site"
mkdir -p "$site_dir"
http_log="${socket_dir}/http.log"

cleanup() {
  if [[ -n "${http_pid:-}" ]]; then
    kill "$http_pid" >/dev/null 2>&1 || true
  fi
  if [[ -n "${tabor_pid:-}" ]]; then
    kill "$tabor_pid" >/dev/null 2>&1 || true
  fi
  rm -rf "$socket_dir"
}
trap cleanup EXIT

"$TABOR_BIN" --socket "$socket_path" >/dev/null 2>&1 &
tabor_pid=$!

for _ in $(seq 1 100); do
  if [[ -S "$socket_path" ]]; then
    break
  fi
  sleep 0.05
done

if [[ ! -S "$socket_path" ]]; then
  echo "IPC socket not found at $socket_path" >&2
  exit 1
fi

"$PYTHON_BIN" - "$site_dir/popup-icon.png" <<'PY'
import base64
import pathlib
import sys

data = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR4nGNgYAAAAAMAASsJTYQAAAAASUVORK5CYII="
)
path = pathlib.Path(sys.argv[1])
path.write_bytes(data)
PY

cat >"$site_dir/opener.html" <<'HTML'
<!doctype html>
<title>popup-opener</title>
<script>
let got = false;
window.addEventListener("message", (event) => {
  if (event.data === "popup-ok") {
    got = true;
    document.title = "popup-ok";
  }
});
window.onload = () => {
  const popup = window.open("", "_blank", "width=400,height=400");
  if (!popup) {
    document.title = "popup-blocked";
    return;
  }
  const popupHtml = [
    '<!doctype html>',
    '<title>popup</title>',
    '<link rel="icon" href="/popup-icon.png">',
    '<script>',
    'try {',
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
    if (!got && document.title === "popup-opener") {
      document.title = "popup-timeout";
    }
  }, 2000);
};
</script>
HTML

port=$("$PYTHON_BIN" - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)

"$PYTHON_BIN" -m http.server "$port" --bind 127.0.0.1 --directory "$site_dir" >"$http_log" 2>&1 &
http_pid=$!

for _ in $(seq 1 50); do
  if "$PYTHON_BIN" - <<PY >/dev/null 2>&1; then
import socket
sock = socket.socket()
sock.settimeout(0.1)
sock.connect(("127.0.0.1", int("$port")))
sock.close()
PY
    break
  fi
  sleep 0.05
done

opener_url="http://127.0.0.1:${port}/opener.html"

create_payload=$("$PYTHON_BIN" - "$opener_url" <<'PY'
import json
import sys

print(json.dumps({
  "type": "create_tab",
  "options": {
    "terminal_options": {"hold": False, "command": []},
    "window_identity": {},
    "window_kind": {"kind": "web", "url": sys.argv[1]},
    "option": [],
  },
}))
PY
)

create_reply=$("$TABOR_BIN" msg --socket "$socket_path" send "$create_payload")

"$PYTHON_BIN" - "$create_reply" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
if data.get("type") != "tab_created":
    print(f"Unexpected reply: {data}", file=sys.stderr)
    sys.exit(1)
PY

list_payload=$("$PYTHON_BIN" - <<'PY'
import json

print(json.dumps({"type": "list_tabs"}))
PY
)

deadline=$((SECONDS + 10))
wait_for_favicon() {
  local favicon_deadline=$((SECONDS + 5))
  while [[ $SECONDS -lt $favicon_deadline ]]; do
    if grep -q "/popup-icon.png" "$http_log"; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}
while [[ $SECONDS -lt $deadline ]]; do
  list_reply=$("$TABOR_BIN" msg --socket "$socket_path" send "$list_payload")
  status=$("$PYTHON_BIN" - "$list_reply" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
titles = []
for group in data.get("groups", []):
    for tab in group.get("tabs", []):
        title = tab.get("title")
        if title:
            titles.append(title)

success = {"popup-sent", "popup-ok"}
failure = {"popup-no-opener", "popup-error", "popup-blocked", "popup-timeout"}

for title in titles:
    if title in success:
        print("success")
        sys.exit(0)
    if title in failure:
        print(f"fail:{title}")
        sys.exit(0)

print("pending")
PY
)
  case "$status" in
    success)
      if ! wait_for_favicon; then
        echo "Popup smoke test failed: favicon request missing" >&2
        exit 1
      fi
      echo "Popup smoke test passed."
      exit 0
      ;;
    fail:*)
      echo "Popup smoke test failed: ${status#fail:}" >&2
      exit 1
      ;;
  esac
  sleep 0.1
done

echo "Popup smoke test failed: timeout waiting for popup result" >&2
exit 1
