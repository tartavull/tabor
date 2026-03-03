#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  exit 0
fi

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <path-to-Tabor.app>" >&2
  exit 1
fi

app_path="${1%/}"
contents_dir="$app_path/Contents"
macos_dir="$contents_dir/MacOS"

if [[ ! -d "$contents_dir" || ! -d "$macos_dir" ]]; then
  echo "Expected a macOS app bundle at '$app_path'" >&2
  exit 1
fi

identity="${TABOR_CODESIGN_IDENTITY:--}"
entitlements="${TABOR_CODESIGN_ENTITLEMENTS:-}"
provisioning_profile="${TABOR_CODESIGN_PROVISIONING_PROFILE:-}"
passkey_entitlement_key="com.apple.developer.web-browser.public-key-credential"
requires_passkey_profile=0

# Entitlements are opt-in. Set TABOR_CODESIGN_ENTITLEMENTS explicitly when needed.

if [[ -n "$entitlements" && ! -f "$entitlements" ]]; then
  echo "Entitlements file not found: $entitlements" >&2
  exit 1
fi
if [[ -n "$entitlements" ]] && /usr/libexec/PlistBuddy -c "Print :$passkey_entitlement_key" "$entitlements" >/dev/null 2>&1; then
  requires_passkey_profile=1
fi

if [[ "$requires_passkey_profile" -eq 1 ]]; then
  if [[ -z "$provisioning_profile" ]]; then
    echo "TABOR_CODESIGN_PROVISIONING_PROFILE is required when passkey entitlement is enabled." >&2
    exit 1
  fi

  if [[ ! -f "$provisioning_profile" ]]; then
    echo "Provisioning profile not found: $provisioning_profile" >&2
    exit 1
  fi

  cp -f "$provisioning_profile" "$contents_dir/embedded.provisionprofile"
else
  rm -f "$contents_dir/embedded.provisionprofile"
fi

main_executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$contents_dir/Info.plist" 2>/dev/null || true)"
if [[ -z "$main_executable" ]]; then
  echo "Unable to read CFBundleExecutable from '$contents_dir/Info.plist'" >&2
  exit 1
fi

main_binary="$macos_dir/$main_executable"
if [[ ! -f "$main_binary" ]]; then
  echo "Main executable not found: $main_binary" >&2
  exit 1
fi

codesign_file() {
  local target="$1"
  shift
  /usr/bin/codesign --force --sign "$identity" "$@" "$target"
}

is_macho() {
  local path="$1"
  /usr/bin/file -b "$path" | /usr/bin/grep -q "Mach-O"
}

# Sign code files first so bundle signatures can be finalized without --deep.
while IFS= read -r candidate; do
  [[ "$candidate" == "$main_binary" ]] && continue
  if is_macho "$candidate"; then
    codesign_file "$candidate"
  fi
done < <(find "$contents_dir" -type f \( -perm -u+x -o -name '*.dylib' -o -name '*.so' \) | sort)

# Sign nested bundles from the deepest level up.
while IFS= read -r bundle; do
  [[ "$bundle" == "$app_path" ]] && continue
  codesign_file "$bundle"
done < <(find "$contents_dir" -depth -type d \( -name '*.app' -o -name '*.appex' -o -name '*.framework' -o -name '*.xpc' \) | sort)

while IFS= read -r executable; do
  codesign_file "$executable"
done < <(find "$macos_dir" -maxdepth 1 -type f -perm -u+x | sort)

codesign_file "$app_path"

if [[ -n "$entitlements" ]]; then
  codesign_file "$main_binary" --entitlements "$entitlements"
fi

/usr/bin/codesign --verify --deep --strict --verbose=2 "$app_path"

if [[ -n "$entitlements" ]]; then
  if ! /usr/bin/codesign -d --entitlements :- "$main_binary" 2>&1 | /usr/bin/grep -q "<dict>"; then
    echo "Entitlements were not embedded into '$main_binary'" >&2
    exit 1
  fi
fi
