#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <path-to-Tabor.app>" >&2
  exit 1
fi

app_path="${1%/}"
contents_dir="$app_path/Contents"
macos_dir="$contents_dir/MacOS"
frameworks_dir="$contents_dir/Frameworks"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! -d "$contents_dir" || ! -d "$macos_dir" ]]; then
  echo "Expected a macOS app bundle at '$app_path'" >&2
  exit 1
fi

mkdir -p "$frameworks_dir"

canonical_dir() {
  local path="$1"
  (
    cd "$path"
    pwd
  )
}

resolve_framework_dir() {
  local path="$1"

  if [[ -d "$path" && "$(basename "$path")" == "Chromium Embedded Framework.framework" ]]; then
    canonical_dir "$path"
    return 0
  fi

  local release="$path/Release/Chromium Embedded Framework.framework"
  if [[ -d "$release" ]]; then
    canonical_dir "$release"
    return 0
  fi

  local debug="$path/Debug/Chromium Embedded Framework.framework"
  if [[ -d "$debug" ]]; then
    canonical_dir "$debug"
    return 0
  fi

  local direct="$path/Chromium Embedded Framework.framework"
  if [[ -d "$direct" ]]; then
    canonical_dir "$direct"
    return 0
  fi

  return 1
}

detect_vendor_cef_framework() {
  local arch_tag
  if [[ "$(uname -m)" == "arm64" ]]; then
    arch_tag="macosarm64"
  else
    arch_tag="macosx64"
  fi

  local vendor_root="$repo_root/vendor/cef"
  if [[ ! -d "$vendor_root" ]]; then
    return 1
  fi

  local candidate
  local framework=""
  local resolved=""
  while IFS= read -r candidate; do
    if resolved="$(resolve_framework_dir "$candidate" 2>/dev/null)"; then
      framework="$resolved"
    fi
  done < <(find "$vendor_root" -mindepth 1 -maxdepth 1 -type d -name "*${arch_tag}*" | sort)

  if [[ -z "$framework" ]]; then
    return 1
  fi

  printf '%s\n' "$framework"
}

detect_cef_framework() {
  local explicit_framework="${TABOR_CEF_FRAMEWORK_DIR:-}"
  local resolved=""
  if [[ -n "$explicit_framework" ]]; then
    if resolved="$(resolve_framework_dir "$explicit_framework" 2>/dev/null)"; then
      printf '%s\n' "$resolved"
      return 0
    fi
    echo "TABOR_CEF_FRAMEWORK_DIR does not point to Chromium Embedded Framework.framework: $explicit_framework" >&2
    exit 1
  fi

  local cef_root="${TABOR_CEF_PATH:-${CEF_PATH:-}}"
  if [[ -n "$cef_root" ]]; then
    if resolved="$(resolve_framework_dir "$cef_root" 2>/dev/null)"; then
      printf '%s\n' "$resolved"
      return 0
    fi
    echo "TABOR_CEF_PATH/CEF_PATH does not contain Chromium Embedded Framework.framework: $cef_root" >&2
    exit 1
  fi

  if resolved="$(detect_vendor_cef_framework 2>/dev/null)"; then
    printf '%s\n' "$resolved"
    return 0
  fi

  return 1
}

