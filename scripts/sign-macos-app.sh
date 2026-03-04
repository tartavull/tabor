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

required_team_id="${TABOR_CODESIGN_TEAM_ID:-7A5AR5N85X}"
required_team_name="${TABOR_CODESIGN_TEAM_NAME:-Tiny Mile US, Corp}"
require_team_codesign="${TABOR_REQUIRE_TEAM_CODESIGN:-1}"
identity="${TABOR_CODESIGN_IDENTITY:-}"
entitlements="${TABOR_CODESIGN_ENTITLEMENTS:-}"
provisioning_profile="${TABOR_CODESIGN_PROVISIONING_PROFILE:-}"
passkey_entitlement_key="com.apple.developer.web-browser.public-key-credential"
requires_passkey_profile=0
hardened_runtime="${TABOR_CODESIGN_HARDENED_RUNTIME:-0}"
timestamp_signing="${TABOR_CODESIGN_TIMESTAMP:-0}"
codesign_extra_flags=()

list_codesign_identities() {
  /usr/bin/security find-identity -v -p codesigning 2>/dev/null || true
}

identity_subject() {
  local identity_label="$1"
  /usr/bin/security find-certificate -a -p -c "$identity_label" 2>/dev/null \
    | /usr/bin/openssl x509 -noout -subject -nameopt RFC2253 2>/dev/null \
    | /usr/bin/head -n 1
}

resolve_team_identity() {
  local line
  local identity_match
  local subject
  local subject_norm
  local team_name_norm

  team_name_norm="$(printf '%s' "$required_team_name" | /usr/bin/tr '[:upper:]' '[:lower:]')"

  while IFS= read -r line; do
    identity_match="$(printf '%s\n' "$line" | /usr/bin/sed -nE 's/.*"([^"]+)".*/\1/p')"
    if [[ -z "$identity_match" ]]; then
      continue
    fi

    subject="$(identity_subject "$identity_match")"
    if [[ -z "$subject" ]]; then
      continue
    fi

    subject_norm="$(printf '%s' "$subject" | /usr/bin/tr '[:upper:]' '[:lower:]' | /usr/bin/tr -d '\\')"

    if [[ -n "$required_team_name" && "$subject_norm" != *"$team_name_norm"* ]]; then
      continue
    fi

    if [[ -n "$required_team_id" && "$subject" != *"OU=$required_team_id"* ]]; then
      continue
    fi

    printf '%s\n' "$identity_match"
    return 0
  done < <(list_codesign_identities)

  return 1
}

if [[ -z "$identity" ]]; then
  if [[ "$require_team_codesign" == "1" ]]; then
    if ! identity="$(resolve_team_identity)"; then
      echo "No codesigning identity found for ${required_team_name}${required_team_id:+ (${required_team_id})}." >&2
      echo "Import/unlock the Tiny Mile signing certificate, or set TABOR_CODESIGN_IDENTITY explicitly." >&2
      echo "Available codesigning identities:" >&2
      list_codesign_identities >&2
      exit 1
    fi
  else
    identity="-"
  fi
fi

if [[ "$require_team_codesign" == "1" && "$identity" == "-" ]]; then
  echo "Ad-hoc signing is disabled when TABOR_REQUIRE_TEAM_CODESIGN=1." >&2
  echo "Set TABOR_CODESIGN_IDENTITY to a ${required_team_name} certificate, or set TABOR_REQUIRE_TEAM_CODESIGN=0 for local debug bundles." >&2
  exit 1
fi
if [[ "$identity" != "-" ]]; then
  if [[ "$hardened_runtime" == "1" ]]; then
    codesign_extra_flags+=(--options runtime)
  fi

  if [[ "$timestamp_signing" == "1" ]]; then
    codesign_extra_flags+=(--timestamp)
  fi
fi

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
  /usr/bin/codesign --force --sign "$identity" "${codesign_extra_flags[@]}" "$@" "$target"
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

if [[ "$require_team_codesign" == "1" && -n "$required_team_id" ]]; then
  team_identifier="$(
    /usr/bin/codesign -dv "$main_binary" 2>&1 \
      | /usr/bin/awk -F= '/^TeamIdentifier=/{print $2; exit}'
  )"

  if [[ "$team_identifier" != "$required_team_id" ]]; then
    echo "Expected TeamIdentifier=$required_team_id on '$main_binary', got '${team_identifier:-<empty>}'." >&2
    exit 1
  fi
fi

if [[ -n "$entitlements" ]]; then
  if ! /usr/bin/codesign -d --entitlements :- "$main_binary" 2>&1 | /usr/bin/grep -q "<dict>"; then
    echo "Entitlements were not embedded into '$main_binary'" >&2
    exit 1
  fi
fi
