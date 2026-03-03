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

if [[ ! -d "$contents_dir" || ! -d "$macos_dir" ]]; then
  echo "Expected a macOS app bundle at '$app_path'" >&2
  exit 1
fi

mkdir -p "$frameworks_dir"

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
