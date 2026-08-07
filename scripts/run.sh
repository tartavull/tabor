#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

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
    echo "Raw macOS Tabor launches are disabled because they bypass signed Tabor.app verification." >&2
    exit 1
  fi

  exec cargo xtask run -- "$@"
fi

exec cargo run -p tabor --bin tabor -- "$@"
