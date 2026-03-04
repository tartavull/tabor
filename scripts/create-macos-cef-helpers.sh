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

  cat > "$helper_info" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>$helper_name</string>
  <key>CFBundleIdentifier</key>
  <string>$helper_identifier</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>$helper_name</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.17.0-dev</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSUIElement</key>
  <true/>
</dict>
</plist>
PLIST

done

echo "Created CEF helper apps in '$frameworks_dir'"
