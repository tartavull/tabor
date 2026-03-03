#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -z "${CEF_PATH:-}" && -n "${TABOR_CEF_PATH:-}" ]]; then
  export CEF_PATH="$TABOR_CEF_PATH"
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
  passkey_enabled=false
  case "${TABOR_ENABLE_PASSKEY:-0}" in
    1|true|TRUE|yes|YES)
      passkey_enabled=true
      ;;
  esac

  run_raw=false
  case "${TABOR_RUN_RAW:-0}" in
    1|true|TRUE|yes|YES)
      run_raw=true
      ;;
  esac

  # Backward-compatible override for the old env knob.
  case "${TABOR_RUN_APP_BUNDLE:-1}" in
    0|false|FALSE|no|NO)
      run_raw=true
      ;;
  esac

  if $passkey_enabled; then
    exec cargo xtask run --passkey -- "$@"
  fi

  if $run_raw; then
    exec cargo xtask run-raw -- "$@"
  fi

  exec cargo xtask run -- "$@"
fi

exec cargo run -p tabor --bin tabor -- "$@"