find_cef_sidecar_dir() {
  local framework_dir="$1"
  local candidates=(
    "$(dirname "$framework_dir")"
    "$framework_dir/Libraries"
    "$(dirname "$(dirname "$framework_dir")")"
    "$framework_dir"
  )

  local candidate
  for candidate in "${candidates[@]}"; do
    [[ -d "$candidate" ]] || continue
    if [[ -f "$candidate/libGLESv2.dylib" && -f "$candidate/libEGL.dylib" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  return 1
}

bundle_cef_runtime() {
  local framework_src
  framework_src="$(detect_cef_framework || true)"
  if [[ -z "$framework_src" ]]; then
    echo "Unable to locate CEF framework for app bundling." >&2
    echo "Set TABOR_CEF_FRAMEWORK_DIR or TABOR_CEF_PATH/CEF_PATH, or provide vendor/cef/<...>." >&2
    exit 1
  fi

  local framework_dst="$frameworks_dir/Chromium Embedded Framework.framework"
  rm -rf "$framework_dst"
  ditto "$framework_src" "$framework_dst"
  chmod -R u+w "$framework_dst"
  echo "Bundled Chromium Embedded Framework.framework"

  local sidecar_src
  sidecar_src="$(find_cef_sidecar_dir "$framework_src" || true)"
  if [[ -z "$sidecar_src" ]]; then
    echo "Missing CEF sidecar libraries near '$framework_src'" >&2
    exit 1
  fi

  local lib
  for lib in libGLESv2.dylib libEGL.dylib; do
    local src="$sidecar_src/$lib"
    local dst="$frameworks_dir/$lib"
    if [[ ! -f "$src" ]]; then
      echo "Missing CEF sidecar library '$lib' in '$sidecar_src'" >&2
      exit 1
    fi
    cp -fp "$src" "$dst"
    chmod u+w "$dst"
    ln -sfn "../Frameworks/$lib" "$macos_dir/$lib"
    echo "Bundled $lib"
  done
}

is_macho() {
  local path="$1"
  file "$path" | grep -q "Mach-O"
}

is_shared_lib() {
  local path="$1"
  file "$path" | grep -q "dynamically linked shared library"
}

is_external_dep() {
  local dep="$1"
  case "$dep" in
    @*|/System/Library/*|/usr/lib/*)
      return 1
      ;;
    /*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_cef_framework_binary_dep() {
  local dep="$1"
  case "$dep" in
    */Chromium\ Embedded\ Framework.framework/*Chromium\ Embedded\ Framework)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

replacement_for_cef_framework_dep() {
  local host="$1"
  case "$host" in
    "$macos_dir"/*)
      printf '@executable_path/../Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework'
      ;;
    "$frameworks_dir"/*)
      printf '@loader_path/Chromium Embedded Framework.framework/Chromium Embedded Framework'
      ;;
    *)
      printf '@loader_path/../Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework'
      ;;
  esac
}

replacement_for() {
  local host="$1"
  local basename="$2"
  case "$host" in
    "$macos_dir"/*)
      printf '@executable_path/../Frameworks/%s' "$basename"
      ;;
    "$frameworks_dir"/*)
      printf '@loader_path/%s' "$basename"
      ;;
    *)
      printf '@loader_path/../Frameworks/%s' "$basename"
      ;;
  esac
}

list_candidate_binaries() {
  find "$macos_dir" -type f \( -perm -111 -o -name '*.dylib' -o -name '*.so' \)
  if [[ -d "$frameworks_dir" ]]; then
    find "$frameworks_dir" -maxdepth 1 -type f \( -perm -111 -o -name '*.dylib' -o -name '*.so' \)
  fi
}

bundle_cef_runtime

queue=()

while IFS= read -r candidate; do
  queue+=("$candidate")
done < <(list_candidate_binaries | sort)

index=0
while (( index < ${#queue[@]} )); do
  current="${queue[$index]}"
  index=$((index + 1))

  if ! is_macho "$current"; then
    continue
  fi

  if [[ "$(dirname "$current")" == "$frameworks_dir" ]] && is_shared_lib "$current"; then
    dylib_id="@loader_path/$(basename "$current")"
    install_name_tool -id "$dylib_id" "$current"
  fi

  while IFS= read -r dep; do
    [[ -n "$dep" ]] || continue

    if ! is_external_dep "$dep"; then
      continue
    fi

    if is_cef_framework_binary_dep "$dep"; then
      replacement="$(replacement_for_cef_framework_dep "$current")"
      install_name_tool -change "$dep" "$replacement" "$current"
      continue
    fi

    if [[ ! -f "$dep" ]]; then
      echo "Missing dependency referenced by '$current': $dep" >&2
      exit 1
    fi

    dep_name="$(basename "$dep")"
    bundled_dep="$frameworks_dir/$dep_name"

    if [[ ! -f "$bundled_dep" ]]; then
      cp -fp "$dep" "$bundled_dep"
      chmod u+w "$bundled_dep"
      queue+=("$bundled_dep")
      echo "Bundled $dep_name"
    fi

    replacement="$(replacement_for "$current" "$dep_name")"
    install_name_tool -change "$dep" "$replacement" "$current"
  done < <(otool -L "$current" | tail -n +2 | awk '{print $1}')
done

has_errors=0
while IFS= read -r candidate; do
  if ! is_macho "$candidate"; then
    continue
  fi

  while IFS= read -r dep; do
    [[ -n "$dep" ]] || continue
    if is_external_dep "$dep"; then
      echo "Unbundled dependency remains in '$candidate': $dep" >&2
      has_errors=1
    fi
  done < <(otool -L "$candidate" | tail -n +2 | awk '{print $1}')
done < <(list_candidate_binaries | sort)

if (( has_errors != 0 )); then
  exit 1
fi

echo "Bundled and relinked macOS dependencies for $app_path"
