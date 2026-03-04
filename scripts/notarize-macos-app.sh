#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  exit 0
fi

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "Usage: $0 <path-to-Tabor.app> [path-to-Tabor.dmg]" >&2
  exit 1
fi

app_path="${1%/}"
dmg_path="${2:-}"

if [[ ! -d "$app_path" || "${app_path##*.}" != "app" ]]; then
  echo "Expected a macOS app bundle path ending in .app, got: $app_path" >&2
  exit 1
fi

if [[ -n "$dmg_path" && ! -f "$dmg_path" ]]; then
  echo "DMG not found: $dmg_path" >&2
  exit 1
fi

if ! command -v xcrun >/dev/null 2>&1; then
  echo "xcrun not found; install Xcode command line tools first." >&2
  exit 1
fi

if ! xcrun --find notarytool >/dev/null 2>&1; then
  echo "notarytool is unavailable. Install a recent Xcode toolchain." >&2
  exit 1
fi

if ! xcrun --find stapler >/dev/null 2>&1; then
  echo "stapler is unavailable. Install a recent Xcode toolchain." >&2
  exit 1
fi

contents_dir="$app_path/Contents"
main_executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$contents_dir/Info.plist" 2>/dev/null || true)"
if [[ -z "$main_executable" ]]; then
  echo "Unable to read CFBundleExecutable from '$contents_dir/Info.plist'" >&2
  exit 1
fi

main_binary="$contents_dir/MacOS/$main_executable"
if [[ ! -f "$main_binary" ]]; then
  echo "Main executable not found: $main_binary" >&2
  exit 1
fi

first_authority="$(
  /usr/bin/codesign -dvv "$main_binary" 2>&1 |
    /usr/bin/awk -F= '/^Authority=/{if (!seen) {print $2; seen=1}}'
)"
if [[ "$first_authority" != Developer\ ID\ Application* ]]; then
  echo "Notarization requires Developer ID Application signing. Found: ${first_authority:-<none>}" >&2
  exit 1
fi

has_runtime="$(
  /usr/bin/codesign -dv "$main_binary" 2>&1 |
    /usr/bin/awk '/flags=.*runtime/{print "1"}'
)"
if [[ -z "$has_runtime" ]]; then
  echo "Hardened runtime is missing on '$main_binary'. Re-sign with TABOR_CODESIGN_HARDENED_RUNTIME=1." >&2
  exit 1
fi

auth_args=()

if [[ -n "${TABOR_NOTARY_KEYCHAIN_PROFILE:-}" ]]; then
  auth_args+=("--keychain-profile" "$TABOR_NOTARY_KEYCHAIN_PROFILE")
  if [[ -n "${TABOR_NOTARY_KEYCHAIN:-}" ]]; then
    auth_args+=("--keychain" "$TABOR_NOTARY_KEYCHAIN")
  fi
  if [[ -n "${TABOR_NOTARY_TEAM_ID:-}" ]]; then
    auth_args+=("--team-id" "$TABOR_NOTARY_TEAM_ID")
  fi
elif [[ -n "${TABOR_NOTARY_API_KEY_PATH:-}" && -n "${TABOR_NOTARY_API_KEY_ID:-}" && -n "${TABOR_NOTARY_API_ISSUER:-}" ]]; then
  auth_args+=("--key" "$TABOR_NOTARY_API_KEY_PATH")
  auth_args+=("--key-id" "$TABOR_NOTARY_API_KEY_ID")
  auth_args+=("--issuer" "$TABOR_NOTARY_API_ISSUER")
elif [[ -n "${TABOR_NOTARY_APPLE_ID:-}" && -n "${TABOR_NOTARY_APP_SPECIFIC_PASSWORD:-}" && -n "${TABOR_NOTARY_TEAM_ID:-}" ]]; then
  auth_args+=("--apple-id" "$TABOR_NOTARY_APPLE_ID")
  auth_args+=("--password" "$TABOR_NOTARY_APP_SPECIFIC_PASSWORD")
  auth_args+=("--team-id" "$TABOR_NOTARY_TEAM_ID")
else
  cat >&2 <<'ERR'
Missing notarization credentials. Provide one of:
  1) TABOR_NOTARY_KEYCHAIN_PROFILE (optional TABOR_NOTARY_KEYCHAIN, TABOR_NOTARY_TEAM_ID)
  2) TABOR_NOTARY_API_KEY_PATH + TABOR_NOTARY_API_KEY_ID + TABOR_NOTARY_API_ISSUER
  3) TABOR_NOTARY_APPLE_ID + TABOR_NOTARY_APP_SPECIFIC_PASSWORD + TABOR_NOTARY_TEAM_ID
ERR
  exit 1
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

app_name="$(basename "$app_path")"
submission_zip="$workdir/${app_name%.app}-notarization.zip"

echo "Creating notarization archive: $submission_zip"
/usr/bin/ditto -c -k --keepParent "$app_path" "$submission_zip"

echo "Submitting to Apple notary service"
xcrun notarytool submit "$submission_zip" "${auth_args[@]}" --wait --timeout 30m

echo "Stapling ticket to app"
xcrun stapler staple "$app_path"

echo "Validating app ticket"
xcrun stapler validate "$app_path"
/usr/sbin/spctl --assess --type execute --verbose "$app_path"

if [[ -n "$dmg_path" ]]; then
  echo "Stapling ticket to DMG"
  xcrun stapler staple "$dmg_path"

  echo "Validating DMG ticket"
  xcrun stapler validate "$dmg_path"
fi
