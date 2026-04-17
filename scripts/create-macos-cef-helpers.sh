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
info_plist="$contents_dir/Info.plist"

if [[ ! -d "$contents_dir" || ! -d "$macos_dir" || ! -f "$info_plist" ]]; then
  echo "Expected a macOS app bundle at '$app_path'" >&2
  exit 1
fi

main_executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$info_plist" 2>/dev/null || true)"
if [[ -z "$main_executable" ]]; then
  echo "Unable to read CFBundleExecutable from '$info_plist'" >&2
  exit 1
fi

main_binary="$macos_dir/$main_executable"
if [[ ! -f "$main_binary" ]]; then
  echo "Main executable not found: $main_binary" >&2
  exit 1
fi

main_identifier="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$info_plist" 2>/dev/null || true)"
if [[ -z "$main_identifier" ]]; then
  echo "Unable to read CFBundleIdentifier from '$info_plist'" >&2
  exit 1
fi

read_optional_plist_string() {
  local key_path="$1"
  /usr/libexec/PlistBuddy -c "Print :$key_path" "$info_plist" 2>/dev/null || true
}

xml_escape() {
  local value="$1"
  value="${value//&/&amp;}"
  value="${value//</&lt;}"
  value="${value//>/&gt;}"
  value="${value//\"/&quot;}"
  value="${value//\'/&apos;}"
  printf '%s' "$value"
}

print_string_key() {
  local key="$1"
  local value="$2"
  printf '  <key>%s</key>\n' "$key"
  printf '  <string>%s</string>\n' "$(xml_escape "$value")"
}

print_optional_string_key() {
  local key="$1"
  local value="$2"
  if [[ -n "$value" ]]; then
    print_string_key "$key" "$value"
  fi
}

print_bool_key() {
  local key="$1"
  local value="$2"
  printf '  <key>%s</key>\n' "$key"
  if [[ "$value" == "false" || "$value" == "0" ]]; then
    printf '  <false/>\n'
  else
    printf '  <true/>\n'
  fi
}

development_region="$(read_optional_plist_string 'CFBundleDevelopmentRegion')"
if [[ -z "$development_region" ]]; then
  development_region="en"
fi

bundle_info_dictionary_version="$(read_optional_plist_string 'CFBundleInfoDictionaryVersion')"
if [[ -z "$bundle_info_dictionary_version" ]]; then
  bundle_info_dictionary_version="6.0"
fi

short_version="$(read_optional_plist_string 'CFBundleShortVersionString')"
if [[ -z "$short_version" ]]; then
  short_version="0.11.0"
fi

bundle_version="$(read_optional_plist_string 'CFBundleVersion')"
if [[ -z "$bundle_version" ]]; then
  bundle_version="0.11.0"
fi

supports_automatic_graphics_switching="$(
  read_optional_plist_string 'NSSupportsAutomaticGraphicsSwitching'
)"
if [[ -z "$supports_automatic_graphics_switching" ]]; then
  supports_automatic_graphics_switching="true"
fi

camera_usage_description="$(read_optional_plist_string 'NSCameraUsageDescription')"
microphone_usage_description="$(read_optional_plist_string 'NSMicrophoneUsageDescription')"
public_key_credential_usage_description="$(
  read_optional_plist_string 'NSWebBrowserPublicKeyCredentialUsageDescription'
)"
distribution_channel="$(read_optional_plist_string 'TABORDistributionChannel')"

helper_prefix="${TABOR_CEF_HELPER_PREFIX:-Tabor}"
helpers=(
  "$helper_prefix Helper"
  "$helper_prefix Helper (Renderer)"
  "$helper_prefix Helper (GPU)"
  "$helper_prefix Helper (Plugin)"
  "$helper_prefix Helper (Alerts)"
)

sanitize_identifier_suffix() {
  local value="$1"
  printf '%s' "$value" \
    | tr '[:upper:]' '[:lower:]' \
    | tr -cs '[:alnum:]' '-' \
    | sed -e 's/^-*//' -e 's/-*$//'
}

mkdir -p "$frameworks_dir"

for helper_name in "${helpers[@]}"; do
  helper_app="$frameworks_dir/$helper_name.app"
  helper_contents="$helper_app/Contents"
  helper_macos="$helper_contents/MacOS"
  helper_info="$helper_contents/Info.plist"
  helper_binary="$helper_macos/$helper_name"
  helper_suffix="$(sanitize_identifier_suffix "$helper_name")"
  helper_identifier="$main_identifier.helper.$helper_suffix"

  mkdir -p "$helper_macos"
  cp -f "$main_binary" "$helper_binary"
  chmod +x "$helper_binary"
  rm -rf "$helper_contents/Frameworks"
  ln -sfn ../.. "$helper_contents/Frameworks"
  for lib in libGLESv2.dylib libEGL.dylib; do
    ln -sfn "../Frameworks/$lib" "$helper_macos/$lib"
  done

  {
    cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>$(xml_escape "$development_region")</string>
  <key>CFBundleDisplayName</key>
  <string>$(xml_escape "$helper_name")</string>
  <key>CFBundleExecutable</key>
  <string>$(xml_escape "$helper_name")</string>
  <key>CFBundleIdentifier</key>
  <string>$(xml_escape "$helper_identifier")</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>$(xml_escape "$bundle_info_dictionary_version")</string>
  <key>CFBundleName</key>
  <string>$(xml_escape "$helper_name")</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$(xml_escape "$short_version")</string>
  <key>CFBundleVersion</key>
  <string>$(xml_escape "$bundle_version")</string>
  <key>LSEnvironment</key>
  <dict>
    <key>MallocNanoZone</key>
    <string>0</string>
  </dict>
  <key>LSUIElement</key>
  <true/>
PLIST
    print_bool_key "NSSupportsAutomaticGraphicsSwitching" "$supports_automatic_graphics_switching"
    print_optional_string_key "TABORDistributionChannel" "$distribution_channel"
    print_optional_string_key "NSWebBrowserPublicKeyCredentialUsageDescription" \
      "$public_key_credential_usage_description"
    print_optional_string_key "NSCameraUsageDescription" "$camera_usage_description"
    print_optional_string_key "NSMicrophoneUsageDescription" "$microphone_usage_description"
    cat <<PLIST
</dict>
</plist>
PLIST
  } > "$helper_info"

done

echo "Created CEF helper apps in '$frameworks_dir'"
