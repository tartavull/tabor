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
  app_frameworks="$app_dir/Contents/Frameworks"

  mkdir -p "$(dirname "$app_dir")"
  if [[ -d "$app_dir" ]]; then
    chmod -R u+w "$app_dir" >/dev/null 2>&1 || true
  fi
  rm -rf "$app_dir"
  cp -a "$app_template" "$app_dir"
  mkdir -p "$app_dir/Contents/MacOS"
  cp -f "$repo_root/target/debug/tabor" "$app_bin"
  chmod +x "$app_bin"

  # Bundle CEF so web tabs work when launched via Finder/Spotlight/open (no shell env vars).
  # Prefer Release/Debug frameworks over a root-level framework.
  mkdir -p "$app_frameworks"
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
    rm -rf "$app_frameworks/Chromium Embedded Framework.framework"
    cp -a "$cef_framework_dir" "$app_frameworks/Chromium Embedded Framework.framework"

    for lib in libEGL.dylib libGLESv2.dylib; do
      src="$cef_framework_dir/Libraries/$lib"
      if [[ -f "$src" ]]; then
        cp -f "$src" "$app_frameworks/$lib"
        ln -sf "../Frameworks/$lib" "$app_dir/Contents/MacOS/$lib"
      fi
    done
  fi

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
