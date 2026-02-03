#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -z "${CEF_PATH:-}" && -n "${TABOR_CEF_PATH:-}" ]]; then
  export CEF_PATH="$TABOR_CEF_PATH"
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
  cargo build -p tabor --bin tabor

  app_template="$repo_root/extra/osx/Tabor.app"
  app_dir="$repo_root/target/debug/osx/Tabor.app"
  app_bin="$app_dir/Contents/MacOS/tabor"

  mkdir -p "$(dirname "$app_dir")"
  rm -rf "$app_dir"
  cp -a "$app_template" "$app_dir"
  mkdir -p "$app_dir/Contents/MacOS"
  cp -f "$repo_root/target/debug/tabor" "$app_bin"
  chmod +x "$app_bin"

  use_args=false
  for arg in "$@"; do
    if [[ "$arg" == -* ]]; then
      use_args=true
      break
    fi
  done
  case "${1-}" in
    msg|migrate)
      use_args=true
      ;;
  esac

  if $use_args; then
    exec open -a "$app_dir" --args "$@"
  else
    exec open -a "$app_dir" "$@"
  fi
fi

exec cargo run -p tabor --bin tabor -- "$@"
